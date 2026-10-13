//! The one record type every source produces and every command reads.
//!
//! A chat message, a mail, a calendar event and a Jira issue are all an [`Item`]:
//! same table, same cursor, same renderer. What differs is the [`Kind`] and which
//! optional fields are filled.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What an item is. Drives rendering and which command group lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A chat message (LINE, WhatsApp, Matrix…).
    Msg,
    /// A mail.
    Mail,
    /// A calendar event.
    Event,
    /// A task or issue (Jira).
    Task,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Msg => "msg",
            Kind::Mail => "mail",
            Kind::Event => "event",
            Kind::Task => "task",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "msg" => Some(Kind::Msg),
            "mail" => Some(Kind::Mail),
            "event" => Some(Kind::Event),
            "task" => Some(Kind::Task),
            _ => None,
        }
    }
}

/// What "news" means for a source. Decided by the source, overridable in the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum News {
    /// Anything inserted *or changed* since the cursor: a task whose status moved is news. Jira, calendar.
    Changes,
    /// Only items that *arrived* since the cursor; later edits are not news. Mail, chat.
    Arrivals,
}

/// One cached record. `seq` is assigned by the store and grows on every
/// insert or content change, so "new since" is a single integer comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// Store-assigned, monotonic, bumped on every content change. 0 before insertion.
    #[serde(default)]
    pub seq: i64,
    /// The `seq` the item was first inserted with. Never changes. `Arrivals` compares this.
    #[serde(default)]
    pub created_seq: i64,
    /// Source id from the policy file: `gm`, `jira`, `gcal`…
    pub source: String,
    pub kind: Kind,
    /// Provider id, unique within a source (Gmail message id, Jira key, event id).
    pub ext_id: String,
    /// Conversation key: chat id, mail thread id, Jira project. Groups lines.
    #[serde(default)]
    pub thread: Option<String>,
    /// Human name of the thread: group chat name, mail subject of the thread.
    #[serde(default)]
    pub thread_name: Option<String>,
    /// Who it is from (mail, msg) or who owns it (task assignee). Display name.
    pub who: String,
    /// Machine id of `who`: address, account id. Used by hide rules and `mine`.
    #[serde(default)]
    pub who_addr: Option<String>,
    /// Subject, summary, event title.
    #[serde(default)]
    pub title: Option<String>,
    /// Plain text. Already stripped of HTML, quotes and signatures.
    #[serde(default)]
    pub body: String,
    /// When it happened or was last updated. Unix seconds.
    pub ts: i64,
    /// Events: start and end, unix seconds.
    #[serde(default)]
    pub start: Option<i64>,
    #[serde(default)]
    pub end: Option<i64>,
    #[serde(default)]
    pub all_day: bool,
    /// Tasks: status name. Events: "confirmed", "tentative"…
    #[serde(default)]
    pub status: Option<String>,
    /// Tasks: priority name.
    #[serde(default)]
    pub prio: Option<String>,
    /// Tasks: closed (status category done). `mine` hides these.
    #[serde(default)]
    pub done: bool,
    /// Tasks: issue key (`HT-12`). Shown instead of `who` as the handle.
    #[serde(default)]
    pub key: Option<String>,
    /// Labels, Gmail labels, event visibility, Jira labels. Hide rules match these.
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
}

impl Item {
    /// A minimal item; fill the rest with struct update syntax.
    pub fn new(source: &str, kind: Kind, ext_id: &str, who: &str, ts: i64) -> Item {
        Item {
            seq: 0,
            created_seq: 0,
            source: source.to_string(),
            kind,
            ext_id: ext_id.to_string(),
            thread: None,
            thread_name: None,
            who: who.to_string(),
            who_addr: None,
            title: None,
            body: String::new(),
            ts,
            start: None,
            end: None,
            all_day: false,
            status: None,
            prio: None,
            done: false,
            key: None,
            labels: Vec::new(),
            url: None,
        }
    }

    /// Hash of everything a user could notice changing. Same hash on re-sync = no new `seq`.
    pub fn content_hash(&self) -> String {
        let mut h = Sha256::new();
        for part in [
            self.who.as_str(),
            self.who_addr.as_deref().unwrap_or(""),
            self.title.as_deref().unwrap_or(""),
            self.body.as_str(),
            self.status.as_deref().unwrap_or(""),
            self.prio.as_deref().unwrap_or(""),
            self.thread_name.as_deref().unwrap_or(""),
        ] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        h.update(self.ts.to_le_bytes());
        h.update(self.start.unwrap_or(0).to_le_bytes());
        h.update(self.end.unwrap_or(0).to_le_bytes());
        h.update([self.all_day as u8, self.done as u8]);
        h.update(self.labels.join(",").as_bytes());
        let out = h.finalize();
        out.iter().take(16).map(|b| format!("{b:02x}")).collect()
    }

    /// The handle a human or a model uses to refer back to it: issue key, else `who`.
    pub fn handle(&self) -> &str {
        self.key.as_deref().unwrap_or(&self.who)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_changes_with_content_only() {
        let a = Item::new("gm", Kind::Mail, "1", "Alice", 10);
        let mut b = a.clone();
        b.seq = 99;
        b.url = Some("x".into());
        assert_eq!(
            a.content_hash(),
            b.content_hash(),
            "seq and url are not content"
        );
        b.body = "hello".into();
        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn kind_roundtrip() {
        for k in [Kind::Msg, Kind::Mail, Kind::Event, Kind::Task] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(Kind::parse("nope"), None);
    }
}
