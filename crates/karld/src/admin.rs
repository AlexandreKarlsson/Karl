//! The admin socket: mode 0600, the daemon's own user only.
//! `status`, `sync [source]`, `purge`, `approvals`, `approve <id>`, `deny <id>`.

use crate::dispatch::execute_write;
use crate::server::bind;
use crate::state::{age, App};
use crate::users::my_uid;
use anyhow::Result;
use karl_core::proto::{self, Reply, MAX_REQUEST};
use karl_sources::WriteReq;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub async fn serve(app: Arc<App>) -> Result<()> {
    let listener = bind(&app.cfg.daemon.admin_socket, 0o600, None)?;
    tracing::info!(socket = %app.cfg.daemon.admin_socket.display(), "admin listening");
    let me = my_uid();
    loop {
        let (mut stream, _) = listener.accept().await?;
        let uid = stream.peer_cred().map(|c| c.uid()).unwrap_or(u32::MAX);
        if uid != me && uid != 0 {
            let _ = stream
                .write_all(&Reply::denied("admin socket").encode())
                .await;
            continue;
        }
        let a = app.clone();
        tokio::spawn(async move {
            let (rd, mut wr) = stream.split();
            let mut line = Vec::new();
            if BufReader::new(rd.take(MAX_REQUEST as u64))
                .read_until(b'\n', &mut line)
                .await
                .is_err()
            {
                return;
            }
            let reply = match proto::decode_request(line.strip_suffix(b"\n").unwrap_or(&line)) {
                Ok(r) => handle(&a, &r.args).await,
                Err(e) => Reply::usage(format!("bad request: {e}")),
            };
            let _ = wr.write_all(&reply.encode()).await;
        });
    }
}

async fn handle(app: &App, args: &[String]) -> Reply {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["status"] | [] => status(app),
        ["sync"] => {
            for n in app.kick.values() {
                n.notify_one();
            }
            Reply::ok("sync requested for all sources")
        }
        ["sync", id] => match app.kick.get(*id) {
            Some(n) => {
                n.notify_one();
                Reply::ok(format!("sync requested for {id}"))
            }
            None => Reply::usage(format!("unknown source {id}")),
        },
        ["purge"] => {
            let before =
                chrono::Utc::now().timestamp() - app.cfg.daemon.retention_days as i64 * 86400;
            match app.store.lock().unwrap().purge_older_than(before) {
                Ok(n) => Reply::ok(format!("purged {n}")),
                Err(e) => Reply::error(format!("{e:#}")),
            }
        }
        ["approvals"] => {
            let list = match app
                .store
                .lock()
                .unwrap()
                .write_list(None, Some("pending"), 50)
            {
                Ok(l) => l,
                Err(e) => return Reply::error(format!("{e:#}")),
            };
            if list.is_empty() {
                return Reply::ok("no pending writes");
            }
            let now = chrono::Utc::now().timestamp();
            let lines: Vec<String> = list
                .iter()
                .map(|w| {
                    let what = WriteReq::parse(&w.args)
                        .map(|r| r.describe())
                        .unwrap_or_else(|| w.args.join(" "));
                    format!(
                        "#{} {} ago {} {} {}",
                        w.id,
                        age(now - w.created),
                        w.profile,
                        w.source,
                        what
                    )
                })
                .collect();
            Reply::ok(lines.join("\n"))
        }
        ["approve", id] | ["deny", id] => {
            let approve = a[0] == "approve";
            let Ok(id) = id.parse::<i64>() else {
                return Reply::usage("approve <id> | deny <id>");
            };
            let w = match app.store.lock().unwrap().write_get(id) {
                Ok(Some(w)) => w,
                Ok(None) => return Reply::usage(format!("no write #{id}")),
                Err(e) => return Reply::error(format!("{e:#}")),
            };
            if w.status != "pending" {
                return Reply::usage(format!("#{id} is {}, not pending", w.status));
            }
            if !approve {
                let _ = app.store.lock().unwrap().write_set(id, "denied", None);
                app.audit.log(&format!(
                    "admin DENIED #{id} {} {}",
                    w.profile,
                    w.args.join(" ")
                ));
                return Reply::ok(format!("#{id} denied"));
            }
            let Some(req) = WriteReq::parse(&w.args) else {
                let _ = app
                    .store
                    .lock()
                    .unwrap()
                    .write_set(id, "failed", Some("unparseable"));
                return Reply::error(format!("#{id}: cannot parse {}", w.args.join(" ")));
            };
            let result = execute_write(app, &w.source, &req).await;
            let (status, text) = match &result {
                Ok(r) => ("done", r.clone()),
                Err(e) => ("failed", format!("{e:#}")),
            };
            let _ = app.store.lock().unwrap().write_set(id, status, Some(&text));
            app.audit.log(&format!(
                "admin APPROVED #{id} {} {} {} → {status}: {text}",
                w.profile,
                w.source,
                req.describe()
            ));
            match result {
                Ok(r) => Reply::ok(format!("#{id} done: {r}")),
                Err(e) => Reply::error(format!("#{id} failed: {e:#}")),
            }
        }
        _ => Reply::usage(
            "admin: status | sync [source] | purge | approvals | approve <id> | deny <id>",
        ),
    }
}

fn status(app: &App) -> Reply {
    let now = chrono::Utc::now().timestamp();
    let counts = app.store.lock().unwrap().counts().unwrap_or_default();
    let st = app.status.lock().unwrap();
    let mut out = String::new();
    for id in app.sources.keys() {
        let n = counts
            .iter()
            .find(|(s, _)| s == id)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        let line = match st.get(id) {
            Some(s) => match (&s.last_err, s.last_ok) {
                (Some(e), _) => format!("ERR {e}"),
                (None, Some(t)) => format!(
                    "ok {} ago, last: fetched {} +{} ~{} -{}",
                    age(now - t),
                    s.last.fetched,
                    s.last.inserted,
                    s.last.updated,
                    s.last.deleted
                ),
                _ => "never".into(),
            },
            None => "never".into(),
        };
        out.push_str(&format!(
            "{id:<8}{:<8}{:<5}{:<9}{n:>6} items  {line}\n",
            app.source_kind(id),
            app.group_of(id),
            format!("{:?}", app.news_of(id)).to_lowercase()
        ));
    }
    for p in &app.profiles {
        let modes: Vec<String> = p
            .policy
            .sources()
            .iter()
            .map(|s| format!("{s}={}", p.policy.cfg.mode_for(s).as_str()))
            .collect();
        out.push_str(&format!(
            "profile {} → {}  writes: {}\n",
            p.policy.name,
            p.policy.cfg.socket.display(),
            modes.join(" ")
        ));
    }
    Reply::ok(out)
}
