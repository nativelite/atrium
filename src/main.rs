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
use atrium::input::{encode_sgr_click, Action, Dir, PrefixScanner};
use atrium::layout::{self, Tree};
use atrium::tile::screen_to_pane_local;
use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use atrium::ctl::AgentId;
use std::time::{Duration, Instant};

mod ctl_server;
mod fleet_cli;
mod loop_phases;
mod mouse;
mod overlay_keys;
mod overview;
mod pane_keys;
mod pane_spawn;
mod panels;
mod prompt;
mod renderer;
mod safety_net;
mod tiled;
pub(crate) use ctl_server::*;
pub(crate) use fleet_cli::*;
pub(crate) use loop_phases::*;
pub(crate) use mouse::*;
pub(crate) use overlay_keys::*;
pub(crate) use overview::*;
pub(crate) use pane_keys::*;
pub(crate) use pane_spawn::*;
pub(crate) use panels::*;
pub(crate) use prompt::*;
pub(crate) use renderer::*;
pub(crate) use safety_net::*;
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
/// How often the run loop checks whether the session snapshot needs refreshing.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5);

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

/// Set at startup from the `--trust` / `--skip-permissions` flags (or a fleet's
/// declared posture): how much atrium relaxes the permission posture of every
/// agent pane it spawns. Read by the spawn path (`spawn_pane_full`) via
/// [`trust_mode`].
///
/// Stores the enum itself. It used to be an `AtomicU8` with a hand-written codec
/// whose numbering (`Edits=1, Skip=2, Plan=3, Auto=4`) disagreed with
/// [`TrustMode::rank`](atrium::ctl::TrustMode::rank) — one integer confused for the
/// other was a silent privilege up/down-grade, and an unknown code decoded to
/// `Off` (r10 audit B3). With no integer form there is nothing to confuse.
static AGENT_TRUST: Mutex<atrium::ctl::TrustMode> = Mutex::new(atrium::ctl::TrustMode::Off);

/// The launch trust mode. A poisoned lock still holds a whole `TrustMode` (it is
/// `Copy` and only ever replaced), so recovering its value is sound.
fn trust_mode() -> atrium::ctl::TrustMode {
    *AGENT_TRUST.lock().unwrap_or_else(|e| e.into_inner())
}

/// Publish the launch trust mode to the spawn path.
fn set_trust_mode(m: atrium::ctl::TrustMode) {
    *AGENT_TRUST.lock().unwrap_or_else(|e| e.into_inner()) = m;
}

fn next_agent_id() -> AgentId {
    AgentId(NEXT_AGENT.fetch_add(1, Ordering::Relaxed))
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

/// The longest the loop sleeps with nothing arriving: its timed work (the splash
/// spinner, the bar refresh, resize checks, ctl polling, the safety net) runs at
/// least this often. Keys and pane output wake it immediately.
const LOOP_TICK: Duration = Duration::from_millis(20);

/// How long exit waits for the terminal to take atrium's last output (the
/// screen restore) before leaving anyway.
const SCREEN_FINISH: Duration = Duration::from_secs(3);

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

/// Max height (rows) of a spurious "shrink" atrium ignores in the resize poll: the
/// Windows Terminal ConPTY reports a few rows fewer once atrium is on the alternate
/// screen, and adopting it leaves a strip of stale content below the bar. A
/// height-only shrink no larger than this is treated as that reservation, not a
/// real resize. Generous enough to cover the observed reserve, small enough that a
/// genuine window resize (usually larger, and changing width) is still honored.
const ALT_SCREEN_RESERVE_ROWS: u16 = 8;

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

/// One hosted terminal: a pty, its emulator (for tiled compositing), its
/// passthrough filter (for the passthrough / zoom path), and bar metadata. Each
/// pane has a stable `id` the window's split tree refers to.
pub(crate) struct Pane {
    /// This pane's output, from its reader thread; `None` until the loop starts
    /// one. **Declared before `pty` on purpose**: fields drop in order, and the
    /// inbox must go first so its reader stops before the `Pty`'s drop waits on
    /// the console host (see `atrium::events::PaneInbox`).
    pub(crate) inbox: Option<atrium::events::PaneInbox>,
    pub(crate) id: usize,
    pub(crate) pty: pty::Pty,
    pub(crate) term: vterm::Term,
    pub(crate) filter: atrium::filter::Passthrough,
    pub(crate) title: String,
    pub(crate) activity: bool,
    /// When this pane last produced output, on a **monotonic** clock. Stamped in
    /// the run loop wherever `activity` is set; the ctl `list`/`status` reply
    /// reports `now - last_activity` as `idle_ms` (via [`atrium::ipc::idle_ms`]),
    /// splitting the overloaded "idle" into busy vs. genuinely-stale. Monotonic so
    /// an NTP step can't make idle jump or go negative. Initialized at spawn.
    pub(crate) last_activity: std::time::Instant,
    /// Unsent human input in this pane's prompt, inferred from forwarded keys. A
    /// queued `ctl send` waits while it holds, so a delivery is never spliced
    /// onto — and submitted with — what the human is typing.
    pub(crate) draft: atrium::deliver::Draft,
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
    pub(crate) agent_id: AgentId,
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
    pub(crate) parent: Option<AgentId>,
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
    /// The command vector this pane was spawned with — the user command before
    /// atrium appends trust flags or `--session-id`. Used by `atrium recover`.
    pub(crate) argv: Vec<String>,
    /// The git worktree this pane runs in, if any.
    pub(crate) worktree: Option<String>,
    /// This pane's own deny entries (its fleet agent's `deny`). Kept on the pane
    /// because `argv` does not carry them — `--disallowedTools` is built at spawn
    /// — so a snapshot has something to record and `atrium recover` something to
    /// re-apply. Empty for a pane with no per-agent rules.
    pub(crate) deny: Vec<String>,
    /// The effective trust posture this pane was spawned at, which may sit below
    /// the session ceiling (a fleet agent's `trust`, a de-escalated ctl worker).
    /// Recorded so recovery restores the pane's own posture, not the ceiling.
    pub(crate) mode: atrium::ctl::TrustMode,
    /// The last element of `argv` is this fleet agent's kickoff prompt. Set by
    /// the fleet launcher; a resume drops it so a mid-task agent is not told to
    /// start over.
    pub(crate) kickoff: bool,
    /// The worktree instructions folded into this pane's system prompt at spawn,
    /// kept so a resume can fold them in again (`argv` never carried them).
    pub(crate) norms: Option<String>,
    /// The fleet context-store variables this pane was given, for the same reason.
    pub(crate) context_env: Vec<(String, String)>,
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

    /// The pane that stands for this window in the bar: the focused one (so a
    /// zoomed or focused fleet agent reads as itself), else the first.
    pub(crate) fn bar_pane(&self) -> Option<&Pane> {
        self.pane(self.tree.focus()).or_else(|| self.panes.first())
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
    // The user-global config (`config.json`): aliases, session-wide lists and
    // fleet defaults, in force for every launch path below. Not for the ctl
    // client, which runs inside panes many times a session and needs none of
    // it. A malformed file is an error here, never a silent default.
    if args.first().map(String::as_str) != Some("ctl") {
        if let Err(e) = atrium::config::install_from_disk() {
            eprintln!("atrium: {e}");
            return ExitCode::FAILURE;
        }
    }
    // `atrium recover` rehydrates the most recent session snapshot — same dispatch
    // level as `fleet` and `ctl`, before flag parsing, so `recover` is never
    // mistaken for a hosted command.
    if args.first().map(String::as_str) == Some("recover") {
        return recover_cmd(&args[1..]);
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
            "usage: atrium [--identity <name>] [--reap-orphans] [--allow-ctl [--max-depth <N>]] [--trust [plan|accept|automode|skip] | --skip-permissions] [-n <N> | --grid <R>x<C>] [up <fleet> | command [args...]]\n\
             \x20      up <fleet>: launch a saved fleet under the leading flags (alias for `atrium fleet up <fleet>`),\n\
             \x20               e.g. `atrium --trust automode up context-build`. Put session flags BEFORE `up`.\n\
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
    // `atrium [--identity X] [--trust <policy>] [--allow-ctl] up <name>` is the
    // ergonomic alias for `atrium fleet up <name>`: the launch flags atrium just
    // parsed become the fleet's posture. Without this, `up` falls through to the
    // hosted-program path below — atrium runs a bogus program called `up`, the
    // fleet never launches through `fleet_up`, and its agents come up in regular
    // mode because the session trust was never set. `atrium fleet up …` (the
    // early dispatch above) still serves the flags-after-name form.
    if let Some(result) = up_alias(&rest) {
        return match result {
            Ok(name) => fleet_up(name, allow_ctl, max_depth, trust),
            Err(msg) => {
                eprintln!("atrium: {msg}");
                ExitCode::FAILURE
            }
        };
    }
    let command: Vec<String> = if rest.is_empty() {
        vec![default_shell()]
    } else {
        rest
    };
    // The last session in this project crashed: offer to pick it back up before
    // starting a fresh one. Ahead of the skip confirmation, because a resume
    // settles its own posture (and asks its own questions).
    if let Some(code) = offer_resume(allow_ctl, max_depth, trust) {
        return code;
    }
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
        // The default (non-fleet) path declares no topic vocabulary: soft-gate.
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

/// Build a [`atrium::session::PaneCapture`] from the individual pane metadata
/// fields that `snapshot_if_changed` reads from a live [`Pane`]. Extracted
/// from the inline closure so the field mapping is directly unit-testable
/// without a real PTY — if `worktree` (or any other field) is accidentally
/// dropped, the test at `capture_pane_fields_propagates_worktree` fails.
// The field list is the point: this is the one place a live `Pane` becomes a
// `PaneCapture`, and naming every field here is what makes a dropped one a
// compile error rather than a silently thinner snapshot. A bag struct would just
// be `PaneCapture` again.
#[allow(clippy::too_many_arguments)]
fn capture_pane_fields(
    id: usize,
    role: Option<String>,
    argv: Vec<String>,
    cwd: Option<String>,
    identity: Option<String>,
    session_id: Option<String>,
    worktree: Option<String>,
    deny: Vec<String>,
    can_spawn: bool,
    depth: usize,
    parent_pane: Option<usize>,
    mode: atrium::ctl::TrustMode,
    kickoff: bool,
    norms: Option<String>,
    context_env: Vec<(String, String)>,
) -> atrium::session::PaneCapture {
    atrium::session::PaneCapture {
        id,
        role,
        argv,
        cwd,
        identity,
        session_id,
        worktree,
        deny,
        can_spawn,
        depth,
        parent_pane,
        mode: Some(mode),
        kickoff,
        norms,
        context_env,
    }
}

/// Surface a best-effort operation's failure in the bar **once per failure
/// streak**: the first `Err` flashes `<what> failed: <error>`, later ones stay
/// quiet until an `Ok` resets the streak. Returns the success value, if any. The
/// pattern the r10 audit (B8) asked every swallowed best-effort `Result` to follow.
fn surface_once<T>(
    failing: &mut bool,
    result: std::io::Result<T>,
    what: &str,
    flash: &mut Option<(String, Instant)>,
) -> Option<T> {
    match result {
        Ok(v) => {
            *failing = false;
            Some(v)
        }
        Err(e) => {
            if !*failing {
                *failing = true;
                *flash = Some((format!("{what} failed: {e}"), Instant::now()));
            }
            None
        }
    }
}

/// Write a session snapshot when the window state has changed since the last
/// write, or when the heartbeat is due. Returns whether a write landed, so the
/// caller can move the warden's baseline to it.
///
/// The heartbeat is what lets a later launch tell a running session from a dead
/// one: a live atrium rewrites its file every `HEARTBEAT_MS` even when nothing
/// changed, so a stale `saved_at_ms` means the writer is gone even if its pid has
/// since been reused (see `session_store::liveness`). `last` holds the snapshot
/// *without* the timestamp, so the change check still compares content.
///
/// `last` advances only when the write lands, so a failed save is retried on the
/// next check instead of being forgotten; the error is returned for the caller to
/// surface (r10 audit B8 — it used to be dropped, and `atrium recover` silently
/// had nothing to restore).
fn snapshot_if_changed(
    windows: &[Window],
    path: &std::path::Path,
    last: &mut Option<atrium::session::Snapshot>,
    last_write_ms: &mut u64,
    meta: &atrium::session::SessionMeta,
) -> std::io::Result<bool> {
    let w = match windows.first() {
        Some(w) => w,
        None => return Ok(false),
    };
    // A pane's `parent` is an `AgentId`, minted per process and re-minted on
    // recovery — storing one would name a *different* pane in the recovered
    // session. Resolve it to the parent's pane id, which is stable inside the
    // snapshot. A parent in another window, or one that has already exited,
    // resolves to `None`; the pane's restored `depth` still holds the recursion
    // guard, which is what `--max-depth` actually checks.
    let parent_pane = |parent: Option<atrium::ctl::AgentId>| -> Option<usize> {
        let parent = parent?;
        w.panes.iter().find(|p| p.agent_id == parent).map(|p| p.id)
    };
    let snap = atrium::session::capture(
        w.tree.ids(),
        w.tree.focus(),
        w.panes
            .iter()
            .map(|p| {
                capture_pane_fields(
                    p.id,
                    p.role.clone(),
                    p.argv.clone(),
                    p.cwd.clone(),
                    p.identity.clone(),
                    p.session_id.clone(),
                    p.worktree.clone(), // set by spawn_worker_* via sp.worktree
                    p.deny.clone(),
                    p.can_spawn,
                    p.depth,
                    parent_pane(p.parent),
                    p.mode,
                    p.kickoff,
                    p.norms.clone(),
                    p.context_env.clone(),
                )
            })
            .collect(),
        atrium::session::policy(),
    );
    let mut snap = snap;
    snap.meta = meta.clone();
    let now = atrium::session_store::now_ms();
    let heartbeat_due = now.saturating_sub(*last_write_ms) >= atrium::session_store::HEARTBEAT_MS;
    if last.as_ref() == Some(&snap) && !heartbeat_due {
        return Ok(false);
    }
    // Re-created on every write rather than once at startup: a deleted directory
    // (a cleanup, or tampering) must not turn into a silent stream of failures.
    if let Some(dir) = path.parent() {
        atrium::session_store::ensure_private_dir(dir)?;
    }
    let mut stamped = snap.clone();
    stamped.meta.saved_at_ms = Some(now);
    atrium::session::save(path, &stamped)?;
    *last = Some(snap);
    *last_write_ms = now;
    Ok(true)
}

/// Drop repeats, keeping first-seen order. Small-n by construction (a deny list),
/// so the quadratic scan is cheaper than the allocation a set would need.
fn dedup_preserving_order(v: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(v.len());
    for e in v {
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// Was one of `names` typed on the recover command line (bare or `=`-glued)?
///
/// `ctl::parse_flags` returns a value for every flag, defaulted, so it cannot say
/// whether the operator *chose* one. Recovery needs that distinction: an explicit
/// flag must win over the snapshot, while an absent one must defer to it rather
/// than silently overwriting the recorded session with a default.
fn flag_typed(args: &[String], names: &[&str]) -> bool {
    args.iter().any(|a| {
        names.contains(&a.as_str())
            || names
                .iter()
                .any(|n| a.starts_with(&format!("{n}=")) && n.starts_with("--"))
    })
}

/// The trust ceiling a recovery runs at: an explicitly typed flag wins, else the
/// posture the snapshot recorded, else the parsed default. The result is still
/// capped to any enclosing session by `cap_trust_to_ancestor` at the call site —
/// a snapshot is data on disk and must never be a way to climb.
fn recovered_trust(
    typed: bool,
    parsed: atrium::ctl::TrustMode,
    recorded: Option<atrium::ctl::TrustMode>,
) -> atrium::ctl::TrustMode {
    if typed {
        return parsed;
    }
    recorded.unwrap_or(parsed)
}

/// One pane's posture on recovery: what it ran at before, capped to the session
/// ceiling. A pane that sat below the ceiling (a fleet agent's own `trust`, a
/// de-escalated worker) comes back where it was instead of being promoted to the
/// ceiling, and a snapshot naming a *higher* posture is capped, not obeyed.
fn recovered_pane_mode(
    recorded: Option<atrium::ctl::TrustMode>,
    ceiling: atrium::ctl::TrustMode,
) -> atrium::ctl::TrustMode {
    effective_mode(recorded, ceiling).0
}

/// A one-line, human-readable account of what a recovery is about to restore —
/// or, for a pre-policy snapshot, what it cannot.
///
/// `policy` is the **effective** policy (what will actually be installed), not
/// the raw record: a trust mode the ancestor cap lowered must read as lowered.
/// `trust_from_snapshot` marks the posture as coming from the file rather than
/// the command line, because that is the one value a recovery can raise above
/// the flags the operator typed — and the snapshot is a file in a shared temp
/// directory. Printed *before* the skip confirmation, so it informs the only
/// prompt the operator gets.
fn recovery_notice(
    panes: &[atrium::session::PaneRecord],
    policy: Option<&atrium::session::PolicyRecord>,
    trust_from_snapshot: bool,
    ceiling: atrium::ctl::TrustMode,
) -> String {
    let mut parts = vec![format!("{} pane(s)", panes.len())];
    let head = match policy {
        Some(p) => {
            if let Some(t) = p.trust {
                let source = if trust_from_snapshot {
                    " (from the snapshot)"
                } else {
                    ""
                };
                parts.push(format!("trust {}{source}", t.policy_label()));
            }
            if p.allow_ctl {
                // An unlimited guard is stored clamped to `i64::MAX` (see
                // `session::num`); printing that as a 19-digit number reads as
                // corruption rather than as "no limit".
                let depth = if p.max_depth > 1_000_000 {
                    "unlimited".to_string()
                } else {
                    p.max_depth.to_string()
                };
                parts.push(format!("ctl on (max depth {depth})"));
            }
            if !p.deny.is_empty() {
                parts.push(format!("{} session deny rule(s)", p.deny.len()));
            }
            if let Some(j) = p.build_jobs {
                parts.push(format!("build pool {j}"));
            }
            if let Some(mb) = p.memory_mb {
                parts.push(format!("memory cap {mb} MB"));
            }
            format!("atrium recover: restoring {}", parts.join(", "))
        }
        None => format!(
            "atrium recover: restoring {} — this snapshot predates the policy block, so the \
             session's deny rules, compile pool and memory cap are NOT restored; \
             pass them on the command line if the session had them",
            parts.join(", ")
        ),
    };
    // Then every pane: what it will run and with what it is allowed to do. The
    // summary line alone left out the part a hostile snapshot actually controls —
    // the commands, and each pane's spawn right and deny rules — so the operator
    // was approving a count. Every string is from the file, so every string is
    // defanged before it reaches the terminal, as the fleet banner does.
    let mut lines = vec![head];
    for (n, record) in panes.iter().enumerate() {
        let who = record
            .role
            .as_deref()
            .map(atrium::fleet::sanitize)
            .unwrap_or_else(|| format!("pane {}", n + 1));
        lines.push(format!("  [{who}] {}", argv_summary(&resume_argv(record))));
        let mut facts = Vec::new();
        if let Some(cwd) = &record.cwd {
            facts.push(format!("cwd {}", atrium::fleet::sanitize(cwd)));
        }
        if let Some(id) = &record.identity {
            facts.push(format!("identity {}", atrium::fleet::sanitize(id)));
        }
        facts.push(format!(
            "mode {}",
            recovered_pane_mode(record.mode, ceiling).policy_label()
        ));
        facts.push(if record.can_spawn {
            "may spawn teammates".to_string()
        } else {
            "cannot spawn".to_string()
        });
        if !record.deny.is_empty() {
            facts.push(format!("{} own deny rule(s)", record.deny.len()));
        }
        // Shown, not just named: both come from the file and both reach the agent.
        if let Some(norms) = &record.norms {
            let flat = atrium::fleet::sanitize(&norms.replace(['\n', '\r'], " "));
            let preview: String = flat.chars().take(60).collect();
            facts.push(format!(
                "worktree instructions \"{preview}…\" ({} chars)",
                norms.chars().count()
            ));
        }
        for (name, value) in &record.context_env {
            facts.push(format!("{name}={}", atrium::fleet::sanitize(value)));
        }
        lines.push(format!("      {}", facts.join(" · ")));
        let (_, dropped) = atrium::ctl::sanitize_spawn_argv(&record.argv);
        if !dropped.is_empty() {
            lines.push(format!(
                "      ignored from the saved command: {}",
                dropped.join(", ")
            ));
        }
    }
    lines.join("\n")
}

/// A pane's command for the consent notice: every flag as written, short values
/// verbatim, and anything long or multi-word — a system prompt, a kickoff —
/// reduced to its length. Defanged, since it comes from the snapshot file.
fn argv_summary(argv: &[String]) -> String {
    argv.iter()
        .enumerate()
        .map(|(i, a)| {
            let a = atrium::fleet::sanitize(a);
            if i == 0 {
                atrium::bind::command_stem(&a)
            } else if a.starts_with('-') || (a.chars().count() <= 40 && !a.contains(' ')) {
                a
            } else {
                format!("<{} chars>", a.chars().count())
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `atrium recover` — load the most recent session snapshot and relaunch every
/// pane with its session resumed. A missing snapshot prints a clear message
/// and exits cleanly rather than panicking.
fn recover_cmd(args: &[String]) -> ExitCode {
    // recover's own flags first; everything else is the session flags `parse_flags`
    // knows (`--trust`, `--allow-ctl`, `--max-depth`).
    let mut list = false;
    let mut explicit: Option<std::path::PathBuf> = None;
    let mut flags: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list = true,
            "--snapshot" => match args.get(i + 1) {
                Some(path) => {
                    explicit = Some(path.into());
                    i += 1;
                }
                None => {
                    eprintln!("atrium recover: --snapshot needs a path");
                    return ExitCode::FAILURE;
                }
            },
            a if a.starts_with("--snapshot=") => explicit = Some(a["--snapshot=".len()..].into()),
            a => flags.push(a.to_string()),
        }
        i += 1;
    }
    let (allow_ctl, max_depth, trust, rest) = match atrium::ctl::parse_flags(&flags) {
        Ok(quad) => quad,
        Err(msg) => {
            eprintln!("atrium recover: {msg}");
            return ExitCode::FAILURE;
        }
    };
    if !rest.is_empty() {
        eprintln!("atrium recover: unexpected argument {:?}", rest[0]);
        return ExitCode::FAILURE;
    }
    let Ok(cwd) = std::env::current_dir() else {
        eprintln!("atrium recover: the current directory no longer exists — cd into the project");
        return ExitCode::FAILURE;
    };
    let project = atrium::session_store::project_id(&cwd);
    let dir = atrium::session_store::state_root()
        .map(|root| atrium::session_store::project_dir(&root, &project));
    if list {
        print_sessions(&project, dir.as_deref());
        return ExitCode::SUCCESS;
    }
    // Before anything is chosen, asked or marked: a resume needs a terminal to
    // run in, and its confirmation needs a person at one. Run from a script or an
    // agent's shell, `recover` used to read end-of-input as consent, mark the
    // operator's crashed session closed, and only then fail on the terminal.
    {
        use std::io::IsTerminal;
        if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
            eprintln!("atrium recover: needs a terminal (to confirm the resume, and to run it)");
            return ExitCode::FAILURE;
        }
    }
    let now = atrium::session_store::now_ms();
    let (path, snap) = match explicit {
        Some(path) => match atrium::session::load(&path) {
            Ok(snap) => (path, snap),
            Err(e) => {
                eprintln!("atrium recover: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            let stored = dir
                .as_deref()
                .map(|d| atrium::session_store::list(d, &project, now).sessions)
                .unwrap_or_default();
            match atrium::session_store::recoverable(&stored, now, atrium::reap::pid_running) {
                Some(found) => (found.path.clone(), found.snapshot.clone()),
                None => {
                    eprintln!("atrium recover: no saved session for this project ({project})");
                    // Snapshots used to live in the shared temp directory. Name the
                    // newest one there rather than scan it: picking by mtime from a
                    // directory every session and test writes to is exactly how the
                    // wrong session used to get restored.
                    if let Some(old) = find_latest_snapshot(
                        &atrium::reap::registry_dir(),
                        atrium::reap::pid_running,
                    ) {
                        eprintln!(
                            "atrium recover: an older atrium kept snapshots in the temp directory; \
                             the newest is {}\n  restore it with: atrium recover --snapshot \"{}\"",
                            old.display(),
                            old.display()
                        );
                    }
                    return ExitCode::SUCCESS;
                }
            }
        }
    };
    if atrium::session_store::liveness(&snap.meta, now, atrium::reap::pid_running)
        == atrium::session_store::Liveness::Running
    {
        eprintln!(
            "atrium recover: that session is still running (atrium pid {}); resuming it would \
             start a second copy of the same agents on the same transcripts",
            snap.meta
                .pid
                .map_or_else(|| "?".to_string(), |p| p.to_string())
        );
        return ExitCode::FAILURE;
    }
    if snap.panes.is_empty() {
        eprintln!("atrium recover: snapshot has no panes — nothing to recover");
        return ExitCode::SUCCESS;
    }
    if !snap.meta.clean && snap.meta.pid.is_some_and(atrium::reap::pid_running) {
        eprintln!(
            "atrium recover: its atrium (pid {}) still exists but stopped updating — it may be \
             hung. Resuming starts a second copy of the same agents.",
            snap.meta.pid.unwrap_or(0)
        );
    }
    let plan = plan_resume(
        &snap,
        Some(&path),
        allow_ctl,
        max_depth,
        trust,
        flag_typed(&flags, &["--trust", "--skip-permissions"]),
        flag_typed(&flags, &["--max-depth"]),
    );
    // Before the confirmation, not after: this line is what the operator needs in
    // order to answer it.
    eprintln!("{}", plan.notice);
    if !confirm_resume("atrium recover: resume this session?") {
        eprintln!("atrium recover: not resumed.");
        return ExitCode::SUCCESS;
    }
    resume_session(snap, plan, Some(&path))
}

/// What a resume will install, settled before anything is asked or spawned so the
/// operator is shown exactly that.
struct ResumePlan {
    trust: atrium::ctl::TrustMode,
    allow_ctl: bool,
    max_depth: usize,
    /// The policy as it will actually be installed: the snapshot's guards, with
    /// the trust/ctl/depth values settled here. Recorded verbatim for the resumed
    /// session, so its own snapshots carry it forward unchanged — reading it back
    /// off the live globals would lose a `build_jobs` that was inherited rather
    /// than re-created, one recovery generation at a time.
    applied: Option<atrium::session::PolicyRecord>,
    /// The one-line account shown before the confirmation.
    notice: String,
}

/// What a resume's bus and board will bring back, for the consent notice. Their
/// contents are as writable by hosted agents as the snapshot, and agents act on
/// bus messages, so the operator is told they are coming rather than finding out.
/// `None` when the source has neither (or is outside the store, which never seeds).
fn sidecar_summary(source: &std::path::Path) -> Option<String> {
    let root = atrium::session_store::state_root()?;
    if !atrium::session_store::is_in_store(source, &root) {
        return None;
    }
    let bus_path = atrium::session_store::sidecar(source, "bus");
    let board_path = atrium::session_store::sidecar(source, "board");
    let mut parts = Vec::new();
    if bus_path.is_file() {
        let bus = atrium::bus::Bus::with_file(bus_path);
        parts.push(format!(
            "bus: {} event(s), {} open decision(s)",
            bus.tail(atrium::bus::RING_CAP).len(),
            bus.pending_decisions().len()
        ));
    }
    if board_path.is_file() {
        let board = atrium::board::Board::with_file(board_path);
        parts.push(format!("board: {} entr(ies)", board.list().len()));
    }
    (!parts.is_empty()).then(|| {
        format!(
            "  restoring the team's {} — written by the session's agents; treat as such",
            parts.join(", ")
        )
    })
}

/// Settle a resume's posture. A typed flag wins; an omitted one defers to the
/// snapshot instead of overwriting it with a default. The trust ceiling is still
/// capped to any enclosing atrium session — a snapshot is data on disk and must
/// never be a way to climb above one.
fn plan_resume(
    snap: &atrium::session::Snapshot,
    source: Option<&std::path::Path>,
    allow_ctl: bool,
    max_depth: usize,
    trust: atrium::ctl::TrustMode,
    trust_typed: bool,
    depth_typed: bool,
) -> ResumePlan {
    let policy = snap.policy.as_ref();
    let trust = recovered_trust(trust_typed, trust, policy.and_then(|p| p.trust));
    let allow_ctl = allow_ctl || policy.is_some_and(|p| p.allow_ctl);
    let max_depth = if depth_typed {
        max_depth
    } else {
        policy.map_or(max_depth, |p| p.max_depth)
    };
    let trust = cap_trust_to_ancestor(trust);
    let applied = policy.map(|p| atrium::session::PolicyRecord {
        deny: p.deny.clone(),
        claude_aliases: p.claude_aliases.clone(),
        build_jobs: p.build_jobs,
        memory_mb: p.memory_mb,
        trust: Some(trust),
        allow_ctl,
        max_depth,
        topics: p.topics.clone(),
    });
    let mut notice = recovery_notice(&snap.panes, applied.as_ref(), !trust_typed, trust);
    if let Some(line) = source.and_then(sidecar_summary) {
        notice.push('\n');
        notice.push_str(&line);
    }
    ResumePlan {
        trust,
        allow_ctl,
        max_depth,
        applied,
        notice,
    }
}

/// Read a `[Y/n]` answer: anything but an explicit no is yes. Pure, so the rule is
/// testable without a terminal.
fn resume_answer(line: &str) -> bool {
    !matches!(line.trim().to_ascii_lowercase().as_str(), "n" | "no")
}

/// Ask the operator to confirm a resume. Always a real answer from a terminal:
/// `ATRIUM_YES` is deliberately not honoured here. It exists so scripts can skip
/// a fleet's banner, and an operator who keeps it set would otherwise have a
/// crashed session's snapshot — its commands and posture — applied behind their
/// back the next time they started a fleet.
fn confirm_resume(question: &str) -> bool {
    eprint!("{question} [Y/n] ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        // Zero bytes is end-of-input (Ctrl+D, Ctrl+Z, a closed pipe): nobody
        // answered, and nobody answering is not consent. Enter is a line.
        Ok(0) | Err(_) => false,
        Ok(_) => resume_answer(&line),
    }
}

/// Is this atrium running inside another atrium's pane? The pane marker, or —
/// where the platform can tell — process ancestry. The marker alone is an
/// environment variable an agent can unset; ancestry is the check it cannot, and
/// is used wherever it exists (not on Windows yet, see `warden::atrium_ancestor`).
/// Neither is a security boundary on its own: what they prevent is an agent that
/// simply runs `atrium` having its session filed, pruned and offered as the
/// operator's.
fn nested_in_atrium() -> bool {
    std::env::var_os(atrium::ctl::ENV_PANE).is_some()
        || matches!(
            atrium::warden::atrium_ancestor(),
            atrium::warden::Ancestry::Atrium(_)
        )
}

/// Should a launch offer to resume at all? Not when switched off
/// (`ATRIUM_RESUME_OFFER=0`, which the test suite sets), not from inside an
/// atrium pane (an agent launching atrium must not be handed the operator's
/// crashed session), and not without a terminal to ask on.
fn offer_enabled(offer_env: Option<&str>, in_pane: bool, interactive: bool) -> bool {
    offer_env != Some("0") && !in_pane && interactive
}

/// A human-scale age: `42s`, `7m`, `3h`, `2d`.
fn human_age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// On launch in a project whose last session crashed, offer to resume it — one
/// prompt that is both the resume and the approval. `Some(code)` when the launch
/// became a resume; `None` to launch normally. Declining marks that session
/// closed so it is not asked about again (`atrium recover` still has it).
fn offer_resume(
    allow_ctl: bool,
    max_depth: usize,
    trust: atrium::ctl::TrustMode,
) -> Option<ExitCode> {
    use std::io::IsTerminal;
    let offer_env = std::env::var(atrium::session_store::ENV_RESUME_OFFER).ok();
    if !offer_enabled(
        offer_env.as_deref(),
        nested_in_atrium(),
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
    ) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let project = atrium::session_store::project_id(&cwd);
    let root = atrium::session_store::state_root()?;
    let now = atrium::session_store::now_ms();
    let stored = atrium::session_store::list(
        &atrium::session_store::project_dir(&root, &project),
        &project,
        now,
    )
    .sessions;
    let found = atrium::session_store::offer(&stored, now, atrium::reap::pid_running)?;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let plan = plan_resume(
        &found.snapshot,
        Some(&found.path),
        allow_ctl,
        max_depth,
        trust,
        flag_typed(&argv, &["--trust", "--skip-permissions"]),
        flag_typed(&argv, &["--max-depth"]),
    );
    let meta = &found.snapshot.meta;
    eprintln!(
        "atrium: the last session in this project did not exit cleanly (saved {} ago).",
        meta.saved_at_ms
            .map_or_else(|| "?".to_string(), |t| human_age(now.saturating_sub(t)))
    );
    if meta.pid.is_some_and(atrium::reap::pid_running) {
        eprintln!(
            "atrium: its atrium (pid {}) still exists but stopped updating — it may be hung. \
             Resuming starts a second copy of the same agents.",
            meta.pid.unwrap_or(0)
        );
    }
    eprintln!("{}", plan.notice);
    if confirm_resume("Resume it?") {
        return Some(resume_session(
            found.snapshot.clone(),
            plan,
            Some(&found.path),
        ));
    }
    if let Err(e) = atrium::session_store::mark_closed(&found.path) {
        eprintln!("atrium: warning: {e}; this session may be offered again");
    }
    eprintln!("atrium: starting a new session (`atrium recover --list` still has that one).");
    None
}

/// `atrium recover --list`: this project's saved sessions, newest first, with
/// what each would restore — and any file that could not be read, since an
/// unreadable snapshot may be a tampered one.
fn print_sessions(project: &str, dir: Option<&std::path::Path>) {
    let Some(dir) = dir else {
        eprintln!(
            "atrium recover: no state directory could be determined (set {})",
            atrium::session_store::ENV_STATE_DIR
        );
        return;
    };
    let now = atrium::session_store::now_ms();
    let listing = atrium::session_store::list(dir, project, now);
    let (stored, bad) = (&listing.sessions, &listing.unreadable);
    println!("sessions for {project}");
    println!("  in {}", dir.display());
    if stored.is_empty() && bad.is_empty() && listing.suspect.is_empty() {
        println!("  (none)");
    }
    for s in stored {
        let meta = &s.snapshot.meta;
        let state = match atrium::session_store::liveness(meta, now, atrium::reap::pid_running) {
            atrium::session_store::Liveness::Running => "running",
            atrium::session_store::Liveness::Crashed => "crashed",
            atrium::session_store::Liveness::Closed => "closed",
        };
        let age = meta
            .saved_at_ms
            .map_or_else(|| "?".to_string(), |t| human_age(now.saturating_sub(t)));
        let trust = s
            .snapshot
            .policy
            .as_ref()
            .and_then(|p| p.trust)
            .map_or("-", |t| t.policy_label());
        let roles: Vec<&str> = s
            .snapshot
            .panes
            .iter()
            .filter_map(|p| p.role.as_deref())
            .collect();
        // Role strings come from the file: defanged, or an ESC in one could
        // repaint this listing into something reassuring.
        let roles: Vec<String> = roles.into_iter().map(atrium::fleet::sanitize).collect();
        let roles = if roles.is_empty() {
            String::new()
        } else {
            format!("  [{}]", roles.join(", "))
        };
        println!(
            "  {state:<8} {age:>4} ago  {} pane(s)  trust {trust}{roles}",
            s.snapshot.panes.len()
        );
        println!("           {}", s.path.display());
    }
    for (path, e) in bad {
        println!("  unreadable  {}: {e}", path.display());
    }
    for (path, why) in &listing.suspect {
        println!(
            "  suspect     {}: {why} — never offered; restore only with --snapshot, after \
             reading it",
            path.display()
        );
    }
}

/// Resume `snap` under `plan`: the skip confirmation, the session guards, every
/// pane with its own posture and capability, then the run loop. `source` is the
/// file it came from; a file in the store is marked closed once the operator has
/// confirmed, so the next launch does not offer it again. If a pane then fails to
/// start, the session is still reachable with a bare `atrium recover`, which also
/// takes closed sessions.
fn resume_session(
    snap: atrium::session::Snapshot,
    plan: ResumePlan,
    source: Option<&std::path::Path>,
) -> ExitCode {
    let ResumePlan {
        trust,
        allow_ctl,
        max_depth,
        applied,
        ..
    } = plan;
    let policy = snap.policy.clone();
    if trust == atrium::ctl::TrustMode::Skip && !confirm_skip_permissions() {
        eprintln!("atrium recover: aborted.");
        return ExitCode::SUCCESS;
    }
    // Checked again here, after the prompts: the answer can take a while, and a
    // second terminal offered the same crash may have resumed it meanwhile.
    if let Some(src) = source {
        if let (Ok(cwd), Some(root)) =
            (std::env::current_dir(), atrium::session_store::state_root())
        {
            let project = atrium::session_store::project_id(&cwd);
            let now = atrium::session_store::now_ms();
            let stored = atrium::session_store::list(
                &atrium::session_store::project_dir(&root, &project),
                &project,
                now,
            )
            .sessions;
            let me = atrium::session_store::Stored {
                path: src.to_path_buf(),
                snapshot: snap.clone(),
            };
            if atrium::session_store::superseded(&stored, &me, now, atrium::reap::pid_running) {
                eprintln!(
                    "atrium recover: these agents are already running in another atrium \
                     session — not starting a second copy"
                );
                return ExitCode::FAILURE;
            }
        }
    }
    if let (Some(src), Some(root)) = (source, atrium::session_store::state_root()) {
        if atrium::session_store::is_in_store(src, &root) {
            if let Err(e) = atrium::session_store::mark_closed(src) {
                eprintln!("atrium recover: warning: {e}; this session may be offered again");
            }
        }
    }
    set_trust_mode(trust);
    let topics = applied.as_ref().and_then(|p| p.topics.clone());
    if let Some(applied) = applied {
        atrium::session::set_policy(applied);
    }
    // The run loop opens this session's bus and board beside its new snapshot;
    // this is what lets it start them from the old session's instead of empty.
    if let Some(src) = source {
        atrium::session_store::set_resume_source(src);
        // Copied now, not when the run loop opens them: until this session writes
        // its first snapshot, another atrium starting in the project could prune
        // the source and its bus and board would silently start empty.
        if let Some((_, file, _)) = session_location() {
            atrium::session_store::seed_sidecar(file, "bus");
            atrium::session_store::seed_sidecar(file, "board");
        }
    }
    // Re-install the session guards BEFORE any pane spawns, in the same order
    // `fleet up` does: the deny list and memory ceiling are read at each spawn,
    // and the compile pool must exist before the first agent inherits it.
    if let Some(p) = &policy {
        if !p.deny.is_empty() {
            atrium::trust::set_fleet_deny(p.deny.clone());
        }
        // Before any spawn: the panes' commands are judged claude (or not) by
        // the same aliases the session ran with.
        if !p.claude_aliases.is_empty() {
            atrium::bind::set_claude_aliases(p.claude_aliases.clone());
        }
        if let Some(mb) = p.memory_mb {
            atrium::memguard::set_fleet_mb(mb);
        }
        // Guarded the way `fleet up` guards it: `planned_size` is `None` when the
        // pool is disabled or already inherited from an enclosing session, and
        // neither is a failure worth warning about.
        if let Some(jobs) = p.build_jobs {
            if atrium::buildpool::planned_size(Some(jobs)).is_some()
                && atrium::buildpool::init(Some(jobs)).is_none()
            {
                eprintln!(
                    "atrium recover: warning: could not rebuild the build pool — agents' \
                     builds will run unpooled"
                );
            }
        }
    }
    let mut flash: Option<(String, Instant)> = None;
    let ctl_listener = if allow_ctl {
        bind_ctl(&mut flash)
    } else {
        None
    };
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("atrium recover: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (rows, cols) = term.size().unwrap_or((24, 80));
    let pane_rows = rows.saturating_sub(1).max(1);
    let cell_rows = (pane_rows as usize / snap.panes.len().max(1)).max(1) as u16;
    let mut panes: Vec<Pane> = Vec::with_capacity(snap.panes.len());
    for record in &snap.panes {
        let argv = resume_argv(record);
        let place = resume_place(record, |d| std::path::Path::new(d).is_dir());
        if let Some(w) = &place.warning {
            eprintln!("atrium recover: {w}");
        }
        match spawn_pane_full(
            PaneSpec {
                command: &argv,
                id: record.id,
                identity: record.identity.as_deref(),
                cwd: place.cwd.as_deref(),
                // The pane's own posture, capped to the ceiling — not the ceiling
                // itself, which would promote every de-escalated pane.
                mode: recovered_pane_mode(record.mode, trust_mode()),
                // What the original spawn folded in beyond argv: the worktree
                // instructions, and the context-store variables (allowlisted on
                // read, so a snapshot cannot hand a pane any other environment).
                extra_env: &record.context_env,
                extra_norms: place.norms.as_deref(),
                // The agent's own deny rules. `argv` never carried them: the
                // `--disallowedTools` flags are built here, at spawn, from the
                // roster — so a recovery that replayed argv alone handed every
                // pane back the commands its fleet entry had taken away.
                deny: &record.deny,
            },
            cell_rows,
            cols,
            &mut flash,
        ) {
            Ok(mut pane) => {
                pane.role = record.role.clone();
                pane.worktree = record.worktree.clone();
                // The transcript id. `--resume <id>` is a user session argument,
                // so the spawn path injects no id of its own and would leave the
                // pane unbound: no agent status, and a snapshot with nothing to
                // resume next time or to tell a second copy by. Resuming keeps
                // writing the same transcript, so the recorded id stays right.
                if pane.session_id.is_none() {
                    pane.session_id = record.session_id.clone();
                }
                // The kickoff was dropped from this pane's command iff it resumed a
                // transcript; a pane that started fresh still carries it.
                pane.kickoff = record.kickoff && !kickoff_dropped(record);
                // Capability and spawn-depth, from the snapshot. `spawn_pane_full`
                // defaults to the human-pane behaviour (`can_spawn: true`, depth
                // 0); leaving those defaults gave every recovered fleet agent the
                // right to create teammates that its roster had withheld, and
                // restarted the `--max-depth` guard at zero for deep workers.
                pane.can_spawn = record.can_spawn;
                pane.depth = record.depth;
                panes.push(pane);
            }
            Err(e) => {
                // Name the pane and where it was to start: the error alone
                // blamed the command for a directory that was missing.
                let (what, where_) = (
                    pane_label(record),
                    place
                        .cwd
                        .as_deref()
                        .map(|d| format!(" in {d}"))
                        .unwrap_or_default(),
                );
                eprintln!(
                    "atrium recover: cannot start {what} ({:?}){where_}: {e}",
                    argv[0]
                );
                for p in panes.iter_mut() {
                    let _ = p.pty.kill();
                }
                return ExitCode::FAILURE;
            }
        }
    }
    // Re-point each worker at its parent. Agent ids were re-minted by the spawns
    // above, so the snapshot's stored PANE ids are mapped to the new agent ids
    // here, once every pane exists. `panes` is index-aligned with `snap.panes`:
    // the loop pushes one per record and aborts on the first failure.
    let new_ids: Vec<(usize, atrium::ctl::AgentId)> =
        panes.iter().map(|p| (p.id, p.agent_id)).collect();
    for (pane, record) in panes.iter_mut().zip(snap.panes.iter()) {
        if let Some(parent_pane) = record.parent_pane {
            pane.parent = new_ids
                .iter()
                .find(|(id, _)| *id == parent_pane)
                .map(|(_, agent)| *agent);
        }
    }
    // Both id sets, not just the layout's: a pane record carrying an id above
    // every layout id would otherwise collide with the next `Ctrl+A c`.
    let max_id = snap
        .layout
        .ids
        .iter()
        .chain(snap.panes.iter().map(|p| &p.id))
        .copied()
        .max()
        .unwrap_or(0);
    let mut tree = Tree::grid_from_ids(&snap.layout.ids);
    tree.focus_pane(snap.layout.focus);
    let window = Window {
        panes,
        tree,
        zoomed: false,
        next_id: max_id + 1,
    };
    run(
        &mut term,
        &[default_shell()],
        None,
        None,
        Some(window),
        allow_ctl,
        max_depth,
        trust,
        ctl_listener,
        // A fleet's declared topics keep the bus strict after a resume, as `fleet
        // up` made it.
        topics,
    )
}

/// Where a recovered pane runs and which worktree instructions it keeps: its
/// recorded directory and norms while that directory still exists. When it is
/// gone — a worktree the fleet reaped after its item shipped — the pane starts
/// in the project directory *without* the instructions, which would tell it it
/// sits in a worktree it does not, and `warning` says so. Recovery used to hand
/// the missing directory straight to the spawn and abort the whole session on
/// its "file not found", blaming the command. Pure: `dir_exists` is injected.
struct ResumePlace {
    cwd: Option<String>,
    norms: Option<String>,
    warning: Option<String>,
}

fn resume_place(
    record: &atrium::session::PaneRecord,
    dir_exists: impl Fn(&str) -> bool,
) -> ResumePlace {
    match record.cwd.as_deref() {
        Some(dir) if !dir_exists(dir) => ResumePlace {
            cwd: None,
            norms: None,
            warning: Some(format!(
                "{}: its directory {dir} is gone; starting in the project directory without its worktree instructions",
                pane_label(record)
            )),
        },
        _ => ResumePlace {
            cwd: record.cwd.clone(),
            norms: record.norms.clone(),
            warning: None,
        },
    }
}

/// How a recovery message names a pane: its role, else its command.
fn pane_label(record: &atrium::session::PaneRecord) -> String {
    record
        .role
        .clone()
        .or_else(|| record.argv.first().cloned())
        .unwrap_or_else(|| format!("pane {}", record.id))
}

/// Build the recovery argv for one pane. For claude panes with a stored
/// session id, strips `--continue` and appends `--resume <id>` so the agent
/// resumes its conversation. Non-claude panes are relaunched verbatim.
fn resume_argv(record: &atrium::session::PaneRecord) -> Vec<String> {
    // The saved command is data from a file every hosted agent can write. Posture
    // is atrium's to set, from the policy the operator just confirmed — so the
    // governed permission flags never ride along, exactly as `fleet up` and
    // `ctl spawn` strip them from what they launch.
    // A kickoff is replayed only when the agent starts fresh. Resuming a
    // transcript, the agent is mid-task; its opening instructions arriving again
    // as a new turn is how resumed fleet agents re-read CLAUDE.md and went back to
    // step one.
    //
    // Only for claude, where a `--resume` is appended below. Another vendor's pane
    // may carry a session id atrium adopted, but nothing here resumes it: that pane
    // starts a new session, and its kickoff is exactly what it needs.
    let saved: &[String] = match record.argv.split_last() {
        Some((_, rest)) if kickoff_dropped(record) => rest,
        _ => &record.argv,
    };
    let (mut argv, _) = atrium::ctl::sanitize_spawn_argv(saved);
    if argv.is_empty() {
        return argv;
    }
    if !atrium::bind::is_claude_stem(&atrium::bind::command_stem(&argv[0])) {
        return argv;
    }
    if let Some(id) = &record.session_id {
        // A pane that was itself resumed carries `--resume <id>` in its argv
        // already; appending a second one is a malformed command line.
        argv = strip_session_args(&argv);
        argv.push("--resume".to_string());
        argv.push(id.clone());
    }
    argv
}

/// Does a resume leave this pane's kickoff out? Only when the pane has one and a
/// claude transcript will actually be resumed. Shared by `resume_argv` (which
/// drops it) and the resume spawn (which records whether the new command still
/// carries it), so the two can never disagree.
fn kickoff_dropped(record: &atrium::session::PaneRecord) -> bool {
    record.kickoff
        && record.session_id.is_some()
        && record
            .argv
            .first()
            .is_some_and(|c| atrium::bind::is_claude_stem(&atrium::bind::command_stem(c)))
}

/// Remove every session-selecting argument — `--continue`/`-c`, and
/// `--resume`/`-r`/`--session-id` with their value, in either the separate or
/// the `=` form — so exactly one `--resume` can be appended.
fn strip_session_args(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        let head = a.split('=').next().unwrap_or(a);
        match head {
            "--continue" | "-c" if i > 0 => i += 1,
            "--resume" | "-r" | "--session-id" if i > 0 => {
                i += if !a.contains('=') && i + 1 < argv.len() {
                    2
                } else {
                    1
                };
            }
            _ => {
                out.push(a.clone());
                i += 1;
            }
        }
    }
    out
}

/// Scan `dir` — the temp directory older atriums saved snapshots to — for
/// `atrium-session-<pid>.json` files and return the most recently modified one
/// whose atrium is no longer `running`, or `None`.
///
/// Only used to *name* a legacy file for `atrium recover --snapshot`, never to pick
/// one silently. Skipping live writers matters even for that: an older atrium that
/// is still running keeps its snapshot the newest file there, and pointing the
/// operator at their own running session as "the one to recover" is exactly
/// backwards.
fn find_latest_snapshot(
    dir: &std::path::Path,
    running: impl Fn(u32) -> bool,
) -> Option<std::path::PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("atrium-session-") || !name.ends_with(".json") {
            continue;
        }
        let writer = name
            .trim_start_matches("atrium-session-")
            .trim_end_matches(".json")
            .parse::<u32>()
            .ok();
        if writer.is_some_and(&running) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if best.as_ref().map_or(true, |(t, _)| modified > *t) {
                    best = Some((modified, entry.path()));
                }
            }
        }
    }
    best.map(|(_, p)| p)
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
    // The fleet's declared canonical topic vocabulary (fleet config `"topics": []`),
    // if any. `Some` ⇒ the bus enforces strict topic admission (agents route only
    // on declared topics); `None` ⇒ topics are soft-gated (a novel topic needs
    // `--new`). The default, non-fleet path passes `None`.
    canonical_topics: Option<Vec<String>>,
) -> ExitCode {
    // Publish the trust policy before any pane is spawned so even the initial
    // agent picks it up.
    set_trust_mode(trust);
    // Record the session policy for the snapshot writer. This is the one place
    // every launch path meets — a plain launch, `fleet up`, and `recover` all
    // arrive here — and the fleet path has already installed its deny list, pool
    // and memory ceiling by now, so reading them here captures the effective
    // session rather than the command line's half of it.
    atrium::session::set_policy(atrium::session::PolicyRecord {
        // Deduped: `session_deny()` is `ATRIUM_DENY` + the fleet's rules, and a
        // recovery re-installs the merged list as the fleet's — so without this
        // the env entries are re-appended once per recovery generation.
        deny: dedup_preserving_order(atrium::trust::session_deny()),
        claude_aliases: dedup_preserving_order(atrium::bind::session_claude_aliases()),
        build_jobs: atrium::buildpool::session_size(),
        memory_mb: atrium::memguard::fleet_mb(),
        trust: Some(trust),
        allow_ctl,
        max_depth,
        topics: canonical_topics.clone(),
    });
    // The terminal as a queue: a terminal that stops reading must not stop the
    // loop — keys, pane draining, ctl and the safety net all run on. See
    // `atrium::screen`; the view is repainted whole once it catches up.
    let mut out = atrium::screen::Screen::stdout();
    let (mut rows, mut cols) = term.size().unwrap_or((24, 80));
    // Resizes are read from a watcher thread: on Windows the size query itself
    // blocks while the terminal isn't reading (see `SizeWatch`).
    let size_watch = atrium::screen::SizeWatch::start((rows, cols));
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
            // Every pane enrolls in the session job at its own spawn (Windows:
            // created suspended, assigned, then resumed), so there is nothing to
            // pass for enrollment here.
            Some(g) => spawn_window_grid(command, rows, cols, g, identity, &mut flash, None),
            None => spawn_window(
                command,
                rows,
                cols,
                0,
                identity,
                trust_mode(),
                &mut flash,
                None,
                None,
                None,
            ),
        },
    };
    match initial {
        Ok(w) => windows.push(w),
        Err(e) => {
            cleanup_screen(&mut out);
            out.finish(SCREEN_FINISH);
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
    // capture on, a drag selects within one tile via atrium::select; the host's
    // native selection falls back to Shift-drag.) The scanner and the
    // terminal are kept in sync by the toggle.
    let mut mouse_on = false;
    // A drag-selection in progress inside one tile (mouse on, tiled window), with
    // the window it was started in: press starts it, drags extend it, release
    // copies it (atrium::select).
    let mut selection: Option<(usize, atrium::select::Selection)> = None;
    // Whether atrium has forced the OUTER terminal's mouse reporting off because the
    // focused pane does not want the mouse (a shell/WSL). A mouse app (claude)
    // enables motion tracking via passthrough, which leaks to the terminal; once
    // you switch to a non-mouse pane the terminal keeps sending motion events and
    // they get forwarded into that pane as garbage text. See the re-assert below.
    let mut outer_mouse_off = false;
    // The full-screen overlays (overview / board / activity log) and their cursors.
    let mut views = Views::default();
    // The command prompt (`Ctrl+A :`): `Some(line)` while the operator is typing a
    // command to open in a new pane (any shell/program, not just the launch one).
    // Keystrokes edit the line instead of reaching the panes; Enter opens it.
    let mut prompt: Option<String> = None;
    let mut buf = [0u8; 8192];
    let mut force_repaint = true;
    let mut last_size_check = Instant::now();

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
    //
    // Without an explicit file, a session with a place in the store keeps its board
    // and bus there, beside its snapshot, so a resume brings back the team's
    // subscriptions, unread events and board — which lived only in this process
    // before, and died with it. A resume starts from the resumed session's copies.
    let sidecar_of = |kind: &str| {
        session_location().map(|(_, file, _)| atrium::session_store::sidecar(file, kind))
    };
    let mut board = match std::env::var_os(atrium::board::ENV_BOARD) {
        Some(p) if !p.is_empty() => atrium::board::Board::with_file(std::path::PathBuf::from(p)),
        _ => match sidecar_of("board") {
            Some(path) => atrium::board::Board::with_file_deferred(path),
            None => atrium::board::Board::new(),
        },
    };
    // The shared pub/sub bus (coordination layer part 2): the team's event stream,
    // in-memory unless ATRIUM_BUS names a snapshot file. Same ctl gating as the board.
    let mut bus = match std::env::var_os(atrium::bus::ENV_BUS) {
        Some(p) if !p.is_empty() => atrium::bus::Bus::with_file(std::path::PathBuf::from(p)),
        _ => match sidecar_of("bus") {
            Some(path) => atrium::bus::Bus::with_file_deferred(path),
            None => atrium::bus::Bus::new(),
        },
    };
    // Sidecar writes leave the loop: a deferred bus and board hand their state out
    // at most once per snapshot interval, and this thread writes it.
    let sidecar_writer = atrium::session_store::SidecarWriter::start();
    let mut last_sidecar_flush = Instant::now();
    // A fleet that declared a topic vocabulary switches the bus to strict
    // admission; without one it stays soft-gated. Set before any pane is spawned,
    // so the very first publish is already governed by the right policy.
    if let Some(topics) = &canonical_topics {
        bus.set_canonical(topics);
    }
    // Agent state is refreshed on its own thread (`AgentWatch`); the loop holds the
    // newest snapshot and never reads a transcript itself.
    let agents = atrium::vendors::AgentWatch::start(agsess::sessions::now_ms());
    let mut world = atrium::vendors::AgentState::default();
    // Session teardown container: on Windows a kill-on-close Job Object so no pane
    // tree outlives atrium however it dies (TerminateProcess included); a no-op on
    // unix (the process-group teardown + watchdog below already cover the tree).
    // Held for the whole run — dropping it (or the process exiting) fires the
    // guarantee. Panes never inherit its handle, so they live until atrium exits.
    // Process-wide, so every pane — a fleet's, spawned before this loop, included
    // — is enrolled at its own spawn; closed explicitly at teardown below.
    let session_job = atrium::reap::SessionJob::session();
    // What `Ctrl+A c`, splits and the command prompt launch under.
    let launch = Launch {
        command,
        identity,
        job: session_job,
    };
    // The crash registry, session snapshot, warden and orphan watchdog (loop
    // phase 0b); see `safety_net`.
    let mut safety_net = SafetyNet::new();
    // The paint's memory between ticks (retained tiled frame, throttles); see
    // `renderer`.
    let mut renderer = Renderer::new();

    // What wakes the loop: keys on their own thread, and a reader thread per
    // pane (started at the top of each tick), all ringing one doorbell.
    let (bell, wake) = atrium::events::doorbell();
    let mut keys = atrium::events::KeyReader::start(term.input(), bell.clone()).ok();

    // The read at the top can fail (terminal gone) *and* commands deep inside
    // `break 'outer`; a labeled `loop` expresses both. clippy's while-let
    // rewrite can't host the labeled break, so allow it here.
    // Set only where the operator quit or every pane ended. A termination signal
    // (SIGHUP from a closed window, SIGTERM from a shutdown) and a lost terminal
    // leave through the same teardown but are NOT deliberate — the session was
    // taken away, and the next launch should offer it back.
    let mut deliberate_exit = false;
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
        // 0b. The safety net: crash registry, session snapshot, warden tripwires,
        // and (unix) the orphan watchdog.
        if safety_net.tick(&windows, session_job, &mut ctl_audit, &mut bus, &mut flash) {
            force_repaint = true;
        }
        if last_sidecar_flush.elapsed() >= SNAPSHOT_INTERVAL {
            last_sidecar_flush = Instant::now();
            for write in [bus.take_pending_write(), board.take_pending_write()]
                .into_iter()
                .flatten()
            {
                sidecar_writer.submit(write);
            }
        }
        // 1. wait for a key or pane output — woken the moment either arrives —
        //    or at most a tick, for the loop's timed work. Then keystrokes ->
        //    scanner -> focused pane / commands.
        start_pane_readers(&mut windows, &bell);
        let bytes = match &keys {
            Some(keys) => {
                wake.wait(LOOP_TICK);
                match keys.take() {
                    Ok(b) => b,
                    Err(_) => break,
                }
            }
            // No key thread could be started: wait on the keyboard as before.
            None => match term.read_bytes(Duration::from_millis(15)) {
                Ok(b) => b,
                Err(_) => break,
            },
        };
        if !bytes.is_empty() && std::env::var_os("ATRIUM_DEBUG").is_some() {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            eprint!("[atrium-dbg stdin {}]\r\n", hex.join(" "));
        }
        // The command prompt captures keystrokes: while it is open, keys edit the
        // command line (not the panes), and the scanner is fed nothing. Enter opens
        // the typed command in a new pane; Esc/Ctrl+C cancels.
        let feed: &[u8] = if prompt.is_some() {
            feed_prompt(
                &mut prompt,
                &bytes,
                &mut windows,
                &mut active,
                &launch,
                rows,
                cols,
                &mut out,
                &mut flash,
            )
            .apply(&mut renderer, &mut force_repaint);
            b""
        } else {
            &bytes
        };
        for action in scanner.feed(feed) {
            // Keys that arrived in the same read as the `Ctrl+A :` that opened
            // the prompt belong to it (a paste or a fast typist), not the pane.
            if let (Some(_), Action::Forward(b)) = (&prompt, &action) {
                feed_prompt(
                    &mut prompt,
                    b,
                    &mut windows,
                    &mut active,
                    &launch,
                    rows,
                    cols,
                    &mut out,
                    &mut flash,
                )
                .apply(&mut renderer, &mut force_repaint);
                continue;
            }
            if let Some(outcome) = handle_overlay_key(
                &action,
                &mut views,
                &mut windows,
                &world,
                &board,
                &mut bus,
                &mut active,
                rows,
                cols,
                &mut out,
                &mut flash,
            ) {
                if outcome.apply(&mut renderer, &mut force_repaint) {
                    deliberate_exit = true;
                    break 'outer;
                }
                continue;
            }
            match action {
                Action::Forward(b) => {
                    if let Some(p) = windows[active].focused_mut() {
                        p.draft.observe(&b, Instant::now());
                        let _ = p.pty.write(&b);
                    }
                }
                Action::NextPane
                | Action::PrevPane
                | Action::SwitchTo(_)
                | Action::NewPane
                | Action::NewShellPane
                | Action::SplitH
                | Action::SplitV
                | Action::MoveFocus(_)
                | Action::Zoom
                | Action::KillPane => {
                    handle_pane_action(
                        &action,
                        &mut windows,
                        &mut active,
                        &launch,
                        rows,
                        cols,
                        &mut out,
                        &mut flash,
                    )
                    .apply(&mut renderer, &mut force_repaint);
                }
                // With no overlay up these only open one (closing, and switching
                // between overlays, is `handle_overlay_key`'s). Opening is a full
                // repaint: the overlay replaces the panes.
                Action::ToggleBoard => {
                    views.open_board();
                    renderer.reset();
                    force_repaint = true;
                }
                Action::OpenPrompt => {
                    // Start the command prompt; subsequent keystrokes edit the line
                    // (handled above the scanner) until Enter/Esc.
                    prompt = Some(String::new());
                    force_repaint = true;
                }
                Action::ToggleOverview => {
                    views.open_overview(&windows, active, &world);
                    let _ = write!(out, "\x1b[?25h");
                    renderer.reset();
                    force_repaint = true;
                }
                Action::ToggleLog => {
                    views.open_log();
                    let _ = write!(out, "\x1b[?25h");
                    renderer.reset();
                    force_repaint = true;
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
                                "mouse: ON — click focuses, drag selects within a tile (copies), wheel scrolls"
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
                Action::MouseClick { .. }
                | Action::MouseDrag { .. }
                | Action::MouseRelease { .. }
                | Action::MouseScroll { .. } => {
                    handle_mouse(
                        &action,
                        &mut windows,
                        active,
                        &mut selection,
                        rows,
                        cols,
                        &mut out,
                        &mut flash,
                    )
                    .apply(&mut renderer, &mut force_repaint);
                }
                Action::Quit => {
                    if std::env::var_os("ATRIUM_DEBUG").is_some() {
                        eprint!("[atrium-dbg quit-received]\r\n");
                    }
                    deliberate_exit = true;
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
                            session_job,
                        );
                        let _ = listener.respond(&reply.to_json());
                        renderer.reset();
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
        let drained = drain_active_window(&mut windows[active], views.any(), &mut buf, &mut out);
        if drained.force_repaint {
            force_repaint = true;
        }
        // 2b. hand the focused pane off from the startup logo once it has drawn
        //     something and the logo has been up long enough to be seen.
        if finish_splash(
            &mut windows[active],
            views.any(),
            renderer.splash_hold_over(),
            &mut out,
        ) {
            force_repaint = true;
        }
        let tiled_dirty = drained.tiled_dirty;

        // 3. background windows: drain (discarded — emulator/ConPTY keep the
        //    screen) and flag activity so the bar shows it.
        drain_background_windows(&mut windows, active, &mut buf);

        // 4. reap exits; drop dead panes and empty windows, re-tiling.
        let layout_changed = drop_exited_panes(&mut windows);
        if layout_changed {
            let empty_before_active = windows[..active]
                .iter()
                .filter(|w| w.panes.is_empty())
                .count();
            windows.retain(|w| !w.panes.is_empty());
            if windows.is_empty() {
                // Every agent ended on its own: nothing crashed, nothing to resume.
                deliberate_exit = true;
                break;
            }
            active = active
                .saturating_sub(empty_before_active)
                .min(windows.len() - 1);
            resize_window(&mut windows[active], rows, cols);
            renderer.reset();
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
            if let Some((r, c)) = size_watch.latest() {
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
                    // linger; and a tiled recompose diffs against the retained
                    // frame, so without this clear + `renderer.reset()` the first frame
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
                    // SIGWINCH/redraw ONLY because the two sizes differ, which holds
                    // because this branch is gated on `r >= 3` (so `ar >= 2` and
                    // `ar - 1 != ar`): ConPTY emits no event for a same-size resize,
                    // so lowering that floor silently voids the guarantee.
                    // (The `force_repaint` path below only nudges
                    // once the pane has painted, so it misses exactly the boot
                    // window the founder hit; this covers it.)
                    if !windows[active].tiled() {
                        let ar = rows.saturating_sub(1).max(1);
                        let focus = windows[active].tree.focus();
                        if let Some(p) = windows[active].pane_mut(focus) {
                            // The first resize is only the redraw nudge and may fail
                            // harmlessly. The emulator follows the SECOND, so the pty
                            // and emulator are never left at different sizes; a
                            // failure is surfaced instead of desyncing silently
                            // (r10 audit B8).
                            let _ = p.pty.resize(ar.saturating_sub(1).max(1), cols);
                            match p.pty.resize(ar, cols) {
                                Ok(()) => p.term.resize(ar as usize, cols as usize),
                                Err(e) => {
                                    flash =
                                        Some((format!("pane resize failed: {e}"), Instant::now()))
                                }
                            }
                        }
                    }
                    // Force a full recompose at the new size: tiled repaints via
                    // `render_full` (which re-clears + paints every cell),
                    // passthrough nudges the focused pty to redraw.
                    renderer.reset();
                    force_repaint = true;
                }
            }
        }

        // 5b. agent state (§5), from the `AgentWatch` thread. Refresh every second
        //     while anything on screen shows agent state — an agent pane, or the
        //     overview / activity log — and every five seconds otherwise.
        let agent_on_screen = views.overview
            || views.log
            || windows.iter().any(|w| {
                w.panes.iter().any(|p| {
                    p.session_id.is_some() || atrium::vendors::vendor_for_stem(&p.title).is_some()
                })
            });
        agents.set_interval(if agent_on_screen {
            atrium::vendors::AgentWatch::ACTIVE
        } else {
            atrium::vendors::AgentWatch::QUIET
        });
        let mut did_refresh = false;
        if let Some(snapshot) = agents.latest() {
            world = snapshot;
            did_refresh = true;
            // Keep the overview and activity log live but calm: repaint once per
            // refresh (~1 Hz), not every tick, so they update without flicker.
            if views.overview || views.log {
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
            adopt_sessions(&mut windows, &world);
        }

        // 5d. The terminal fell behind, output was discarded, and it has now
        //     caught up: what it shows is stale, so repaint the whole view from
        //     the emulators — the scroll region, the focused passthrough pane,
        //     and (via reset + force_repaint) the tiled frame, any overlay and
        //     the bar. The mouse-off assertion may have been discarded too.
        if out.take_resync() {
            let _ = write!(out, "\x1b[1;{}r\x1b[2J\x1b[H", rows - 1);
            if !views.any() && !windows[active].tiled() {
                let fp = windows[active].tree.focus();
                if windows[active].pane(fp).is_some_and(|p| p.painted) {
                    repaint_focused(&mut windows[active], rows, cols, &mut out);
                }
            }
            outer_mouse_off = false;
            renderer.reset();
            force_repaint = true;
        }

        // 6-7. paint the active window and the bar as one synchronized frame.
        renderer.paint(
            &mut windows,
            active,
            &mut views,
            &selection,
            prompt.as_deref(),
            &world,
            &board,
            &bus,
            &mut flash,
            force_repaint,
            tiled_dirty,
            drained.pane_output,
            rows,
            cols,
            &mut out,
        );
        force_repaint = false;
        // Hand anything a phase wrote without flushing to the terminal this tick.
        let _ = out.flush();
    }

    let dbg = std::env::var_os("ATRIUM_DEBUG").is_some();
    // Stop reading keys before the caller restores the terminal.
    if let Some(k) = keys.as_mut() {
        k.stop();
    }
    // The upkeep thread first: it must not be stopping builds while the panes
    // are torn down, nor holding the session job's handle when it is closed.
    safety_net.stop_upkeep();
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
    // The panes are gone, so nothing holds the compile pool; remove its FIFO
    // (unix) rather than leave a stale path in the temp directory.
    atrium::buildpool::cleanup();
    match atrium::reap::settle_registry(
        safety_net.registry_path(),
        &survivors,
        trust_mode().policy_label(),
    ) {
        Ok(false) => {}
        Ok(true) => eprintln!(
            "atrium: warning: {} pane process group(s) survived teardown: {:?}",
            survivors.len(),
            survivors
        ),
        // The record could not be settled: say so, with the groups a manual
        // cleanup would need, instead of implying the watchdog has them.
        Err(e) => eprintln!(
            "atrium: warning: could not update the crash registry {}: {e}; surviving pane \
             process group(s), if any, are not recorded for the watchdog: {:?}",
            safety_net.registry_path().display(),
            survivors
        ),
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
    // Deliver the restore before the process exits and takes the writer thread
    // with it — bounded, so a terminal that never reads can't hold atrium open.
    out.finish(SCREEN_FINISH);
    // A deliberate exit: mark the snapshot closed so the next launch in this
    // project does not offer to resume what the operator chose to end. After the
    // screen is restored, so a failure is readable.
    // Whatever the bus and board changed since the last flush, then wait for the
    // writer: a resume should find the team as it was when the session ended.
    for write in [bus.take_pending_write(), board.take_pending_write()]
        .into_iter()
        .flatten()
    {
        sidecar_writer.submit(write);
    }
    sidecar_writer.finish();
    let settled = if deliberate_exit {
        safety_net.settle_session()
    } else {
        Ok(())
    };
    if let Err(e) = settled {
        eprintln!(
            "atrium: warning: could not mark the session snapshot closed ({e}); the next \
             launch here may offer to resume this session"
        );
    }
    if dbg {
        eprint!("[atrium-dbg cleaned]\r\n");
    }
    drop(windows);
    if dbg {
        eprint!("[atrium-dbg panes-dropped]\r\n");
    }
    // The session job is static, so nothing drops it: close it here, where the
    // run loop's own job used to go out of scope, so kill-on-close still fires
    // before a fleet's worktree teardown rather than at process exit.
    session_job.close();
    ExitCode::SUCCESS
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

#[cfg(test)]
mod tests {
    use super::{
        assemble_command, audit_outcome, combine_system_prompt, fold_system_prompt, pane_base_env,
        privilege_for, routed_wake, sanitize_shim_arg, surface_once, worktree_spawn_params,
    };

    // -- audit_outcome over the typed Reply (r10 B12) ------------------------

    #[test]
    fn audit_outcome_names_the_salient_ids_from_the_typed_reply() {
        use atrium::ctl::{self, AgentId};
        assert_eq!(
            audit_outcome(&ctl::reply_err("nope")),
            (false, "nope".to_string())
        );
        assert_eq!(
            audit_outcome(&ctl::reply_spawned(AgentId(3), Some("dev"), None, None)),
            (true, "pane=3".to_string())
        );
        assert_eq!(
            audit_outcome(&ctl::reply_status_one(AgentId(2), None, 0)),
            (true, "pane=2".to_string())
        );
        assert_eq!(
            audit_outcome(&ctl::reply_killed(&[AgentId(1), AgentId(4)])),
            (true, "killed=1,4".to_string())
        );
        assert_eq!(
            audit_outcome(&ctl::reply_sent(AgentId(5), false)),
            (true, "target=5".to_string())
        );
        assert_eq!(
            audit_outcome(&ctl::reply_bus_resolved(7, true)),
            (true, String::new())
        );
    }

    // -- surface_once (r10 B8) -------------------------------------------------

    #[test]
    fn a_failure_streak_flashes_once_and_success_resets_it() {
        let fail = || -> std::io::Result<u8> {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "disk full"))
        };
        let (mut failing, mut flash) = (false, None);

        assert_eq!(
            surface_once(&mut failing, fail(), "snapshot", &mut flash),
            None
        );
        let first = flash.take().expect("the first failure is surfaced");
        assert_eq!(first.0, "snapshot failed: disk full");

        // Still failing: quiet, so a persistent error doesn't strobe the bar.
        assert_eq!(
            surface_once(&mut failing, fail(), "snapshot", &mut flash),
            None
        );
        assert!(flash.is_none(), "a repeat failure re-flashed");

        // Recovery returns the value and re-arms the next streak.
        assert_eq!(
            surface_once(&mut failing, Ok(7), "snapshot", &mut flash),
            Some(7)
        );
        surface_once(&mut failing, fail(), "snapshot", &mut flash);
        assert!(
            flash.is_some(),
            "a new streak after recovery must surface again"
        );
    }

    // -- assemble_command pure seam ------------------------------------------

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_resolved_shim_is_spawned_directly_not_wrapped_in_cmd() {
        // pty owns the cmd.exe line for batch targets; wrapping here would put
        // the args back under plain argv quoting (the r10 B1 bug).
        let cmd = argv(&["claude", "-p", "a & b\nnext", "--permission-mode", "auto"]);
        let got = assemble_command(&cmd, Some(r"C:\npm\claude.cmd".into()));
        assert_eq!(
            got,
            argv(&[
                r"C:\npm\claude.cmd",
                "-p",
                "a & b next",
                "--permission-mode",
                "auto"
            ])
        );
    }

    #[test]
    fn a_resolved_exe_keeps_its_args_verbatim() {
        let cmd = argv(&["node", "multi\nline", "x&y"]);
        let got = assemble_command(&cmd, Some(r"C:\node\node.exe".into()));
        assert_eq!(got, argv(&[r"C:\node\node.exe", "multi\nline", "x&y"]));
    }

    #[test]
    fn an_unresolved_command_is_left_for_the_spawn_to_report() {
        let cmd = argv(&["nope", "a"]);
        assert_eq!(assemble_command(&cmd, None), cmd);
    }

    // -- sanitize_shim_arg pure seam -------------------------------------------

    #[test]
    fn sanitize_shim_arg_collapses_all_newline_forms() {
        // CRLF counts as one space, lone CR and LF each become one space.
        assert_eq!(sanitize_shim_arg("a\nb\r\nc"), "a b c");
    }

    #[test]
    fn sanitize_shim_arg_leaves_single_line_unchanged() {
        assert_eq!(sanitize_shim_arg("hello"), "hello");
    }

    #[test]
    fn sanitize_shim_arg_leaves_angle_brackets_and_backticks_untouched() {
        let s = "<name> `echo hi`";
        assert_eq!(sanitize_shim_arg(s), s);
    }

    /// Regression guard for the r8 fleet launch bug: a multi-line kickoff
    /// prompt caused `cmd.exe` to truncate the command at the first newline,
    /// dropping `--permission-mode auto` and `--session-id` so the pane ran in
    /// the wrong mode under a self-generated session id.
    #[test]
    fn sanitize_shim_arg_regression_trailing_flags_survive_newline_in_prompt() {
        let args: Vec<String> = vec![
            "--model".into(),
            "opus".into(),
            "first line\nsecond line".into(), // multi-line prompt
            "--permission-mode".into(),
            "auto".into(),
            "--session-id".into(),
            "X".into(),
        ];
        let sanitized: Vec<String> = args.iter().map(|a| sanitize_shim_arg(a)).collect();
        assert_eq!(sanitized.len(), 7, "no args dropped");
        assert_eq!(sanitized[2], "first line second line", "prompt collapsed");
        assert_eq!(sanitized[3], "--permission-mode");
        assert_eq!(sanitized[4], "auto");
        assert_eq!(sanitized[5], "--session-id");
        assert_eq!(sanitized[6], "X");
    }

    // -- combine_system_prompt pure seam ------------------------------------

    #[test]
    fn combine_both_blocks_into_one_payload() {
        // The key regression guard: norms + ctl must both survive in a single
        // block so callers push exactly ONE --append-system-prompt flag.
        let result = combine_system_prompt(Some("NORMS"), Some("CTL")).unwrap();
        assert!(result.contains("NORMS"), "norms must be present: {result}");
        assert!(
            result.contains("CTL"),
            "ctl directive must be present: {result}"
        );
        // Exactly one block — no embedded flag that would re-introduce last-wins.
        assert!(
            !result.contains("--append-system-prompt"),
            "payload must not contain the flag itself: {result}"
        );
    }

    #[test]
    fn combine_norms_only_returns_norms() {
        let result = combine_system_prompt(Some("NORMS"), None).unwrap();
        assert_eq!(result, "NORMS");
    }

    #[test]
    fn combine_ctl_only_returns_ctl() {
        let result = combine_system_prompt(None, Some("CTL")).unwrap();
        assert_eq!(result, "CTL");
    }

    #[test]
    fn combine_neither_returns_none() {
        assert!(combine_system_prompt(None, None).is_none());
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_fleet_agents_own_prompt_survives_the_ctl_block() {
        // A fleet agent's `prompt` key is already in its command as
        // --append-system-prompt. Pushing the ctl/norms block as a second flag
        // made claude (last-wins) silently drop the agent's own instructions.
        let cmd = args(&[
            "claude",
            "--append-system-prompt",
            "ROLE",
            "--model",
            "opus",
            "go",
            "--append-system-prompt=INLINE",
        ]);
        let out = fold_system_prompt(cmd, Some("BLOCK"));
        let flags: Vec<_> = out
            .iter()
            .filter(|a| a.starts_with("--append-system-prompt"))
            .collect();
        assert_eq!(flags.len(), 1, "exactly one flag: {out:?}");
        let payload = out.last().unwrap();
        assert_eq!(payload, "ROLE\n\nINLINE\n\nBLOCK");
        assert_eq!(&out[..4], &args(&["claude", "--model", "opus", "go"])[..]);
    }

    #[test]
    fn fold_without_a_block_leaves_the_command_untouched() {
        let cmd = args(&["claude", "--append-system-prompt", "ROLE", "go"]);
        assert_eq!(fold_system_prompt(cmd.clone(), None), cmd);
    }

    fn wake_fields(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn routed_message_wakes_the_target_with_its_content() {
        // The footgun this fixes: a `--to` message sat unseen on the bus and an
        // idle target never acted on it. The wake must carry the content so the
        // target does not have to be subscribed to the topic to receive it.
        let f = wake_fields(&[("to", "lead"), ("msg", "ship it")]);
        let (to, text) =
            routed_wake(&f, "ctl-fixes", 42, atrium::bus::Kind::Fyi, "reviewer").unwrap();
        assert_eq!(to, "lead");
        assert!(text.contains("ship it"), "carries the content: {text}");
        assert!(text.contains("reviewer"), "names the sender");
        assert!(text.contains("ctl-fixes"), "names the topic");
        assert!(text.contains("#42"), "carries the seq to resolve/reference");
    }

    #[test]
    fn a_decision_question_is_the_body_when_no_msg() {
        let f = wake_fields(&[("to", "lead"), ("q", "combined or split?")]);
        let (_, text) = routed_wake(&f, "t", 1, atrium::bus::Kind::DecisionNeeded, "gate").unwrap();
        assert!(text.contains("combined or split?"));
    }

    #[test]
    fn an_unrouted_publish_wakes_nobody() {
        let f = wake_fields(&[("msg", "broadcast")]);
        assert!(routed_wake(&f, "t", 1, atrium::bus::Kind::Fyi, "lead").is_none());
    }

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
        let bare = pane_base_env(Some(&key), None, None);
        assert_eq!(
            bare,
            vec![("ATRIUM_SESSION".to_string(), "17686:99".to_string())],
            "a pane without ctl must still be findable"
        );

        // With ctl on, the marker is still there, still first, and the three ctl
        // variables are unchanged. Spelled as literals on purpose: this pins the
        // WIRE names a child reads, so renaming a const without the child is caught.
        let full = pane_base_env(Some(&key), Some(("/tmp/sock", "3", "deadbeef")), None);
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
        assert!(pane_base_env(None, None, None).is_empty());
        let ctl_only = pane_base_env(None, Some(("/tmp/sock", "3", "deadbeef")), None);
        assert!(!ctl_only.iter().any(|(k, _)| k == "ATRIUM_SESSION"));
        assert_eq!(ctl_only.len(), 3);
    }

    /// Every pane gets the session's compile pool, with or without ctl, under
    /// the wire name cargo reads. A pane that misses it builds unpooled — the
    /// exact fleet OOM this exists to prevent — and nothing would say so.
    #[test]
    fn every_pane_gets_the_build_pool() {
        let flags = "-j --jobserver-fds=atrium-build-1-ab --jobserver-auth=atrium-build-1-ab";
        let bare = pane_base_env(None, None, Some(flags));
        assert_eq!(
            bare,
            vec![("CARGO_MAKEFLAGS".to_string(), flags.to_string())]
        );
        let full = pane_base_env(None, Some(("/tmp/sock", "3", "deadbeef")), Some(flags));
        assert!(full.contains(&("CARGO_MAKEFLAGS".to_string(), flags.to_string())));
        // No pool (disabled, inherited, or failed): nothing is injected.
        assert!(pane_base_env(None, None, None).is_empty());
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
        assert!(!privilege_for(None, Some((None, 0))));
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

    /// The startup logo holds until it has been seen, but never hides a pane that
    /// has nothing to show, and never outlives a pane that already exited.
    #[test]
    fn the_startup_logo_holds_until_seen_unless_the_pane_is_done() {
        use super::splash_may_hand_off as may;
        assert!(!may(true, false, false), "a fast shell waits out the hold");
        assert!(may(true, true, false), "then hands off");
        assert!(!may(false, true, false), "nothing drawn yet: keep the logo");
        assert!(
            may(true, false, true),
            "an exited one-shot shows its output at once"
        );
    }

    /// An open synchronized update is only abandoned once the pane has been
    /// quiet past the limit; a working app's frame (bytes still arriving) and a
    /// pane not in an update are never touched.
    #[test]
    fn a_sync_frame_is_stuck_only_when_open_and_long_quiet() {
        use super::sync_frame_stuck as stuck;
        use std::time::Duration;
        let limit = Duration::from_millis(250);
        let cases: &[(bool, u64, bool, &str)] = &[
            (true, 251, true, "open and quiet past the limit: stuck"),
            (true, 5000, true, "open and long quiet: stuck"),
            (true, 250, false, "open, quiet exactly the limit: not yet"),
            (true, 10, false, "open, bytes still arriving: a live frame"),
            (false, 5000, false, "not in an update: nothing to close"),
            (false, 0, false, "not in an update, busy: nothing to close"),
        ];
        for &(open, quiet_ms, want, label) in cases {
            assert_eq!(
                stuck(open, Duration::from_millis(quiet_ms), limit),
                want,
                "{label}"
            );
        }
    }

    /// The fleet banner says a governed flag in `cmd` is ignored, so the launch
    /// must actually drop it — with its value — and keep everything else.
    #[test]
    fn a_fleet_agent_is_launched_without_the_flags_the_banner_ignored() {
        use atrium::fleet::{Agent, AgentPlan};
        let agent = Agent {
            name: "lead".to_string(),
            cmd: vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
                "--allowedTools=Bash(*)".to_string(),
                "--model".to_string(),
                "opus".to_string(),
            ],
            prompt: Some("--allowedTools is atrium's to set".to_string()),
            ..Default::default()
        };
        let disclosed = AgentPlan {
            label: "lead".to_string(),
            identity: None,
            opaque: None,
            cwd: None,
            add_dirs: vec![],
        };
        let (argv, _, _) = super::fleet_launch(&agent, &disclosed);
        assert_eq!(
            argv,
            vec![
                "claude",
                "--model",
                "opus",
                "--append-system-prompt",
                "--allowedTools is atrium's to set",
            ]
        );
    }

    /// A token that matches no live pane is stale or forged — not the operator.
    #[test]
    fn a_token_resolving_to_no_live_pane_is_not_the_operator() {
        assert!(!privilege_for(Some(AgentId(7)), None));
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
        assert!(privilege_for(Some(AgentId(1)), Some((None, 0))));
        assert!(!privilege_for(
            Some(AgentId(2)),
            Some((Some(AgentId(1)), 1))
        ));
    }

    /// A worker whose parent link is GONE is still a worker.
    ///
    /// `atrium recover` resolves each worker's parent through the pane that
    /// spawned it, so a parent that had already exited resolves to `None`. With
    /// the gate keyed on the parent alone, that promoted the worker to operator
    /// across a recovery — session-wide `send`/`kill`/`respawn` and the right to
    /// delegate any identity in the vault. Depth is recorded independently and
    /// survives, so requiring both is what closes it.
    #[test]
    fn a_worker_that_lost_its_parent_link_is_not_promoted_to_operator() {
        assert!(
            !privilege_for(Some(AgentId(3)), Some((None, 1))),
            "depth > 0 is a worker no matter what happened to its parent link"
        );
        assert!(!privilege_for(Some(AgentId(4)), Some((None, 7))));
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

    /// r10 B10: the unit thresholds are exactly where an off-by-one hides.
    #[test]
    fn ago_switches_units_exactly_at_the_thresholds() {
        let s = |secs: u64| ago(1_000_000_000 + secs * 1000, 1_000_000_000);
        assert_eq!(s(0), "0s");
        assert_eq!(s(59), "59s");
        assert_eq!(s(60), "1m");
        assert_eq!(s(3599), "59m");
        assert_eq!(s(3600), "1h");
        assert_eq!(s(86_399), "23h");
        assert_eq!(s(86_400), "1d");
        // A timestamp from the future (clock skew) saturates, never underflows.
        assert_eq!(ago(1_000, 5_000), "0s");
    }

    /// r10 B10: wrap_to's edges — empty input, an exact fit, a word longer than
    /// the width (kept whole, not broken), and overflow marking.
    #[test]
    fn wrap_to_edge_cases() {
        assert!(wrap_to("", 10, 3).is_empty());
        assert!(wrap_to("   ", 10, 3).is_empty());
        // "aa bb" is exactly 5 columns: it fits on one line.
        assert_eq!(wrap_to("aa bb cc", 5, 3), vec!["aa bb", "cc"]);
        // An over-width single word is emitted whole on its own line.
        assert_eq!(wrap_to("abcdefghij", 4, 3), vec!["abcdefghij"]);
        // Exactly max_lines of content: no ellipsis.
        assert_eq!(wrap_to("aa bb", 2, 2), vec!["aa", "bb"]);
        // More than fits: the last kept line is cut to width-1 plus the marker.
        assert_eq!(wrap_to("aa bb cc dd", 2, 2), vec!["aa", "b…"]);
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
        let world = atrium::vendors::AgentState::default(); // no sessions
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

    // -- worktree_spawn_params error-path -------------------------------------

    /// A non-git directory must produce a clear `Err` rather than silently
    /// returning a path that was never created (the original bug).
    #[test]
    fn worktree_spawn_params_non_git_dir_returns_err() {
        let tmp = std::env::temp_dir().join("atrium_test_non_git");
        std::fs::create_dir_all(&tmp).ok();
        let result = worktree_spawn_params(&tmp, Some("my-wt"));
        assert!(
            result.is_err(),
            "expected Err for non-git dir, got Ok: {result:?}"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("could not create worktree"),
            "message should identify the failure: {msg}"
        );
    }

    /// A real git repo must produce `Ok` with a `Some` dir that actually exists.
    #[test]
    fn worktree_spawn_params_git_repo_yields_ok_and_creates_dir() {
        let tmp = std::env::temp_dir().join("atrium_test_git_repo");
        // Clean slate so the test is idempotent.
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // Initialise a minimal git repo with one commit so `worktree add` has HEAD.
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&tmp)
                .output()
        };
        git(&["init"]).unwrap();
        // An explicit identity: on a machine with no global git user (a fresh
        // Linux box, CI) the commit silently fails and there is no HEAD to branch.
        let commit = git(&[
            "-c",
            "user.name=atrium-test",
            "-c",
            "user.email=atrium-test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .unwrap();
        assert!(commit.status.success(), "fixture commit failed: {commit:?}");
        let result = worktree_spawn_params(&tmp, Some("test-wt"));
        assert!(result.is_ok(), "expected Ok for valid git repo: {result:?}");
        let (dir, norms) = result.unwrap();
        let dir = dir.expect("dir must be Some");
        let norms = norms.expect("norms must be Some");
        assert!(
            std::path::Path::new(&dir).exists(),
            "worktree directory must have been created: {dir}"
        );
        assert!(!norms.is_empty(), "norms must not be empty");
        // Cleanup.
        let _ = std::fs::remove_dir_all(&tmp);
    }

    use atrium::ctl::TrustMode;

    // --- resume_argv --------------------------------------------------------

    fn pane_record(argv: Vec<String>, session_id: Option<String>) -> atrium::session::PaneRecord {
        atrium::session::PaneRecord {
            id: 0,
            role: None,
            argv,
            cwd: None,
            identity: None,
            session_id,
            worktree: None,
            deny: Vec::new(),
            can_spawn: true,
            depth: 0,
            parent_pane: None,
            mode: None,
            kickoff: false,
            norms: None,
            context_env: Vec::new(),
        }
    }

    #[test]
    fn resume_argv_claude_session_id_appends_resume_no_session_id_flag() {
        let r = pane_record(vec!["claude".into()], Some("abc-123".into()));
        let got = super::resume_argv(&r);
        assert_eq!(got, vec!["claude", "--resume", "abc-123"]);
        assert!(!got.iter().any(|a| a == "--session-id"));
    }

    #[test]
    fn resume_argv_claude_continue_stripped_and_replaced_with_resume() {
        let r = pane_record(
            vec!["claude".into(), "--continue".into()],
            Some("abc-123".into()),
        );
        let got = super::resume_argv(&r);
        assert!(!got.contains(&"--continue".to_string()));
        assert!(got.contains(&"--resume".to_string()));
        assert!(got.contains(&"abc-123".to_string()));
    }

    #[test]
    fn resume_argv_non_claude_verbatim() {
        let r = pane_record(vec!["bash".into(), "-l".into()], Some("any-id".into()));
        let got = super::resume_argv(&r);
        assert_eq!(got, vec!["bash", "-l"]);
    }

    #[test]
    fn resume_argv_claude_no_session_id_unchanged() {
        let r = pane_record(vec!["claude".into(), "--continue".into()], None);
        let got = super::resume_argv(&r);
        // no session_id → argv returned verbatim, --continue preserved
        assert_eq!(got, vec!["claude", "--continue"]);
    }

    /// A pane whose directory is gone starts in the project directory, loses
    /// the worktree instructions that would now be false, and says so; one
    /// whose directory exists keeps both, quietly.
    #[test]
    fn a_pane_whose_directory_is_gone_starts_in_the_project_without_its_norms() {
        let mut r = pane_record(vec!["claude".into()], None);
        r.role = Some("builder".into());
        r.cwd = Some("/wt/piece".into());
        r.norms = Some("You are in git worktree piece".into());
        let gone = super::resume_place(&r, |_| false);
        assert_eq!(gone.cwd, None);
        assert_eq!(gone.norms, None);
        let w = gone.warning.expect("the operator is told");
        assert!(w.starts_with("builder: "), "{w}");
        assert!(w.contains("/wt/piece is gone"), "{w}");
        let kept = super::resume_place(&r, |_| true);
        assert_eq!(kept.cwd.as_deref(), Some("/wt/piece"));
        assert_eq!(kept.norms.as_deref(), Some("You are in git worktree piece"));
        assert_eq!(kept.warning, None);
        // No directory recorded: nothing to check, nothing to say.
        r.cwd = None;
        let none = super::resume_place(&r, |_| false);
        assert_eq!(none.cwd, None);
        assert_eq!(none.warning, None);
    }

    #[test]
    fn resume_argv_empty_argv_no_panic() {
        let r = pane_record(vec![], Some("any-id".into()));
        let got = super::resume_argv(&r);
        assert!(got.is_empty());
    }

    fn notice_panes(n: usize) -> Vec<atrium::session::PaneRecord> {
        (0..n)
            .map(|_| pane_record(vec!["claude".into()], None))
            .collect()
    }

    /// The consent line must show what a snapshot actually controls. Approving a
    /// pane *count* let a planted file run any command with any spawn right; each
    /// pane's command, posture, spawn right and own deny rules are now listed, a
    /// governed flag the file tried to smuggle is named as ignored, and a role
    /// carrying an escape sequence cannot repaint the prompt.
    #[test]
    fn the_resume_notice_lists_what_each_pane_will_run_and_may_do() {
        let mut lead = pane_record(
            vec![
                "claude".into(),
                "--allowedTools".into(),
                "Bash(*)".into(),
                "--model".into(),
                "opus".into(),
                "You are the lead of a long-running fleet.".into(),
            ],
            Some("t-lead".into()),
        );
        lead.role = Some("lead\u{1b}[2J".to_string());
        lead.can_spawn = true;
        lead.mode = Some(TrustMode::Skip);
        let mut worker = pane_record(vec!["claude".into()], None);
        worker.role = Some("builder".into());
        worker.can_spawn = false;
        worker.deny = vec!["git push".into()];
        let notice = super::recovery_notice(&[lead, worker], None, false, TrustMode::Edits);
        assert!(
            !notice.contains('\u{1b}'),
            "an ESC from the file reached the terminal"
        );
        assert!(notice.contains("--model opus"), "flags are shown: {notice}");
        assert!(
            !notice.contains("Bash(*)"),
            "a governed flag is not part of what will run: {notice}"
        );
        assert!(notice.contains("ignored from the saved command: --allowedTools"));
        assert!(
            notice.contains("<41 chars>"),
            "a prompt is reduced to its length"
        );
        assert!(notice.contains("--resume t-lead"));
        assert!(
            notice.contains("mode accept"),
            "a recorded `skip` is shown capped to the ceiling: {notice}"
        );
        assert!(notice.contains("may spawn teammates") && notice.contains("cannot spawn"));
        assert!(notice.contains("1 own deny rule(s)"));
    }

    /// A kickoff is the agent's opening instructions. Resuming a transcript the
    /// agent is mid-task, so the kickoff is left out; starting fresh (nothing to
    /// resume) it is exactly what the agent needs, so it stays.
    #[test]
    fn a_kickoff_is_replayed_only_when_the_agent_starts_fresh() {
        let argv = vec![
            "claude".into(),
            "--model".into(),
            "opus".into(),
            "Start by reading CLAUDE.md".into(),
        ];
        let mut resumed = pane_record(argv.clone(), Some("t-1".into()));
        resumed.kickoff = true;
        assert_eq!(
            super::resume_argv(&resumed),
            vec!["claude", "--model", "opus", "--resume", "t-1"]
        );
        let mut fresh = pane_record(argv.clone(), None);
        fresh.kickoff = true;
        assert_eq!(
            super::resume_argv(&fresh),
            argv,
            "no transcript: keep the kickoff"
        );
        // Another vendor's pane can carry an adopted session id, but nothing
        // resumes it — it starts a new session and needs its kickoff.
        let mut codex = pane_record(
            vec!["codex".into(), "Start by reading AGENTS.md".into()],
            Some("adopted".into()),
        );
        codex.kickoff = true;
        assert_eq!(
            super::resume_argv(&codex),
            vec!["codex", "Start by reading AGENTS.md"],
            "a non-claude pane keeps its kickoff"
        );
        let not_a_kickoff = pane_record(argv, Some("t-2".into()));
        assert!(
            super::resume_argv(&not_a_kickoff).contains(&"Start by reading CLAUDE.md".to_string()),
            "only a pane marked as having a kickoff loses its last argument"
        );
    }

    /// The consent notice names the parts of a resume the command line does not
    /// show: worktree instructions and the context store.
    #[test]
    fn the_resume_notice_names_worktree_instructions_and_the_context_store() {
        let mut p = pane_record(vec!["claude".into()], None);
        p.norms = Some("stay in the worktree".into());
        p.context_env = vec![("CONTEXT_MODE_DIR".into(), "/ctx".into())];
        let notice = super::recovery_notice(&[p], None, false, TrustMode::Edits);
        assert!(
            notice.contains("worktree instructions \"stay in the worktree"),
            "the instructions are previewed, not just named: {notice}"
        );
        assert!(
            notice.contains("CONTEXT_MODE_DIR=/ctx"),
            "the context store path is shown: {notice}"
        );
    }

    /// A pane that was itself resumed carries `--resume <id>` in its argv; the next
    /// resume must replace it, not append a second one.
    #[test]
    fn resuming_a_resumed_pane_keeps_exactly_one_session_argument() {
        let r = pane_record(
            vec![
                "claude".into(),
                "--model".into(),
                "opus".into(),
                "--resume".into(),
                "old".into(),
                "--session-id=older".into(),
                "-c".into(),
            ],
            Some("current".into()),
        );
        assert_eq!(
            super::resume_argv(&r),
            vec!["claude", "--model", "opus", "--resume", "current"]
        );
    }

    /// Governed permission flags in a saved command never reach the agent: the
    /// posture comes from the policy the operator confirmed.
    #[test]
    fn a_saved_command_cannot_carry_its_own_permission_flags() {
        let r = pane_record(
            vec![
                "claude".into(),
                "--dangerously-skip-permissions".into(),
                "--permission-mode".into(),
                "bypassPermissions".into(),
            ],
            None,
        );
        assert_eq!(super::resume_argv(&r), vec!["claude"]);
    }

    // --- find_latest_snapshot -----------------------------------------------

    /// A legacy snapshot whose atrium is still running is someone's live session,
    /// not a candidate to recover — even when it is the newest file there.
    #[test]
    fn find_latest_skips_a_snapshot_whose_atrium_is_still_running() {
        let tmp =
            std::env::temp_dir().join(format!("atrium_snap_live_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("atrium-session-100.json"), b"{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(tmp.join("atrium-session-200.json"), b"{}").unwrap();
        let got = super::find_latest_snapshot(&tmp, |pid| pid == 200);
        assert_eq!(
            got.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
            Some("atrium-session-100.json".to_string()),
            "the running session (200) is newer but must be skipped"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_latest_picks_newest_json_and_ignores_others() {
        let tmp =
            std::env::temp_dir().join(format!("atrium_snap_latest_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // Non-matching files must be ignored.
        std::fs::write(tmp.join("atrium-session-1.pids"), b"pids").unwrap();
        std::fs::write(tmp.join("other.json"), b"{}").unwrap();
        // Two matching files — write B after A so B has mtime >= A.
        let a = tmp.join("atrium-session-100.json");
        let b = tmp.join("atrium-session-200.json");
        std::fs::write(&a, b"{}").unwrap();
        std::fs::write(&b, b"{}").unwrap();
        let result = super::find_latest_snapshot(&tmp, |_| false);
        assert!(result.is_some(), "must find a snapshot");
        let got = result.unwrap();
        assert!(
            got == a || got == b,
            "must return one of the two json files"
        );
        // The returned file's mtime must be >= the other's.
        let got_mt = std::fs::metadata(&got).unwrap().modified().unwrap();
        let other = if got == a { &b } else { &a };
        let other_mt = std::fs::metadata(other).unwrap().modified().unwrap();
        assert!(
            got_mt >= other_mt,
            "returned file must be newest: got {got_mt:?} vs {other_mt:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_latest_unreadable_dir_returns_none() {
        let missing = std::path::PathBuf::from("/no/such/directory/atrium_test_xyz");
        assert!(super::find_latest_snapshot(&missing, |_| false).is_none());
    }

    #[test]
    fn find_latest_dir_with_no_matching_files_returns_none() {
        let tmp =
            std::env::temp_dir().join(format!("atrium_snap_none_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("atrium-session-1.pids"), b"pids").unwrap();
        std::fs::write(tmp.join("unrelated.json"), b"{}").unwrap();
        assert!(super::find_latest_snapshot(&tmp, |_| false).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // --- Bug 2: worktree field flows through the real snapshot mapping path ---

    /// Guards the `p.worktree -> PaneCapture.worktree` mapping inside
    /// `snapshot_if_changed` (main.rs:818-827, now via `capture_pane_fields`).
    ///
    /// The original bug: `spawn_worker_window`/`spawn_worker_here` never set
    /// `pane.worktree`, so every live snapshot saw `None` even for worktree
    /// panes. The fix adds `pane.worktree = sp.worktree.clone()` in both spawn
    /// paths. This test fails if `worktree` is dropped or hardcoded to `None`
    /// in `capture_pane_fields` — the function `snapshot_if_changed` calls with
    /// `p.worktree.clone()` as the final argument.
    #[test]
    fn capture_pane_fields_propagates_worktree() {
        let cap = super::capture_pane_fields(
            7,
            Some("fix".to_string()),
            vec!["claude".to_string()],
            Some("/work".to_string()),
            None,
            Some("sess-fix".to_string()),
            Some("fix".to_string()),
            vec!["git push".to_string()],
            false,
            1,
            Some(0),
            TrustMode::Edits,
            false,
            None,
            Vec::new(),
        );
        assert_eq!(
            cap.worktree.as_deref(),
            Some("fix"),
            "worktree must flow through capture_pane_fields unchanged"
        );
        // None must propagate too — a non-worktree pane must not invent a name.
        let cap_none = super::capture_pane_fields(
            8,
            None,
            vec!["bash".to_string()],
            None,
            None,
            None,
            None,
            Vec::new(),
            true,
            0,
            None,
            TrustMode::Off,
            false,
            None,
            Vec::new(),
        );
        assert!(
            cap_none.worktree.is_none(),
            "non-worktree pane must have None"
        );
    }

    // --- recovery restores the session policy, not just the layout ---------

    /// Every capability field must survive the one mapping from a live `Pane` to
    /// a `PaneCapture`. Dropping one here is exactly how the deny list and the
    /// `can_spawn` bit went missing from recovery: the pane held them, the
    /// snapshot never saw them.
    #[test]
    fn capture_pane_fields_propagates_the_capability_fields() {
        let cap = super::capture_pane_fields(
            2,
            Some("builder".to_string()),
            vec!["claude".to_string()],
            None,
            None,
            None,
            None,
            vec!["cargo test --workspace".to_string()],
            false,
            3,
            Some(1),
            TrustMode::Plan,
            false,
            None,
            Vec::new(),
        );
        assert_eq!(cap.deny, vec!["cargo test --workspace".to_string()]);
        assert!(
            !cap.can_spawn,
            "the roster withheld it; so must the snapshot"
        );
        assert_eq!(cap.depth, 3);
        assert_eq!(cap.parent_pane, Some(1));
        assert_eq!(cap.mode, Some(TrustMode::Plan));
    }

    #[test]
    fn flag_typed_sees_bare_and_glued_forms_only() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(super::flag_typed(
            &a(&["--trust", "automode"]),
            &["--trust"]
        ));
        assert!(super::flag_typed(&a(&["--max-depth=3"]), &["--max-depth"]));
        assert!(super::flag_typed(
            &a(&["--skip-permissions"]),
            &["--trust", "--skip-permissions"]
        ));
        assert!(
            !super::flag_typed(&a(&["--allow-ctl"]), &["--trust"]),
            "an unrelated flag is not a trust choice"
        );
        assert!(
            !super::flag_typed(&a(&[]), &["--trust"]),
            "recovering with no flags defers to the snapshot"
        );
    }

    /// The precedence rule: what the operator typed wins, what the snapshot
    /// recorded fills the gap, and only then the parser's default. Without the
    /// middle step a bare `atrium recover` silently downgraded an `automode`
    /// fleet to the default posture.
    #[test]
    fn recovered_trust_prefers_the_typed_flag_then_the_snapshot() {
        assert_eq!(
            super::recovered_trust(true, TrustMode::Plan, Some(TrustMode::Auto)),
            TrustMode::Plan,
            "a typed flag wins over the snapshot"
        );
        assert_eq!(
            super::recovered_trust(false, TrustMode::Off, Some(TrustMode::Auto)),
            TrustMode::Auto,
            "no flag: the recorded posture"
        );
        assert_eq!(
            super::recovered_trust(false, TrustMode::Off, None),
            TrustMode::Off,
            "no flag and no record: the parsed default"
        );
    }

    /// A pane comes back where it was, and a snapshot can never promote one: the
    /// ceiling is applied to the recorded posture, not replaced by it.
    #[test]
    fn recovered_pane_mode_restores_below_the_ceiling_and_caps_above_it() {
        assert_eq!(
            super::recovered_pane_mode(Some(TrustMode::Edits), TrustMode::Auto),
            TrustMode::Edits,
            "a de-escalated pane must not be promoted to the ceiling"
        );
        assert_eq!(
            super::recovered_pane_mode(Some(TrustMode::Skip), TrustMode::Plan),
            TrustMode::Plan,
            "a snapshot must never climb above the session ceiling"
        );
        assert_eq!(
            super::recovered_pane_mode(None, TrustMode::Auto),
            TrustMode::Auto,
            "an unrecorded posture falls back to the ceiling"
        );
    }

    /// A pre-policy snapshot must SAY that the guards are not coming back. The
    /// silent version of this is the bug: a recovered fleet that looks identical
    /// and runs with its deny list, compile pool and memory cap gone.
    #[test]
    fn recovery_notice_names_the_guards_or_warns_they_are_missing() {
        let policy = atrium::session::PolicyRecord {
            deny: vec!["git push".to_string()],
            claude_aliases: Vec::new(),
            build_jobs: Some(10),
            memory_mb: Some(32768),
            trust: Some(TrustMode::Auto),
            allow_ctl: true,
            max_depth: 6,
            topics: None,
        };
        let full = super::recovery_notice(&notice_panes(4), Some(&policy), false, TrustMode::Auto);
        for expected in [
            "4 pane(s)",
            "trust automode",
            "ctl on (max depth 6)",
            "1 session deny rule(s)",
            "build pool 10",
            "memory cap 32768 MB",
        ] {
            assert!(
                full.contains(expected),
                "{full:?} must mention {expected:?}"
            );
        }
        let legacy = super::recovery_notice(&notice_panes(4), None, true, TrustMode::Off);
        assert!(
            legacy.contains("NOT restored"),
            "a v1 snapshot must warn, not pretend: {legacy:?}"
        );
    }

    /// The posture is the one value a recovery can raise above the flags the
    /// operator typed, and the snapshot lives in a shared temp directory — so
    /// when it comes from the file, the line that precedes the confirmation
    /// prompt has to say so.
    #[test]
    fn recovery_notice_marks_a_posture_that_came_from_the_file() {
        let policy = atrium::session::PolicyRecord {
            deny: Vec::new(),
            claude_aliases: Vec::new(),
            build_jobs: None,
            memory_mb: None,
            trust: Some(TrustMode::Auto),
            allow_ctl: true,
            max_depth: usize::MAX,
            topics: None,
        };
        let from_file =
            super::recovery_notice(&notice_panes(1), Some(&policy), true, TrustMode::Auto);
        assert!(
            from_file.contains("trust automode (from the snapshot)"),
            "{from_file:?}"
        );
        assert!(
            from_file.contains("max depth unlimited"),
            "an unlimited guard must read as unlimited, not as a 19-digit number: {from_file:?}"
        );
        let typed = super::recovery_notice(&notice_panes(1), Some(&policy), false, TrustMode::Auto);
        assert!(
            typed.contains("trust automode") && !typed.contains("from the snapshot"),
            "a posture the operator typed is not attributed to the file: {typed:?}"
        );
    }

    /// Enter is yes; only an explicit no declines. A resume offer the operator
    /// dismisses by reflex would otherwise be gone for good.
    #[test]
    fn a_resume_prompt_defaults_to_yes_and_only_no_declines() {
        for yes in ["\n", "y\n", "Yes\r\n", "sure\n"] {
            assert!(super::resume_answer(yes), "{yes:?} should resume");
        }
        for no in ["n\n", "N", " no \r\n", "NO"] {
            assert!(!super::resume_answer(no), "{no:?} should decline");
        }
    }

    /// The offer is for the operator at a terminal: never from inside a pane (an
    /// agent launching atrium must not be handed the operator's crashed session),
    /// never without a terminal, never when switched off.
    #[test]
    fn the_resume_offer_needs_an_operator_at_a_terminal() {
        assert!(super::offer_enabled(None, false, true));
        assert!(super::offer_enabled(Some("1"), false, true));
        assert!(
            !super::offer_enabled(Some("0"), false, true),
            "switched off"
        );
        assert!(
            !super::offer_enabled(None, true, true),
            "inside an atrium pane"
        );
        assert!(
            !super::offer_enabled(None, false, false),
            "no terminal to ask on"
        );
    }

    #[test]
    fn ages_read_at_human_scale() {
        assert_eq!(super::human_age(42_000), "42s");
        assert_eq!(super::human_age(7 * 60_000), "7m");
        assert_eq!(super::human_age(3 * 3_600_000), "3h");
        assert_eq!(super::human_age(2 * 86_400_000), "2d");
    }

    /// A recovery re-installs the merged deny list as the fleet's, and `run()`
    /// re-reads `ATRIUM_DENY` on top of it. Without the dedupe the recorded list
    /// grows by the env entries once per recovery generation.
    #[test]
    fn a_recorded_deny_list_does_not_grow_across_recoveries() {
        let merged = vec![
            "git push".to_string(),
            "cargo bench".to_string(),
            "git push".to_string(),
        ];
        assert_eq!(
            super::dedup_preserving_order(merged),
            vec!["git push".to_string(), "cargo bench".to_string()],
            "repeats collapse, first-seen order survives"
        );
    }
}
