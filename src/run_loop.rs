//! The session's event loop. [`run`] opens a session ([`RunState::start`]),
//! ticks it until the operator quits, every pane ends, a termination signal
//! arrives or the terminal is lost, and then tears it down
//! ([`RunState::teardown`]). One [`RunState::tick`] is one pass of the loop:
//! each phase is a method, called in the order the loop has always run them.

mod keys;
mod resize;
mod setup;
mod teardown;

#[cfg(test)]
mod tests;

use crate::*;

/// What a session is launched with: the three launch paths (a plain launch,
/// `fleet up` and `recover`) each build one and hand it to [`run`]. Named fields,
/// because several are `Option`s a positional call passed as runs of `None`.
pub(crate) struct RunArgs<'a> {
    /// The command new panes host (`Ctrl+A c`, splits), under `identity`.
    pub(crate) command: &'a [String],
    pub(crate) identity: Option<&'a str>,
    /// Mass-spawn (`-n` / `--grid`): the first window as a grid of `command`.
    pub(crate) grid: Option<atrium::spawn::Grid>,
    /// A pre-built initial window (the fleet loader spawns its own panes, one per
    /// agent, each with its own identity/cwd). When `Some`, it is used verbatim
    /// as the first window and `command`/`grid` are ignored for it — but they
    /// still drive later `Ctrl+A c` new panes and splits (which host `command`
    /// under `identity`), so a fleet's new panes open a shell as a scratch pane.
    pub(crate) initial_window: Option<Window>,
    /// ctl control plane (§ atrium-ctl-control-plane): when `allow_ctl`, atrium binds
    /// a per-process control endpoint and injects its address into every pane, so
    /// an agent inside a pane can `atrium ctl spawn/list`. `max_depth` is the
    /// recursion guard (usize::MAX == unlimited). Off ⇒ pre-ctl behavior verbatim.
    pub(crate) allow_ctl: bool,
    pub(crate) max_depth: usize,
    /// How atrium relaxes each spawned agent's permissions: `Off`, `Edits`
    /// (`--trust`: acceptEdits + safe allowlist), or `Skip` (`--skip-permissions`:
    /// full bypass). Published to the spawn path via `AGENT_TRUST` before the
    /// first spawn.
    pub(crate) trust: atrium::ctl::TrustMode,
    /// A pre-bound ctl endpoint. The fleet path binds it *before* spawning its
    /// panes (so they get `ATRIUM_CTL` in their env) and hands the listener here;
    /// `None` means run() binds its own after the initial spawn (the default path).
    pub(crate) ctl_listener: Option<atrium::ipc::Listener>,
    /// The fleet's declared canonical topic vocabulary (fleet config `"topics": []`),
    /// if any. `Some` ⇒ the bus enforces strict topic admission (agents route only
    /// on declared topics); `None` ⇒ topics are soft-gated (a novel topic needs
    /// `--new`). The default, non-fleet path passes `None`.
    pub(crate) canonical_topics: Option<Vec<String>>,
}

/// Run one atrium session on `term` until it ends, and return the process's
/// exit code: a failure only when the first window cannot start.
pub(crate) fn run(term: &mut rawterm::Terminal, args: RunArgs<'_>) -> ExitCode {
    let mut state = match RunState::start(term, args) {
        Ok(state) => state,
        Err(code) => return code,
    };
    while state.tick() == Flow::Continue {}
    state.teardown()
}

/// Whether the loop runs another tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    /// Leave the loop for the teardown. Whether the exit was deliberate is
    /// recorded separately, in [`RunState::deliberate_exit`].
    Exit,
}

/// Everything the loop carries from one tick to the next. The fields are what
/// used to be `run()`'s locals, under the same names; [`RunState::start`]
/// explains each where it creates it.
struct RunState<'a> {
    term: &'a mut rawterm::Terminal,
    out: atrium::screen::Screen,
    rows: u16,
    cols: u16,
    size_watch: atrium::screen::SizeWatch,
    windows: Vec<Window>,
    flash: Option<(String, Instant)>,
    pending_sends: Vec<PendingSend>,
    max_depth: usize,
    ctl_extra_allow: Vec<String>,
    ctl_listener: Option<atrium::ipc::Listener>,
    ctl_audit: atrium::audit::Audit,
    /// The active window's index.
    active: usize,
    scanner: PrefixScanner,
    mouse_on: bool,
    selection: Option<(usize, atrium::select::Selection)>,
    outer_mouse_off: bool,
    views: Views,
    prompt: Option<String>,
    buf: [u8; 8192],
    force_repaint: bool,
    last_size_check: Instant,
    board: atrium::board::Board,
    bus: atrium::bus::Bus,
    sidecar_writer: atrium::session_store::SidecarWriter,
    last_sidecar_flush: Instant,
    agents: atrium::vendors::AgentWatch,
    world: atrium::vendors::AgentState,
    /// What `Ctrl+A c`, splits and the command prompt launch under; its `job`
    /// is the session job.
    launch: Launch<'a>,
    safety_net: SafetyNet,
    renderer: Renderer,
    bell: atrium::events::Doorbell,
    wake: atrium::events::Wake,
    keys: Option<atrium::events::KeyReader>,
    /// Set only where the operator quit or every pane ended. A termination signal
    /// (SIGHUP from a closed window, SIGTERM from a shutdown) and a lost terminal
    /// leave through the same teardown but are NOT deliberate — the session was
    /// taken away, and the next launch should offer it back.
    deliberate_exit: bool,
}

impl RunState<'_> {
    /// One pass of the loop. [`Flow::Exit`] ends the session.
    fn tick(&mut self) -> Flow {
        // 0. a termination signal (SIGHUP from a closed terminal window, or a
        // SIGTERM/SIGINT from outside) exits through this *normal* path, so the
        // pane teardown after the loop actually runs. Without it the default
        // action killed atrium outright and every hosted agent was orphaned onto
        // `init` — see `atrium::signals`.
        if atrium::signals::terminating() {
            if std::env::var_os("ATRIUM_DEBUG").is_some() {
                eprint!("[atrium-dbg signal-quit]\r\n");
            }
            return Flow::Exit;
        }
        // 0b. The safety net: crash registry, session snapshot, warden tripwires,
        // and (unix) the orphan watchdog.
        if self.safety_net.tick(
            &self.windows,
            self.launch.job,
            &mut self.ctl_audit,
            &mut self.bus,
            &mut self.flash,
        ) {
            self.force_repaint = true;
        }
        if self.last_sidecar_flush.elapsed() >= SNAPSHOT_INTERVAL {
            self.last_sidecar_flush = Instant::now();
            for write in [
                self.bus.take_pending_write(),
                self.board.take_pending_write(),
            ]
            .into_iter()
            .flatten()
            {
                self.sidecar_writer.submit(write);
            }
        }
        // 1. wait for a key or pane output — woken the moment either arrives —
        //    or at most a tick, for the loop's timed work. Then keystrokes ->
        //    scanner -> focused pane / commands.
        start_pane_readers(&mut self.windows, &self.bell);
        let Some(bytes) = self.wait_for_input() else {
            return Flow::Exit;
        };
        if self.handle_keys(&bytes) == Flow::Exit {
            return Flow::Exit;
        }

        self.serve_ctl();

        // 1c. flush any queued `ctl send`s whose target is now idle (Decision 4).
        if flush_sends(&mut self.pending_sends, &mut self.windows, &self.world) {
            self.force_repaint = true;
        }

        // 2. drain every pane in the active window (all panes are live in
        //    tiled mode); feed the emulator, and in passthrough also write the
        //    focused pane's cleaned bytes straight through.
        let drained = drain_active_window(
            &mut self.windows[self.active],
            self.views.any(),
            &mut self.buf,
            &mut self.out,
        );
        if drained.force_repaint {
            self.force_repaint = true;
        }
        // 2b. hand the focused pane off from the startup logo once it has drawn
        //     something and the logo has been up long enough to be seen.
        if finish_splash(
            &mut self.windows[self.active],
            self.views.any(),
            self.renderer.splash_hold_over(),
            &mut self.out,
        ) {
            self.force_repaint = true;
        }
        let tiled_dirty = drained.tiled_dirty;

        // 3. background windows: drain (discarded — emulator/ConPTY keep the
        //    screen) and flag activity so the bar shows it.
        drain_background_windows(&mut self.windows, self.active, &mut self.buf);

        if self.reap_panes() == Flow::Exit {
            return Flow::Exit;
        }

        self.reassert_mouse_off();

        self.track_resize();

        self.refresh_agents();

        self.resync_screen();

        // 6-7. paint the active window and the bar as one synchronized frame.
        self.renderer.paint(
            &mut self.windows,
            self.active,
            &mut self.views,
            &self.selection,
            self.prompt.as_deref(),
            &self.world,
            &self.board,
            &self.bus,
            &mut self.flash,
            self.force_repaint,
            tiled_dirty,
            drained.pane_output,
            self.rows,
            self.cols,
            &mut self.out,
        );
        self.force_repaint = false;
        // Hand anything a phase wrote without flushing to the terminal this tick.
        let _ = self.out.flush();
        Flow::Continue
    }

    fn serve_ctl(&mut self) {
        // 1b. ctl control channel: drain up to a few requests this tick
        //     (non-blocking; usually zero). Each request is applied as a pane
        //     operation and answered on the same connection. A spawn appends a
        //     visible new window, so we force a repaint after any request.
        if let Some(listener) = self.ctl_listener.as_mut() {
            for _ in 0..8 {
                match listener.poll() {
                    Ok(Some(line)) => {
                        let reply = apply_ctl(
                            &line,
                            &mut CtlSession {
                                windows: &mut self.windows,
                                rows: self.rows,
                                cols: self.cols,
                                max_depth: self.max_depth,
                                extra_allow: &self.ctl_extra_allow,
                                pending: &mut self.pending_sends,
                                world: &self.world,
                                session_identity: self.launch.identity,
                                audit: &mut self.ctl_audit,
                                board: &mut self.board,
                                bus: &mut self.bus,
                                job: self.launch.job,
                            },
                        );
                        let _ = listener.respond(&reply.to_json());
                        self.renderer.reset();
                        self.force_repaint = true;
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        }
    }

    fn reap_panes(&mut self) -> Flow {
        // 4. reap exits; drop dead panes and empty windows, re-tiling.
        let layout_changed = drop_exited_panes(&mut self.windows);
        if layout_changed {
            let empty_before_active = self.windows[..self.active]
                .iter()
                .filter(|w| w.panes.is_empty())
                .count();
            self.windows.retain(|w| !w.panes.is_empty());
            if self.windows.is_empty() {
                // Every agent ended on its own: nothing crashed, nothing to resume.
                self.deliberate_exit = true;
                return Flow::Exit;
            }
            self.active = active_after_reap(self.active, empty_before_active, self.windows.len());
            resize_window(&mut self.windows[self.active], self.rows, self.cols);
            self.renderer.reset();
            self.force_repaint = true;
        }
        Flow::Continue
    }

    fn reassert_mouse_off(&mut self) {
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
        if !self.mouse_on {
            if !self.outer_mouse_off {
                let _ = self.out.write_all(MOUSE_OFF.as_bytes());
                let _ = self.out.flush();
                self.outer_mouse_off = true;
            }
        } else {
            self.outer_mouse_off = false;
        }
    }

    fn refresh_agents(&mut self) {
        // 5b. agent state (§5), from the `AgentWatch` thread. Refresh every second
        //     while anything on screen shows agent state — an agent pane, or the
        //     overview / activity log — and every five seconds otherwise.
        let agent_on_screen = self.views.overview
            || self.views.log
            || self.windows.iter().any(|w| {
                w.panes.iter().any(|p| {
                    p.session_id.is_some() || atrium::vendors::vendor_for_stem(&p.title).is_some()
                })
            });
        self.agents.set_interval(if agent_on_screen {
            atrium::vendors::AgentWatch::ACTIVE
        } else {
            atrium::vendors::AgentWatch::QUIET
        });
        let mut did_refresh = false;
        if let Some(snapshot) = self.agents.latest() {
            self.world = snapshot;
            did_refresh = true;
            // Keep the overview and activity log live but calm: repaint once per
            // refresh (~1 Hz), not every tick, so they update without flicker.
            if self.views.overview || self.views.log {
                self.force_repaint = true;
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
            adopt_sessions(&mut self.windows, &self.world);
        }
    }

    fn resync_screen(&mut self) {
        // 5d. The terminal fell behind, output was discarded, and it has now
        //     caught up: what it shows is stale, so repaint the whole view from
        //     the emulators — the scroll region, the focused passthrough pane,
        //     and (via reset + force_repaint) the tiled frame, any overlay and
        //     the bar. The mouse-off assertion may have been discarded too.
        if self.out.take_resync() {
            let _ = write!(self.out, "\x1b[1;{}r\x1b[2J\x1b[H", self.rows - 1);
            if !self.views.any() && !self.windows[self.active].tiled() {
                let fp = self.windows[self.active].tree.focus();
                if self.windows[self.active]
                    .pane(fp)
                    .is_some_and(|p| p.painted)
                {
                    repaint_focused(
                        &mut self.windows[self.active],
                        self.rows,
                        self.cols,
                        &mut self.out,
                    );
                }
            }
            self.outer_mouse_off = false;
            self.renderer.reset();
            self.force_repaint = true;
        }
    }
}

/// The active window's index after a reap removed the empty windows:
/// `empty_before_active` of them sat before it, and `remaining` windows are
/// left (at least one). Pure.
fn active_after_reap(active: usize, empty_before_active: usize, remaining: usize) -> usize {
    active
        .saturating_sub(empty_before_active)
        .min(remaining - 1)
}
