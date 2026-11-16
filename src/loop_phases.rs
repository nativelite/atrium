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
                    if let Some(p) = sniff_paste_mode(&buf[..n]) {
                        pane.paste_wanted = p;
                    }
                    // The focused passthrough pane is the one under the splash:
                    // it is handed off by [`finish_splash`] instead, which also
                    // holds the logo up for its minimum time.
                    let under_splash = !tiled && pane.id == focus;
                    if !under_splash {
                        note_painted(pane);
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
        unstick_sync_frame(pane);
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
                if let Some(p) = sniff_paste_mode(&buf[..n]) {
                    pane.paste_wanted = p;
                }
                pane.activity = true;
                // Same rule as the active drain. Without it a pane that did all
                // its drawing while its window was in the background was never
                // marked painted, so switching to that window showed the
                // "starting…" spinner over real content (and kept the frame
                // animating) until the pane happened to write again.
                note_painted(pane);
            }
            unstick_sync_frame(pane);
        }
    }
}

/// Mark a pane painted once its emulator holds *visible* content — not merely
/// once setup bytes arrived. One rule for both drains; a pane that has painted
/// stays painted (a respawn resets the flag for the new process).
pub(crate) fn note_painted(pane: &mut Pane) {
    if !pane.painted && !term_blank(&pane.term) {
        pane.painted = true;
    }
}

/// How long a pane may sit inside a synchronized update (DEC mode 2026) with no
/// bytes arriving before the host closes the update for it. Long enough that a
/// working app is never cut mid-frame (a frame's bytes come in one burst); short
/// enough that a stuck one is not a visible freeze.
pub(crate) const SYNC_STUCK_AFTER: Duration = Duration::from_millis(250);

/// Whether an open synchronized update has been abandoned: the pane is inside
/// one and has been byte-quiet for longer than `limit`. Pure, so the rule is
/// testable without a terminal. The silence term is what keeps this from
/// cutting a live redraw in half.
pub(crate) fn sync_frame_stuck(in_sync: bool, quiet_for: Duration, limit: Duration) -> bool {
    in_sync && quiet_for > limit
}

/// Close an abandoned synchronized update on the host side. While the update is
/// open the emulator serves the frame from *before* it, and only the app's own
/// `?2026l` or a resize ends that — so an app that opened one and then blocked
/// (on a query its pty never answers, say) froze its tile for good. Feeding the
/// close to our emulator reveals what the app has drawn since; nothing is sent
/// to the child. `last_activity` is read, never written: it is what ctl reports
/// as `idle_ms`, and this is not activity.
pub(crate) fn unstick_sync_frame(pane: &mut Pane) {
    if sync_frame_stuck(
        pane.term.in_sync(),
        pane.last_activity.elapsed(),
        SYNC_STUCK_AFTER,
    ) {
        pane.term.feed(b"\x1b[?2026l");
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

/// Is the agent in a pane up, as far as atrium can tell, so that its mod
/// could have said hello by now? One of three things, each of which this
/// very process caused:
///
/// - its vendor's prompt hint is on its screen (what a queued send waits for);
/// - something reported a status for it through `ctl report` (its status
///   hooks or its mod: either means the engine is running this pane's hooks);
/// - its session's transcript holds an event newer than the pane's launch.
///
/// A status that merely *exists* is not enough: a resumed or recovered
/// session has a transcript, and a status inferred from it, before its
/// claude has drawn a thing. Nor is a startup dialog (folder trust, a login),
/// however long it sits there. Pure.
pub(crate) fn started_by(
    hint_on_screen: bool,
    reported: bool,
    transcript_ms: Option<u64>,
    launch_ms: u64,
) -> bool {
    hint_on_screen || reported || transcript_ms.is_some_and(|t| t > launch_ms)
}

/// [`started_by`] for a live pane.
pub(crate) fn pane_started(
    pane: &Pane,
    world: &atrium::vendors::AgentState,
    mods: &atrium::modstate::ModState,
) -> bool {
    let hint = atrium::vendors::vendor_for_stem(&pane.title)
        .and_then(atrium::vendors::prompt_marker)
        .is_some_and(|hint| crate::ctl_server::screen_has(pane.term.screen(), hint));
    let reported = mods
        .report_for(pane.agent_id.0)
        .is_some_and(|r| r.status.is_some());
    let transcript_ms = pane.session_id.as_deref().and_then(|id| {
        world
            .sessions()
            .into_iter()
            .find(|s| s.id == id)
            .and_then(|s| s.last_ts_ms)
    });
    started_by(hint, reported, transcript_ms, pane.launch_ms)
}

/// Does this session have a mod whose health is atrium's to watch: it uses
/// the mod (`"mod": false` in the config or the fleet says no), it has a
/// control plane for a hello to arrive on, and it has launched at least one
/// pane with the mod? Otherwise the health phase does nothing: no window, no
/// bar line, no decision, no record, whatever any pane reports. Pure.
pub(crate) fn health_runs(enabled: bool, ctl_on: bool, ever_injected: bool) -> bool {
    enabled && ctl_on && ever_injected
}

/// The bar's one flash slot: a line and when it was raised.
pub(crate) type Flash = Option<(String, Instant)>;

/// What the health phase writes to, lent for one pass.
pub(crate) struct HealthOut<'a> {
    pub(crate) flash: &'a mut Option<(String, Instant)>,
    pub(crate) audit: &'a mut atrium::audit::Audit,
    pub(crate) bus: &'a mut atrium::bus::Bus,
}

/// Act on what a health tick found (`atrium::modstate::HealthEvent`): say
/// each on the bar (`said` is called before each new line, so a headless
/// session prints every one), record it, and for a trip post the one
/// decision that offers a respawn of the panes still running the mod.
/// Nothing here restarts, kills or types into a pane. `label` names a pane
/// for a person; `recovered` and `trip` are `atrium::modfiles`' two calls,
/// passed in so this is testable without the session's globals.
pub(crate) fn apply_health_events(
    events: Vec<atrium::modstate::HealthEvent>,
    now_ms: u64,
    out: &mut HealthOut<'_>,
    label: &dyn Fn(usize) -> Option<String>,
    said: &mut dyn FnMut(&mut Flash),
    recovered: &mut dyn FnMut() -> Option<String>,
    trip: &mut dyn FnMut(&str, bool) -> bool,
) {
    use atrium::modstate::HealthEvent;
    for event in events {
        said(out.flash);
        match event {
            HealthEvent::Healthy { .. } => {
                if let Some(line) = recovered() {
                    out.audit.record(None, "mod-back", "", true, "");
                    *out.flash = Some((line, Instant::now()));
                }
            }
            HealthEvent::SetAside { pane, why } => {
                out.audit.record(
                    None,
                    "mod-set-aside",
                    &format!("pane={pane}"),
                    true,
                    why.label(),
                );
                *out.flash = Some((
                    format!(
                        "pane {pane}: mod set aside ({}); it runs as a pane without one \
                         (sends are typed)",
                        why.label()
                    ),
                    Instant::now(),
                ));
            }
            HealthEvent::InboxLost { pane } => {
                out.audit
                    .record(None, "mod-inbox-lost", &format!("pane={pane}"), true, "");
                *out.flash = Some((
                    format!(
                        "pane {pane}: its mod stopped collecting deliveries; sends are typed \
                         into it now"
                    ),
                    Instant::now(),
                ));
            }
            HealthEvent::FeatureLost { pane, feature } => {
                out.audit.record(
                    None,
                    "mod-feature-lost",
                    &format!("pane={pane} feature={feature}"),
                    true,
                    "",
                );
                *out.flash = Some((
                    format!(
                        "pane {pane}: its mod lost `{feature}` (this Claude Code lacks what it \
                         calls); the rest of the mod goes on"
                    ),
                    Instant::now(),
                ));
            }
            HealthEvent::Tripped {
                reason,
                running,
                remember,
            } => {
                let remembered = trip(&reason, remember);
                let labels: Vec<String> = running.iter().filter_map(|id| label(*id)).collect();
                out.audit.record(
                    None,
                    "mod-breaker",
                    &format!("running={} remembered={remembered}", labels.len()),
                    true,
                    &reason,
                );
                if !labels.is_empty() {
                    if let Err(e) = out.bus.publish(
                        "atrium",
                        atrium::bus::Kind::DecisionNeeded,
                        None,
                        &[("msg".to_string(), trip_decision(&reason, &labels))],
                        now_ms,
                    ) {
                        out.audit
                            .record(None, "mod-breaker", "decision not posted", false, &e);
                    }
                }
                *out.flash = Some((trip_line(&reason, remembered, labels.len()), Instant::now()));
            }
        }
    }
}

/// The one line the bar says when the breaker trips.
pub(crate) fn trip_line(reason: &str, remembered: bool, running: usize) -> String {
    let mut line = format!(
        "mod backed out: {reason}. New claude panes launch without it{}",
        if remembered {
            ", in later sessions too until Claude Code or the mod changes"
        } else {
            " for this session"
        }
    );
    if running > 0 {
        line.push_str(&format!(
            "; the {running} pane{} that {} it {} left as {} (see the bus)",
            if running == 1 { "" } else { "s" },
            if running == 1 { "has" } else { "have" },
            if running == 1 { "is" } else { "are" },
            if running == 1 { "it is" } else { "they are" }
        ));
    }
    line.push_str(". `atrium mod status` says more");
    line
}

/// The decision posted when the breaker trips with panes still running the
/// mod: atrium does not restart a working agent, it offers.
pub(crate) fn trip_decision(reason: &str, running: &[String]) -> String {
    format!(
        "the mod was backed out ({reason}). {} still run{} with it loaded and {} left alone: \
         respawn one with `atrium ctl respawn <pane>` to drop the mod from it, or resolve this \
         to leave them",
        running.join(", "),
        if running.len() == 1 { "s" } else { "" },
        if running.len() == 1 { "is" } else { "are" }
    )
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
            // The pane's own title (`OSC 0`/`2`, kept by its emulator), as
            // its badge; never when it only repeats the program.
            badge: w
                .bar_pane()
                .and_then(|p| atrium::attention::pane_badge(p.term.title(), &p.title)),
            // The flag on its mod, from the broker's state, in a segment of
            // its own: a pane's title cannot spell it.
            mod_note: w.bar_pane().and_then(|p| world.mod_note_for(p.agent_id.0)),
            active: i == active,
            // A window is "waiting" when a bound agent pane in it is blocked
            // on the human (§4.2 rung 3). Status only.
            attention: atrium::bar::Attention::from_facts(
                w.panes.iter().all(|p| p.exited),
                w.panes.iter().any(|p| {
                    matches!(
                        world.status_for(p.session_id.as_deref()),
                        Some(agsess::Status::WaitingApproval)
                    ) || world.errored_for(p.session_id.as_deref())
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
/// What a pane's program said since the last tick, folded from its emulator's
/// notices (`vterm::Notice`, 0.5.6): its title, whether it rang, and its last
/// notification. Read by [`pane_notices`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PaneNotices {
    /// The title the program set (`OSC 0`/`2`, or a title-stack restore), as
    /// the emulator keeps it; not only since the last tick.
    pub(crate) title: Option<String>,
    /// A `BEL` on its own since the last tick (bells in a row count once).
    pub(crate) bell: bool,
    /// The last `OSC 9 ; text` since the last tick.
    pub(crate) toast: Option<String>,
}

/// Drain the pane's emulator notices into a [`PaneNotices`]. The emulator is
/// fed in both drains ([`drain_active_window`], [`drain_background_windows`]),
/// tiled or passthrough, so this sees what every pane says; it holds at most
/// 64 notices between reads and the loop reads once a tick (phase 5c). A
/// `Progress` notice is dropped: atrium drives the host's own indicator from
/// the fleet's state, not from one pane's.
pub(crate) fn pane_notices(pane: &mut Pane) -> PaneNotices {
    let mut n = PaneNotices::default();
    if pane.term.has_notices() {
        for notice in pane.term.take_notices() {
            match notice {
                vterm::Notice::Bell => n.bell = true,
                vterm::Notice::Toast(text) => n.toast = Some(text),
                vterm::Notice::Title(_) | vterm::Notice::Progress { .. } => {}
            }
        }
    }
    n.title = pane.term.title().map(str::to_string);
    n
}

/// What `attention` needs of every pane: a stable key, a label (role, else
/// title), whether it exited, its bound agent's status, and what its program
/// said this tick (a bell, a toast). Drains each pane's notices: once a tick.
pub(crate) fn attention_facts(
    windows: &mut [Window],
    world: &atrium::vendors::AgentState,
) -> Vec<atrium::attention::PaneFact> {
    windows
        .iter_mut()
        .flat_map(|w| w.panes.iter_mut())
        .map(|p| {
            let said = pane_notices(p);
            atrium::attention::PaneFact {
                key: p.agent_id.0 as u64,
                label: p.role.clone().unwrap_or_else(|| p.title.clone()),
                exited: p.exited,
                status: world.status_for(p.session_id.as_deref()),
                bell: said.bell,
                toast: said.toast,
            }
        })
        .collect()
}

pub(crate) fn decision_note(bus: &atrium::bus::Bus, windows: &[Window]) -> String {
    let decisions_open = bus
        .pending_decisions()
        .iter()
        .filter(|e| !decision_for_agent(e, windows))
        .count();
    match decisions_open {
        0 => String::new(),
        1 => "1 decision needs you \u{00b7} Ctrl+A b".to_string(),
        n => format!("{n} decisions need you \u{00b7} Ctrl+A b"),
    }
}

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
    sniff_private_mode(buf, &["1000", "1002", "1003"])
}

/// [`sniff_mouse_mode`] for bracketed paste, private mode 2004: whether the
/// app wants a paste delimited (`ESC[200~ … ESC[201~`). Drives
/// `Pane::paste_wanted`, which decides how a queued send is typed in.
fn sniff_paste_mode(buf: &[u8]) -> Option<bool> {
    sniff_private_mode(buf, &["2004"])
}

/// The last DECSET (`Some(true)`) or DECRST (`Some(false)`) of any of `modes`
/// in `buf`, `None` when it names none of them.
fn sniff_private_mode(buf: &[u8], modes: &[&str]) -> Option<bool> {
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
                    .map(|s| s.split(';').any(|p| modes.contains(&p)))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane for the drain rules: a real (idle) shell, so the fields the rules
    /// read exist, with its emulator driven directly by the test.
    fn idle_pane() -> Pane {
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        let mut flash = None;
        spawn_pane(
            &[shell.to_string()],
            5,
            20,
            1,
            None,
            atrium::ctl::TrustMode::Off,
            &mut flash,
        )
        .expect("a shell to host the pane")
    }

    /// What the program says lands on the pane through its emulator: a title
    /// and a bell after one feed, read once and then gone (the title stays,
    /// the bell does not); a toast carries its words.
    #[test]
    fn notices_land_on_the_pane_after_a_feed() {
        let mut pane = idle_pane();
        let before = pane_notices(&mut pane);
        assert_eq!(
            before,
            PaneNotices::default(),
            "a fresh pane has said nothing"
        );
        pane.term.feed(b"\x1b]2;hi\x07\x07");
        let said = pane_notices(&mut pane);
        assert_eq!(said.title.as_deref(), Some("hi"));
        assert!(
            said.bell,
            "the bare BEL rang; the title's terminator did not"
        );
        assert_eq!(said.toast, None);
        let again = pane_notices(&mut pane);
        assert_eq!(again.title.as_deref(), Some("hi"), "the title is kept");
        assert!(!again.bell, "a bell is read once");
        pane.term.feed(b"\x1b]9;done\x07\x1b]9;4;1;50\x07");
        let said = pane_notices(&mut pane);
        assert_eq!(said.toast.as_deref(), Some("done"));
        assert!(!said.bell, "a progress report is not a bell");
    }

    /// Feeding the close to the emulator reveals what the app drew inside the
    /// abandoned update — but only once the pane has been quiet past the limit,
    /// and without touching the activity clock ctl reports as `idle_ms`.
    #[test]
    fn an_abandoned_sync_update_is_closed_after_the_quiet_limit() {
        let mut pane = idle_pane();
        pane.term.feed(b"\x1b[?2026hX");
        assert!(pane.term.in_sync(), "the update is open");
        assert_ne!(pane.term.screen().cell(0, 0).ch, 'X', "the frame is held");

        // Bytes just arrived: a live frame, left alone.
        pane.last_activity = Instant::now();
        unstick_sync_frame(&mut pane);
        assert!(pane.term.in_sync(), "a live frame is not cut");

        // Quiet past the limit: closed for the app, revealing the drawn cell.
        let quiet_since = Instant::now() - SYNC_STUCK_AFTER - Duration::from_millis(50);
        pane.last_activity = quiet_since;
        unstick_sync_frame(&mut pane);
        assert!(!pane.term.in_sync(), "the abandoned update is closed");
        assert_eq!(
            pane.term.screen().cell(0, 0).ch,
            'X',
            "the frame is revealed"
        );
        assert_eq!(
            pane.last_activity, quiet_since,
            "unsticking is not activity"
        );
        let _ = pane.pty.kill();
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

    /// The breaker's one bar line says why, what happens to new panes, for
    /// how long, and that nothing running was touched.
    #[test]
    fn the_trip_line_says_why_how_long_and_that_running_panes_are_left() {
        use super::trip_line;
        let line = trip_line("2 claude panes loaded it and never said hello", true, 2);
        assert!(line.starts_with("mod backed out: 2 claude panes loaded it and never said hello."));
        assert!(
            line.contains("until Claude Code or the mod changes"),
            "{line}"
        );
        assert!(
            line.contains("the 2 panes that have it are left as they are"),
            "{line}"
        );
        assert!(!line.contains("relaunch"), "{line}");
        assert!(line.ends_with("`atrium mod status` says more"), "{line}");
        let session_only = trip_line("why", false, 0);
        assert!(session_only.contains("for this session"), "{session_only}");
        assert!(!session_only.contains("left as"));
        let one = trip_line("why", true, 1);
        assert!(
            one.contains("the 1 pane that has it is left as it is"),
            "{one}"
        );
    }

    /// A working pane is offered a respawn on the bus, by name, never given one.
    #[test]
    fn the_trip_decision_names_the_panes_and_offers_a_respawn() {
        use super::trip_decision;
        let one = trip_decision("why", &["2 (builder)".to_string()]);
        assert!(
            one.contains("2 (builder) still runs with it loaded and is left alone"),
            "{one}"
        );
        assert!(one.contains("atrium ctl respawn <pane>"), "{one}");
        let two = trip_decision("why", &["1".to_string(), "2".to_string()]);
        assert!(
            two.contains("1, 2 still run with it loaded and are left alone"),
            "{two}"
        );
    }

    /// The hello window opens on something this process's pane did: a status
    /// that was already there for a resumed or recovered session is not it.
    #[test]
    fn a_pane_is_started_by_its_own_doing_not_by_a_transcript_that_was_already_there() {
        use super::started_by;
        let launch: u64 = 10_000_000;
        // A recovered session: its transcript, and the status inferred from
        // it, are an hour old when the pane is launched. Not started.
        assert!(!started_by(false, false, Some(launch - 3_600_000), launch));
        assert!(!started_by(false, false, Some(launch), launch));
        // It writes to the transcript: started.
        assert!(started_by(false, false, Some(launch + 1), launch));
        // No transcript at all (a fresh session at a trust dialog): not started.
        assert!(!started_by(false, false, None, launch));
        // The prompt hint, or a status reported through ctl: started.
        assert!(started_by(true, false, None, launch));
        assert!(started_by(false, true, Some(launch - 3_600_000), launch));
    }

    /// A session with no mod to watch runs no health phase.
    #[test]
    fn the_health_phase_runs_only_where_there_is_a_mod_to_watch() {
        use super::health_runs;
        assert!(health_runs(true, true, true));
        assert!(!health_runs(false, true, true), "\"mod\": false");
        assert!(!health_runs(true, false, true), "no control plane");
        assert!(!health_runs(true, true, false), "no pane was given the mod");
    }

    /// What the health phase does with each thing a tick finds, with the
    /// session's two globals stood in for.
    #[test]
    fn health_events_become_bar_lines_audit_rows_and_one_decision() {
        use super::{apply_health_events, HealthOut};
        use atrium::modstate::{Aside, HealthEvent};
        let mut flash = None;
        let mut audit = atrium::audit::Audit::in_memory();
        let mut bus = atrium::bus::Bus::new();
        let mut said: Vec<String> = Vec::new();
        let mut trips: Vec<(String, bool)> = Vec::new();
        let mut recoveries = 0;
        let events = vec![
            HealthEvent::Healthy { pane: 3 },
            HealthEvent::SetAside {
                pane: 1,
                why: Aside::NoHello,
            },
            HealthEvent::InboxLost { pane: 4 },
            HealthEvent::FeatureLost {
                pane: 3,
                feature: "inbox",
            },
            HealthEvent::Tripped {
                reason: "2 claude panes loaded it and never said hello".to_string(),
                running: vec![1, 2, 99],
                remember: true,
            },
        ];
        apply_health_events(
            events,
            5,
            &mut HealthOut {
                flash: &mut flash,
                audit: &mut audit,
                bus: &mut bus,
            },
            // Pane 99 left between the tick and here: it is simply not named.
            &|id| (id != 99).then(|| format!("{id} (builder)")),
            &mut |flash| {
                if let Some((line, _)) = flash.take() {
                    said.push(line);
                }
            },
            &mut || {
                recoveries += 1;
                Some("mod: back on".to_string())
            },
            &mut |reason, remember| {
                trips.push((reason.to_string(), remember));
                true
            },
        );
        // Every line was said, in order; the last one is still on the bar.
        assert_eq!(said.len(), 4, "{said:?}");
        assert_eq!(said[0], "mod: back on");
        assert!(said[1].starts_with("pane 1: mod set aside (no hello within its window)"));
        assert!(said[2].starts_with("pane 4: its mod stopped collecting"));
        assert!(said[3].starts_with("pane 3: its mod lost `inbox`"));
        let last = flash.expect("the trip line").0;
        assert!(
            last.starts_with("mod backed out:") && last.contains("the 2 panes"),
            "{last}"
        );
        assert_eq!(recoveries, 1);
        // The trip went to modfiles once, with what the tick decided.
        assert_eq!(
            trips,
            vec![(
                "2 claude panes loaded it and never said hello".to_string(),
                true
            )]
        );
        // One decision, naming the panes, offering a respawn.
        let open = bus.pending_decisions();
        assert_eq!(open.len(), 1);
        let msg = open[0].fields.get("msg").cloned().unwrap_or_default();
        assert!(
            msg.contains("1 (builder), 2 (builder) still run")
                && msg.contains("atrium ctl respawn"),
            "{msg}"
        );
        // And nothing here could have restarted a pane: it holds no pane.

        // A trip with nobody running posts no decision; one not to remember
        // is passed on as such.
        let mut flash = None;
        let mut bus = atrium::bus::Bus::new();
        let mut asked = None;
        apply_health_events(
            vec![HealthEvent::Tripped {
                reason: "why".to_string(),
                running: vec![],
                remember: false,
            }],
            5,
            &mut HealthOut {
                flash: &mut flash,
                audit: &mut audit,
                bus: &mut bus,
            },
            &|_| None,
            &mut |_| {},
            &mut || None,
            &mut |_, remember| {
                asked = Some(remember);
                false
            },
        );
        assert_eq!(asked, Some(false));
        assert!(bus.pending_decisions().is_empty());
        assert!(flash.expect("a line").0.contains("for this session"));
    }
}
