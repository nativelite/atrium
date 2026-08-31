//! Pre-accept Claude Code's **workspace folder-trust dialog** for a directory,
//! so a `--trust` launch is truly hands-off.
//!
//! `--dangerously-skip-permissions` skips per-action tool prompts, but claude's
//! *"Do you trust the files in this folder?"* dialog is a **separate** gate: it
//! is stored per-directory in `~/.claude.json` under
//! `projects["<dir>"].hasTrustDialogAccepted`, and no CLI flag / env var
//! bypasses it (verified — there is only the stored state). So amux, only when
//! the operator opts into `--trust`, writes that same bit for the pane's working
//! directory before spawning claude there: the programmatic equivalent of the
//! human clicking *"trust this folder."*
//!
//! This is the one place amux writes another tool's config; it is deliberate,
//! opt-in behind `--trust`, and surgical — it parses the whole file, flips (or
//! adds) exactly the trust keys for one directory, and writes it back atomically
//! (temp + rename), preserving everything else byte-for-byte (json round-trips
//! losslessly). A file it cannot parse is left **untouched** — amux never
//! clobbers a config it did not understand.

use std::path::{Path, PathBuf};

use json::Value;

/// The per-project key claude sets when the folder-trust dialog is accepted.
const KEY_TRUST: &str = "hasTrustDialogAccepted";
/// Set alongside it so a freshly-trusted project also skips onboarding.
const KEY_ONBOARDED: &str = "hasCompletedProjectOnboarding";

/// What [`ensure_trusted`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The directory was already trusted; the file was not touched.
    AlreadyTrusted,
    /// The directory is now trusted; the file was rewritten.
    NowTrusted,
}

/// Ensure `dir` is marked trusted in `~/.claude.json`, so claude launched there
/// won't show the folder-trust dialog. Idempotent: a no-op (no write) when the
/// directory is already trusted. Returns a clear error string on any IO/parse
/// problem — callers treat it as non-fatal (the spawn still proceeds; worst case
/// the dialog appears), never a reason to abort a pane.
pub fn ensure_trusted(dir: &Path) -> Result<Outcome, String> {
    let path = config_path().ok_or_else(|| "no HOME to locate ~/.claude.json".to_string())?;
    let key = project_key(dir);

    // Absent config: claude creates it on launch, but if we create a minimal one
    // first, the trust bit is already set when it reads. Seed just the projects
    // entry; claude fills in the rest on its own read-merge-write.
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::from("{}"),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };

    let mut root = json::parse(&src).map_err(|e| {
        // Never clobber a file we couldn't parse.
        format!("~/.claude.json is not valid JSON ({e}); left untouched")
    })?;
    if !matches!(root, Value::Object(_)) {
        return Err("~/.claude.json is not a JSON object; left untouched".to_string());
    }

    if !set_trusted_in(&mut root, &key) {
        return Ok(Outcome::AlreadyTrusted);
    }

    let out = root.pretty(2);
    // Guard: re-parse our own output; never write JSON we can't read back.
    json::parse(&out).map_err(|e| format!("internal: produced invalid JSON ({e}); not written"))?;
    write_atomic(&path, &out)?;
    Ok(Outcome::NowTrusted)
}

/// The claude config file: `$CLAUDE_CONFIG_DIR/.claude.json` when that env var
/// is set (claude honors it), else `~/.claude.json` (`$HOME` / `$USERPROFILE`).
fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join(".claude.json"));
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".claude.json"))
}

/// Convert a directory to claude's project-map key: an absolute path with
/// forward slashes and no trailing slash (e.g. `D:/projects/x`). claude (a Node
/// app) keys projects by `process.cwd()` in exactly this shape on every
/// platform, so the child's inherited cwd maps to the same key we write.
fn project_key(dir: &Path) -> String {
    let abs = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(dir))
            .unwrap_or_else(|_| dir.to_path_buf())
    };
    let mut s = abs.to_string_lossy().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// Set `projects[key].hasTrustDialogAccepted = true` (and `…Onboarding = true`)
/// on the parsed root, creating the `projects` map and the per-dir entry as
/// needed. Returns whether anything changed — `false` (no write needed) when the
/// directory was already trusted. Pure; the unit-test seam.
fn set_trusted_in(root: &mut Value, key: &str) -> bool {
    let root_obj = match root {
        Value::Object(m) => m,
        _ => return false,
    };
    let projects = obj_entry(root_obj, "projects");
    let proj_obj = match projects {
        Value::Object(m) => m,
        // `projects` exists but isn't an object — replace it with one.
        other => {
            *other = Value::Object(Vec::new());
            match other {
                Value::Object(m) => m,
                _ => unreachable!(),
            }
        }
    };
    let entry = obj_entry(proj_obj, key);
    let entry_obj = match entry {
        Value::Object(m) => m,
        other => {
            *other = Value::Object(Vec::new());
            match other {
                Value::Object(m) => m,
                _ => unreachable!(),
            }
        }
    };

    let already = obj_get(entry_obj, KEY_TRUST) == Some(&Value::Bool(true));
    if already {
        return false;
    }
    obj_set(entry_obj, KEY_TRUST, Value::Bool(true));
    obj_set(entry_obj, KEY_ONBOARDED, Value::Bool(true));
    true
}

// ---- small object helpers over json's order-preserving `Vec<(String,Value)>` --

/// A mutable reference to `members[key]`, inserting `{}` if absent.
fn obj_entry<'a>(members: &'a mut Vec<(String, Value)>, key: &str) -> &'a mut Value {
    if let Some(i) = members.iter().position(|(k, _)| k == key) {
        &mut members[i].1
    } else {
        members.push((key.to_string(), Value::Object(Vec::new())));
        &mut members.last_mut().unwrap().1
    }
}

fn obj_get<'a>(members: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Replace `members[key]` if present, else append it.
fn obj_set(members: &mut Vec<(String, Value)>, key: &str, val: Value) {
    if let Some(i) = members.iter().position(|(k, _)| k == key) {
        members[i].1 = val;
    } else {
        members.push((key.to_string(), val));
    }
}

/// Write `contents` to `path` atomically: a sibling temp file, then rename over
/// the target (atomic replace on Windows and Unix), so a reader never sees a
/// half-written config even if we're interrupted.
fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    let tmp = PathBuf::from(format!("{}.amux-tmp", path.display()));
    std::fs::write(&tmp, contents).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // don't leave a turd behind
            Err(format!("replace {}: {e}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trusted(root: &Value, key: &str) -> Option<bool> {
        root.get("projects")?.get(key)?.get(KEY_TRUST).and_then(Value::as_bool)
    }

    #[test]
    fn sets_trust_on_a_fresh_config() {
        let mut root = json::parse("{}").unwrap();
        assert!(set_trusted_in(&mut root, "D:/x"));
        assert_eq!(trusted(&root, "D:/x"), Some(true));
        // onboarding set too
        assert_eq!(
            root.get("projects").unwrap().get("D:/x").unwrap().get(KEY_ONBOARDED),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn flips_an_existing_false_entry() {
        let src = r#"{"projects":{"D:/x":{"hasTrustDialogAccepted":false,"lastCost":0.5}}}"#;
        let mut root = json::parse(src).unwrap();
        assert!(set_trusted_in(&mut root, "D:/x"));
        assert_eq!(trusted(&root, "D:/x"), Some(true));
        // sibling data preserved
        assert_eq!(
            root.get("projects").unwrap().get("D:/x").unwrap().get("lastCost").and_then(Value::as_f64),
            Some(0.5)
        );
    }

    #[test]
    fn already_trusted_is_a_no_op() {
        let src = r#"{"projects":{"D:/x":{"hasTrustDialogAccepted":true}}}"#;
        let mut root = json::parse(src).unwrap();
        assert!(!set_trusted_in(&mut root, "D:/x"), "should report no change");
    }

    #[test]
    fn preserves_other_projects_and_top_level_keys() {
        let src = r#"{"numStartups":5,"projects":{"D:/a":{"hasTrustDialogAccepted":true},"D:/b":{"hasTrustDialogAccepted":false}}}"#;
        let mut root = json::parse(src).unwrap();
        assert!(set_trusted_in(&mut root, "D:/b"));
        assert_eq!(trusted(&root, "D:/a"), Some(true)); // untouched
        assert_eq!(trusted(&root, "D:/b"), Some(true)); // flipped
        assert_eq!(root.get("numStartups").and_then(Value::as_i64), Some(5));
    }

    #[test]
    fn creates_projects_map_when_absent() {
        let src = r#"{"numStartups":1}"#;
        let mut root = json::parse(src).unwrap();
        assert!(set_trusted_in(&mut root, "D:/x"));
        assert_eq!(trusted(&root, "D:/x"), Some(true));
        assert_eq!(root.get("numStartups").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn project_key_uses_forward_slashes_no_trailing() {
        // Absolute paths pass through with separators normalized and any
        // trailing slash trimmed — tested per-platform (an absolute path is
        // platform-specific; a Unix path isn't absolute on Windows).
        #[cfg(windows)]
        {
            assert_eq!(project_key(Path::new("D:\\projects\\x")), "D:/projects/x");
            assert_eq!(project_key(Path::new("D:/projects/x/")), "D:/projects/x");
        }
        #[cfg(unix)]
        {
            assert_eq!(project_key(Path::new("/home/u/proj")), "/home/u/proj");
            assert_eq!(project_key(Path::new("/home/u/proj/")), "/home/u/proj");
        }
    }
}
