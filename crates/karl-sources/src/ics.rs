//! iCalendar (RFC 5545) → events. Enough for what CalDAV servers and `.ics`
//! feeds return: unfolding, escapes, DTSTART/DTEND/DURATION, all-day dates,
//! `Z` and `TZID` times. A `TZID` is taken as the daemon's local zone: the
//! daemon runs in the zone the calendars are kept in (`TZ` in the unit), and
//! CalDAV `expand` replies are in UTC anyway. No recurrence expansion here:
//! ask the server to expand (`CalDav` does), a plain `.ics` feed shows the
//! first instance only.

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VEvent {
    pub uid: String,
    /// `RECURRENCE-ID` of an expanded instance, so each instance has its own id.
    pub recurrence_id: Option<String>,
    pub summary: Option<String>,
    pub description: String,
    pub location: Option<String>,
    pub status: Option<String>,
    pub organizer: Option<String>,
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub last_modified: Option<i64>,
    pub url: Option<String>,
}

impl VEvent {
    pub fn ext_id(&self) -> String {
        match &self.recurrence_id {
            Some(r) => format!("{}@{r}", self.uid),
            None => self.uid.clone(),
        }
    }
}

/// Every VEVENT in the text. Lines that do not parse are skipped, not fatal.
pub fn parse(text: &str) -> Vec<VEvent> {
    let mut out = Vec::new();
    let mut cur: Option<BTreeMap<String, (String, String)>> = None; // name → (params, value)
    for line in unfold(text) {
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            cur = Some(BTreeMap::new());
            continue;
        }
        if line.eq_ignore_ascii_case("END:VEVENT") {
            if let Some(props) = cur.take() {
                if let Some(ev) = build(&props) {
                    out.push(ev);
                }
            }
            continue;
        }
        let Some(props) = cur.as_mut() else { continue };
        let Some((head, value)) = line.split_once(':') else {
            continue;
        };
        // DTSTART;TZID=Asia/Tokyo:20261013T100000  → name DTSTART, params TZID=…
        let (name, params) = match head.split_once(';') {
            Some((n, p)) => (n, p),
            None => (head, ""),
        };
        props.insert(
            name.to_ascii_uppercase(),
            (params.to_string(), value.to_string()),
        );
    }
    out
}

/// RFC 5545 line folding: a line starting with a space or tab continues the previous one.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let l = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = l.strip_prefix(' ').or_else(|| l.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(l.to_string());
    }
    lines
}

/// `\n` `\,` `\;` `\\` → the characters.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn build(p: &BTreeMap<String, (String, String)>) -> Option<VEvent> {
    let uid = p.get("UID")?.1.trim().to_string();
    let (start, all_day) = date_time(p.get("DTSTART")?)?;
    let end = match p.get("DTEND") {
        Some(e) => date_time(e)?.0,
        None => match p.get("DURATION") {
            Some((_, d)) => start + duration_secs(d).unwrap_or(0),
            None => start + if all_day { 86400 } else { 0 },
        },
    };
    let text = |k: &str| {
        p.get(k)
            .map(|(_, v)| unescape(v).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let organizer = p.get("ORGANIZER").map(|(params, v)| {
        params
            .split(';')
            .find_map(|kv| kv.strip_prefix("CN="))
            .map(|cn| cn.trim_matches('"').to_string())
            .unwrap_or_else(|| v.trim_start_matches("mailto:").to_string())
    });
    Some(VEvent {
        uid,
        recurrence_id: p.get("RECURRENCE-ID").map(|(_, v)| v.trim().to_string()),
        summary: text("SUMMARY"),
        description: text("DESCRIPTION").unwrap_or_default(),
        location: text("LOCATION"),
        status: text("STATUS").map(|s| s.to_lowercase()),
        organizer,
        start,
        end,
        all_day,
        last_modified: p
            .get("LAST-MODIFIED")
            .or_else(|| p.get("DTSTAMP"))
            .and_then(|v| date_time(v).map(|(t, _)| t)),
        url: text("URL"),
    })
}

/// (unix seconds, all_day). Dates are local midnight; `Z` is UTC; `TZID`/floating are local.
fn date_time((params, value): &(String, String)) -> Option<(i64, bool)> {
    let v = value.trim();
    if params.contains("VALUE=DATE") || (v.len() == 8 && !v.contains('T')) {
        let d = NaiveDate::parse_from_str(v, "%Y%m%d").ok()?;
        let t = Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
            .single()?
            .timestamp();
        return Some((t, true));
    }
    if let Some(utc) = v.strip_suffix('Z') {
        let dt = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some((dt.and_utc().timestamp(), false));
    }
    let dt = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok()?;
    let t = Local.from_local_datetime(&dt).single()?.timestamp();
    Some((t, false))
}

/// `PT1H30M`, `P1D`, `-PT15M` → seconds.
fn duration_secs(d: &str) -> Option<i64> {
    let (neg, rest) = match d.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, d.strip_prefix('+').unwrap_or(d)),
    };
    let rest = rest.strip_prefix('P')?;
    let mut total = 0i64;
    let mut num = String::new();
    let mut in_time = false;
    for c in rest.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            'W' | 'D' | 'H' | 'M' | 'S' => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += n * match c {
                    'W' => 7 * 86400,
                    'D' => 86400,
                    'H' => 3600,
                    'M' if in_time => 60,
                    'S' => 1,
                    _ => return None,
                };
            }
            _ => return None,
        }
    }
    Some(if neg { -total } else { total })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a1\r\nSUMMARY:Stand\r\n up\r\n\
                   DTSTART:20261015T010000Z\r\nDURATION:PT1H\r\nDESCRIPTION:line1\\nline2\\, ok\r\n\
                   ORGANIZER;CN=\"Alice\":mailto:a@x\r\nSTATUS:CONFIRMED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let ev = parse(ics);
        assert_eq!(ev.len(), 1);
        let e = &ev[0];
        assert_eq!(e.summary.as_deref(), Some("Standup"));
        assert_eq!(e.end - e.start, 3600);
        assert_eq!(e.description, "line1\nline2, ok");
        assert_eq!(e.organizer.as_deref(), Some("Alice"));
        assert_eq!(e.status.as_deref(), Some("confirmed"));
        assert!(!e.all_day);
    }

    #[test]
    fn all_day_and_recurrence() {
        let ics = "BEGIN:VEVENT\nUID:b\nDTSTART;VALUE=DATE:20261020\nRECURRENCE-ID:20261020\nEND:VEVENT\n";
        let e = &parse(ics)[0];
        assert!(e.all_day);
        assert_eq!(e.end - e.start, 86400);
        assert_eq!(e.ext_id(), "b@20261020");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_secs("PT1H30M"), Some(5400));
        assert_eq!(duration_secs("P1DT2H"), Some(93600));
        assert_eq!(duration_secs("P2W"), Some(14 * 86400));
        assert_eq!(duration_secs("-PT15M"), Some(-900));
        assert_eq!(duration_secs("nope"), None);
    }

    #[test]
    fn skips_broken() {
        assert!(parse("BEGIN:VEVENT\nSUMMARY:no uid\nEND:VEVENT\n").is_empty());
        assert!(parse("BEGIN:VEVENT\nUID:x\nDTSTART:garbage\nEND:VEVENT\n").is_empty());
    }
}
