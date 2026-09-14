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
mod pane_spawn;
mod panels;
mod renderer;
mod safety_net;
mod tiled;
pub(crate) use ctl_server::*;
pub(crate) use fleet_cli::*;
pub(crate) use loop_phases::*;
pub(crate) use mouse::*;
pub(crate) use overlay_keys::*;
pub(crate) use overview::*;
pub(crate) use pane_spawn::*;
pub(crate) use panels::*;
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
fn capture_pane_fields(
    id: usize,
    role: Option<String>,
    argv: Vec<String>,
    cwd: Option<String>,
    identity: Option<String>,
    session_id: Option<String>,
    worktree: Option<String>,
) -> atrium::session::PaneCapture {
    atrium::session::PaneCapture {
        id,
        role,
        argv,
        cwd,
        identity,
        session_id,
        worktree,
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

/// Write a session snapshot iff the window state has changed since the last write.
/// Pure capture + equality check — no I/O when nothing changed.
///
/// `last` advances only when the write lands, so a failed save is retried on the
/// next check instead of being forgotten; the error is returned for the caller to
/// surface (r10 audit B8 — it used to be dropped, and `atrium recover` silently
/// had nothing to restore).
fn snapshot_if_changed(
    windows: &[Window],
    path: &std::path::Path,
    last: &mut Option<atrium::session::Snapshot>,
) -> std::io::Result<()> {
    let w = match windows.first() {
        Some(w) => w,
        None => return Ok(()),
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
                )
            })
            .collect(),
    );
    if last.as_ref() == Some(&snap) {
        return Ok(());
    }
    atrium::session::save(path, &snap)?;
    *last = Some(snap);
    Ok(())
}

/// `atrium recover` — load the most recent session snapshot and relaunch every
/// pane with its session resumed. A missing snapshot prints a clear message
/// and exits cleanly rather than panicking.
fn recover_cmd(args: &[String]) -> ExitCode {
    let (allow_ctl, max_depth, trust, rest) = match atrium::ctl::parse_flags(args) {
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
    let snap_dir = atrium::reap::registry_dir();
    let snap = match find_latest_snapshot(&snap_dir) {
        Some(path) => match atrium::session::load(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("atrium recover: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            eprintln!(
                "atrium recover: no snapshot found in {} — nothing to recover",
                snap_dir.display()
            );
            return ExitCode::SUCCESS;
        }
    };
    if snap.panes.is_empty() {
        eprintln!("atrium recover: snapshot has no panes — nothing to recover");
        return ExitCode::SUCCESS;
    }
    let trust = cap_trust_to_ancestor(trust);
    if trust == atrium::ctl::TrustMode::Skip && !confirm_skip_permissions() {
        eprintln!("atrium recover: aborted.");
        return ExitCode::SUCCESS;
    }
    set_trust_mode(trust);
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
        match spawn_pane_full(
            PaneSpec {
                command: &argv,
                id: record.id,
                identity: record.identity.as_deref(),
                cwd: record.cwd.as_deref(),
                mode: trust_mode(),
                extra_env: &[],
                extra_norms: None,
            },
            cell_rows,
            cols,
            &mut flash,
        ) {
            Ok(mut pane) => {
                pane.role = record.role.clone();
                pane.worktree = record.worktree.clone();
                panes.push(pane);
            }
            Err(e) => {
                eprintln!("atrium recover: cannot start {:?}: {e}", argv[0]);
                for p in panes.iter_mut() {
                    let _ = p.pty.kill();
                }
                return ExitCode::FAILURE;
            }
        }
    }
    let max_id = snap.layout.ids.iter().copied().max().unwrap_or(0);
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
        None,
    )
}

/// Build the recovery argv for one pane. For claude panes with a stored
/// session id, strips `--continue` and appends `--resume <id>` so the agent
/// resumes its conversation. Non-claude panes are relaunched verbatim.
fn resume_argv(record: &atrium::session::PaneRecord) -> Vec<String> {
    let mut argv = record.argv.clone();
    if argv.is_empty() {
        return argv;
    }
    if !atrium::bind::is_claude_stem(&atrium::bind::command_stem(&argv[0])) {
        return argv;
    }
    if let Some(id) = &record.session_id {
        argv.retain(|a| a != "--continue");
        argv.push("--resume".to_string());
        argv.push(id.clone());
    }
    argv
}

/// Scan `dir` for `atrium-session-*.json` files and return the most recently
/// modified one, or `None` if the directory is unreadable or empty.
fn find_latest_snapshot(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("atrium-session-") || !name.ends_with(".json") {
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
            // Startup panes (created before the session Job exists) enroll via the
            // run loop's lazy pass on the first tick — status quo; not a
            // kill-after-spawn race. Hence `None` here.
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
    // A fleet that declared a topic vocabulary switches the bus to strict
    // admission; without one it stays soft-gated. Set before any pane is spawned,
    // so the very first publish is already governed by the right policy.
    if let Some(topics) = &canonical_topics {
        bus.set_canonical(topics);
    }
    let mut world = atrium::vendors::VendorWorlds::new();
    let process_start_ms = agsess::sessions::now_ms();
    world.refresh_since(process_start_ms);
    let mut last_agent_poll = Instant::now();
    let mut last_agent_discover = Instant::now();
    // Session teardown container: on Windows a kill-on-close Job Object so no pane
    // tree outlives atrium however it dies (TerminateProcess included); a no-op on
    // unix (the process-group teardown + watchdog below already cover the tree).
    // Held for the whole run — dropping it (or the process exiting) fires the
    // guarantee. Panes never inherit its handle, so they live until atrium exits.
    let session_job = atrium::reap::SessionJob::create();
    // The crash registry, session snapshot, warden and orphan watchdog (loop
    // phase 0b); see `safety_net`.
    let mut safety_net = SafetyNet::new();
    // The paint's memory between ticks (retained tiled frame, throttles); see
    // `renderer`.
    let mut renderer = Renderer::new();

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
        // 0b. The safety net: crash registry, session snapshot, warden tripwires,
        // and (unix) the orphan watchdog.
        if safety_net.tick(&windows, &session_job, &mut ctl_audit, &mut bus, &mut flash) {
            force_repaint = true;
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
                        match open_window(
                            &mut windows,
                            &mut active,
                            &argv,
                            identity,
                            trust_mode(),
                            rows,
                            cols,
                            &mut out,
                            &mut flash,
                            &session_job,
                        ) {
                            Ok(()) => renderer.reset(),
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
                if outcome.quit {
                    break 'outer;
                }
                if outcome.reset_frame {
                    renderer.reset();
                }
                if outcome.repaint {
                    force_repaint = true;
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
                Action::NextPane => {
                    let next = (active + 1) % windows.len();
                    switch_window(&mut windows, &mut active, next, rows, cols, &mut out);
                    renderer.reset();
                    force_repaint = true;
                }
                Action::PrevPane => {
                    let prev = (active + windows.len() - 1) % windows.len();
                    switch_window(&mut windows, &mut active, prev, rows, cols, &mut out);
                    renderer.reset();
                    force_repaint = true;
                }
                Action::SwitchTo(i) => {
                    if i < windows.len() {
                        switch_window(&mut windows, &mut active, i, rows, cols, &mut out);
                        renderer.reset();
                        force_repaint = true;
                    }
                }
                Action::NewPane => {
                    match open_window(
                        &mut windows,
                        &mut active,
                        command,
                        identity,
                        trust_mode(),
                        rows,
                        cols,
                        &mut out,
                        &mut flash,
                        &session_job,
                    ) {
                        Ok(()) => renderer.reset(),
                        Err(e) => {
                            flash = Some((
                                format!("cannot start {:?}: {e}", command[0]),
                                Instant::now(),
                            ));
                        }
                    }
                    force_repaint = true;
                }
                Action::NewShellPane => {
                    // A plain shell in a new window — no identity, no trust posture
                    // (it's not an agent), but it still gets the ctl env injected,
                    // so you can run `atrium ctl board list` here and see it rendered.
                    let shell = vec![default_shell()];
                    match open_window(
                        &mut windows,
                        &mut active,
                        &shell,
                        None,
                        atrium::ctl::TrustMode::Off,
                        rows,
                        cols,
                        &mut out,
                        &mut flash,
                        &session_job,
                    ) {
                        Ok(()) => renderer.reset(),
                        Err(e) => {
                            flash = Some((
                                format!("cannot start shell {:?}: {e}", shell[0]),
                                Instant::now(),
                            ));
                        }
                    }
                    force_repaint = true;
                }
                Action::ToggleBoard => {
                    views.board = !views.board;
                    views.feed_scroll = 0; // always open at the live/newest end
                    views.decision_sel = 0;
                    // Toggling either way is a full repaint: on → draw the panel
                    // (it clears the screen); off → recompose/repaint the panes the
                    // panel covered. On close, restore the cursor the panel hid.
                    if !views.board {
                        let _ = write!(out, "\x1b[?25h");
                    }
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
                    // Open the overview (close the board if it was up — one overlay
                    // at a time). Selection starts at the focused agent so Enter
                    // dives back into what you were watching.
                    views.overview = true;
                    views.board = false;
                    let focus = windows[active].tree.focus();
                    views.overview_sel = overview_nodes(&windows, &world)
                        .iter()
                        .position(|n| n.window == active && n.pane_id == focus)
                        .unwrap_or(0);
                    let _ = write!(out, "\x1b[?25h");
                    renderer.reset();
                    force_repaint = true;
                }
                Action::ToggleLog => {
                    // Open the activity log (one overlay at a time).
                    views.log = true;
                    views.board = false;
                    views.overview = false;
                    views.log_scroll = 0;
                    let _ = write!(out, "\x1b[?25h");
                    renderer.reset();
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
                    renderer.reset();
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
                    renderer.reset();
                    force_repaint = true;
                }
                Action::MoveFocus(d) => {
                    let outer = tiled_outer(rows, cols);
                    windows[active].tree.move_focus(to_move(d), outer);
                    renderer.reset();
                    force_repaint = true;
                }
                Action::Zoom => {
                    let w = &mut windows[active];
                    // Zoom only means something with more than one pane.
                    if w.panes.len() > 1 {
                        w.zoomed = !w.zoomed;
                        resize_window(w, rows, cols);
                        renderer.reset();
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
                    let outcome = handle_mouse(
                        &action,
                        &mut windows,
                        active,
                        &mut selection,
                        rows,
                        cols,
                        &mut out,
                        &mut flash,
                    );
                    if outcome.reset_frame {
                        renderer.reset();
                    }
                    if outcome.repaint {
                        force_repaint = true;
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
                        renderer.reset();
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
                            &session_job,
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
            rows,
            cols,
            &mut out,
        );
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
    if dbg {
        eprint!("[atrium-dbg cleaned]\r\n");
    }
    drop(windows);
    if dbg {
        eprint!("[atrium-dbg panes-dropped]\r\n");
    }
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

#[cfg(test)]
mod tests {
    use super::{
        assemble_command, audit_outcome, combine_system_prompt, pane_base_env, privilege_for,
        routed_wake, sanitize_shim_arg, surface_once, worktree_spawn_params,
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
        let bare = pane_base_env(Some(&key), None);
        assert_eq!(
            bare,
            vec![("ATRIUM_SESSION".to_string(), "17686:99".to_string())],
            "a pane without ctl must still be findable"
        );

        // With ctl on, the marker is still there, still first, and the three ctl
        // variables are unchanged. Spelled as literals on purpose: this pins the
        // WIRE names a child reads, so renaming a const without the child is caught.
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
        assert!(privilege_for(Some(AgentId(1)), Some(None)));
        assert!(!privilege_for(Some(AgentId(2)), Some(Some(AgentId(1)))));
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
        git(&["commit", "--allow-empty", "-m", "init"]).unwrap();
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

    #[test]
    fn resume_argv_empty_argv_no_panic() {
        let r = pane_record(vec![], Some("any-id".into()));
        let got = super::resume_argv(&r);
        assert!(got.is_empty());
    }

    // --- find_latest_snapshot -----------------------------------------------

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
        let result = super::find_latest_snapshot(&tmp);
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
        assert!(super::find_latest_snapshot(&missing).is_none());
    }

    #[test]
    fn find_latest_dir_with_no_matching_files_returns_none() {
        let tmp =
            std::env::temp_dir().join(format!("atrium_snap_none_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("atrium-session-1.pids"), b"pids").unwrap();
        std::fs::write(tmp.join("unrelated.json"), b"{}").unwrap();
        assert!(super::find_latest_snapshot(&tmp).is_none());
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
        );
        assert_eq!(
            cap.worktree.as_deref(),
            Some("fix"),
            "worktree must flow through capture_pane_fields unchanged"
        );
        // None must propagate too — a non-worktree pane must not invent a name.
        let cap_none =
            super::capture_pane_fields(8, None, vec!["bash".to_string()], None, None, None, None);
        assert!(
            cap_none.worktree.is_none(),
            "non-worktree pane must have None"
        );
    }
}
