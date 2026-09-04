//! The ctl **audit log**: an in-memory record of every control request the run
//! loop applies — who issued it, what it was, and how it resolved — so a ctl
//! session is reconstructable (design §5). Optionally mirrored to a JSONL file
//! (opt-in via `AMUX_CTL_AUDIT`), read live via `ctl audit`. Only identity
//! **names** ever appear here; resolved secret values never touch the log,
//! in memory or on disk.

use std::collections::VecDeque;
use std::io::Write;

use json::{Number, Value};

/// Default in-memory ring capacity: enough to reconstruct a busy session's
/// recent history without unbounded growth. Oldest entries evict first.
pub const DEFAULT_CAP: usize = 1024;

/// One recorded ctl request and its outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Monotonic sequence number (1-based) — the log's total order.
    pub seq: u64,
    /// The caller pane's agent id, or `None` for the operator (no attributed
    /// pane).
    pub caller: Option<usize>,
    /// The verb: `spawn` | `send` | `status` | `list` | `kill` | `bad-request`.
    pub action: String,
    /// A compact, secret-free description (target, role, argv stem, identity
    /// name, text length) — never text bodies or resolved secrets.
    pub detail: String,
    /// Whether the request succeeded.
    pub ok: bool,
    /// A short outcome note: the error on failure, a result tag on success.
    pub note: String,
}

impl Entry {
    /// Serialize to a JSON object — used for both the on-disk line and the
    /// `ctl audit` reply. Stable key order for readable logs.
    pub fn to_json(&self) -> Value {
        Value::Object(vec![
            (
                "seq".to_string(),
                Value::Number(Number::Int(self.seq as i64)),
            ),
            (
                "caller".to_string(),
                self.caller
                    .map(|c| Value::Number(Number::Int(c as i64)))
                    .unwrap_or(Value::Null),
            ),
            ("action".to_string(), Value::String(self.action.clone())),
            ("detail".to_string(), Value::String(self.detail.clone())),
            ("ok".to_string(), Value::Bool(self.ok)),
            ("note".to_string(), Value::String(self.note.clone())),
        ])
    }
}

/// The append-only audit log: a bounded in-memory ring, optionally mirrored to
/// a JSONL file. Built once per ctl-enabled run.
pub struct Audit {
    seq: u64,
    cap: usize,
    ring: VecDeque<Entry>,
    file: Option<std::fs::File>,
    /// Set if a disk open/write failed, surfaced once by [`Audit::take_error`];
    /// the in-memory log keeps working regardless.
    file_error: Option<String>,
}

impl Audit {
    /// A memory-only log with the default capacity.
    pub fn in_memory() -> Self {
        Self::new(DEFAULT_CAP, None)
    }

    /// Build a log with capacity `cap`, mirroring to `path` (append, created if
    /// absent) when given. A file that cannot be opened is reported via
    /// [`Audit::take_error`]; the log still runs in memory.
    pub fn new(cap: usize, path: Option<&str>) -> Self {
        let (file, file_error) = match path {
            Some(p) => match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
            {
                Ok(f) => (Some(f), None),
                Err(e) => (None, Some(format!("audit log {p:?}: {e}"))),
            },
            None => (None, None),
        };
        Audit {
            seq: 0,
            cap: cap.max(1),
            ring: VecDeque::new(),
            file,
            file_error,
        }
    }

    /// Record one request + outcome. Returns the assigned sequence number.
    pub fn record(
        &mut self,
        caller: Option<usize>,
        action: &str,
        detail: &str,
        ok: bool,
        note: &str,
    ) -> u64 {
        self.seq += 1;
        let entry = Entry {
            seq: self.seq,
            caller,
            action: action.to_string(),
            detail: detail.to_string(),
            ok,
            note: note.to_string(),
        };
        if let Some(f) = self.file.as_mut() {
            // Best-effort: a write failure is flagged once, never fatal.
            let line = format!("{}\n", entry.to_json());
            if let Err(e) = f.write_all(line.as_bytes()) {
                if self.file_error.is_none() {
                    self.file_error = Some(format!("audit write failed: {e}"));
                }
            }
        }
        self.ring.push_back(entry);
        while self.ring.len() > self.cap {
            self.ring.pop_front();
        }
        self.seq
    }

    /// The in-memory entries, oldest-first.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.ring.iter()
    }

    /// The most recent `tail` entries (or all when `None`), oldest-first, kept
    /// only where `keep` returns true (the subtree-scope predicate), serialized
    /// to JSON `Value`s ready for [`crate::ctl::reply_audit`]. The `tail` cut is
    /// applied *after* scoping, so a worker sees its own last N, not the last N
    /// of the whole log.
    pub fn view(&self, tail: Option<usize>, keep: impl Fn(&Entry) -> bool) -> Vec<Value> {
        let filtered: Vec<&Entry> = self.ring.iter().filter(|e| keep(e)).collect();
        let start = match tail {
            Some(n) => filtered.len().saturating_sub(n),
            None => 0,
        };
        filtered[start..].iter().map(|e| e.to_json()).collect()
    }

    /// The lowest sequence number still in the ring, if any.
    ///
    /// The ring is bounded and evicts SILENTLY, so a reader had no way to tell a
    /// quiet period from a rolled log. That matters: `list` is cheap and
    /// recorded, so roughly `DEFAULT_CAP` calls push everything older out — an
    /// attacker could exfiltrate the log and then erase what came before by
    /// making noise. Reporting the oldest surviving seq lets a consumer notice
    /// the gap (`oldest > 1` means entries are already gone).
    pub fn oldest_seq(&self) -> Option<u64> {
        self.ring.front().map(|e| e.seq)
    }

    /// The highest sequence number ever assigned, including entries since evicted.
    pub fn latest_seq(&self) -> u64 {
        self.seq
    }

    /// Take and clear any pending file error, to surface it once in the bar.
    pub fn take_error(&mut self) -> Option<String> {
        self.file_error.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(a: &mut Audit, caller: Option<usize>, action: &str) -> u64 {
        a.record(caller, action, "detail", true, "")
    }

    #[test]
    fn record_assigns_monotonic_seqs() {
        let mut a = Audit::in_memory();
        assert_eq!(rec(&mut a, None, "list"), 1);
        assert_eq!(rec(&mut a, Some(1), "spawn"), 2);
        assert_eq!(rec(&mut a, Some(1), "send"), 3);
        assert_eq!(a.entries().count(), 3);
    }

    #[test]
    fn ring_evicts_oldest_past_capacity() {
        let mut a = Audit::new(2, None);
        rec(&mut a, None, "one");
        rec(&mut a, None, "two");
        rec(&mut a, None, "three");
        let seqs: Vec<u64> = a.entries().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3]); // seq 1 evicted, order preserved
    }

    #[test]
    fn view_scopes_then_tails() {
        let mut a = Audit::new(64, None);
        // callers: operator(None), 1, 2, 1, 2
        a.record(None, "list", "", true, "");
        a.record(Some(1), "spawn", "", true, "");
        a.record(Some(2), "spawn", "", true, "");
        a.record(Some(1), "send", "", true, "");
        a.record(Some(2), "kill", "", true, "");
        // Keep only caller 1's entries, last 1 of them.
        let v = a.view(Some(1), |e| e.caller == Some(1));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].get("action").and_then(Value::as_str), Some("send"));
        // No tail: all of caller 1's entries.
        assert_eq!(a.view(None, |e| e.caller == Some(1)).len(), 2);
        // Operator predicate keeps everything.
        assert_eq!(a.view(None, |_| true).len(), 5);
    }

    #[test]
    fn entry_json_has_expected_shape() {
        let e = Entry {
            seq: 7,
            caller: Some(3),
            action: "kill".to_string(),
            detail: "target=2".to_string(),
            ok: true,
            note: "killed=2,4".to_string(),
        };
        let j = e.to_json();
        assert_eq!(j.get("seq").and_then(Value::as_i64), Some(7));
        assert_eq!(j.get("caller").and_then(Value::as_i64), Some(3));
        assert_eq!(j.get("action").and_then(Value::as_str), Some("kill"));
        assert_eq!(j.get("ok").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn operator_entry_serializes_caller_null() {
        let e = Entry {
            seq: 1,
            caller: None,
            action: "list".to_string(),
            detail: String::new(),
            ok: true,
            note: String::new(),
        };
        assert_eq!(e.to_json().get("caller"), Some(&Value::Null));
    }

    /// Eviction must be VISIBLE (review #7).
    ///
    /// The ring is bounded and drops its tail silently, and `list` is both cheap
    /// and recorded — so roughly `cap` calls push everything older out. Combined
    /// with an unauthenticated read that was the erase half of "exfiltrate, then
    /// cover your tracks". A consumer could not tell a quiet log from a rolled
    /// one; now `oldest_seq() > 1` says so plainly.
    #[test]
    fn a_rolled_ring_reports_that_entries_are_gone() {
        let mut a = Audit::new(4, None);
        assert_eq!(a.oldest_seq(), None, "an empty log has no oldest entry");
        for i in 0..4 {
            a.record(Some(i), "list", "", true, "");
        }
        assert_eq!(a.oldest_seq(), Some(1), "nothing evicted yet");
        assert_eq!(a.latest_seq(), 4);

        // Two more push the first two out.
        a.record(Some(9), "list", "", true, "");
        a.record(Some(9), "list", "", true, "");
        assert_eq!(
            a.oldest_seq(),
            Some(3),
            "seq 1 and 2 were evicted and the log must admit it"
        );
        assert_eq!(a.latest_seq(), 6, "latest counts everything ever assigned");
    }
}
