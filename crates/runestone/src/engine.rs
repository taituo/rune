//! The record: every event snapshots the world and is attributed by construction from its position in the
//! lifecycle. No model decides authorship.
//!
//! | segment ends with | previous event      | author          |
//! |-------------------|---------------------|-----------------|
//! | tool.post         | tool.pre (same call)| agent, that call|
//! | turn.start/spawn  | turn.end, spawn, -  | outside         |
//! | world.rewind      | any                 | rewind          |
//! | anything else     | any                 | unknown         |

use crate::store::{Event, Store};
use crate::world::{Change, World, EMPTY_TREE};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub fn rune_home() -> PathBuf {
    if let Ok(h) = std::env::var("RUNE_HOME") {
        return PathBuf::from(h);
    }
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(x).join("rune");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/rune")
}

pub struct Engine {
    pub store: Store,
    pub world: World,
    pub home: PathBuf,
}

impl Engine {
    pub fn open(worktree: &Path) -> Result<Self> {
        Self::open_in(&rune_home(), worktree)
    }

    pub fn open_in(home: &Path, worktree: &Path) -> Result<Self> {
        let world = World::open(home, worktree)?;
        let store = Store::open(&home.join("rune.db"))?;
        Ok(Engine { store, world, home: home.to_path_buf() })
    }

    /// Branch names are namespaced by worktree.
    pub fn branch(&self, name: &str) -> String {
        format!("{}/{}", self.world.key, name)
    }

    /// The most recently moved branch of this worktree.
    pub fn latest_branch(&self) -> Result<Option<String>> {
        Ok(self.store.heads(&format!("{}/", self.world.key))?.into_iter().next().map(|h| h.0))
    }

    /// Snapshot the world and append one event to `branch`.
    pub fn record(&self, branch: &str, kind: &str, extra: Value) -> Result<Event> {
        let tree = self.world.snapshot()?;
        self.record_tree(branch, kind, &tree, extra)
    }

    pub fn record_tree(&self, branch: &str, kind: &str, tree: &str, extra: Value) -> Result<Event> {
        self.store.append(branch, kind, |head| {
            let prev_tree = head.and_then(Event::tree).unwrap_or(EMPTY_TREE);
            let changed = self.world.diff(prev_tree, tree)?;
            let author = author_of(head, kind, &extra);
            let mut turn = head.and_then(Event::turn).unwrap_or(0);
            if kind == "turn.start" {
                turn += 1;
            }
            // A caller may pin the turn (a rewind continues from the target's turn).
            if let Some(t) = extra.get("turn").and_then(Value::as_u64) {
                turn = t;
            }
            let mut body = Map::new();
            if let Value::Object(m) = &extra {
                body.extend(m.clone());
            }
            if kind == "turn.start" && author == "outside" && !body.contains_key("note") {
                // Stored, not recomputed: what the model saw must not change when the template does.
                if let Some(n) = outside_note_text(&changed, turn) {
                    body.insert("note".into(), json!(n));
                }
            }
            body.insert("tree".into(), json!(tree));
            body.insert("turn".into(), json!(turn));
            body.insert("worktree".into(), json!(self.world.key));
            body.insert("author".into(), json!(author));
            body.insert("changed".into(), Value::Array(changed.iter().map(Change::to_json).collect()));
            Ok(Value::Object(body))
        })
    }

    pub fn lineage(&self, branch: &str) -> Result<Vec<Event>> {
        match self.store.head(branch)? {
            Some(id) => self.store.lineage(&id),
            None => Ok(vec![]),
        }
    }

    /// The note injected at turn start: outside changes since the agent's last turn. Paths and counts
    /// only, never content (hook output carries the harness's authority).
    pub fn outside_note(&self, ev: &Event) -> Option<String> {
        ev.str("note").map(str::to_string)
    }

    /// Per-path history on a branch, newest first: (event, change).
    pub fn blame(&self, branch: &str, path: &str) -> Result<Vec<(Event, Change)>> {
        let mut out = Vec::new();
        for ev in self.lineage(branch)?.into_iter().rev() {
            if let Some(c) = changes(&ev).into_iter().find(|c| c.path == path) {
                out.push((ev, c));
            }
        }
        Ok(out)
    }
}

fn outside_note_text(changes: &[Change], turn: u64) -> Option<String> {
    if changes.is_empty() {
        return None;
    }
    let last = turn.saturating_sub(1);
    let listed: Vec<String> = changes.iter().take(10).map(|c| format!("{} {} ({})", c.status, c.path, c.stat())).collect();
    let more = if changes.len() > 10 { format!("\n  …and {} more", changes.len() - 10) } else { String::new() };
    Some(format!(
        "[harness event, not from the user] Since your last turn (turn {last}), outside changes in this worktree:\n  {}{more}\nNot made by you. Read them before editing these files.",
        listed.join("   ")
    ))
}

pub fn changes(ev: &Event) -> Vec<Change> {
    ev.body
        .get("changed")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Change::from_json).collect())
        .unwrap_or_default()
}

fn author_of(prev: Option<&Event>, kind: &str, extra: &Value) -> String {
    let prev_kind = prev.map(|e| e.kind.as_str());
    match kind {
        "world.rewind" => "rewind".into(),
        "tool.post" => {
            let call = extra.get("call").and_then(Value::as_str);
            let pre_call = prev.and_then(|p| p.str("call"));
            if prev_kind == Some("tool.pre") && (call.is_none() || pre_call.is_none() || call == pre_call) {
                "agent".into()
            } else {
                "unknown".into()
            }
        }
        "turn.start" | "spawn" => match prev_kind {
            None | Some("turn.end") | Some("spawn") | Some("world.rewind") | Some("manual") => "outside".into(),
            _ => "unknown".into(), // a turn that never ended: a concurrent writer is possible
        },
        "manual" => "outside".into(),
        _ => "unknown".into(),
    }
}
