//! The command table: one definition drives argument parsing, the `karl` index
//! the model reads, and the dispatcher in the daemon. Nothing else knows the grammar.

use crate::policy::parse_duration;
use chrono::NaiveDate;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Read,
    Write,
}

/// A row of the table. `group` is the first word (`msg`, `mail`, `cal`, `jira`).
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub group: &'static str,
    pub name: &'static str,
    pub args: &'static str,
    pub verb: Verb,
}

/// Every command. Order = order in the index.
pub const SPECS: &[Spec] = &[
    Spec {
        group: "",
        name: "news",
        args: "",
        verb: Verb::Read,
    },
    Spec {
        group: "",
        name: "pending",
        args: "",
        verb: Verb::Read,
    },
    Spec {
        group: "msg",
        name: "news",
        args: "",
        verb: Verb::Read,
    },
    Spec {
        group: "msg",
        name: "recent",
        args: "<2h|3d>",
        verb: Verb::Read,
    },
    Spec {
        group: "msg",
        name: "show",
        args: "<who> [n]",
        verb: Verb::Read,
    },
    Spec {
        group: "msg",
        name: "find",
        args: "<words>",
        verb: Verb::Read,
    },
    Spec {
        group: "mail",
        name: "news",
        args: "",
        verb: Verb::Read,
    },
    Spec {
        group: "mail",
        name: "recent",
        args: "<2h|3d>",
        verb: Verb::Read,
    },
    Spec {
        group: "mail",
        name: "show",
        args: "<who> [n]",
        verb: Verb::Read,
    },
    Spec {
        group: "mail",
        name: "find",
        args: "<words>",
        verb: Verb::Read,
    },
    Spec {
        group: "cal",
        name: "today",
        args: "",
        verb: Verb::Read,
    },
    Spec {
        group: "cal",
        name: "tomorrow",
        args: "",
        verb: Verb::Read,
    },
    Spec {
        group: "cal",
        name: "week",
        args: "",
        verb: Verb::Read,
    },
    Spec {
        group: "cal",
        name: "<yyyy-mm-dd>",
        args: "[<yyyy-mm-dd>]",
        verb: Verb::Read,
    },
    Spec {
        group: "jira",
        name: "mine",
        args: "",
        verb: Verb::Read,
    },
    Spec {
        group: "jira",
        name: "news",
        args: "",
        verb: Verb::Read,
    },
    Spec {
        group: "jira",
        name: "show",
        args: "<KEY>",
        verb: Verb::Read,
    },
    Spec {
        group: "jira",
        name: "comments",
        args: "<KEY>",
        verb: Verb::Read,
    },
    Spec {
        group: "jira",
        name: "q",
        args: "<JQL>",
        verb: Verb::Read,
    },
    Spec {
        group: "jira",
        name: "comment",
        args: "<KEY> <text>",
        verb: Verb::Write,
    },
    Spec {
        group: "jira",
        name: "move",
        args: "<KEY> <status>",
        verb: Verb::Write,
    },
];

/// A parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Index,
    News,
    Pending,
    MsgNews,
    MsgRecent(Duration),
    MsgShow { who: String, n: usize },
    MsgFind(String),
    MailNews,
    MailRecent(Duration),
    MailShow { who: String, n: usize },
    MailFind(String),
    CalToday,
    CalTomorrow,
    CalWeek,
    CalRange { from: NaiveDate, to: NaiveDate },
    JiraMine,
    JiraNews,
    JiraShow(String),
    JiraComments(String),
    JiraQuery(String),
    JiraComment { key: String, text: String },
    JiraMove { key: String, status: String },
}

impl Command {
    pub fn group(&self) -> &'static str {
        use Command::*;
        match self {
            Index | News | Pending => "",
            MsgNews | MsgRecent(_) | MsgShow { .. } | MsgFind(_) => "msg",
            MailNews | MailRecent(_) | MailShow { .. } | MailFind(_) => "mail",
            CalToday | CalTomorrow | CalWeek | CalRange { .. } => "cal",
            JiraMine
            | JiraNews
            | JiraShow(_)
            | JiraComments(_)
            | JiraQuery(_)
            | JiraComment { .. }
            | JiraMove { .. } => "jira",
        }
    }

    pub fn verb(&self) -> Verb {
        match self {
            Command::JiraComment { .. } | Command::JiraMove { .. } => Verb::Write,
            _ => Verb::Read,
        }
    }

    /// Parse argv. Errors are the usage text the model should see.
    pub fn parse(args: &[String]) -> Result<Command, String> {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let dur =
            |s: &str| parse_duration(s).ok_or_else(|| format!("bad duration {s} (30m, 2h, 3d)"));
        let n_arg = |s: Option<&&str>| -> Result<usize, String> {
            match s {
                None => Ok(5),
                Some(v) => v
                    .parse::<usize>()
                    .map(|n| n.clamp(1, 50))
                    .map_err(|_| format!("bad count {v}")),
            }
        };
        let key = |s: &str| -> Result<String, String> {
            let k = s.to_uppercase();
            if k.len() > 32
                || !k.contains('-')
                || !k
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!("bad issue key {s}"));
            }
            Ok(k)
        };
        let rest = |from: usize| -> Result<String, String> {
            let s = a[from..].join(" ");
            if s.trim().is_empty() {
                Err("missing text".into())
            } else {
                Ok(s)
            }
        };
        Ok(match a.as_slice() {
            [] => Command::Index,
            ["news" | "new"] => Command::News,
            ["pending"] => Command::Pending,
            ["msg", "news" | "new"] => Command::MsgNews,
            ["msg", "recent", d] => Command::MsgRecent(dur(d)?),
            ["msg", "show", who, n @ ..] if n.len() <= 1 => Command::MsgShow {
                who: who.to_string(),
                n: n_arg(n.first())?,
            },
            ["msg", "find", _, ..] => Command::MsgFind(rest(2)?),
            ["mail", "news" | "new"] => Command::MailNews,
            ["mail", "recent", d] => Command::MailRecent(dur(d)?),
            ["mail", "show", who, n @ ..] if n.len() <= 1 => Command::MailShow {
                who: who.to_string(),
                n: n_arg(n.first())?,
            },
            ["mail", "find", _, ..] => Command::MailFind(rest(2)?),
            ["cal", "today"] => Command::CalToday,
            ["cal", "tomorrow"] => Command::CalTomorrow,
            ["cal", "week"] => Command::CalWeek,
            ["cal", d] => {
                let from = NaiveDate::parse_from_str(d, "%Y-%m-%d")
                    .map_err(|_| format!("bad date {d} (yyyy-mm-dd)"))?;
                Command::CalRange { from, to: from }
            }
            ["cal", d1, d2] => {
                let from = NaiveDate::parse_from_str(d1, "%Y-%m-%d")
                    .map_err(|_| format!("bad date {d1}"))?;
                let to = NaiveDate::parse_from_str(d2, "%Y-%m-%d")
                    .map_err(|_| format!("bad date {d2}"))?;
                if to < from {
                    return Err("end before start".into());
                }
                Command::CalRange { from, to }
            }
            ["jira", "mine"] => Command::JiraMine,
            ["jira", "news" | "new"] => Command::JiraNews,
            ["jira", "show", k] => Command::JiraShow(key(k)?),
            ["jira", "comments", k] => Command::JiraComments(key(k)?),
            ["jira", "q", _, ..] => Command::JiraQuery(rest(2)?),
            ["jira", "comment", k, _, ..] => Command::JiraComment {
                key: key(k)?,
                text: rest(3)?,
            },
            ["jira", "move", k, _, ..] => Command::JiraMove {
                key: key(k)?,
                status: rest(3)?,
            },
            [g, ..] if ["msg", "mail", "cal", "jira"].contains(g) => return Err(usage_for(g)),
            [other, ..] => {
                return Err(format!(
                    "unknown command {other}. `karl` alone lists commands"
                ))
            }
        })
    }
}

fn usage_for(group: &str) -> String {
    let mut s = format!("usage: karl {group} ");
    s.push_str(&group_line(group));
    s
}

fn group_line(group: &str) -> String {
    verbs_line(group, Verb::Read)
}

/// The write verbs of a group, shown only to profiles allowed to write there.
pub fn write_line(group: &str) -> String {
    verbs_line(group, Verb::Write)
}

fn verbs_line(group: &str, verb: Verb) -> String {
    SPECS
        .iter()
        .filter(|s| s.group == group && s.verb == verb)
        .map(|s| {
            if s.args.is_empty() {
                s.name.to_string()
            } else {
                format!("{} {}", s.name, s.args)
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The index the model reads. ~80 tokens. Only groups with a source are listed,
/// and a group's write verbs only when `writable` names it.
/// `sources` is the trailing status line (`gm 2m · jira 1m`), already rendered.
pub fn index(groups: &[&str], writable: &[&str], sources: &str) -> String {
    let mut out = String::from("karl news = everything new. `karl` alone = this index.\n");
    for g in ["msg", "mail", "cal", "jira"] {
        if groups.contains(&g) {
            out.push_str(&format!("  {g:<5}{}", group_line(g)));
            if writable.contains(&g) {
                let w = write_line(g);
                if !w.is_empty() {
                    out.push_str(&format!(" | {w}"));
                }
            }
            out.push('\n');
        }
    }
    if groups.is_empty() {
        out.push_str("  (no sources configured for this profile)\n");
    }
    out.push_str("sources: ");
    out.push_str(if sources.is_empty() { "none" } else { sources });
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command, String> {
        Command::parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses() {
        assert_eq!(p(""), Ok(Command::Index));
        assert_eq!(p("news"), Ok(Command::News));
        assert_eq!(p("new"), Ok(Command::News), "old spelling still accepted");
        assert_eq!(p("jira news"), Ok(Command::JiraNews));
        assert_eq!(p("pending"), Ok(Command::Pending));
        assert_eq!(
            p("jira move HT-1 In Progress"),
            Ok(Command::JiraMove {
                key: "HT-1".into(),
                status: "In Progress".into()
            })
        );
        assert_eq!(
            p("msg recent 2h"),
            Ok(Command::MsgRecent(Duration::from_secs(7200)))
        );
        assert_eq!(
            p("mail show alice"),
            Ok(Command::MailShow {
                who: "alice".into(),
                n: 5
            })
        );
        assert_eq!(
            p("mail show alice 3"),
            Ok(Command::MailShow {
                who: "alice".into(),
                n: 3
            })
        );
        assert_eq!(
            p("mail find invoice q3"),
            Ok(Command::MailFind("invoice q3".into()))
        );
        assert_eq!(p("jira show ht-12"), Ok(Command::JiraShow("HT-12".into())));
        assert_eq!(
            p("jira q project = HT"),
            Ok(Command::JiraQuery("project = HT".into()))
        );
        assert_eq!(
            p("cal 2026-10-13 2026-10-15"),
            Ok(Command::CalRange {
                from: NaiveDate::from_ymd_opt(2026, 10, 13).unwrap(),
                to: NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()
            })
        );
        assert_eq!(
            p("jira comment HT-1 hello there").unwrap().verb(),
            Verb::Write
        );
    }

    #[test]
    fn errors_are_usage() {
        assert!(p("msg").unwrap_err().starts_with("usage: karl msg"));
        assert!(p("mail recent soon").unwrap_err().contains("bad duration"));
        assert!(p("jira show ../x").unwrap_err().contains("bad issue key"));
        assert!(p("cal 2026-13-40").unwrap_err().contains("bad date"));
        assert!(p("rm -rf").unwrap_err().contains("unknown command"));
        assert!(p("mail show alice 3 4").is_err());
    }

    #[test]
    fn index_is_small() {
        let s = index(&["mail", "jira"], &[], "gm 2m · jira 1m");
        assert!(s.contains("mail news | recent"));
        assert!(!s.contains("msg"));
        assert!(
            !s.contains("comment <KEY>"),
            "writes are not advertised without authorization"
        );
        assert!(s.len() < 400, "{}: {s}", s.len());
        let w = index(&["jira"], &["jira"], "jira 1m");
        assert!(
            w.contains("| comment <KEY> <text> | move <KEY> <status>"),
            "{w}"
        );
    }
}
