//! CalDAV (RFC 4791) and plain `.ics` feeds. Covers Yahoo! JAPAN カレンダー
//! (`https://caldav.yahoo.co.jp/dav/<id>/Calendar/<calendar>/`), Nextcloud,
//! iCloud, Fastmail: anything that answers a `calendar-query` REPORT. Read-only:
//! the source has no write path at all.
//!
//! Window `[now - 1 day, now + days_ahead]` is replaced on every sync, with
//! recurrences expanded by the server (`<C:expand>`), so an instance is one
//! item and an event gone upstream is deleted from the cache.

use crate::http::{basic, client, request_text};
use crate::ics;
use crate::{extra_str, extra_u64, now, SharedStore, Source, SyncReport};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use karl_core::model::News;
use karl_core::policy::{Secrets, SourceCfg};
use karl_core::text::{collapse_ws, truncate_chars};
use karl_core::{Item, Kind};
use reqwest::header::{HeaderMap, HeaderValue};

pub struct CalDav {
    id: String,
    url: String,
    /// `.ics` feed: one GET, no REPORT. Set when the url ends with `.ics` or `feed = true`.
    feed: bool,
    auth: Option<(String, String)>,
    days_ahead: u64,
    client: reqwest::Client,
}

impl CalDav {
    pub fn new(id: &str, cfg: &SourceCfg, secrets: &Secrets) -> Result<CalDav> {
        let url = extra_str(cfg, "url")
            .with_context(|| format!("source {id}: url missing"))?
            .to_string();
        if !url.starts_with("https://") {
            bail!("source {id}: url must be https://");
        }
        let feed = url.ends_with(".ics")
            || cfg
                .extra
                .get("feed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        // a feed may be a public, unauthenticated url: the secret table is then optional
        let auth = match (
            secrets.get(&cfg.secret, "user"),
            secrets.get(&cfg.secret, "password"),
        ) {
            (Ok(u), Ok(p)) => Some((u.to_string(), p.to_string())),
            _ if feed => None,
            (Err(e), _) | (_, Err(e)) => return Err(e),
        };
        Ok(CalDav {
            id: id.to_string(),
            url: if feed { url } else { ensure_slash(&url) },
            feed,
            auth,
            days_ahead: extra_u64(cfg, "days_ahead", 14),
            client: client()?,
        })
    }

    fn headers(&self) -> Result<HeaderMap> {
        match &self.auth {
            Some((u, p)) => basic(u, p),
            None => Ok(HeaderMap::new()),
        }
    }

    fn window(&self) -> (i64, i64) {
        let n = now();
        (n - 86400, n + self.days_ahead as i64 * 86400)
    }

    /// The raw iCalendar text for the window: a GET for a feed, a REPORT for CalDAV.
    async fn fetch(&self) -> Result<String> {
        if self.feed {
            let (_, text) = request_text(
                &self.client,
                reqwest::Method::GET,
                &self.url,
                self.headers()?,
                None,
            )
            .await?;
            return Ok(text);
        }
        let (from, to) = self.window();
        let fmt = |t: i64| {
            chrono::DateTime::from_timestamp(t, 0)
                .unwrap()
                .format("%Y%m%dT%H%M%SZ")
                .to_string()
        };
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><C:calendar-data><C:expand start="{0}" end="{1}"/></C:calendar-data></D:prop>
  <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
    <C:time-range start="{0}" end="{1}"/>
  </C:comp-filter></C:comp-filter></C:filter>
</C:calendar-query>"#,
            fmt(from),
            fmt(to)
        );
        let mut h = self.headers()?;
        h.insert("Depth", HeaderValue::from_static("1"));
        h.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        );
        let (status, text) = request_text(
            &self.client,
            reqwest::Method::from_bytes(b"REPORT").unwrap(),
            &self.url,
            h,
            Some(body),
        )
        .await?;
        if status.as_u16() != 207 {
            bail!("caldav: REPORT answered {status}, not 207 multistatus");
        }
        Ok(calendar_data(&text).join("\n"))
    }

    fn to_item(&self, e: &ics::VEvent) -> Item {
        let who = e.organizer.clone().unwrap_or_else(|| "me".into());
        let ts = e.last_modified.unwrap_or(e.start);
        let mut it = Item::new(&self.id, Kind::Event, &e.ext_id(), &who, ts);
        it.title = e.summary.clone();
        it.start = Some(e.start);
        it.end = Some(e.end);
        it.all_day = e.all_day;
        it.status = e.status.clone();
        it.labels = vec!["default".into()];
        if let Some(loc) = &e.location {
            it.labels.push(format!("at:{loc}"));
        }
        it.body = truncate_chars(&collapse_ws(&e.description), 1000);
        it.url = e.url.clone();
        it
    }
}

#[async_trait]
impl Source for CalDav {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "caldav"
    }
    fn group(&self) -> &'static str {
        "cal"
    }
    fn news(&self) -> News {
        News::Changes
    }

    async fn preflight(&self) -> Result<()> {
        let text = self.fetch().await?;
        let n = ics::parse(&text).len();
        tracing::info!(source = %self.id, events = n, "caldav ok");
        Ok(())
    }

    async fn sync(&self, store: &SharedStore) -> Result<SyncReport> {
        let (from, to) = self.window();
        let text = self.fetch().await?;
        let items: Vec<Item> = ics::parse(&text)
            .iter()
            .filter(|e| e.start < to && e.end > from)
            .map(|e| self.to_item(e))
            .collect();
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

fn ensure_slash(u: &str) -> String {
    if u.ends_with('/') {
        u.to_string()
    } else {
        format!("{u}/")
    }
}

/// Every `<calendar-data>` text of a multistatus reply, XML-unescaped. No XML
/// parser: the element holds text only, and the prefix varies per server.
pub fn calendar_data(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find("calendar-data") {
        // the opening tag ends at the next '>' after the name; a self-closing tag has no body
        let Some(gt) = rest[open..].find('>') else {
            break;
        };
        let tag_end = open + gt + 1;
        if rest[open..tag_end].ends_with("/>")
            || rest[..open].ends_with("</")
            || rest[..open].ends_with("</C:")
        {
            rest = &rest[tag_end..];
            continue;
        }
        let body_start = tag_end;
        let Some(close) = rest[body_start..].find("calendar-data>") else {
            break;
        };
        // walk back to the '<' that starts the closing tag
        let close_abs = body_start + close;
        let lt = rest[..close_abs].rfind('<').unwrap_or(close_abs);
        let body = &rest[body_start..lt];
        let body = body.trim();
        let body = body
            .strip_prefix("<![CDATA[")
            .and_then(|b| b.strip_suffix("]]>"))
            .map(str::to_string)
            .unwrap_or_else(|| xml_unescape(body));
        if !body.is_empty() {
            out.push(body);
        }
        rest = &rest[close_abs + "calendar-data>".len()..];
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#13;", "\r")
        .replace("&#10;", "\n")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_calendar_data() {
        let xml = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
<D:response><D:href>/a.ics</D:href><D:propstat><D:prop><C:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:1
SUMMARY:A &amp; B
DTSTART:20261015T010000Z
END:VEVENT
END:VCALENDAR
</C:calendar-data></D:prop></D:propstat></D:response>
<D:response><D:propstat><D:prop><cal:calendar-data xmlns:cal="urn:ietf:params:xml:ns:caldav"><![CDATA[BEGIN:VEVENT
UID:2
DTSTART;VALUE=DATE:20261016
END:VEVENT]]></cal:calendar-data></D:prop></D:propstat></D:response>
</D:multistatus>"#;
        let d = calendar_data(xml);
        assert_eq!(d.len(), 2);
        assert!(d[0].contains("SUMMARY:A & B"));
        let ev = ics::parse(&d.join("\n"));
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].summary.as_deref(), Some("A & B"));
        assert!(ev[1].all_day);
    }

    #[test]
    fn empty_and_selfclosing() {
        assert!(calendar_data("<D:multistatus/>").is_empty());
        assert!(calendar_data("<C:calendar-data/>").is_empty());
    }
}
