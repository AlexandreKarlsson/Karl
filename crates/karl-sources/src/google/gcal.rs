//! Google Calendar, read-only scope. Replaces the window
//! `[now - 1 day, now + days_ahead]` on every sync; events gone upstream are deleted.

use super::oauth::{parse_rfc3339, GoogleAuth};
use crate::http::get_json;
use crate::{extra_str, extra_u64, now, SharedStore, Source, SyncReport};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{Local, NaiveDate, TimeZone};
use karl_core::model::News;
use karl_core::policy::{Secrets, SourceCfg};
use karl_core::text::{html_to_text, truncate_chars};
use karl_core::{Item, Kind};
use serde_json::Value;

const API: &str = "https://www.googleapis.com/calendar/v3";

pub struct Gcal {
    id: String,
    auth: GoogleAuth,
    calendar: String,
    days_ahead: u64,
    scopes: Vec<String>,
}

impl Gcal {
    pub fn new(id: &str, cfg: &SourceCfg, secrets: &Secrets) -> Result<Gcal> {
        Ok(Gcal {
            id: id.to_string(),
            auth: GoogleAuth::from_secrets(secrets, &cfg.secret)?,
            calendar: extra_str(cfg, "calendar").unwrap_or("primary").to_string(),
            days_ahead: extra_u64(cfg, "days_ahead", 14),
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

    fn window(&self) -> (i64, i64) {
        let n = now();
        (n - 86400, n + self.days_ahead as i64 * 86400)
    }

    fn event_to_item(&self, e: &Value) -> Option<Item> {
        if e["status"].as_str() == Some("cancelled") {
            return None;
        }
        let id = e["id"].as_str()?;
        let (start, end, all_day) = event_times(e)?;
        let who = e["organizer"]["displayName"]
            .as_str()
            .or_else(|| e["organizer"]["email"].as_str())
            .unwrap_or("me")
            .to_string();
        let ts = e["updated"]
            .as_str()
            .and_then(parse_rfc3339)
            .unwrap_or(start);
        let mut it = Item::new(&self.id, Kind::Event, id, &who, ts);
        it.who_addr = e["organizer"]["email"].as_str().map(|s| s.to_lowercase());
        it.title = e["summary"].as_str().map(|s| s.trim().to_string());
        it.start = Some(start);
        it.end = Some(end);
        it.all_day = all_day;
        it.status = e["status"].as_str().map(String::from);
        it.labels = vec![e["visibility"].as_str().unwrap_or("default").to_string()];
        if let Some(loc) = e["location"].as_str() {
            it.labels.push(format!("at:{loc}"));
        }
        let desc = e["description"]
            .as_str()
            .map(html_to_text)
            .unwrap_or_default();
        it.body = truncate_chars(&desc, 1000);
        it.url = e["htmlLink"].as_str().map(String::from);
        Some(it)
    }
}

/// (start, end, all_day) in unix seconds. All-day events are local midnight to midnight.
fn event_times(e: &Value) -> Option<(i64, i64, bool)> {
    if let (Some(s), Some(en)) = (
        e["start"]["dateTime"].as_str(),
        e["end"]["dateTime"].as_str(),
    ) {
        return Some((parse_rfc3339(s)?, parse_rfc3339(en)?, false));
    }
    let day = |s: &str| -> Option<i64> {
        let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
        Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
            .single()
            .map(|t| t.timestamp())
    };
    let s = day(e["start"]["date"].as_str()?)?;
    let en = e["end"]["date"].as_str().and_then(day).unwrap_or(s + 86400);
    Some((s, en, true))
}

#[async_trait]
impl Source for Gcal {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "gcal"
    }
    fn group(&self) -> &'static str {
        "cal"
    }
    fn news(&self) -> News {
        News::Changes
    }

    async fn preflight(&self) -> Result<()> {
        self.auth.check_scopes(&self.scopes).await?;
        let v = self
            .get(&format!(
                "/calendars/{}",
                urlencoding::encode(&self.calendar)
            ))
            .await?;
        tracing::info!(source = %self.id, calendar = %v["summary"].as_str().unwrap_or("?"), "gcal ok");
        Ok(())
    }

    async fn sync(&self, store: &SharedStore) -> Result<SyncReport> {
        let (from, to) = self.window();
        let fmt = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap().to_rfc3339();
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut path = format!(
                "/calendars/{}/events?timeMin={}&timeMax={}&singleEvents=true&orderBy=startTime&maxResults=250",
                urlencoding::encode(&self.calendar),
                urlencoding::encode(&fmt(from)),
                urlencoding::encode(&fmt(to))
            );
            if let Some(t) = &token {
                path.push_str(&format!("&pageToken={}", urlencoding::encode(t)));
            }
            let v = self.get(&path).await?;
            items.extend(
                v["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| self.event_to_item(e)),
            );
            match v["nextPageToken"].as_str() {
                Some(t) if items.len() < 5000 => token = Some(t.to_string()),
                _ => break,
            }
        }
        let keep: Vec<String> = items.iter().map(|i| i.ext_id.clone()).collect();
        let mut s = store.lock().unwrap();
        let c = s.upsert_many(&items)?;
        let deleted = s.delete_missing(&self.id, Kind::Event, from, to, &keep)?;
        Ok(SyncReport {
            fetched: items.len(),
            inserted: c.inserted,
            updated: c.updated,
            deleted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let e = serde_json::json!({"start":{"dateTime":"2026-10-13T10:00:00+09:00"},"end":{"dateTime":"2026-10-13T11:00:00+09:00"}});
        let (s, en, all) = event_times(&e).unwrap();
        assert_eq!(en - s, 3600);
        assert!(!all);
        let e = serde_json::json!({"start":{"date":"2026-10-13"},"end":{"date":"2026-10-14"}});
        let (s, en, all) = event_times(&e).unwrap();
        assert_eq!(en - s, 86400);
        assert!(all);
    }
}
