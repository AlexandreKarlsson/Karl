//! Jira Cloud. Syncs issues updated in the last `window_days` (default 30)
//! into the store as tasks; answers `jira q` and `jira comments` live.
//!
//! Reads go anywhere under `/rest/`. Writes exist for two verbs, `comment` and
//! `move`, and only through [`Jira::post`], which accepts issue paths alone. The
//! daemon calls them only after the profile's mode allowed or a human approved.

use crate::http::{basic, client, get_json, post_json};
use crate::{extra_str, extra_u64, now, Live, SharedStore, Source, SyncReport, WriteReq};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use karl_core::model::News;
use karl_core::policy::{Secrets, SourceCfg};
use karl_core::{Item, Kind};
use reqwest::header::HeaderMap;
use serde_json::Value;

pub struct Jira {
    id: String,
    site: String,
    email: String,
    token: String,
    window_days: u64,
    jql_filter: Option<String>,
    client: reqwest::Client,
}

const FIELDS: &str = "summary,status,priority,updated,created,assignee,reporter,issuetype,project,labels,description";

impl Jira {
    pub fn new(id: &str, cfg: &SourceCfg, secrets: &Secrets) -> Result<Jira> {
        let site = extra_str(cfg, "site").with_context(|| format!("source {id}: site missing"))?;
        let site = site.trim_end_matches('/').to_string();
        if !site.starts_with("https://") {
            bail!("source {id}: site must be https://");
        }
        Ok(Jira {
            id: id.to_string(),
            site,
            email: secrets.get(&cfg.secret, "email")?.to_string(),
            token: secrets.get(&cfg.secret, "token")?.to_string(),
            window_days: extra_u64(cfg, "window_days", 30),
            jql_filter: extra_str(cfg, "jql").map(String::from),
            client: client()?,
        })
    }

    fn headers(&self) -> Result<HeaderMap> {
        basic(&self.email, &self.token)
    }

    async fn get(&self, path_and_query: &str) -> Result<Value> {
        if !path_and_query.starts_with("/rest/") {
            bail!("jira: refusing path {path_and_query}");
        }
        get_json(
            &self.client,
            &format!("{}{}", self.site, path_and_query),
            self.headers()?,
        )
        .await
    }

    /// The one write path. Only `/rest/api/3/issue/<KEY>/…`, nothing else.
    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        if !path.starts_with("/rest/api/3/issue/") {
            bail!("jira: refusing write to {path}");
        }
        post_json(
            &self.client,
            &format!("{}{}", self.site, path),
            self.headers()?,
            body,
        )
        .await
    }

    /// Add a comment. Returns a one-line confirmation.
    pub async fn add_comment(&self, key: &str, text: &str) -> Result<String> {
        let body = serde_json::json!({ "body": { "type": "doc", "version": 1, "content": [
            { "type": "paragraph", "content": [ { "type": "text", "text": text } ] } ] } });
        let v = self
            .post(
                &format!("/rest/api/3/issue/{}/comment", urlencoding::encode(key)),
                &body,
            )
            .await?;
        Ok(format!(
            "commented on {key} (id {})",
            v["id"].as_str().unwrap_or("?")
        ))
    }

    /// Transition an issue to the status named `status` (case-insensitive).
    pub async fn move_to(&self, key: &str, status: &str) -> Result<String> {
        let path = format!("/rest/api/3/issue/{}/transitions", urlencoding::encode(key));
        let v = self.get(&path).await?;
        let want = status.trim().to_lowercase();
        let names: Vec<String> = v["transitions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| {
                t["name"]
                    .as_str()
                    .or_else(|| t["to"]["name"].as_str())
                    .map(String::from)
            })
            .collect();
        let id = v["transitions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| {
                t["name"]
                    .as_str()
                    .map(|n| n.to_lowercase() == want)
                    .unwrap_or(false)
                    || t["to"]["name"]
                        .as_str()
                        .map(|n| n.to_lowercase() == want)
                        .unwrap_or(false)
            })
            .and_then(|t| t["id"].as_str())
            .with_context(|| {
                format!(
                    "{key}: no transition to \"{status}\" (have: {})",
                    names.join(", ")
                )
            })?
            .to_string();
        self.post(&path, &serde_json::json!({ "transition": { "id": id } }))
            .await?;
        Ok(format!("{key} moved to {status}"))
    }

    /// Who am I: account id, cached in the store meta as `jira:<id>:me`.
    pub async fn myself(&self) -> Result<(String, String)> {
        let v = self.get("/rest/api/3/myself").await?;
        let acc = v["accountId"]
            .as_str()
            .context("myself: no accountId")?
            .to_string();
        let name = v["displayName"].as_str().unwrap_or("me").to_string();
        Ok((acc, name))
    }

    /// Run a JQL search, all pages up to `max`.
    pub async fn search(&self, jql: &str, max: usize) -> Result<Vec<Item>> {
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = (max - items.len()).min(100);
            if page == 0 {
                break;
            }
            let mut q = format!(
                "/rest/api/3/search/jql?jql={}&maxResults={page}&fields={FIELDS}",
                urlencoding::encode(jql)
            );
            if let Some(t) = &token {
                q.push_str(&format!("&nextPageToken={}", urlencoding::encode(t)));
            }
            let v = self.get(&q).await?;
            for issue in v["issues"].as_array().into_iter().flatten() {
                if let Some(it) = self.issue_to_item(issue) {
                    items.push(it);
                }
            }
            match v["nextPageToken"].as_str() {
                Some(t) if !v["isLast"].as_bool().unwrap_or(false) => token = Some(t.to_string()),
                _ => break,
            }
        }
        Ok(items)
    }

    fn issue_to_item(&self, issue: &Value) -> Option<Item> {
        let key = issue["key"].as_str()?;
        let f = &issue["fields"];
        let assignee = f["assignee"]["displayName"]
            .as_str()
            .unwrap_or("unassigned");
        let ts = parse_jira_ts(f["updated"].as_str().unwrap_or("")).unwrap_or_else(now);
        let mut it = Item::new(&self.id, Kind::Task, key, assignee, ts);
        it.key = Some(key.to_string());
        it.who_addr = f["assignee"]["accountId"].as_str().map(String::from);
        it.title = f["summary"].as_str().map(|s| s.trim().to_string());
        it.status = f["status"]["name"].as_str().map(String::from);
        it.done = f["status"]["statusCategory"]["key"].as_str() == Some("done");
        it.prio = f["priority"]["name"].as_str().map(String::from);
        it.thread = f["project"]["key"].as_str().map(String::from);
        it.thread_name = f["project"]["name"].as_str().map(String::from);
        it.labels = f["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(String::from))
            .collect();
        if let Some(t) = f["issuetype"]["name"].as_str() {
            it.labels.insert(0, format!("type:{t}"));
        }
        it.body = adf_to_text(&f["description"]);
        it.url = Some(format!("{}/browse/{key}", self.site));
        Some(it)
    }

    pub async fn comments(&self, key: &str, max: usize) -> Result<String> {
        let v = self
            .get(&format!(
                "/rest/api/3/issue/{}/comment?orderBy=-created&maxResults={max}",
                urlencoding::encode(key)
            ))
            .await?;
        let mut lines = Vec::new();
        for c in v["comments"].as_array().into_iter().flatten() {
            let who = c["author"]["displayName"].as_str().unwrap_or("?");
            let when = c["created"]
                .as_str()
                .map(|s| s[..10.min(s.len())].to_string())
                .unwrap_or_default();
            let text = adf_to_text(&c["body"]);
            lines.push(format!(
                "{when} {who}: {}",
                karl_core::text::one_line(&text, 240)
            ));
        }
        lines.reverse();
        if lines.is_empty() {
            lines.push("no comments".into());
        }
        Ok(lines.join("\n"))
    }
}

#[async_trait]
impl Source for Jira {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "jira"
    }
    fn group(&self) -> &'static str {
        "jira"
    }
    fn news(&self) -> News {
        News::Changes
    }

    async fn preflight(&self) -> Result<()> {
        let (acc, name) = self.myself().await?;
        tracing::info!(source = %self.id, who = %name, "jira ok");
        if acc.is_empty() {
            bail!("jira: empty accountId");
        }
        Ok(())
    }

    async fn sync(&self, store: &SharedStore) -> Result<SyncReport> {
        let me_key = format!("source:{}:me", self.id);
        if store.lock().unwrap().meta_get(&me_key)?.is_none() {
            let (acc, name) = self.myself().await?;
            let s = store.lock().unwrap();
            s.meta_set(&me_key, &acc)?;
            s.meta_set(&format!("source:{}:me_name", self.id), &name)?;
        }
        let mut jql = format!("updated >= -{}d", self.window_days);
        if let Some(f) = &self.jql_filter {
            jql = format!("({f}) AND {jql}");
        }
        jql.push_str(" ORDER BY updated DESC");
        let mut items = self.search(&jql, 2000).await?;
        items.sort_by_key(|i| i.ts); // oldest first: seq order follows time
        let counts = store.lock().unwrap().upsert_many(&items)?;
        Ok(SyncReport {
            fetched: items.len(),
            inserted: counts.inserted,
            updated: counts.updated,
            deleted: 0,
        })
    }

    async fn live(&self, what: Live<'_>) -> Result<Option<String>> {
        match what {
            Live::JiraQuery { jql, max } => {
                let items = self.search(jql, max).await?;
                Ok(Some(serde_json::to_string(&items)?))
            }
            Live::JiraComments { key, max } => Ok(Some(self.comments(key, max).await?)),
        }
    }

    async fn write(&self, what: &WriteReq) -> Result<String> {
        match what {
            WriteReq::Comment { key, text } => self.add_comment(key, text).await,
            WriteReq::Move { key, status } => self.move_to(key, status).await,
        }
    }
}

/// `2026-10-13T11:40:02.000+0900` → unix seconds.
pub fn parse_jira_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3f%z")
        .ok()
        .map(|d| d.timestamp())
}

/// Atlassian Document Format → plain text. Same walk as the bash tool's jq filter.
pub fn adf_to_text(v: &Value) -> String {
    fn walk(v: &Value, out: &mut String) {
        let Some(obj) = v.as_object() else { return };
        let t = obj.get("type").and_then(Value::as_str).unwrap_or("");
        match t {
            "text" => out.push_str(obj.get("text").and_then(Value::as_str).unwrap_or("")),
            "hardBreak" => out.push('\n'),
            "mention" => {
                let text = obj
                    .get("attrs")
                    .and_then(|a| a["text"].as_str())
                    .unwrap_or("");
                if !text.starts_with('@') {
                    out.push('@');
                }
                out.push_str(text);
            }
            "emoji" => out.push_str(
                obj.get("attrs")
                    .and_then(|a| a["shortName"].as_str())
                    .unwrap_or(""),
            ),
            _ => {
                if t == "listItem" {
                    out.push_str("- ");
                }
                if let Some(c) = obj.get("content").and_then(Value::as_array) {
                    for child in c {
                        walk(child, out);
                    }
                }
                if matches!(
                    t,
                    "paragraph" | "heading" | "listItem" | "codeBlock" | "blockquote" | "tableRow"
                ) {
                    out.push('\n');
                }
            }
        }
    }
    let mut s = String::new();
    walk(v, &mut s);
    karl_core::text::collapse_ws(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adf() {
        let v: Value = serde_json::json!({"type":"doc","content":[
            {"type":"paragraph","content":[{"type":"text","text":"Hello "},{"type":"mention","attrs":{"text":"@Bob"}}]},
            {"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"one"}]}]}]}
        ]});
        assert_eq!(adf_to_text(&v), "Hello @Bob\n- one");
        assert_eq!(adf_to_text(&Value::Null), "");
    }

    #[test]
    fn ts() {
        let want = chrono::DateTime::parse_from_rfc3339("2026-10-13T11:40:02+09:00")
            .unwrap()
            .timestamp();
        assert_eq!(parse_jira_ts("2026-10-13T11:40:02.000+0900"), Some(want));
        assert!(parse_jira_ts("nope").is_none());
    }
}
