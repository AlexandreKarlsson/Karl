//! Gmail, read-only scope. Lists message ids newer than `window_days`, fetches
//! the ones the store has not seen, stores them as plain text.

use super::oauth::GoogleAuth;
use crate::http::get_json;
use crate::{extra_u64, now, SharedStore, Source, SyncReport};
use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use karl_core::model::News;
use karl_core::policy::{Secrets, SourceCfg};
use karl_core::text::{collapse_ws, html_to_text, strip_quotes, truncate_chars};
use karl_core::{Item, Kind};
use serde_json::Value;
use std::collections::HashMap;

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
/// New messages fetched per sync pass; the next pass continues.
const FETCH_PER_PASS: usize = 60;
/// Chars of body kept per mail.
const BODY_CHARS: usize = 4000;

pub struct Gmail {
    id: String,
    auth: GoogleAuth,
    window_days: u64,
    scopes: Vec<String>,
}

impl Gmail {
    pub fn new(id: &str, cfg: &SourceCfg, secrets: &Secrets) -> Result<Gmail> {
        Ok(Gmail {
            id: id.to_string(),
            auth: GoogleAuth::from_secrets(secrets, &cfg.secret)?,
            window_days: extra_u64(cfg, "window_days", 14),
            scopes: cfg.scopes.clone(),
        })
    }

    async fn get(&self, path: &str) -> Result<Value> {
        get_json(
            self.auth.client(),
            &format!("{API}{path}"),
            self.auth.headers().await?,
        )
        .await
    }

    async fn label_names(&self) -> Result<HashMap<String, String>> {
        let v = self.get("/labels").await?;
        Ok(v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| {
                Some((
                    l["id"].as_str()?.to_string(),
                    l["name"].as_str()?.to_string(),
                ))
            })
            .collect())
    }

    async fn list_ids(&self) -> Result<Vec<String>> {
        let q = format!("newer_than:{}d -in:spam -in:trash", self.window_days);
        let mut ids = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut path = format!("/messages?q={}&maxResults=100", urlencoding::encode(&q));
            if let Some(t) = &token {
                path.push_str(&format!("&pageToken={}", urlencoding::encode(t)));
            }
            let v = self.get(&path).await?;
            ids.extend(
                v["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["id"].as_str().map(String::from)),
            );
            match v["nextPageToken"].as_str() {
                Some(t) if ids.len() < 2000 => token = Some(t.to_string()),
                _ => break,
            }
        }
        Ok(ids)
    }

    fn message_to_item(&self, v: &Value, labels: &HashMap<String, String>) -> Option<Item> {
        let id = v["id"].as_str()?;
        let headers: HashMap<String, String> = v["payload"]["headers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| {
                Some((
                    h["name"].as_str()?.to_ascii_lowercase(),
                    h["value"].as_str()?.to_string(),
                ))
            })
            .collect();
        let (who, addr) = split_from(headers.get("from").map(String::as_str).unwrap_or("?"));
        let ts = v["internalDate"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .map(|ms| ms / 1000)
            .unwrap_or_else(now);
        let mut it = Item::new(&self.id, Kind::Mail, id, &who, ts);
        it.who_addr = Some(addr);
        it.title = headers
            .get("subject")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        it.thread = v["threadId"].as_str().map(String::from);
        it.labels = v["labelIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|l| labels.get(l).cloned().unwrap_or_else(|| l.to_string()))
            .collect();
        let body = extract_body(&v["payload"])
            .unwrap_or_else(|| v["snippet"].as_str().unwrap_or("").to_string());
        it.body = truncate_chars(&strip_quotes(&body), BODY_CHARS);
        it.url = Some(format!("https://mail.google.com/mail/#all/{id}"));
        Some(it)
    }
}

#[async_trait]
impl Source for Gmail {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "gmail"
    }
    fn group(&self) -> &'static str {
        "mail"
    }
    fn news(&self) -> News {
        News::Arrivals
    }

    async fn preflight(&self) -> Result<()> {
        self.auth.check_scopes(&self.scopes).await?;
        let v = self.get("/profile").await?;
        tracing::info!(source = %self.id, account = %v["emailAddress"].as_str().unwrap_or("?"), "gmail ok");
        Ok(())
    }

    async fn sync(&self, store: &SharedStore) -> Result<SyncReport> {
        let labels = self.label_names().await?;
        let ids = self.list_ids().await?;
        let fresh: Vec<String> = {
            let s = store.lock().unwrap();
            ids.into_iter()
                .filter(|id| !matches!(s.get(&self.id, id), Ok(Some(_))))
                .collect()
        };
        let mut items = Vec::new();
        let mut failed = 0usize;
        for id in fresh.iter().take(FETCH_PER_PASS) {
            // one message that 404s (deleted meanwhile) or 5xxs must not wedge the
            // whole source forever: skip it this pass, it is retried next time
            let v = match self.get(&format!("/messages/{id}?format=full")).await {
                Ok(v) => v,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(source = %self.id, id = %id, "message fetch failed: {e:#}");
                    continue;
                }
            };
            if let Some(it) = self.message_to_item(&v, &labels) {
                items.push(it);
            }
        }
        if failed > 0 && items.is_empty() {
            anyhow::bail!("gmail: every message fetch failed this pass ({failed})");
        }
        // oldest first so seq order follows time
        items.sort_by_key(|i| i.ts);
        let c = store.lock().unwrap().upsert_many(&items)?;
        if fresh.len() > FETCH_PER_PASS {
            tracing::info!(source = %self.id, left = fresh.len() - FETCH_PER_PASS, "more mails next pass");
        }
        Ok(SyncReport {
            fetched: items.len(),
            inserted: c.inserted,
            updated: c.updated,
            deleted: 0,
        })
    }
}

/// `"Alice Smith" <alice@x.com>` → (Alice Smith, alice@x.com); bare address → (alice, alice@x.com).
pub fn split_from(from: &str) -> (String, String) {
    let from = from.trim();
    if let Some(lt) = from.find('<') {
        let name = from[..lt].trim().trim_matches('"').trim();
        let addr = from[lt + 1..].trim_end_matches('>').trim();
        let name = if name.is_empty() {
            addr.split('@').next().unwrap_or(addr)
        } else {
            name
        };
        return (name.to_string(), addr.to_lowercase());
    }
    let addr = from.trim_matches(|c| c == '<' || c == '>');
    (
        addr.split('@').next().unwrap_or(addr).to_string(),
        addr.to_lowercase(),
    )
}

/// Prefer text/plain, else text/html stripped. Walks multipart recursively.
fn extract_body(payload: &Value) -> Option<String> {
    fn find(part: &Value, mime: &str) -> Option<String> {
        if part["mimeType"].as_str() == Some(mime) {
            if let Some(d) = part["body"]["data"].as_str() {
                return decode_b64url(d);
            }
        }
        for p in part["parts"].as_array().into_iter().flatten() {
            if let Some(s) = find(p, mime) {
                return Some(s);
            }
        }
        None
    }
    if let Some(t) = find(payload, "text/plain") {
        let t = collapse_ws(&t);
        if !t.is_empty() {
            return Some(t);
        }
    }
    find(payload, "text/html").map(|h| html_to_text(&h))
}

fn decode_b64url(d: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(d.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(d))
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_header() {
        assert_eq!(
            split_from("\"Alice Smith\" <Alice@X.com>"),
            ("Alice Smith".into(), "alice@x.com".into())
        );
        assert_eq!(split_from("bob@y.org"), ("bob".into(), "bob@y.org".into()));
        assert_eq!(split_from("<c@z.io>"), ("c".into(), "c@z.io".into()));
    }

    #[test]
    fn body_prefers_plain() {
        let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let p = serde_json::json!({"mimeType":"multipart/alternative","parts":[
            {"mimeType":"text/html","body":{"data":b64("<p>Hi <b>there</b></p>")}},
            {"mimeType":"text/plain","body":{"data":b64("Hi there\n")}}
        ]});
        assert_eq!(extract_body(&p).as_deref(), Some("Hi there"));
        let p = serde_json::json!({"mimeType":"text/html","body":{"data":b64("<p>Only <i>html</i></p>")}});
        assert_eq!(extract_body(&p).as_deref(), Some("Only html"));
    }
}
