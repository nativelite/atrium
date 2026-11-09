//! Certainty status for a codex pane without the mod, through codex's own
//! hooks — the codex counterpart of [`crate::status_hooks`].
//!
//! atrium reads a codex pane's status from its rollout transcript (agsess),
//! with the same lag a claude pane had before 0.43.0: an approval prompt shows
//! as `waiting-approval` only after the transcript has sat on an unresolved
//! tool call for the dwell, plus the refresh interval. codex runs *hooks* too:
//! shell commands on named events, the Claude Code shape (`hooks.<Event>` is
//! a list of matcher groups, each a list of `{type = "command", command}`
//! handlers; JSON on stdin, exit 0 and no stdout means "no decision"), read
//! from `$CODEX_HOME/hooks.json`, a project's `.codex/hooks.json`, and the
//! `[hooks]` table of every enabled config layer. The `-c key=value` overrides
//! on the command line are one such layer (`session-flags`), so a pane's
//! argv can carry the hooks itself — atrium writes nothing into the user's
//! codex home or the project, and the hooks live and die with the pane.
//!
//! Verified against the codex fork at `repos/codex` (e9ff0fae01, 2026-10-21)
//! and the installed `codex-cli 0.163.0` binary, which carries the
//! `<session-flags>` hook source and the TOML hooks loader:
//!
//! - `config/src/loader/mod.rs:423-428`: the `-c` overrides become the
//!   `ConfigLayerSource::SessionFlags` layer; `utils/cli/src/config_override.rs:49-97`:
//!   a dotted key, the value parsed as TOML.
//! - `hooks/src/engine/discovery.rs:144-200`, `421-438`: every enabled layer
//!   contributes its `[hooks]` table (`HookEventsToml`, `config/src/hook_config.rs:39-68`,
//!   the event names as written here); `config/src/config_toml.rs:497` has
//!   the `hooks` field, so `--strict-config` accepts it.
//! - `discovery.rs:893-908`: a hook with no trust hash is *untrusted*, and
//!   `discovery.rs:811-816` runs an enabled untrusted hook only under
//!   `--dangerously-bypass-hook-trust`; `tui/src/startup_hooks_review.rs:291-293`
//!   skips the "Hooks need review" screen under the same flag. atrium already
//!   passes it at any posture but `off` (0.42.0, [`crate::vendors::codex_hooks_unreviewed`]),
//!   so the hooks ride on that flag and are never added without it.
//! - `hooks/src/engine/command_runner.rs:406-430`: the command runs through
//!   `%COMSPEC% /C "<command>"` on Windows and `$SHELL -lc <command>` elsewhere,
//!   with codex's own environment (`hooks/src/registry.rs:83`, the pane's
//!   `ATRIUM_*` included), a 5 s default timeout (`hooks/src/engine/mod.rs:366`),
//!   and a hook that never reads its stdin is fine (`command_runner.rs:277-283`).
//! - `core/src/tools/approvals.rs:486`: `PermissionRequest` runs before the
//!   approval prompt; `hooks/src/events/permission_request.rs:205-210`: exit 0
//!   with empty stdout is no decision, the prompt still shows. `Stop`,
//!   `UserPromptSubmit`, `PreToolUse`, `Interrupt` and `SessionEnd` read an
//!   empty exit-0 the same way (`hooks/src/events/*.rs`).
//!
//! | event | reported |
//! | --- | --- |
//! | `UserPromptSubmit`, `PreToolUse` | `working` |
//! | `PermissionRequest` | `waiting-approval` |
//! | `Stop`, `Interrupt` | `waiting-prompt` |
//! | `SessionEnd` | `ended` |
//!
//! Each hook prints nothing (`--quiet`) and exits 0, so codex's own flow is
//! untouched: the prompt still shows, the tool still runs. A hook can only
//! ever speak for the pane it runs in, like any `atrium ctl` caller.

use std::path::Path;

/// The events and the status each one reports, as codex spells them
/// (`HookEventsToml`'s `#[serde(rename)]` names).
pub const EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", "working"),
    ("PreToolUse", "working"),
    ("PermissionRequest", "waiting-approval"),
    ("Stop", "waiting-prompt"),
    ("Interrupt", "waiting-prompt"),
    ("SessionEnd", "ended"),
];

/// The config table the hooks live under; a user's own override of it on the
/// command line means atrium adds none.
pub const TABLE: &str = "hooks";

/// The hook command for one status: the same text as a claude pane's
/// ([`crate::status_hooks::command`]), since both shells read a quoted path.
pub fn command(exe: &Path, status: &str) -> String {
    crate::status_hooks::command(exe, status)
}

/// The TOML value of one event's override: one matcher group matching every
/// tool, one command handler. Inline, since a `-c` value is a single TOML
/// value; the command is a basic string ([`crate::vendors::toml_string`]), so
/// the exe path's backslashes survive.
pub fn override_value(exe: &Path, status: &str) -> String {
    format!(
        "[{{hooks=[{{type=\"command\",command={}}}]}}]",
        crate::vendors::toml_string(&command(exe, status))
    )
}

/// The `-c hooks.<Event>=<value>` pairs for every event in [`EVENTS`], for
/// `exe` (this atrium).
pub fn args(exe: &Path) -> Vec<String> {
    let mut out = Vec::with_capacity(EVENTS.len() * 2);
    for (event, status) in EVENTS {
        out.extend(crate::vendors::codex_config(
            &format!("{TABLE}.{event}"),
            &override_value(exe, status),
        ));
    }
    out
}

/// Should atrium add its hooks to this argv? Only when it already carries
/// `--dangerously-bypass-hook-trust` — the posture is not `off`, this codex
/// has the flag, and the launch is the interactive session — since without
/// it an unreviewed hook is a "Hooks need review" screen; and never when the
/// user already overrides the `hooks` table on the command line: that is
/// their session's hooks, and theirs win.
pub fn wants(argv: &[String]) -> bool {
    argv.iter()
        .any(|a| a == crate::vendors::CODEX_BYPASS_HOOK_TRUST)
        && !has_user_hooks_override(argv)
}

/// Does `argv` carry a `-c`/`--config` override whose key is the `hooks`
/// table or anything under it?
fn has_user_hooks_override(argv: &[String]) -> bool {
    let is_hooks_key = |kv: &str| {
        kv == TABLE || kv.starts_with(&format!("{TABLE}.")) || kv.starts_with(&format!("{TABLE}="))
    };
    let mut iter = argv.iter().skip(1).take_while(|a| a.as_str() != "--");
    while let Some(a) = iter.next() {
        if a == "-c" || a == "--config" {
            if let Some(kv) = iter.next() {
                if is_hooks_key(kv) {
                    return true;
                }
            }
        } else if let Some(kv) = a.strip_prefix("--config=") {
            if is_hooks_key(kv) {
                return true;
            }
        } else if let Some(kv) = a.strip_prefix("-c") {
            // clap's attached short value: `-chooks.Stop=…`.
            if !kv.is_empty() && !kv.starts_with('-') && is_hooks_key(kv) {
                return true;
            }
        }
    }
    false
}

/// `command` with atrium's hooks appended when [`wants`] says so, else as
/// given. Appended, like the `-c developer_instructions` fold: clap takes
/// options after the prompt positional.
pub fn add(command: &[String], exe: &Path) -> Vec<String> {
    let mut out = command.to_vec();
    if wants(command) {
        out.extend(args(exe));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn exe() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"C:\Users\me\.cargo\bin\atrium.exe"
        } else {
            "/home/me/.cargo/bin/atrium"
        })
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const BYPASS: &str = "--dangerously-bypass-hook-trust";

    /// The inverse of a TOML basic string for the two escapes a path or a
    /// command can need (`\\`, `\"`); anything else in the value is a bug.
    fn toml_unquote(lit: &str) -> String {
        let inner = lit
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .expect("a quoted basic string");
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    other => panic!("unexpected escape {other:?} in {lit}"),
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn every_event_reports_a_word_ctl_report_accepts() {
        let accepted = [
            "working",
            "waiting-approval",
            "waiting-prompt",
            "idle",
            "errored",
            "ended",
        ];
        for (event, status) in EVENTS {
            assert!(accepted.contains(status), "{event} -> {status}");
        }
        // The map the module promises.
        let status = |e: &str| EVENTS.iter().find(|(n, _)| *n == e).map(|(_, s)| *s);
        assert_eq!(status("UserPromptSubmit"), Some("working"));
        assert_eq!(status("PreToolUse"), Some("working"));
        assert_eq!(status("PermissionRequest"), Some("waiting-approval"));
        assert_eq!(status("Stop"), Some("waiting-prompt"));
        assert_eq!(status("Interrupt"), Some("waiting-prompt"));
        assert_eq!(status("SessionEnd"), Some("ended"));
    }

    #[test]
    fn one_override_per_event_with_one_command_handler() {
        let a = args(&exe());
        assert_eq!(a.len(), EVENTS.len() * 2);
        for (i, (event, status)) in EVENTS.iter().enumerate() {
            assert_eq!(a[2 * i], "-c");
            let kv = &a[2 * i + 1];
            let (key, value) = kv.split_once('=').expect("key=value");
            assert_eq!(key, format!("hooks.{event}"));
            // One matcher group, every tool (no `matcher`), one command.
            let head = "[{hooks=[{type=\"command\",command=";
            let tail = "}]}]";
            assert!(value.starts_with(head), "{value}");
            assert!(value.ends_with(tail), "{value}");
            let lit = &value[head.len()..value.len() - tail.len()];
            assert_eq!(toml_unquote(lit), command(&exe(), status));
        }
    }

    #[test]
    fn the_command_is_the_quoted_exe_then_a_quiet_report() {
        for (_, status) in EVENTS {
            let cmd = command(&exe(), status);
            assert_eq!(
                cmd,
                format!("\"{}\" ctl report status={status} --quiet", exe().display())
            );
        }
        // The claude file and the codex argv run the same text.
        assert_eq!(
            command(&exe(), "working"),
            crate::status_hooks::command(&exe(), "working")
        );
    }

    #[test]
    fn hooks_ride_on_the_bypass_flag_only() {
        // No flag: posture off, an older codex, a subcommand, `--remote`.
        assert!(!wants(&s(&["codex"])));
        assert!(!wants(&s(&["codex", "--no-daemon", "do the thing"])));
        assert!(!wants(&s(&["codex", "resume", "--last"])));
        // The flag, wherever atrium or the user put it.
        assert!(wants(&s(&["codex", BYPASS, "--no-daemon"])));
        assert!(wants(&s(&["codex", "--no-daemon", BYPASS, "a prompt"])));
    }

    #[test]
    fn a_users_hooks_override_wins() {
        let base = ["codex", BYPASS];
        let with = |extra: &[&str]| {
            let mut v = s(&base);
            v.extend(s(extra));
            v
        };
        assert!(!wants(&with(&["-c", "hooks.Stop=[]"])));
        assert!(!wants(&with(&["--config", "hooks.PreToolUse=[]"])));
        assert!(!wants(&with(&["--config=hooks.SessionEnd=[]"])));
        assert!(!wants(&with(&["-chooks.Stop=[]"])));
        assert!(!wants(&with(&["-c", "hooks={}"])));
        // Other overrides, and a `hooks` word elsewhere, change nothing.
        assert!(wants(&with(&["-c", "model=\"o3\""])));
        assert!(wants(&with(&[
            "-c",
            "developer_instructions=\"edit hooks.rs\""
        ])));
        assert!(wants(&with(&["-c", "features.hooks=true"])));
        assert!(wants(&with(&["hooks.Stop please"])));
        assert!(wants(&with(&["--", "-c", "hooks.Stop=[]"])));
    }

    #[test]
    fn add_appends_the_overrides_or_leaves_the_command_alone() {
        let given = s(&["codex", BYPASS, "--no-daemon", "-c", "model=\"o3\"", "hi"]);
        let out = add(&given, &exe());
        assert_eq!(&out[..given.len()], &given[..]);
        assert_eq!(&out[given.len()..], &args(&exe())[..]);

        let off = s(&["codex", "--no-daemon", "hi"]);
        assert_eq!(add(&off, &exe()), off);
        let theirs = s(&["codex", BYPASS, "-c", "hooks.Stop=[]"]);
        assert_eq!(add(&theirs, &exe()), theirs);
    }

    #[test]
    fn the_value_is_a_single_toml_inline_value_on_one_line() {
        for (_, status) in EVENTS {
            let v = override_value(&exe(), status);
            assert!(!v.contains('\n'), "{v}");
            // Brackets balance outside the string literal.
            let mut depth = 0i32;
            let mut in_str = false;
            let mut esc = false;
            for c in v.chars() {
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else if c == '"' {
                        in_str = false;
                    }
                    continue;
                }
                match c {
                    '"' => in_str = true,
                    '[' | '{' => depth += 1,
                    ']' | '}' => depth -= 1,
                    _ => {}
                }
                assert!(depth >= 0, "{v}");
            }
            assert_eq!(depth, 0, "{v}");
            assert!(!in_str, "{v}");
        }
    }
}
