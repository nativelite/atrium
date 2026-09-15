//! Self-contained phases of the event loop in `run()`, carved out as plain
//! functions with explicit inputs (r10 audit B11). Each body is the loop's code
//! verbatim; where it used to set the loop's `force_repaint` or read its locals,
//! it now takes arguments and returns what the loop must apply.

use crate::*;

/// Take a pane's next output without waiting: `Some(n)` bytes into `buf`,
/// `Some(0)` when its output has ended, `None` when nothing is waiting. From the
/// pane's reader thread once it has one; before that (or if a reader could not
/// be started) a zero-wait read of the pty itself. Never both at once: the loop
/// starts a reader before a tick's drains, and only reads the pty while it has
/// none.
pub(crate) fn pane_read(pane: &mut Pane, buf: &mut [u8]) -> Option<usize> {
    match &pane.inbox {
        Some(inbox) => match inbox.take(buf) {
            atrium::events::PaneRead::Bytes(n) => Some(n),
            atrium::events::PaneRead::Ended => Some(0),
            atrium::events::PaneRead::Empty => None,
        },
        None => match pane.pty.read_timeout(buf, Duration::ZERO) {
            Ok(Some(n)) => Some(n),
            _ => None,
        },
    }
}

/// Start a reader thread for every pane that has none, each ringing `bell`.
/// Cheap when there is nothing new: a scan of the panes.
pub(crate) fn start_pane_readers(windows: &mut [Window], bell: &atrium::events::Doorbell) {
    for pane in windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
        if pane.inbox.is_none() && !pane.exited {
            if let Ok(inbox) = pane
                .pty
                .reader()
                .and_then(|r| atrium::events::PaneInbox::start(r, bell.clone()))
            {
                pane.inbox = Some(inbox);
            }
        }
    }
}

/// What draining the active window this tick asks of the loop.
pub(crate) struct ActiveDrain {
    /// Repaint the bar/frame this tick (a first paint or a full-screen clear
    /// wiped the bar row).
    pub(crate) force_repaint: bool,
    /// Some pane's emulator changed (collected after all draining, resetting
    /// every pane's flag).
    pub(crate) tiled_dirty: bool,
    /// Any pane in the active window produced bytes this tick. The bar's timed
    /// refresh needs this: see [`crate::renderer::bar_refresh_due`].
    pub(crate) pane_output: bool,
}

/// Phase 2 of the loop: drain every pane in the active window `w` (all panes are
/// live in tiled mode), feed each emulator, and in passthrough also write the
/// focused pane's cleaned bytes straight to `out`. `overlay_up` suppresses the
/// passthrough paint while a full-screen overlay covers the pane.
pub(crate) fn drain_active_window(
    w: &mut Window,
    overlay_up: bool,
    buf: &mut [u8],
    out: &mut impl std::io::Write,
) -> ActiveDrain {
    let tiled = w.tiled();
    let focus = w.tree.focus();
    let mut force_repaint = false;
    let mut pane_output = false;
    for pane in w.panes.iter_mut() {
        // Drain each active pane *fully* before we composite, so a big data
        // burst (a large `ctl send`, a wall of tool output) is a whole frame
        // rather than a truncated one — a partial drain that straddles a
        // `?2026` block stalls the outer terminal (the "shutter"). The loop
        // still breaks the instant there is no more data, so the high cap only
        // bites on a genuinely huge burst; it never adds latency when idle.
        // No read waits here: the loop is woken the moment any pane writes (see
        // `atrium::events`), so whatever is due has already arrived.
        for _ in 0..DRAIN_READS_PER_TICK {
            match pane_read(pane, buf) {
                Some(n) if n > 0 => {
                    pane_output = true;
                    // Always feed the emulator so a later switch/split/zoom
                    // renders the current screen without a repaint nudge.
                    pane.term.feed(&buf[..n]);
                    // This pane just produced output → it is active now. Stamp
                    // the monotonic activity clock for ctl `idle_ms` (covers the
                    // focused pane and any background pane in the active window).
                    pane.last_activity = std::time::Instant::now();
                    // Track whether this pane's app wants the mouse (so scroll
                    // routes to it, not a bare shell).
                    if let Some(m) = sniff_mouse_mode(&buf[..n]) {
                        pane.mouse_wanted = m;
                    }
                    // "painted" = the emulator now has *visible* content, not
                    // merely that setup bytes arrived. The focused passthrough
                    // pane is the one under the splash: it is handed off by
                    // [`finish_splash`] instead, which also holds the logo up
                    // for its minimum time.
                    let under_splash = !tiled && pane.id == focus;
                    if !under_splash && !pane.painted && !term_blank(&pane.term) {
                        pane.painted = true;
                    }
                    if under_splash {
                        if overlay_up {
                            // An overlay (board / overview) is up: keep the
                            // passthrough filter state current, but don't paint
                            // the pane over the panel. (The emulator was already
                            // fed above, so a toggle-off repaints from the
                            // current screen.)
                            let _ = pane.filter.feed(&buf[..n]);
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
                    }
                }
                Some(_) => {
                    pane.exited = true;
                    break;
                }
                None => break,
            }
        }
    }
    if !tiled {
        let _ = out.flush();
    }

    // Collect per-pane dirty bits for the active window in one pass after
    // all draining is done. fold (not any) is required so every term's
    // flag is reset even when an earlier pane was already dirty.
    let tiled_dirty = w
        .panes
        .iter_mut()
        .fold(false, |d, p| d | p.term.take_dirty());
    ActiveDrain {
        force_repaint,
        tiled_dirty,
        pane_output,
    }
}

/// Whether the startup splash may hand off to its pane now: the pane has drawn
/// something, and either the logo has been up for its minimum time or the pane
/// has already exited (a one-shot command's output must not be lost to the
/// logo). Pure, so the rule is testable without a terminal.
pub(crate) fn splash_may_hand_off(has_content: bool, hold_over: bool, exited: bool) -> bool {
    has_content && (hold_over || exited)
}

/// Phase 2b of the loop: hand the focused passthrough pane off from the startup
/// splash once [`splash_may_hand_off`] allows it. Runs every tick, not only when
/// bytes arrive, so a shell that printed its prompt and went quiet during the
/// hold still takes over when the hold ends. Returns whether the bar must be
/// repainted this tick.
pub(crate) fn finish_splash(
    w: &mut Window,
    overlay_up: bool,
    hold_over: bool,
    out: &mut impl std::io::Write,
) -> bool {
    if w.tiled() {
        return false;
    }
    let focus = w.tree.focus();
    let Some(pane) = w.pane_mut(focus) else {
        return false;
    };
    if pane.painted || term_blank(&pane.term) {
        return false;
    }
    let exited = pane.exited || matches!(pane.pty.try_wait(), Ok(Some(_)));
    if !splash_may_hand_off(true, hold_over, exited) {
        return false;
    }
    pane.painted = true;
    if overlay_up {
        // The overlay's close repaints the pane from its emulator.
        return false;
    }
    // Restore the cursor the splash hid, then paint the pane's current screen
    // straight from the emulator (`render_full` clears and reflects everything
    // fed so far), and resume live passthrough from here.
    let full = pane.term.screen().render_full();
    let _ = out.write_all(b"\x1b[?25h");
    let _ = out.write_all(&full);
    let _ = out.flush();
    // render_full cleared the whole screen, bar row included: repaint the bar
    // this tick so it never blinks out during the handoff.
    true
}

/// Phase 3 of the loop: drain every background window (output is discarded —
/// the emulator/ConPTY keep the screen) and flag activity so the bar shows it.
/// Bounded per pane exactly like the active drain.
pub(crate) fn drain_background_windows(windows: &mut [Window], active: usize, buf: &mut [u8]) {
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
                let Some(n) = pane_read(pane, buf) else {
                    break;
                };
                reads += 1;
                if n == 0 {
                    pane.exited = true;
                    break;
                }
                pane.term.feed(&buf[..n]);
                pane.last_activity = std::time::Instant::now();
                if let Some(m) = sniff_mouse_mode(&buf[..n]) {
                    pane.mouse_wanted = m;
                }
                pane.activity = true;
            }
        }
    }
}

/// Phase 4 of the loop (first half): mark panes whose child exited, collapse
/// them out of each window's split tree and drop them. Returns whether any window
/// lost a pane (the caller removes emptied windows and re-tiles).
pub(crate) fn drop_exited_panes(windows: &mut [Window]) -> bool {
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
    layout_changed
}

/// Phase 5c of the loop: after a status refresh, stamp every still-unbound
/// non-claude agent pane with the newest session its vendor wrote at/after the
/// pane's launch (cwd-preferred), so it binds through the normal `status_for` path.
/// Claude panes already carry an injected `--session-id` and are skipped.
pub(crate) fn adopt_sessions(windows: &mut [Window], world: &atrium::vendors::AgentState) {
    let sessions = world.sessions();
    if !sessions.is_empty() {
        for w in windows.iter_mut() {
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
                let mine: Vec<&atrium::vendors::SessionView> = sessions
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

/// Phase 7 of the loop: one bar entry per window — its focused pane's title,
/// identity and role, whether it is active, and what it flags.
pub(crate) fn bar_infos(
    windows: &[Window],
    active: usize,
    world: &atrium::vendors::AgentState,
) -> Vec<PaneInfo> {
    windows
        .iter()
        .enumerate()
        .map(|(i, w)| PaneInfo {
            title: w.bar_pane().map(|p| p.title.clone()).unwrap_or_default(),
            active: i == active,
            // A window is "waiting" when a bound agent pane in it is blocked
            // on the human (§4.2 rung 3). Status only.
            attention: atrium::bar::Attention::from_facts(
                w.panes.iter().all(|p| p.exited),
                w.panes.iter().any(|p| {
                    matches!(
                        world.status_for(p.session_id.as_deref()),
                        Some(agsess::Status::WaitingApproval)
                    )
                }),
                w.panes.iter().any(|p| p.activity),
            ),
            // The window's identity tag: the bar pane's identity name (all
            // panes in a window inherit the same identity in v1). Name only.
            identity: w.bar_pane().and_then(|p| p.identity.clone()),
            // The window's persona/role (fleet agent name or ctl --role) —
            // of the FOCUSED pane, so the entry reads `N:claude:persona` for
            // the agent you are looking at, zoomed or tiled.
            role: w.bar_pane().and_then(|p| p.role.clone()),
        })
        .collect()
}

/// The bar note for open `decision_needed` escalations the human must answer —
/// empty when there are none. A decision addressed to a live teammate
/// (`to=<role>`) is that agent's to answer and is not counted.
pub(crate) fn decision_note(bus: &atrium::bus::Bus, windows: &[Window]) -> String {
    let decisions_open = bus
        .pending_decisions()
        .iter()
        .filter(|e| !decision_for_agent(e, &windows))
        .count();
    match decisions_open {
        0 => String::new(),
        1 => "1 decision needs you \u{00b7} Ctrl+A b".to_string(),
        n => format!("{n} decisions need you \u{00b7} Ctrl+A b"),
    }
}
