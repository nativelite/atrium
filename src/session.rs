//! Session snapshot: capture, persist, and restore a window's grid layout and
//! pane roster so an atrium session can be resumed after a restart or crash.
//!
//! Three clean seams:
//!
//! * [`capture`] — pure: takes in-memory state, builds a [`Snapshot`]. No I/O,
//!   fully testable without a pty, window, or live session.
//! * [`save`] — atomic: serialises to a sibling `.tmp` file then renames over
//!   the target. A crash mid-write leaves the previous snapshot intact —
//!   matching the pattern in `reap::write_registry`.
//! * [`load`] — tolerant: a missing or corrupt file returns a clear `Err`,
//!   never panics.
//!
//! The on-disk format is a single JSON line (no pretty-printing), matching the
//! JSONL idiom in `audit.rs`. Versioned from the start so a future daemon can
//! detect and migrate old snapshots without a hard failure. Only identity
//! **names** are stored — never resolved secrets (name-only contract from
//! `identity.rs`).

use std::path::Path;

use json::{Number, Value};

/// On-disk schema version. Increment on any backward-incompatible change.
pub const VERSION: u32 = 1;

/// One pane's recoverable state. `Option` fields mirror what the live `Pane`
/// holds — `None` when the pane was opened without that attribute.
///
/// `argv` is the command vector the pane was spawned with — the minimum
/// needed for `atrium recover` to re-launch each agent. `worktree` is the git
/// worktree directory the agent ran in, if the fleet declared one.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneRecord {
    pub id: usize,
    pub role: Option<String>,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub identity: Option<String>,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
}

/// The serialisable grid layout: pane ids in left-to-right tree order and
/// which pane has focus. On restore, `Tree::grid_from_ids(&layout.ids)`
/// rebuilds a balanced grid; exact split proportions are a v2 concern.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutRecord {
    pub ids: Vec<usize>,
    pub focus: usize,
}

/// A versioned snapshot of a window's grid and pane roster. Designed to also
/// serve as the future daemon's persistent state model (ROADMAP Phase 2).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub layout: LayoutRecord,
    pub panes: Vec<PaneRecord>,
}

/// Input for one pane in a [`capture`] call — a plain data struct so the
/// function is testable without a live `Pty`, `Window`, or terminal.
#[derive(Debug, Clone)]
pub struct PaneCapture {
    pub id: usize,
    pub role: Option<String>,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub identity: Option<String>,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
}

/// Build a [`Snapshot`] from explicit window state. **Pure** — no I/O, no
/// side effects. Fully testable without a live window.
///
/// * `ordered_ids` — pane ids in layout (left-to-right tree) order, as
///   returned by `Tree::ids()`.
/// * `focus` — the focused pane id, as returned by `Tree::focus()`.
/// * `panes` — one [`PaneCapture`] per live pane (any order).
pub fn capture(ordered_ids: Vec<usize>, focus: usize, panes: Vec<PaneCapture>) -> Snapshot {
    Snapshot {
        version: VERSION,
        layout: LayoutRecord {
            ids: ordered_ids,
            focus,
        },
        panes: panes
            .into_iter()
            .map(|p| PaneRecord {
                id: p.id,
                role: p.role,
                argv: p.argv,
                cwd: p.cwd,
                identity: p.identity,
                session_id: p.session_id,
                worktree: p.worktree,
            })
            .collect(),
    }
}

// ---- serialisation -------------------------------------------------------

fn opt_str(v: Option<&str>) -> Value {
    v.map(|s| Value::String(s.to_string()))
        .unwrap_or(Value::Null)
}

fn pane_to_json(p: &PaneRecord) -> Value {
    Value::Object(vec![
        ("id".to_string(), Value::Number(Number::Int(p.id as i64))),
        ("role".to_string(), opt_str(p.role.as_deref())),
        (
            "argv".to_string(),
            Value::Array(p.argv.iter().map(|s| Value::String(s.clone())).collect()),
        ),
        ("cwd".to_string(), opt_str(p.cwd.as_deref())),
        ("identity".to_string(), opt_str(p.identity.as_deref())),
        ("session_id".to_string(), opt_str(p.session_id.as_deref())),
        ("worktree".to_string(), opt_str(p.worktree.as_deref())),
    ])
}

fn snapshot_to_json(s: &Snapshot) -> Value {
    let ids: Vec<Value> = s
        .layout
        .ids
        .iter()
        .map(|&id| Value::Number(Number::Int(id as i64)))
        .collect();
    Value::Object(vec![
        (
            "version".to_string(),
            Value::Number(Number::Int(s.version as i64)),
        ),
        (
            "layout".to_string(),
            Value::Object(vec![
                ("ids".to_string(), Value::Array(ids)),
                (
                    "focus".to_string(),
                    Value::Number(Number::Int(s.layout.focus as i64)),
                ),
            ]),
        ),
        (
            "panes".to_string(),
            Value::Array(s.panes.iter().map(pane_to_json).collect()),
        ),
    ])
}

// ---- deserialisation helpers ---------------------------------------------

fn get<'a>(obj: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    // Last-wins, matching the fleet/context parsers.
    obj.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn obj_of(v: &Value) -> Option<&Vec<(String, Value)>> {
    match v {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

fn opt_str_from(v: &Value) -> Option<Option<String>> {
    match v {
        Value::String(s) => Some(Some(s.clone())),
        Value::Null => Some(None),
        _ => None,
    }
}

fn pane_from_json(v: &Value) -> Option<PaneRecord> {
    let o = obj_of(v)?;
    let id = get(o, "id").and_then(|v| match v {
        Value::Number(Number::Int(n)) => Some(*n as usize),
        _ => None,
    })?;
    let role = get(o, "role").and_then(opt_str_from)?;
    let argv = get(o, "argv").and_then(|v| match v {
        Value::Array(arr) => arr
            .iter()
            .map(|a| match a {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>(),
        _ => None,
    })?;
    let cwd = get(o, "cwd").and_then(opt_str_from)?;
    let identity = get(o, "identity").and_then(opt_str_from)?;
    let session_id = get(o, "session_id").and_then(opt_str_from)?;
    let worktree = get(o, "worktree").and_then(opt_str_from)?;
    Some(PaneRecord {
        id,
        role,
        argv,
        cwd,
        identity,
        session_id,
        worktree,
    })
}

fn snapshot_from_json(v: &Value) -> Option<Snapshot> {
    let o = obj_of(v)?;
    let version = get(o, "version").and_then(|v| match v {
        Value::Number(Number::Int(n)) => Some(*n as u32),
        _ => None,
    })?;
    let layout_o = get(o, "layout").and_then(obj_of)?;
    let ids = get(layout_o, "ids").and_then(|v| match v {
        Value::Array(arr) => arr
            .iter()
            .map(|a| match a {
                Value::Number(Number::Int(n)) => Some(*n as usize),
                _ => None,
            })
            .collect::<Option<Vec<_>>>(),
        _ => None,
    })?;
    let focus = get(layout_o, "focus").and_then(|v| match v {
        Value::Number(Number::Int(n)) => Some(*n as usize),
        _ => None,
    })?;
    let panes = get(o, "panes").and_then(|v| match v {
        Value::Array(arr) => arr.iter().map(pane_from_json).collect::<Option<Vec<_>>>(),
        _ => None,
    })?;
    Some(Snapshot {
        version,
        layout: LayoutRecord { ids, focus },
        panes,
    })
}

// ---- I/O -----------------------------------------------------------------

/// Write `snap` to `path` **atomically**: serialises to a sibling `.tmp` file
/// then renames over `path`. A crash mid-write leaves the previous snapshot
/// intact — matching the atomic-write pattern in `reap::write_registry`.
pub fn save(path: &Path, snap: &Snapshot) -> std::io::Result<()> {
    let body = format!("{}\n", snapshot_to_json(snap));
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

/// Load a snapshot from `path`. Returns a clear `Err` for a missing,
/// unreadable, non-UTF-8, malformed-JSON, or structurally-invalid file.
/// Never panics.
pub fn load(path: &Path) -> Result<Snapshot, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read snapshot {}: {e}", path.display()))?;
    let v = json::parse(text.trim())
        .map_err(|e| format!("snapshot {}: JSON parse error: {e:?}", path.display()))?;
    snapshot_from_json(&v).ok_or_else(|| {
        format!(
            "snapshot {}: unexpected schema (corrupt or version mismatch)",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_panes() -> (Vec<usize>, usize, Vec<PaneCapture>) {
        let ids = vec![0, 1, 2, 3];
        let focus = 1;
        let panes = vec![
            PaneCapture {
                id: 0,
                role: Some("lead".to_string()),
                argv: vec![
                    "claude".to_string(),
                    "--model".to_string(),
                    "opus".to_string(),
                ],
                cwd: Some("/proj".to_string()),
                identity: Some("work".to_string()),
                session_id: Some("sess-abc".to_string()),
                worktree: Some("/proj/.atrium-worktrees/lead".to_string()),
            },
            PaneCapture {
                id: 1,
                role: None,
                argv: vec!["bash".to_string()],
                cwd: None,
                identity: None,
                session_id: None,
                worktree: None,
            },
            PaneCapture {
                id: 2,
                role: Some("dev_1".to_string()),
                argv: vec!["claude".to_string()],
                cwd: Some("/proj/feature".to_string()),
                identity: None,
                session_id: Some("sess-xyz".to_string()),
                worktree: None,
            },
            PaneCapture {
                id: 3,
                role: Some("dev_2".to_string()),
                argv: vec!["claude".to_string(), "--continue".to_string()],
                cwd: None,
                identity: Some("personal".to_string()),
                session_id: None,
                worktree: Some("/proj/.atrium-worktrees/dev_2".to_string()),
            },
        ];
        (ids, focus, panes)
    }

    #[test]
    fn capture_builds_correct_snapshot() {
        let (ids, focus, panes) = sample_panes();
        let snap = capture(ids.clone(), focus, panes);
        assert_eq!(snap.version, VERSION);
        assert_eq!(snap.layout.ids, ids);
        assert_eq!(snap.layout.focus, focus);
        assert_eq!(snap.panes.len(), 4);
        assert_eq!(snap.panes[0].role.as_deref(), Some("lead"));
        assert_eq!(snap.panes[1].role, None);
        assert_eq!(snap.panes[0].session_id.as_deref(), Some("sess-abc"));
        assert_eq!(snap.panes[1].identity, None);
        assert_eq!(
            snap.panes[3].worktree.as_deref(),
            Some("/proj/.atrium-worktrees/dev_2")
        );
    }

    #[test]
    fn round_trip_save_and_load() {
        let (ids, focus, panes) = sample_panes();
        let snap = capture(ids, focus, panes);
        let path =
            std::env::temp_dir().join(format!("atrium_session_rt_{}.json", std::process::id()));
        save(&path, &snap).expect("save must succeed");
        let loaded = load(&path).expect("load must succeed");
        assert_eq!(snap, loaded);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_returns_err_not_panic() {
        let path = std::env::temp_dir().join("atrium_session_no_such_file_XXXXX_test.json");
        let result = load(&path);
        assert!(result.is_err(), "missing file must return Err");
        assert!(
            !result.unwrap_err().is_empty(),
            "error message must not be empty"
        );
    }

    #[test]
    fn load_corrupt_json_returns_err_not_panic() {
        let path = std::env::temp_dir().join(format!(
            "atrium_session_corrupt_{}.json",
            std::process::id()
        ));
        std::fs::write(&path, b"not valid json {{{").expect("test setup write");
        let result = load(&path);
        assert!(result.is_err(), "corrupt JSON must return Err");
        assert!(!result.unwrap_err().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_wrong_schema_returns_err_not_panic() {
        let path = std::env::temp_dir().join(format!(
            "atrium_session_badschema_{}.json",
            std::process::id()
        ));
        std::fs::write(&path, b"{\"version\":1,\"oops\":true}").expect("test setup write");
        let result = load(&path);
        assert!(result.is_err(), "wrong schema must return Err");
        let _ = std::fs::remove_file(&path);
    }
}
