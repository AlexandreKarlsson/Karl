//! Matrix client-server API, read-only: `/sync` with a stored batch token. This
//! is how WhatsApp, Messenger and Instagram reach Karl: a mautrix bridge puppets
//! the chat network into rooms on your own homeserver, and Karl reads the rooms.
//! Meta has no personal API; the bridge speaks the app's own protocol, which is
//! outside Meta's terms, so that risk sits with the bridge, not here.
//!
//! One source per network: `protocol = "whatsapp"` keeps only rooms whose
//! `m.bridge` state says so (mautrix sets `protocol.id`). No `protocol` = every room.

use crate::http::{bearer, client, get_json};
use crate::{extra_str, extra_u64, now, SharedStore, Source, SyncReport};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use karl_core::model::News;
use karl_core::policy::{Secrets, SourceCfg};
use karl_core::text::{collapse_ws, html_to_text, truncate_chars};
use karl_core::{Item, Kind};
use reqwest::header::HeaderMap;
use serde_json::Value;

const BODY_CHARS: usize = 1500;
/// Timeline events per room on the first sync (there is no cursor yet).
const INITIAL_LIMIT: usize = 30;

pub struct Matrix {
    id: String,
    homeserver: String,
    token: String,
    user_id: String,
    protocol: Option<String>,
    window_days: u64,
    client: reqwest::Client,
}

impl Matrix {
    pub fn new(id: &str, cfg: &SourceCfg, secrets: &Secrets) -> Result<Matrix> {
        let hs = extra_str(cfg, "homeserver")
            .with_context(|| format!("source {id}: homeserver missing"))?
            .trim_end_matches('/')
            .to_string();
        if !hs.starts_with("https://") && !hs.starts_with("http://") {
            bail!("source {id}: homeserver must be a url");
        }
        Ok(Matrix {
            id: id.to_string(),
            homeserver: hs,
            token: secrets.get(&cfg.secret, "access_token")?.to_string(),
            user_id: secrets.get(&cfg.secret, "user_id")?.to_string(),
            protocol: extra_str(cfg, "protocol").map(|s| s.to_lowercase()),
            window_days: extra_u64(cfg, "window_days", 14),
            client: client()?,
        })
    }

    fn headers(&self) -> Result<HeaderMap> {
        bearer(&self.token)
    }

    async fn get(&self, path_and_query: &str) -> Result<Value> {
        if !path_and_query.starts_with("/_matrix/") {
            bail!("matrix: refusing path {path_and_query}");
        }
        get_json(
            &self.client,
            &format!("{}{}", self.homeserver, path_and_query),
            self.headers()?,
        )
        .await
    }

    async fn sync_once(&self, since: Option<&str>) -> Result<Value> {
        let filter = serde_json::json!({
            "room": {
                "timeline": { "limit": INITIAL_LIMIT, "lazy_load_members": true },
                "state": { "lazy_load_members": true },
                "ephemeral": { "types": [] },
                "account_data": { "types": [] }
            },
            "presence": { "types": [] },
            "account_data": { "types": [] }
        });
        let mut q = format!(
            "/_matrix/client/v3/sync?timeout=0&filter={}",
            urlencoding::encode(&filter.to_string())
        );
        if let Some(s) = since {
            q.push_str(&format!("&since={}", urlencoding::encode(s)));
        }
        self.get(&q).await
    }

    /// Protocol id of a bridged room from its `m.bridge` state event, if any.
    fn bridge_protocol(room: &Value) -> Option<String> {
        for section in ["state", "timeline"] {
            for ev in room[section]["events"].as_array().into_iter().flatten() {
                if ev["type"].as_str() == Some("m.bridge") {
                    if let Some(p) = ev["content"]["protocol"]["id"].as_str() {
                        return Some(p.to_lowercase());
                    }
                }
            }
        }
        None
    }

    fn room_name(room: &Value) -> Option<String> {
        for section in ["state", "timeline"] {
            for ev in room[section]["events"].as_array().into_iter().flatten() {
                if ev["type"].as_str() == Some("m.room.name") {
                    if let Some(n) = ev["content"]["name"].as_str() {
                        let n = n.trim();
                        if !n.is_empty() {
                            return Some(n.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    fn event_to_item(
        &self,
        ev: &Value,
        room_id: &str,
        room_name: Option<&str>,
        names: &std::collections::HashMap<String, String>,
    ) -> Option<Item> {
        if ev["type"].as_str() != Some("m.room.message") {
            return None;
        }
        let id = ev["event_id"].as_str()?;
        let sender = ev["sender"].as_str().unwrap_or("?");
        let c = &ev["content"];
        let msgtype = c["msgtype"].as_str().unwrap_or("m.text");
        let mut body = match msgtype {
            "m.text" | "m.notice" | "m.emote" => {
                if c["format"].as_str() == Some("org.matrix.custom.html") {
                    if let Some(h) = c["formatted_body"].as_str() {
                        html_to_text(h)
                    } else {
                        collapse_ws(c["body"].as_str().unwrap_or(""))
                    }
                } else {
                    collapse_ws(c["body"].as_str().unwrap_or(""))
                }
            }
            "m.image" => "[image]".to_string(),
            "m.video" => "[video]".to_string(),
            "m.audio" => "[voice]".to_string(),
            "m.file" => format!("[file {}]", c["body"].as_str().unwrap_or("")),
            "m.location" => "[location]".to_string(),
            _ => collapse_ws(c["body"].as_str().unwrap_or("")),
        };
        // a reply quotes the original in body: keep what follows the quote
        if c["m.relates_to"]["m.in_reply_to"].is_object() {
            if let Some(pos) = body.rfind("\n\n") {
                body = body[pos + 2..].to_string();
            }
        }
        if body.trim().is_empty() {
            return None;
        }
        let who = if sender == self.user_id {
            "me".to_string()
        } else {
            names
                .get(sender)
                .cloned()
                .unwrap_or_else(|| localpart(sender))
        };
        let ts = ev["origin_server_ts"]
            .as_i64()
            .map(|ms| ms / 1000)
            .unwrap_or_else(now);
        let mut it = Item::new(&self.id, Kind::Msg, id, &who, ts);
        it.who_addr = Some(sender.to_string());
        it.thread = Some(room_id.to_string());
        it.thread_name = room_name.map(String::from);
        it.body = truncate_chars(&body, BODY_CHARS);
        Some(it)
    }
}

/// `@whatsapp_8190…:home` → `whatsapp_8190…`; bridges give puppets a display name anyway.
fn localpart(mxid: &str) -> String {
    mxid.trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or(mxid)
        .to_string()
}

#[async_trait]
impl Source for Matrix {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "matrix"
    }
    fn group(&self) -> &'static str {
        "msg"
    }
    fn news(&self) -> News {
        News::Arrivals
    }

    async fn preflight(&self) -> Result<()> {
        let v = self.get("/_matrix/client/v3/account/whoami").await?;
        let who = v["user_id"].as_str().unwrap_or("?");
        if who != self.user_id {
            bail!(
                "matrix: token belongs to {who}, config says {}",
                self.user_id
            );
        }
        tracing::info!(source = %self.id, who, "matrix ok");
        Ok(())
    }

    async fn sync(&self, store: &SharedStore) -> Result<SyncReport> {
        let since_key = format!("source:{}:since", self.id);
        let since = store.lock().unwrap().meta_get(&since_key)?;
        let v = self.sync_once(since.as_deref()).await?;
        let next = v["next_batch"]
            .as_str()
            .context("sync: no next_batch")?
            .to_string();
        let cutoff = now() - self.window_days as i64 * 86400;
        let mut items = Vec::new();
        let mut fetched = 0;
        let rooms = &v["rooms"]["join"];
        let s = store.lock().unwrap();
        for (room_id, room) in rooms.as_object().into_iter().flatten() {
            // which bridge this room belongs to is state: learned once, remembered
            let proto_key = format!("source:{}:room:{room_id}:protocol", self.id);
            let proto = match Self::bridge_protocol(room) {
                Some(p) => {
                    s.meta_set(&proto_key, &p)?;
                    Some(p)
                }
                None => s.meta_get(&proto_key)?,
            };
            if let Some(want) = &self.protocol {
                if proto.as_deref() != Some(want.as_str()) {
                    continue;
                }
            }
            let name_key = format!("source:{}:room:{room_id}:name", self.id);
            let name = match Self::room_name(room) {
                Some(n) => {
                    s.meta_set(&name_key, &n)?;
                    Some(n)
                }
                None => s.meta_get(&name_key)?,
            };
            // display names of senders, from lazy-loaded member state
            let mut names = std::collections::HashMap::new();
            for section in ["state", "timeline"] {
                for ev in room[section]["events"].as_array().into_iter().flatten() {
                    if ev["type"].as_str() == Some("m.room.member") {
                        if let (Some(k), Some(n)) = (
                            ev["state_key"].as_str(),
                            ev["content"]["displayname"].as_str(),
                        ) {
                            names.insert(k.to_string(), n.to_string());
                        }
                    }
                }
            }
            for ev in room["timeline"]["events"].as_array().into_iter().flatten() {
                fetched += 1;
                if let Some(it) = self.event_to_item(ev, room_id, name.as_deref(), &names) {
                    if it.ts >= cutoff {
                        items.push(it);
                    }
                }
            }
        }
        drop(s);
        items.sort_by_key(|i| i.ts);
        let mut s = store.lock().unwrap();
        let c = s.upsert_many(&items)?;
        s.meta_set(&since_key, &next)?;
        Ok(SyncReport {
            fetched,
            inserted: c.inserted,
            updated: c.updated,
            deleted: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> Matrix {
        Matrix {
            id: "wa".into(),
            homeserver: "https://m.example".into(),
            token: "t".into(),
            user_id: "@alex:m.example".into(),
            protocol: Some("whatsapp".into()),
            window_days: 14,
            client: client().unwrap(),
        }
    }

    #[test]
    fn message_event() {
        let m = src();
        let names = std::collections::HashMap::from([(
            "@whatsapp_1:m.example".to_string(),
            "Bob".to_string(),
        )]);
        let ev = serde_json::json!({"type":"m.room.message","event_id":"$1","sender":"@whatsapp_1:m.example",
            "origin_server_ts": 1_700_000_000_000i64, "content":{"msgtype":"m.text","body":"hi  there"}});
        let it = m.event_to_item(&ev, "!r", Some("Bob"), &names).unwrap();
        assert_eq!(it.who, "Bob");
        assert_eq!(it.body, "hi there");
        assert_eq!(it.ts, 1_700_000_000);
        let mine = serde_json::json!({"type":"m.room.message","event_id":"$2","sender":"@alex:m.example",
            "content":{"msgtype":"m.image","body":"x.jpg"}});
        assert_eq!(
            m.event_to_item(&mine, "!r", None, &names).unwrap().who,
            "me"
        );
        let state = serde_json::json!({"type":"m.room.member","event_id":"$3","sender":"@x:y","content":{}});
        assert!(m.event_to_item(&state, "!r", None, &names).is_none());
    }

    #[test]
    fn bridge_state() {
        let room = serde_json::json!({"state":{"events":[
            {"type":"m.bridge","content":{"protocol":{"id":"WhatsApp"}}},
            {"type":"m.room.name","content":{"name":"Family"}}]}, "timeline":{"events":[]}});
        assert_eq!(Matrix::bridge_protocol(&room).as_deref(), Some("whatsapp"));
        assert_eq!(Matrix::room_name(&room).as_deref(), Some("Family"));
        assert_eq!(localpart("@whatsapp_81:m"), "whatsapp_81");
    }
}
