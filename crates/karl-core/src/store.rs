//! The SQLite cache. Sources write into it in the background; commands only
//! read it, so a tool call never waits on a network.
//!
//! `seq` is a global counter bumped on every insert and every content change.
//! A profile's cursor is the last `seq` it saw; "new" is `seq > cursor`.

use crate::model::{Item, Kind, News};
use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS items (
  id INTEGER PRIMARY KEY,
  seq INTEGER NOT NULL,
  created_seq INTEGER NOT NULL DEFAULT 0,
  source TEXT NOT NULL,
  kind TEXT NOT NULL,
  ext_id TEXT NOT NULL,
  thread TEXT,
  thread_name TEXT,
  who TEXT NOT NULL,
  who_addr TEXT,
  title TEXT,
  body TEXT NOT NULL DEFAULT '',
  ts INTEGER NOT NULL,
  start_ts INTEGER,
  end_ts INTEGER,
  all_day INTEGER NOT NULL DEFAULT 0,
  status TEXT,
  prio TEXT,
  done INTEGER NOT NULL DEFAULT 0,
  key TEXT,
  labels TEXT NOT NULL DEFAULT '',
  url TEXT,
  hash TEXT NOT NULL,
  synced_at INTEGER NOT NULL,
  UNIQUE(source, ext_id)
);
CREATE INDEX IF NOT EXISTS items_seq ON items(seq);
CREATE INDEX IF NOT EXISTS items_kind_ts ON items(kind, ts);
CREATE INDEX IF NOT EXISTS items_start ON items(start_ts);
CREATE INDEX IF NOT EXISTS items_key ON items(key);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS cursors (
  profile TEXT NOT NULL, key TEXT NOT NULL, seq INTEGER NOT NULL,
  PRIMARY KEY (profile, key)
);
CREATE TABLE IF NOT EXISTS writes (
  id INTEGER PRIMARY KEY,
  profile TEXT NOT NULL,
  source TEXT NOT NULL,
  args TEXT NOT NULL,
  created INTEGER NOT NULL,
  status TEXT NOT NULL,
  result TEXT,
  decided INTEGER
);
CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
  title, body, who, thread_name, content='items', content_rowid='id', tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS items_ai AFTER INSERT ON items BEGIN
  INSERT INTO items_fts(rowid, title, body, who, thread_name)
  VALUES (new.id, new.title, new.body, new.who, new.thread_name);
END;
CREATE TRIGGER IF NOT EXISTS items_ad AFTER DELETE ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, title, body, who, thread_name)
  VALUES ('delete', old.id, old.title, old.body, old.who, old.thread_name);
END;
CREATE TRIGGER IF NOT EXISTS items_au AFTER UPDATE ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, title, body, who, thread_name)
  VALUES ('delete', old.id, old.title, old.body, old.who, old.thread_name);
  INSERT INTO items_fts(rowid, title, body, who, thread_name)
  VALUES (new.id, new.title, new.body, new.who, new.thread_name);
END;
"#;

const COLS: &str =
    "seq, source, kind, ext_id, thread, thread_name, who, who_addr, title, body, ts, \
                    start_ts, end_ts, all_day, status, prio, done, key, labels, url, created_seq";

/// What an upsert did. Sync reports count these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Inserted,
    Updated,
    Unchanged,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UpsertCounts {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
}

/// A queued or executed write (`authorization-required` mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    pub id: i64,
    pub profile: String,
    pub source: String,
    pub args: Vec<String>,
    pub created: i64,
    /// pending | approved | denied | done | failed
    pub status: String,
    pub result: Option<String>,
    pub decided: Option<i64>,
}

const WCOLS: &str = "id, profile, source, args, created, status, result, decided";

fn row_to_write(r: &Row) -> rusqlite::Result<PendingWrite> {
    let args: String = r.get(3)?;
    Ok(PendingWrite {
        id: r.get(0)?,
        profile: r.get(1)?,
        source: r.get(2)?,
        args: serde_json::from_str(&args).unwrap_or_default(),
        created: r.get(4)?,
        status: r.get(5)?,
        result: r.get(6)?,
        decided: r.get(7)?,
    })
}

/// Older databases: add `created_seq`, backfilled from `seq`.
fn migrate(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(items)")?;
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    if !cols.iter().any(|c| c == "created_seq") {
        conn.execute_batch(
            "ALTER TABLE items ADD COLUMN created_seq INTEGER NOT NULL DEFAULT 0; UPDATE items SET created_seq = seq;",
        )?;
    }
    Ok(())
}

/// Filter shared by the read queries.
#[derive(Debug, Default, Clone)]
pub struct Filter<'a> {
    pub kinds: &'a [Kind],
    /// Source ids the caller may see. Empty = none (a profile with no sources sees nothing).
    pub sources: &'a [String],
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Self::init(conn)
    }

    pub fn open_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.execute_batch(SCHEMA).context("schema")?;
        migrate(&conn)?;
        Ok(Store { conn })
    }

    // ---- writes (sources) ----

    pub fn upsert(&mut self, item: &Item) -> Result<Upsert> {
        let tx = self.conn.transaction()?;
        let r = upsert_in(&tx, item, now())?;
        tx.commit()?;
        Ok(r)
    }

    pub fn upsert_many(&mut self, items: &[Item]) -> Result<UpsertCounts> {
        let tx = self.conn.transaction()?;
        let t = now();
        let mut c = UpsertCounts::default();
        for it in items {
            match upsert_in(&tx, it, t)? {
                Upsert::Inserted => c.inserted += 1,
                Upsert::Updated => c.updated += 1,
                Upsert::Unchanged => c.unchanged += 1,
            }
        }
        tx.commit()?;
        Ok(c)
    }

    /// Delete items of `source`/`kind` whose start lies in `[from, to)` and whose
    /// ext_id is not in `keep`. Calendar syncs use it: an event gone upstream goes here too.
    pub fn delete_missing(
        &mut self,
        source: &str,
        kind: Kind,
        from: i64,
        to: i64,
        keep: &[String],
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare(
            "SELECT id, ext_id FROM items WHERE source=?1 AND kind=?2 AND start_ts>=?3 AND start_ts<?4",
        )?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(params![source, kind.as_str(), from, to], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let mut n = 0;
        for (id, ext) in rows {
            if !keep.contains(&ext) {
                tx.execute("DELETE FROM items WHERE id=?1", params![id])?;
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Drop items older than `before` (unix s), except events that have not happened yet.
    pub fn purge_older_than(&mut self, before: i64) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM items WHERE ts < ?1 AND (start_ts IS NULL OR start_ts < ?1)",
            params![before],
        )?;
        Ok(n)
    }

    // ---- meta / sync state / cursors ----

    pub fn meta_get(&self, k: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT v FROM meta WHERE k=?1", params![k], |r| r.get(0))
            .optional()?)
    }

    pub fn meta_set(&self, k: &str, v: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![k, v],
        )?;
        Ok(())
    }

    pub fn cursor_get(&self, profile: &str, key: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT seq FROM cursors WHERE profile=?1 AND key=?2",
                params![profile, key],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn cursor_set(&self, profile: &str, key: &str, seq: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cursors(profile,key,seq) VALUES(?1,?2,?3) ON CONFLICT(profile,key) DO UPDATE SET seq=excluded.seq",
            params![profile, key, seq],
        )?;
        Ok(())
    }

    pub fn max_seq(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(seq),0) FROM items", [], |r| r.get(0))?)
    }

    // ---- reads (commands) ----

    /// News since a cursor, oldest first. `Changes`: `seq > since` (inserted or edited).
    /// `Arrivals`: `created_seq > since` (inserted only; later edits are not news).
    pub fn since_seq(&self, f: &Filter, since: i64, limit: usize, news: News) -> Result<Vec<Item>> {
        let (w, mut p) = where_clause(f);
        p.push(Box::new(since));
        p.push(Box::new(limit as i64));
        let col = match news {
            News::Changes => "seq",
            News::Arrivals => "created_seq",
        };
        let sql =
            format!("SELECT {COLS} FROM items WHERE {w} AND {col} > ? ORDER BY {col} ASC LIMIT ?");
        self.query(&sql, p)
    }

    // ---- write queue (authorization-required mode) ----

    pub fn write_queue(&self, profile: &str, source: &str, args: &[String]) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO writes(profile, source, args, created, status) VALUES(?1, ?2, ?3, ?4, 'pending')",
            params![profile, source, serde_json::to_string(args)?, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn write_get(&self, id: i64) -> Result<Option<PendingWrite>> {
        let sql = format!("SELECT {WCOLS} FROM writes WHERE id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![id], row_to_write)
            .optional()?)
    }

    /// Writes of one profile (or all when `None`), newest first; `status` filters when given.
    pub fn write_list(
        &self,
        profile: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PendingWrite>> {
        let mut sql = format!("SELECT {WCOLS} FROM writes WHERE 1=1");
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(pr) = profile {
            sql.push_str(" AND profile = ?");
            p.push(Box::new(pr.to_string()));
        }
        if let Some(st) = status {
            sql.push_str(" AND status = ?");
            p.push(Box::new(st.to_string()));
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        p.push(Box::new(limit as i64));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(p.iter().map(|b| b.as_ref())), row_to_write)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn write_set(&self, id: i64, status: &str, result: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE writes SET status = ?2, result = ?3, decided = ?4 WHERE id = ?1",
            params![id, status, result, now()],
        )?;
        Ok(())
    }

    /// Items with `ts >= since`, newest first.
    pub fn recent(&self, f: &Filter, since: i64, limit: usize) -> Result<Vec<Item>> {
        let (w, mut p) = where_clause(f);
        p.push(Box::new(since));
        p.push(Box::new(limit as i64));
        let sql =
            format!("SELECT {COLS} FROM items WHERE {w} AND ts >= ? ORDER BY ts DESC LIMIT ?");
        self.query(&sql, p)
    }

    /// Last `limit` items whose who / thread_name / key / who_addr contains `who` (case-insensitive), newest first.
    pub fn by_who(&self, f: &Filter, who: &str, limit: usize) -> Result<Vec<Item>> {
        let (w, mut p) = where_clause(f);
        let pat = format!("%{}%", who.to_lowercase());
        p.push(Box::new(pat.clone()));
        p.push(Box::new(pat.clone()));
        p.push(Box::new(pat.clone()));
        p.push(Box::new(pat));
        p.push(Box::new(limit as i64));
        let sql = format!(
            "SELECT {COLS} FROM items WHERE {w} AND (lower(who) LIKE ? OR lower(COALESCE(thread_name,'')) LIKE ? \
             OR lower(COALESCE(key,'')) LIKE ? OR lower(COALESCE(who_addr,'')) LIKE ?) ORDER BY ts DESC LIMIT ?"
        );
        self.query(&sql, p)
    }

    /// Full-text search, best match first.
    pub fn find(&self, f: &Filter, query: &str, limit: usize) -> Result<Vec<Item>> {
        let (w, mut p) = where_clause(f);
        p.push(Box::new(fts_query(query)));
        p.push(Box::new(limit as i64));
        let sql = format!(
            "SELECT {COLS} FROM items WHERE {w} AND id IN (SELECT rowid FROM items_fts WHERE items_fts MATCH ?) \
             ORDER BY ts DESC LIMIT ?"
        );
        self.query(&sql, p)
    }

    /// Events overlapping `[from, to)`, by start.
    pub fn events_between(&self, sources: &[String], from: i64, to: i64) -> Result<Vec<Item>> {
        let f = Filter {
            kinds: &[Kind::Event],
            sources,
        };
        let (w, mut p) = where_clause(&f);
        p.push(Box::new(to));
        p.push(Box::new(from));
        let sql = format!(
            "SELECT {COLS} FROM items WHERE {w} AND start_ts < ? AND COALESCE(end_ts, start_ts) > ? ORDER BY start_ts ASC"
        );
        self.query(&sql, p)
    }

    /// Open tasks assigned to `who_addr`, most recently updated first.
    pub fn tasks_assigned(
        &self,
        sources: &[String],
        who_addr: &str,
        limit: usize,
    ) -> Result<Vec<Item>> {
        let f = Filter {
            kinds: &[Kind::Task],
            sources,
        };
        let (w, mut p) = where_clause(&f);
        p.push(Box::new(who_addr.to_string()));
        p.push(Box::new(limit as i64));
        let sql = format!("SELECT {COLS} FROM items WHERE {w} AND who_addr = ? AND done = 0 ORDER BY ts DESC LIMIT ?");
        self.query(&sql, p)
    }

    pub fn get(&self, source: &str, ext_id: &str) -> Result<Option<Item>> {
        let sql = format!("SELECT {COLS} FROM items WHERE source=?1 AND ext_id=?2");
        Ok(self
            .conn
            .query_row(&sql, params![source, ext_id], row_to_item)
            .optional()?)
    }

    /// Item by task key (`HT-12`), among the given sources.
    pub fn get_key(&self, sources: &[String], key: &str) -> Result<Option<Item>> {
        let f = Filter {
            kinds: &[Kind::Task],
            sources,
        };
        let (w, mut p) = where_clause(&f);
        p.push(Box::new(key.to_uppercase()));
        let sql = format!("SELECT {COLS} FROM items WHERE {w} AND upper(key) = ? LIMIT 1");
        Ok(self.query(&sql, p)?.into_iter().next())
    }

    /// Per-source item counts.
    pub fn counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source, COUNT(*) FROM items GROUP BY source ORDER BY source")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    fn query(&self, sql: &str, p: Vec<Box<dyn rusqlite::ToSql>>) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params_from_iter(p.iter().map(|b| b.as_ref())), row_to_item)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn upsert_in(tx: &rusqlite::Transaction, it: &Item, t: i64) -> Result<Upsert> {
    let hash = it.content_hash();
    let existing: Option<(i64, String)> = tx
        .query_row(
            "SELECT id, hash FROM items WHERE source=?1 AND ext_id=?2",
            params![it.source, it.ext_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let labels = it.labels.join(",");
    match existing {
        Some((_, h)) if h == hash => {
            tx.execute(
                "UPDATE items SET synced_at=?3 WHERE source=?1 AND ext_id=?2",
                params![it.source, it.ext_id, t],
            )?;
            Ok(Upsert::Unchanged)
        }
        Some((id, _)) => {
            let seq = next_seq(tx)?;
            tx.execute(
                "UPDATE items SET seq=?1, thread=?2, thread_name=?3, who=?4, who_addr=?5, title=?6, body=?7, ts=?8, \
                 start_ts=?9, end_ts=?10, all_day=?11, status=?12, prio=?13, done=?14, key=?15, labels=?16, url=?17, \
                 hash=?18, synced_at=?19 WHERE id=?20",
                params![
                    seq, it.thread, it.thread_name, it.who, it.who_addr, it.title, it.body, it.ts, it.start, it.end,
                    it.all_day as i64, it.status, it.prio, it.done as i64, it.key, labels, it.url, hash, t, id
                ],
            )?;
            Ok(Upsert::Updated)
        }
        None => {
            let seq = next_seq(tx)?;
            tx.execute(
                "INSERT INTO items(seq, source, kind, ext_id, thread, thread_name, who, who_addr, title, body, ts, \
                 start_ts, end_ts, all_day, status, prio, done, key, labels, url, hash, synced_at, created_seq) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?1)",
                params![
                    seq, it.source, it.kind.as_str(), it.ext_id, it.thread, it.thread_name, it.who, it.who_addr,
                    it.title, it.body, it.ts, it.start, it.end, it.all_day as i64, it.status, it.prio,
                    it.done as i64, it.key, labels, it.url, hash, t
                ],
            )?;
            Ok(Upsert::Inserted)
        }
    }
}

fn next_seq(tx: &rusqlite::Transaction) -> Result<i64> {
    let cur: i64 = tx
        .query_row("SELECT v FROM meta WHERE k='seq'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let next = cur + 1;
    tx.execute(
        "INSERT INTO meta(k,v) VALUES('seq',?1) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        params![next.to_string()],
    )?;
    Ok(next)
}

fn where_clause(f: &Filter) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut parts = Vec::new();
    let mut p: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if f.sources.is_empty() {
        parts.push("0".to_string());
    } else {
        let qs = vec!["?"; f.sources.len()].join(",");
        parts.push(format!("source IN ({qs})"));
        for s in f.sources {
            p.push(Box::new(s.clone()));
        }
    }
    if !f.kinds.is_empty() {
        let qs = vec!["?"; f.kinds.len()].join(",");
        parts.push(format!("kind IN ({qs})"));
        for k in f.kinds {
            p.push(Box::new(k.as_str().to_string()));
        }
    }
    (parts.join(" AND "), p)
}

/// Turn free text into a safe FTS5 query: every word quoted, AND-ed, prefix match.
fn fts_query(q: &str) -> String {
    q.split_whitespace()
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn row_to_item(r: &Row) -> rusqlite::Result<Item> {
    let kind: String = r.get(2)?;
    let labels: String = r.get(18)?;
    Ok(Item {
        seq: r.get(0)?,
        source: r.get(1)?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Mail),
        ext_id: r.get(3)?,
        thread: r.get(4)?,
        thread_name: r.get(5)?,
        who: r.get(6)?,
        who_addr: r.get(7)?,
        title: r.get(8)?,
        body: r.get(9)?,
        ts: r.get(10)?,
        start: r.get(11)?,
        end: r.get(12)?,
        all_day: r.get::<_, i64>(13)? != 0,
        status: r.get(14)?,
        prio: r.get(15)?,
        done: r.get::<_, i64>(16)? != 0,
        key: r.get(17)?,
        labels: if labels.is_empty() {
            vec![]
        } else {
            labels.split(',').map(String::from).collect()
        },
        url: r.get(19)?,
        created_seq: r.get(20)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(id: &str, who: &str, ts: i64, body: &str) -> Item {
        let mut it = Item::new("gm", Kind::Mail, id, who, ts);
        it.title = Some(format!("subject {id}"));
        it.body = body.into();
        it
    }

    #[test]
    fn upsert_seq_and_new() {
        let mut s = Store::open_memory().unwrap();
        assert_eq!(
            s.upsert(&mail("1", "Alice", 10, "hi")).unwrap(),
            Upsert::Inserted
        );
        assert_eq!(
            s.upsert(&mail("1", "Alice", 10, "hi")).unwrap(),
            Upsert::Unchanged
        );
        assert_eq!(
            s.upsert(&mail("1", "Alice", 10, "hi!")).unwrap(),
            Upsert::Updated
        );
        assert_eq!(
            s.upsert(&mail("2", "Bob", 20, "yo")).unwrap(),
            Upsert::Inserted
        );
        assert_eq!(s.max_seq().unwrap(), 3);
        let src = vec!["gm".to_string()];
        let f = Filter {
            kinds: &[Kind::Mail],
            sources: &src,
        };
        let all = s.since_seq(&f, 0, 10, News::Changes).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            all[0].ext_id, "1",
            "updated item 1 has seq 2, item 2 has seq 3"
        );
        let newer = s.since_seq(&f, 2, 10, News::Changes).unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].who, "Bob");
        // arrivals: item 1 arrived at seq 1, its later edit (seq 2) is not news
        let arrived = s.since_seq(&f, 1, 10, News::Arrivals).unwrap();
        assert_eq!(arrived.len(), 1);
        assert_eq!(arrived[0].who, "Bob");
        assert_eq!(all[0].created_seq, 1);
        assert_eq!(all[0].seq, 2);
        // cursor
        assert_eq!(s.cursor_get("alex", "mail").unwrap(), 0);
        s.cursor_set("alex", "mail", 3).unwrap();
        assert_eq!(s.cursor_get("alex", "mail").unwrap(), 3);
        // other source invisible
        let none = s
            .since_seq(
                &Filter {
                    kinds: &[Kind::Mail],
                    sources: &[],
                },
                0,
                10,
                News::Changes,
            )
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn find_and_who() {
        let mut s = Store::open_memory().unwrap();
        s.upsert(&mail(
            "1",
            "Alice Smith",
            10,
            "the invoice for Q3 is attached",
        ))
        .unwrap();
        s.upsert(&mail("2", "Bob", 20, "lunch tomorrow?")).unwrap();
        let src = vec!["gm".to_string()];
        let f = Filter {
            kinds: &[Kind::Mail],
            sources: &src,
        };
        let r = s.find(&f, "invoice q3", 5).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].who, "Alice Smith");
        let r = s.find(&f, "inv", 5).unwrap();
        assert_eq!(r.len(), 1, "prefix match");
        let r = s.by_who(&f, "alice", 5).unwrap();
        assert_eq!(r.len(), 1);
        let r = s.find(&f, "\"quoted\" OR x", 5).unwrap();
        assert!(r.is_empty(), "operators are literal, not injected");
    }

    #[test]
    fn events_and_delete_missing() {
        let mut s = Store::open_memory().unwrap();
        let mut e = Item::new("gcal", Kind::Event, "e1", "me", 1);
        e.start = Some(100);
        e.end = Some(200);
        e.title = Some("Standup".into());
        let mut e2 = e.clone();
        e2.ext_id = "e2".into();
        e2.start = Some(300);
        e2.end = Some(400);
        s.upsert_many(&[e.clone(), e2]).unwrap();
        let src = vec!["gcal".to_string()];
        assert_eq!(s.events_between(&src, 0, 250).unwrap().len(), 1);
        assert_eq!(s.events_between(&src, 150, 350).unwrap().len(), 2);
        let n = s
            .delete_missing("gcal", Kind::Event, 0, 1000, &["e1".to_string()])
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.events_between(&src, 0, 1000).unwrap().len(), 1);
    }

    #[test]
    fn tasks_and_purge() {
        let mut s = Store::open_memory().unwrap();
        let mut t = Item::new("jira", Kind::Task, "HT-1", "Alex", 50);
        t.key = Some("HT-1".into());
        t.who_addr = Some("acc1".into());
        let mut d = t.clone();
        d.ext_id = "HT-2".into();
        d.key = Some("HT-2".into());
        d.done = true;
        s.upsert_many(&[t, d]).unwrap();
        let src = vec!["jira".to_string()];
        assert_eq!(s.tasks_assigned(&src, "acc1", 10).unwrap().len(), 1);
        assert_eq!(
            s.get_key(&src, "ht-2").unwrap().unwrap().key.as_deref(),
            Some("HT-2")
        );
        assert_eq!(s.purge_older_than(60).unwrap(), 2);
    }

    #[test]
    fn write_queue_roundtrip() {
        let s = Store::open_memory().unwrap();
        let id = s
            .write_queue(
                "alex",
                "jira",
                &["comment".into(), "HT-1".into(), "hi".into()],
            )
            .unwrap();
        let w = s.write_get(id).unwrap().unwrap();
        assert_eq!(w.status, "pending");
        assert_eq!(w.args, vec!["comment", "HT-1", "hi"]);
        assert_eq!(
            s.write_list(Some("alex"), Some("pending"), 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(s.write_list(Some("asami"), None, 10).unwrap().len(), 0);
        s.write_set(id, "done", Some("ok")).unwrap();
        let w = s.write_get(id).unwrap().unwrap();
        assert_eq!(w.status, "done");
        assert!(w.decided.is_some());
        assert!(s.write_list(None, Some("pending"), 10).unwrap().is_empty());
    }

    #[test]
    fn migration_adds_created_seq() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, seq INTEGER NOT NULL, source TEXT NOT NULL, kind TEXT NOT NULL, \
             ext_id TEXT NOT NULL, who TEXT NOT NULL, body TEXT NOT NULL DEFAULT '', ts INTEGER NOT NULL, \
             all_day INTEGER NOT NULL DEFAULT 0, done INTEGER NOT NULL DEFAULT 0, labels TEXT NOT NULL DEFAULT '', \
             hash TEXT NOT NULL, synced_at INTEGER NOT NULL, UNIQUE(source, ext_id)); \
             INSERT INTO items(seq, source, kind, ext_id, who, ts, hash, synced_at) VALUES(7,'gm','mail','x','a',1,'h',1);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let v: i64 = conn
            .query_row("SELECT created_seq FROM items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 7);
    }
}
