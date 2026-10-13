//! Sources: each one pulls a provider into the core store.
//!
//! A source is built from its `[source.<id>]` table and the secrets file, then
//! the daemon calls [`Source::sync`] every `every`. Sources never render: they
//! produce [`karl_core::Item`]s and let the store decide what is new.

pub mod google;
pub mod http;
pub mod jira;

use anyhow::{bail, Result};
use async_trait::async_trait;
use karl_core::model::News;
use karl_core::policy::{Secrets, SourceCfg};
use karl_core::Store;
use std::sync::{Arc, Mutex};

/// What a sync did, for the log and `karl` status line.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncReport {
    pub fetched: usize,
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
}

/// Shared handle to the store: rusqlite is not `Sync`, the daemon serialises through a mutex.
pub type SharedStore = Arc<Mutex<Store>>;

#[async_trait]
pub trait Source: Send + Sync {
    /// The source id from the policy file (`gm`, `jira`).
    fn id(&self) -> &str;
    /// Implementation name (`gmail`, `jira`, `gcal`).
    fn kind(&self) -> &'static str;
    /// The command group this source feeds: `msg`, `mail`, `cal` or `jira`.
    fn group(&self) -> &'static str;
    /// What "news" means here by default. `Changes` for tasks and events, `Arrivals` for mail and chat.
    fn news(&self) -> News;
    /// Pull what changed since the last run into the store.
    async fn sync(&self, store: &SharedStore) -> Result<SyncReport>;
    /// Live lookups the cache cannot answer (`jira q`, `jira comments`). Default: none.
    async fn live(&self, _what: Live<'_>) -> Result<Option<String>> {
        Ok(None)
    }
    /// A write, already authorized by the daemon. Default: this source cannot write.
    async fn write(&self, _what: &WriteReq) -> Result<String> {
        bail!("{} cannot write", self.kind())
    }
    /// Verify credentials and scopes once at start. Default: nothing to check.
    async fn preflight(&self) -> Result<()> {
        Ok(())
    }
}

/// Requests a source may answer live, from the network, at call time.
#[derive(Debug, Clone, Copy)]
pub enum Live<'a> {
    JiraQuery { jql: &'a str, max: usize },
    JiraComments { key: &'a str, max: usize },
}

/// A write the daemon has authorized (or a human approved). Sources execute these.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WriteReq {
    /// Add a comment to a task.
    Comment { key: String, text: String },
    /// Move a task to a status by name.
    Move { key: String, status: String },
}

impl WriteReq {
    /// From the argv the agent typed (`comment KEY text…`, `move KEY status…`).
    pub fn parse(args: &[String]) -> Option<WriteReq> {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        match a.as_slice() {
            ["comment", key, text @ ..] if !text.is_empty() => Some(WriteReq::Comment {
                key: key.to_uppercase(),
                text: text.join(" "),
            }),
            ["move", key, status @ ..] if !status.is_empty() => Some(WriteReq::Move {
                key: key.to_uppercase(),
                status: status.join(" "),
            }),
            _ => None,
        }
    }

    /// One line for the audit log and the approval list.
    pub fn describe(&self) -> String {
        match self {
            WriteReq::Comment { key, text } => format!(
                "comment {key}: {}",
                karl_core::text::truncate_chars(text, 80)
            ),
            WriteReq::Move { key, status } => format!("move {key} → {status}"),
        }
    }
}

/// Build the source for one `[source.<id>]` table.
pub fn build(id: &str, cfg: &SourceCfg, secrets: &Secrets) -> Result<Box<dyn Source>> {
    Ok(match cfg.kind.as_str() {
        "jira" => Box::new(jira::Jira::new(id, cfg, secrets)?),
        "gmail" => Box::new(google::gmail::Gmail::new(id, cfg, secrets)?),
        "gcal" => Box::new(google::gcal::Gcal::new(id, cfg, secrets)?),
        other => bail!("source {id}: unknown kind {other} (jira, gmail, gcal)"),
    })
}

/// Read an optional key of the source's extra table.
pub(crate) fn extra_str<'a>(cfg: &'a SourceCfg, key: &str) -> Option<&'a str> {
    cfg.extra.get(key).and_then(|v| v.as_str())
}

pub(crate) fn extra_u64(cfg: &SourceCfg, key: &str, default: u64) -> u64 {
    cfg.extra
        .get(key)
        .and_then(|v| v.as_integer())
        .map(|i| i.max(0) as u64)
        .unwrap_or(default)
}

pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
