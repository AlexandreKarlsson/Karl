//! Items → lines. This is the token budget of the whole system: one line per
//! item, source id first, names instead of ids, bodies cut, noisy threads folded.

use crate::model::{Item, Kind};
use crate::text::{one_line, truncate_bytes};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct Opts {
    /// Max chars of one rendered line.
    pub line_chars: usize,
    /// Threads with more new items than this are folded into one line.
    pub fold_after: usize,
    /// Now, unix seconds. Dates equal to today are not printed.
    pub now: i64,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            line_chars: 160,
            fold_after: 3,
            now: chrono::Utc::now().timestamp(),
        }
    }
}

fn local(ts: i64) -> DateTime<Local> {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).unwrap())
}

fn day_tag(ts: i64, now: i64) -> String {
    let d = local(ts);
    let today = local(now).date_naive();
    if d.date_naive() == today {
        String::new()
    } else {
        format!("{} ", d.format("%a %-d").to_string().to_lowercase())
    }
}

fn hm(ts: i64) -> String {
    local(ts).format("%H:%M").to_string()
}

/// One line for one item. `with_time` prefixes the day/time (for `recent` and `show`).
pub fn line(it: &Item, o: &Opts, with_time: bool) -> String {
    let mut head = format!("{} ", it.source);
    if with_time {
        head.push_str(&day_tag(it.ts, o.now));
        head.push_str(&hm(it.ts));
        head.push(' ');
    }
    match it.kind {
        Kind::Msg => {
            if let Some(t) = &it.thread_name {
                head.push_str(&format!("{t} "));
            }
            head.push_str(&format!("{}: ", it.who));
            let room = o.line_chars.saturating_sub(head.chars().count()).max(20);
            head + &one_line(&it.body, room)
        }
        Kind::Mail => {
            head.push_str(&format!("{}: ", it.who));
            if let Some(t) = &it.title {
                head.push_str(t.trim());
            }
            let room = o.line_chars.saturating_sub(head.chars().count());
            if room > 24 && !it.body.is_empty() {
                head.push_str(" — ");
                head + &one_line(&it.body, room - 3)
            } else {
                head
            }
        }
        Kind::Event => {
            head.push_str(&event_span(it, o.now));
            head.push(' ');
            head.push_str(it.title.as_deref().unwrap_or("(untitled)"));
            head
        }
        Kind::Task => {
            if let Some(k) = &it.key {
                head.push_str(&format!("{k} "));
            }
            if let Some(s) = &it.status {
                head.push_str(&format!("{s} "));
            }
            // Medium is Jira's default: not information, not worth tokens
            if let Some(p) = it.prio.as_deref().filter(|p| *p != "Medium") {
                head.push_str(&format!("{p} "));
            }
            head.push_str(&format!("{}: ", it.who));
            head.push_str(it.title.as_deref().unwrap_or(""));
            truncate_line(&head, o.line_chars)
        }
    }
}

fn truncate_line(s: &str, max: usize) -> String {
    crate::text::truncate_chars(s, max)
}

/// `10:00-11:00`, `all-day`, or `wed 15 10:00-11:00` when not today.
fn event_span(it: &Item, now: i64) -> String {
    let start = it.start.unwrap_or(it.ts);
    let tag = day_tag(start, now);
    if it.all_day {
        return format!("{tag}all-day");
    }
    match it.end {
        Some(e) if e > start => format!("{tag}{}-{}", hm(start), hm(e)),
        _ => format!("{tag}{}", hm(start)),
    }
}

/// Lines for a `new` or `recent` listing, in input order: chat threads with more
/// than `fold_after` items collapse into one line placed where their last item was.
pub fn listing(items: &[Item], o: &Opts, with_time: bool) -> Vec<String> {
    let thread_key = |it: &Item| {
        format!(
            "{}\u{1}{}",
            it.source,
            it.thread.clone().unwrap_or_else(|| it.who.clone())
        )
    };
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    let mut last_at: BTreeMap<String, usize> = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.kind == Kind::Msg {
            let k = thread_key(it);
            *count.entry(k.clone()).or_default() += 1;
            last_at.insert(k, i);
        }
    }
    let mut out = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if it.kind != Kind::Msg {
            out.push(line(it, o, with_time));
            continue;
        }
        let k = thread_key(it);
        let n = count[&k];
        if n <= o.fold_after {
            out.push(line(it, o, with_time));
        } else if last_at[&k] == i {
            let mut folded = it.clone();
            let name = it.thread_name.clone().unwrap_or_else(|| it.who.clone());
            folded.thread_name = Some(format!("{name}(+{})", n - 1));
            out.push(line(&folded, o, with_time));
        }
    }
    out
}

/// Agenda: one line per day, events joined with ` · `. Empty days skipped.
pub fn agenda(events: &[Item], from: NaiveDate, to: NaiveDate, o: &Opts) -> Vec<String> {
    let mut days: BTreeMap<NaiveDate, Vec<String>> = BTreeMap::new();
    for e in events {
        let start = e.start.unwrap_or(e.ts);
        let d = local(start).date_naive();
        let d = d.clamp(from, to);
        let span = if e.all_day {
            "all-day".to_string()
        } else {
            match e.end {
                Some(end) if end > start => format!("{}-{}", hm(start), hm(end)),
                _ => hm(start),
            }
        };
        let title = e.title.as_deref().unwrap_or("(untitled)");
        days.entry(d).or_default().push(format!("{span} {title}"));
    }
    let today = local(o.now).date_naive();
    days.into_iter()
        .map(|(d, evs)| {
            let tag = if d == today {
                "today".to_string()
            } else {
                let t = d.format("%a %-d").to_string().to_lowercase();
                if d.month() != today.month() {
                    format!("{t} {}", d.format("%b").to_string().to_lowercase())
                } else {
                    t
                }
            };
            truncate_line(&format!("{tag}: {}", evs.join(" · ")), o.line_chars * 3)
        })
        .collect()
}

/// Full view of one item: header line, then the body cut to `max_bytes`.
pub fn show(it: &Item, o: &Opts, max_bytes: usize) -> String {
    let mut head = line(it, o, true);
    if it.kind == Kind::Mail || it.kind == Kind::Msg {
        // the listing line already embeds a body snippet; rebuild a head without it
        head = format!(
            "{} {}{} {}",
            it.source,
            day_tag(it.ts, o.now),
            hm(it.ts),
            it.who
        );
        if let Some(t) = &it.title {
            head.push_str(&format!(": {}", t.trim()));
        }
    }
    let mut extra = Vec::new();
    if let Some(u) = &it.url {
        extra.push(u.clone());
    }
    if !it.labels.is_empty() && it.kind == Kind::Task {
        extra.push(it.labels.join(","));
    }
    let mut out = head;
    if !extra.is_empty() {
        out.push_str(&format!(" [{}]", extra.join(" ")));
    }
    out.push('\n');
    let budget = max_bytes.saturating_sub(out.len()).max(64);
    out.push_str(&truncate_bytes(it.body.trim(), budget));
    out
}

/// Apply the profile caps: at most `max_lines` lines and `max_bytes` bytes, with a `(+N more)` tail.
pub fn cap(lines: &[String], max_lines: usize, max_bytes: usize) -> String {
    let mut out = String::new();
    let mut shown = 0;
    for l in lines {
        if shown >= max_lines || out.len() + l.len() + 1 > max_bytes.saturating_sub(16) {
            break;
        }
        out.push_str(l);
        out.push('\n');
        shown += 1;
    }
    if shown < lines.len() {
        out.push_str(&format!("(+{} more)\n", lines.len() - shown));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Opts {
        Opts {
            line_chars: 80,
            fold_after: 2,
            now: 1_760_000_000,
        }
    }

    #[test]
    fn mail_line_cuts_body() {
        let mut m = Item::new("gm", Kind::Mail, "1", "Alice Smith", 1_760_000_000);
        m.title = Some("Re: invoice Q3".into());
        m.body = "can you check the attached file and tell me if the numbers are right before friday please".into();
        let l = line(&m, &opts(), false);
        assert!(
            l.starts_with("gm Alice Smith: Re: invoice Q3 — can you"),
            "{l}"
        );
        assert!(l.chars().count() <= 80 + 10, "{l}");
        assert!(l.contains("…(+"), "{l}");
    }

    #[test]
    fn msg_fold() {
        let mut v = Vec::new();
        for i in 0..5 {
            let mut m = Item::new("wa", Kind::Msg, &i.to_string(), "Mom", 1_760_000_000 + i);
            m.thread = Some("fam".into());
            m.thread_name = Some("Family".into());
            m.body = format!("msg {i}");
            v.push(m);
        }
        let mut solo = Item::new("line", Kind::Msg, "x", "Juliette", 1_760_000_000);
        solo.body = "Hey what are you doing tonight brother?".into();
        v.push(solo);
        let ls = listing(&v, &opts(), false);
        assert_eq!(ls.len(), 2, "{ls:?}");
        assert_eq!(ls[0], "wa Family(+4) Mom: msg 4");
        assert_eq!(
            ls[1],
            "line Juliette: Hey what are you doing tonight brother?"
        );
    }

    #[test]
    fn task_line_and_cap() {
        let mut t = Item::new("jira", Kind::Task, "HT-12", "Alice", 1);
        t.key = Some("HT-12".into());
        t.status = Some("In Progress".into());
        t.prio = Some("High".into());
        t.title = Some("Fix login".into());
        assert_eq!(
            line(&t, &opts(), false),
            "jira HT-12 In Progress High Alice: Fix login"
        );
        let lines: Vec<String> = (0..10).map(|i| format!("l{i}")).collect();
        let c = cap(&lines, 3, 1000);
        assert_eq!(c, "l0\nl1\nl2\n(+7 more)\n");
        let c = cap(&lines, 100, 24);
        assert!(c.ends_with("more)\n"), "{c}");
    }

    #[test]
    fn agenda_lines() {
        let o = opts();
        let today = local(o.now).date_naive();
        let mut e = Item::new("gcal", Kind::Event, "1", "me", o.now);
        e.start = Some(o.now + 600);
        e.end = Some(o.now + 4200);
        e.title = Some("Standup".into());
        let mut h = e.clone();
        h.ext_id = "2".into();
        h.all_day = true;
        h.start = Some(o.now + 86400);
        h.title = Some("Holiday".into());
        let ls = agenda(&[e, h], today, today + chrono::Duration::days(7), &o);
        assert_eq!(ls.len(), 2);
        assert!(ls[0].starts_with("today: "), "{}", ls[0]);
        assert!(ls[0].contains("Standup"));
        assert!(ls[1].contains("all-day Holiday"), "{}", ls[1]);
    }

    #[test]
    fn show_has_header_and_body() {
        let mut m = Item::new("gm", Kind::Mail, "1", "Alice", 1_760_000_000);
        m.title = Some("Hi".into());
        m.body = "x".repeat(500);
        let s = show(&m, &opts(), 200);
        assert!(s.starts_with("gm "), "{s}");
        assert!(s.contains("Alice: Hi\n"));
        assert!(s.len() <= 230, "{}", s.len());
        assert!(s.contains("(+"));
    }
}
