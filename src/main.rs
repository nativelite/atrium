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

mod ask_panel;
mod config_cli;
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
mod recover;
mod renderer;
mod run_loop;
mod safety_net;
mod snapshot;
mod tiled;
mod ui;
mod window;
pub(crate) use config_cli::*;
mod mod_cli;
pub(crate) use ctl_server::*;
pub(crate) use fleet_cli::*;
pub(crate) use loop_phases::*;
pub(crate) use mod_cli::*;
pub(crate) use mouse::*;
pub(crate) use overlay_keys::*;
pub(crate) use overview::*;
pub(crate) use pane_keys::*;
pub(crate) use pane_spawn::*;
pub(crate) use panels::*;
pub(crate) use prompt::*;
pub(crate) use recover::*;
pub(crate) use renderer::*;
pub(crate) use run_loop::{run, RunArgs};
pub(crate) use safety_net::*;
pub(crate) use snapshot::*;
pub(crate) use tiled::*;
pub(crate) use ui::*;
pub(crate) use window::*;

#[cfg(test)]
mod test_support;

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

/// Bind the ctl endpoint and publish its address to [`CTL_ADDRESS`]
/// so every pane spawned afterward is born with `ATRIUM_CTL`/`ATRIUM_PANE` in its
/// env. The endpoint is the operator's `--ctl-socket` / `ATRIUM_CTL_SOCKET` when
/// one was given and the per-process default otherwise
/// (`atrium::ipc::endpoint`). Returns the [`atrium::ipc::Listener`] to poll, or
/// `None` on a bind failure (surfaced in the bar, non-fatal; a launch that must
/// not go on without a chosen socket asks [`bind_ctl_chosen`] first).
/// `CTL_ADDRESS` is a `OnceLock`, so only the
/// first successful bind per process publishes the address. The listener's
/// accept thread rings the run loop's doorbell on every connect
/// (`atrium::ipc::set_bell`), so a request is served when it arrives, not on
/// the next tick; the loop's per-tick poll stays as the fallback.
fn bind_ctl(flash: &mut Option<(String, Instant)>) -> Option<atrium::ipc::Listener> {
    match try_bind_ctl() {
        Ok(l) => Some(l),
        Err(why) => {
            if flash.is_none() {
                *flash = Some((format!("ctl channel disabled: {why}"), Instant::now()));
            }
            None
        }
    }
}

/// [`bind_ctl`]'s work: resolve the endpoint, bind it, publish the address to
/// the spawn path and to the discovery file. `Err` is the reason to show.
fn try_bind_ctl() -> Result<atrium::ipc::Listener, String> {
    let endpoint = atrium::ipc::endpoint()?;
    let (listener, address) = atrium::ipc::bind_endpoint(&endpoint)?;
    atrium::ipc::publish_endpoint(&address);
    let _ = CTL_ADDRESS.set(address);
    Ok(listener)
}

/// Bind an operator-chosen ctl socket before the terminal is taken, so a path
/// that is live in another session (or cannot be an endpoint at all) fails the
/// launch with a line on stderr instead of a bar flash in a session that came
/// up without it. `Ok(None)` when ctl is off or no socket was chosen: the
/// per-process default keeps its non-fatal [`bind_ctl`].
fn bind_ctl_chosen(allow_ctl: bool) -> Result<Option<atrium::ipc::Listener>, String> {
    let chosen = !matches!(atrium::ipc::endpoint(), Ok(e) if !e.explicit);
    if !allow_ctl {
        // Naming the endpoint does not switch the control plane on: an
        // environment variable must never be what opens it. Said, not silent.
        if chosen {
            eprintln!(
                "atrium: a ctl socket was named (--ctl-socket / ATRIUM_CTL_SOCKET) but the \
                 control plane is off, so nothing is bound: add --allow-ctl"
            );
        }
        return Ok(None);
    }
    if !chosen {
        return Ok(None);
    }
    try_bind_ctl().map(Some)
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

/// Is this argv a launch that hosts a session, where the first-run offer of
/// the mod belongs? `recover` and `fleet up` (not `--headless`, which has no
/// one to ask, and not their help), and a plain launch or `up` (not
/// `--version`, `--help` or `--stdin-probe`, which answer and leave). Never
/// `ctl`, `mod` or another `fleet` command; `config`, `reap`, the shim and
/// the watchdog have returned before this is asked. Whether a person is
/// there is not decided here ([`atrium::modfiles::offer_of`]).
fn offers_mod(args: &[String]) -> bool {
    let wants_help = |rest: &[String]| rest.iter().any(|a| atrium::help::wants(a));
    match args.first().map(String::as_str) {
        Some("ctl" | "mod" | "config" | "reap") => false,
        Some("recover") => !wants_help(&args[1..]) && !args.iter().any(|a| a == "--list"),
        Some("fleet") => {
            args.get(1).map(String::as_str) == Some("up")
                && !wants_help(&args[2..])
                && !args.iter().any(|a| a == "--headless")
        }
        _ => {
            // atrium's own meta-flags are read after `--identity` and
            // `--reap-orphans`, as `launch` reads them.
            let (_, rest) = atrium::identity::parse(args);
            let (_, rest) = atrium::orphan::parse_flag(&rest);
            !matches!(
                rest.first().map(String::as_str),
                Some("--version" | "-V" | "--help" | "-h" | "--stdin-probe")
            )
        }
    }
}

/// What a plain launch says before the terminal is taken when the command
/// it hosts is claude and the control plane is on: the path a named teammate
/// takes ([`atrium::modfiles::session_teammates_line`]); and the one line
/// for a mod that is not installed and an offer nobody answered
/// ([`atrium::modfiles::session_absent_note`]). Nothing for any other
/// command.
fn claude_launch_notes(command: &[String], allow_ctl: bool) -> Vec<String> {
    let is_claude = command
        .first()
        .is_some_and(|c| atrium::bind::is_claude_stem(&atrium::bind::command_stem(c)));
    if !is_claude || !allow_ctl {
        return Vec::new();
    }
    let mut lines = vec![atrium::modfiles::session_teammates_line(true)];
    lines.extend(atrium::modfiles::session_absent_note());
    lines
}

fn main() -> ExitCode {
    let code = launch();
    // The discovery file names a running session; this one is over.
    atrium::ipc::retract_endpoint();
    code
}

fn launch() -> ExitCode {
    // Before anything else: a closed terminal window (SIGHUP) must reach the
    // event loop's normal exit, not kill atrium where it stands and orphan every
    // hosted agent onto `init`. Installed here so every path — single pane,
    // mass-spawn, `fleet up` — is covered.
    atrium::signals::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The tmux shim: this same binary under the name `tmux`, answering Claude
    // Code's teams backend from inside a ctl pane (`atrium::tmux_shim`); or
    // `--tmux-shim <tmux args…>` to reach it by its real name for a probe.
    if atrium::tmux_shim::invoked_as_shim() {
        return atrium::tmux_shim::main(&args);
    }
    if args.first().map(String::as_str) == Some(atrium::tmux_shim::FLAG) {
        return atrium::tmux_shim::main(&args[1..]);
    }
    // A teammate placeholder (`--tmux-hold`): the pane Claude Code 2.1.325
    // reserves before it starts a teammate. It says what it is and waits to
    // be killed; it never reaches the session code below.
    if args.first().map(String::as_str) == Some(atrium::tmux_shim::HOLD_FLAG) {
        atrium::tmux_shim::hold_main();
    }
    // Operator tokens (`--operator-token-file <path>` first, or
    // ATRIUM_OPERATOR_TOKEN_FILE): the file is read and checked once, here,
    // before any dispatch, so every path that hosts a session (a plain launch,
    // `fleet up`, `up`, `recover`) has them and none starts on a file that
    // fails a check. The variable leaves atrium's environment in the same step,
    // before any pane exists to inherit it.
    let args = match atrium::operator::host_startup(args) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("atrium: {msg}");
            return ExitCode::FAILURE;
        }
    };
    // Started inside a real tmux: its `TMUX` / `TMUX_PANE` must not reach a
    // pane, or Claude Code's teams there split the outer tmux window. A pane's
    // environment is this process's plus overrides, and an override cannot
    // remove a variable, so the two are dropped from this process, here: after
    // the shim (which runs inside a pane, on the values atrium gave it), before
    // any thread or pane exists, and on every path that can host a pane (a
    // plain launch, `up`, `fleet up`, `recover`). Not for `atrium ctl`, a
    // client inside a pane. `tmux_shim::outer_notice` remembers it for the bar.
    if args.first().map(String::as_str) != Some("ctl") {
        atrium::tmux_shim::hide_outer();
    }
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
        if args.get(1).is_some_and(|a| atrium::help::wants(a)) {
            print!("{}", atrium::help::REAP);
            return ExitCode::SUCCESS;
        }
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
    // `atrium config …`: where the user-global config lives, and a starter
    // file. Before the config is loaded, since `init` is how one comes to exist.
    if args.first().map(String::as_str) == Some("config") {
        return config_cmd(&args[1..]);
    }
    // First run: `fleet up` and `recover` are the launches a human is at (each
    // already asks for confirmation) and where the config matters, so they ask
    // once where it should live. A plain launch, a script or a test never sees
    // the question, and once the pointer exists nothing asks again.
    let first = args.first().map(String::as_str);
    if first == Some("recover")
        || (first == Some("fleet") && args.get(1).map(String::as_str) == Some("up"))
    {
        atrium::config::first_run_setup();
    }
    // The second question of the first run, asked once and kept: whether to
    // install the mod. On every launch that hosts a session and can have a
    // person at it (`offers_mod`), a plain one included, since the mod is
    // loaded into a claude pane wherever it runs. After the config question,
    // so a new user answers both in one go and the answer has a file to go
    // into; before the config is installed, so this launch already runs on
    // the answer. It asks nothing where nobody is there.
    if offers_mod(&args) {
        atrium::modfiles::first_run_offer();
    }
    // The user-global config (`config.json`): aliases, session-wide lists and
    // fleet defaults, in force for every launch path below. Not for the ctl
    // client, which runs inside panes many times a session and needs none of
    // it. A malformed file is an error here, never a silent default.
    if first != Some("ctl") {
        if let Err(e) = atrium::config::install_from_disk() {
            eprintln!("atrium: {e}");
            return ExitCode::FAILURE;
        }
    }
    // `atrium mod …`: the plugin a claude pane loads, written out of this
    // binary. After the config, which may name its folder.
    if first == Some("mod") {
        return mod_cmd(&args[1..]);
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
        // `--version --json`: the same answer for a script (version, the
        // commit when the build knew it, the target), one object on one line.
        if rest.get(1).map(String::as_str) == Some("--json") {
            println!("{}", atrium::help::version_json());
        } else {
            println!("atrium {}", env!("CARGO_PKG_VERSION"));
        }
        return ExitCode::SUCCESS;
    }
    if rest.first().is_some_and(|a| atrium::help::wants(a)) {
        print!("{}", atrium::help::top());
        return ExitCode::SUCCESS;
    }
    if rest.first().map(String::as_str) == Some("--stdin-probe") {
        return stdin_probe();
    }
    // atrium's own launch meta-flags (`--allow-ctl` / `--max-depth <N>` / `--trust`)
    // are stripped next — after `--identity`, before mass-spawn flags and the
    // hosted command. A bad `--max-depth` is a startup error, never a silent
    // fallback. `--ctl-socket <path>` stands anywhere among them and is taken
    // out first; it names the endpoint `bind_ctl` binds.
    let rest = match atrium::ipc::take_socket_flag(&rest) {
        Ok((socket, rest)) => {
            if let Some(address) = socket {
                atrium::ipc::set_socket_flag(address);
            }
            rest
        }
        Err(msg) => {
            eprintln!("atrium: {msg}");
            return ExitCode::FAILURE;
        }
    };
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
    // Found here, the operator flag was written after another session flag or
    // after the command, where it would be taken for the command to host or
    // handed to it, and the session would start without its operators. Say
    // where it goes.
    if let Err(msg) = atrium::operator::misplaced(&rest) {
        eprintln!("atrium: {msg}");
        return ExitCode::FAILURE;
    }
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
    // A token file with the control plane off: no operator could connect.
    if let Err(msg) = atrium::operator::require_ctl(allow_ctl) {
        eprintln!("atrium: {msg}");
        return ExitCode::FAILURE;
    }
    // A `teams_env` list lets a pane's request set variables on a Claude Code
    // lead's teammates. A fleet's banner shows it; a plain launch says it
    // here, in one line, while stderr is still a plain terminal.
    if allow_ctl && atrium::tmux_shim::session_enabled() {
        let names = atrium::tmux_shim::session_teams_env();
        if let Some(line) = atrium::tmux_shim::teams_env_disclosure(&names) {
            eprintln!("atrium: {line}");
        }
    }
    // A launch that hosts claude says, here too, what a fleet's banner says:
    // which path handles a subagent the model names (the mod, the tmux shim,
    // neither), and, once, that no mod is installed when none is and nobody
    // has answered the offer (a recorded no is silent).
    for line in claude_launch_notes(&command, allow_ctl) {
        eprintln!("atrium: {line}");
    }
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
    // A chosen ctl socket is bound here, while stderr is still a plain
    // terminal: live elsewhere, it ends the launch.
    let ctl_listener = match bind_ctl_chosen(allow_ctl) {
        Ok(listener) => listener,
        Err(msg) => {
            eprintln!("atrium: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("atrium: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    run(
        &mut term,
        RunArgs {
            command: &command,
            identity: identity.as_deref(),
            grid,
            initial_window: None,
            allow_ctl,
            max_depth,
            trust,
            ctl_listener,
            attention: atrium::attention::Setup {
                name: atrium::attention::Setup::project_dir_name(),
                notify: atrium::config::get()
                    .notify
                    .unwrap_or(atrium::attention::Notify::DEFAULT),
            },
            // The default (non-fleet) path declares no topic vocabulary: soft-gate.
            canonical_topics: None,
        },
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
    #[cfg(unix)]
    {
        extern "C" {
            fn isatty(fd: i32) -> i32;
        }
        // SAFETY: isatty on a borrowed fd, no ownership taken.
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

/// Every mouse-reporting mode atrium ever turns off, in one place.
///
/// This existed twice with DIFFERENT contents: the per-tick suppressor sent
/// `?1015l` and `cleanup_screen` did not. So a hosted app that enabled
/// urxvt-style reporting (1015) left the user's real shell emitting escape
/// garbage on every click after atrium exited — the one mode the exit path forgot.
/// Two lists that must agree will not stay in agreement; there is now one.
const MOUSE_OFF: &str = "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l";

/// Sanitize the terminal on exit and leave the alt screen. atrium owns the alt
/// buffer (§ the run loop's `\x1b[?1049h` at start), but a hosted app may have
/// left modes on — mouse reporting, bracketed paste, a hidden cursor, an
/// altered scroll region, a non-default SGR. Leaving the alt screen alone does
/// not undo those, so we reset them first, in a sensible order, before the
/// `?1049l` swap so the user's original shell comes back clean. Called on
/// **every** exit path in `run` (normal quit, last-pane-exit, read error, and
/// the initial-spawn failure).
fn cleanup_screen(out: &mut impl Write) {
    let _ = write!(
        out,
        // Reset SGR; disable every mouse mode; disable bracketed paste (2004);
        // show the cursor (25); reset the scroll region; then leave the alt
        // screen (1049) last so the swap-back is the final act.
        // The title reset (`attention`) goes between the scroll-region reset
        // and the alt-screen swap-back: an empty title for hosts with no title
        // stack (ConPTY), then the stack pop for those with one.
        "\x1b[0m{MOUSE_OFF}\x1b[?2004l\x1b[?25h\x1b[r{}\x1b[?1049l",
        atrium::attention::TITLE_RESET
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
mod offer_tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The offer belongs to the launches that host a session, never to a
    /// command that answers and leaves or that runs with nobody there.
    #[test]
    fn the_mod_is_offered_only_on_launches_that_host_a_session() {
        for hosts in [
            "",
            "claude",
            "--allow-ctl claude",
            "--identity work --allow-ctl claude --help",
            "-n 4 claude",
            "--reap-orphans claude",
            "--trust up team",
            "up team",
            "fleet up team",
            "fleet up team --allow-ctl",
            "recover",
            "recover --trust accept",
        ] {
            assert!(offers_mod(&argv(hosts)), "{hosts:?}");
        }
        for not in [
            "ctl list",
            "ctl spawn claude",
            "mod install",
            "mod status",
            "config path",
            "reap",
            "--version",
            "-V --json",
            "--help",
            "-h",
            "--identity work --help",
            "--reap-orphans --version",
            "--stdin-probe",
            "fleet",
            "fleet ls",
            "fleet init",
            "fleet clean team",
            "fleet --help",
            "fleet up --help",
            "fleet up team --headless",
            "fleet up team --headless --size 40x120",
            "recover --help",
            "recover --list",
        ] {
            assert!(!offers_mod(&argv(not)), "{not:?}");
        }
    }

    /// A plain launch speaks of teammates and the mod only when it hosts
    /// claude with the control plane on.
    #[test]
    fn only_a_ctl_launch_of_claude_gets_the_notes() {
        let cmd = |s: &str| argv(s);
        assert!(claude_launch_notes(&cmd("cmd"), true).is_empty());
        assert!(claude_launch_notes(&cmd("codex"), true).is_empty());
        assert!(claude_launch_notes(&cmd("claude"), false).is_empty());
        let notes = claude_launch_notes(&cmd("claude --model x"), true);
        assert!(!notes.is_empty() && notes.len() <= 2, "{notes:?}");
        assert!(notes[0].starts_with("teams: "), "{notes:?}");
        // The full path of a claude counts, as it does everywhere else.
        assert!(!claude_launch_notes(&cmd("/usr/local/bin/claude"), true).is_empty());
    }
}
