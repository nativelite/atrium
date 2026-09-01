//! The shared **board**: durable source-of-truth state for a coordinating team.
//!
//! A schemaless `key → fields` tracker that lives in the amux daemon. Because
//! amux is the single broker process every pane's `ctl` talks to, the board is a
//! plain map behind the pipe — **single process, single writer, so no locking
//! and no consensus.** This is the "what is currently true" half of the
//! coordination layer (`auth: DONE, owner: Max, url: …`), the durable complement
//! to the ephemeral `send` message stream: the lead reads current state on demand
//! instead of re-scraping transcripts.
//!
//! Optionally mirrored to a snapshot file (set `AMUX_BOARD=<path>`) so a board
//! survives a restart. Pure logic + a best-effort atomic file write; unit-tested
//! without a terminal.
//!
//! Roadmap: this module (and the pub/sub bus to come) is slated to move into an
//! `abus` org crate once the bus lands and the coordination layer is clearly its
//! own concern — see `standards`/the roadmap. Kept here for the first increment.

use json::{Number, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Environment knob naming a snapshot file to persist the board to. Unset ⇒ the
/// board is in-memory only (lost on exit). Opt-in, on top of `--allow-ctl`.
pub const ENV_BOARD: &str = "AMUX_BOARD";

/// One board entry: freeform field→value pairs plus who/when last touched it.
/// `BTreeMap` so fields render in a stable (sorted) order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub fields: BTreeMap<String, String>,
    /// The role (or pane) that last wrote this entry — accountability, not auth.
    pub updated_by: Option<String>,
    pub updated_ms: u64,
}

/// The shared board — keyed entries with an optional snapshot file.
#[derive(Debug, Default)]
pub struct Board {
    entries: BTreeMap<String, Entry>,
    path: Option<PathBuf>,
}

impl Board {
    /// An in-memory board (no persistence).
    pub fn new() -> Self {
        Self::default()
    }

    /// A board backed by a snapshot file, loading any existing state. A missing,
    /// empty, or corrupt file simply starts empty — persistence is best-effort and
    /// never fatal.
    pub fn with_file(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| parse_snapshot(&s))
            .unwrap_or_default();
        Board {
            entries,
            path: Some(path),
        }
    }

    /// Merge `fields` into `key`'s entry (creating it if absent), stamping who and
    /// when. A field with an **empty** value is cleared; unmentioned fields are
    /// kept. Returns the updated entry. Persists if backed by a file.
    pub fn set(
        &mut self,
        key: &str,
        fields: &[(String, String)],
        by: Option<&str>,
        now_ms: u64,
    ) -> Entry {
        let e = self.entries.entry(key.to_string()).or_default();
        for (f, v) in fields {
            if v.is_empty() {
                e.fields.remove(f);
            } else {
                e.fields.insert(f.clone(), v.clone());
            }
        }
        e.updated_by = by.map(str::to_string);
        e.updated_ms = now_ms;
        let out = e.clone();
        self.persist();
        out
    }

    /// One entry, if present.
    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// Every `(key, entry)`, key-sorted.
    pub fn list(&self) -> Vec<(&String, &Entry)> {
        self.entries.iter().collect()
    }

    /// Remove an entry; `true` if it existed. Persists if backed by a file.
    pub fn del(&mut self, key: &str) -> bool {
        let had = self.entries.remove(key).is_some();
        if had {
            self.persist();
        }
        had
    }

    fn persist(&self) {
        if let Some(p) = &self.path {
            let _ = write_atomic(p, &snapshot(&self.entries));
        }
    }
}

/// Render an entry as a JSON value: `{"by":…, "ms":…, "fields":{…}}`.
pub fn entry_to_value(e: &Entry) -> Value {
    let fields: Vec<(String, Value)> = e
        .fields
        .iter()
        .map(|(f, v)| (f.clone(), Value::String(v.clone())))
        .collect();
    Value::Object(vec![
        (
            "by".to_string(),
            e.updated_by
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        ),
        ("ms".to_string(), Value::Number(Number::Int(e.updated_ms as i64))),
        ("fields".to_string(), Value::Object(fields)),
    ])
}

/// Render a `(key, entry)` roll-up as a JSON array of `{"key":…, "by":…, "ms":…,
/// "fields":{…}}` objects.
pub fn entries_to_value(entries: &[(&String, &Entry)]) -> Value {
    let arr = entries
        .iter()
        .map(|(k, e)| {
            let mut members = vec![("key".to_string(), Value::String((*k).clone()))];
            if let Value::Object(o) = entry_to_value(e) {
                members.extend(o);
            }
            Value::Object(members)
        })
        .collect();
    Value::Array(arr)
}

/// Serialize the whole board to a snapshot: `{key:{by,ms,fields}}`.
fn snapshot(entries: &BTreeMap<String, Entry>) -> String {
    let obj = entries
        .iter()
        .map(|(k, e)| (k.clone(), entry_to_value(e)))
        .collect();
    Value::Object(obj).to_string()
}

/// Parse a snapshot back into entries. `None` if the text is not a JSON object.
fn parse_snapshot(s: &str) -> Option<BTreeMap<String, Entry>> {
    let v = json::parse(s).ok()?;
    let obj = v.as_object()?;
    let mut out = BTreeMap::new();
    for (k, ev) in obj {
        let updated_by = ev.get("by").and_then(Value::as_str).map(str::to_string);
        let updated_ms = ev.get("ms").and_then(Value::as_i64).unwrap_or(0).max(0) as u64;
        let mut fields = BTreeMap::new();
        if let Some(fo) = ev.get("fields").and_then(Value::as_object) {
            for (f, fv) in fo {
                if let Some(val) = fv.as_str() {
                    fields.insert(f.clone(), val.to_string());
                }
            }
        }
        out.insert(
            k.clone(),
            Entry {
                fields,
                updated_by,
                updated_ms,
            },
        );
    }
    Some(out)
}

/// Write `contents` to `path` atomically (temp file + rename), so a reader never
/// sees a half-written board.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn set_creates_then_merges_fields() {
        let mut b = Board::new();
        b.set("auth", &f(&[("status", "WIP"), ("owner", "Max")]), Some("cto"), 100);
        // A second set merges: status updated, owner kept, url added.
        let e = b.set("auth", &f(&[("status", "DONE"), ("url", "http://x")]), Some("Max"), 200);
        assert_eq!(e.fields.get("status").map(String::as_str), Some("DONE"));
        assert_eq!(e.fields.get("owner").map(String::as_str), Some("Max"));
        assert_eq!(e.fields.get("url").map(String::as_str), Some("http://x"));
        assert_eq!(e.updated_by.as_deref(), Some("Max"));
        assert_eq!(e.updated_ms, 200);
    }

    #[test]
    fn empty_value_clears_a_field() {
        let mut b = Board::new();
        b.set("t", &f(&[("blocker", "waiting on api")]), None, 1);
        let e = b.set("t", &f(&[("blocker", "")]), None, 2);
        assert!(!e.fields.contains_key("blocker"), "empty value clears the field");
    }

    #[test]
    fn get_list_and_del() {
        let mut b = Board::new();
        b.set("a", &f(&[("status", "DONE")]), None, 1);
        b.set("b", &f(&[("status", "WIP")]), None, 1);
        assert!(b.get("a").is_some());
        assert_eq!(b.list().len(), 2);
        // list is key-sorted.
        assert_eq!(b.list()[0].0, "a");
        assert!(b.del("a"));
        assert!(!b.del("a"), "second delete is a no-op");
        assert!(b.get("a").is_none());
        assert_eq!(b.list().len(), 1);
    }

    #[test]
    fn snapshot_round_trips() {
        let mut b = Board::new();
        b.set("auth", &f(&[("status", "DONE"), ("owner", "Max")]), Some("cto"), 42);
        b.set("bill", &f(&[("status", "BLOCKED")]), Some("Vic"), 7);
        let snap = snapshot(
            &b.list()
                .into_iter()
                .map(|(k, e)| (k.clone(), e.clone()))
                .collect(),
        );
        let parsed = parse_snapshot(&snap).unwrap();
        assert_eq!(parsed.len(), 2);
        let auth = &parsed["auth"];
        assert_eq!(auth.fields.get("status").map(String::as_str), Some("DONE"));
        assert_eq!(auth.fields.get("owner").map(String::as_str), Some("Max"));
        assert_eq!(auth.updated_by.as_deref(), Some("cto"));
        assert_eq!(auth.updated_ms, 42);
    }

    #[test]
    fn entries_to_value_carries_key_and_fields() {
        let mut b = Board::new();
        b.set("auth", &f(&[("status", "DONE")]), Some("cto"), 1);
        let v = entries_to_value(&b.list());
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("key").and_then(Value::as_str), Some("auth"));
        assert_eq!(
            arr[0]
                .get("fields")
                .and_then(|f| f.get("status"))
                .and_then(Value::as_str),
            Some("DONE")
        );
    }
}
