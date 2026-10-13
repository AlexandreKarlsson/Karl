//! argv → policy checks → store queries → rendered reply. The whole request
//! path lives here, in enforcement order: parse, write authorization, source
//! present, rate, query, hide, render, redact, cap.

use crate::state::App;
use chrono::{Days, Local, NaiveDate, TimeZone};
use karl_core::commands::{index, Command, Verb};
use karl_core::model::News;
use karl_core::policy::Mode;
use karl_core::proto::Reply;
use karl_core::render::{self, Opts};
use karl_core::store::Filter;
use karl_core::{Item, Kind};
use karl_sources::{Live, WriteReq};
use std::time::Instant;

pub async fn handle(app: &App, idx: usize, args: &[String]) -> Reply {
    let p = &app.profiles[idx];
    // 1. parse
    let cmd = match Command::parse(args) {
        Ok(c) => c,
        Err(usage) => return Reply::usage(usage),
    };
    // 2. a write on a source this profile is not authorized for: refused before any lookup
    if cmd.verb() == Verb::Write && writable_source(app, idx, cmd.group()).is_none() {
        return Reply::denied(format!(
            "writes not authorized for {} (mode {})",
            cmd.group(),
            Mode::NotAuthorized.as_str()
        ));
    }
    // 3. a group with no source: say so instead of an empty answer
    if !cmd.group().is_empty() && group_sources(app, idx, cmd.group()).is_empty() {
        return Reply::usage(format!("no {} source for this profile", cmd.group()));
    }
    // 4. rate
    if let Err(wait) = p.limiter.lock().unwrap().check(Instant::now()) {
        return Reply::rate_limited(format!(
            "rate limited ({}), retry in {}m",
            p.policy.cfg.rate,
            wait.as_secs() / 60 + 1
        ));
    }
    // 5. run
    let ctx = Ctx {
        app,
        idx,
        opts: Opts {
            line_chars: p.policy.cfg.line_chars,
            fold_after: 3,
            now: now(),
        },
    };
    match run(&ctx, cmd, args).await {
        Ok(reply) => reply,
        Err(e) => Reply::error(format!("{e:#}")),
    }
}

struct Ctx<'a> {
    app: &'a App,
    idx: usize,
    opts: Opts,
}

impl Ctx<'_> {
    fn profile(&self) -> &karl_core::policy::Profile {
        &self.app.profiles[self.idx].policy
    }

    fn sources(&self, group: &str) -> Vec<String> {
        group_sources(self.app, self.idx, group)
    }

    /// Drop hidden items, render lines, redact, cap. The last step of every listing.
    fn finish(&self, items: &[Item], with_time: bool, empty: &str) -> Reply {
        let p = self.profile();
        let visible: Vec<Item> = items.iter().filter(|i| !p.hidden(i)).cloned().collect();
        let lines = render::listing(&visible, &self.opts, with_time);
        self.finish_lines(lines, empty)
    }

    fn finish_lines(&self, lines: Vec<String>, empty: &str) -> Reply {
        let p = self.profile();
        if lines.is_empty() {
            return Reply::ok(empty);
        }
        let text = render::cap(&lines, p.cfg.max_lines, p.cfg.max_bytes);
        Reply::ok(p.redact(&text))
    }

    fn finish_text(&self, text: String) -> Reply {
        let p = self.profile();
        Reply::ok(p.redact(&karl_core::text::truncate_bytes(&text, p.cfg.max_bytes)))
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Source ids of a profile that feed `group` (`mail` → its gmail/imap sources).
fn group_sources(app: &App, idx: usize, group: &str) -> Vec<String> {
    app.profiles[idx]
        .policy
        .sources()
        .iter()
        .filter(|s| app.group_of(s) == group)
        .cloned()
        .collect()
}

/// The source a write on `group` goes to, with its mode: the first source of the
/// group the profile may write to. `None` when every source is not-authorized.
fn writable_source(app: &App, idx: usize, group: &str) -> Option<(String, Mode)> {
    let p = &app.profiles[idx].policy;
    group_sources(app, idx, group)
        .into_iter()
        .map(|s| {
            let m = p.cfg.mode_for(&s);
            (s, m)
        })
        .find(|(_, m)| *m != Mode::NotAuthorized)
}

/// Groups where this profile may write at all; the index advertises their write verbs.
fn writable_groups(app: &App, idx: usize) -> Vec<&'static str> {
    ["msg", "mail", "cal", "jira"]
        .into_iter()
        .filter(|g| writable_source(app, idx, g).is_some())
        .collect()
}

fn kind_of_group(group: &str) -> Kind {
    match group {
        "msg" => Kind::Msg,
        "mail" => Kind::Mail,
        "cal" => Kind::Event,
        _ => Kind::Task,
    }
}

/// Run an authorized write on a source. Shared by the request path (`authorized`)
/// and the admin socket (`approve`).
pub async fn execute_write(app: &App, source: &str, req: &WriteReq) -> anyhow::Result<String> {
    let src = app
        .sources
        .get(source)
        .ok_or_else(|| anyhow::anyhow!("unknown source {source}"))?;
    src.write(req).await
}

async fn run(ctx: &Ctx<'_>, cmd: Command, args: &[String]) -> anyhow::Result<Reply> {
    use Command::*;
    let p = ctx.profile();
    let group = cmd.group();
    Ok(match cmd {
        Index => {
            let groups: Vec<&str> = ["msg", "mail", "cal", "jira"]
                .into_iter()
                .filter(|g| !ctx.sources(g).is_empty())
                .collect();
            let writable = writable_groups(ctx.app, ctx.idx);
            Reply::ok(index(&groups, &writable, &ctx.app.status_line(p.sources())))
        }
        News => news_items(ctx, &["msg", "mail", "cal", "jira"]),
        MsgNews => news_items(ctx, &["msg"]),
        MailNews => news_items(ctx, &["mail"]),
        JiraNews => news_items(ctx, &["jira"]),
        Pending => pending(ctx),
        MsgRecent(d) | MailRecent(d) => {
            let srcs = ctx.sources(group);
            let items = ctx.app.store.lock().unwrap().recent(
                &Filter {
                    kinds: &[kind_of_group(group)],
                    sources: &srcs,
                },
                now() - d.as_secs() as i64,
                p.cfg.max_lines,
            )?;
            ctx.finish(&items, true, "nothing in that window")
        }
        MsgShow { who, n } | MailShow { who, n } => {
            let srcs = ctx.sources(group);
            let items: Vec<Item> = ctx
                .app
                .store
                .lock()
                .unwrap()
                .by_who(
                    &Filter {
                        kinds: &[kind_of_group(group)],
                        sources: &srcs,
                    },
                    &who,
                    n,
                )?
                .into_iter()
                .filter(|i| !p.hidden(i))
                .collect();
            match items.as_slice() {
                [] => Reply::ok(format!("nothing from {who}")),
                [one] if n == 1 => ctx.finish_text(render::show(one, &ctx.opts, p.cfg.max_bytes)),
                _ => ctx.finish(&items, true, ""),
            }
        }
        MsgFind(q) | MailFind(q) => {
            let srcs = ctx.sources(group);
            let items = ctx.app.store.lock().unwrap().find(
                &Filter {
                    kinds: &[kind_of_group(group)],
                    sources: &srcs,
                },
                &q,
                p.cfg.max_lines,
            )?;
            ctx.finish(&items, true, "no match")
        }
        CalToday | CalTomorrow | CalWeek | CalRange { .. } => {
            let today = Local::now().date_naive();
            let (from, to) = match cmd {
                CalToday => (today, today),
                CalTomorrow => (today + Days::new(1), today + Days::new(1)),
                CalWeek => (today, today + Days::new(6)),
                CalRange { from, to } => (from, to),
                _ => unreachable!(),
            };
            let srcs = ctx.sources("cal");
            let (a, b) = (day_start(from), day_start(to + Days::new(1)));
            let events: Vec<Item> = ctx
                .app
                .store
                .lock()
                .unwrap()
                .events_between(&srcs, a, b)?
                .into_iter()
                .filter(|e| !p.hidden(e))
                .collect();
            let lines = render::agenda(&events, from, to, &ctx.opts);
            ctx.finish_lines(lines, "nothing scheduled")
        }
        JiraMine => {
            let mut items = Vec::new();
            for src in ctx.sources("jira") {
                let s = ctx.app.store.lock().unwrap();
                if let Some(me) = s.meta_get(&format!("source:{src}:me"))? {
                    items.extend(s.tasks_assigned(
                        std::slice::from_ref(&src),
                        &me,
                        p.cfg.max_lines,
                    )?);
                }
            }
            items.sort_by_key(|i| std::cmp::Reverse(i.ts));
            ctx.finish(
                &items,
                false,
                "no open issues assigned to you (or jira not synced yet)",
            )
        }
        JiraShow(key) => {
            let srcs = ctx.sources("jira");
            let cached = ctx.app.store.lock().unwrap().get_key(&srcs, &key)?;
            let item = match cached {
                Some(it) => Some(it),
                None => live_issue(ctx, &srcs, &key).await?,
            };
            match item {
                Some(it) if !p.hidden(&it) => {
                    ctx.finish_text(render::show(&it, &ctx.opts, p.cfg.max_bytes))
                }
                _ => Reply::ok(format!("{key}: not found")),
            }
        }
        JiraComments(key) => {
            let mut out = None;
            for src in ctx.sources("jira") {
                if let Some(t) = ctx.app.sources[&src]
                    .live(Live::JiraComments { key: &key, max: 10 })
                    .await?
                {
                    out = Some(t);
                    break;
                }
            }
            ctx.finish_text(out.unwrap_or_else(|| "no jira source".into()))
        }
        JiraQuery(jql) => {
            let mut items = Vec::new();
            for src in ctx.sources("jira") {
                if let Some(json) = ctx.app.sources[&src]
                    .live(Live::JiraQuery {
                        jql: &jql,
                        max: p.cfg.max_lines,
                    })
                    .await?
                {
                    items.extend(serde_json::from_str::<Vec<Item>>(&json)?);
                }
            }
            ctx.finish(&items, false, "no match")
        }
        JiraComment { .. } | JiraMove { .. } => write(ctx, group, &args[1..]).await?,
    })
}

/// A write on `group`: execute now when `authorized`, queue when
/// `authorization-required`. `not-authorized` was refused in `handle`.
async fn write(ctx: &Ctx<'_>, group: &str, args: &[String]) -> anyhow::Result<Reply> {
    let p = ctx.profile();
    let Some((source, mode)) = writable_source(ctx.app, ctx.idx, group) else {
        return Ok(Reply::denied("writes not authorized"));
    };
    let Some(req) = WriteReq::parse(args) else {
        return Ok(Reply::usage(
            "usage: comment <KEY> <text> | move <KEY> <status>",
        ));
    };
    match mode {
        Mode::Authorized => {
            let result = execute_write(ctx.app, &source, &req).await;
            ctx.app.audit.log(&format!(
                "{} WRITE {source} {} → {}",
                p.name,
                req.describe(),
                match &result {
                    Ok(r) => format!("ok: {r}"),
                    Err(e) => format!("failed: {e:#}"),
                }
            ));
            Ok(match result {
                Ok(r) => Reply::ok(r),
                Err(e) => Reply::error(format!("{e:#}")),
            })
        }
        Mode::AuthorizationRequired => {
            let id = ctx
                .app
                .store
                .lock()
                .unwrap()
                .write_queue(&p.name, &source, args)?;
            ctx.app.audit.log(&format!(
                "{} WRITE-QUEUED #{id} {source} {}",
                p.name,
                req.describe()
            ));
            Ok(Reply::ok(format!(
                "queued #{id} for approval: {} (karl pending shows its status)",
                req.describe()
            )))
        }
        Mode::NotAuthorized => Ok(Reply::denied("writes not authorized")),
    }
}

/// This profile's writes, newest first: `#12 pending jira comment HT-36: …`.
fn pending(ctx: &Ctx<'_>) -> Reply {
    let p = ctx.profile();
    let list = match ctx
        .app
        .store
        .lock()
        .unwrap()
        .write_list(Some(&p.name), None, p.cfg.max_lines)
    {
        Ok(l) => l,
        Err(e) => return Reply::error(format!("{e:#}")),
    };
    let lines: Vec<String> = list
        .iter()
        .map(|w| {
            let what = WriteReq::parse(&w.args)
                .map(|r| r.describe())
                .unwrap_or_else(|| w.args.join(" "));
            let mut line = format!("#{} {} {} {}", w.id, w.status, w.source, what);
            if let Some(r) = &w.result {
                line.push_str(&format!(" — {}", karl_core::text::one_line(r, 100)));
            }
            line
        })
        .collect();
    ctx.finish_lines(lines, "no pending writes")
}

/// Everything new per source since this profile's cursor for that source, oldest
/// first, at most `max_lines` items; each cursor advances to the last item shown.
/// What "new" means is the source's strategy: `Changes` (edits count) or `Arrivals`.
fn news_items(ctx: &Ctx<'_>, groups: &[&str]) -> Reply {
    let p = ctx.profile();
    let max = p.cfg.max_lines;
    let mut all: Vec<(String, News, Item)> = Vec::new();
    let mut more = false;
    let mut started: Vec<String> = Vec::new();
    {
        let s = ctx.app.store.lock().unwrap();
        for g in groups {
            let kind = kind_of_group(g);
            for src in ctx.sources(g) {
                let news = ctx.app.news_of(&src);
                let mut cur = s.cursor_get(&p.name, &src).unwrap_or(0);
                if cur == 0 {
                    // first ask defines "now": the backlog is not news
                    cur = s.max_seq().unwrap_or(0);
                    let _ = s.cursor_set(&p.name, &src, cur);
                    started.push(src.clone());
                }
                let f = Filter {
                    kinds: &[kind],
                    sources: std::slice::from_ref(&src),
                };
                match s.since_seq(&f, cur, max + 1, news) {
                    Ok(items) => all.extend(items.into_iter().map(|i| (src.clone(), news, i))),
                    Err(e) => return Reply::error(format!("{e:#}")),
                }
            }
        }
    }
    all.sort_by_key(|(_, _, i)| i.seq);
    if all.len() > max {
        all.truncate(max);
        more = true;
    }
    {
        let s = ctx.app.store.lock().unwrap();
        for g in groups {
            for src in ctx.sources(g) {
                let last = all
                    .iter()
                    .filter(|(sid, _, _)| *sid == src)
                    .map(|(_, news, i)| match news {
                        News::Changes => i.seq,
                        News::Arrivals => i.created_seq,
                    })
                    .max();
                if let Some(last) = last {
                    let _ = s.cursor_set(&p.name, &src, last);
                }
            }
        }
    }
    let items: Vec<Item> = all.into_iter().map(|(_, _, i)| i).collect();
    let empty = if started.is_empty() {
        "nothing new".to_string()
    } else {
        format!("nothing new (tracking {} from now)", started.join(", "))
    };
    let mut reply = ctx.finish(&items, false, &empty);
    if more {
        reply.body.push_str("(more: run again)\n");
    }
    reply
}

async fn live_issue(ctx: &Ctx<'_>, srcs: &[String], key: &str) -> anyhow::Result<Option<Item>> {
    for src in srcs {
        let jql = format!("key = {key}");
        if let Some(json) = ctx.app.sources[src]
            .live(Live::JiraQuery { jql: &jql, max: 1 })
            .await?
        {
            let items: Vec<Item> = serde_json::from_str(&json)?;
            if let Some(it) = items.into_iter().next() {
                return Ok(Some(it));
            }
        }
    }
    Ok(None)
}

fn day_start(d: NaiveDate) -> i64 {
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|t| t.timestamp())
        .unwrap_or(0)
}
