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

use crate::ctl::TrustMode;
use json::Value;

/// Environment knob (comma-separated command prefixes) that **extends** the
/// built-in `--trust` allowlist ([`DEFAULT_ALLOW`]) with the user's own safe dev
/// commands — e.g. `AMUX_TRUST_ALLOW="just build,make test"`. Each prefix P
/// becomes the claude matcher `Bash(P *)`, so it runs hands-off while anything
/// outside the list still prompts. Set by the human who launches amux.
pub const ENV_TRUST_ALLOW: &str = "AMUX_TRUST_ALLOW";

/// The built-in safe dev-command prefixes `--trust` ([`crate::ctl::TrustMode::Edits`])
/// lets an agent run **without a prompt** — the build/test/run loop. Read-only
/// shell (ls, cat, grep, `git status`, …) is already auto-accepted by claude's
/// `acceptEdits` mode, so it is not repeated here. Anything not on this list (and
/// not extended via [`ENV_TRUST_ALLOW`]) still surfaces as a visible approval
/// prompt in the pane — that is the safety of this mode.
const DEFAULT_ALLOW: &[&str] = &[
    // amux itself. Without this a coordinating fleet cannot use the coordination
    // layer: every `amux ctl bus pub` / `board claim` in an agent's kickoff
    // surfaces an approval prompt, so the operator is asked to approve the very
    // messages the fleet exists to exchange. Seven agents then queue up asking
    // permission to do their jobs.
    //
    // Safe to allow now in a way it was not before: a nested `amux --trust skip`
    // is capped to this session's policy by the ancestry check, `fleet up` is
    // gated the same way, and `ctl spawn` is separately governed by the spawn
    // allowlist and the trust ceiling. So the dangerous things amux can be asked
    // to do are already bounded, and what remains is talking to its own bus.
    "amux",
    "python",
    "python3",
    "pytest",
    "cargo test",
    "cargo build",
    "cargo check",
    "cargo clippy",
    "cargo fmt",
    "go test",
    "go build",
    "go vet",
    "node",
    "npm test",
];

/// Read [`ENV_TRUST_ALLOW`] into extra allow prefixes (trimmed, empties dropped).
pub fn extra_allow_from_env() -> Vec<String> {
    // Shares only the PARSING with `ctl::extra_allow_from_env`. The two read
    // different variables and gate different postures — this one widens claude's
    // own tool allowlist, that one the ctl spawn allowlist — so they must stay
    // separate functions even though their bodies once looked identical.
    crate::ctl::parse_allow_list(ENV_TRUST_ALLOW)
}

/// Build the claude CLI args for the safe hands-off `--trust` posture:
/// `--permission-mode acceptEdits` plus an `--allowedTools` allowlist mapping
/// each safe command prefix `P` (built-in [`DEFAULT_ALLOW`] then `extra`) to the
/// matcher `Bash(P *)`. Edits and those commands run without a prompt; everything
/// else still prompts (a visible approval in the pane). Pure and unit-tested.
pub fn accept_edits_args(extra: &[String]) -> Vec<String> {
    let mut args = vec![
        "--permission-mode".to_string(),
        "acceptEdits".to_string(),
        "--allowedTools".to_string(),
    ];
    let prefixes = DEFAULT_ALLOW
        .iter()
        .map(|s| s.to_string())
        .chain(extra.iter().cloned());
    for p in prefixes {
        let p = p.trim();
        if !p.is_empty() {
            args.push(format!("Bash({p} *)"));
        }
    }
    args
}

/// Build the **codex** (OpenAI) CLI args for a trust posture — the codex analog
/// of [`accept_edits_args`]. codex has no claude-style `--permission-mode`; its
/// autonomy is governed by `--ask-for-approval <policy>` + `--sandbox <mode>`
/// (verified against codex 0.153.0). Its approval granularity is coarser than
/// claude's — there is no per-command allowlist, only `on-request`/`never` — so
/// the hands-off-but-safe postures (`Edits`, `Auto`) both map to *never prompt,
/// confined to the workspace*; `Skip` is the full parity with claude's
/// `--dangerously-skip-permissions` (no approvals, no sandbox); `Plan` is a
/// read-only sandbox (inspect but never modify or run mutating commands). Pure
/// and unit-tested. NOTE: codex also has a per-folder *trust_level* stored in
/// `~/.codex/config.toml` (its own "trust this folder?" gate); writing that
/// safely needs a TOML-preserving editor and is a deliberate follow-up — these
/// flags deliver hands-off approval, which is the piece that was missing.
pub fn codex_trust_args(mode: TrustMode) -> Vec<String> {
    let s = |x: &str| x.to_string();
    match mode {
        // Auto-approve, but confined to the working tree — the safe hands-off default.
        TrustMode::Edits | TrustMode::Auto => {
            vec![
                s("--ask-for-approval"),
                s("never"),
                s("--sandbox"),
                s("workspace-write"),
            ]
        }
        // Full bypass: no approvals, no sandbox. The human confirmed `skip` at launch.
        TrustMode::Skip => vec![s("--dangerously-bypass-approvals-and-sandbox")],
        // Read-only exploration: codex may inspect but not modify or run mutating cmds.
        TrustMode::Plan => {
            vec![
                s("--sandbox"),
                s("read-only"),
                s("--ask-for-approval"),
                s("never"),
            ]
        }
        // `Off` → no flags, by contract: a caller must not rely on Off producing a
        // posture (the spawn path never calls this with Off — a trusted launch gates
        // it out). Total-match returning empty keeps the fn safe if ever called with Off.
        TrustMode::Off => Vec::new(),
    }
}

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
    ensure_trusted_at(&path, dir)
}

/// [`ensure_trusted`] against an explicit config path.
///
/// Split out purely so this can be TESTED. The public entry point resolves
/// `~/.claude.json` from the environment and writes it, so exercising it in a
/// test would mutate the developer's real claude config — which is why the one
/// function here that touches another tool's live state had no coverage at all.
/// With the path injectable, the guarantees that actually matter — never clobber
/// a file we could not parse, never write JSON we cannot read back — can be
/// asserted against a temp file.
pub fn ensure_trusted_at(path: &Path, dir: &Path) -> Result<Outcome, String> {
    let key = project_key(dir);

    // Absent config: claude creates it on launch, but if we create a minimal one
    // first, the trust bit is already set when it reads. Seed just the projects
    // entry; claude fills in the rest on its own read-merge-write.
    let src = match std::fs::read_to_string(path) {
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
    write_atomic(path, &out)?;
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
        root.get("projects")?
            .get(key)?
            .get(KEY_TRUST)
            .and_then(Value::as_bool)
    }

    #[test]
    fn sets_trust_on_a_fresh_config() {
        let mut root = json::parse("{}").unwrap();
        assert!(set_trusted_in(&mut root, "D:/x"));
        assert_eq!(trusted(&root, "D:/x"), Some(true));
        // onboarding set too
        assert_eq!(
            root.get("projects")
                .unwrap()
                .get("D:/x")
                .unwrap()
                .get(KEY_ONBOARDED),
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
            root.get("projects")
                .unwrap()
                .get("D:/x")
                .unwrap()
                .get("lastCost")
                .and_then(Value::as_f64),
            Some(0.5)
        );
    }

    #[test]
    fn already_trusted_is_a_no_op() {
        let src = r#"{"projects":{"D:/x":{"hasTrustDialogAccepted":true}}}"#;
        let mut root = json::parse(src).unwrap();
        assert!(
            !set_trusted_in(&mut root, "D:/x"),
            "should report no change"
        );
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
    fn accept_edits_args_map_prefixes_to_bash_matchers() {
        let args = accept_edits_args(&["just build".to_string()]);
        // mode + allowlist header present
        assert_eq!(args[0], "--permission-mode");
        assert_eq!(args[1], "acceptEdits");
        assert_eq!(args[2], "--allowedTools");
        // a built-in prefix and the extra both become Bash(P *) matchers
        assert!(args.iter().any(|a| a == "Bash(pytest *)"), "{args:?}");
        assert!(args.iter().any(|a| a == "Bash(cargo test *)"), "{args:?}");
        assert!(args.iter().any(|a| a == "Bash(just build *)"), "{args:?}");
        // and it does NOT open all of bash
        assert!(
            !args.iter().any(|a| a == "Bash" || a == "Bash(*)"),
            "{args:?}"
        );
    }

    #[test]
    fn codex_trust_args_map_each_posture_to_codex_flags() {
        // Edits/Auto → never-prompt, workspace-sandboxed autonomy.
        for m in [TrustMode::Edits, TrustMode::Auto] {
            let a = codex_trust_args(m);
            assert_eq!(
                a,
                vec![
                    "--ask-for-approval",
                    "never",
                    "--sandbox",
                    "workspace-write"
                ],
                "{m:?}"
            );
        }
        // Skip → full bypass (parity with claude --dangerously-skip-permissions).
        assert_eq!(
            codex_trust_args(TrustMode::Skip),
            vec!["--dangerously-bypass-approvals-and-sandbox"]
        );
        // Plan → read-only sandbox, no prompts.
        assert_eq!(
            codex_trust_args(TrustMode::Plan),
            vec!["--sandbox", "read-only", "--ask-for-approval", "never"]
        );
        // Off → nothing (never applied in a trusted launch).
        assert!(codex_trust_args(TrustMode::Off).is_empty());
        // Crucially, none of these are claude flags (would break codex's launch).
        for m in [
            TrustMode::Edits,
            TrustMode::Auto,
            TrustMode::Skip,
            TrustMode::Plan,
        ] {
            let a = codex_trust_args(m);
            assert!(
                !a.iter()
                    .any(|x| x == "--permission-mode" || x == "--allowedTools"),
                "{m:?}"
            );
        }
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

    fn tmp(nonce: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("amux-trust-{}-{nonce}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// Seeds a config that does not exist yet, so the trust bit is already set
    /// when claude first reads it.
    #[test]
    fn ensure_trusted_seeds_a_missing_config() {
        let d = tmp("fresh");
        let cfg = d.join(".claude.json");
        let _ = std::fs::remove_file(&cfg);
        let out = ensure_trusted_at(&cfg, &d).expect("should seed");
        assert!(matches!(out, Outcome::NowTrusted));
        let text = std::fs::read_to_string(&cfg).expect("file written");
        let root = json::parse(&text).expect("valid JSON written");
        assert_eq!(trusted(&root, &project_key(&d)), Some(true));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **Never clobber a config we could not parse.** This is the guarantee that
    /// matters most here: the file belongs to another tool, and amux overwriting
    /// a config it did not understand would destroy the user's settings. Untested
    /// until now, on the one function in this module that touches live state.
    #[test]
    fn ensure_trusted_refuses_to_touch_unparseable_json() {
        let d = tmp("bad");
        let cfg = d.join(".claude.json");
        let garbage = "{ this is not json at all ";
        std::fs::write(&cfg, garbage).unwrap();
        let err = ensure_trusted_at(&cfg, &d).expect_err("must refuse");
        assert!(err.contains("left untouched"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            garbage,
            "amux overwrote a config it could not parse"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A valid config that is not an object is equally off-limits.
    #[test]
    fn ensure_trusted_refuses_a_non_object_config() {
        let d = tmp("array");
        let cfg = d.join(".claude.json");
        std::fs::write(&cfg, "[1,2,3]").unwrap();
        let err = ensure_trusted_at(&cfg, &d).expect_err("must refuse");
        assert!(err.contains("left untouched"), "unexpected error: {err}");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "[1,2,3]");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Existing unrelated settings survive, and a second call is a no-op.
    #[test]
    fn ensure_trusted_preserves_other_settings_and_is_idempotent() {
        let d = tmp("keep");
        let cfg = d.join(".claude.json");
        std::fs::write(&cfg, r#"{"theme":"dark","projects":{}}"#).unwrap();
        assert!(matches!(
            ensure_trusted_at(&cfg, &d).unwrap(),
            Outcome::NowTrusted
        ));
        let root = json::parse(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            root.get("theme").and_then(Value::as_str),
            Some("dark"),
            "an unrelated setting was lost"
        );
        assert!(matches!(
            ensure_trusted_at(&cfg, &d).unwrap(),
            Outcome::AlreadyTrusted
        ));
        let _ = std::fs::remove_dir_all(&d);
    }
}
