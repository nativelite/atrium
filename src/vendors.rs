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
        let worlds = SUPPORTED_VENDORS
            .iter()
            .filter_map(|&vendor| vendor_root(vendor).map(|root| World::for_vendor(root, vendor)))
            .collect();
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
pub fn adopt_session_for(
    sessions: &[&agsess::AgentSession],
    pane_cwd: Option<&str>,
    launch_ms: u64,
) -> Option<String> {
    // Phase 1: time gate — transcripts first seen before this pane launched
    // belong to prior runs of the same vendor, not to this pane.
    let candidates: Vec<&agsess::AgentSession> = sessions
        .iter()
        .copied()
        .filter(|s| s.first_seen_ms >= launch_ms)
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Phase 2: cwd preference — narrow to sessions whose transcript-recorded
    // cwd matches the pane's working directory. Vendors that do not write cwd
    // to their transcripts leave session.cwd == None; those produce an empty
    // cwd_match set and fall through to the full candidate pool.
    let pool: Vec<&agsess::AgentSession> = if let Some(cwd) = pane_cwd {
        let cwd_match: Vec<&agsess::AgentSession> = candidates
            .iter()
            .copied()
            .filter(|s| s.cwd.as_deref() == Some(cwd))
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
        .max_by_key(|s| s.first_seen_ms)
        .map(|s| s.id.clone())
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
    }
}
