//! Opening a session: everything the loop needs, created in the order it
//! always was.

use super::{RunArgs, RunState};
use crate::*;

impl<'a> RunState<'a> {
    /// Open the session: publish its policy, take the screen, bind ctl, spawn
    /// the first window and start the loop's helpers. A first window that
    /// cannot start ends atrium with a failure, after restoring the screen.
    pub(super) fn start(
        term: &'a mut rawterm::Terminal,
        args: RunArgs<'a>,
    ) -> Result<Self, ExitCode> {
        let RunArgs {
            command,
            identity,
            grid,
            initial_window,
            allow_ctl,
            max_depth,
            trust,
            mut ctl_listener,
            canonical_topics,
        } = args;
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
        let (rows, cols) = term.size().unwrap_or((24, 80));
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
        let pending_sends: Vec<PendingSend> = Vec::new();
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
        let ctl_audit = if allow_ctl {
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
                Some(g) => spawn_window_grid(command, rows, cols, g, identity, &mut flash),
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
                ),
            },
        };
        match initial {
            Ok(w) => windows.push(w),
            Err(e) => {
                cleanup_screen(&mut out);
                out.finish(SCREEN_FINISH);
                eprintln!("atrium: cannot start {:?}: {e}", command[0]);
                return Err(ExitCode::FAILURE);
            }
        }
        // A grid or pre-built (fleet) window is born tiled: resize so every pane's
        // pty/emulator gets its true inner rect (spawn used rough sizes).
        if grid.is_some() || prebuilt {
            resize_window(&mut windows[0], rows, cols);
        }
        let active = 0usize; // active window index
        let scanner = PrefixScanner::new();
        // Mouse capture is OFF by default so the terminal's own **text selection**
        // works out of the box (dragging highlights, as in any shell) — the common
        // case. `Ctrl+A m` toggles capture ON when you want the atrium mouse: click to
        // focus the pane under the cursor, wheel to scroll the hovered tile. (With
        // capture on, a drag selects within one tile via atrium::select; the host's
        // native selection falls back to Shift-drag.) The scanner and the
        // terminal are kept in sync by the toggle.
        let mouse_on = false;
        // A drag-selection in progress inside one tile (mouse on, tiled window), with
        // the window it was started in: press starts it, drags extend it, release
        // copies it (atrium::select).
        let selection: Option<(usize, atrium::select::Selection)> = None;
        // Whether atrium has forced the OUTER terminal's mouse reporting off because the
        // focused pane does not want the mouse (a shell/WSL). A mouse app (claude)
        // enables motion tracking via passthrough, which leaks to the terminal; once
        // you switch to a non-mouse pane the terminal keeps sending motion events and
        // they get forwarded into that pane as garbage text. See the re-assert below.
        let outer_mouse_off = false;
        // The full-screen overlays (overview / board / activity log) and their cursors.
        let views = Views::default();
        // The command prompt (`Ctrl+A :`): `Some(line)` while the operator is typing a
        // command to open in a new pane (any shell/program, not just the launch one).
        // Keystrokes edit the line instead of reaching the panes; Enter opens it.
        let prompt: Option<String> = None;
        let buf = [0u8; 8192];
        let force_repaint = true;
        let last_size_check = Instant::now();

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
        let board = match std::env::var_os(atrium::board::ENV_BOARD) {
            Some(p) if !p.is_empty() => {
                atrium::board::Board::with_file(std::path::PathBuf::from(p))
            }
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
        let last_sidecar_flush = Instant::now();
        // A fleet that declared a topic vocabulary switches the bus to strict
        // admission; without one it stays soft-gated. Set before any pane is spawned,
        // so the very first publish is already governed by the right policy.
        if let Some(topics) = &canonical_topics {
            bus.set_canonical(topics);
        }
        // Agent state is refreshed on its own thread (`AgentWatch`); the loop holds the
        // newest snapshot and never reads a transcript itself.
        let agents = atrium::vendors::AgentWatch::start(agsess::sessions::now_ms());
        let world = atrium::vendors::AgentState::default();
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
        let safety_net = SafetyNet::new();
        // The paint's memory between ticks (retained tiled frame, throttles); see
        // `renderer`.
        let renderer = Renderer::new();

        // What wakes the loop: keys on their own thread, and a reader thread per
        // pane (started at the top of each tick), all ringing one doorbell.
        let (bell, wake) = atrium::events::doorbell();
        let keys = atrium::events::KeyReader::start(term.input(), bell.clone()).ok();

        Ok(RunState {
            term,
            out,
            rows,
            cols,
            size_watch,
            windows,
            flash,
            pending_sends,
            max_depth,
            ctl_extra_allow,
            ctl_listener,
            ctl_audit,
            active,
            scanner,
            mouse_on,
            selection,
            outer_mouse_off,
            views,
            prompt,
            buf,
            force_repaint,
            last_size_check,
            board,
            bus,
            sidecar_writer,
            last_sidecar_flush,
            agents,
            world,
            launch,
            safety_net,
            renderer,
            bell,
            wake,
            keys,
            deliberate_exit: false,
        })
    }
}
