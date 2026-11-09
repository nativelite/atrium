//! Certainty status for a claude pane without the mod, through Claude Code's
//! own settings hooks.
//!
//! atrium reads a claude pane's status from its session transcript (agsess):
//! a permission prompt shows as `waiting-approval` only after the transcript
//! has ended on an unresolved tool call for `APPROVAL_DWELL_MS`, plus the
//! refresh interval, so the yellow border arrives seven to eight seconds after
//! the prompt. The mod reports the change the instant it happens, but it is a
//! plugin the user installs.
//!
//! Claude Code also runs *settings hooks*: shell commands on named events,
//! configured in a settings file, and a `--settings <file>` flag that adds one
//! for a single session on top of the user's own. In a ctl session atrium
//! writes one such file beside its state (`hooks/claude-status.json`) with a
//! command per event that runs `atrium ctl report status=…` with the pane's
//! own token, inherited from the pane's environment, and passes the file to
//! every claude pane whose argv does not already carry `--settings`. The
//! broker treats the report as it treats the mod's: certain, ahead of agsess.
//!
//! | event | reported |
//! | --- | --- |
//! | `UserPromptSubmit`, `PreToolUse`, `PermissionDenied` | `working` |
//! | `PermissionRequest` | `waiting-approval` |
//! | `Stop` | `waiting-prompt` |
//! | `SessionEnd` | `ended` |
//!
//! Each hook prints nothing (`--quiet`) and exits 0, which for every one of
//! these events means "no decision": the permission prompt still shows, the
//! tool still runs. A hook can only ever speak for the pane it runs in, like
//! any `atrium ctl` caller.

use std::path::{Path, PathBuf};

/// The events and the status each one reports.
pub const EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", "working"),
    ("PreToolUse", "working"),
    ("PermissionDenied", "working"),
    ("PermissionRequest", "waiting-approval"),
    ("Stop", "waiting-prompt"),
    ("SessionEnd", "ended"),
];

/// The flag the settings file rides on.
pub const FLAG: &str = "--settings";

/// A JSON string literal: `"…"` with `\` and `"` escaped (the exe path has
/// backslashes on Windows; nothing else in the file is user-controlled).
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The hook command for one status: the exe quoted for the shell the hook
/// runs in (`cmd` or `sh`; double quotes mean the same to both for a path),
/// then `ctl report status=<s> --quiet`.
pub fn command(exe: &Path, status: &str) -> String {
    format!("\"{}\" ctl report status={status} --quiet", exe.display())
}

/// The settings file's content for `exe` (this atrium).
pub fn settings_json(exe: &Path) -> String {
    let mut out = String::from("{\n  \"hooks\": {\n");
    for (i, (event, status)) in EVENTS.iter().enumerate() {
        out.push_str(&format!(
            "    \"{event}\": [{{\"hooks\": [{{\"type\": \"command\", \"command\": {}}}]}}]{}\n",
            json_str(&command(exe, status)),
            if i + 1 < EVENTS.len() { "," } else { "" }
        ));
    }
    out.push_str("  }\n}\n");
    out
}

/// Should atrium add its file to this argv? Not when the user already passes
/// `--settings`: Claude Code takes one, and the user's wins.
pub fn wants(argv: &[String]) -> bool {
    !argv
        .iter()
        .any(|a| a == FLAG || a.starts_with("--settings="))
}

/// Where the file lives: beside the sessions store (`root` is
/// [`crate::session_store::state_root`]), as `hooks/claude-status.json`.
pub fn hooks_path(root: &Path) -> PathBuf {
    root.parent()
        .unwrap_or(root)
        .join("hooks")
        .join("claude-status.json")
}

/// Write the file for `exe` if it is missing or differs; return its path.
pub fn ensure(exe: &Path, root: &Path) -> Result<PathBuf, String> {
    let path = hooks_path(root);
    let want = settings_json(exe);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(want.as_str()) {
        return Ok(path);
    }
    if let Some(dir) = path.parent() {
        crate::session_store::ensure_private_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    crate::session_store::write_private(&path, &want)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"C:\Users\me\.cargo\bin\atrium.exe"
        } else {
            "/home/me/.cargo/bin/atrium"
        })
    }

    #[test]
    fn the_settings_file_is_json_with_one_hook_per_event() {
        let text = settings_json(&exe());
        let v = json::parse(&text).expect("valid json");
        let hooks = v
            .get("hooks")
            .and_then(json::Value::as_object)
            .expect("hooks");
        assert_eq!(hooks.len(), EVENTS.len());
        for (event, status) in EVENTS {
            let arr = v
                .get("hooks")
                .and_then(|h| h.get(event))
                .and_then(json::Value::as_array)
                .unwrap_or_else(|| panic!("{event}"));
            let cmd = arr[0]
                .get("hooks")
                .and_then(json::Value::as_array)
                .and_then(|a| a[0].get("command"))
                .and_then(json::Value::as_str)
                .unwrap();
            assert!(
                cmd.ends_with(&format!("ctl report status={status} --quiet")),
                "{cmd}"
            );
            assert!(
                cmd.starts_with(&format!("\"{}\"", exe().display())),
                "{cmd}"
            );
        }
    }

    #[test]
    fn a_user_settings_flag_wins() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(wants(&s(&["claude", "--session-id", "x"])));
        assert!(!wants(&s(&["claude", "--settings", "mine.json"])));
        assert!(!wants(&s(&["claude", "--settings={\"a\":1}"])));
    }

    #[test]
    fn ensure_writes_once_and_rewrites_when_the_exe_moves() {
        let root = std::env::temp_dir().join(format!("atrium-hooks-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let p = ensure(&exe(), &sessions).unwrap();
        assert_eq!(p, root.join("hooks").join("claude-status.json"));
        let first = std::fs::read_to_string(&p).unwrap();
        assert_eq!(ensure(&exe(), &sessions).unwrap(), p);
        let other = ensure(&exe().with_file_name("atrium2.exe"), &sessions).unwrap();
        assert_ne!(std::fs::read_to_string(&other).unwrap(), first);
        let _ = std::fs::remove_dir_all(&root);
    }
}
