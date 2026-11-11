//! Event DAG with compare-and-swap branch heads.
//!
//! Event id = sha256 of the canonical JSON of (parents, kind, ts, body). serde_json's default map is a
//! BTreeMap, so keys are sorted and the encoding is canonical. Identical histories dedupe, any node can be
//! verified, and a branch is only a pointer moved by CAS. The same schema works in SQLite and Postgres.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: String,
    pub parents: Vec<String>,
    pub kind: String,
    pub ts: f64,
    pub body: Value,
}

impl Event {
    pub fn tree(&self) -> Option<&str> {
        self.body.get("tree").and_then(Value::as_str)
    }
    pub fn str(&self, key: &str) -> Option<&str> {
        self.body.get(key).and_then(Value::as_str)
    }
    pub fn turn(&self) -> Option<u64> {
        self.body.get("turn").and_then(Value::as_u64)
    }
    pub fn short(&self) -> &str {
        &self.id[..12.min(self.id.len())]
    }
}

pub fn event_id(parents: &[String], kind: &str, ts: f64, body: &Value) -> String {
    let canon = json!({ "parents": parents, "kind": kind, "ts": ts, "body": body });
    let bytes = serde_json::to_vec(&canon).expect("json encodes");
    hex::encode(Sha256::digest(&bytes))
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
  id      TEXT PRIMARY KEY,
  parents TEXT NOT NULL,          -- JSON array of event ids
  kind    TEXT NOT NULL,
  ts      REAL NOT NULL,
  body    TEXT NOT NULL           -- canonical JSON
);
CREATE TABLE IF NOT EXISTS heads (
  name    TEXT PRIMARY KEY,
  id      TEXT NOT NULL,
  updated REAL NOT NULL
);
-- Content-addressed bytes: rendered model requests, large tool results, system prompts.
CREATE TABLE IF NOT EXISTS blobs (
  hash  TEXT PRIMARY KEY,
  bytes BLOB NOT NULL
);
-- Results keyed by content: (tree, command) -> outcome. Branches converge on the same tree often.
CREATE TABLE IF NOT EXISTS results (
  tree    TEXT NOT NULL,
  cmd     TEXT NOT NULL,
  exit    INTEGER NOT NULL,
  output  TEXT NOT NULL,
  ts      REAL NOT NULL,
  PRIMARY KEY (tree, cmd)
);
";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn put(&self, parents: Vec<String>, kind: &str, body: Value) -> Result<Event> {
        let ts = now();
        let id = event_id(&parents, kind, ts, &body);
        self.conn.execute(
            "INSERT OR IGNORE INTO events (id, parents, kind, ts, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, serde_json::to_string(&parents)?, kind, ts, serde_json::to_string(&body)?],
        )?;
        Ok(Event { id, parents, kind: kind.to_string(), ts, body })
    }

    pub fn get(&self, id: &str) -> Result<Event> {
        self.find(id)?.with_context(|| format!("no event {id}"))
    }

    pub fn find(&self, id: &str) -> Result<Option<Event>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, parents, kind, ts, body FROM events WHERE id = ?1",
                params![id],
                row_to_event,
            )
            .optional()?)
    }

    /// Resolve a unique id prefix (like git's short hashes).
    pub fn resolve(&self, prefix: &str) -> Result<Event> {
        let mut stmt = self.conn.prepare("SELECT id FROM events WHERE id LIKE ?1 || '%' LIMIT 2")?;
        let ids: Vec<String> = stmt.query_map(params![prefix], |r| r.get(0))?.collect::<Result<_, _>>()?;
        match ids.len() {
            1 => self.get(&ids[0]),
            0 => bail!("no event matches {prefix}"),
            _ => bail!("event prefix {prefix} is ambiguous"),
        }
    }

    pub fn head(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM heads WHERE name = ?1", params![name], |r| r.get(0))
            .optional()?)
    }

    /// Move a branch head from `old` to `new` only if it still points at `old`. One statement, no lock.
    pub fn cas_head(&self, name: &str, old: Option<&str>, new: &str) -> Result<bool> {
        let n = match old {
            Some(old) => self.conn.execute(
                "UPDATE heads SET id = ?1, updated = ?2 WHERE name = ?3 AND id = ?4",
                params![new, now(), name, old],
            )?,
            None => self.conn.execute(
                "INSERT OR IGNORE INTO heads (name, id, updated) VALUES (?1, ?2, ?3)",
                params![name, new, now()],
            )?,
        };
        Ok(n == 1)
    }

    /// Append an event to a branch. `make` builds the body from the current head event, so the body can
    /// depend on it (turn numbers, diffs); on a lost CAS race it is rebuilt against the new head.
    pub fn append<F>(&self, branch: &str, kind: &str, mut make: F) -> Result<Event>
    where
        F: FnMut(Option<&Event>) -> Result<Value>,
    {
        for _ in 0..16 {
            let head_id = self.head(branch)?;
            let head = match &head_id {
                Some(id) => Some(self.get(id)?),
                None => None,
            };
            let body = make(head.as_ref())?;
            let parents = head_id.iter().cloned().collect();
            let ev = self.put(parents, kind, body)?;
            if self.cas_head(branch, head_id.as_deref(), &ev.id)? {
                return Ok(ev);
            }
        }
        bail!("could not append to {branch}: head kept moving")
    }

    /// Events from the root to `id`, following first parents.
    pub fn lineage(&self, id: &str) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        let mut cur = Some(id.to_string());
        while let Some(c) = cur {
            let ev = self.get(&c)?;
            cur = ev.parents.first().cloned();
            out.push(ev);
        }
        out.reverse();
        Ok(out)
    }

    pub fn heads(&self, prefix: &str) -> Result<Vec<(String, String, f64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, id, updated FROM heads WHERE name LIKE ?1 || '%' ORDER BY updated DESC")?;
        let rows = stmt.query_map(params![prefix], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let hash = hex::encode(Sha256::digest(bytes));
        self.conn
            .execute("INSERT OR IGNORE INTO blobs (hash, bytes) VALUES (?1, ?2)", params![hash, bytes])?;
        Ok(hash)
    }

    pub fn blob(&self, hash: &str) -> Result<Vec<u8>> {
        self.conn
            .query_row("SELECT bytes FROM blobs WHERE hash = ?1", params![hash], |r| r.get(0))
            .optional()?
            .with_context(|| format!("no blob {hash}"))
    }

    pub fn result(&self, tree: &str, cmd: &str) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT exit, output FROM results WHERE tree = ?1 AND cmd = ?2",
                params![tree, cmd],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn put_result(&self, tree: &str, cmd: &str, exit: i64, output: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO results (tree, cmd, exit, output, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![tree, cmd, exit, output, now()],
        )?;
        Ok(())
    }
}

fn row_to_event(r: &rusqlite::Row) -> rusqlite::Result<Event> {
    let parents: String = r.get(1)?;
    let body: String = r.get(4)?;
    Ok(Event {
        id: r.get(0)?,
        parents: serde_json::from_str(&parents).unwrap_or_default(),
        kind: r.get(2)?,
        ts: r.get(3)?,
        body: serde_json::from_str(&body).unwrap_or(Value::Null),
    })
}

/// Recompute an event's id from its content. A mismatch means the row was edited.
pub fn verify(ev: &Event) -> bool {
    event_id(&ev.parents, &ev.kind, ev.ts, &ev.body) == ev.id
}
