//! The atrium binary: a dual-mode run loop — passthrough (0.1) and tiled (0.2).
//!
//!     atrium                      # panes run your shell (COMSPEC / $SHELL)
//!     atrium claude               # panes run `claude`
//!     atrium claude --continue    # any command + args
//!
//! Ctrl+A then: c new window, n/p cycle, 1-9 switch windows, x kill focused
//! pane, q quit; `"`/`%` split the focused pane, h/j/k/l or arrows move focus,
//! z zoom (full-screen passthrough) the focused pane.
//!
//! ## The two rendering modes
//!
//! A window whose split tree is a single pane — or any window with its focused
//! pane *zoomed* — renders in **passthrough**: the pane's raw VT bytes go
//! straight to the real terminal, zero emulation, perfect fidelity. This is the
//! 0.1 path, untouched.
//!
//! The moment a window holds two or more visible panes it renders **tiled**:
//! each pane drives a `vterm::Term` sized to its rect, every pane's screen is
//! composited into one master `ansi::Screen`, and `Screen::diff` writes only the
//! changed bytes. Zoom is the escape hatch back to perfect fidelity for a TUI
//! that the emulator can't render pixel-exact (wide glyphs, sixel, mouse).

use atrium::bar::{bar_paint, PaneInfo};
use atrium::input::{Action, Dir, PrefixScanner};
use atrium::layout::{self, Tree};
use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

mod fleet_cli;
mod overview;
mod panels;
mod tiled;
pub(crate) use fleet_cli::*;
pub(crate) use overview::*;
pub(crate) use panels::*;
pub(crate) use tiled::*;

/// A process-global, monotonic **agent id** stamped on every pane atrium hosts —
/// the stable key of the ctl spawn tree (`Pane.id` is only unique within a
/// window; this is unique across the whole run). Incremented once per spawn.
static NEXT_AGENT: AtomicUsize = AtomicUsize::new(0);

/// The ctl channel address, set once at startup iff `--allow-ctl` was given.
/// The spawn path reads it to inject `ATRIUM_CTL`/`ATRIUM_PANE` into each pane so an
/// agent inside can drive `atrium ctl`. `None` (unset) ⇒ ctl is off and every
/// spawn is byte-identical to pre-ctl atrium.
static CTL_ADDRESS: OnceLock<String> = OnceLock::new();
/// Set when the ancestry cap lowered this session's posture. The cap runs in
/// `main`, before the bus exists, so the notice is parked here and raised as a
/// decision on the first loop tick - an agent quietly launching its own atrium is
/// something the operator should be asked about, not just told once on stderr.
static CAP_NOTICE: OnceLock<String> = OnceLock::new();
/// How often the warden re-reads what it snapshotted. Slow on purpose: a
/// tripwire that costs the event loop is a tripwire that gets removed.
const WARDEN_INTERVAL: Duration = Duration::from_secs(3);

/// Bind the per-process ctl endpoint and publish its address to [`CTL_ADDRESS`]
/// so every pane spawned afterward is born with `ATRIUM_CTL`/`ATRIUM_PANE` in its
/// env. Returns the [`atrium::ipc::Listener`] to poll, or `None` on a bind failure
/// (surfaced in the bar, non-fatal). `CTL_ADDRESS` is a `OnceLock`, so only the
/// first successful bind per process publishes the address.
fn bind_ctl(flash: &mut Option<(String, Instant)>) -> Option<atrium::ipc::Listener> {
    let addr = atrium::ipc::default_address();
    match atrium::ipc::Listener::bind(&addr) {
        Ok(l) => {
            let _ = CTL_ADDRESS.set(addr);
            Some(l)
        }
        Err(e) => {
            if flash.is_none() {
                *flash = Some((format!("ctl channel disabled: {e}"), Instant::now()));
            }
            None
        }
    }
}

/// Set once at startup from the `--trust` / `--skip-permissions` flags: how much
/// atrium relaxes the permission posture of every agent pane it spawns. Encoded as
/// `0=Off`, `1=Edits` (`--trust`: acceptEdits + safe allowlist), `2=Skip`
/// (`--skip-permissions`: full bypass). Read by the spawn path
/// (`spawn_pane_full`) via [`trust_mode`].
static AGENT_TRUST: AtomicU8 = AtomicU8::new(0);

/// Decode [`AGENT_TRUST`] into the typed mode.
fn trust_mode() -> atrium::ctl::TrustMode {
    match AGENT_TRUST.load(Ordering::Relaxed) {
        1 => atrium::ctl::TrustMode::Edits,
        2 => atrium::ctl::TrustMode::Skip,
        3 => atrium::ctl::TrustMode::Plan,
        4 => atrium::ctl::TrustMode::Auto,
        _ => atrium::ctl::TrustMode::Off,
    }
}

/// Publish the launch trust mode to the spawn path.
fn set_trust_mode(m: atrium::ctl::TrustMode) {
    let code = match m {
        atrium::ctl::TrustMode::Off => 0,
        atrium::ctl::TrustMode::Edits => 1,
        atrium::ctl::TrustMode::Skip => 2,
        atrium::ctl::TrustMode::Plan => 3,
        atrium::ctl::TrustMode::Auto => 4,
    };
    AGENT_TRUST.store(code, Ordering::Relaxed);
}

fn next_agent_id() -> usize {
    NEXT_AGENT.fetch_add(1, Ordering::Relaxed)
}

/// DEC synchronized-output (private mode 2026). atrium wraps each composited frame
/// it emits to the *real* terminal in these markers, so the outer terminal paints
/// the whole frame (all tiled panes + the bar) atomically instead of showing it
/// half-drawn — that half-drawn frame is the tiled "shutter". This is the emit
/// side of the same mode `vterm` honors on the way in; a terminal that doesn't
/// support 2026 ignores the markers, so it degrades cleanly.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Max reads per active pane per tick (each up to the 8 KiB `buf`). High enough
/// to drain a whole frame from a big burst before compositing (~0.5 MiB), so we
/// never composite a half-drawn pane; the drain still breaks early the instant
/// the pane has no more data, so this cap only matters on a genuine flood.
const DRAIN_READS_PER_TICK: usize = 64;

/// Draw the animated startup splash for a passthrough pane that has not painted
/// yet — a centered `a t r i u m` wordmark and a spinner, so the agent's boot reads
/// as *loading*, not a hang. Written straight to the terminal and wrapped in
/// synchronized output so each frame is atomic (no flicker from the per-frame
/// clear); the drain wipes it on the pane's first real bytes.
/// True while a pane's emulator has produced no *visible* content yet — every
/// cell is default. Used to keep the startup splash up until the agent actually
/// paints (its terminal-setup bytes arrive first and must not count as painted).
/// Short-circuits, so it is cheap and only fully scans during the brief boot.
fn term_blank(t: &vterm::Term) -> bool {
    let s = t.screen();
    let blank = ansi::Cell::default();
    for r in 0..s.rows() {
        for c in 0..s.cols() {
            if s.cell(r, c) != blank {
                return false;
            }
        }
    }
    true
}

/// True if `bytes` contains a full-screen erase (`ESC[2J` or `ESC[3J`). Unlike a
/// scroll, `2J`/`3J` ignore the scroll region and wipe the whole screen — the bar
/// row included — so atrium must repaint the bar after one.
fn clears_screen(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|w| w == b"\x1b[2J" || w == b"\x1b[3J")
}

/// Sniff a pty output chunk for the app turning mouse tracking on/off — a
/// DECSET/DECRST for private mode 1000/1002/1003 (`ESC [ ? … h` / `… l`). Returns
/// `Some(true)` if the chunk last enabled mouse, `Some(false)` if it last
/// disabled it, `None` if it touched neither. Handles combined params
/// (`ESC[?1002;1006h`). Best-effort and stateless: a sequence split across two
/// reads may be missed, but apps emit these as a single write at startup/teardown
/// so it reliably tracks whether a pane wants the wheel. Drives `Pane::mouse_wanted`.
fn sniff_mouse_mode(buf: &[u8]) -> Option<bool> {
    let mut result = None;
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let start = i + 3;
            let mut j = start;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j < buf.len() && (buf[j] == b'h' || buf[j] == b'l') {
                let is_mouse = std::str::from_utf8(&buf[start..j])
                    .ok()
                    .map(|s| s.split(';').any(|p| matches!(p, "1000" | "1002" | "1003")))
                    .unwrap_or(false);
                if is_mouse {
                    result = Some(buf[j] == b'h');
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    result
}

/// A `ctl send` awaiting delivery. The design queues a task until the target is
/// **idle** (agsess-gated) rather than injecting into a live turn (Decision 4).
/// Once the target is ready we write the text, then — after a short beat so the
/// agent's TUI registers the line before the submit — write the Enter. The beat
/// mirrors the C0 spike, which split the text and `\r` with a delay.
struct PendingSend {
    /// Target pane's global agent id (already resolved + scope-checked).
    target: usize,
    text: String,
    /// When the send was accepted — a fallback so a target that never yields a
    /// derivable status (a shell, a not-yet-bound agent) still gets it.
    queued_at: Instant,
    /// How many bytes of `text` the target's pty has actually accepted.
    ///
    /// `write(2)` returns a COUNT and may legitimately take less than offered —
    /// a pty's input buffer is finite, and in canonical mode a single line is
    /// capped near 1 KB. The old code discarded that count, marked the send
    /// delivered, and submitted Enter regardless, so a long task arrived as a
    /// fragment while `ctl` had already replied `ok:true`.
    written: usize,
    /// `Some(t)` once the text has been written IN FULL; `t` gates the follow-up
    /// Enter. Never set on a partial write — Enter must not submit a fragment.
    text_written_at: Option<Instant>,
}

/// How long after writing the task text we send the Enter that submits it.
const SEND_ENTER_DELAY: Duration = Duration::from_millis(400);
/// If a target never yields a derivable agsess status (non-agent / unbound),
/// deliver anyway once the send has waited this long, so a queue never wedges.
const SEND_UNBOUND_FALLBACK: Duration = Duration::from_secs(2);

/// Max height (rows) of a spurious "shrink" atrium ignores in the resize poll: the
/// Windows Terminal ConPTY reports a few rows fewer once atrium is on the alternate
/// screen, and adopting it leaves a strip of stale content below the bar. A
/// height-only shrink no larger than this is treated as that reservation, not a
/// real resize. Generous enough to cover the observed reserve, small enough that a
/// genuine window resize (usually larger, and changing width) is still honored.
const ALT_SCREEN_RESERVE_ROWS: u16 = 8;

/// Find a hosted pane by its global agent id (immutable / mutable).
fn pane_by_agent(windows: &[Window], id: usize) -> Option<&Pane> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .find(|p| p.agent_id == id)
}
fn pane_by_agent_mut(windows: &mut [Window], id: usize) -> Option<&mut Pane> {
    windows
        .iter_mut()
        .flat_map(|w| w.panes.iter_mut())
        .find(|p| p.agent_id == id)
}

/// `(agent_id, role)` for every live pane — the candidate set for target
/// resolution.
fn ctl_candidates(windows: &[Window]) -> Vec<(usize, Option<String>)> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| (p.agent_id, p.role.clone()))
        .collect()
}

/// `(agent_id, parent)` for every live pane — the spawn-tree edges the subtree
/// guard walks.
fn ctl_parents(windows: &[Window]) -> Vec<(usize, Option<usize>)> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| (p.agent_id, p.parent))
        .collect()
}

/// The **human** controls everything (Decision 3). A caller is human-privileged
/// when it has no attributed pane, or when its pane is a *root* (one atrium opened,
/// `parent == None`, depth 0) — i.e. where the operator sits. A spawned worker
/// (depth > 0) is scoped to its own subtree.
fn caller_privileged(windows: &[Window], caller: Option<usize>) -> bool {
    // Look the caller up, then decide in a pure function so the DECISION can be
    // tested without constructing panes (each of which needs a live pty). This
    // gate had no tests at all: when its two `None` arms were flipped from
    // `true` to `false` — a security fix — nothing in the suite changed, which is
    // precisely the problem.
    let pane_parent = caller
        .and_then(|id| pane_by_agent(windows, id))
        .map(|p| p.parent);
    privilege_for(caller, pane_parent)
}

/// Lower `trust` to the ceiling of the atrium session this one is nested under.
///
/// Used by EVERY entry point that starts a session. It first lived inline in the
/// single-pane path, which meant `atrium fleet up X --trust skip` sailed straight
/// past it: `fleet` is dispatched before that code ever runs. An agent in a pane
/// could escape the ceiling by launching a fleet instead of a pane - the ceiling
/// held for one door and not the other.
fn cap_trust_to_ancestor(mut trust: atrium::ctl::TrustMode) -> atrium::ctl::TrustMode {
    // A pane cannot escape its session's posture by launching its own atrium.
    //
    // The ceiling hangs off process ANCESTRY, not the environment. An agent owns
    // its environment, so an `ATRIUM_AGENT=1` marker or an inherited policy var
    // dies to `env -u`; and it owns a spawned child's stdin, so it can answer the
    // skip confirmation just below on its own behalf. It cannot unset its own
    // parent. A double-fork still erases the trail - that case belongs to the
    // warden, which watches for sessions the cap could not account for.
    match atrium::warden::atrium_ancestor() {
        atrium::warden::Ancestry::Atrium(parent) => {
            let inherited = atrium::reap::read_policy(&atrium::reap::registry_path(parent))
                .and_then(|p| atrium::ctl::TrustMode::from_policy_keyword(&p));
            // An UNREADABLE policy is not an absent ceiling. We know from the
            // kernel that we are nested; if the parent's registry has been deleted
            // or corrupted we still must not exceed a ceiling we cannot see. This
            // used to fall through with no cap at all, so `rm
            // /tmp/atrium-session-<parent>.pids` - one command, no double-fork, from
            // a pane running as the same uid - turned the ceiling off. Plan is the
            // safest posture that still leaves the session usable, and the warden
            // reports the deletion separately.
            let (ceiling, why) = match inherited {
                Some(c) => (c, "whose session policy is"),
                None => (
                    atrium::ctl::TrustMode::Plan,
                    "whose session policy could not be read, so the safe floor is",
                ),
            };
            if trust.rank() > ceiling.rank() {
                eprintln!(
                    "atrium: capped to {} - running inside another atrium (pid {}) {} {}. \
                     A pane cannot raise its own posture; set it at the outer launch.",
                    ceiling.policy_label(),
                    parent,
                    why,
                    ceiling.policy_label()
                );
                let _ = CAP_NOTICE.set(format!(
                    "a pane launched its own atrium (pid {}) and was capped to {}",
                    parent,
                    ceiling.policy_label()
                ));
                trust = ceiling;
            }
        }
        // The walk did not complete: no process table, or a hop whose executable
        // could not be identified. We cannot invent a ceiling - there may be no
        // parent at all, and capping every launch on an unreadable table would
        // break atrium in a bare terminal on any host where the probe fails. So the
        // posture is left alone and the operator is told, which is the difference
        // between "cannot read a known parent's policy" (cap, above) and "cannot
        // tell whether a parent exists" (report, here).
        atrium::warden::Ancestry::Unknown => {
            eprintln!(
                "atrium: could not determine whether this session is nested inside \
                 another atrium; no ceiling was applied."
            );
            let _ = CAP_NOTICE.set(
                "could not determine whether this session is nested inside another \
                 atrium, so no ceiling was applied"
                    .to_string(),
            );
        }
        // A top-level session, or a platform with no implementation (Windows -
        // see `warden::atrium_ancestor`, where that gap is written down).
        atrium::warden::Ancestry::NoneFound | atrium::warden::Ancestry::Unsupported => {}
    }
    trust
}

/// Resolve a spawn's permission mode against the session policy.
///
/// The session policy is a CEILING, and it now holds for everyone — including a
/// root pane. It used to be bypassed outright for a "privileged" caller
/// (`Some(req) if privileged => req`), on the reading that the root pane is the
/// human and the human may elevate. Two things break that:
///
/// 1. `-n N` and `--grid` give every pane no parent, so EVERY pane in a
///    mass-spawned session was classified as the operator. An agent could then
///    ask for `skip` — a full permission bypass — in a session the human had
///    deliberately set to `plan`.
/// 2. atrium's own documentation calls `--trust` "the mode spawned agents run in,
///    and the ceiling they are capped at". A ceiling that a pane can exceed is
///    not a ceiling, and the human cannot tell from the outside which panes could.
///
/// So the bypass is gone. `--mode` may still DE-ESCALATE freely (asking for less
/// than the policy is always allowed), which is the useful half; it can no longer
/// escalate. To run agents at a higher posture, the human sets it at launch,
/// where it is a visible, deliberate choice rather than something a pane can
/// request. Returns the effective mode and a note when a request was capped.
fn effective_mode(
    requested: Option<atrium::ctl::TrustMode>,
    policy: atrium::ctl::TrustMode,
) -> (atrium::ctl::TrustMode, Option<String>) {
    match requested {
        None => (policy, None),
        Some(req) if req.rank() <= policy.rank() => (req, None),
        Some(req) => (
            policy,
            Some(format!(
                "capped to {} (session policy); nothing may elevate itself to {} \
                 — set the policy at launch with --trust",
                policy.policy_label(),
                req.policy_label()
            )),
        ),
    }
}

/// The privilege decision, given only what it needs.
///
/// `pane_parent` says what the caller's capability token resolved to:
/// - `None` — no such live pane (a stale or forged token), or no caller at all
/// - `Some(None)` — a live pane with no parent: a root pane the human opened
/// - `Some(Some(_))` — a live pane spawned by another: a worker
fn privilege_for(caller: Option<usize>, pane_parent: Option<Option<usize>>) -> bool {
    match caller {
        // No authenticated caller is NOT the operator. This arm used to return
        // `true`, which inverted the gate: holding no credential granted strictly
        // more than holding a worker's, so `env -u ATRIUM_TOKEN atrium ctl ...`
        // promoted you. `caller` is derived from the capability token, never
        // self-reported, so `None` means exactly "unauthenticated" and must be
        // the least trusted state, not the most.
        None => false,
        Some(_) => match pane_parent {
            Some(parent) => parent.is_none(),
            // A token resolving to no live pane is stale or forged, not the
            // operator. Same inversion as above.
            None => false,
        },
    }
}

/// One hosted terminal: a pty, its emulator (for tiled compositing), its
/// passthrough filter (for the passthrough / zoom path), and bar metadata. Each
/// pane has a stable `id` the window's split tree refers to.
pub(crate) struct Pane {
    pub(crate) id: usize,
    pub(crate) pty: pty::Pty,
    pub(crate) term: vterm::Term,
    pub(crate) filter: atrium::filter::Passthrough,
    pub(crate) title: String,
    pub(crate) activity: bool,
    pub(crate) exited: bool,
    /// The session id atrium injected via `--session-id` when this pane is an
    /// agent it launched (§3.3). `None` for shells and agents atrium did not
    /// bind. The binder maps this to the pane's live `agsess::Status`.
    pub(crate) session_id: Option<String>,
    /// When atrium launched this pane (epoch ms, agsess clock). Used to *adopt* a
    /// session for a non-claude agent pane: atrium cannot hand it a `--session-id`,
    /// so after launch it associates the pane with the newest session discovered
    /// under the vendor's root *at or after* this instant (see
    /// [`atrium::vendors::adopt_session_for`]).
    pub(crate) launch_ms: u64,
    /// The working directory this pane's agent was launched in, if known. A tie-
    /// breaker for session adoption (prefer a discovered session whose `cwd`
    /// matches). `None` for panes opened in atrium's own cwd.
    pub(crate) cwd: Option<String>,
    /// The credential identity **name** this pane's agent runs under, if any
    /// (path B). Only the name is stored — never the resolved secret. On every
    /// spawn atrium re-resolves the env via `akey` and injects it for that child
    /// alone; the resolved values live only for the spawn call and are dropped
    /// immediately. Surfaced in the chrome as a `·<name>` tag.
    pub(crate) identity: Option<String>,
    /// Process-global agent id (see [`NEXT_AGENT`]): the ctl spawn-tree key,
    /// stable across windows. Injected into the pane as `ATRIUM_PANE` so an agent
    /// inside can attribute its own `ctl spawn` calls.
    pub(crate) agent_id: usize,
    /// The pane's **capability token** — an unguessable secret injected into its
    /// env as `ATRIUM_TOKEN` and matched by the ctl server to authenticate requests
    /// from this pane (identity comes from the token, not the self-reported
    /// `ATRIUM_PANE`). Empty for panes spawned before the ctl endpoint existed.
    /// The pane's capability token, or `None` when OS entropy was unavailable at
    /// spawn and atrium refused to mint a guessable one. `None` must never
    /// authenticate: a pane without a token has no ctl access, by construction.
    pub(crate) token: Option<String>,
    /// The ctl role label this pane was spawned under (`dev_1`), if any. `None`
    /// for the human's own panes and shells.
    pub(crate) role: Option<String>,
    /// The agent id of the pane whose `ctl spawn` created this one. `None` for a
    /// root pane the human opened.
    pub(crate) parent: Option<usize>,
    /// Depth in the spawn tree: 0 for a root/human pane, parent.depth + 1 for a
    /// ctl-spawned worker. The `--max-depth` recursion guard is checked against
    /// this.
    pub(crate) depth: usize,
    /// May this pane create teammates with `atrium ctl spawn`?
    ///
    /// A capability, not an inference. It used to be implied by having ctl access
    /// at all, so every agent in a fleet could spawn - in a seven-agent review
    /// fleet, all seven, when only the lead should. A fleet declares it per agent
    /// (default FALSE); a pane the human opened directly keeps the old behaviour,
    /// because there the human IS the caller.
    ///
    /// Checked before the depth cap and the trust ceiling, not instead of them.
    pub(crate) can_spawn: bool,
    /// True once the pane's process has produced any output. Until then, a
    /// *passthrough* (single/zoomed) pane shows the animated startup splash
    /// instead of a blank screen (the tiled path uses a per-pane blank check in
    /// the compositor). Set on the pane's first byte in the drain.
    pub(crate) painted: bool,
    /// True while this pane's app has mouse tracking enabled (it emitted a
    /// DECSET 1000/1002/1003), sniffed from its output. Scroll-wheel notches are
    /// only forwarded to a pane that wants mouse — so hovering a claude tile
    /// scrolls it, while a bare shell never receives stray mouse bytes.
    pub(crate) mouse_wanted: bool,
}

/// One window: a split tree over a set of panes, plus a zoom flag. Windows are
/// the 0.1 "switchable full-screen" concept; each can now itself be a tiled
/// split tree (design doc §3: "windows and splits coexist").
pub(crate) struct Window {
    pub(crate) panes: Vec<Pane>,
    pub(crate) tree: Tree,
    pub(crate) zoomed: bool,
    pub(crate) next_id: usize,
}

impl Window {
    pub(crate) fn pane(&self, id: usize) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    pub(crate) fn pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    pub(crate) fn focused_mut(&mut self) -> Option<&mut Pane> {
        let f = self.tree.focus();
        self.pane_mut(f)
    }

    /// Tiled iff more than one pane and not zoomed. A single pane, or a zoomed
    /// pane, is passthrough.
    pub(crate) fn tiled(&self) -> bool {
        self.panes.len() > 1 && !self.zoomed
    }
}

fn main() -> ExitCode {
    // Before anything else: a closed terminal window (SIGHUP) must reach the
    // event loop's normal exit, not kill atrium where it stands and orphan every
    // hosted agent onto `init`. Installed here so every path — single pane,
    // mass-spawn, `fleet up` — is covered.
    atrium::signals::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Watchdog mode: a re-exec of atrium that outlives this session and cleans up
    // if it dies in a way no handler can catch (SIGKILL, panic, OOM). Dispatched
    // before anything else — it must never touch the terminal.
    if args.first().map(String::as_str) == Some(atrium::reap::WATCHDOG_FLAG) {
        let parent: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let registry = args.get(2).map(std::path::PathBuf::from);
        // The owner's session key, optional so an argv from an older build still
        // starts a watchdog (it just has nothing but the registry to go on).
        let session = args
            .get(3)
            .and_then(|s| atrium::orphan::SessionKey::parse(s));
        match (parent, registry) {
            (0, _) | (_, None) => return ExitCode::FAILURE,
            (parent, Some(reg)) => {
                atrium::reap::ignore_terminal_signals();
                atrium::reap::watchdog_main(parent, &reg, session);
                return ExitCode::SUCCESS;
            }
        }
    }
    // `atrium reap` cleans up after sessions that are already gone — a crash, or a
    // session from before any of this existed.
    if args.first().map(String::as_str) == Some("reap") {
        let (sessions, watchdogs, orphans) = atrium::reap::reap_stale();
        // Never silent about a kill: name every process group collected, and say
        // which owner it was orphaned by. An operator who runs this after losing
        // a machine's pty pool needs to see what it actually did.
        for v in &orphans {
            println!("atrium reap: killed orphaned pane group {v}");
        }
        match (sessions, watchdogs, orphans.len()) {
            (0, 0, 0) => println!("atrium reap: nothing to clean up"),
            (s, w, o) => {
                let mut parts = Vec::new();
                if s > 0 {
                    parts.push(format!("{s} dead session(s)"));
                }
                if w > 0 {
                    parts.push(format!("{w} stuck watchdog(s)"));
                }
                if o > 0 {
                    parts.push(format!("{o} orphaned pane group(s)"));
                }
                println!("atrium reap: cleaned up {}", parts.join(", "));
            }
        }
        return ExitCode::SUCCESS;
    }
    // `atrium fleet …` is its own command family (a saved roster of agents), not a
    // hosted program — dispatch it before any of atrium's flag parsing so `fleet`
    // and its subcommands are never mistaken for a command to host.
    if args.first().map(String::as_str) == Some("fleet") {
        return fleet_cmd(&args[1..]);
    }
    // `atrium ctl …` is the control-channel client: it connects to the running
    // atrium's `ATRIUM_CTL` endpoint, so it is a command family, not a hosted
    // program — dispatch it before flag parsing too.
    if args.first().map(String::as_str) == Some("ctl") {
        return atrium::ctl::ctl_cmd(&args[1..]);
    }
    // atrium's own `--identity <name>` / `-I <name>` is stripped off the front,
    // before the hosted command begins; it tags the initial agent pane and is
    // inherited by every split/new pane (stored, re-resolved per spawn). Only
    // the NAME is threaded through — never the resolved secret. We parse it
    // *first* so atrium's own meta-flags (`--help`, `--stdin-probe`) are still
    // recognized when they follow an identity (`atrium --identity work --help`).
    let (identity, rest) = atrium::identity::parse(&args);
    // `--reap-orphans`: sweep this machine for pane groups whose atrium is gone,
    // BEFORE taking the terminal, and print every one collected.
    //
    // Opt-in for this first release, deliberately. The sweep kills process
    // groups on evidence gathered from the machine, and the fail-safe direction
    // is already "do not kill" (see `atrium::orphan`), but a launch flag is not
    // where a wrong rule should first meet a user's machine. It also must never
    // be silent: a startup path that kills things without saying so is how the
    // last teardown bug stayed invisible for so long.
    let (reap_orphans, rest) = atrium::orphan::parse_flag(&rest);
    if reap_orphans {
        for v in atrium::orphan::sweep(None) {
            eprintln!("atrium: reaped orphaned pane group {v}");
        }
    }
    // `--version` / `-V`: the first thing anyone types when filing a bug, and
    // the last thing a release wants missing. It answers BEFORE the terminal is
    // taken — an unrecognized flag is otherwise treated as the command to host,
    // so `atrium --version` used to die on "stdin is not a terminal" when piped.
    if matches!(
        rest.first().map(String::as_str),
        Some("--version") | Some("-V")
    ) {
        println!("atrium {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if rest.first().map(String::as_str) == Some("--help") {
        eprintln!(
            "usage: atrium [--identity <name>] [--reap-orphans] [--allow-ctl [--max-depth <N>]] [--trust [plan|accept|automode|skip] | --skip-permissions] [-n <N> | --grid <R>x<C>] [command [args...]]\n\
             \x20      --reap-orphans: before starting, kill pane process groups whose atrium is gone (prints each\n\
             \x20               one). Off by default; `atrium reap` does the same thing on its own.\n\
             \x20      --trust <policy>: the session trust policy — the mode spawned agents run in, and the\n\
             \x20               ceiling they are capped at (low→high: plan < accept < automode < skip):\n\
             \x20               `plan` read-only plan mode; `accept` (bare --trust) auto-accept edits + a safe\n\
             \x20               dev-command allowlist (build/test/run), anything else (curl, git push, rm outside\n\
             \x20               the dir) still prompts, visibly; `automode` claude's auto mode (hands-off edits +\n\
             \x20               commands with claude's guardrails); `skip` FULL bypass (--dangerously-skip-\n\
             \x20               permissions, no gate — atrium confirms it at launch). All pre-accept claude's\n\
             \x20               folder-trust dialog. Extend the accept allowlist with ATRIUM_TRUST_ALLOW=\"a,b\".\n\
             \x20               The policy is a CEILING for every caller: `ctl spawn --mode …` may match\n\
             \x20               it or de-escalate, never elevate. Raise it at launch, not mid-session.\n\
             \x20      --skip-permissions: alias for --trust skip.\n\
             \x20      --version, -V: print the version and exit.\n\
             \x20      atrium ctl spawn [--role R] [--identity X] [--here] [--mode plan|accept|automode|skip] -- <cmd...> | list | send <target> <text> | status [target] | kill <target> | audit [N]\n\
             \x20      (ATRIUM_CTL_AUDIT=<file> mirrors the ctl audit log to JSONL)\n\
             \x20      (mouse capture is OFF by default so text selection works; Ctrl+A m turns it on: click focuses a pane, wheel scrolls the hovered tile)"
        );
        return ExitCode::SUCCESS;
    }
    if rest.first().map(String::as_str) == Some("--stdin-probe") {
        return stdin_probe();
    }
    // atrium's own launch meta-flags (`--allow-ctl` / `--max-depth <N>` / `--trust`)
    // are stripped next — after `--identity`, before mass-spawn flags and the
    // hosted command. A bad `--max-depth` is a startup error, never a silent
    // fallback.
    let (allow_ctl, max_depth, mut trust, rest) = match atrium::ctl::parse_flags(&rest) {
        Ok(quad) => quad,
        Err(msg) => {
            eprintln!("atrium: {msg}");
            return ExitCode::FAILURE;
        }
    };
    // atrium's own mass-spawn flags (`-n <N>` / `--grid <R>x<C>`) are stripped off
    // the front, after `--identity`, before the hosted command. A bad value is a
    // startup error, surfaced on stderr — never a silent fallback to one pane.
    let (grid, rest) = match atrium::spawn::parse(&rest) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("atrium: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let command: Vec<String> = if rest.is_empty() {
        vec![default_shell()]
    } else {
        rest
    };
    // `--skip-permissions` is full bypass — a conscious, dangerous choice. Make
    // the human confirm it once, in plain terms, *before* the TUI takes the
    // terminal (this reads stdin normally; the run loop takes raw mode after).
    trust = cap_trust_to_ancestor(trust);
    if trust == atrium::ctl::TrustMode::Skip && !confirm_skip_permissions() {
        eprintln!("atrium: aborted (use --trust for safe hands-off: edits + a dev allowlist, dangerous commands still prompt).");
        return ExitCode::SUCCESS;
    }
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("atrium: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    run(
        &mut term,
        &command,
        identity.as_deref(),
        grid,
        None,
        allow_ctl,
        max_depth,
        trust,
        None,
    )
}

/// Wait for the operator to acknowledge a fleet's banner before the TUI takes the
/// screen. Returns false if they decline.
///
/// Non-interactive callers (no tty on stdin, or `ATRIUM_YES=1`) proceed without
/// asking - there is nobody to answer, and blocking a scripted launch forever is
/// worse than not confirming it. The banner is still printed either way, so a log
/// records what was granted.
fn fleet_ack() -> bool {
    if std::env::var_os("ATRIUM_YES").is_some() {
        return true;
    }
    // SAFETY: isatty on a borrowed fd, no ownership taken.
    #[cfg(unix)]
    {
        extern "C" {
            fn isatty(fd: i32) -> i32;
        }
        if unsafe { isatty(0) } == 0 {
            return true;
        }
    }
    eprint!("atrium fleet: press Enter to start, or Ctrl+C to abort... ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    // A read error (closed stdin) is not consent, but it is also not a reason to
    // hang: treat it as the non-interactive case we already allow.
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line).is_ok()
}

/// Plain-English confirmation gate for `--skip-permissions` (full bypass). Prints
/// what it does and the risk, then reads one line from stdin: only an explicit
/// `y`/`yes` proceeds. Any other input, EOF, or a non-interactive stdin aborts,
/// so the dangerous mode is never entered by accident. Runs before raw mode.
fn confirm_skip_permissions() -> bool {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "\n\x1b[1;31mWARNING: --skip-permissions runs EVERY command with no gate.\x1b[0m\n\
         Each agent pane (and every teammate it spawns) can run any command — delete\n\
         files, git push, network calls — with no approval prompt, on this machine with\n\
         your permissions. Use it only where damage is easily undone (a sandbox/VM).\n\
         For safe hands-off instead, quit and use --trust (edits + a dev allowlist;\n\
         dangerous commands still prompt, visibly)."
    );
    let _ = write!(err, "Proceed in full-bypass mode? [y/N] ");
    let _ = err.flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(n) if n > 0 => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        _ => false, // EOF / non-interactive → do not enter the dangerous mode
    }
}

// The run loop threads several independent, well-named launch parameters; a
// bag struct would obscure more than it helps here.
#[allow(clippy::too_many_arguments)]
fn run(
    term: &mut rawterm::Terminal,
    command: &[String],
    identity: Option<&str>,
    grid: Option<atrium::spawn::Grid>,
    // A pre-built initial window (the fleet loader spawns its own panes, one per
    // agent, each with its own identity/cwd). When `Some`, it is used verbatim
    // as the first window and `command`/`grid` are ignored for it — but they
    // still drive later `Ctrl+A c` new panes and splits (which host `command`
    // under `identity`), so a fleet's new panes open a shell as a scratch pane.
    initial_window: Option<Window>,
    // ctl control plane (§ atrium-ctl-control-plane): when `allow_ctl`, atrium binds
    // a per-process control endpoint and injects its address into every pane, so
    // an agent inside a pane can `atrium ctl spawn/list`. `max_depth` is the
    // recursion guard (usize::MAX == unlimited). Off ⇒ pre-ctl behavior verbatim.
    allow_ctl: bool,
    max_depth: usize,
    // How atrium relaxes each spawned agent's permissions: `Off`, `Edits`
    // (`--trust`: acceptEdits + safe allowlist), or `Skip` (`--skip-permissions`:
    // full bypass). Published to the spawn path via `AGENT_TRUST` before the
    // first spawn.
    trust: atrium::ctl::TrustMode,
    // A pre-bound ctl endpoint. The fleet path binds it *before* spawning its
    // panes (so they get `ATRIUM_CTL` in their env) and hands the listener here;
    // `None` means run() binds its own after the initial spawn (the default path).
    mut ctl_listener: Option<atrium::ipc::Listener>,
) -> ExitCode {
    // Publish the trust policy before any pane is spawned so even the initial
    // agent picks it up.
    set_trust_mode(trust);
    let mut out = std::io::stdout();
    let (mut rows, mut cols) = term.size().unwrap_or((24, 80));
    // Alt screen; scroll region above the bar so bottom-line newlines from the
    // passthrough stream can never push the bar away.
    let _ = write!(out, "\x1b[?1049h\x1b[2J\x1b[H\x1b[1;{}r", rows - 1);
    let _ = out.flush();

    let mut windows: Vec<Window> = Vec::new();
    // The initial pane can fail to spawn (command missing) *or* fail to resolve
    // its identity (no such target, vault locked). A spawn failure aborts atrium
    // (there is nothing to host); an identity-resolve failure is surfaced in the
    // bar and the pane spawns *without* the credential — never silently, and
    // never unauthenticated-without-saying-so (§7).
    let mut flash: Option<(String, Instant)> = None;
    // ctl control channel (opt-in). Bind the endpoint and publish its address to
    // the spawn path *before* the first pane is spawned, so every pane — the
    // initial one included — is born with `ATRIUM_CTL`/`ATRIUM_PANE` in its
    // environment. A bind failure is non-fatal: atrium still runs, just without
    // ctl, and says so in the bar (never silently unavailable).
    // Queue-until-idle deliveries for `ctl send` (flushed each tick).
    let mut pending_sends: Vec<PendingSend> = Vec::new();
    // Operator-approved extra allowlist stems (ATRIUM_CTL_ALLOW); empty ⇒ the
    // built-in agents-only guard. Read once at startup.
    let ctl_extra_allow = if allow_ctl {
        atrium::ctl::extra_allow_from_env()
    } else {
        Vec::new()
    };
    // Bind the endpoint here only if a caller hasn't already (the fleet path
    // pre-binds so its panes get `ATRIUM_CTL`). `CTL_ADDRESS` is a `OnceLock`, so a
    // pre-bind means this is a no-op.
    if allow_ctl && ctl_listener.is_none() {
        ctl_listener = bind_ctl(&mut flash);
    }
    // ctl audit log (design §5): in-memory always when ctl is on, mirrored to a
    // JSONL file when the operator opts in via `ATRIUM_CTL_AUDIT`. A file that
    // can't be opened is surfaced once in the bar; the in-memory log runs on.
    let mut ctl_audit = if allow_ctl {
        let path = std::env::var(atrium::ctl::ENV_AUDIT)
            .ok()
            .filter(|p| !p.is_empty());
        let mut a = atrium::audit::Audit::new(atrium::audit::DEFAULT_CAP, path.as_deref());
        if let Some(err) = a.take_error() {
            if flash.is_none() {
                flash = Some((format!("ctl audit: {err}"), Instant::now()));
            }
        }
        a
    } else {
        atrium::audit::Audit::in_memory()
    };
    // A pre-built window (fleet) is used as-is; otherwise mass-spawn opens one
    // window of N tiles in a balanced grid, or the 0.1 single-pane path. All
    // share the same spawn machinery (each pane its own session).
    let prebuilt = initial_window.is_some();
    let initial = match initial_window {
        Some(w) => Ok(w),
        None => match grid {
            Some(g) => spawn_window_grid(command, rows, cols, g, identity, &mut flash),
            None => spawn_window(command, rows, cols, 0, identity, trust_mode(), &mut flash),
        },
    };
    match initial {
        Ok(w) => windows.push(w),
        Err(e) => {
            cleanup_screen(&mut out);
            eprintln!("atrium: cannot start {:?}: {e}", command[0]);
            return ExitCode::FAILURE;
        }
    }
    // A grid or pre-built (fleet) window is born tiled: resize so every pane's
    // pty/emulator gets its true inner rect (spawn used rough sizes).
    if grid.is_some() || prebuilt {
        resize_window(&mut windows[0], rows, cols);
    }
    let mut active = 0usize; // active window index
    let mut scanner = PrefixScanner::new();
    // Mouse capture is OFF by default so the terminal's own **text selection**
    // works out of the box (dragging highlights, as in any shell) — the common
    // case. `Ctrl+A m` toggles capture ON when you want the atrium mouse: click to
    // focus the pane under the cursor, wheel to scroll the hovered tile. (With
    // capture on, native selection falls back to Shift-drag.) The scanner and the
    // terminal are kept in sync by the toggle.
    let mut mouse_on = false;
    // Whether atrium has forced the OUTER terminal's mouse reporting off because the
    // focused pane does not want the mouse (a shell/WSL). A mouse app (claude)
    // enables motion tracking via passthrough, which leaks to the terminal; once
    // you switch to a non-mouse pane the terminal keeps sending motion events and
    // they get forwarded into that pane as garbage text. See the re-assert below.
    let mut outer_mouse_off = false;
    // The full-screen board dashboard overlay (`Ctrl+A b`). While on, the panes
    // keep running (drained, emulated) but are not painted, and keystrokes don't
    // reach them — it's a read-only view of the shared board.
    let mut board_view = false;
    // The mission-control overview (`Ctrl+A o`): the agent tree colored by status
    // with a selection cursor. While on, keystrokes drive the cursor / dive-in
    // instead of reaching the panes. `overview_sel` is the selected agent index.
    let mut overview_view = false;
    let mut overview_sel = 0usize;
    // Scrollback offset for the board panel's bus feed: how many of the newest FYI
    // events to skip so older ones come into view (`0` = live/newest). Reset when
    // the panel opens; driven by PgUp/PgDn while it's up.
    let mut feed_scroll = 0usize;
    // Which open decision is selected in the board panel (index into the pending
    // list). j/k move it; `r` resolves it; `g`/Enter jumps to the agent that
    // raised it. The detail bar shows the selected decision's full question.
    let mut decision_sel = 0usize;
    // The activity log (`Ctrl+A l`): a time-ordered merge of bus + board + agent
    // actions. `log_scroll` is how many events back from the newest (bottom) the
    // view is scrolled; `0` = tailing the latest.
    let mut log_view = false;
    let mut log_scroll = 0usize;
    // The command prompt (`Ctrl+A :`): `Some(line)` while the operator is typing a
    // command to open in a new pane (any shell/program, not just the launch one).
    // Keystrokes edit the line instead of reaching the panes; Enter opens it.
    let mut prompt: Option<String> = None;
    let mut buf = [0u8; 8192];
    let mut force_repaint = true;
    let mut last_bar_paint = Instant::now();
    let mut last_size_check = Instant::now();
    // Animation clock for the per-pane loading spinner (advances ~8 frames/sec).
    // A blank (still-booting) pane's spinner changes with this, so the tiled diff
    // repaints just those cells; once every pane has painted, the frame no longer
    // affects the master, so it stops driving repaints.
    let anim_start = Instant::now();
    // The spinner frame last painted by the passthrough startup splash, so it
    // redraws only when the frame advances (not every tick).
    let mut last_splash_frame: usize = usize::MAX;
    let mut last_bar = String::new();

    // Agent session state (§5): one read-only `agsess::World` over the Claude
    // projects root, polled on the loop's existing `Instant`-throttle pattern —
    // no threads. The *first* refresh uses `refresh_since(process_start_ms)` so
    // the cold history scan (agtop measures ~1.4 s for ~60 MB) never freezes
    // keystrokes; atrium can never care about a session that stopped writing
    // before it started. Thereafter: bound panes tail ~1 s, discovery ~5 s
    // (accelerated to ~1 s while any agent pane is still unbound).
    // The shared board (coordination layer): source-of-truth team state, in-memory
    // unless ATRIUM_BOARD names a snapshot file. Part of the ctl surface, so it is
    // already gated by `--allow-ctl`.
    let mut board = match std::env::var_os(atrium::board::ENV_BOARD) {
        Some(p) if !p.is_empty() => atrium::board::Board::with_file(std::path::PathBuf::from(p)),
        _ => atrium::board::Board::new(),
    };
    // The shared pub/sub bus (coordination layer part 2): the team's event stream,
    // in-memory unless ATRIUM_BUS names a snapshot file. Same ctl gating as the board.
    let mut bus = match std::env::var_os(atrium::bus::ENV_BUS) {
        Some(p) if !p.is_empty() => atrium::bus::Bus::with_file(std::path::PathBuf::from(p)),
        _ => atrium::bus::Bus::new(),
    };
    let mut world = atrium::vendors::VendorWorlds::new();
    let process_start_ms = agsess::sessions::now_ms();
    world.refresh_since(process_start_ms);
    let mut last_agent_poll = Instant::now();
    let mut last_agent_discover = Instant::now();
    // The previous composited master, kept per-frame so tiled mode diffs. Reset
    // to None (full repaint) on mode/layout/window changes.
    let mut prev_master: Option<ansi::Screen> = None;
    // The last view identity (window / zoom / focused pane / overlay / size). A
    // passthrough (single or zoomed) pane only gets the heavy repaint nudge
    // (clear + resize) on a *real* transition — never on a routine `force_repaint`
    // from bus/board/flash churn. Without this, a zoomed pane during a fleet run
    // is cleared several times a second (every ctl request forces a repaint),
    // which reads as paint corruption and makes text selection impossible (the
    // clear wipes the drag). Sentinel start so the first frame counts as a change.
    // The crash registry: the pane process groups a watchdog should kill if this
    // process dies without running any teardown at all.
    let registry_path = atrium::reap::registry_path(std::process::id());
    // The warden: tripwires, not gates. atrium cannot stop an agent that can run
    // commands from launching an unconstrained one (it could run claude directly
    // with no atrium at all), so what it CAN do is notice - its own binary being
    // edited to remove the ceiling, the registry that ceiling is read from being
    // tampered with, or a session appearing that the ancestry cap did not explain.
    let mut warden = atrium::warden::Warden::new(registry_path.clone());
    let mut last_warden_check = Instant::now();
    let mut cap_notice_raised = false;
    let mut registered: Vec<u32> = Vec::new();
    // Session teardown container: on Windows a kill-on-close Job Object so no pane
    // tree outlives atrium however it dies (TerminateProcess included); a no-op on
    // unix (the process-group teardown + watchdog below already cover the tree).
    // Held for the whole run — dropping it (or the process exiting) fires the
    // guarantee. Panes never inherit its handle, so they live until atrium exits.
    let session_job = atrium::reap::SessionJob::create();
    // Held for the whole run: dropping this Child closes the pipe atrium uses as its
    // death signal, which would fire the watchdog early. Unix only — on Windows
    // the Job Object replaces it (a watchdog there can't signal a process group
    // and would block on its pipe forever, R5).
    #[cfg(unix)]
    let mut watchdog: Option<std::process::Child> = None;
    let mut last_view: (usize, bool, usize, bool, bool, bool, u16, u16) =
        (usize::MAX, false, usize::MAX, false, false, false, 0, 0);

    // The read at the top can fail (terminal gone) *and* commands deep inside
    // `break 'outer`; a labeled `loop` expresses both. clippy's while-let
    // rewrite can't host the labeled break, so allow it here.
    #[allow(clippy::while_let_loop)]
    'outer: loop {
        // 0. a termination signal (SIGHUP from a closed terminal window, or a
        // SIGTERM/SIGINT from outside) exits through this *normal* path, so the
        // pane teardown after the loop actually runs. Without it the default
        // action killed atrium outright and every hosted agent was orphaned onto
        // `init` — see `atrium::signals`.
        if atrium::signals::terminating() {
            if std::env::var_os("ATRIUM_DEBUG").is_some() {
                eprint!("[atrium-dbg signal-quit]\r\n");
            }
            break 'outer;
        }
        // 0b. Keep the crash registry current. Rewritten only when the pane set
        // changes, so an idle session does no filesystem work; the watchdog
        // re-reads it at teardown, which is how panes opened later are covered.
        {
            let cur: Vec<u32> = windows
                .iter()
                .flat_map(|w| w.panes.iter().map(|p| p.pty.pid()))
                .filter(|p| *p != 0)
                .collect();
            if cur != registered {
                // Assign any newly-appeared pane to the session job so its whole
                // tree is torn down with atrium (no-op on unix). Only the new pids,
                // so a process is never re-assigned.
                for pid in cur.iter().filter(|p| !registered.contains(p)) {
                    session_job.assign(*pid);
                }
                let _ =
                    atrium::reap::write_registry(&registry_path, &cur, trust_mode().policy_label());
                warden.registry_rewritten();
                registered = cur;
            }
            if last_warden_check.elapsed() >= WARDEN_INTERVAL {
                last_warden_check = Instant::now();
                // The live pane pids double as the second descent signal: `pty`
                // makes each pane a session leader, so anything the agent spawns
                // in a pane carries that session id even after a double-fork has
                // erased its parent link.
                let mut alerts = warden.check(&registered);
                // The ancestry cap fired before the bus existed; raise it once now.
                if let Some(note) = CAP_NOTICE.get() {
                    if !cap_notice_raised {
                        cap_notice_raised = true;
                        alerts.push(atrium::warden::Alert {
                            kind: "warden-nested-atrium",
                            detail: note.clone(),
                        });
                    }
                }
                for alert in alerts {
                    ctl_audit.record(None, alert.kind, &alert.detail, false, "");
                    // A decision, not an FYI: these are exactly the events that
                    // should stop the operator rather than scroll past them.
                    let _ = bus.publish(
                        "warden",
                        atrium::bus::Kind::DecisionNeeded,
                        None,
                        &[("msg".to_string(), alert.detail.clone())],
                        agsess::sessions::now_ms(),
                    );
                    force_repaint = true;
                }
                // There is no enforcement branch here any more, and that is a
                // decision rather than an omission: `ATRIUM_WARDEN=enforce` used to
                // tear down anything judged an escapee, and both the judgement and
                // the kill were unsound. `warden`'s module docs carry the full
                // reasoning; the short version is that under a correct ancestry
                // rule the enforceable set is empty, the only sessions left to
                // accuse are indistinguishable from an ordinary reparenting, and
                // the kill target was read out of a file the accused could write.
            }
            // The watchdog is the unix answer to a death no handler can catch.
            // Windows does not need it and must not run it: the durable fix there
            // is a Job Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, which the
            // kernel honours however atrium dies. A watchdog on Windows would spawn
            // a second atrium that cannot signal a process group (`reap`'s
            // non-unix `signal_group` returns false) and would sit blocked on its
            // pipe forever. The Job Object attaches just above (`session_job`),
            // in this same pane-set-changed block.
            #[cfg(unix)]
            if watchdog.is_none() && !registered.is_empty() {
                watchdog = atrium::reap::spawn_watchdog(&registry_path).ok();
            }
        }
        // 1. keystrokes -> scanner -> focused pane / commands
        let bytes = match term.read_bytes(Duration::from_millis(15)) {
            Ok(b) => b,
            Err(_) => break,
        };
        if !bytes.is_empty() && std::env::var_os("ATRIUM_DEBUG").is_some() {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            eprint!("[atrium-dbg stdin {}]\r\n", hex.join(" "));
        }
        // The command prompt captures keystrokes: while it is open, keys edit the
        // command line (not the panes), and the scanner is fed nothing. Enter opens
        // the typed command in a new pane; Esc/Ctrl+C cancels.
        let feed: &[u8] = if prompt.is_some() {
            match edit_prompt(prompt.as_mut().unwrap(), &bytes) {
                PromptEdit::Continue => {}
                PromptEdit::Cancel => prompt = None,
                PromptEdit::Submit => {
                    let argv = split_cmdline(prompt.take().unwrap_or_default().trim());
                    if !argv.is_empty() {
                        match spawn_window(
                            &argv,
                            rows,
                            cols,
                            windows.len(),
                            identity,
                            trust_mode(),
                            &mut flash,
                        ) {
                            Ok(w) => {
                                windows.push(w);
                                let last = windows.len() - 1;
                                switch_window(
                                    &mut windows,
                                    &mut active,
                                    last,
                                    rows,
                                    cols,
                                    &mut out,
                                );
                                prev_master = None;
                            }
                            Err(e) => {
                                flash = Some((
                                    format!("cannot start {:?}: {e}", argv[0]),
                                    Instant::now(),
                                ));
                            }
                        }
                    }
                }
            }
            // Repaint only when the user actually pressed something (the line
            // changed, or it opened/closed) — NOT every idle tick, which made the
            // prompt row flash rapidly.
            if !bytes.is_empty() {
                force_repaint = true;
            }
            b""
        } else {
            &bytes
        };
        for action in scanner.feed(feed) {
            // While the overview is open, keystrokes drive the selection cursor and
            // dive-in — not the panes. `Ctrl+A o` (toggle) and `Ctrl+A q` (quit)
            // still work via the scanner; everything else is consumed here.
            if overview_view {
                let count: usize = windows.iter().map(|w| w.panes.len()).sum();
                match &action {
                    Action::ToggleOverview => {
                        overview_view = false;
                        prev_master = None;
                        force_repaint = true;
                    }
                    Action::ToggleBoard => {
                        // Switch straight from the overview to the board — one press,
                        // no need to close the overview first (the board render hides
                        // the cursor and clears the screen itself).
                        overview_view = false;
                        board_view = true;
                        feed_scroll = 0;
                        decision_sel = 0;
                        prev_master = None;
                        force_repaint = true;
                    }
                    Action::ToggleLog => {
                        overview_view = false;
                        log_view = true;
                        log_scroll = 0;
                        prev_master = None;
                        force_repaint = true;
                    }
                    Action::Quit => break 'outer,
                    Action::MoveFocus(Dir::Up) => {
                        overview_sel = overview_sel.saturating_sub(1);
                        force_repaint = true;
                    }
                    Action::MoveFocus(Dir::Down) => {
                        overview_sel = (overview_sel + 1).min(count.saturating_sub(1));
                        force_repaint = true;
                    }
                    Action::Forward(b) => {
                        let s = b.as_slice();
                        if s == b"\r" || s == b"\n" {
                            // Dive into the selected agent: focus its pane, zoom it
                            // full-screen, and RESIZE it (as `Ctrl+A z` does) — the
                            // pane was a small tile, so without the resize it would
                            // paint into a corner.
                            let target = overview_nodes(&windows, &world)
                                .get(overview_sel)
                                .map(|n| (n.window, n.pane_id));
                            if let Some((wi, pid)) = target {
                                active = wi;
                                windows[active].tree.focus_pane(pid);
                                windows[active].zoomed = windows[active].panes.len() > 1;
                                resize_window(&mut windows[active], rows, cols);
                                overview_view = false;
                                prev_master = None;
                                force_repaint = true;
                            }
                        } else if s == b"j" || s == b"\x1b[B" || s == b"\x1bOB" {
                            overview_sel = (overview_sel + 1).min(count.saturating_sub(1));
                            force_repaint = true;
                        } else if s == b"k" || s == b"\x1b[A" || s == b"\x1bOA" {
                            overview_sel = overview_sel.saturating_sub(1);
                            force_repaint = true;
                        } else if s == b"\x1b" {
                            overview_view = false;
                            prev_master = None;
                            force_repaint = true;
                        }
                    }
                    _ => {} // swallow every other command while the overview is up
                }
                continue;
            }
            // While the board panel is up, keystrokes drive it (scroll / switch /
            // quit) rather than reaching the hidden panes. Mirrors the overview
            // block above; `Ctrl+A b` closes, `Ctrl+A o` switches to the overview.
            if board_view {
                // Snapshot the selected decision's seq + source before any mutation
                // (pending_decisions borrows the bus; resolving needs it free).
                let decisions_now = bus.pending_decisions();
                let dcount = decisions_now.len();
                let dsel = decision_sel.min(dcount.saturating_sub(1));
                let sel_seq = decisions_now.get(dsel).map(|e| e.seq);
                let sel_from = decisions_now.get(dsel).and_then(|e| e.from.clone());
                drop(decisions_now);
                let scroll_max = bus.tail(atrium::bus::RING_CAP).len().saturating_sub(1);
                // Up/down select a decision when there are any; otherwise they
                // scroll the FYI history. PgUp/PgDn always scroll it.
                let nav = |up: bool, decision_sel: &mut usize, feed_scroll: &mut usize| {
                    if dcount > 0 {
                        *decision_sel = if up {
                            dsel.saturating_sub(1)
                        } else {
                            (dsel + 1).min(dcount - 1)
                        };
                    } else if up {
                        *feed_scroll = (*feed_scroll + 1).min(scroll_max);
                    } else {
                        *feed_scroll = feed_scroll.saturating_sub(1);
                    }
                };
                match &action {
                    Action::ToggleBoard => {
                        board_view = false;
                        let _ = write!(out, "\x1b[?25h");
                        prev_master = None;
                        force_repaint = true;
                    }
                    Action::ToggleOverview => {
                        overview_view = true;
                        board_view = false;
                        overview_sel = overview_nodes(&windows, &world)
                            .iter()
                            .position(|n| {
                                n.window == active && n.pane_id == windows[active].tree.focus()
                            })
                            .unwrap_or(0);
                        prev_master = None;
                        force_repaint = true;
                    }
                    Action::ToggleLog => {
                        board_view = false;
                        log_view = true;
                        log_scroll = 0;
                        let _ = write!(out, "\x1b[?25h");
                        prev_master = None;
                        force_repaint = true;
                    }
                    Action::Quit => break 'outer,
                    Action::MoveFocus(Dir::Up) => {
                        nav(true, &mut decision_sel, &mut feed_scroll);
                        force_repaint = true;
                    }
                    Action::MoveFocus(Dir::Down) => {
                        nav(false, &mut decision_sel, &mut feed_scroll);
                        force_repaint = true;
                    }
                    Action::Forward(b) => {
                        let s = b.as_slice();
                        if s == b"k" || s == b"\x1b[A" || s == b"\x1bOA" {
                            nav(true, &mut decision_sel, &mut feed_scroll);
                            force_repaint = true;
                        } else if s == b"j" || s == b"\x1b[B" || s == b"\x1bOB" {
                            nav(false, &mut decision_sel, &mut feed_scroll);
                            force_repaint = true;
                        } else if s == b"\x1b[5~" {
                            feed_scroll = (feed_scroll + 10).min(scroll_max);
                            force_repaint = true;
                        } else if s == b"\x1b[6~" {
                            feed_scroll = feed_scroll.saturating_sub(10);
                            force_repaint = true;
                        } else if s == b"r" || s == b"R" {
                            // Resolve the selected decision (after you've answered it
                            // in the agent's pane); clear it from the awaiting list.
                            if let Some(seq) = sel_seq {
                                bus.resolve(seq);
                                flash = Some((format!("resolved decision #{seq}"), Instant::now()));
                                decision_sel = decision_sel.min(dcount.saturating_sub(2));
                                force_repaint = true;
                            }
                        } else if s == b"g" || s == b"\r" || s == b"\n" {
                            // Jump to the pane of the agent that raised the decision
                            // (by role), zoomed, and close the panel — go answer it.
                            let target = sel_from.as_ref().and_then(|role| {
                                windows.iter().enumerate().find_map(|(wi, w)| {
                                    w.panes
                                        .iter()
                                        .find(|p| p.role.as_deref() == Some(role.as_str()))
                                        .map(|p| (wi, p.id))
                                })
                            });
                            if let Some((wi, pid)) = target {
                                active = wi;
                                windows[active].tree.focus_pane(pid);
                                windows[active].zoomed = windows[active].panes.len() > 1;
                                resize_window(&mut windows[active], rows, cols);
                                board_view = false;
                                let _ = write!(out, "\x1b[?25h");
                                prev_master = None;
                                force_repaint = true;
                            } else {
                                flash = Some((
                                    format!(
                                        "no pane for agent {}",
                                        sel_from.clone().unwrap_or_default()
                                    ),
                                    Instant::now(),
                                ));
                                force_repaint = true;
                            }
                        } else if s == b"\x1b" {
                            board_view = false;
                            let _ = write!(out, "\x1b[?25h");
                            prev_master = None;
                            force_repaint = true;
                        }
                    }
                    _ => {} // swallow every other command while the board is up
                }
                continue;
            }
            // While the activity log is up, keystrokes scroll it or switch views.
            if log_view {
                let total = collect_log(&windows, &world, &board, &bus).len();
                let scroll_max = total.saturating_sub(1);
                match &action {
                    Action::ToggleLog => {
                        log_view = false;
                        prev_master = None;
                        force_repaint = true;
                    }
                    Action::ToggleBoard => {
                        log_view = false;
                        board_view = true;
                        feed_scroll = 0;
                        decision_sel = 0;
                        prev_master = None;
                        force_repaint = true;
                    }
                    Action::ToggleOverview => {
                        log_view = false;
                        overview_view = true;
                        overview_sel = overview_nodes(&windows, &world)
                            .iter()
                            .position(|n| {
                                n.window == active && n.pane_id == windows[active].tree.focus()
                            })
                            .unwrap_or(0);
                        prev_master = None;
                        force_repaint = true;
                    }
                    Action::Quit => break 'outer,
                    // Up/k = older (scroll back); down/j = newer; PgUp/PgDn ×10.
                    Action::MoveFocus(Dir::Up) => {
                        log_scroll = (log_scroll + 1).min(scroll_max);
                        force_repaint = true;
                    }
                    Action::MoveFocus(Dir::Down) => {
                        log_scroll = log_scroll.saturating_sub(1);
                        force_repaint = true;
                    }
                    Action::Forward(b) => {
                        let s = b.as_slice();
                        if s == b"k" || s == b"\x1b[A" || s == b"\x1bOA" {
                            log_scroll = (log_scroll + 1).min(scroll_max);
                            force_repaint = true;
                        } else if s == b"j" || s == b"\x1b[B" || s == b"\x1bOB" {
                            log_scroll = log_scroll.saturating_sub(1);
                            force_repaint = true;
                        } else if s == b"\x1b[5~" {
                            log_scroll = (log_scroll + 10).min(scroll_max);
                            force_repaint = true;
                        } else if s == b"\x1b[6~" {
                            log_scroll = log_scroll.saturating_sub(10);
                            force_repaint = true;
                        } else if s == b"\x1b" {
                            log_view = false;
                            prev_master = None;
                            force_repaint = true;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match action {
                Action::Forward(b) => {
                    if let Some(p) = windows[active].focused_mut() {
                        let _ = p.pty.write(&b);
                    }
                }
                Action::NextPane => {
                    let next = (active + 1) % windows.len();
                    switch_window(&mut windows, &mut active, next, rows, cols, &mut out);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::PrevPane => {
                    let prev = (active + windows.len() - 1) % windows.len();
                    switch_window(&mut windows, &mut active, prev, rows, cols, &mut out);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::SwitchTo(i) => {
                    if i < windows.len() {
                        switch_window(&mut windows, &mut active, i, rows, cols, &mut out);
                        prev_master = None;
                        force_repaint = true;
                    }
                }
                Action::NewPane => {
                    match spawn_window(
                        command,
                        rows,
                        cols,
                        windows.len(),
                        identity,
                        trust_mode(),
                        &mut flash,
                    ) {
                        Ok(w) => {
                            windows.push(w);
                            let last = windows.len() - 1;
                            switch_window(&mut windows, &mut active, last, rows, cols, &mut out);
                            prev_master = None;
                            force_repaint = true;
                        }
                        Err(e) => {
                            flash = Some((
                                format!("cannot start {:?}: {e}", command[0]),
                                Instant::now(),
                            ));
                            force_repaint = true;
                        }
                    }
                }
                Action::NewShellPane => {
                    // A plain shell in a new window — no identity, no trust posture
                    // (it's not an agent), but it still gets the ctl env injected,
                    // so you can run `atrium ctl board list` here and see it rendered.
                    let shell = vec![default_shell()];
                    match spawn_window(
                        &shell,
                        rows,
                        cols,
                        windows.len(),
                        None,
                        atrium::ctl::TrustMode::Off,
                        &mut flash,
                    ) {
                        Ok(w) => {
                            windows.push(w);
                            let last = windows.len() - 1;
                            switch_window(&mut windows, &mut active, last, rows, cols, &mut out);
                            prev_master = None;
                            force_repaint = true;
                        }
                        Err(e) => {
                            flash = Some((
                                format!("cannot start shell {:?}: {e}", shell[0]),
                                Instant::now(),
                            ));
                            force_repaint = true;
                        }
                    }
                }
                Action::ToggleBoard => {
                    board_view = !board_view;
                    feed_scroll = 0; // always open at the live/newest end
                    decision_sel = 0;
                    // Toggling either way is a full repaint: on → draw the panel
                    // (it clears the screen); off → recompose/repaint the panes the
                    // panel covered. On close, restore the cursor the panel hid.
                    if !board_view {
                        let _ = write!(out, "\x1b[?25h");
                    }
                    prev_master = None;
                    force_repaint = true;
                }
                Action::OpenPrompt => {
                    // Start the command prompt; subsequent keystrokes edit the line
                    // (handled above the scanner) until Enter/Esc.
                    prompt = Some(String::new());
                    force_repaint = true;
                }
                Action::ToggleOverview => {
                    // Open the overview (close the board if it was up — one overlay
                    // at a time). Selection starts at the focused agent so Enter
                    // dives back into what you were watching.
                    overview_view = true;
                    board_view = false;
                    let focus = windows[active].tree.focus();
                    overview_sel = overview_nodes(&windows, &world)
                        .iter()
                        .position(|n| n.window == active && n.pane_id == focus)
                        .unwrap_or(0);
                    let _ = write!(out, "\x1b[?25h");
                    prev_master = None;
                    force_repaint = true;
                }
                Action::ToggleLog => {
                    // Open the activity log (one overlay at a time).
                    log_view = true;
                    board_view = false;
                    overview_view = false;
                    log_scroll = 0;
                    let _ = write!(out, "\x1b[?25h");
                    prev_master = None;
                    force_repaint = true;
                }
                Action::SplitH => {
                    split_focused(
                        &mut windows[active],
                        layout::Dir::Horizontal,
                        command,
                        rows,
                        cols,
                        identity,
                        &mut flash,
                    );
                    resize_window(&mut windows[active], rows, cols);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::SplitV => {
                    split_focused(
                        &mut windows[active],
                        layout::Dir::Vertical,
                        command,
                        rows,
                        cols,
                        identity,
                        &mut flash,
                    );
                    resize_window(&mut windows[active], rows, cols);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::MoveFocus(d) => {
                    let outer = tiled_outer(rows, cols);
                    windows[active].tree.move_focus(to_move(d), outer);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::Zoom => {
                    let w = &mut windows[active];
                    // Zoom only means something with more than one pane.
                    if w.panes.len() > 1 {
                        w.zoomed = !w.zoomed;
                        resize_window(w, rows, cols);
                        prev_master = None;
                        force_repaint = true;
                    }
                }
                Action::ToggleMouse => {
                    mouse_on = !mouse_on;
                    // Keep the terminal's capture and the scanner's parsing in
                    // lockstep — set both, and don't leave one on if the other
                    // fails.
                    if term.set_mouse(mouse_on).is_ok() {
                        scanner.set_mouse(mouse_on);
                        flash = Some((
                            if mouse_on {
                                "mouse: ON — click focuses, wheel scrolls the hovered tile (Shift-drag selects)"
                                    .to_string()
                            } else {
                                "mouse: OFF — native drag to select / copy".to_string()
                            },
                            Instant::now(),
                        ));
                    } else {
                        mouse_on = !mouse_on; // revert on failure
                        flash = Some(("mouse mode unavailable here".to_string(), Instant::now()));
                    }
                    force_repaint = true;
                }
                Action::MouseClick { col, row } => {
                    // Focus the pane under the cursor. Only meaningful in a tiled
                    // window (a single/zoomed pane already has focus).
                    let w = &mut windows[active];
                    if w.tiled() {
                        let outer = tiled_outer(rows, cols);
                        let mx = col.saturating_sub(1) as usize;
                        let my = row.saturating_sub(1) as usize;
                        for (id, rect) in w.tree.rects(outer) {
                            let inside = my >= rect.row
                                && my < rect.row + rect.rows
                                && mx >= rect.col
                                && mx < rect.col + rect.cols;
                            if inside {
                                if w.tree.focus_pane(id) {
                                    prev_master = None;
                                    force_repaint = true;
                                }
                                break;
                            }
                        }
                    }
                }
                Action::MouseScroll { up, col, row } => {
                    // Route a wheel notch to the tile under the cursor (not
                    // necessarily the focused one) so you scroll whatever you're
                    // hovering. Forward a translated SGR wheel event to that pane's
                    // app — only if it wants the mouse, so a bare shell never gets
                    // stray bytes. `64` = wheel up, `65` = wheel down.
                    let w = &mut windows[active];
                    let notch = if up { 64 } else { 65 };
                    if w.tiled() {
                        let outer = tiled_outer(rows, cols);
                        let mx = col.saturating_sub(1) as usize;
                        let my = row.saturating_sub(1) as usize;
                        let hit = w.tree.rects(outer).into_iter().find(|(_, r)| {
                            my >= r.row && my < r.row + r.rows && mx >= r.col && mx < r.col + r.cols
                        });
                        if let Some((id, rect)) = hit {
                            if let Some(p) = w.pane_mut(id) {
                                if p.mouse_wanted {
                                    // Master cell → the pane's inner (bordered)
                                    // 1-based coords: content is inset one cell.
                                    let inner_cols = rect.cols.saturating_sub(2).max(1);
                                    let inner_rows = rect.rows.saturating_sub(2).max(1);
                                    let cx =
                                        mx.saturating_sub(rect.col + 1).min(inner_cols - 1) + 1;
                                    let cy =
                                        my.saturating_sub(rect.row + 1).min(inner_rows - 1) + 1;
                                    let seq = format!("\x1b[<{notch};{cx};{cy}M");
                                    let _ = p.pty.write(seq.as_bytes());
                                }
                            }
                        }
                    } else if let Some(p) = windows[active].focused_mut() {
                        // Passthrough / zoom: the sole pane fills the area above the
                        // bar; forward with the original coordinates.
                        if p.mouse_wanted {
                            let seq = format!("\x1b[<{notch};{col};{row}M");
                            let _ = p.pty.write(seq.as_bytes());
                        }
                    }
                }
                Action::KillPane => {
                    let w = &mut windows[active];
                    let victim = w.tree.focus();
                    if w.panes.len() > 1 {
                        // Multi-pane window: close synchronously so focus leaves
                        // the dying pane at once (the next keystrokes must route
                        // to the survivor, not the corpse) and the frame re-tiles
                        // without waiting for the async reap.
                        if let Some(p) = w.pane_mut(victim) {
                            let _ = p.pty.kill();
                        }
                        w.tree.close(victim);
                        w.panes.retain(|p| p.id != victim);
                        w.zoomed = w.zoomed && w.panes.len() > 1;
                        resize_window(w, rows, cols);
                        prev_master = None;
                        force_repaint = true;
                    } else if let Some(p) = w.pane_mut(victim) {
                        // Sole pane: async kill; the reap step drops the window
                        // and the app exits when the last window is gone (0.1).
                        let _ = p.pty.kill();
                    }
                }
                Action::Quit => {
                    if std::env::var_os("ATRIUM_DEBUG").is_some() {
                        eprint!("[atrium-dbg quit-received]\r\n");
                    }
                    break 'outer;
                }
            }
        }

        // 1b. ctl control channel: drain up to a few requests this tick
        //     (non-blocking; usually zero). Each request is applied as a pane
        //     operation and answered on the same connection. A spawn appends a
        //     visible new window, so we force a repaint after any request.
        if let Some(listener) = ctl_listener.as_mut() {
            for _ in 0..8 {
                match listener.poll() {
                    Ok(Some(line)) => {
                        let reply = apply_ctl(
                            &line,
                            &mut windows,
                            rows,
                            cols,
                            max_depth,
                            &ctl_extra_allow,
                            &mut pending_sends,
                            &world,
                            identity,
                            &mut ctl_audit,
                            &mut board,
                            &mut bus,
                        );
                        let _ = listener.respond(&reply);
                        prev_master = None;
                        force_repaint = true;
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        }

        // 1c. flush any queued `ctl send`s whose target is now idle (Decision 4).
        if flush_sends(&mut pending_sends, &mut windows, &world) {
            force_repaint = true;
        }

        // 2. drain every pane in the active window (all panes are live in
        //    tiled mode); feed the emulator, and in passthrough also write the
        //    focused pane's cleaned bytes straight through.
        let tiled = windows[active].tiled();
        let focus = windows[active].tree.focus();
        for pane in windows[active].panes.iter_mut() {
            // Drain each active pane *fully* before we composite, so a big data
            // burst (a large `ctl send`, a wall of tool output) is a whole frame
            // rather than a truncated one — a partial drain that straddles a
            // `?2026` block stalls the outer terminal (the "shutter"). The loop
            // still breaks the instant there is no more data, so the high cap only
            // bites on a genuinely huge burst; it never adds latency when idle.
            for i in 0..DRAIN_READS_PER_TICK {
                let wait = if i == 0 {
                    Duration::from_millis(5)
                } else {
                    Duration::ZERO
                };
                match pane.pty.read_timeout(&mut buf, wait) {
                    Ok(Some(n)) if n > 0 => {
                        // Always feed the emulator so a later switch/split/zoom
                        // renders the current screen without a repaint nudge.
                        pane.term.feed(&buf[..n]);
                        // Track whether this pane's app wants the mouse (so scroll
                        // routes to it, not a bare shell).
                        if let Some(m) = sniff_mouse_mode(&buf[..n]) {
                            pane.mouse_wanted = m;
                        }
                        // "painted" = the emulator now has *visible* content, not
                        // merely that setup bytes arrived — so the splash stays up
                        // until the agent's first frame.
                        let first_paint = !pane.painted && !term_blank(&pane.term);
                        if first_paint {
                            pane.painted = true;
                        }
                        if !tiled && pane.id == focus {
                            if board_view || overview_view || log_view {
                                // An overlay (board / overview) is up: keep the
                                // passthrough filter state current, but don't paint
                                // the pane over the panel. (The emulator was already
                                // fed above, so a toggle-off repaints from the
                                // current screen.)
                                let _ = pane.filter.feed(&buf[..n]);
                            } else if first_paint {
                                // Hand off from the splash: restore the cursor the
                                // splash hid, then paint the agent's current screen
                                // straight from the emulator (its `render_full`
                                // already clears + reflects everything fed so far),
                                // and resume live passthrough from here.
                                let full = pane.term.screen().render_full();
                                let _ = out.write_all(b"\x1b[?25h");
                                let _ = out.write_all(&full);
                                // render_full cleared the whole screen (the bar row
                                // too); repaint the bar this tick so it never blinks
                                // out during the handoff.
                                force_repaint = true;
                            } else if pane.painted {
                                let cleaned = pane.filter.feed(&buf[..n]);
                                let _ = out.write_all(&cleaned);
                                // A full-screen clear from the app (claude emits one
                                // at startup, and again as its UI settles) ignores
                                // the scroll region and wipes the bar row. Repaint the
                                // bar this tick so it doesn't vanish until some later
                                // trigger (which is why it only appeared once ctl/
                                // remote-control connected).
                                if clears_screen(&buf[..n]) {
                                    force_repaint = true;
                                }
                            } else {
                                // Still on the splash: keep the passthrough filter's
                                // state current, but suppress output so the agent's
                                // setup bytes don't scribble under the splash.
                                let _ = pane.filter.feed(&buf[..n]);
                            }
                        } else if pane.id != focus {
                            pane.activity = true;
                            pane.painted = true; // background/tiled: any output = painted
                        }
                    }
                    Ok(Some(_)) => {
                        pane.exited = true;
                        break;
                    }
                    _ => break,
                }
            }
        }
        if !tiled {
            let _ = out.flush();
        }

        // 3. background windows: drain (discarded — emulator/ConPTY keep the
        //    screen) and flag activity so the bar shows it.
        for (i, w) in windows.iter_mut().enumerate() {
            if i == active {
                continue;
            }
            for pane in w.panes.iter_mut() {
                // Bounded exactly like the focused drain above. This used to be
                // an unbounded `while let`, so one noisy background agent could
                // hold the loop for as long as it kept producing output —
                // starving keystrokes, signal handling and ctl for the whole
                // session. A fleet makes that likely rather than theoretical.
                let mut reads = 0usize;
                while reads < DRAIN_READS_PER_TICK {
                    let Ok(Some(n)) = pane.pty.read_timeout(&mut buf, Duration::ZERO) else {
                        break;
                    };
                    reads += 1;
                    if n == 0 {
                        pane.exited = true;
                        break;
                    }
                    pane.term.feed(&buf[..n]);
                    if let Some(m) = sniff_mouse_mode(&buf[..n]) {
                        pane.mouse_wanted = m;
                    }
                    pane.activity = true;
                }
            }
        }

        // 4. reap exits; drop dead panes and empty windows, re-tiling.
        let mut layout_changed = false;
        for w in windows.iter_mut() {
            for pane in w.panes.iter_mut() {
                if let Ok(Some(_)) = pane.pty.try_wait() {
                    pane.exited = true;
                }
            }
            if w.panes.iter().any(|p| p.exited) {
                let dead: Vec<usize> = w.panes.iter().filter(|p| p.exited).map(|p| p.id).collect();
                for id in dead {
                    // Collapse the tree first (keeps focus valid), then drop the
                    // pane. A last-pane close leaves the tree single; the window
                    // itself is removed below when its panes go empty.
                    let _ = w.tree.close(id);
                }
                w.panes.retain(|p| !p.exited);
                w.zoomed = w.zoomed && w.panes.len() > 1;
                layout_changed = true;
            }
        }
        if layout_changed {
            let empty_before_active = windows[..active]
                .iter()
                .filter(|w| w.panes.is_empty())
                .count();
            windows.retain(|w| !w.panes.is_empty());
            if windows.is_empty() {
                break;
            }
            active = active
                .saturating_sub(empty_before_active)
                .min(windows.len() - 1);
            resize_window(&mut windows[active], rows, cols);
            prev_master = None;
            force_repaint = true;
        }

        // 4b. Keep the OUTER terminal's mouse reporting off unless atrium itself
        //     turned it on (`Ctrl+A m`). A pane's own request no longer reaches
        //     the real terminal — `filter.rs` terminates the mouse modes with the
        //     other host-level negotiations — so atrium's own state is the whole
        //     truth here, and this only has to re-assert it.
        //
        //     This used to defer to the focused pane (`mouse_wanted`), because a
        //     mouse app's motion tracking DID pass through: focusing a non-mouse
        //     pane afterwards left the terminal emitting motion events that landed
        //     in that pane as literal text (`35;79;16M…`). Stripping at the filter
        //     removes that class outright, and with it the reason an agent pane
        //     cost the user native text selection.
        if !mouse_on {
            if !outer_mouse_off {
                let _ = out.write_all(MOUSE_OFF.as_bytes());
                let _ = out.flush();
                outer_mouse_off = true;
            }
        } else {
            outer_mouse_off = false;
        }

        // 5. resize propagation
        if last_size_check.elapsed() >= Duration::from_millis(150) {
            last_size_check = Instant::now();
            if let Ok((r, c)) = term.size() {
                // Windows Terminal's ConPTY reports a few rows FEWER once atrium is
                // in the alternate screen buffer (a persistent reservation), and
                // adopting it makes atrium redraw short — leaving a strip of stale
                // content below the bar (the initial full-screen draw was correct).
                // Treat a small height-only shrink as that reservation and keep the
                // current size; real resizes (width change, growth, or a large
                // shrink) still apply.
                let altscreen_reserve =
                    c == cols && r < rows && rows - r <= ALT_SCREEN_RESERVE_ROWS;
                if (r, c) != (rows, cols) && r >= 3 && !altscreen_reserve {
                    rows = r;
                    cols = c;
                    // Reset the scroll region for the new height, then wipe the
                    // whole screen. The wipe is what fixes the tiled-resize
                    // garble: when the terminal shrinks, cells from the previous,
                    // larger frame lie outside the new master and would otherwise
                    // linger; and a tiled recompose diffs against `prev_master`,
                    // so without this clear + `prev_master = None` the first frame
                    // at the new size can paint over stale geometry. This mirrors
                    // the clear `switch_window`/`repaint_focused` already emit.
                    let _ = write!(out, "\x1b[1;{}r\x1b[2J\x1b[H", rows - 1);
                    // Resize every window's panes (pty + emulator) to their new
                    // inner rects at the new size — tiled windows recompute the
                    // grid, passthrough windows refit the sole/focused pane.
                    // `resize_window` is the single helper both initial layout
                    // and resize share, so their inner-rect math cannot drift.
                    for w in windows.iter_mut() {
                        resize_window(w, rows, cols);
                    }
                    // Force a full redraw of the focused passthrough pane at the new
                    // size with a *second* resize (to size-1, then size). A single
                    // resize can be missed by an app mid-boot or mid-reconnect —
                    // claude keeps drawing at the stale size, ending up in a corner
                    // of the larger terminal. The extra resize guarantees a fresh
                    // SIGWINCH/redraw. (The `force_repaint` path below only nudges
                    // once the pane has painted, so it misses exactly the boot
                    // window the founder hit; this covers it.)
                    if !windows[active].tiled() {
                        let ar = rows.saturating_sub(1).max(1);
                        let focus = windows[active].tree.focus();
                        if let Some(p) = windows[active].pane_mut(focus) {
                            let _ = p.pty.resize(ar.saturating_sub(1).max(1), cols);
                            let _ = p.pty.resize(ar, cols);
                            p.term.resize(ar as usize, cols as usize);
                        }
                    }
                    // Force a full recompose at the new size: tiled repaints via
                    // `render_full` (which re-clears + paints every cell),
                    // passthrough nudges the focused pty to redraw.
                    prev_master = None;
                    force_repaint = true;
                }
            }
        }

        // 5b. agent state (§5). `refresh` tails only files that grew, so the
        //     bound-pane tick is cheap; discovery is the full `read_dir`, run
        //     rarely — but accelerated while any agent pane still lacks its
        //     transcript so a fresh agent binds promptly.
        let any_unbound = windows.iter().any(|w| {
            w.panes.iter().any(|p| {
                p.session_id
                    .as_deref()
                    .is_some_and(|id| world.status_for(Some(id)).is_none())
            })
        });
        let discover_every = if any_unbound {
            Duration::from_millis(1000)
        } else {
            Duration::from_millis(5000)
        };
        // While the sole pane is still on the startup splash, skip the refresh
        // entirely: nothing is bound yet (so the bar has nothing to show), and the
        // discovery pass is a full `read_dir` over the projects root that can block
        // the loop for the better part of a second — which froze the splash spinner
        // for a beat right at ~1 s in. Once the agent paints, polling resumes and
        // it binds on the next tick.
        let booting_splash = !windows[active].tiled()
            && windows[active]
                .pane(windows[active].tree.focus())
                .map(|p| !p.painted)
                .unwrap_or(false);
        let mut did_refresh = false;
        if booting_splash {
            // hold off — resumes as soon as the pane paints
        } else if last_agent_discover.elapsed() >= discover_every {
            // Discovery + tail: a full refresh picks up new transcripts and
            // tails grown ones in one pass.
            world.refresh();
            last_agent_discover = Instant::now();
            last_agent_poll = Instant::now();
            did_refresh = true;
        } else if last_agent_poll.elapsed() >= Duration::from_millis(1000) {
            // Bound-pane tick: refresh tails only files whose length grew, so
            // this is ~a handful of stats for a small grid.
            world.refresh();
            last_agent_poll = Instant::now();
            did_refresh = true;
            // Keep the overview and activity log live but calm: repaint once per
            // status poll (~1 Hz), not every tick, so they update without flicker.
            if overview_view || log_view {
                force_repaint = true;
            }
        }

        // 5c. session adoption. A claude pane binds via the `--session-id` atrium
        //     injected at spawn; a non-claude agent's CLI does not understand that
        //     flag, so its pane launches with `session_id == None` and would never
        //     bind. After each refresh, associate every still-unstamped non-claude
        //     agent pane with the newest session discovered under its vendor root
        //     at/after the pane's launch (cwd-preferred) and stamp that id — from
        //     then on the pane binds through the normal `status_for` path.
        //     Idempotent: only `None`, non-exited, known-non-claude panes are
        //     considered; a successful adopt fills `session_id` so later passes skip
        //     it.
        if did_refresh {
            let sessions = world.sessions();
            if !sessions.is_empty() {
                for w in &mut windows {
                    for p in &mut w.panes {
                        if p.session_id.is_some() || p.exited {
                            continue;
                        }
                        // Only a *known non-claude* vendor pane adopts (claude
                        // panes already carry an injected id). Scope the candidate
                        // pool to this pane's own vendor via `AgentSession.vendor`,
                        // so a gemini pane can never adopt the newest claude session
                        // that happens to sit in the merged pool.
                        let Some(vendor) = atrium::vendors::vendor_for_stem(&p.title) else {
                            continue;
                        };
                        if vendor == agsess::Vendor::ClaudeCode {
                            continue;
                        }
                        let mine: Vec<&agsess::AgentSession> = sessions
                            .iter()
                            .copied()
                            .filter(|s| s.vendor == vendor)
                            .collect();
                        if let Some(id) =
                            atrium::vendors::adopt_session_for(&mine, p.cwd.as_deref(), p.launch_ms)
                        {
                            p.session_id = Some(id);
                        }
                    }
                }
            }
        }

        // 6. render the active window (tiled compose+diff, else passthrough is
        //    already written above) then paint the bar. The tiled composite and
        //    the bar are accumulated into one `frame` and emitted wrapped in
        //    synchronized output (§`SYNC_BEGIN`), so the outer terminal paints
        //    panes+bar atomically — no mid-frame tearing. An empty frame (idle
        //    tick) emits nothing, so the markers never spam.
        let spin_frame = (anim_start.elapsed().as_millis() / 120) as usize;
        let mut frame: Vec<u8> = Vec::new();
        // Set when this tick composited the startup splash into `frame`; its `2J`
        // wipes the bar, so the bar is force-appended below to keep the frame whole.
        let mut splash_drawn = false;
        // A real view transition (which window, zoom, focused pane, overlay, or
        // terminal size) — the only thing that should trigger a passthrough pane's
        // clear+repaint nudge. Bus/board/flash `force_repaint`s don't change it.
        let view_key = (
            active,
            windows.get(active).map(|w| w.zoomed).unwrap_or(false),
            windows
                .get(active)
                .map(|w| w.tree.focus())
                .unwrap_or(usize::MAX),
            board_view,
            overview_view,
            log_view,
            rows,
            cols,
        );
        let view_changed = view_key != last_view;
        last_view = view_key;
        if overview_view {
            // The overview replaces the panes. Clamp the selection to the live
            // agent count (panes may have been reaped) and re-render on a repaint.
            let count: usize = windows.iter().map(|w| w.panes.len()).sum();
            overview_sel = overview_sel.min(count.saturating_sub(1));
            if force_repaint {
                let nodes = overview_nodes(&windows, &world);
                frame.extend_from_slice(
                    render_overview_panel(&windows, &bus, &nodes, overview_sel, rows, cols)
                        .as_bytes(),
                );
            }
        } else if board_view {
            // The board overlay replaces the panes. Re-render only on a repaint
            // (toggle, or a board write — every ctl request forces one), so idle
            // ticks leave the panel steady; the bar still paints below.
            if force_repaint {
                frame.extend_from_slice(
                    render_board_panel(&board, &bus, rows, cols, feed_scroll, decision_sel)
                        .as_bytes(),
                );
            }
        } else if log_view {
            // The activity log overlay: a live time-ordered merge of bus + board
            // + agent actions. Re-rendered on a repaint (toggle, scroll, or any
            // agent/ctl activity that forces one).
            if force_repaint {
                let now = agsess::sessions::now_ms();
                frame.extend_from_slice(
                    render_log_panel(&windows, &world, &board, &bus, rows, cols, log_scroll, now)
                        .as_bytes(),
                );
            }
        } else if windows[active].tiled() {
            let master = render_tiled(&windows[active], rows, cols, &world, spin_frame);
            match &prev_master {
                Some(prev) => frame.extend_from_slice(&prev.diff(&master)),
                None => frame.extend_from_slice(&master.render_full()),
            };
            prev_master = Some(master);
        } else {
            // Passthrough (single pane or zoomed). While the focused pane has not
            // painted yet, animate the startup splash so the ~seconds of agent
            // boot don't look like a hang; the drain wipes it and takes over on
            // the pane's first bytes. Throttled to the spinner's ~8 fps.
            let fp = windows[active].tree.focus();
            let unpainted = windows[active]
                .pane(fp)
                .map(|p| !p.painted)
                .unwrap_or(false);
            if unpainted {
                if spin_frame != last_splash_frame {
                    draw_startup_splash(&mut frame, rows, cols, spin_frame);
                    last_splash_frame = spin_frame;
                    splash_drawn = true;
                }
            } else if view_changed {
                // Nudge the focused pane's pty to repaint in full (the same trick
                // 0.1 uses on window switch) — but ONLY on a real view transition,
                // not on every force_repaint. A zoomed pane during a fleet run sees
                // constant force_repaints (each ctl request forces one); nudging on
                // those would clear + redraw it several times a second, wrecking the
                // paint and any in-progress text selection. The pane paints its own
                // steady-state output through passthrough; the nudge is only needed
                // to recover after a transition cleared the screen.
                repaint_focused(&mut windows[active], rows, cols, &mut out);
            }
        }

        // 7. the bar (windows, with the active one starred)
        let infos: Vec<PaneInfo> = windows
            .iter()
            .enumerate()
            .map(|(i, w)| PaneInfo {
                title: w.panes.first().map(|p| p.title.clone()).unwrap_or_default(),
                active: i == active,
                activity: w.panes.iter().any(|p| p.activity),
                exited: w.panes.iter().all(|p| p.exited),
                // A window is "waiting" when a bound agent pane in it is blocked
                // on the human (§4.2 rung 3). Status only.
                waiting: w.panes.iter().any(|p| {
                    matches!(
                        world.status_for(p.session_id.as_deref()),
                        Some(agsess::Status::WaitingApproval)
                    )
                }),
                // The window's identity tag: the first pane's identity name (all
                // panes in a window inherit the same identity in v1). Name only.
                identity: w.panes.first().and_then(|p| p.identity.clone()),
                // The window's persona/role (fleet agent name or ctl --role), so
                // the entry reads `N:claude:persona` and a decision's `from` maps
                // to a window number.
                role: w.panes.first().and_then(|p| p.role.clone()),
            })
            .collect();
        if flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5))
        {
            flash = None;
            force_repaint = true;
        }
        // A flash note wins; otherwise, if the bus has open `decision_needed`
        // escalations, surface them on the bar so you see them without opening the
        // `Ctrl+A b` panel — the bus's push channel to the human. A decision
        // addressed to a live teammate (`to=<role>`) is that agent's to answer, so
        // it is NOT counted here (the human still sees it in the board panel — the
        // broker keeps full visibility, just isn't urgently pinged for it).
        let decisions_open = bus
            .pending_decisions()
            .iter()
            .filter(|e| !decision_for_agent(e, &windows))
            .count();
        let decision_note = match decisions_open {
            0 => String::new(),
            1 => "1 decision needs you \u{00b7} Ctrl+A b".to_string(),
            n => format!("{n} decisions need you \u{00b7} Ctrl+A b"),
        };
        let note = flash.as_ref().map(|(m, _)| m.as_str()).unwrap_or({
            if decision_note.is_empty() {
                ""
            } else {
                decision_note.as_str()
            }
        });
        // While the command prompt is open it takes over the bar row: a `:` and
        // the line typed so far, cursor left right after it (visible) so you can
        // see what you're launching. Otherwise the normal status bar.
        let painted = if let Some(line) = &prompt {
            format!(
                "\x1b[{};1H\x1b[2K\x1b[1;38;2;70;235;255m:\x1b[0m{line}\x1b[?25h",
                rows
            )
        } else {
            bar_paint(&infos, rows, cols as usize, note)
        };
        let mut bar_appended = false;
        if force_repaint
            || splash_drawn
            || painted != last_bar
            // The 500ms periodic refresh keeps the status bar's activity markers
            // live, but while the command prompt is open it would reflash the
            // prompt row twice a second — skip it there (the prompt repaints on
            // keystroke via `painted != last_bar`).
            || (prompt.is_none() && last_bar_paint.elapsed() >= Duration::from_millis(500))
        {
            frame.extend_from_slice(painted.as_bytes());
            last_bar = painted;
            last_bar_paint = Instant::now();
            bar_appended = true;
        }
        // The bar leaves the cursor parked on the bar row. Move it back to the
        // pane's real cursor with an explicit CUP — NOT DECSC/DECRC, whose single
        // save slot is shared with the hosted app (claude parks its cursor there
        // for its own menus; saving/restoring around the bar corrupted it and left
        // redraw fragments in passthrough). We track the cursor ourselves: the
        // tiled master carries it, and even a passthrough pane is fed to its
        // emulator, so `term.screen().cursor` is current. Both are 0-based; the
        // passthrough pane fills the screen from (0,0), so +1 gives 1-based screen
        // coordinates in either mode.
        if bar_appended && !board_view && !overview_view && !log_view && prompt.is_none() {
            let cur = if windows[active].tiled() {
                prev_master.as_ref().map(|m| m.cursor)
            } else {
                let fp = windows[active].tree.focus();
                windows[active].pane(fp).map(|p| p.term.screen().cursor)
            };
            if let Some((cr, cc)) = cur {
                frame.extend_from_slice(format!("\x1b[{};{}H", cr + 1, cc + 1).as_bytes());
            }
        }
        // Emit the tick's composite+bar as ONE synchronized frame, so the outer
        // terminal never shows it half-drawn (the tiled "shutter"). Nothing to
        // draw ⇒ no write, no markers.
        if !frame.is_empty() {
            let _ = out.write_all(SYNC_BEGIN);
            let _ = out.write_all(&frame);
            let _ = out.write_all(SYNC_END);
            let _ = out.flush();
        }
        force_repaint = false;
    }

    let dbg = std::env::var_os("ATRIUM_DEBUG").is_some();
    // Kill process TREES, not processes. `pty.kill()` is SIGKILL to the direct
    // child alone, so everything an agent spawned — MCP servers, language
    // servers, node helpers — outlived atrium and was reparented onto init. That
    // leaked on every clean quit, not just on a signal. See `atrium::reap`.
    let pids: Vec<u32> = windows
        .iter()
        .flat_map(|w| w.panes.iter().map(|p| p.pty.pid()))
        .filter(|p| *p != 0)
        .collect();
    for pid in &pids {
        atrium::reap::term_tree(*pid);
    }
    if !pids.is_empty() {
        std::thread::sleep(atrium::reap::GRACE);
    }
    for pid in &pids {
        atrium::reap::kill_tree(*pid);
    }
    for w in windows.iter_mut() {
        // Still reap the direct child, so it does not linger as a zombie.
        for pane in w.panes.iter_mut() {
            let _ = pane.pty.kill();
        }
    }
    // Do not claim success if something survived: a teardown that can fail
    // silently is how 23 agent processes ended up holding 4.6 GB unnoticed.
    let survivors: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|p| atrium::reap::tree_alive(*p))
        .collect();
    // A CLEAN teardown leaves nothing for the watchdog: emptying the registry
    // before it notices is what makes a normal quit silent.
    //
    // A teardown with survivors is the opposite case, and this used to delete
    // the registry there too — unconditionally, on the one path where atrium had
    // just PROVED that pane groups outlived it. It then printed a warning to a
    // terminal `cleanup_screen` was about to tear down and exited; the watchdog
    // woke on pipe EOF, read a file that no longer existed, got an empty list
    // out of `read_registry`'s `unwrap_or_default`, and killed nothing. The
    // record was destroyed at exactly the moment it was needed.
    //
    // So: survivors mean the registry is REWRITTEN down to just those groups —
    // the watchdog then TERMs and KILLs precisely what is left, and a later
    // `atrium reap` finds the same short list rather than a stale full one. Panes
    // that did die are dropped from it, because a dead pane's pgid can be reused.
    if atrium::reap::settle_registry(&registry_path, &survivors, trust_mode().policy_label()) {
        eprintln!(
            "atrium: warning: {} pane process group(s) survived teardown: {:?}",
            survivors.len(),
            survivors
        );
    }
    // Panes that are genuinely gone leave no stamp behind. A survivor keeps
    // its stamp — that is the marker the sweep needs to collect it later.
    for pid in pids.iter().filter(|p| !survivors.contains(p)) {
        atrium::orphan::unstamp(*pid);
    }
    if dbg {
        eprint!("[atrium-dbg killed]\r\n");
    }
    cleanup_screen(&mut out);
    if dbg {
        eprint!("[atrium-dbg cleaned]\r\n");
    }
    drop(windows);
    if dbg {
        eprint!("[atrium-dbg panes-dropped]\r\n");
    }
    ExitCode::SUCCESS
}

/// The agsess status label the ctl protocol reports (stable strings the calling
/// agent can match on).
fn status_label(s: agsess::Status) -> &'static str {
    match s {
        agsess::Status::Working => "working",
        agsess::Status::WaitingApproval => "waiting-approval",
        agsess::Status::WaitingPrompt => "waiting-prompt",
        agsess::Status::Idle => "idle",
    }
}

/// Apply one ctl request against the live window set and return the JSON reply
/// line, **recording it to the audit log** (design §5). Thin wrapper: parse,
/// serve `audit` reads directly (they need the log and are not self-recorded),
/// else [`dispatch_ctl`] the request and record its outcome. `spawn`/`send`/
/// `status`/`kill` with a target are subtree-scoped; `spawn --identity` is
/// delegation-scoped.
#[allow(clippy::too_many_arguments)]
/// Is this decision addressed to a live teammate (a `to=<role>` field naming a
/// pane that exists)? Such a decision routes to that agent — the human isn't
/// urgently pinged for it — implementing worker→lead escalation before lead→human.
/// A `to` naming no live pane, or no `to` at all, is human-facing.
fn decision_for_agent(e: &atrium::bus::Event, windows: &[Window]) -> bool {
    e.fields.get("to").is_some_and(|to| {
        windows
            .iter()
            .flat_map(|w| &w.panes)
            .any(|p| p.role.as_deref() == Some(to.as_str()))
    })
}

/// Read-only control commands — served to any caller, authenticated or not.
/// Everything else mutates the fleet or its shared state (spawn, send, kill,
/// board writes/claims, bus publish/subscribe/resolve) and requires an
/// authenticated, token-matched caller.
fn ctl_is_read_only(cmd: &atrium::ctl::Cmd) -> bool {
    use atrium::ctl::{BoardOp, BusOp, Cmd};
    matches!(
        cmd,
        Cmd::List
            | Cmd::Status(_)
            | Cmd::Audit(_)
            | Cmd::Board(BoardOp::Get { .. })
            | Cmd::Board(BoardOp::List)
            | Cmd::Bus(BusOp::Feed { .. })
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_ctl(
    line: &str,
    windows: &mut Vec<Window>,
    rows: u16,
    cols: u16,
    max_depth: usize,
    extra_allow: &[String],
    pending: &mut Vec<PendingSend>,
    world: &atrium::vendors::VendorWorlds,
    session_identity: Option<&str>,
    audit: &mut atrium::audit::Audit,
    board: &mut atrium::board::Board,
    bus: &mut atrium::bus::Bus,
) -> String {
    use atrium::ctl::{self, Cmd};

    let mut req = match ctl::parse_request(line) {
        Ok(r) => r,
        Err(e) => {
            let reply = ctl::reply_err(&e);
            // NEVER log the raw request. `build_request` emits
            // `{"caller":N,"token":"` - a 21-character prefix - so an 80-char
            // truncation wrote 59 of a 64-character token into a log that
            // `ctl audit` hands to an unauthenticated reader. Record the shape
            // and the parse error, which is what debugging actually needs, and
            // nothing that was in the payload.
            audit.record(
                None,
                "bad-request",
                &format!("<unparseable, {} bytes>", line.len()),
                false,
                &e,
            );
            return reply;
        }
    };
    // Authenticate the caller by its capability token, not its self-reported id:
    // the caller *is* the pane whose minted token matches. This overwrites the
    // self-reported `caller`, so a pane cannot claim another pane's id or claim
    // operator (its own token forces its real, depth-scoped identity). A request
    // with no token or a non-matching one is unauthenticated. Every pane atrium
    // spawns in a ctl session is born with a token (the endpoint binds before any
    // spawn), so a legitimate caller is never locked out; only an external or
    // token-stripped request is. See §4.1 of the whitepaper for the threat model
    // and the peer-credential hardening that closes the remaining raw-socket vector.
    let authed = req
        .token
        .as_deref()
        .filter(|t| !t.is_empty())
        .and_then(|tok| {
            windows
                .iter()
                .flat_map(|w| &w.panes)
                // `as_deref()` so a pane with no token (entropy failure at
                // spawn) can never match — `None == None` must not authenticate.
                .find(|p| p.token.as_deref() == Some(tok))
                .map(|p| p.agent_id)
        });
    let authenticated = authed.is_some();
    req.caller = authed;
    // Reads (list/status/audit/board get/list/bus feed) are open; anything that
    // mutates the fleet or its shared state requires an authenticated caller.
    if !authenticated && !ctl_is_read_only(&req.cmd) {
        let reply = ctl::reply_err(
            "unauthenticated: control request carries no valid pane token (ATRIUM_TOKEN)",
        );
        let (action, detail) = audit_label(&req);
        audit.record(None, action, &detail, false, "unauthenticated");
        return reply;
    }
    let caller = req.caller;
    let privileged = caller_privileged(windows, caller);

    // `audit` reads the log, and IS recorded. It used to be exempt on the
    // reasoning that a query shouldn't pollute what it queries — but that made
    // the most sensitive read the only one leaving no trace, so exfiltrating the
    // log was invisible after the fact. The entry is written before the reply is
    // built, so a reader sees its own access: self-documenting, not hidden.
    // Subtree-scoped like any read: a worker sees only its own subtree.
    if let Cmd::Audit(ar) = &req.cmd {
        audit.record(caller, "audit", "read", true, "");
        return audit_reply(audit, windows, caller, privileged, ar.tail);
    }

    let (action, detail) = audit_label(&req);
    let reply = dispatch_ctl(
        req,
        windows,
        rows,
        cols,
        max_depth,
        extra_allow,
        pending,
        world,
        session_identity,
        privileged,
        board,
        bus,
    );
    let (ok, note) = audit_outcome(&reply);
    audit.record(caller, action, &detail, ok, &note);
    reply
}

/// The server side of the control channel: turn one parsed [`atrium::ctl::Request`]
/// into its JSON reply. `list`/`status` serialize the spawn tree (agsess status
/// folded in); `spawn` opens a visible worker after the pure allowlist/depth
/// guard ([`atrium::ctl::evaluate_spawn`]) and the credential-delegation guard
/// ([`atrium::ctl::delegation_allowed`]); `send` enqueues a queue-until-idle
/// delivery; `kill` tears down the target's subtree (the reap step reaps them).
/// `send`/`status`/`kill` with a target are subtree-scoped.
#[allow(clippy::too_many_arguments)]
fn dispatch_ctl(
    req: atrium::ctl::Request,
    windows: &mut Vec<Window>,
    rows: u16,
    cols: u16,
    max_depth: usize,
    extra_allow: &[String],
    pending: &mut Vec<PendingSend>,
    world: &atrium::vendors::VendorWorlds,
    session_identity: Option<&str>,
    privileged: bool,
    board: &mut atrium::board::Board,
    bus: &mut atrium::bus::Bus,
) -> String {
    use atrium::ctl::{self, Cmd};
    let caller = req.caller;

    match req.cmd {
        Cmd::List => reply_tree(windows, world, None),
        Cmd::Status(sr) => match sr.target {
            None => {
                // No target: the caller's subtree (whole tree for the operator).
                let root = if privileged { None } else { caller };
                reply_tree(windows, world, root)
            }
            Some(t) => {
                let candidates = ctl_candidates(windows);
                let id = match ctl::resolve_target(&t, &candidates) {
                    Ok(id) => id,
                    Err(e) => return ctl::reply_err(&e),
                };
                if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                    return deny;
                }
                let status = pane_by_agent(windows, id)
                    .and_then(|p| world.status_for(p.session_id.as_deref()).map(status_label));
                ctl::reply_status_one(id, status)
            }
        },
        Cmd::Send(sr) => {
            let candidates = ctl_candidates(windows);
            let id = match ctl::resolve_target(&sr.target, &candidates) {
                Ok(id) => id,
                Err(e) => return ctl::reply_err(&e),
            };
            if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                return deny;
            }
            // Queued iff the target is mid-turn now; either way delivery is async
            // and happens when the target is idle.
            let busy = matches!(
                pane_by_agent(windows, id).and_then(|p| world.status_for(p.session_id.as_deref())),
                Some(agsess::Status::Working) | Some(agsess::Status::WaitingApproval)
            );
            pending.push(PendingSend {
                target: id,
                text: sr.text,
                queued_at: Instant::now(),
                written: 0,
                text_written_at: None,
            });
            ctl::reply_sent(id, busy)
        }
        Cmd::Spawn(mut sp) => {
            // Capability check, FIRST. Creating teammates used to be implied by
            // having ctl access at all - so every agent in a fleet could do it,
            // and in a seven-agent review fleet all seven could, when only the
            // lead should. A depth cap bounds how FAR fan-out goes; it never says
            // who may start it.
            //
            // Before the depth cap and the trust ceiling, not instead of them: a
            // pane that may spawn is still capped on both.
            if let Some(c) = caller {
                let allowed = pane_by_agent(windows, c)
                    .map(|p| p.can_spawn)
                    .unwrap_or(false);
                if !allowed {
                    return ctl::reply_err(
                        "this agent is not permitted to create teammates \
                         (set \"can_spawn\": true for it in the fleet file)",
                    );
                }
            }
            // atrium owns the permission posture. Two layers, both surfaced (never
            // silent) in the reply `note`:
            //
            //  1. Strip RAW claude permission flags the agent slipped into the argv
            //     (`--dangerously-skip-permissions`, `--permission-mode`, …). Agents
            //     request a mode through `--mode`, not raw flags, so atrium stays the
            //     single source of truth.
            //  2. Resolve the effective mode from the per-spawn `--mode` under the
            //     session policy: the **operator** (the human's root pane) may set
            //     ANY mode (elevation is the human directing); a non-operator
            //     **worker** is capped at the policy — it may match or de-escalate
            //     but never elevate itself. No `--mode` ⇒ inherit the policy.
            // Vet before anything else: an unknown vendor or a flag a teammate
            // may not choose is refused outright, not quietly cleaned. Stripping
            // only ever covered three names, and the surface it guards — MCP
            // servers, plugin dirs, settings files, all of which execute code at
            // startup — is far larger than that.
            let stripped = match ctl::vet_spawn_argv(&sp.argv) {
                ctl::ArgvVerdict::Refused(why) => return ctl::reply_err(&why),
                ctl::ArgvVerdict::Ok { argv, stripped } => {
                    sp.argv = argv;
                    stripped
                }
            };
            let mut notes: Vec<String> = Vec::new();
            if !stripped.is_empty() {
                notes.push(format!(
                    "ignored raw {} — request a mode with --mode instead",
                    stripped.join(", ")
                ));
            }
            let policy = trust_mode();
            let (effective, cap_note) = effective_mode(sp.mode, policy);
            if let Some(n) = cap_note {
                notes.push(n);
            }
            let note = if notes.is_empty() {
                None
            } else {
                Some(notes.join("; "))
            };
            let note = note.as_deref();
            let caller_depth = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .map(|p| p.depth)
                .unwrap_or(0);
            let new_depth =
                match ctl::evaluate_spawn(&sp.argv, caller_depth, max_depth, extra_allow) {
                    Ok(d) => d,
                    Err(denied) => return ctl::reply_err(&denied.message()),
                };
            // Pane cap (host-resource guard): the depth guard bounds recursion; this
            // bounds total *breadth* so a runaway fan-out can't exhaust the machine
            // (agent processes dominate RAM, not atrium). Host-derived, overridable
            // with ATRIUM_MAX_PANES.
            let live = windows.iter().map(|w| w.panes.len()).sum::<usize>();
            let cap = atrium::resources::effective_cap();
            if live >= cap {
                return ctl::reply_err(&format!(
                    "pane cap reached ({live}/{cap}) — reap an agent or raise it with ATRIUM_MAX_PANES"
                ));
            }
            // Credential-delegation guard (§5): a worker may only pass down an
            // identity it itself holds (its own or the session default); the
            // operator delegates anything.
            let caller_identity = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .and_then(|p| p.identity.clone());
            if let Err(msg) = ctl::delegation_allowed(
                sp.identity.as_deref(),
                caller_identity.as_deref(),
                session_identity,
                privileged,
            ) {
                return ctl::reply_err(&msg);
            }
            if sp.new_window {
                spawn_worker_window(windows, &sp, caller, new_depth, rows, cols, effective, note)
            } else {
                spawn_worker_here(windows, &sp, caller, new_depth, rows, cols, effective, note)
            }
        }
        Cmd::Kill(kr) => {
            let candidates = ctl_candidates(windows);
            let id = match ctl::resolve_target(&kr.target, &candidates) {
                Ok(id) => id,
                Err(e) => return ctl::reply_err(&e),
            };
            if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                return deny;
            }
            // Tear down the target and every descendant. We only mark the panes
            // dead (kill the pty, set `exited`); the run loop's reap step (§4)
            // then collapses the split trees, drops the panes, and removes any
            // window left empty — the same proven path an interactive `x` uses.
            let parents = ctl_parents(windows);
            let mut killed: Vec<usize> = Vec::new();
            for w in windows.iter_mut() {
                for p in w.panes.iter_mut() {
                    if atrium::ctl::in_subtree(p.agent_id, id, &parents) {
                        let _ = p.pty.kill();
                        p.exited = true;
                        killed.push(p.agent_id);
                    }
                }
            }
            killed.sort_unstable();
            ctl::reply_killed(&killed)
        }
        // `audit` is handled in `apply_ctl` (it needs the log); never reaches here.
        Cmd::Audit(_) => ctl::reply_err("internal: audit dispatched to the wrong handler"),
        Cmd::Board(op) => {
            // The board is SHARED team state: any pane in the session reads and
            // writes the same source of truth (no subtree scoping — coordination
            // is the team's job, and `updated_by` + the audit log keep it
            // accountable). Single-writer daemon ⇒ no locking.
            let by = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .map(|p| {
                    p.role
                        .clone()
                        // 1-based to match the status bar's `1:`, `2:` numbering
                        // (agent_id is 0-based internally). Roles are the stable
                        // identity; this is the human-friendly fallback label.
                        .unwrap_or_else(|| format!("pane {}", p.agent_id + 1))
                })
                .or_else(|| Some("operator".to_string()));
            match op {
                ctl::BoardOp::Set { key, fields } => {
                    let e = board.set(&key, &fields, by.as_deref(), agsess::sessions::now_ms());
                    ctl::reply_board_entry(&key, Some(atrium::board::entry_to_value(&e)))
                }
                ctl::BoardOp::Get { key } => {
                    let entry = board.get(&key).map(atrium::board::entry_to_value);
                    ctl::reply_board_entry(&key, entry)
                }
                ctl::BoardOp::List => {
                    ctl::reply_board_list(atrium::board::entries_to_value(&board.list()))
                }
                ctl::BoardOp::Del { key } => {
                    let deleted = board.del(&key);
                    ctl::reply_board_del(&key, deleted)
                }
                ctl::BoardOp::Claim { key, ttl_ms } => {
                    // The owner is the caller (`by`), derived server-side — a
                    // worker can't claim as someone else. Default the lease to the
                    // board's DEFAULT_LEASE_MS unless the caller set --ttl.
                    let owner = by.as_deref().unwrap_or("operator");
                    let ttl = ttl_ms.unwrap_or(atrium::board::DEFAULT_LEASE_MS);
                    let outcome = board.claim(&key, owner, ttl, agsess::sessions::now_ms());
                    ctl::reply_board_claim(&key, &outcome)
                }
                ctl::BoardOp::Release { key } => {
                    let released = board.release(&key, agsess::sessions::now_ms());
                    ctl::reply_board_release(&key, released)
                }
            }
        }
        Cmd::Bus(op) => {
            // The bus is SHARED like the board. The server derives `who` (the
            // caller's role, else its pane, else the operator) so a worker
            // publishes and subscribes *as itself* — it cannot forge another
            // sender. `who` is stable per pane, so the no-echo filter and a
            // subscriber's cursor stay consistent across calls.
            let who = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .map(|p| {
                    p.role
                        .clone()
                        // 1-based to match the status bar's `1:`, `2:` numbering
                        // (agent_id is 0-based internally). Roles are the stable
                        // identity; this is the human-friendly fallback label.
                        .unwrap_or_else(|| format!("pane {}", p.agent_id + 1))
                })
                .unwrap_or_else(|| "operator".to_string());
            let now = agsess::sessions::now_ms();
            match op {
                ctl::BusOp::Pub {
                    topic,
                    kind,
                    fields,
                } => match bus.publish(&topic, kind, Some(&who), &fields, now) {
                    Ok(e) => ctl::reply_bus_published(atrium::bus::event_to_value(&e)),
                    Err(msg) => ctl::reply_err(&msg),
                },
                ctl::BusOp::Sub { topics } => {
                    bus.subscribe(&who, &topics);
                    ctl::reply_bus_subscribed(current_subs(bus, &who))
                }
                ctl::BusOp::Unsub { topics } => {
                    bus.unsubscribe(&who, &topics);
                    ctl::reply_bus_subscribed(current_subs(bus, &who))
                }
                ctl::BusOp::Feed { since } => {
                    let events = bus.feed(&who, since);
                    // The new cursor is the max seq pulled, or `since` when empty,
                    // so it never rewinds.
                    let cursor = events.iter().map(|e| e.seq).max().unwrap_or(since);
                    ctl::reply_bus_feed(atrium::bus::events_to_value(&events), cursor)
                }
                ctl::BusOp::Resolve { seq } => ctl::reply_bus_resolved(seq, bus.resolve(seq)),
            }
        }
    }
}

/// A subscriber's current topic set as a `Vec` (for the `sub`/`unsub` reply).
fn current_subs(bus: &atrium::bus::Bus, who: &str) -> Vec<String> {
    bus.subscriptions(who)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

/// Truncate a string to `n` chars (char-safe), appending `…` when cut. Keeps a
/// bad-request line short in the audit log.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// The audit `(action, detail)` for a request: a stable verb plus a compact,
/// **secret-free** description (identity *names* only; `send` logs the text
/// *length*, never the body).
fn audit_label(req: &atrium::ctl::Request) -> (&'static str, String) {
    use atrium::ctl::Cmd;
    match &req.cmd {
        Cmd::List => ("list", String::new()),
        Cmd::Status(sr) => (
            "status",
            match &sr.target {
                Some(t) => format!("target={t}"),
                None => "scope=subtree".to_string(),
            },
        ),
        Cmd::Send(sr) => (
            "send",
            format!("target={} len={}", sr.target, sr.text.chars().count()),
        ),
        Cmd::Spawn(sp) => {
            let stem = sp
                .argv
                .first()
                .map(|a| atrium::bind::command_stem(a))
                .unwrap_or_default();
            (
                "spawn",
                format!(
                    "role={} argv={} identity={}",
                    sp.role.as_deref().unwrap_or("-"),
                    stem,
                    sp.identity.as_deref().unwrap_or("-")
                ),
            )
        }
        Cmd::Kill(kr) => ("kill", format!("target={}", kr.target)),
        Cmd::Audit(_) => ("audit", String::new()),
        Cmd::Board(op) => (
            "board",
            match op {
                atrium::ctl::BoardOp::Set { key, fields } => {
                    format!("set {key} fields={}", fields.len())
                }
                atrium::ctl::BoardOp::Get { key } => format!("get {key}"),
                atrium::ctl::BoardOp::List => "list".to_string(),
                atrium::ctl::BoardOp::Del { key } => format!("del {key}"),
                atrium::ctl::BoardOp::Claim { key, .. } => format!("claim {key}"),
                atrium::ctl::BoardOp::Release { key } => format!("release {key}"),
            },
        ),
        Cmd::Bus(op) => (
            "bus",
            match op {
                atrium::ctl::BusOp::Pub {
                    topic,
                    kind,
                    fields,
                } => {
                    format!("pub {topic} {} fields={}", kind.as_str(), fields.len())
                }
                atrium::ctl::BusOp::Sub { topics } => format!("sub {}", topics.join(",")),
                atrium::ctl::BusOp::Unsub { topics } => format!("unsub {}", topics.join(",")),
                atrium::ctl::BusOp::Feed { since } => format!("feed since={since}"),
                atrium::ctl::BusOp::Resolve { seq } => format!("resolve {seq}"),
            },
        ),
    }
}

/// Read `(ok, note)` back out of a reply line for the audit record: the error on
/// failure, or a compact success tag naming the salient id(s).
fn audit_outcome(reply: &str) -> (bool, String) {
    let v = match json::parse(reply) {
        Ok(v) => v,
        Err(_) => return (false, "unparseable reply".to_string()),
    };
    let ok = v.get("ok").and_then(json::Value::as_bool).unwrap_or(false);
    if !ok {
        return (
            false,
            v.get("err")
                .and_then(json::Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
    }
    let note = if let Some(p) = v.get("pane").and_then(json::Value::as_i64) {
        format!("pane={p}")
    } else if let Some(k) = v.get("killed").and_then(json::Value::as_array) {
        let ids: Vec<String> = k
            .iter()
            .filter_map(json::Value::as_i64)
            .map(|n| n.to_string())
            .collect();
        format!("killed={}", ids.join(","))
    } else if let Some(t) = v.get("target").and_then(json::Value::as_i64) {
        format!("target={t}")
    } else {
        String::new()
    };
    (true, note)
}

/// Serve a `ctl audit` read: the in-memory log, subtree-scoped. The operator
/// sees everything; a worker sees only entries issued from within its own
/// subtree (operator/`None`-caller entries are hidden from it).
fn audit_reply(
    audit: &atrium::audit::Audit,
    windows: &[Window],
    caller: Option<usize>,
    privileged: bool,
    tail: Option<usize>,
) -> String {
    let entries = if privileged {
        audit.view(tail, |_| true)
    } else if let Some(root) = caller {
        let parents = ctl_parents(windows);
        audit.view(tail, |e| match e.caller {
            Some(c) => atrium::ctl::in_subtree(c, root, &parents),
            None => false, // operator actions are hidden from a worker
        })
    } else {
        // An UNAUTHENTICATED caller (no token, so `caller == None`) used to land
        // here, and this branch was byte-identical to the privileged one - so
        // holding no credential returned the entire fleet-wide audit trail,
        // including the operator actions the branch above deliberately hides
        // from a worker. That is the same "absent means most trusted" inversion
        // `privilege_for` was fixed to remove, left standing in one call site.
        Vec::new()
    };
    atrium::ctl::reply_audit(entries, audit.oldest_seq(), audit.latest_seq())
}

/// Serialize the spawn tree as a `list`/`status` reply. `root == Some(id)` limits
/// it to that pane's subtree (subtree-scoped status); `None` is the whole tree.
fn reply_tree(
    windows: &[Window],
    world: &atrium::vendors::VendorWorlds,
    root: Option<usize>,
) -> String {
    let parents = ctl_parents(windows);
    let mut panes: Vec<&Pane> = windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .filter(|p| match root {
            None => true,
            Some(r) => atrium::ctl::in_subtree(p.agent_id, r, &parents),
        })
        .collect();
    panes.sort_by_key(|p| p.agent_id);
    let nodes: Vec<atrium::ctl::TreeNode> = panes
        .iter()
        .map(|p| atrium::ctl::TreeNode {
            id: p.agent_id,
            parent: p.parent,
            role: p.role.as_deref(),
            title: &p.title,
            depth: p.depth,
            status: world.status_for(p.session_id.as_deref()).map(status_label),
        })
        .collect();
    atrium::ctl::reply_list(&nodes)
}

/// Subtree-scope guard: `None` if the caller may act on `target`, else a ready
/// JSON refusal. The operator (privileged) may act on anything.
fn scope_denied(
    windows: &[Window],
    caller: Option<usize>,
    privileged: bool,
    target: usize,
) -> Option<String> {
    if privileged {
        return None;
    }
    let root = caller?; // non-privileged implies a known caller pane
    if atrium::ctl::in_subtree(target, root, &ctl_parents(windows)) {
        None
    } else {
        Some(atrium::ctl::reply_err(&format!(
            "pane {target} is outside your subtree; a worker may only steer what it spawned"
        )))
    }
}

/// `ctl spawn` (default): a visible worker in a brand-new window.
#[allow(clippy::too_many_arguments)]
fn spawn_worker_window(
    windows: &mut Vec<Window>,
    sp: &atrium::ctl::SpawnReq,
    caller: Option<usize>,
    new_depth: usize,
    rows: u16,
    cols: u16,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
) -> String {
    let mut flash = None;
    match spawn_window(
        &sp.argv,
        rows,
        cols,
        windows.len(),
        sp.identity.as_deref(),
        mode,
        &mut flash,
    ) {
        Ok(mut w) => {
            let pane = &mut w.panes[0];
            pane.role = sp.role.clone();
            pane.parent = caller;
            pane.depth = new_depth;
            let agent_id = pane.agent_id;
            let session = pane.session_id.clone();
            windows.push(w);
            // Size the new window's pane to its true rect now, so the agent
            // paints full-height immediately instead of at the rough spawn size
            // (which otherwise needs a manual terminal resize to correct).
            let last = windows.len() - 1;
            resize_window(&mut windows[last], rows, cols);
            atrium::ctl::reply_spawned(agent_id, sp.role.as_deref(), session.as_deref(), note)
        }
        Err(e) => atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// `ctl spawn --here`: tile the worker *beside* the caller, in the caller's own
/// window, so a lead and its ICs sit in one view. Falls back to an error if the
/// caller's pane can't be located (nothing to sit beside).
#[allow(clippy::too_many_arguments)]
fn spawn_worker_here(
    windows: &mut [Window],
    sp: &atrium::ctl::SpawnReq,
    caller: Option<usize>,
    new_depth: usize,
    rows: u16,
    cols: u16,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
) -> String {
    let Some(caller_id) = caller else {
        return atrium::ctl::reply_err(
            "`--here` needs a caller pane; run it from inside an atrium pane",
        );
    };
    // Locate the window holding the caller and that caller's per-window pane id.
    let Some((wi, caller_pane_id)) = windows.iter().enumerate().find_map(|(i, w)| {
        w.panes
            .iter()
            .find(|p| p.agent_id == caller_id)
            .map(|p| (i, p.id))
    }) else {
        return atrium::ctl::reply_err("`--here`: caller pane not found (rerun without --here)");
    };

    let w = &mut windows[wi];
    let new_id = w.next_id;
    // Rough half-cell inner size; the caller resizes the window right after.
    let (pr, pc) = (
        (rows.saturating_sub(1).max(1) / 2).saturating_sub(2),
        (cols / 2).saturating_sub(2),
    );
    let mut flash = None;
    match spawn_pane(
        &sp.argv,
        pr.max(1),
        pc.max(1),
        new_id,
        sp.identity.as_deref(),
        mode,
        &mut flash,
    ) {
        Ok(mut pane) => {
            pane.role = sp.role.clone();
            pane.parent = caller;
            pane.depth = new_depth;
            let agent_id = pane.agent_id;
            let session = pane.session_id.clone();
            w.panes.push(pane);
            w.next_id += 1;
            // Re-tile the WHOLE window into a balanced near-square grid over all
            // its panes (keeping their stable ids), rather than just splitting the
            // caller side-by-side. A plain `split_pane` each time stacks every
            // `--here` worker into one column, so N of them degrade to an
            // unusable `1×N` strip; re-gridding keeps 2→1×2, 4→2×2, 6→2×3,
            // 12→3×4, … balanced. Focus lands on the fresh worker.
            let _ = caller_pane_id; // (kept for the error message above)
            let ids: Vec<usize> = w.panes.iter().map(|p| p.id).collect();
            w.tree = Tree::grid_from_ids(&ids);
            w.tree.focus_pane(new_id);
            w.zoomed = false;
            // Resize the whole window so both the caller and the fresh worker get
            // their exact inner rects — without this the worker keeps its rough
            // half-size and paints short (blank below), fixed only by a manual
            // terminal resize. Mirrors what the interactive split handlers do.
            resize_window(&mut windows[wi], rows, cols);
            atrium::ctl::reply_spawned(agent_id, sp.role.as_deref(), session.as_deref(), note)
        }
        Err(e) => atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// Flush queued `ctl send`s (Decision 4: queue until the target is idle). For
/// each pending send: once the target reports a ready status (or the unbound
/// fallback elapses), write the text; a beat later write the Enter and drop it.
/// A vanished target is dropped. Returns whether anything was written (so the
/// caller can request a repaint).
fn flush_sends(
    pending: &mut Vec<PendingSend>,
    windows: &mut [Window],
    world: &atrium::vendors::VendorWorlds,
) -> bool {
    if pending.is_empty() {
        return false;
    }
    let now = Instant::now();
    let mut wrote = false;
    pending.retain_mut(|ps| {
        match ps.text_written_at {
            None => {
                // Decide readiness from the target's live status.
                let ready = match pane_by_agent(windows, ps.target) {
                    None => return false, // target gone — drop the send
                    Some(p) => {
                        match world.status_for(p.session_id.as_deref()) {
                            Some(agsess::Status::WaitingPrompt) | Some(agsess::Status::Idle) => {
                                true
                            }
                            Some(_) => false, // Working / WaitingApproval — keep waiting
                            None => now.duration_since(ps.queued_at) >= SEND_UNBOUND_FALLBACK,
                        }
                    }
                };
                if ready {
                    if let Some(p) = pane_by_agent_mut(windows, ps.target) {
                        // Write from where we left off and advance by what was
                        // actually accepted. A short write is normal, not an
                        // error: the pty's buffer is finite. Resuming across
                        // ticks — rather than looping here until the whole thing
                        // lands — is deliberate, because this runs on the single
                        // event loop and a blocking write into a full buffer
                        // would freeze every pane until the target drained it.
                        let bytes = ps.text.as_bytes();
                        match p.pty.write(&bytes[ps.written..]) {
                            Ok(0) => {} // took nothing this tick; try the next
                            Ok(n) => {
                                ps.written += n;
                                wrote = true;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(_) => return false, // target unwritable — drop it
                        }
                        // Only now is the send real. Enter waits for the last byte.
                        if ps.written >= bytes.len() {
                            ps.text_written_at = Some(now);
                        }
                    } else {
                        return false;
                    }
                }
                true
            }
            Some(t) => {
                if now.duration_since(t) >= SEND_ENTER_DELAY {
                    if let Some(p) = pane_by_agent_mut(windows, ps.target) {
                        let _ = p.pty.write(b"\r");
                        wrote = true;
                    }
                    false // delivered — drop
                } else {
                    true
                }
            }
        }
    });
    wrote
}

fn spawn_window(
    command: &[String],
    rows: u16,
    cols: u16,
    _idx: usize,
    identity: Option<&str>,
    mode: atrium::ctl::TrustMode,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Window> {
    let pane = spawn_pane(
        command,
        rows.saturating_sub(1).max(1),
        cols,
        0,
        identity,
        mode,
        flash,
    )?;
    Ok(Window {
        panes: vec![pane],
        tree: Tree::new(0),
        zoomed: false,
        next_id: 1,
    })
}

/// Mass-spawn a single window of N tiles laid out as a balanced `grid`. The
/// split tree is built with [`layout::Tree::grid`] (ids `0..N`, focus 0) so
/// focus/rects/close all keep working exactly as for a hand-split grid. Each
/// tile runs the SAME command, each its own session (a fresh `--session-id` per
/// pane via the existing `bind` path inside `spawn_pane`), all under the same
/// `identity` if one was given. Panes are spawned at a rough grid-cell size; the
/// caller resizes the window right after so each pty/emulator gets its exact
/// inner rect. If any pane fails to spawn, the whole window is abandoned and the
/// already-spawned panes are killed (nothing to host half a grid).
fn spawn_window_grid(
    command: &[String],
    rows: u16,
    cols: u16,
    grid: atrium::spawn::Grid,
    identity: Option<&str>,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Window> {
    let tree = Tree::grid(grid.rows, grid.cols);
    let n = grid.total();
    // Rough per-cell inner size (minus the one-cell border on each side); the
    // caller's resize_window fixes it exactly right after.
    let cell_rows = ((rows.saturating_sub(1).max(1) as usize / grid.rows.max(1)).saturating_sub(2))
        .max(1) as u16;
    let cell_cols = ((cols as usize / grid.cols.max(1)).saturating_sub(2)).max(1) as u16;
    let mut panes: Vec<Pane> = Vec::with_capacity(n);
    for id in 0..n {
        match spawn_pane(
            command,
            cell_rows,
            cell_cols,
            id,
            identity,
            trust_mode(),
            flash,
        ) {
            Ok(pane) => panes.push(pane),
            Err(e) => {
                // Tear down whatever we already started — a partial grid is not
                // a coherent window.
                for p in panes.iter_mut() {
                    let _ = p.pty.kill();
                }
                return Err(e);
            }
        }
    }
    Ok(Window {
        panes,
        tree,
        zoomed: false,
        next_id: n,
    })
}

fn spawn_pane(
    command: &[String],
    rows: u16,
    cols: u16,
    id: usize,
    identity: Option<&str>,
    mode: atrium::ctl::TrustMode,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Pane> {
    // The single-pane / split / grid path: no per-pane working directory (the
    // child inherits atrium's cwd, today's behavior). The fleet loader is the only
    // caller that supplies a `cwd`; everyone else routes through here with `None`.
    spawn_pane_full(command, rows, cols, id, identity, None, mode, flash)
}

/// The non-secret environment every pane is born with.
///
/// Split out from [`spawn_pane_full`] so the one rule that matters here can be
/// asserted directly: **the session marker does not depend on ctl.** It used to,
/// and that was the first of the two holes that stranded 75 processes holding
/// 526 ptys against a machine-wide limit of 511. `ATRIUM_CTL`/`ATRIUM_PANE`/
/// `ATRIUM_TOKEN` were pushed inside `if let (Some(addr), Some(token)) = …`, so a
/// pane launched without `--allow-ctl` — the default — carried no atrium
/// environment whatsoever. Nothing on the process said it was ours, so once atrium
/// was gone nothing could find it, and they had to be cleared by hand.
///
/// `ATRIUM_SESSION` is deliberately NOT one of those credentials. `ATRIUM_TOKEN` is
/// a capability that must not leak; this is a label that is *meant* to be read —
/// by the watchdog, by `atrium reap`, by a human with `ps`. Holding it authorises
/// nothing, and forging it can only nominate the forger's own process group for
/// collection, and then only once the atrium it names is already dead.
fn pane_base_env(
    session: Option<&atrium::orphan::SessionKey>,
    ctl: Option<(&str, &str, &str)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(key) = session {
        env.push((atrium::orphan::ENV_SESSION.to_string(), key.encode()));
    }
    if let Some((addr, pane, token)) = ctl {
        env.push((atrium::ctl::ENV_ADDRESS.to_string(), addr.to_string()));
        env.push((atrium::ctl::ENV_PANE.to_string(), pane.to_string()));
        env.push((atrium::ctl::ENV_TOKEN.to_string(), token.to_string()));
    }
    env
}

/// The shared spawn core: build the agent's launch (session-id inject, Windows
/// shim wrapping, identity env resolution), then spawn it on a pty of the given
/// size in the given working directory. Every pane atrium hosts — the initial one,
/// a split, a grid tile, and each fleet agent — is born here, so the identity /
/// `--session-id` / effective-command discipline is written once and shared.
///
/// `command` is the *base* user command with any extra agent args already
/// appended (e.g. the fleet loader's `--add-dir` / `--append-system-prompt` /
/// `--model` / `--effort`); this function then appends `--session-id` when the
/// pane is a bindable agent, exactly as before. `cwd` is `Some(dir)` for a fleet
/// agent (so its `CLAUDE.md` auto-loads) and `None` everywhere else.
#[allow(clippy::too_many_arguments)]
fn spawn_pane_full(
    command: &[String],
    rows: u16,
    cols: u16,
    id: usize,
    identity: Option<&str>,
    cwd: Option<&str>,
    mode: atrium::ctl::TrustMode,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Pane> {
    // Cross-platform stem: split on `/` and `\` on every OS so a Windows-authored
    // fleet command (e.g. `C:\tools\claude.cmd`) is recognized as an agent on
    // macOS/Linux too — `Path::file_stem` would keep the backslashes there. This
    // title feeds is_claude/is_codex/vendor_tag, so the fix must live here.
    let title = atrium::bind::command_stem(&command[0]);
    // Permission posture for an agent pane (never a shell pane):
    //   Edits (`--trust`)          → `--permission-mode acceptEdits` + a safe
    //                                dev-command allowlist; dangerous commands
    //                                still prompt, visibly, in the pane.
    //   Auto (`automode`)          → `--permission-mode auto` (claude's auto mode:
    //                                hands-off edits + commands, its own guardrails).
    //   Skip (`skip`)              → `--dangerously-skip-permissions` (full bypass;
    //                                the human confirmed it at launch).
    //   Plan (`plan`)              → `--permission-mode plan` (read-only).
    // `mode` is the *effective* mode for this pane: the session policy for the
    // panes atrium opens itself, or — for a ctl spawn — the per-spawn `--mode` after
    // the operator-elevate / worker-cap governance in `apply_ctl`.
    // `--session-id`, `--append-system-prompt`, and the ~/.claude.json folder-trust
    // gate below are all Claude Code CLI specifics — injecting them into another
    // vendor would break its launch — so the narrow `is_claude` test gates them.
    // The **trust posture** now maps per vendor: claude gets its `--permission-mode`
    // flags, codex gets its own `--ask-for-approval`/`--sandbox` flags (§`--trust`
    // is vendor-aware). Any other agent still launches with its command untouched.
    // The broad `is_agent_stem` continues to govern vendor-neutral treatment like
    // the identity-env decision in `wants_env` below.
    let is_claude = atrium::bind::is_claude_stem(&title);
    let is_codex = atrium::vendors::vendor_for_stem(&title) == Some(agsess::Vendor::Codex);
    let trusted_launch = (is_claude || is_codex) && mode != atrium::ctl::TrustMode::Off;
    let mut base: Vec<String> = if trusted_launch {
        let mut v = command.to_vec();
        if is_claude {
            match mode {
                atrium::ctl::TrustMode::Edits => {
                    v.extend(atrium::trust::accept_edits_args(
                        &atrium::trust::extra_allow_from_env(),
                    ));
                }
                atrium::ctl::TrustMode::Auto => {
                    v.push("--permission-mode".to_string());
                    v.push("auto".to_string());
                }
                atrium::ctl::TrustMode::Skip => {
                    v.push(atrium::ctl::SKIP_PERMISSIONS_FLAG.to_string());
                }
                atrium::ctl::TrustMode::Plan => {
                    v.push("--permission-mode".to_string());
                    v.push("plan".to_string());
                }
                atrium::ctl::TrustMode::Off => unreachable!("trusted_launch implies not Off"),
            }
        } else if is_codex {
            // codex's approval/sandbox flags — its analog of the above. Guarded by
            // `is_codex` (not a bare `else`) so that if the claude/codex stem sets
            // ever overlap or a third vendor is added, codex flags reach ONLY a
            // codex pane — never silently some other agent's command.
            v.extend(atrium::trust::codex_trust_args(mode));
        }
        v
    } else {
        command.to_vec()
    };
    // When the ctl channel is live, teach every agent pane — the initial one and
    // ctl-spawned workers alike — to delegate through `atrium ctl` (visible panes)
    // instead of its own invisible Task/background-agents tool. This is the
    // dependable layer: it is always in the agent's context (via
    // `--append-system-prompt`), so reliable delegation no longer hinges on a
    // skill happening to surface. Appended after any trust flags so it terminates
    // the `--allowedTools` list cleanly rather than being read as one of its values.
    if is_claude && CTL_ADDRESS.get().is_some() {
        base.push("--append-system-prompt".to_string());
        base.push(atrium::ctl::AGENT_CTL_DIRECTIVE.to_string());
    }
    // …and pre-accept claude's *folder-trust* dialog for this pane's working
    // directory — a separate gate the permission mode does NOT cover (it's stored
    // per-dir in ~/.claude.json). Without this a trusted launch in an untrusted
    // folder still blocks on "trust this folder?". claude-only: codex has its own
    // per-folder trust in ~/.codex/config.toml (TOML — a follow-up; see
    // `trust::codex_trust_args`). Only under --trust/--skip, only the trust bit,
    // only this pane's cwd; a parse/IO problem is flashed and the pane spawns anyway.
    if trusted_launch && is_claude {
        let dir = cwd
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        if let Err(e) = atrium::trust::ensure_trusted(&dir) {
            *flash = Some((format!("folder-trust: {e}"), Instant::now()));
        }
    }
    // Agent-aware bind (§3.3): if this is an agent pane atrium is launching and the
    // user did not already pick a session, mint a uuid and append
    // `--session-id <uuid>` to the *agent's* args (before any `cmd /C` shim
    // wrapping, so the flag reaches claude, not the shim host). Remember the id.
    let session_id = atrium::bind::session_id_for(&base);
    let mut user_cmd: Vec<String> = base;
    if let Some(uuid) = &session_id {
        user_cmd.push("--session-id".to_string());
        user_cmd.push(uuid.clone());
    }
    let effective = effective_command(&user_cmd);
    // Opt-in spawn diagnostic: `ATRIUM_SPAWN_LOG=<file>` appends the exact command
    // (post trust-flags, post shim) atrium launches for each pane, so a "why isn't
    // this pane in the mode I expected" question is answered by data, not guesses.
    if let Ok(path) = std::env::var("ATRIUM_SPAWN_LOG") {
        if !path.is_empty() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(f, "[{title}] {}", effective.join(" "));
            }
        }
    }
    let argrefs: Vec<&str> = effective[1..].iter().map(String::as_str).collect();
    let r = rows.max(1);
    let c = cols.max(1);

    // Stamp the process-global agent id now — it is both the pane's spawn-tree
    // key and the `ATRIUM_PANE` value injected below, so an agent inside can
    // attribute its own `ctl spawn` calls back to this pane.
    let agent_id = next_agent_id();

    // ctl env (non-secret): when the control channel is on, every pane learns
    // the endpoint (`ATRIUM_CTL`) and its own id (`ATRIUM_PANE`). This is the base
    // env; identity secrets (if any) are merged on top for this one spawn.
    // A per-pane capability token: 32 bytes of OS CSPRNG entropy (hex), so a
    // sibling pane cannot guess or brute-force it. Injected as ATRIUM_TOKEN and
    // stored on the pane; the ctl server authenticates a request by matching it.
    // Peer-credential binding at the ipc layer (same-user endpoint) is the second
    // gate underneath this token — see `ipc.rs`.
    // No token means OS entropy failed. Fail CLOSED: this pane gets no ctl
    // environment at all, so it simply has no control-plane access. The old
    // behaviour minted a guessable token from non-crypto ids and injected it
    // anyway, which is strictly worse — a sibling pane could then forge it.
    // `\r\n` because atrium is in raw mode here.
    let token = atrium::uid::token();
    if token.is_none() {
        eprint!(
            "atrium: warning: OS entropy unavailable; this pane is starting WITHOUT \
             ctl access rather than with a guessable capability token\r\n"
        );
    }
    let pane_id = agent_id.to_string();
    let base_env = pane_base_env(
        atrium::orphan::session_key().as_ref(),
        match (CTL_ADDRESS.get(), token.as_ref()) {
            (Some(addr), Some(tok)) => Some((addr.as_str(), pane_id.as_str(), tok.as_str())),
            _ => None,
        },
    );

    // Identity injection (path B): only for an agent pane with an identity set.
    // Decide ONCE so the spawn path and the pane's stored tag can never diverge
    // — a pane tagged with an identity is exactly a pane spawned with its env.
    let inject = atrium::identity::wants_env(command, identity);

    // We RE-RESOLVE on every spawn — the resolved env (which contains secret
    // material) lives only in this local, is handed straight to the pty, and is
    // dropped at the end of this function. It is never cached on the pane,
    // never logged, never printed. The pane stores only the identity *name*.
    //
    // Resolve failure (no such target, vault locked) is surfaced in the bar and
    // the pane spawns with plain env (ambient creds) but still in `cwd` — visible,
    // not silent, and never unauthenticated-without-saying-so (§7).
    let pty = if inject {
        let name = identity.expect("wants_env implies Some");
        // An identity may be a comma-separated list (`work,hf`) so one agent gets
        // several keys at once — each resolves to its own env var(s) and they are
        // merged. A later entry that maps to the same var wins. The resolved env
        // holds secret values: merged with the (non-secret) ctl base env for this
        // one spawn, then dropped — never formatted, logged, or stored.
        let mut merged = base_env.clone();
        let mut failed: Vec<String> = Vec::new();
        for part in name.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match akey::resolve(part) {
                Ok(env) => merged.extend(env),
                // Names only in the message (akey's error carries the typed name,
                // never a secret); one bad name doesn't sink the others.
                Err(_) => failed.push(part.to_string()),
            }
        }
        if !failed.is_empty() {
            *flash = Some((
                format!(
                    "identity {:?} unresolved — running without {}",
                    failed.join(","),
                    if merged.len() == base_env.len() {
                        "credentials"
                    } else {
                        "those"
                    }
                ),
                Instant::now(),
            ));
        }
        pty::Pty::spawn_full(&effective[0], &argrefs, r, c, &merged, cwd)?
    } else {
        pty::Pty::spawn_full(&effective[0], &argrefs, r, c, &base_env, cwd)?
    };
    // The second marker, on disk. Measured on macOS 26: `ps -E` prints the
    // environment of ordinary binaries but NOTHING for a SIP/platform binary,
    // and `/bin/sh` — what a default pane runs, and the exact population that
    // stranded — is one of those. So the env marker alone is blind to shells
    // here; the stamp covers every pane. It records the pane's own start token,
    // so a stale file cannot become a kill order against a recycled pid.
    atrium::orphan::stamp(pty.pid());
    Ok(Pane {
        id,
        pty,
        term: vterm::Term::new(r as usize, c as usize),
        filter: atrium::filter::Passthrough::new(),
        title,
        activity: false,
        exited: false,
        session_id,
        launch_ms: agsess::sessions::now_ms(),
        cwd: cwd.map(str::to_string),
        // Store the identity NAME only, and only for panes that would actually
        // run under it (agent panes) — a shell keeps `None`, so no misleading
        // tag on a pane that was never credentialed. `inject` is the same
        // decision the spawn used, so tag and env can never disagree.
        identity: identity.filter(|_| inject).map(str::to_string),
        agent_id,
        token,
        // Spawn-tree fields default to "root pane the human opened"; the ctl
        // spawn handler overrides role/parent/depth for a ctl-created worker.
        role: None,
        parent: None,
        depth: 0,
        can_spawn: true,
        painted: false,
        mouse_wanted: false,
    })
}

/// Sanitize the terminal on exit and leave the alt screen. atrium owns the alt
/// buffer (§ the run loop's `\x1b[?1049h` at start), but a hosted app may have
/// left modes on — mouse reporting, bracketed paste, a hidden cursor, an
/// altered scroll region, a non-default SGR. Leaving the alt screen alone does
/// not undo those, so we reset them first, in a sensible order, before the
/// `?1049l` swap so the user's original shell comes back clean. Called on
/// **every** exit path in `run` (normal quit, last-pane-exit, read error, and
/// the initial-spawn failure).
/// Every mouse-reporting mode atrium ever turns off, in one place.
///
/// This existed twice with DIFFERENT contents: the per-tick suppressor sent
/// `?1015l` and `cleanup_screen` did not. So a hosted app that enabled
/// urxvt-style reporting (1015) left the user's real shell emitting escape
/// garbage on every click after atrium exited — the one mode the exit path forgot.
/// Two lists that must agree will not stay in agreement; there is now one.
const MOUSE_OFF: &str = "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l";

fn cleanup_screen(out: &mut impl Write) {
    let _ = write!(
        out,
        // Reset SGR; disable every mouse mode; disable bracketed paste (2004);
        // show the cursor (25); reset the scroll region; then leave the alt
        // screen (1049) last so the swap-back is the final act.
        "\x1b[0m{MOUSE_OFF}\x1b[?2004l\x1b[?25h\x1b[r\x1b[?1049l"
    );
    let _ = out.flush();
}

/// Diagnostic mode: print the hex of every raw byte stdin delivers for a few
/// seconds. Answers "what does my terminal actually send?" — including through
/// nested consoles — without guessing.
fn stdin_probe() -> ExitCode {
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("atrium: not a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = std::io::stdout();
    let _ = writeln!(out, "probe: type for 3s\r");
    let _ = out.flush();
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        if let Ok(bytes) = term.read_bytes(Duration::from_millis(100)) {
            if !bytes.is_empty() {
                let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
                let _ = writeln!(out, "got[{}]\r", hex.join(" "));
                let _ = out.flush();
            }
        }
    }
    let _ = writeln!(out, "probe-done\r");
    let _ = out.flush();
    ExitCode::SUCCESS
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".into())
    }
}

/// The outcome of feeding a keystroke chunk to the open command prompt.
enum PromptEdit {
    /// The line changed (or nothing happened); keep the prompt open.
    Continue,
    /// Esc / Ctrl+C: close the prompt without running anything.
    Cancel,
    /// Enter: the caller runs the accumulated line.
    Submit,
}

/// Apply a chunk of raw input `bytes` to the prompt line `buf`: printable ASCII is
/// appended, Backspace deletes, Enter submits, Esc/Ctrl+C cancels. Other control
/// bytes (including the rest of an arrow-key escape) are ignored. Returns as soon
/// as a terminating key (Enter/Esc) is seen so trailing bytes don't leak.
fn edit_prompt(buf: &mut String, bytes: &[u8]) -> PromptEdit {
    for &b in bytes {
        match b {
            b'\r' | b'\n' => return PromptEdit::Submit,
            0x1b | 0x03 => return PromptEdit::Cancel, // Esc or Ctrl+C
            0x7f | 0x08 => {
                buf.pop();
            }
            0x20..=0x7e => buf.push(b as char),
            _ => {} // ignore other control bytes
        }
    }
    PromptEdit::Continue
}

/// Split a command line into argv, honoring double quotes so a path with spaces
/// stays one argument (`"C:\Program Files\Git\bin\bash.exe" --login`). Whitespace
/// separates unquoted words; quotes are removed. Minimal by design — enough to
/// launch a shell with a flag, not a full shell parser.
fn split_cmdline(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has = false;
    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// On Windows, resolve the command the way the shell would (PATH x PATHEXT) and
/// host `.cmd`/`.bat` shims under `cmd /C` — npm-installed CLIs (Claude Code
/// included) are such shims, and `CreateProcessW` cannot launch them directly.
#[cfg(windows)]
fn effective_command(command: &[String]) -> Vec<String> {
    use atrium::resolve;
    let dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect();
    match resolve::resolve(&command[0], &dirs, &exts) {
        Some(path) if resolve::needs_shell(&path) => {
            let mut v = vec!["cmd".to_string(), "/C".to_string()];
            v.push(path.to_string_lossy().into_owned());
            v.extend(command[1..].iter().cloned());
            v
        }
        Some(path) => {
            let mut v = vec![path.to_string_lossy().into_owned()];
            v.extend(command[1..].iter().cloned());
            v
        }
        None => command.to_vec(),
    }
}

#[cfg(not(windows))]
fn effective_command(command: &[String]) -> Vec<String> {
    command.to_vec()
}

#[cfg(test)]
mod tests {
    use super::{pane_base_env, privilege_for};

    /// **A pane must carry the session marker whether or not ctl is on.**
    ///
    /// The marker used to be pushed inside the ctl block, so the DEFAULT launch
    /// (no `--allow-ctl`) produced a pane with no atrium environment at all —
    /// nothing identified the process as ours, so nothing could ever find it
    /// again. Move the `ATRIUM_SESSION` push back inside the `if let Some((addr,
    /// …)) = ctl` arm and the first assertion here fails, which is exactly the
    /// shipped bug.
    #[test]
    fn every_pane_is_marked_even_with_no_control_channel() {
        let key = atrium::orphan::SessionKey {
            owner: 17686,
            started: 99,
        };
        let bare = pane_base_env(Some(&key), None);
        assert_eq!(
            bare,
            vec![("ATRIUM_SESSION".to_string(), "17686:99".to_string())],
            "a pane without ctl must still be findable"
        );

        // With ctl on, the marker is still there, still first, and the three ctl
        // variables are unchanged.
        let full = pane_base_env(Some(&key), Some(("/tmp/sock", "3", "deadbeef")));
        let names: Vec<&str> = full.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "ATRIUM_SESSION",
                "ATRIUM_CTL",
                "ATRIUM_PANE",
                "ATRIUM_TOKEN"
            ]
        );
        assert_eq!(full[3].1, "deadbeef");
    }

    /// No key (Windows, where the Job Object makes the whole sweep unnecessary)
    /// means no marker — never a bare pid, which would be a marker that points at
    /// a live unrelated process after the pid is reused.
    #[test]
    fn a_pane_is_never_marked_with_a_key_we_could_not_compute() {
        assert!(pane_base_env(None, None).is_empty());
        let ctl_only = pane_base_env(None, Some(("/tmp/sock", "3", "deadbeef")));
        assert!(!ctl_only.iter().any(|(k, _)| k == "ATRIUM_SESSION"));
        assert_eq!(ctl_only.len(), 3);
    }

    /// **Dropping your token must not promote you** (review #1).
    ///
    /// `caller` is derived from the capability token and is never self-reported,
    /// so `None` means exactly "unauthenticated". This arm returned `true`, which
    /// inverted the gate: holding NO credential granted strictly more than
    /// holding a worker's, so `env -u ATRIUM_TOKEN atrium ctl ...` promoted you.
    ///
    /// This fails against the old code, which is the point — an earlier attempt
    /// at an integration test for the same fix passed with the fix REVERTED,
    /// because the elevated spawn was refused earlier by policy and never reached
    /// this decision at all.
    #[test]
    fn an_unauthenticated_caller_is_never_the_operator() {
        assert!(!privilege_for(None, None));
        // Even if a pane were somehow associated, no token means no authority.
        assert!(!privilege_for(None, Some(None)));
    }

    /// The child is launched with the paths that were DISCLOSED, not with the
    /// file's strings resolved a second time.
    ///
    /// Reverting this (mapping `Grant::raw` back through `resolve_dir`) is the
    /// change that reopens the drift between the banner and the launch, and it
    /// is invisible to every unit test of the classifier itself - the
    /// classification stays correct while the spawn ignores it.
    #[test]
    fn a_fleet_agent_is_launched_with_the_paths_that_were_disclosed() {
        use atrium::fleet::{Agent, AgentPlan, Field, Grant, Reach};
        let g = |raw: &str, given: &str, field| Grant {
            field,
            raw: raw.to_string(),
            given: std::path::PathBuf::from(given),
            real: Some(std::path::PathBuf::from(given)),
            exists: true,
            reach: Reach::Outside,
            holds: None,
        };
        let agent = Agent {
            name: "lead\u{1b}[2J".to_string(),
            cmd: vec!["claude".to_string()],
            cwd: Some("./work".to_string()),
            add_dirs: vec!["./ctx".to_string()],
            ..Default::default()
        };
        let disclosed = AgentPlan {
            label: "lead".to_string(),
            identity: None,
            opaque: None,
            cwd: Some(g("./work", "/real/work", Field::Cwd)),
            add_dirs: vec![g("./ctx", "/real/ctx", Field::AddDir)],
        };
        let (argv, cwd, role) = super::fleet_launch(&agent, &disclosed);
        assert_eq!(
            cwd.as_deref(),
            Some("/real/work"),
            "cwd must be the resolved one"
        );
        assert_eq!(argv, vec!["claude", "--add-dir", "/real/ctx"]);
        // The pane label is the DEFANGED one from the plan, not the raw name:
        // `role` is painted into the status bar and the overview unfiltered.
        assert_eq!(role, "lead");
        assert!(!role.chars().any(|c| c.is_control()));
    }

    /// A token that matches no live pane is stale or forged — not the operator.
    #[test]
    fn a_token_resolving_to_no_live_pane_is_not_the_operator() {
        assert!(!privilege_for(Some(7), None));
    }

    /// **The session policy is a ceiling for everyone** (review #1, remainder).
    ///
    /// A "privileged" caller used to bypass the cap entirely. But `-n N` gives
    /// every pane no parent, so every pane in a mass-spawned session counted as
    /// the operator — and could ask for `skip`, a full permission bypass, in a
    /// session the human had set to `plan`.
    #[test]
    fn the_session_policy_caps_every_request() {
        use atrium::ctl::TrustMode;
        // Escalation is refused and explained, whoever asks.
        let (mode, note) = super::effective_mode(Some(TrustMode::Skip), TrustMode::Plan);
        assert_eq!(mode, TrustMode::Plan, "skip escaped a plan-mode session");
        assert!(note
            .expect("a capped request must say so")
            .contains("capped"));
    }

    /// De-escalation stays free: asking for LESS than the policy is always fine,
    /// and is the useful half of `--mode`.
    #[test]
    fn a_request_below_the_policy_is_honoured() {
        use atrium::ctl::TrustMode;
        let (mode, note) = super::effective_mode(Some(TrustMode::Plan), TrustMode::Skip);
        assert_eq!(mode, TrustMode::Plan);
        assert!(note.is_none(), "de-escalation should not be flagged");
        // No request at all inherits the policy.
        let (mode, note) = super::effective_mode(None, TrustMode::Edits);
        assert_eq!(mode, TrustMode::Edits);
        assert!(note.is_none());
    }

    /// The intended rule, unchanged: a root pane is the operator, a spawned
    /// worker is not. (Whether `-n N` should make every pane a root pane is a
    /// separate, still-open question — see review finding #1's remainder.)
    #[test]
    fn a_root_pane_is_the_operator_and_a_worker_is_not() {
        assert!(privilege_for(Some(1), Some(None)));
        assert!(!privilege_for(Some(2), Some(Some(1))));
    }

    use super::*;

    /// Strip CSI escapes so a rendered panel can be asserted on its glyphs.
    fn strip_csi(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                if it.peek() == Some(&'[') {
                    it.next();
                    for c2 in it.by_ref() {
                        if c2.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// A minimal OverviewNode for aggregation tests.
    fn ov_node(window: usize, status: Option<agsess::Status>, exited: bool) -> OverviewNode {
        OverviewNode {
            window,
            pane_id: 0,
            label: "a".into(),
            depth: 0,
            status,
            identity: None,
            exited,
            action: String::new(),
            is_agent: true,
            vtag: "",
        }
    }

    #[test]
    fn window_agg_counts_by_status() {
        use agsess::Status::*;
        let nodes = vec![
            ov_node(0, Some(Working), false),
            ov_node(0, Some(WaitingApproval), false),
            ov_node(0, Some(Idle), false),
            ov_node(0, None, false),          // unbound → idle bucket
            ov_node(0, Some(Working), true),  // exited wins over status
            ov_node(1, Some(Working), false), // other window — excluded
        ];
        let a = window_agg(&nodes, 0);
        assert_eq!(a.total, 5);
        assert_eq!(a.working, 1);
        assert_eq!(a.waiting, 1);
        assert_eq!(a.idle, 2);
        assert_eq!(a.exited, 1);
    }

    #[test]
    fn overview_rows_single_window_has_no_header() {
        let nodes = vec![ov_node(0, None, false), ov_node(0, None, false)];
        let rows = overview_rows(&nodes);
        // Two agents, no group header (global counts already cover one fleet).
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| matches!(r, OvRow::Agent(_))));
    }

    #[test]
    fn overview_rows_multi_window_inserts_one_header_per_window() {
        let nodes = vec![
            ov_node(0, None, false),
            ov_node(0, None, false),
            ov_node(1, None, false),
        ];
        let rows = overview_rows(&nodes);
        // Header(w0), Agent0, Agent1, Header(w1), Agent2 = 5 rows.
        assert_eq!(rows.len(), 5);
        let headers: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                OvRow::Header(a) => Some(a.window),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec![0, 1], "one header per window, in order");
        // Agent rows still carry their original node indices, in order.
        let agents: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                OvRow::Agent(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(agents, vec![0, 1, 2]);
    }

    #[test]
    fn overview_scroll_start_keeps_selection_visible_and_fills() {
        // Fits entirely → no scroll.
        assert_eq!(overview_scroll_start(5, 4, 10), 0);
        // Selection near the top → start at 0.
        assert_eq!(overview_scroll_start(20, 2, 5), 0);
        // Selection deep → pinned to the bottom edge (sel at last visible row).
        assert_eq!(overview_scroll_start(20, 12, 5), 8);
        // Selection at the very end → clamp so the viewport is filled, not overshot.
        assert_eq!(overview_scroll_start(20, 19, 5), 15);
        // Degenerate list_rows.
        assert_eq!(overview_scroll_start(20, 19, 0), 0);
    }

    #[test]
    fn overview_panel_shows_window_headers_only_when_multiple_fleets() {
        let bus = atrium::bus::Bus::new();
        // One window: no "window 1" group header.
        let one = vec![ov_node(0, None, false), ov_node(0, None, false)];
        let r1 = strip_csi(&render_overview_panel(&[], &bus, &one, 0, 24, 100));
        assert!(
            !r1.contains("window 1"),
            "single fleet must not show a group header"
        );
        // Two windows: both group headers appear.
        let two = vec![ov_node(0, None, false), ov_node(1, None, false)];
        let r2 = strip_csi(&render_overview_panel(&[], &bus, &two, 0, 24, 100));
        assert!(r2.contains("window 1"), "multi-fleet must group window 1");
        assert!(r2.contains("window 2"), "multi-fleet must group window 2");
    }

    #[test]
    fn feed_scroll_pages_back_through_history() {
        // Six FYI events, a 3-row feed. One row is the scroll hint, so two events
        // show at a time; scroll offsets the window toward older events.
        let mut bus = atrium::bus::Bus::new();
        for i in 1..=6u64 {
            bus.publish(
                "t",
                atrium::bus::Kind::Fyi,
                Some("w"),
                &[("msg".to_string(), format!("e{i}"))],
                i,
            )
            .unwrap();
        }
        let render = |scroll| {
            let mut out = String::new();
            render_fyi_feed(&mut out, &bus, &[], 1, 3, scroll);
            strip_csi(&out)
        };
        // Live (scroll 0): the two newest, not the oldest.
        let top = render(0);
        assert!(top.contains("e6") && top.contains("e5"), "scroll 0: {top}");
        assert!(!top.contains("e1"), "oldest hidden at scroll 0: {top}");
        // Scrolled back two: the window slides to older events.
        let back = render(2);
        assert!(
            back.contains("e4") && back.contains("e3"),
            "scroll 2: {back}"
        );
        assert!(
            !back.contains("e6"),
            "newest scrolled off at scroll 2: {back}"
        );
    }

    #[test]
    fn wrap_to_wraps_within_width_and_marks_overflow() {
        // Fits: two short lines, each within the width.
        let w = wrap_to("alpha beta gamma delta", 11, 3);
        assert!(
            w.iter().all(|l| l.chars().count() <= 11),
            "within width: {w:?}"
        );
        assert_eq!(w.join(" "), "alpha beta gamma delta");
        // Overflows the line budget: the last line ends with an ellipsis.
        let long = "aaa bbb ccc ddd eee fff ggg hhh iii jjj";
        let w2 = wrap_to(long, 7, 2);
        assert_eq!(w2.len(), 2);
        assert!(w2.last().unwrap().ends_with('…'), "overflow marked: {w2:?}");
    }

    #[test]
    fn decision_detail_shows_the_full_question() {
        // The panel line can truncate; the detail bar must show the whole question.
        let mut bus = atrium::bus::Bus::new();
        let q = "Should the initial release be tagged 0.1.0 as a pre-release alpha or 1.0.0 as the first stable release";
        bus.publish(
            "ui",
            atrium::bus::Kind::DecisionNeeded,
            Some("grace"),
            &[("q".to_string(), q.to_string())],
            1,
        )
        .unwrap();
        let decisions = bus.pending_decisions();
        let mut out = String::new();
        render_decision_detail(&mut out, decisions.first(), 24, 80);
        let flat: String = strip_csi(&out)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(flat.contains(q), "full question visible: {flat}");
        assert!(
            flat.contains("#1") && flat.contains("grace"),
            "seq + source shown: {flat}"
        );
    }

    #[test]
    fn ctl_read_ops_are_open_mutations_require_auth() {
        use atrium::ctl::{BoardOp, Cmd, KillReq};
        // Reads are servable to an unauthenticated caller…
        assert!(ctl_is_read_only(&Cmd::List));
        assert!(ctl_is_read_only(&Cmd::Board(BoardOp::List)));
        assert!(ctl_is_read_only(&Cmd::Board(BoardOp::Get {
            key: "auth".into()
        })));
        // …while anything that mutates the fleet or shared state is gated.
        assert!(!ctl_is_read_only(&Cmd::Kill(KillReq {
            target: "w".into()
        })));
        assert!(!ctl_is_read_only(&Cmd::Board(BoardOp::Set {
            key: "t".into(),
            fields: vec![],
        })));
        assert!(!ctl_is_read_only(&Cmd::Board(BoardOp::Claim {
            key: "t".into(),
            ttl_ms: None,
        })));
    }

    #[test]
    fn ago_formats_coarsely() {
        assert_eq!(ago(10_000, 5_000), "5s");
        assert_eq!(ago(200_000, 20_000), "3m");
        assert_eq!(ago(10_000_000, 2_800_000), "2h");
        assert_eq!(ago(200_000_000, 200_000), "2d");
    }

    #[test]
    fn collect_log_merges_bus_and_board_in_time_order() {
        let mut bus = atrium::bus::Bus::new();
        bus.publish(
            "ui",
            atrium::bus::Kind::Fyi,
            Some("ada"),
            &[("msg".to_string(), "hi".to_string())],
            300,
        )
        .unwrap();
        let mut board = atrium::board::Board::new();
        board.set(
            "title",
            &[("status".to_string(), "done".to_string())],
            Some("lead"),
            100,
        );
        let world = atrium::vendors::VendorWorlds::new(); // no sessions (unrefreshed)
        let rows = collect_log(&[], &world, &board, &bus);
        assert!(rows.len() >= 2, "bus + board rows present");
        assert!(
            rows.windows(2).all(|w| w[0].ts <= w[1].ts),
            "sorted ascending by ts"
        );
        assert!(rows
            .iter()
            .any(|r| r.who == "lead" && r.text.contains("title")));
        assert!(rows.iter().any(|r| r.who == "ada" && r.text.contains("ui")));
    }

    #[test]
    fn split_cmdline_splits_on_whitespace() {
        assert_eq!(split_cmdline("wsl -d Ubuntu"), vec!["wsl", "-d", "Ubuntu"]);
        assert_eq!(split_cmdline("  pwsh   -NoLogo "), vec!["pwsh", "-NoLogo"]);
        assert!(split_cmdline("   ").is_empty());
    }

    #[test]
    fn split_cmdline_keeps_quoted_paths_whole() {
        assert_eq!(
            split_cmdline(r#""C:\Program Files\Git\bin\bash.exe" --login"#),
            vec![r"C:\Program Files\Git\bin\bash.exe", "--login"]
        );
    }

    #[test]
    fn edit_prompt_appends_backspaces_submits_and_cancels() {
        let mut b = String::new();
        assert!(matches!(edit_prompt(&mut b, b"wsl"), PromptEdit::Continue));
        assert_eq!(b, "wsl");
        // backspace deletes the last char
        assert!(matches!(edit_prompt(&mut b, &[0x7f]), PromptEdit::Continue));
        assert_eq!(b, "ws");
        // Enter submits, keeping what was typed so far
        assert!(matches!(edit_prompt(&mut b, b"l\r"), PromptEdit::Submit));
        assert_eq!(b, "wsl");
        // Esc cancels
        let mut c = String::from("pwsh");
        assert!(matches!(edit_prompt(&mut c, &[0x1b]), PromptEdit::Cancel));
    }
}
