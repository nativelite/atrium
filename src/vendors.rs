//! Multi-vendor session worlds: light up a non-claude agent pane in the
//! overview the same way a claude pane already lights up.
//!
//! atrium today holds **one** [`agsess::World`] over the claude projects root and
//! only claude panes ever bind (`src/main.rs`). This module generalizes that to
//! **one world per supported vendor** ([`VendorWorlds`]), plus the small pure
//! helpers each pane needs: which vendor a command stem produces transcripts for
//! ([`vendor_for_stem`]), where that vendor writes them ([`vendor_root`]), how to
//! associate a *started* non-claude pane with a discovered session when atrium
//! could not hand it a `--session-id` ([`adopt_session_for`]), and the overview
//! decoration for a node's vendor ([`vendor_tag`]).
//!
//! ## Supported vs. recognised (be realistic — founder directive 2026-09-03)
//! We only build worlds for the two vendors that (a) run as a terminal CLI in a
//! pane and (b) write append-only JSONL atrium can tail: [`SUPPORTED_VENDORS`] =
//! **Claude (Anthropic)** and **Codex (OpenAI)**. Claude's format is verified;
//! Codex writes rollout JSONL (`~/.codex/sessions/**/rollout-*.jsonl`), which the
//! agsess codex parser reads. Every other `agsess::Vendor` (Gemini, Aider, Cursor,
//! Copilot, Qwen, OpenCode, Goose) keeps its stem/root/tag helpers here but **no
//! world is built** — those panes show a tag yet aren't status-tracked.
//!
//! Google's individual CLIs were inspected against a live install and are
//! excluded on purpose: the Gemini CLI writes a growing JSON *array*, and the
//! Antigravity CLI uses protobuf + SQLite — neither is a tailable JSONL log, so
//! they don't fit this model (see [`SUPPORTED_VENDORS`] for the specifics).
//!
//! ## Verification status
//! Both supported vendors are confirmed against live installs: Claude from day
//! one, and Codex 0.153.0 on 2026-09-03 (a real codex pane bound and appeared in
//! the atrium activity log; its transcripts live exactly at
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`). The mechanism is also proven
//! end-to-end against a **synthetic** codex root in
//! `vendorworlds_discovers_adopts_and_binds_a_codex_pane`. Any *future* vendor
//! stays labeled with a `NOTE` until likewise verified; never a guessed path
//! dressed up as confirmed.

use agsess::{Status, Vendor, World};
use std::path::PathBuf;

/// The `codex` block of a fleet file: what it may set about a codex pane
/// beyond its posture.
pub mod perms;

/// Every vendor atrium knows how to *recognise* (map a stem/root/tag for). This is
/// the total set mirroring `agsess::Vendor`; the compiler does **not** force it to
/// stay total, so a new `agsess::Vendor` variant must be added here too. Building
/// a world is gated separately by [`SUPPORTED_VENDORS`].
pub const ALL_VENDORS: &[Vendor] = &[
    Vendor::ClaudeCode,
    Vendor::Gemini,
    Vendor::Codex,
    Vendor::Aider,
    Vendor::CursorAgent,
    Vendor::Copilot,
    Vendor::Qwen,
    Vendor::OpenCode,
    Vendor::Goose,
];

/// The vendors atrium actually builds a world for and stands behind — **Claude
/// (Anthropic)** and **Codex (OpenAI)**. Both write append-only JSONL transcripts,
/// which is exactly what atrium's tail-by-byte-offset model reads.
/// [`VendorWorlds::new`] enumerates exactly these. Everything else in
/// [`ALL_VENDORS`] is recognised (stem → tag in the overview) but not tracked.
///
/// Google's individual CLIs are **deliberately excluded** (verified against a live
/// install 2026-09-03): the Gemini CLI writes a single growing JSON *array*
/// (`~/.gemini/tmp/<user>/logs.json`), not line-delimited JSON, and the Antigravity
/// CLI stores conversations as protobuf + SQLite (`~/.gemini/antigravity-cli/`,
/// `.pb` files + `conversation_summaries.db`). Neither is tailable by the current
/// model, so tracking them would be a separate SQLite/array integration, not a
/// parser tweak — and we don't claim what we can't read.
pub const SUPPORTED_VENDORS: &[Vendor] = &[Vendor::ClaudeCode, Vendor::Codex];

/// Map a pane command stem to the `agsess::Vendor` it produces transcripts for.
///
/// `"claude"` → `ClaudeCode`, `"gemini"` → `Gemini`, `"codex"` → `Codex`,
/// `"aider"` → `Aider`, `"cursor-agent"` → `CursorAgent`, `"copilot"` →
/// `Copilot`, `"qwen"` → `Qwen`, `"opencode"` → `OpenCode`, `"goose"` → `Goose`;
/// any other stem (a shell, an editor) → `None`.
///
/// `stem` is the command file-stem as atrium derives it (e.g. `"cursor-agent"`
/// with the hyphen, not the display name). Recognising a stem does not imply a
/// world is built for it — see [`SUPPORTED_VENDORS`].
pub fn vendor_for_stem(stem: &str) -> Option<Vendor> {
    // A `claude_aliases` command (`claude2`, a second account's shim) is claude
    // here too, or its pane had no vendor: no status, no overview tag.
    if crate::bind::is_claude_stem(stem) {
        return Some(Vendor::ClaudeCode);
    }
    match stem {
        "claude" => Some(Vendor::ClaudeCode),
        "gemini" => Some(Vendor::Gemini),
        "codex" => Some(Vendor::Codex),
        "aider" => Some(Vendor::Aider),
        "cursor-agent" => Some(Vendor::CursorAgent),
        "copilot" => Some(Vendor::Copilot),
        "qwen" => Some(Vendor::Qwen),
        "opencode" => Some(Vendor::OpenCode),
        "goose" => Some(Vendor::Goose),
        _ => None,
    }
}

/// Codex's flag for running in its own process instead of the shared daemon.
pub const CODEX_NO_DAEMON: &str = "--no-daemon";

/// A codex launch with [`CODEX_NO_DAEMON`] inserted after the program, when the
/// launch can take it.
///
/// From 0.157 an interactive codex attaches to one per-user background server
/// (`codex app-server --managed-daemon`), forked by whichever codex started
/// first, anywhere, and outliving it. The agent's shell commands run as that
/// server's children with *its* environment, not the pane's. So a codex pane
/// lost `ATRIUM_CTL`/`ATRIUM_PANE`/`ATRIUM_TOKEN` when the server came from
/// outside atrium (an editor's codex), and worse, ran as another pane's
/// capability when it came from an earlier atrium pane — and every codex on the
/// host, inside atrium or not, then carried that pane's token. Without the
/// daemon codex runs commands from the process atrium spawned, in the pane's env.
///
/// Left as given: a command that already says it, one that targets a remote
/// server (`--remote`), and the `queue`/`agents` subcommands, which codex
/// refuses the flag on. `supported` is whether this codex has the flag at all
/// (older ones fail on an unknown argument); the caller probes, this stays pure.
pub fn codex_daemonless(command: &[String], supported: bool) -> Vec<String> {
    let mut out = command.to_vec();
    if !supported || command.is_empty() {
        return out;
    }
    let rest = &command[1..];
    let opts = rest.iter().take_while(|a| a.as_str() != "--");
    let mut skip = false;
    let mut seen_positional = false;
    for a in opts {
        if a == CODEX_NO_DAEMON || a == "--remote" || a.starts_with("--remote=") {
            skip = true;
            break;
        }
        if !seen_positional && !a.starts_with('-') {
            seen_positional = true;
            if a == "queue" || a == "agents" {
                skip = true;
                break;
            }
        }
    }
    if !skip {
        out.insert(1, CODEX_NO_DAEMON.to_string());
    }
    out
}

/// Codex's flag for running its enabled hooks without its startup review.
pub const CODEX_BYPASS_HOOK_TRUST: &str = "--dangerously-bypass-hook-trust";

/// A codex launch with [`CODEX_BYPASS_HOOK_TRUST`] inserted after the program,
/// when the launch can take it.
///
/// A codex whose enabled hooks (a plugin's, a folder's `.codex/hooks.json`)
/// carry no trust hash in its `config.toml` opens on a "Hooks need review"
/// screen instead of its prompt, and no posture flag skips it: a hands-off
/// pane sat there, and the brief typed at it picked the screen's first item
/// (measured, codex 0.163.0: "Review hooks" is highlighted, so Enter opened
/// the hooks browser and the brief was lost). The flag is codex's own way to
/// run those hooks unreviewed, which is what a posture that never prompts
/// means; the fleet banner says so, since hooks run outside the sandbox.
///
/// Left as given: a command that already says it, one that targets a remote
/// server (`--remote`), and one that runs a subcommand other than the
/// interactive session ([`CODEX_SUBCOMMANDS`]): only the interactive launch
/// (bare, or with a prompt) is known to take the flag. `supported` is whether
/// this codex has the flag at all (0.163.0 does; older ones fail on an unknown
/// argument); the caller probes, this stays pure.
pub fn codex_hooks_unreviewed(command: &[String], supported: bool) -> Vec<String> {
    let mut out = command.to_vec();
    if !supported || command.is_empty() {
        return out;
    }
    let rest = &command[1..];
    let opts = rest.iter().take_while(|a| a.as_str() != "--");
    let mut skip = false;
    let mut seen_positional = false;
    for a in opts {
        if a == CODEX_BYPASS_HOOK_TRUST || a == "--remote" || a.starts_with("--remote=") {
            skip = true;
            break;
        }
        if !seen_positional && !a.starts_with('-') {
            seen_positional = true;
            if CODEX_SUBCOMMANDS.contains(&a.as_str()) {
                skip = true;
                break;
            }
        }
    }
    if !skip {
        out.insert(1, CODEX_BYPASS_HOOK_TRUST.to_string());
    }
    out
}

/// codex's subcommands that are not the interactive session (`codex --help`,
/// 0.163.0). A first positional that is one of these names a subcommand; any
/// other is the interactive session's prompt.
pub const CODEX_SUBCOMMANDS: &[&str] = &[
    "agents",
    "exec",
    "e",
    "review",
    "login",
    "logout",
    "mcp",
    "plugin",
    "app-server",
    "remote-control",
    "app",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "resume",
    "queue",
    "archive",
    "delete",
    "migrate-rollouts",
    "unarchive",
    "fork",
    "cloud",
    "exec-server",
    "features",
    "help",
];

/// The text `vendor`'s TUI shows while a startup screen of its own is up
/// instead of the prompt, where typing is lost or picks an item nobody chose.
/// A queued send is held while any is on screen, however long it has waited
/// ([`crate::deliver`]): codex's hooks review ("Hooks need review") and its
/// folder trust ("Trust this folder?"), both measured on 0.163.0. atrium
/// clears both before a trusted launch ([`crate::trust::ensure_codex_trusted`],
/// [`codex_hooks_unreviewed`]); the hold covers an older codex, an untrusted
/// launch and a pane someone else started. Empty for every other vendor.
pub fn dialog_markers(vendor: Vendor) -> &'static [&'static str] {
    match vendor {
        Vendor::Codex => &["Hooks need review", "Trust this folder?"],
        _ => &[],
    }
}

/// `s` as a TOML basic string, quotes included.
///
/// Codex parses the value of `-c key=value` as TOML and falls back to the raw
/// text only when that fails, so a raw value is unpredictable: a prompt that
/// happens to read `true` or `[1]` becomes a boolean or an array and codex
/// refuses to start. Quoted, every value is a string.
pub fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One codex config override: `-c <key>=<value>`, `value` already TOML.
pub fn codex_config(key: &str, value: &str) -> [String; 2] {
    ["-c".to_string(), format!("{key}={value}")]
}

/// The codex permission profile atrium defines for a pane that must reach the
/// control plane. Named so it can't merge into a profile of the user's own.
pub const CODEX_CTL_PROFILE: &str = "atrium_ctl";

/// The codex feature that enforces a profile's network rules.
pub const CODEX_NETWORK_PROXY: &str = "network_proxy";

/// The text `vendor`'s TUI shows once its prompt is up and empty, which a pane
/// of it has on screen when it is ready to be typed into — the shortcuts hint
/// under the input box, the same words in codex and claude (`? for
/// shortcuts`, measured: codex 0.163.0's welcome screen and after each turn;
/// claude's `shortcuts-hint`). `None` for a vendor whose prompt atrium cannot
/// tell, which gets the plain settle gate.
///
/// A queued send into an unbound pane waits for this ([`crate::deliver`]): a
/// TUI that has drawn something and gone quiet may still be starting (codex
/// sits on `loading` while it fetches its models), and a paste typed into
/// that boundary lands in the composer without its Enter, or is discarded —
/// measured, codex 0.163.0: a paste 1.5 s after launch is never submitted,
/// the same paste at 8 s, with the hint on screen, is.
pub fn prompt_marker(vendor: Vendor) -> Option<&'static str> {
    match vendor {
        Vendor::Codex | Vendor::ClaudeCode => Some("? for shortcuts"),
        _ => None,
    }
}

/// Does `vendor`'s TUI take a bracketed paste (`ESC[200~ … ESC[201~`) as one
/// insertion? codex (crossterm) and claude (ink) both parse the markers
/// whether or not atrium saw them enable the mode, so a send into their panes
/// is always typed as a paste: typed as keystrokes, codex's composer took a
/// multi-line text as a run of lines, lost its tail and never submitted.
pub fn takes_paste(vendor: Vendor) -> bool {
    matches!(vendor, Vendor::Codex | Vendor::ClaudeCode)
}

/// One filesystem rule of a codex permission profile: an absolute `path` and
/// whether codex may write under it (`write`) or only read (`read`). A read
/// rule inside a write rule is a carve-out; the deeper rule wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexFsRule {
    pub path: String,
    pub write: bool,
}

/// The rules that let git work in a sandboxed codex pane whose working
/// directory is a checkout, given its git dir and common dir as git spells
/// them ([`crate::worktree::git_dirs`]).
///
/// codex's `:workspace` sandbox keeps a checkout's `.git` read-only, and for a
/// linked worktree the git dir its `.git` file points at too, so `git add` and
/// `git commit` fail there with `Read-only file system` (measured, codex
/// 0.163.0 on Linux: `Unable to create '<main>/.git/worktrees/<name>/index.lock'`).
/// A fleet item is done only with a commit, so the pane gets explicit rules
/// instead: the common dir (objects, refs, logs, and the main checkout's own
/// index) and the worktree's git dir writable, with the files that run code
/// as the human the next time they use git outside the sandbox kept read-only:
/// `hooks/`, `config` (`core.hooksPath`, `core.fsmonitor`, `core.sshCommand`)
/// and the worktree's `config.worktree`. Measured under these rules: a commit
/// succeeds in a main checkout and in a linked worktree; creating a hook and
/// appending to `config` are refused; a rule naming a file that does not exist
/// yet is accepted.
pub fn codex_git_rules(git_dir: &str, common_dir: &str) -> Vec<CodexFsRule> {
    let rule = |path: String, write: bool| CodexFsRule { path, write };
    let mut v = vec![rule(common_dir.to_string(), true)];
    if git_dir != common_dir {
        v.push(rule(git_dir.to_string(), true));
    }
    v.push(rule(format!("{common_dir}/hooks"), false));
    v.push(rule(format!("{common_dir}/config"), false));
    v.push(rule(format!("{git_dir}/config.worktree"), false));
    v
}

/// The codex permission profile atrium launches a sandboxed pane under: the
/// built-in `base` (`":workspace"` or `":read-only"`) plus what atrium opens in
/// it — `fs`, filesystem rules such as [`codex_git_rules`], and `ctl`, the
/// control socket its commands may reach, as in [`codex_ctl_sandbox`]. A
/// profile replaces `--sandbox`, which codex refuses alongside one.
///
/// The filesystem rules are one inline table keyed by path, because codex
/// splits a dotted `-c` key on every `.`, quotes or not, and a path has dots.
pub fn codex_profile(base: &str, net: Option<&CodexNet<'_>>, fs: &[CodexFsRule]) -> Vec<String> {
    let p = CODEX_CTL_PROFILE;
    let mut v = Vec::new();
    v.extend(codex_config("default_permissions", &toml_string(p)));
    v.extend(codex_config(
        &format!("permissions.{p}.extends"),
        &toml_string(base),
    ));
    if !fs.is_empty() {
        let rules: Vec<String> = fs
            .iter()
            .map(|r| {
                let access = if r.write { "write" } else { "read" };
                format!("{}={}", toml_string(&r.path), toml_string(access))
            })
            .collect();
        v.extend(codex_config(
            &format!("permissions.{p}.filesystem"),
            &format!("{{{}}}", rules.join(",")),
        ));
    }
    if let Some(net) = net.filter(|n| n.any()) {
        if net.opened() {
            v.extend(codex_config(
                &format!("permissions.{p}.network.enabled"),
                "true",
            ));
            // `limited` lets only the allowed unix sockets through (HTTPS is
            // refused without MITM); the block's own mode when it opened the
            // network, `full` for every method and every host or the listed ones.
            v.extend(codex_config(
                &format!("permissions.{p}.network.mode"),
                &toml_string(net.mode.unwrap_or("limited")),
            ));
            if net.mode.is_some() {
                // A map with entries is an allow list to codex: denies alone
                // would refuse every host (measured). `*` allows the rest, so
                // `deny_domains` on its own means every host but these.
                let star = net.domains.is_empty() && !net.deny_domains.is_empty();
                let hosts: Vec<(&str, &str)> = star
                    .then_some(("*", "allow"))
                    .into_iter()
                    .chain(net.domains.iter().map(|d| (d.as_str(), "allow")))
                    .chain(net.deny_domains.iter().map(|d| (d.as_str(), "deny")))
                    .collect();
                if !hosts.is_empty() {
                    v.extend(codex_config(
                        &format!("permissions.{p}.network.domains"),
                        &access_table(hosts.into_iter()),
                    ));
                }
            }
            // An inline table, not a dotted key: codex splits a `-c` key on every `.`,
            // quotes or not, so the socket path's own dots would break it.
            let sockets: Vec<(&str, &str)> = net
                .ctl
                .into_iter()
                .chain(net.sockets.iter().map(String::as_str))
                .map(|s| (s, "allow"))
                .collect();
            if !sockets.is_empty() {
                v.extend(codex_config(
                    &format!("permissions.{p}.network.unix_sockets"),
                    &access_table(sockets.into_iter()),
                ));
            }
            if net.local {
                v.extend(codex_config(
                    &format!("permissions.{p}.network.allow_local_binding"),
                    "true",
                ));
            }
            v.push("--enable".to_string());
            v.push(CODEX_NETWORK_PROXY.to_string());
        }
        // The block's raw keys, as written: the rest of codex's profile.
        for (key, value) in net.raw {
            v.extend(codex_config(&format!("permissions.{p}.{key}"), value));
        }
    }
    v
}

/// What a codex permission profile opens on the network, and the rest of the
/// block: the control socket `ctl`; the network itself through codex's proxy
/// in `mode` (`"full"` or `"limited"`, `None` shut) to every host or the
/// `domains` listed, less `deny_domains`; `local`, loopback and local servers;
/// `sockets`, further unix sockets; and `raw`, other profile keys with their
/// TOML values ([`perms::CodexPerms::raw`]). See [`codex_profile`].
#[derive(Debug, Clone, Copy)]
pub struct CodexNet<'a> {
    pub ctl: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub domains: &'a [String],
    pub deny_domains: &'a [String],
    pub local: bool,
    pub sockets: &'a [String],
    pub raw: &'a [(String, String)],
}

impl CodexNet<'_> {
    /// Is the network opened at all (keys, proxy feature)? Nothing means the
    /// base's own no-network.
    pub fn opened(&self) -> bool {
        self.ctl.is_some() || self.mode.is_some() || !self.sockets.is_empty() || self.local
    }

    /// Is there anything to emit, the network or raw keys?
    pub fn any(&self) -> bool {
        self.opened() || !self.raw.is_empty()
    }
}

/// An inline TOML table of `key=access` pairs, the shape of codex's
/// `network.unix_sockets` and `network.domains` maps (`allow`/`deny`).
fn access_table<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let entries: Vec<String> = pairs
        .map(|(k, access)| format!("{}={}", toml_string(k), toml_string(access)))
        .collect();
    format!("{{{}}}", entries.join(","))
}

/// Codex's sandbox args for a pane that must reach atrium's control socket at
/// `ctl`, for the sandbox `base` (`":workspace"` or `":read-only"`).
///
/// Codex's sandbox counts a unix-socket connect as network, and its sandboxes
/// have no network, so a codex pane at `accept`, `automode` or `plan` could not
/// run `atrium ctl` at all (measured on macOS, codex 0.163.0: `Operation not
/// permitted`). This profile is `base` with the network on but routed through
/// codex's proxy in `limited` mode with no domain allowed, plus one allowed
/// unix socket: this pane's `ATRIUM_CTL`. Measured under it: the ctl socket
/// connects; another unix socket, HTTPS and raw TCP are refused; writes stay
/// confined as in `base`. A profile replaces `--sandbox`, which codex refuses
/// alongside one; the network proxy is an experimental codex feature, so the
/// caller checks this codex has it first.
pub fn codex_ctl_sandbox(base: &str, ctl: &str) -> Vec<String> {
    let net = CodexNet {
        ctl: Some(ctl),
        mode: None,
        domains: &[],
        deny_domains: &[],
        local: false,
        sockets: &[],
        raw: &[],
    };
    codex_profile(base, Some(&net), &[])
}

/// The filesystem root under which `vendor` writes session transcripts, if known.
///
/// `ClaudeCode` → [`agsess::default_root`] (verified). For every other vendor:
/// the path is sourced from published documentation and is a **researched-stub**
/// (not verified against a live install — see [`crate::vendors`] module doc).
/// Each non-claude vendor supports an env-var override so operators can redirect
/// the root without recompiling. Returns `None` for vendors with no centralised
/// transcript root (Aider, OpenCode, Copilot) — see per-arm NOTE.
pub fn vendor_root(vendor: Vendor) -> Option<PathBuf> {
    /// Resolve the user's home directory: USERPROFILE (Windows) → HOME (Unix).
    fn home() -> PathBuf {
        let h = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        PathBuf::from(h)
    }

    match vendor {
        // Verified: Claude Code writes to ~/.claude/projects/<project>/<session>.jsonl.
        Vendor::ClaudeCode => Some(agsess::default_root()),

        // NOTE(roots): recognised-not-tracked, NOT in SUPPORTED_VENDORS. Verified
        // against a live install 2026-09-03: the Gemini CLI does NOT write per-
        // session JSONL — it keeps a single growing JSON *array* at
        // ~/.gemini/tmp/<user>/logs.json plus chats under ~/.gemini/tmp/<user>/chats/.
        // A JSON array can't be append-tailed line-by-line, so no world is built
        // for Gemini today. (Google's newer Antigravity CLI is worse for this model
        // still: protobuf + SQLite under ~/.gemini/antigravity-cli/.) The path below
        // is retained only so a future array-aware integration has a starting point;
        // it is unused while Gemini stays out of SUPPORTED_VENDORS.
        // Override: ATRIUM_GEMINI_ROOT env var.
        Vendor::Gemini => Some(
            std::env::var("ATRIUM_GEMINI_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home().join(".gemini").join("tmp")),
        ),

        // NOTE(roots): VERIFIED against a live install (codex 0.153.0, 2026-09-03):
        // Codex CLI writes session transcripts to
        // ~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl — a real codex pane
        // bound and showed in the atrium activity log. Override: CODEX_HOME (the app's
        // own override) — replaces ~/.codex, so the sessions subdir becomes
        // $CODEX_HOME/sessions. (Auto-approval for unattended runs is codex-side:
        // `approval_policy = "never"` in ~/.codex/config.toml, or the launch flags
        // --ask-for-approval never --sandbox workspace-write.)
        Vendor::Codex => Some(
            std::env::var("CODEX_HOME")
                .map(|h| PathBuf::from(h).join("sessions"))
                .unwrap_or_else(|_| home().join(".codex").join("sessions")),
        ),

        // NOTE(roots): Aider writes to <project-cwd>/.aider.chat.history.md —
        // a per-project file with no centralised home root. A single-root World
        // cannot enumerate aider sessions; per-cwd discovery is required (deferred).
        // None is the correct honest answer here.
        Vendor::Aider => None,

        // NOTE(roots): researched-stub. Cursor Background Agent sub-agent
        // transcripts live under ~/.cursor/projects/<project-id>/agent-transcripts/
        // <session-id>/subagents/*.jsonl (from Cursor docs; not verified against a
        // live install). Discovery caveat: files are 4 levels deep.
        // Override: ATRIUM_CURSOR_ROOT env var. (experimental — not in SUPPORTED_VENDORS.)
        Vendor::CursorAgent => Some(
            std::env::var("ATRIUM_CURSOR_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home().join(".cursor").join("projects")),
        ),

        // NOTE(roots): GitHub Copilot CLI does not write a JSONL transcript
        // today — the path is hypothetical (a future CLI format, not yet shipped).
        // Returning None until a real root is confirmed.
        Vendor::Copilot => None,

        // NOTE(roots): researched-stub. Qwen Code writes session transcripts under
        // ~/.qwen/projects/<sanitized-cwd>/chats/<session>.jsonl (from Qwen Code
        // docs; not verified against a live install).
        // Override: ATRIUM_QWEN_ROOT env var. (experimental — not in SUPPORTED_VENDORS.)
        Vendor::Qwen => Some(
            std::env::var("ATRIUM_QWEN_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home().join(".qwen").join("projects")),
        ),

        // NOTE(roots): OpenCode stores sessions in SQLite
        // (~/.local/share/opencode/opencode.db on Linux; platform-equivalent
        // elsewhere). There is no tailable JSONL file on disk; a row-export or
        // CDC layer is needed before a World can discover sessions. Returning
        // None until that layer exists.
        Vendor::OpenCode => None,

        // NOTE(roots): researched-stub. Goose (Block) writes session transcripts
        // to a platform-specific path (from Goose docs; not verified):
        //   Linux / macOS: ~/.local/share/goose/sessions (XDG data home fallback)
        //   Windows:       %APPDATA%\goose\sessions
        // Override: ATRIUM_GOOSE_ROOT env var. (experimental — not in SUPPORTED_VENDORS.)
        Vendor::Goose => Some(
            std::env::var("ATRIUM_GOOSE_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    if let Ok(appdata) = std::env::var("APPDATA") {
                        // Windows
                        PathBuf::from(appdata).join("goose").join("sessions")
                    } else {
                        // Linux / macOS — honour XDG_DATA_HOME when set
                        std::env::var("XDG_DATA_HOME")
                            .map(|d| PathBuf::from(d).join("goose").join("sessions"))
                            .unwrap_or_else(|_| {
                                home()
                                    .join(".local")
                                    .join("share")
                                    .join("goose")
                                    .join("sessions")
                            })
                    }
                }),
        ),
    }
}

/// One [`agsess::World`] per **supported** vendor ([`SUPPORTED_VENDORS`]),
/// refreshed together and answering status across all of them. This is the
/// drop-in replacement for the single `world` in `src/main.rs`: it always
/// contains a `ClaudeCode` world (so claude behavior is unchanged) and
/// additionally a world for Gemini and Codex.
pub struct VendorWorlds {
    /// One world per supported vendor. Order is [`SUPPORTED_VENDORS`] order; the
    /// first is always the ClaudeCode world.
    worlds: Vec<World>,
}

impl VendorWorlds {
    /// Build a world for every vendor in [`SUPPORTED_VENDORS`] whose
    /// [`vendor_root`] is known. Always includes the ClaudeCode world (its root
    /// is always known), so this is a superset of today's single-world behavior.
    pub fn new() -> Self {
        Self::with_extra_claude_roots(crate::config::get().alias_transcript_roots())
    }

    /// The vendor worlds plus one claude world per extra root: the transcripts
    /// of each `claude_aliases` entry that runs under its own config dir.
    /// Without them a second account's panes never bound — their sessions were
    /// looked for under the first account's projects. A root equal to the
    /// default is not added twice.
    pub fn with_extra_claude_roots(extra: Vec<PathBuf>) -> Self {
        let mut worlds: Vec<World> = SUPPORTED_VENDORS
            .iter()
            .filter_map(|&vendor| vendor_root(vendor).map(|root| World::for_vendor(root, vendor)))
            .collect();
        let default_claude = vendor_root(Vendor::ClaudeCode);
        let mut seen: Vec<PathBuf> = Vec::new();
        for root in extra {
            if default_claude.as_ref() == Some(&root) || seen.contains(&root) {
                continue;
            }
            worlds.push(World::for_vendor(root.clone(), Vendor::ClaudeCode));
            seen.push(root);
        }
        VendorWorlds { worlds }
    }

    /// Refresh every world (full tail). Mirrors [`agsess::World::refresh`].
    pub fn refresh(&mut self) {
        for w in &mut self.worlds {
            w.refresh();
        }
    }

    /// Refresh every world with a cold-start cutoff. Mirrors
    /// [`agsess::World::refresh_since`] — atrium passes its process start time so a
    /// first scan never blocks on transcripts that stopped writing before atrium.
    pub fn refresh_since(&mut self, cutoff_ms: u64) {
        for w in &mut self.worlds {
            w.refresh_since(cutoff_ms);
        }
    }

    /// Resolve a pane's agent status from any vendor's sessions.
    ///
    /// `session_id` is the id atrium stamped on the pane — an injected uuid for
    /// claude, or an [`adopt_session_for`] result for another vendor; `None` for
    /// an unbound pane. Session ids are unique across worlds (uuids / distinct
    /// transcript stems), so the first match across worlds is the answer and
    /// search order does not affect correctness.
    pub fn status_for(&self, session_id: Option<&str>) -> Option<Status> {
        let id = session_id?;
        self.worlds
            .iter()
            .flat_map(|w| w.sessions.iter())
            .find(|s| s.id == id)
            .map(|s| s.status)
    }

    /// Does the pane's bound session end on an unresolved `tool_use` — a running
    /// tool, or a question / approval dialog open on screen? `false` when
    /// unbound. See [`agsess::AgentSession::awaiting_tool`].
    pub fn awaiting_tool_for(&self, session_id: Option<&str>) -> bool {
        let Some(id) = session_id else {
            return false;
        };
        self.worlds
            .iter()
            .flat_map(|w| w.sessions.iter())
            .find(|s| s.id == id)
            .is_some_and(|s| s.awaiting_tool())
    }

    /// Every session across every vendor world, for overview counts and for
    /// [`adopt_session_for`] to scan. Flattened; no ordering guarantee beyond
    /// each world's own most-recent-first sort.
    pub fn sessions(&self) -> Vec<&agsess::AgentSession> {
        self.worlds.iter().flat_map(|w| w.sessions.iter()).collect()
    }
}

impl Default for VendorWorlds {
    fn default() -> Self {
        Self::new()
    }
}

impl VendorWorlds {
    /// What the UI reads, copied out: the run loop holds this, never the worlds.
    pub fn snapshot(&self) -> AgentState {
        AgentState {
            overrides: Default::default(),
            mod_facts: Default::default(),
            collisions: Vec::new(),
            sessions: self
                .worlds
                .iter()
                .flat_map(|w| w.sessions.iter())
                .map(SessionView::from)
                .collect(),
        }
    }
}

/// One session as the UI reads it: status for the bar and ctl, the facts
/// [`adopt_session_for`] decides on, and the last action for the overview and
/// the activity log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: String,
    pub vendor: Vendor,
    pub status: Status,
    /// [`agsess::AgentSession::awaiting_tool`], evaluated at the snapshot.
    pub awaiting_tool: bool,
    pub cwd: Option<String>,
    pub first_seen_ms: u64,
    pub last_ts_ms: Option<u64>,
    pub last_action: String,
}

impl From<&agsess::AgentSession> for SessionView {
    fn from(s: &agsess::AgentSession) -> SessionView {
        SessionView {
            id: s.id.clone(),
            vendor: s.vendor,
            status: s.status,
            awaiting_tool: s.awaiting_tool(),
            cwd: s.cwd.clone(),
            first_seen_ms: s.first_seen_ms,
            last_ts_ms: s.last_ts_ms,
            last_action: s.last_action.clone(),
        }
    }
}

/// A snapshot of every vendor world, answering the same questions as
/// [`VendorWorlds`] without touching a file. The run loop reads this; the worlds
/// are refreshed on an [`AgentWatch`] thread.
#[derive(Debug, Clone, Default)]
pub struct AgentState {
    sessions: Vec<SessionView>,
    /// Statuses a pane's mod reported, laid over the inferred ones by
    /// [`crate::modstate::ModState::overlay`]; keyed by session id. A fresh
    /// snapshot from the watch thread carries none, so the loop lays them
    /// again after every refresh.
    overrides: std::collections::HashMap<String, Status>,
    /// What else the mod reported for the same panes: the richer status
    /// (`errored`), its reason, the context fill and the cost. Laid and
    /// cleared with `overrides`.
    mod_facts: std::collections::HashMap<String, ModFacts>,
    /// Files two or more live panes edited, as the overlay computed them
    /// ([`crate::modstate::ModState::collisions`]); keyed by agent id.
    collisions: Vec<crate::modstate::Collision>,
}

/// What a pane's mod reported beyond the four inferred statuses: the chrome
/// reads `status` for the states the inference has no word for (`errored`),
/// the overview reads the fill and the cost.
#[derive(Debug, Clone, PartialEq)]
pub struct ModFacts {
    pub status: crate::modstate::ModStatus,
    pub reason: Option<String>,
    pub context_pct: Option<u8>,
    pub cost_usd: Option<f64>,
    /// The caption: what the pane is doing right now, in a few words.
    pub doing: Option<String>,
    /// The pane edited a file another live pane edited too.
    pub colliding: bool,
}

impl AgentState {
    /// See [`VendorWorlds::status_for`]. A status the pane's own mod reported
    /// (see [`crate::modstate`]) wins over the inference.
    pub fn status_for(&self, session_id: Option<&str>) -> Option<Status> {
        let id = session_id?;
        if let Some(s) = self.overrides.get(id) {
            return Some(*s);
        }
        self.sessions.iter().find(|s| s.id == id).map(|s| s.status)
    }

    /// Lay a reported status over the inferred one for `session_id`.
    pub fn set_status_override(&mut self, session_id: &str, status: Status) {
        self.overrides.insert(session_id.to_string(), status);
    }

    /// Drop every reported status and fact, back to the inference alone.
    pub fn clear_status_overrides(&mut self) {
        self.overrides.clear();
        self.mod_facts.clear();
        self.collisions.clear();
    }

    /// Record the live collisions the overlay computed.
    pub fn set_collisions(&mut self, collisions: Vec<crate::modstate::Collision>) {
        self.collisions = collisions;
    }

    /// Files two or more live panes edited, by agent id.
    pub fn collisions(&self) -> &[crate::modstate::Collision] {
        &self.collisions
    }

    /// The caption a pane's mod reported, when fresh.
    pub fn doing_for(&self, session_id: Option<&str>) -> Option<&str> {
        self.mod_facts_for(session_id)
            .and_then(|f| f.doing.as_deref())
    }

    /// Is the pane editing a file another live pane edits too?
    pub fn colliding_for(&self, session_id: Option<&str>) -> bool {
        self.mod_facts_for(session_id).is_some_and(|f| f.colliding)
    }

    /// Record what the pane's mod reported beyond its status.
    pub fn set_mod_facts(&mut self, session_id: &str, facts: ModFacts) {
        self.mod_facts.insert(session_id.to_string(), facts);
    }

    /// What the pane's mod reported, when it did and the report is fresh.
    pub fn mod_facts_for(&self, session_id: Option<&str>) -> Option<&ModFacts> {
        self.mod_facts.get(session_id?)
    }

    /// Did the pane's mod report its last turn ended on an error or a refusal?
    /// A state the transcript inference cannot see: the chrome shows it red.
    pub fn errored_for(&self, session_id: Option<&str>) -> bool {
        self.mod_facts_for(session_id)
            .is_some_and(|f| f.status == crate::modstate::ModStatus::Errored)
    }

    /// See [`VendorWorlds::awaiting_tool_for`].
    pub fn awaiting_tool_for(&self, session_id: Option<&str>) -> bool {
        session_id.is_some_and(|id| {
            self.sessions
                .iter()
                .find(|s| s.id == id)
                .is_some_and(|s| s.awaiting_tool)
        })
    }

    /// See [`VendorWorlds::sessions`].
    pub fn sessions(&self) -> Vec<&SessionView> {
        self.sessions.iter().collect()
    }
}

/// The vendor worlds, refreshed on a thread of their own.
///
/// A refresh walks every transcript root and stats every session in the history
/// — measured at ~135 ms on a 439-session root before `agsess` was made cheaper,
/// ~35 ms after — and it ran on the run loop once a second. That was both most
/// of atrium's idle CPU and its worst key-echo latency (the loop stalled up to
/// 153 ms). The thread refreshes and sends an [`AgentState`]; the loop takes the
/// newest one whenever it ticks.
pub struct AgentWatch {
    rx: std::sync::mpsc::Receiver<AgentState>,
    /// How often to refresh, in ms; the loop tunes it to what is on screen.
    every_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AgentWatch {
    /// The refresh interval while an agent pane or an agent view is up.
    pub const ACTIVE: std::time::Duration = std::time::Duration::from_millis(1000);
    /// The refresh interval when nothing on screen shows agent state.
    pub const QUIET: std::time::Duration = std::time::Duration::from_millis(5000);

    /// Start the thread. Its first refresh uses `cutoff_ms` (see
    /// [`VendorWorlds::refresh_since`]); later ones are full.
    pub fn start(cutoff_ms: u64) -> AgentWatch {
        use std::sync::atomic::Ordering;
        let (tx, rx) = std::sync::mpsc::channel();
        let every_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
            Self::ACTIVE.as_millis() as u64,
        ));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (interval, stopping) = (every_ms.clone(), stop.clone());
        let _ = std::thread::Builder::new()
            .name("atrium-agents".to_string())
            .spawn(move || {
                let mut worlds = VendorWorlds::new();
                worlds.refresh_since(cutoff_ms);
                let mut last = std::time::Instant::now();
                if tx.send(worlds.snapshot()).is_err() {
                    return;
                }
                while !stopping.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    let every = std::time::Duration::from_millis(interval.load(Ordering::SeqCst));
                    if last.elapsed() < every {
                        continue;
                    }
                    last = std::time::Instant::now();
                    worlds.refresh();
                    if tx.send(worlds.snapshot()).is_err() {
                        return;
                    }
                }
            });
        AgentWatch { rx, every_ms, stop }
    }

    /// The newest snapshot since the last call, if any.
    pub fn latest(&self) -> Option<AgentState> {
        self.rx.try_iter().last()
    }

    /// Refresh at most every `every` from now on.
    pub fn set_interval(&self, every: std::time::Duration) {
        self.every_ms.store(
            every.as_millis() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
    }
}

impl Drop for AgentWatch {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The facts [`adopt_session_for`] decides on, so it works on a live
/// [`agsess::AgentSession`] and on a [`SessionView`] snapshot alike.
pub trait AdoptFacts {
    fn session_id(&self) -> &str;
    fn session_cwd(&self) -> Option<&str>;
    fn first_seen(&self) -> u64;
}

impl AdoptFacts for agsess::AgentSession {
    fn session_id(&self) -> &str {
        &self.id
    }
    fn session_cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }
    fn first_seen(&self) -> u64 {
        self.first_seen_ms
    }
}

impl AdoptFacts for SessionView {
    fn session_id(&self) -> &str {
        &self.id
    }
    fn session_cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }
    fn first_seen(&self) -> u64 {
        self.first_seen_ms
    }
}

/// Associate a *started* non-claude pane with a discovered session, returning the
/// session id to stamp on the pane so [`VendorWorlds::status_for`] can bind it.
///
/// atrium cannot hand a non-claude CLI a `--session-id` (that flag is claude's), so
/// the pane launches with no id and we must *adopt* one: pick, from `sessions`,
/// the session that this pane most plausibly produced.
///
/// ## Strategy
///
/// 1. **Time gate** — discard any session whose `first_seen_ms < launch_ms`.
///    A transcript that predates the pane's spawn belongs to a previous run of
///    that vendor in the same root, not to this pane.
/// 2. **cwd preference** — if `pane_cwd` is `Some` *and* any candidate records a
///    matching `cwd` in its transcript, restrict the pool to those matches.
///    Vendors that do not write cwd to their transcripts (e.g. Gemini) leave
///    `session.cwd == None` on every session, so the cwd_match set is empty and
///    the full candidate pool is used.
/// 3. **Newest wins** — among the remaining candidates, return the one with the
///    largest `first_seen_ms`. When two same-vendor panes start in the same
///    refresh window, the more recently discovered transcript is marginally more
///    likely to be the one just launched.
///
/// Returns `None` when no candidate satisfies the time gate (caller retries on
/// the next [`VendorWorlds::refresh`]).
///
/// ## Caller contract
///
/// The caller is responsible for filtering `sessions` to the vendor that the
/// pane runs (e.g. via `s.vendor == vendor_for_stem(stem)`). This function is
/// vendor-blind and works on any slice of [`agsess::AgentSession`] references.
///
/// ## Known limits
///
/// - **Simultaneous same-vendor same-cwd launches**: if two panes of the same
///   vendor start in the same directory within the same refresh interval, both
///   have equal `first_seen_ms` and matching `cwd`; the result is arbitrary. In
///   practice this is rare and the run-loop will stamp both panes' session ids
///   after the next refresh, so at worst one pane stays grey for one poll interval.
/// - **Vendors without transcript cwd** (Gemini, …): the cwd filter does nothing,
///   so simultaneous launches of those vendors in *different* directories may bind
///   to each other. Increasing the poll rate shrinks the window.
pub fn adopt_session_for<S: AdoptFacts>(
    sessions: &[&S],
    pane_cwd: Option<&str>,
    launch_ms: u64,
) -> Option<String> {
    // Phase 1: time gate — transcripts first seen before this pane launched
    // belong to prior runs of the same vendor, not to this pane.
    let candidates: Vec<&S> = sessions
        .iter()
        .copied()
        .filter(|s| s.first_seen() >= launch_ms)
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Phase 2: cwd preference — narrow to sessions whose transcript-recorded
    // cwd matches the pane's working directory. Vendors that do not write cwd
    // to their transcripts leave session.cwd == None; those produce an empty
    // cwd_match set and fall through to the full candidate pool.
    let pool: Vec<&S> = if let Some(cwd) = pane_cwd {
        let cwd_match: Vec<&S> = candidates
            .iter()
            .copied()
            .filter(|s| s.session_cwd() == Some(cwd))
            .collect();
        if cwd_match.is_empty() {
            candidates
        } else {
            cwd_match
        }
    } else {
        candidates
    };

    // Phase 3: newest by first_seen_ms — among equal-confidence candidates, the
    // most recently discovered transcript is the best guess for the just-started pane.
    pool.into_iter()
        .max_by_key(|s| s.first_seen())
        .map(|s| s.session_id().to_string())
}

/// The overview glyph/label decoration for a node's vendor: a short 2–3 char
/// tag shown next to the status in the overview row so the reader knows which
/// agent CLI a pane is running. Rendered by `render_overview_panel` in
/// `src/main.rs` as `·{tag}` in dim style — identical to the identity decorator.
///
/// **Design rules:**
/// - `ClaudeCode` → `""` (empty): claude's appearance is **unchanged** — the
///   overview today shows claude panes correctly, and this tag must not disturb
///   them. An empty tag produces no output in the render helper.
/// - All other vendors → a unique 2–3 char ASCII abbreviation, all-lowercase,
///   chosen to be recognisable without being verbose.
pub fn vendor_tag(vendor: Vendor) -> &'static str {
    match vendor {
        Vendor::ClaudeCode => "", // keep claude's appearance unchanged
        Vendor::Gemini => "gem",
        Vendor::Codex => "cdx",
        Vendor::Aider => "aid",
        Vendor::CursorAgent => "cur",
        Vendor::Copilot => "cop",
        Vendor::Qwen => "qwn",
        Vendor::OpenCode => "oc",
        Vendor::Goose => "goo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// A codex that has the flag runs off the shared daemon, the flag right
    /// after the program so it precedes any subcommand; one that lacks it, or a
    /// launch codex refuses it on, is left exactly as given.
    #[test]
    fn codex_daemonless_inserts_the_flag_only_where_codex_takes_it() {
        let got = |a: &[&str], ok| codex_daemonless(&argv(a), ok);
        assert_eq!(got(&["codex"], true), argv(&["codex", "--no-daemon"]));
        assert_eq!(
            got(&["codex", "--model", "o3", "Start here"], true),
            argv(&["codex", "--no-daemon", "--model", "o3", "Start here"])
        );
        assert_eq!(
            got(&["codex", "resume", "--last"], true),
            argv(&["codex", "--no-daemon", "resume", "--last"])
        );
        // An older codex fails on an unknown flag: untouched.
        assert_eq!(
            got(&["codex", "-m", "o3"], false),
            argv(&["codex", "-m", "o3"])
        );
        // Already there, never twice.
        assert_eq!(
            got(&["codex", "--no-daemon"], true),
            argv(&["codex", "--no-daemon"])
        );
        // Codex refuses the flag with a remote server and on these subcommands.
        for a in [
            &["codex", "--remote", "ws://h:1"][..],
            &["codex", "--remote=ws://h:1"],
            &["codex", "queue", "list"],
            &["codex", "agents"],
        ] {
            assert_eq!(got(a, true), argv(a), "{a:?}");
        }
        // Only the first positional is a subcommand; a prompt that mentions one
        // later, or anything after `--`, does not count.
        assert_eq!(
            got(&["codex", "fix the queue", "--", "--remote"], true),
            argv(&["codex", "--no-daemon", "fix the queue", "--", "--remote"])
        );
    }

    /// A value is always a TOML string, whatever it reads like: codex would
    /// otherwise take `true` as a boolean and refuse to start.
    #[test]
    fn toml_string_quotes_and_escapes_every_value() {
        assert_eq!(toml_string("true"), r#""true""#);
        assert_eq!(toml_string(""), r#""""#);
        assert_eq!(
            toml_string("say \"hi\"\\ now\nthen\ttab\r"),
            r#""say \"hi\"\\ now\nthen\ttab\r""#
        );
        assert_eq!(toml_string("bell\u{7}"), r#""bell\u0007""#);
        // Non-ASCII passes through; TOML strings are UTF-8.
        assert_eq!(toml_string("caf\u{e9} \u{2014}"), "\"caf\u{e9} \u{2014}\"");
    }

    /// The exact argv codex 0.163.0 was measured with: the ctl socket connects,
    /// nothing else does. The socket map is an inline table, because codex
    /// splits a `-c` key on the path's own dots.
    #[test]
    fn codex_ctl_sandbox_is_the_measured_profile() {
        assert_eq!(
            codex_ctl_sandbox(":workspace", "/var/T/atrium-7.sock"),
            argv(&[
                "-c",
                r#"default_permissions="atrium_ctl""#,
                "-c",
                r#"permissions.atrium_ctl.extends=":workspace""#,
                "-c",
                "permissions.atrium_ctl.network.enabled=true",
                "-c",
                r#"permissions.atrium_ctl.network.mode="limited""#,
                "-c",
                r#"permissions.atrium_ctl.network.unix_sockets={"/var/T/atrium-7.sock"="allow"}"#,
                "--enable",
                "network_proxy",
            ])
        );
    }

    /// A profile with git rules carries them as one path-keyed inline table
    /// (`filesystem`, the key codex reads) before the network keys, and a
    /// profile with rules and no socket has no network keys and no feature.
    #[test]
    fn codex_profile_spells_filesystem_rules_as_one_inline_table() {
        let rules = vec![
            CodexFsRule {
                path: "/r/.git".into(),
                write: true,
            },
            CodexFsRule {
                path: "/r/.git/hooks".into(),
                write: false,
            },
        ];
        assert_eq!(
            codex_profile(":workspace", None, &rules),
            argv(&[
                "-c",
                r#"default_permissions="atrium_ctl""#,
                "-c",
                r#"permissions.atrium_ctl.extends=":workspace""#,
                "-c",
                r#"permissions.atrium_ctl.filesystem={"/r/.git"="write","/r/.git/hooks"="read"}"#,
            ])
        );
        let ctl = CodexNet {
            ctl: Some("/t/a.sock"),
            mode: None,
            domains: &[],
            deny_domains: &[],
            local: false,
            sockets: &[],
            raw: &[],
        };
        let with_ctl = codex_profile(":workspace", Some(&ctl), &rules);
        let fs = with_ctl
            .iter()
            .position(|a| a.starts_with("permissions.atrium_ctl.filesystem="))
            .unwrap();
        let net = with_ctl
            .iter()
            .position(|a| a.starts_with("permissions.atrium_ctl.network."))
            .unwrap();
        assert!(fs < net, "{with_ctl:?}");
        assert_eq!(with_ctl.last().map(String::as_str), Some("network_proxy"));
    }

    /// The git rules measured to let a sandboxed codex commit while hooks and
    /// config stay read-only: a main checkout names its one `.git` once, a
    /// linked worktree names its own git dir as well, and a Windows git dir
    /// (forward slashes, drive letter) is joined the same way.
    #[test]
    fn codex_git_rules_open_the_git_dirs_and_keep_hooks_and_config_read_only() {
        let paths = |rules: &[CodexFsRule]| -> Vec<(String, bool)> {
            rules.iter().map(|r| (r.path.clone(), r.write)).collect()
        };
        assert_eq!(
            paths(&codex_git_rules("/r/.git", "/r/.git")),
            vec![
                ("/r/.git".to_string(), true),
                ("/r/.git/hooks".to_string(), false),
                ("/r/.git/config".to_string(), false),
                ("/r/.git/config.worktree".to_string(), false),
            ]
        );
        assert_eq!(
            paths(&codex_git_rules("/r/.git/worktrees/dev", "/r/.git")),
            vec![
                ("/r/.git".to_string(), true),
                ("/r/.git/worktrees/dev".to_string(), true),
                ("/r/.git/hooks".to_string(), false),
                ("/r/.git/config".to_string(), false),
                ("/r/.git/worktrees/dev/config.worktree".to_string(), false),
            ]
        );
        let win = codex_git_rules("C:/p/.git/worktrees/dev", "C:/p/.git");
        assert_eq!(win[1].path, "C:/p/.git/worktrees/dev");
        assert_eq!(win[4].path, "C:/p/.git/worktrees/dev/config.worktree");
    }

    /// A block that opens the network gets `full` mode, its hosts as an allow
    /// map when it names any, and its sockets after the control socket; one
    /// with nothing to open gets no network keys at all.
    #[test]
    fn codex_profile_opens_the_network_as_the_block_says() {
        let hosts = vec!["api.github.com".to_string(), "*.crates.io".to_string()];
        let socks = vec!["/run/x.sock".to_string()];
        let net = CodexNet {
            ctl: Some("/t/a.sock"),
            mode: Some("full"),
            domains: &hosts,
            deny_domains: &[],
            local: false,
            sockets: &socks,
            raw: &[],
        };
        let v = codex_profile(":workspace", Some(&net), &[]).join(" ");
        assert!(v.contains(r#"network.mode="full""#), "{v}");
        assert!(
            v.contains(r#"network.domains={"api.github.com"="allow","*.crates.io"="allow"}"#),
            "{v}"
        );
        assert!(
            v.contains(r#"unix_sockets={"/t/a.sock"="allow","/run/x.sock"="allow"}"#),
            "{v}"
        );
        assert!(v.ends_with("--enable network_proxy"), "{v}");
        // Open to every host: no domains map at all.
        let every = CodexNet {
            ctl: None,
            mode: Some("full"),
            domains: &[],
            deny_domains: &[],
            local: false,
            sockets: &[],
            raw: &[],
        };
        let v = codex_profile(":workspace", Some(&every), &[]).join(" ");
        assert!(
            v.contains(r#"network.mode="full""#) && !v.contains("domains"),
            "{v}"
        );
        assert!(!v.contains("unix_sockets"), "{v}");
        // Nothing to open: the base alone.
        let shut = CodexNet {
            ctl: None,
            mode: None,
            domains: &hosts,
            deny_domains: &[],
            local: false,
            sockets: &[],
            raw: &[],
        };
        assert!(!shut.any());
        let v = codex_profile(":workspace", Some(&shut), &[]).join(" ");
        assert!(!v.contains("network"), "{v}");
    }

    /// Denied hosts ride in the same map as allowed ones, `local` and
    /// `limited` are spelled as codex's keys, and raw keys follow the named
    /// ones as written; raw keys alone open no network.
    #[test]
    fn codex_profile_spells_denied_hosts_local_binding_and_raw_keys() {
        let allow = vec!["api.github.com".to_string()];
        let deny = vec!["telemetry.example".to_string()];
        let raw = vec![(
            "network.proxy_url".to_string(),
            "\"http://127.0.0.1:3128\"".to_string(),
        )];
        let net = CodexNet {
            ctl: None,
            mode: Some("limited"),
            domains: &allow,
            deny_domains: &deny,
            local: true,
            sockets: &[],
            raw: &raw,
        };
        let v = codex_profile(":workspace", Some(&net), &[]).join(" ");
        assert!(v.contains(r#"network.mode="limited""#), "{v}");
        assert!(
            v.contains(r#"network.domains={"api.github.com"="allow","telemetry.example"="deny"}"#),
            "{v}"
        );
        assert!(v.contains("network.allow_local_binding=true"), "{v}");
        assert!(
            v.ends_with(r#"--enable network_proxy -c permissions.atrium_ctl.network.proxy_url="http://127.0.0.1:3128""#),
            "{v}"
        );
        // Denies alone: codex would refuse every host, so `*` allows the rest
        // (measured: `*` allow + one deny = that host refused, others answer).
        let deny_only = CodexNet {
            ctl: None,
            mode: Some("full"),
            domains: &[],
            deny_domains: &deny,
            local: false,
            sockets: &[],
            raw: &[],
        };
        let v = codex_profile(":workspace", Some(&deny_only), &[]).join(" ");
        assert!(
            v.contains(r#"network.domains={"*"="allow","telemetry.example"="deny"}"#),
            "{v}"
        );
        let raw_only = CodexNet {
            ctl: None,
            mode: None,
            domains: &[],
            deny_domains: &[],
            local: false,
            sockets: &[],
            raw: &raw,
        };
        assert!(raw_only.any() && !raw_only.opened());
        let v = codex_profile(":workspace", Some(&raw_only), &[]).join(" ");
        assert!(
            !v.contains("network.enabled") && !v.contains("--enable"),
            "{v}"
        );
        assert!(
            v.ends_with(r#"network.proxy_url="http://127.0.0.1:3128""#),
            "{v}"
        );
    }

    /// codex and claude both put `? for shortcuts` under an empty prompt; a
    /// vendor whose prompt atrium cannot tell has no marker to wait for.
    #[test]
    fn the_prompt_marker_is_the_shortcuts_hint_for_the_two_supported_vendors() {
        assert_eq!(prompt_marker(Vendor::Codex), Some("? for shortcuts"));
        assert_eq!(prompt_marker(Vendor::ClaudeCode), Some("? for shortcuts"));
        assert_eq!(prompt_marker(Vendor::Gemini), None);
    }

    /// Only codex has startup screens atrium knows to hold a send for.
    #[test]
    fn the_dialog_markers_are_codex_startup_screens() {
        assert_eq!(
            dialog_markers(Vendor::Codex),
            &["Hooks need review", "Trust this folder?"]
        );
        assert!(dialog_markers(Vendor::ClaudeCode).is_empty());
        assert!(dialog_markers(Vendor::Gemini).is_empty());
    }

    /// The hooks bypass goes right after the program of an interactive
    /// launch (bare, with options, with a prompt), and only there: a codex
    /// without the flag, a launch that already says it, a remote one and a
    /// subcommand are left as given.
    #[test]
    fn the_hooks_bypass_reaches_only_an_interactive_codex() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            codex_hooks_unreviewed(&s(&["codex"]), true),
            s(&["codex", CODEX_BYPASS_HOOK_TRUST])
        );
        assert_eq!(
            codex_hooks_unreviewed(&s(&["codex", "--no-daemon", "-m", "o3"]), true),
            s(&["codex", CODEX_BYPASS_HOOK_TRUST, "--no-daemon", "-m", "o3"])
        );
        assert_eq!(codex_hooks_unreviewed(&s(&["codex"]), false), s(&["codex"]));
        let said = s(&["codex", CODEX_BYPASS_HOOK_TRUST]);
        assert_eq!(codex_hooks_unreviewed(&said, true), said);
        let remote = s(&["codex", "--remote", "host:1"]);
        assert_eq!(codex_hooks_unreviewed(&remote, true), remote);
        for sub in ["exec", "resume", "queue", "features"] {
            let cmd = s(&["codex", sub, "task"]);
            assert_eq!(codex_hooks_unreviewed(&cmd, true), cmd, "{sub}");
        }
        assert_eq!(
            codex_hooks_unreviewed(&s(&["codex", "-m", "o3", "do the thing"]), true),
            s(&["codex", CODEX_BYPASS_HOOK_TRUST, "-m", "o3", "do the thing"])
        );
        assert!(codex_hooks_unreviewed(&[], true).is_empty());
    }

    /// The skeleton wiring holds: a fresh `VendorWorlds` always contains the
    /// claude world, `refresh` over (possibly missing) roots does not panic, and
    /// an unbound pane binds to nothing.
    #[test]
    fn vendorworlds_smoke() {
        let mut worlds = VendorWorlds::new();
        // ClaudeCode root is always known, so at least one world exists.
        assert!(!worlds.worlds.is_empty(), "claude world must be present");
        // Missing/real roots are tolerated by agsess — refresh must not panic.
        worlds.refresh();
        // A pane with no stamped id never binds.
        assert_eq!(worlds.status_for(None), None);
    }

    /// Only the supported two (Claude + Codex) get a world; every other vendor —
    /// Gemini included — is recognised but not built (be-realistic scope).
    #[test]
    fn only_supported_vendors_get_a_world() {
        assert_eq!(SUPPORTED_VENDORS, &[Vendor::ClaudeCode, Vendor::Codex]);
        // Google's CLIs are deliberately excluded (non-tailable storage).
        assert!(
            !SUPPORTED_VENDORS.contains(&Vendor::Gemini),
            "Gemini must not be tracked"
        );
        let worlds = VendorWorlds::new();
        // Claude + Codex both have a known root → two worlds.
        assert_eq!(
            worlds.worlds.len(),
            2,
            "exactly the supported two build a world"
        );
    }

    #[test]
    fn claude_stem_and_root_are_wired() {
        assert_eq!(vendor_for_stem("claude"), Some(Vendor::ClaudeCode));
        assert_eq!(vendor_for_stem("definitely-not-an-agent"), None);
        assert_eq!(
            vendor_root(Vendor::ClaudeCode),
            Some(agsess::default_root())
        );
    }

    // --- detect: vendor_for_stem ---

    #[test]
    fn vendor_for_stem_all_known_stems() {
        let cases: &[(&str, Vendor)] = &[
            ("claude", Vendor::ClaudeCode),
            ("gemini", Vendor::Gemini),
            ("codex", Vendor::Codex),
            ("aider", Vendor::Aider),
            ("cursor-agent", Vendor::CursorAgent),
            ("copilot", Vendor::Copilot),
            ("qwen", Vendor::Qwen),
            ("opencode", Vendor::OpenCode),
            ("goose", Vendor::Goose),
        ];
        for &(stem, expected) in cases {
            assert_eq!(
                vendor_for_stem(stem),
                Some(expected),
                "stem {stem:?} should map to {expected:?}"
            );
        }
    }

    #[test]
    fn vendor_for_stem_negative_cases() {
        // Shells, editors, partial matches, and case variants must all return None.
        for stem in &[
            "bash",
            "zsh",
            "sh",
            "fish",
            "nvim",
            "vim",
            "python",
            "",
            "CLAUDE",
            "Gemini",
            "cursor",
            "cursor_agent",
            "open-code",
        ] {
            assert_eq!(
                vendor_for_stem(stem),
                None,
                "stem {stem:?} should not map to any vendor"
            );
        }
    }

    #[test]
    fn vendor_for_stem_covers_all_vendors() {
        // Every Vendor in ALL_VENDORS must be reachable through some known stem so
        // no vendor is silently orphaned from the map.
        let known_stems = [
            "claude",
            "gemini",
            "codex",
            "aider",
            "cursor-agent",
            "copilot",
            "qwen",
            "opencode",
            "goose",
        ];
        for &vendor in ALL_VENDORS {
            let found = known_stems
                .iter()
                .any(|&s| vendor_for_stem(s) == Some(vendor));
            assert!(found, "{vendor:?} has no stem that maps to it");
        }
    }

    /// vendor_tag: claude must produce an empty string (appearance unchanged);
    /// every other vendor must produce a non-empty, non-whitespace tag.
    #[test]
    fn vendor_tag_claude_is_empty() {
        assert_eq!(vendor_tag(Vendor::ClaudeCode), "");
    }

    #[test]
    fn vendor_tag_non_claude_are_short_and_nonempty() {
        let non_claude = [
            Vendor::Gemini,
            Vendor::Codex,
            Vendor::Aider,
            Vendor::CursorAgent,
            Vendor::Copilot,
            Vendor::Qwen,
            Vendor::OpenCode,
            Vendor::Goose,
        ];
        for v in non_claude {
            let tag = vendor_tag(v);
            assert!(!tag.is_empty(), "{v:?} tag must be non-empty");
            assert!(tag.is_ascii(), "{v:?} tag must be ASCII: {tag:?}");
            assert!(
                tag.len() <= 4,
                "{v:?} tag must be ≤ 4 chars for the overview column: {tag:?}"
            );
            assert!(
                tag.chars().all(|c| !c.is_whitespace()),
                "{v:?} tag must not contain whitespace: {tag:?}"
            );
        }
    }

    /// vendor_tag: all 9 vendors are covered, producing distinct tags.
    #[test]
    fn vendor_tag_all_distinct() {
        let tags: Vec<&str> = ALL_VENDORS.iter().map(|&v| vendor_tag(v)).collect();
        // ClaudeCode is "" — the rest must be unique among themselves.
        let non_empty: Vec<&str> = tags.iter().copied().filter(|t| !t.is_empty()).collect();
        let mut deduped = non_empty.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            non_empty.len(),
            "non-claude vendor tags must all be distinct: {non_empty:?}"
        );
    }

    // --- roots: vendor_root ---

    // Mutex to serialise env-touching tests. cargo test runs tests in parallel
    // by default and std::env::set_var / remove_var are process-global.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn vendor_root_claude_is_default_root() {
        // ClaudeCode is the only verified root; it must equal agsess::default_root().
        assert_eq!(
            vendor_root(Vendor::ClaudeCode),
            Some(agsess::default_root())
        );
    }

    #[test]
    fn vendor_root_none_cases() {
        // Aider: per-cwd .aider.chat.history.md, no centralised home root.
        assert_eq!(vendor_root(Vendor::Aider), None, "Aider has no home root");
        // OpenCode: SQLite store, no tailable JSONL root.
        assert_eq!(
            vendor_root(Vendor::OpenCode),
            None,
            "OpenCode is SQLite-backed"
        );
        // Copilot: path is hypothetical (no real CLI JSONL today).
        assert_eq!(
            vendor_root(Vendor::Copilot),
            None,
            "Copilot root is hypothetical"
        );
    }

    #[test]
    fn vendor_root_codex_home_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("CODEX_HOME").ok();
        std::env::set_var("CODEX_HOME", "/tmp/my_codex");
        let result = vendor_root(Vendor::Codex);
        match prev {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
        // CODEX_HOME replaces ~/.codex; sessions subdir is appended.
        assert_eq!(
            result,
            Some(std::path::PathBuf::from("/tmp/my_codex/sessions"))
        );
    }

    #[test]
    fn vendor_root_atrium_gemini_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("ATRIUM_GEMINI_ROOT").ok();
        std::env::set_var("ATRIUM_GEMINI_ROOT", "/tmp/my_gemini");
        let result = vendor_root(Vendor::Gemini);
        match prev {
            Some(v) => std::env::set_var("ATRIUM_GEMINI_ROOT", v),
            None => std::env::remove_var("ATRIUM_GEMINI_ROOT"),
        }
        assert_eq!(result, Some(std::path::PathBuf::from("/tmp/my_gemini")));
    }

    // --- binding: adopt_session_for ---
    //
    // Tests go through a real agsess::World built over synthetic transcript dirs
    // because AgentSession has no public constructor.

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("atrium-adopt-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_adopt_session(root: &std::path::Path, project: &str, id: &str, lines: &[&str]) {
        use std::io::Write;
        let dir = root.join(project);
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join(format!("{id}.jsonl"))).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    /// A Claude Code user line recording `cwd` → `session.cwd = Some(cwd)` after refresh.
    fn claude_user_with_cwd(cwd: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"2026-01-01T00:00:01.000Z","cwd":"{cwd}","message":{{"role":"user","content":"hi"}}}}"#
        )
    }

    /// A Gemini user line — no `cwd` field, so `session.cwd` stays `None`.
    const GEMINI_LINE: &str =
        r#"{"role":"user","parts":[{"text":"hello"}],"timestamp":"2026-01-01T00:00:01.000Z"}"#;

    /// A second account's transcripts live under its own config dir. A world
    /// for that root (from a `claude_aliases` entry's `config_dir`) binds a
    /// session there by id; without it the same id is unknown. The default
    /// root is never added twice.
    #[test]
    fn an_alias_config_dir_gives_its_sessions_a_world() {
        let td = TempDir::new("alias-root");
        let root = td.0.join("projects");
        write_adopt_session(&root, "proj", "sess-2nd", &[&claude_user_with_cwd("/w")]);

        let mut with = VendorWorlds::with_extra_claude_roots(vec![root.clone()]);
        with.refresh();
        assert!(
            with.status_for(Some("sess-2nd")).is_some(),
            "the alias root's session binds"
        );

        let mut without = VendorWorlds::with_extra_claude_roots(Vec::new());
        without.refresh();
        assert_eq!(without.status_for(Some("sess-2nd")), None);

        let dup = VendorWorlds::with_extra_claude_roots(vec![
            root.clone(),
            root.clone(),
            agsess::default_root(),
        ]);
        let base = VendorWorlds::with_extra_claude_roots(Vec::new())
            .worlds
            .len();
        assert_eq!(
            dup.worlds.len(),
            base + 1,
            "one world per distinct extra root"
        );
    }

    #[test]
    fn adopt_no_candidates_before_launch() {
        let td = TempDir::new("no-cand");
        write_adopt_session(&td.0, "proj", "sess1", &[GEMINI_LINE]);
        let mut w = agsess::World::for_vendor(td.0.clone(), Vendor::Gemini);
        w.refresh();
        let sessions: Vec<_> = w.sessions.iter().collect();
        // launch_ms far in the future — session predates it → no candidate.
        assert_eq!(adopt_session_for(&sessions, None, u64::MAX), None);
    }

    #[test]
    fn adopt_single_candidate_no_cwd() {
        let td = TempDir::new("single");
        write_adopt_session(&td.0, "proj", "gemini-sess-1", &[GEMINI_LINE]);
        let mut w = agsess::World::for_vendor(td.0.clone(), Vendor::Gemini);
        w.refresh();
        let sessions: Vec<_> = w.sessions.iter().collect();
        // launch_ms=0 → passes time gate; no cwd filter → single candidate returned.
        let id = adopt_session_for(&sessions, None, 0).expect("one candidate");
        assert_eq!(id, "gemini-sess-1");
    }

    #[test]
    fn adopt_cwd_match_adopted() {
        let td = TempDir::new("cwd-match");
        let cwd = "/projects/my-app";
        write_adopt_session(&td.0, "proj", "claude-sess", &[&claude_user_with_cwd(cwd)]);
        let mut w = agsess::World::new(td.0.clone());
        w.refresh();
        let sessions: Vec<_> = w.sessions.iter().collect();
        let id = adopt_session_for(&sessions, Some(cwd), 0).expect("cwd match");
        assert_eq!(id, "claude-sess");
    }

    #[test]
    fn adopt_cwd_mismatch_falls_back_to_newest() {
        let td = TempDir::new("cwd-fallback");
        write_adopt_session(
            &td.0,
            "proj",
            "sess1",
            &[&claude_user_with_cwd("/projects/other")],
        );
        let mut w = agsess::World::new(td.0.clone());
        w.refresh();
        let sessions: Vec<_> = w.sessions.iter().collect();
        // pane_cwd doesn't match recorded cwd; cwd_match is empty → fall back to pool.
        let id = adopt_session_for(&sessions, Some("/projects/my-app"), 0).expect("fallback");
        assert_eq!(id, "sess1");
    }

    #[test]
    fn adopt_cwd_match_wins_over_non_matching() {
        let td = TempDir::new("prefer-cwd");
        let target = "/projects/target";
        write_adopt_session(
            &td.0,
            "proj",
            "target-sess",
            &[&claude_user_with_cwd(target)],
        );
        write_adopt_session(
            &td.0,
            "proj",
            "other-sess",
            &[&claude_user_with_cwd("/projects/other")],
        );
        let mut w = agsess::World::new(td.0.clone());
        w.refresh();
        let sessions: Vec<_> = w.sessions.iter().collect();
        // Two sessions; only target-sess matches pane_cwd → it wins.
        let id = adopt_session_for(&sessions, Some(target), 0).expect("cwd match");
        assert_eq!(id, "target-sess");
    }

    #[test]
    fn adopt_time_gate_ge_boundary() {
        let td = TempDir::new("timegate");
        write_adopt_session(&td.0, "proj", "gemini-old", &[GEMINI_LINE]);
        let mut w = agsess::World::for_vendor(td.0.clone(), Vendor::Gemini);
        w.refresh();
        let sessions: Vec<_> = w.sessions.iter().collect();
        assert!(!sessions.is_empty());
        let first_seen = sessions[0].first_seen_ms;
        // Pane launched 1 ms after first_seen → session predates → None.
        assert_eq!(adopt_session_for(&sessions, None, first_seen + 1), None);
        // Pane launched exactly at first_seen (>= boundary) → qualifies.
        assert!(adopt_session_for(&sessions, None, first_seen).is_some());
    }

    #[test]
    fn adopt_newest_chosen_when_multiple_candidates() {
        let td = TempDir::new("newest");
        write_adopt_session(&td.0, "proj", "old-sess", &[GEMINI_LINE]);
        let mut w = agsess::World::for_vendor(td.0.clone(), Vendor::Gemini);
        w.refresh(); // old-sess.first_seen_ms = now_a

        // A brief sleep ensures next refresh's now_ms() > now_a so that
        // new-sess.first_seen_ms > old-sess.first_seen_ms.
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_adopt_session(&td.0, "proj", "new-sess", &[GEMINI_LINE]);
        w.refresh(); // new-sess.first_seen_ms = now_b > now_a

        let sessions: Vec<_> = w.sessions.iter().collect();
        assert_eq!(sessions.len(), 2);
        let id = adopt_session_for(&sessions, None, 0).expect("newest chosen");
        assert_eq!(id, "new-sess");
    }

    /// A Codex rollout line (event_msg/agent_message) — no `cwd`, so the discovered
    /// session's `cwd` stays `None`, matching a pane launched with unknown cwd.
    const CODEX_LINE: &str = r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"hello"}}"#;

    /// End-to-end chain (the DoD for this feature): the exact sequence the run
    /// loop drives — a `VendorWorlds` built the way the app builds it discovers a
    /// **codex** session under its (env-overridden) root, [`adopt_session_for`]
    /// over *that vendor's* sessions yields the id to stamp on the pane, and
    /// [`VendorWorlds::status_for`] then binds it. This is what makes a codex pane
    /// light up — codex being one of the two SUPPORTED_VENDORS.
    ///
    /// Uses `CODEX_HOME` (codex's own root override → `$CODEX_HOME/sessions`) to
    /// point the codex world at a synthetic transcript dir, so it goes through
    /// `VendorWorlds::new()` — not a hand-built `World` — proving the real path.
    #[test]
    fn vendorworlds_discovers_adopts_and_binds_a_codex_pane() {
        let td = TempDir::new("e2e-codex");
        // vendor_root(Codex) == $CODEX_HOME/sessions, so write under sessions/.
        let sessions_root = td.0.join("sessions");
        write_adopt_session(&sessions_root, "proj", "rollout-live-1", &[CODEX_LINE]);

        // Capture launch just before the world first sees the file, so the
        // discovered session's `first_seen_ms >= launch_ms` (the time gate).
        let launch_ms = agsess::sessions::now_ms();

        // Point the codex world at our synthetic root exactly as an operator would
        // (CODEX_HOME is codex's own env var), then build + refresh via the same
        // API the run loop uses. Serialize with the other env tests via ENV_LOCK.
        let _lock = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("CODEX_HOME").ok();
        std::env::set_var("CODEX_HOME", &td.0);
        let mut worlds = VendorWorlds::new();
        worlds.refresh();
        match prev {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }

        // Scope to Codex sessions (what the run loop passes to adopt for a codex
        // pane) so real ~/.claude sessions on the dev box can't perturb the test.
        let codex: Vec<&agsess::AgentSession> = worlds
            .sessions()
            .into_iter()
            .filter(|s| s.vendor == Vendor::Codex)
            .collect();
        assert!(
            codex.iter().any(|s| s.id == "rollout-live-1"),
            "VendorWorlds must discover the codex session under the overridden root"
        );

        // A pane launched at `launch_ms` (cwd unknown) adopts that session…
        let id = adopt_session_for(&codex, None, launch_ms)
            .expect("a started codex pane adopts its discovered session");
        assert_eq!(id, "rollout-live-1");

        // …and once stamped, the pane binds: status_for resolves a live Status,
        // i.e. the overview node is no longer grey/unbound.
        assert!(
            worlds.status_for(Some(&id)).is_some(),
            "the adopted id must bind through VendorWorlds::status_for"
        );
        // Sanity: an unstamped pane (no id) still binds to nothing.
        assert_eq!(worlds.status_for(None), None);

        // The run loop holds a snapshot, not the worlds: it must answer exactly
        // the same, and adoption must pick the same session from it.
        let snap = worlds.snapshot();
        assert_eq!(snap.sessions().len(), worlds.sessions().len());
        assert_eq!(snap.status_for(Some(&id)), worlds.status_for(Some(&id)));
        assert_eq!(
            snap.awaiting_tool_for(Some(&id)),
            worlds.awaiting_tool_for(Some(&id))
        );
        let codex_views: Vec<&SessionView> = snap
            .sessions()
            .into_iter()
            .filter(|s| s.vendor == Vendor::Codex)
            .collect();
        assert_eq!(
            adopt_session_for(&codex_views, None, launch_ms).as_deref(),
            Some("rollout-live-1")
        );
    }

    /// The watch thread delivers a first snapshot without the loop doing any
    /// file work, and newer snapshots replace older ones.
    #[test]
    fn the_agent_watch_publishes_snapshots_off_the_loop() {
        let watch = AgentWatch::start(agsess::sessions::now_ms());
        watch.set_interval(std::time::Duration::from_millis(100));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut got = 0;
        while got < 2 && std::time::Instant::now() < deadline {
            if watch.latest().is_some() {
                got += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        assert_eq!(got, 2, "the thread must keep publishing");
    }
}

/// Is this the Microsoft Store (MSIX) build of PowerShell? codex runs an agent's
/// commands through `pwsh` when one is on PATH, and its Windows sandbox, a
/// restricted token, cannot start a packaged app: `CreateProcessAsUserW` fails
/// with `Access is denied` before the command runs, whatever the command was.
/// A packaged app lives under a `WindowsApps` directory (the per-user alias or
/// the machine store), which is the whole test. `None` (no `pwsh` on PATH)
/// is fine: codex falls back to Windows PowerShell, which the sandbox can run.
pub fn is_store_pwsh(resolved: Option<&std::path::Path>) -> bool {
    // Split on both separators rather than `Path::components`, which off
    // Windows treats a backslash as part of the name; the rule then reads the
    // same on every platform (as `bind::command_stem` does).
    resolved.is_some_and(|p| {
        p.to_string_lossy()
            .split(['\\', '/'])
            .any(|c| c.eq_ignore_ascii_case("WindowsApps"))
    })
}

#[cfg(test)]
mod store_pwsh_tests {
    use super::is_store_pwsh;
    use std::path::Path;

    #[test]
    fn only_a_pwsh_under_windowsapps_is_the_store_build() {
        for p in [
            r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\pwsh.exe",
            r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\Microsoft.PowerShell_8wekyb3d8bbwe\pwsh.exe",
            r"C:\Program Files\WindowsApps\Microsoft.PowerShell_7.5.0.0_x64__8wekyb3d8bbwe\pwsh.exe",
            r"c:\users\me\appdata\local\microsoft\windowsapps\pwsh.exe",
        ] {
            assert!(is_store_pwsh(Some(Path::new(p))), "{p}");
        }
        for p in [
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            r"D:\tools\pwsh\pwsh.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        ] {
            assert!(!is_store_pwsh(Some(Path::new(p))), "{p}");
        }
        assert!(!is_store_pwsh(None));
    }
}
