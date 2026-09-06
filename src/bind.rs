//! The binder: pane session ids × live sessions → each pane's agent status.
//!
//! atrium injects `--session-id <uuid>` when it launches an agent pane (§3.3 of
//! the 0.3 design) and remembers that uuid on the pane. The transcript stem *is*
//! the session id, so binding a pane to its agent's status collapses to an O(1)
//! lookup: find the [`agsess::AgentSession`] whose `id` equals the pane's uuid
//! and read its already-derived `status`.
//!
//! This module is that lookup and nothing more — a **pure function** over the
//! remembered ids and a `&[agsess::AgentSession]` snapshot, free of I/O so it is
//! testable without a real `World`. A pane with no uuid (a shell, or an agent
//! atrium did not launch with the flag) never binds; a uuid with no matching
//! transcript yet stays unbound until the file appears; a binding clears the
//! moment the pane leaves the input set (its child exited).
//!
//! `agsess::AgentSession` cannot be built outside its crate (it carries private
//! tail state and no public constructor), so the lookup is expressed against a
//! tiny [`Bindable`] trait that `AgentSession` implements here. The pure logic
//! is then unit-tested against a trivial in-test stand-in — no real `World`, no
//! I/O — while the app calls it with real sessions.

/// The command file-stems atrium treats as agents worth hosting with agent-aware
/// chrome (§3.3 step 0): status binding, layout, identity-env injection, and the
/// `ctl` spawn allowlist. Claude Code plus the other agent CLIs atrium can host. A
/// set so a new vendor is one entry, not a branch.
///
/// Membership here is deliberately broad — it only says "this pane is an agent,
/// give it agent treatment". It does **not** imply the vendor speaks Claude
/// Code's CLI dialect; that is a separate, narrower question ([`CLAUDE_STEMS`]).
pub const AGENT_STEMS: &[&str] = &["claude", "gemini", "codex", "aider", "cursor-agent"];

/// The subset of [`AGENT_STEMS`] that speak Claude Code's CLI dialect. The flags
/// atrium injects at spawn — `--session-id`, the trust-posture flags
/// (`--permission-mode`, `--dangerously-skip-permissions`), and
/// `--append-system-prompt` — plus the `~/.claude.json` folder-trust gate are all
/// Claude-specific. Passing them to another vendor's CLI would break its launch,
/// so atrium only adds them for a claude stem. Every other agent launches with its
/// command untouched and stays **unbound** (no status chrome) until per-vendor
/// transcript/status parsing lands in `agsess` — see the board note for that
/// deeper work.
pub const CLAUDE_STEMS: &[&str] = &["claude"];

/// Command flags that mean the *user* already chose a session identity, so atrium
/// must not inject its own `--session-id` (it would override or conflict). Covers
/// an explicit id (`--session-id`), a resume (`--resume`/`-r`), and a continue
/// (`--continue`/`-c`).
const USER_SESSION_ARGS: &[&str] = &["--session-id", "--resume", "-r", "--continue", "-c"];

/// Is this pane an agent worth binding? True iff the `file_stem` of the command
/// (its title, as atrium already derives it) is in [`AGENT_STEMS`]. Non-agent
/// panes (shells, editors) are never bound and never get agent chrome.
pub fn is_agent_stem(stem: &str) -> bool {
    AGENT_STEMS.contains(&stem)
}

/// Does this stem speak Claude Code's CLI dialect? True only for a claude stem.
/// Gates every Claude-specific flag injection ([`session_id_for`] and, in the
/// spawn core, the trust-posture / `--append-system-prompt` / folder-trust steps)
/// so a non-claude agent — recognized as an agent by [`is_agent_stem`] — still
/// launches with its own command untouched.
pub fn is_claude_stem(stem: &str) -> bool {
    CLAUDE_STEMS.contains(&stem)
}

/// Extract a command's **stem** (basename without extension) in a way that is
/// platform-independent for the path separator.
///
/// `std::path::Path::file_stem` only treats `\` as a separator on Windows, so a
/// Windows-authored fleet command like `C:\tools\claude.cmd` yields the stem
/// `C:\tools\claude` on macOS/Linux — and the pane is then not recognized as an
/// agent (no status chrome, no `--session-id`, no identity injection). atrium fleet
/// configs are shared across platforms, so we split on **both** `/` and `\` on
/// every OS, take the last component, then drop a single trailing extension.
/// `claude`, `claude.exe`, `claude.cmd`, `C:\x\claude.exe`, and `/usr/bin/claude`
/// all map to `claude`. A leading-dot name (`.bashrc`) keeps its whole name, like
/// `file_stem`. On Windows this is byte-identical to the old `file_stem` path.
pub fn command_stem(cmd: &str) -> String {
    // Last path component, splitting on either separator regardless of the host OS.
    let base = cmd.rsplit(['/', '\\']).next().unwrap_or(cmd);
    // Drop a single trailing extension (…claude.exe → claude); a leading dot is not
    // an extension (`.bashrc` stays whole), matching `Path::file_stem`.
    match base.rfind('.') {
        Some(i) if i > 0 => base[..i].to_string(),
        _ => base.to_string(),
    }
}

/// Did the user already pass a session-selecting flag? If so atrium leaves the
/// command untouched — the user owns that id (a `--resume` reopens a transcript
/// whose stem is the user's, not one atrium minted), so no `--session-id` inject.
pub fn has_user_session_arg(args: &[String]) -> bool {
    args.iter().any(|a| {
        USER_SESSION_ARGS.contains(&a.as_str())
            // also catch the `--session-id=<uuid>` / `--resume=<id>` glued form
            || USER_SESSION_ARGS
                .iter()
                .any(|f| f.starts_with("--") && a.starts_with(&format!("{f}=")))
    })
}

/// Decide whether atrium should inject `--session-id <uuid>` for a pane launching
/// `command`, and if so return the fresh uuid to inject and remember. `command`
/// is the *user's* command vector (`command[0]` is the program, the rest args),
/// before any Windows `cmd /C` shim wrapping — the agent decision is about what
/// the user asked to run, not how the platform hosts it.
///
/// Returns `Some(uuid)` only for a **claude** command with no user-supplied
/// session arg; `None` for shells, non-agents, a non-claude agent (whose CLI does
/// not understand `--session-id`), or a claude agent the user already gave a
/// session identity. On `Some`, the caller appends `--session-id <uuid>` to the
/// agent's args and stores `uuid` on the pane for the binder.
pub fn session_id_for(command: &[String]) -> Option<String> {
    let stem = command_stem(&command[0]);
    // `--session-id` is a Claude Code flag; only claude gets an injected id.
    if !is_claude_stem(&stem) {
        return None;
    }
    if has_user_session_arg(&command[1..]) {
        return None;
    }
    Some(crate::uid::v4())
}

/// The two fields the binder reads off a session: its id and its status. A seam
/// so the pure logic is testable without constructing an [`agsess::AgentSession`]
/// (which has no public constructor).
pub trait Bindable {
    fn id(&self) -> &str;
    fn status(&self) -> agsess::Status;
}

impl Bindable for agsess::AgentSession {
    fn id(&self) -> &str {
        &self.id
    }
    fn status(&self) -> agsess::Status {
        self.status
    }
}

/// Resolve one pane's agent status from the live session snapshot.
///
/// `pane_id` is the uuid atrium injected at spawn, or `None` for a non-agent pane
/// (a shell) or an agent pane atrium did not bind. Returns `Some(status)` only
/// when a session whose id equals `pane_id` exists in `sessions`; otherwise
/// `None` — the pane stays unbound and wears its local `PaneState` chrome.
pub fn status_for<S: Bindable>(pane_id: Option<&str>, sessions: &[S]) -> Option<agsess::Status> {
    let id = pane_id?;
    sessions.iter().find(|s| s.id() == id).map(|s| s.status())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsess::Status;

    /// A stand-in session for the pure binder tests — the real
    /// `agsess::AgentSession` has no public constructor.
    struct FakeSession {
        id: String,
        status: Status,
    }

    impl Bindable for FakeSession {
        fn id(&self) -> &str {
            &self.id
        }
        fn status(&self) -> Status {
            self.status
        }
    }

    fn session(id: &str, status: Status) -> FakeSession {
        FakeSession {
            id: id.to_string(),
            status,
        }
    }

    #[test]
    fn matching_id_binds_to_its_status() {
        let sessions = vec![
            session("aaaa", Status::WaitingApproval),
            session("bbbb", Status::Working),
        ];
        assert_eq!(status_for(Some("bbbb"), &sessions), Some(Status::Working));
        assert_eq!(
            status_for(Some("aaaa"), &sessions),
            Some(Status::WaitingApproval)
        );
    }

    #[test]
    fn unknown_id_stays_unbound() {
        let sessions = vec![session("aaaa", Status::Working)];
        // The transcript for this pane's uuid has not appeared yet.
        assert_eq!(status_for(Some("no-such-id"), &sessions), None);
    }

    #[test]
    fn a_pane_with_no_uuid_never_binds() {
        let sessions = vec![session("aaaa", Status::WaitingApproval)];
        // A shell pane carries no session id, so it can never acquire agent chrome.
        assert_eq!(status_for(None, &sessions), None);
    }

    #[test]
    fn binding_clears_when_the_pane_leaves_the_input_set() {
        // Model a pane whose child exited: atrium stops passing its uuid (None),
        // so even though the transcript still lingers in the snapshot, the pane
        // no longer binds. This is the pure contract that only a live pane's id
        // can bind; the run loop drops exited panes so their id never arrives.
        let sessions = vec![session("gone", Status::WaitingPrompt)];
        assert_eq!(status_for(None, &sessions), None);
    }

    #[test]
    fn empty_snapshot_binds_nothing() {
        assert_eq!(status_for::<FakeSession>(Some("aaaa"), &[]), None);
    }

    // --- the spawn-side inject decision ------------------------------------

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn agent_command_gets_a_fresh_session_id() {
        let id = session_id_for(&cmd(&["claude"])).expect("agent binds");
        // A syntactically valid v4 uuid (uid.rs guarantees the shape).
        assert_eq!(id.split('-').count(), 5, "{id}");
    }

    #[test]
    fn agent_command_with_args_still_binds() {
        assert!(session_id_for(&cmd(&["claude", "--model", "opus"])).is_some());
    }

    #[test]
    fn windows_style_path_stem_is_recognized_as_agent() {
        // atrium derives the title from the command stem; the inject decision must
        // too. Both a Windows-style path and a Unix path resolve to `claude` on
        // EVERY host (this failed on macOS while the stem used Path::file_stem,
        // which keeps the backslashes off-Windows).
        assert!(session_id_for(&cmd(&["C:\\tools\\claude.cmd"])).is_some());
        assert!(session_id_for(&cmd(&["/usr/local/bin/claude"])).is_some());
    }

    #[test]
    fn command_stem_is_platform_independent() {
        // Both separators are honored on every OS; a single trailing extension is
        // dropped; a leading-dot name is kept whole (like Path::file_stem).
        for (input, want) in [
            ("claude", "claude"),
            ("claude.exe", "claude"),
            ("claude.cmd", "claude"),
            ("C:\\tools\\claude.cmd", "claude"),
            ("C:/tools/claude.exe", "claude"),
            ("/usr/local/bin/claude", "claude"),
            ("cursor-agent", "cursor-agent"),
            ("C:\\bin\\cursor-agent.exe", "cursor-agent"),
            (".bashrc", ".bashrc"),
            ("codex", "codex"),
        ] {
            assert_eq!(command_stem(input), want, "stem of {input:?}");
        }
        // The vendor agents are recognized from a Windows path too (the defect: a
        // Windows-authored fleet config silently losing agent chrome on macOS).
        for v in ["gemini", "codex", "aider", "cursor-agent"] {
            let winpath = format!("C:\\Program Files\\{v}\\{v}.exe");
            assert!(is_agent_stem(&command_stem(&winpath)), "{v} via win path");
        }
    }

    #[test]
    fn shell_and_non_agent_panes_never_bind() {
        assert_eq!(session_id_for(&cmd(&["cmd"])), None);
        assert_eq!(session_id_for(&cmd(&["sh", "-i"])), None);
        assert_eq!(session_id_for(&cmd(&["bash"])), None);
        assert_eq!(session_id_for(&cmd(&["vim"])), None);
    }

    // --- multi-vendor agent recognition ------------------------------------

    #[test]
    fn other_agent_clis_are_recognized_as_agents() {
        // These get agent-aware chrome (layout, identity env, ctl allowlist)…
        for stem in ["gemini", "codex", "aider", "cursor-agent"] {
            assert!(is_agent_stem(stem), "{stem} should be an agent");
        }
        // …including from a full path, exactly as atrium derives the title.
        assert!(is_agent_stem(
            std::path::Path::new("/usr/local/bin/gemini")
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap()
        ));
    }

    #[test]
    fn only_claude_speaks_the_claude_cli_dialect() {
        assert!(is_claude_stem("claude"));
        for stem in ["gemini", "codex", "aider", "cursor-agent", "cmd", "bash"] {
            assert!(!is_claude_stem(stem), "{stem} is not claude");
        }
    }

    #[test]
    fn non_claude_agents_get_no_session_id() {
        // Recognized as agents, but their CLI does not understand `--session-id`,
        // so atrium must not mint one — they launch with the command untouched.
        assert_eq!(session_id_for(&cmd(&["gemini"])), None);
        assert_eq!(session_id_for(&cmd(&["codex", "--model", "o1"])), None);
        assert_eq!(session_id_for(&cmd(&["aider"])), None);
        assert_eq!(session_id_for(&cmd(&["cursor-agent"])), None);
        assert_eq!(session_id_for(&cmd(&["/opt/bin/gemini"])), None);
    }

    #[test]
    fn user_supplied_session_args_suppress_injection() {
        assert_eq!(session_id_for(&cmd(&["claude", "--continue"])), None);
        assert_eq!(session_id_for(&cmd(&["claude", "-c"])), None);
        assert_eq!(session_id_for(&cmd(&["claude", "--resume"])), None);
        assert_eq!(session_id_for(&cmd(&["claude", "-r", "abc"])), None);
        assert_eq!(
            session_id_for(&cmd(&["claude", "--session-id", "deadbeef"])),
            None
        );
        // the glued `--flag=value` form is caught too
        assert_eq!(
            session_id_for(&cmd(&["claude", "--session-id=deadbeef"])),
            None
        );
        assert_eq!(session_id_for(&cmd(&["claude", "--resume=abc"])), None);
    }
}
