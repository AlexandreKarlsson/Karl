//! Microsoft Teams chats over Microsoft Graph, delegated permission `Chat.Read`
//! (a user can consent to it unless the tenant forbids user consent; nothing
//! here needs admin consent). Channel messages are not read: `ChannelMessage.Read.All`
//! is admin-consent only and most tenants restrict it, which is the point.
//!
//! Sync: the user's chats ordered by last activity, then the recent messages of
//! each chat touched inside the window. Messages are chat items (`msg` group),
//! one per Graph message id; system events (members added…) are skipped.

use super::oauth::MsAuth;
use crate::http::get_json;
use crate::{extra_u64, now, SharedStore, Source, SyncReport};
use anyhow::{Context, Result};
use async_trait::async_trait;
use karl_core::model::News;
use karl_core::policy::{Secrets, SourceCfg};
use karl_core::text::{collapse_ws, html_to_text, truncate_chars};
use karl_core::{Item, Kind};
use serde_json::Value;

const API: &str = "https://graph.microsoft.com/v1.0";
const SCOPES: &[&str] = &["offline_access", "User.Read", "Chat.Read"];
/// Chats looked at per pass, newest activity first.
const CHATS_PER_PASS: usize = 30;
/// Messages fetched per chat per pass.
const MSGS_PER_CHAT: usize = 30;
const BODY_CHARS: usize = 1500;

pub struct Teams {
    id: String,
    auth: MsAuth,
    window_days: u64,
}

impl Teams {
    pub fn new(
        id: &str,
        cfg: &SourceCfg,
        secrets: &Secrets,
        secrets_path: &std::path::Path,
    ) -> Result<Teams> {
        let scopes = if cfg.scopes.is_empty() {
            default_scopes()
        } else {
            cfg.scopes.clone()
        };
        Ok(Teams {
            id: id.to_string(),
            auth: MsAuth::from_secrets(secrets, secrets_path, &cfg.secret, &scopes)?,
            window_days: extra_u64(cfg, "window_days", 14),
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

    async fn me(&self) -> Result<(String, String)> {
        let v = self.get("/me?$select=id,displayName").await?;
        Ok((
            v["id"].as_str().context("me: no id")?.to_string(),
            v["displayName"].as_str().unwrap_or("me").to_string(),
        ))
    }

    fn message_to_item(&self, m: &Value, chat_id: &str, chat_name: &str, me: &str) -> Option<Item> {
        if m["messageType"].as_str() != Some("message") || m["deletedDateTime"].as_str().is_some() {
            return None;
        }
        let id = m["id"].as_str()?;
        let from = &m["from"]["user"];
        let addr = from["id"].as_str().unwrap_or("");
        let who = if addr == me {
            "me".to_string()
        } else {
            from["displayName"]
                .as_str()
                .or_else(|| m["from"]["application"]["displayName"].as_str())
                .unwrap_or("?")
                .to_string()
        };
        let ts = m["createdDateTime"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp())
            .unwrap_or_else(now);
        let raw = m["body"]["content"].as_str().unwrap_or("");
        let mut body = if m["body"]["contentType"].as_str() == Some("html") {
            html_to_text(raw)
        } else {
            collapse_ws(raw)
        };
        let n_att = m["attachments"].as_array().map(|a| a.len()).unwrap_or(0);
        if n_att > 0 {
            if !body.is_empty() {
                body.push(' ');
            }
            body.push_str(&format!("[{n_att} attachment(s)]"));
        }
        if body.is_empty() {
            return None;
        }
        let mut it = Item::new(&self.id, Kind::Msg, id, &who, ts);
        it.who_addr = (!addr.is_empty()).then(|| addr.to_string());
        it.thread = Some(chat_id.to_string());
        it.thread_name = (!chat_name.is_empty()).then(|| chat_name.to_string());
        it.body = truncate_chars(&body, BODY_CHARS);
        it.url = m["webUrl"].as_str().map(String::from);
        Some(it)
    }
}

pub fn default_scopes() -> Vec<String> {
    SCOPES.iter().map(|s| s.to_string()).collect()
}

/// A chat's name: its topic, else the other members' names (without me).
fn chat_name(c: &Value, me: &str) -> String {
    if let Some(t) = c["topic"].as_str().filter(|t| !t.trim().is_empty()) {
        return t.trim().to_string();
    }
    let names: Vec<&str> = c["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["userId"].as_str() != Some(me))
        .filter_map(|m| m["displayName"].as_str())
        .collect();
    names.join(", ")
}

#[async_trait]
impl Source for Teams {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "teams"
    }
    fn group(&self) -> &'static str {
        "msg"
    }
    fn news(&self) -> News {
        News::Arrivals
    }

    async fn preflight(&self) -> Result<()> {
        let (_, name) = self.me().await?;
        let v = self.get("/me/chats?$top=1").await?;
        tracing::info!(source = %self.id, who = %name, chats = v["value"].as_array().map(|a| a.len()).unwrap_or(0), "teams ok");
        Ok(())
    }

    async fn sync(&self, store: &SharedStore) -> Result<SyncReport> {
        let me_key = format!("source:{}:me", self.id);
        let me: String = store.lock().unwrap().meta_get(&me_key)?.unwrap_or_default();
        let me = if me.is_empty() {
            let (id, _) = self.me().await?;
            store.lock().unwrap().meta_set(&me_key, &id)?;
            id
        } else {
            me
        };
        let since = now() - self.window_days as i64 * 86400;
        let chats = self
            .get(&format!(
                "/me/chats?$expand=members&$orderby=lastMessagePreview/createdDateTime desc&$top={CHATS_PER_PASS}"
            ))
            .await?;
        let mut items = Vec::new();
        for c in chats["value"].as_array().into_iter().flatten() {
            let Some(cid) = c["id"].as_str() else {
                continue;
            };
            let last = c["lastMessagePreview"]["createdDateTime"]
                .as_str()
                .or_else(|| c["lastUpdatedDateTime"].as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.timestamp())
                .unwrap_or(0);
            if last != 0 && last < since {
                continue;
            }
            let name = chat_name(c, &me);
            let msgs = match self
                .get(&format!(
                    "/chats/{}/messages?$top={MSGS_PER_CHAT}",
                    urlencoding::encode(cid)
                ))
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(source = %self.id, chat = %cid, "messages: {e:#}");
                    continue;
                }
            };
            for m in msgs["value"].as_array().into_iter().flatten() {
                if let Some(it) = self.message_to_item(m, cid, &name, &me) {
                    if it.ts >= since {
                        items.push(it);
                    }
                }
            }
        }
        items.sort_by_key(|i| i.ts);
        let c = store.lock().unwrap().upsert_many(&items)?;
        Ok(SyncReport {
            fetched: items.len(),
            inserted: c.inserted,
            updated: c.updated,
            deleted: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_a_chat() {
        let c = serde_json::json!({"topic": null, "members": [
            {"userId":"me","displayName":"Me"}, {"userId":"b","displayName":"Bob"}]});
        assert_eq!(chat_name(&c, "me"), "Bob");
        let c = serde_json::json!({"topic": "Launch", "members": []});
        assert_eq!(chat_name(&c, "me"), "Launch");
    }
}
