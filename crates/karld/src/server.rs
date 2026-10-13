//! Profile sockets: accept, check the peer uid, read one request, answer, log.

use crate::dispatch;
use crate::state::App;
use crate::users::gid_of;
use anyhow::{Context, Result};
use karl_core::proto::{self, Reply, MAX_REQUEST};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Per-request wall clock, including live lookups.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub fn bind(path: &Path, mode: u32, group: Option<&str>) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // A stale socket from the last run, or an empty directory: a container runtime
    // that bind-mounted the socket path while the daemon was down creates one.
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if m.is_dir() {
            std::fs::remove_dir(path)
                .with_context(|| format!("remove directory at {}", path.display()))?;
        } else {
            std::fs::remove_file(path)
                .with_context(|| format!("remove stale {}", path.display()))?;
        }
    }
    let l = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    if let Some(g) = group {
        let gid = gid_of(g).with_context(|| format!("unknown group {g}"))?;
        std::os::unix::fs::chown(path, None, Some(gid))
            .with_context(|| format!("chgrp {g} {}", path.display()))?;
    }
    Ok(l)
}

pub async fn serve_profile(app: Arc<App>, idx: usize) -> Result<()> {
    let p = &app.profiles[idx];
    let path = p.policy.cfg.socket.clone();
    let listener = bind(
        &path,
        app.cfg.daemon.socket_mode(),
        app.cfg.daemon.socket_group.as_deref(),
    )?;
    tracing::info!(profile = %p.policy.name, socket = %path.display(), "listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let a = app.clone();
        tokio::spawn(async move { handle_conn(a, idx, stream).await });
    }
}

async fn handle_conn(app: Arc<App>, idx: usize, mut stream: UnixStream) {
    let p = &app.profiles[idx];
    let name = p.policy.name.clone();
    let uid = match stream.peer_cred() {
        Ok(c) => c.uid(),
        Err(e) => {
            tracing::warn!(profile = %name, "peer_cred failed: {e}");
            return;
        }
    };
    if !p.uids.contains(&uid) {
        app.audit.log(&format!("{name} uid={uid} REFUSED(uid)"));
        let _ = stream
            .write_all(&Reply::denied("not allowed on this socket").encode())
            .await;
        return;
    }
    let Ok(_permit) = p.sem.try_acquire() else {
        let _ = stream
            .write_all(&Reply::error("busy, retry").encode())
            .await;
        return;
    };
    let t0 = Instant::now();
    let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let (rd, mut wr) = stream.split();
        let mut line = Vec::new();
        BufReader::new(rd.take(MAX_REQUEST as u64 + 1))
            .read_until(b'\n', &mut line)
            .await?;
        if line.len() > MAX_REQUEST {
            let r = Reply::usage("request too long");
            wr.write_all(&r.encode()).await?;
            return Ok::<_, anyhow::Error>((vec!["<too long>".to_string()], r));
        }
        let req = match proto::decode_request(line.strip_suffix(b"\n").unwrap_or(&line)) {
            Ok(r) => r,
            Err(e) => {
                let r = Reply::usage(format!("bad request: {e}"));
                wr.write_all(&r.encode()).await?;
                return Ok((vec!["<bad>".to_string()], r));
            }
        };
        let reply = dispatch::handle(&app, idx, &req.args).await;
        wr.write_all(&reply.encode()).await?;
        wr.shutdown().await?;
        Ok((req.args, reply))
    })
    .await;
    match result {
        Ok(Ok((args, reply))) => app.audit.log(&format!(
            "{name} uid={uid} {:?} exit={} {}B {}ms",
            args.join(" "),
            reply.exit,
            reply.body.len(),
            t0.elapsed().as_millis()
        )),
        Ok(Err(e)) => app.audit.log(&format!("{name} uid={uid} io-error {e}")),
        Err(_) => {
            app.audit.log(&format!("{name} uid={uid} TIMEOUT"));
            let _ = stream.write_all(&Reply::error("timeout").encode()).await;
        }
    }
}
