//! Loop phases 6 and 7: paint the active window (an overlay, the tiled
//! composite, or the passthrough splash/nudge) and the bar, as one synchronized
//! frame.
//!
//! Carved out of `run()` (r10 audit B11, step 4c). The eight locals that only the
//! paint read — the retained tiled frame, its scratch buffer, the spinner and
//! bar throttles, the last view identity — are [`Renderer`]'s fields, and every
//! loop site that dropped the retained frame now calls [`Renderer::reset`]. The
//! paint logic is unchanged.

use crate::*;
use atrium::select::Selection;

/// How long the startup logo stays up once it has appeared, even when the pane
/// behind it is ready sooner. A plain `atrium` hosts a shell that prints its
/// prompt within milliseconds, which used to wipe the splash before its first
/// frame reached the screen; the hold is what makes the logo visible at all
/// there. An agent that takes longer to boot is unaffected — its splash was
/// already up for seconds. A pane that exits during the hold ends it at once.
pub(crate) const SPLASH_MIN: Duration = Duration::from_millis(1200);

/// How long a pane must be quiet before the bar is repaired (see
/// [`bar_refresh_due`]). Comfortably longer than the console host's ~16 ms
/// frame, and short enough that a damaged bar is fixed before you notice.
const BAR_QUIET: Duration = Duration::from_millis(150);
/// The least time between two repairs.
const BAR_REFRESH: Duration = Duration::from_millis(500);

/// Whether to repaint the bar with bytes **identical** to what is already on
/// screen — a repair, not a change. A changed bar always paints at once; the
/// caller compares the bytes first.
///
/// Every write atrium makes opens a ~16 ms frame window in the Windows console
/// host, and a key that echoes inside that window waits for the window to close.
/// So a no-op repaint is not free: it is a 16 ms keystroke for whoever types
/// next. Measured with a relay (a pty inside a pty, no atrium) that echoed keys
/// at p99 0.17 ms while silent: repainting a bar on a 500 ms timer with nothing
/// to say took it to p99 8.7 ms, and on a 20 ms timer to a p50 of 12 ms. That
/// was the whole of atrium's Windows-only ~16 ms echo outliers.
///
/// The repair is worth one write only after a pane has actually printed
/// something *and then gone quiet* for [`BAR_QUIET`]:
/// - Nothing printed ⇒ nothing can have damaged the bar. Idle ⇒ silent.
/// - Still printing ⇒ wait. The bar would only be damaged again, the host is
///   busy anyway, and this is exactly when a keystroke is waiting on a frame.
/// - The command prompt owns the bar row while it is open, so never then.
///
/// The damage this repairs is narrow by construction: the pane is a row shorter
/// than the screen and runs under a scroll region, so its output cannot push the
/// bar away, and a full-screen erase (`2J`/`3J`), which ignores the region, is
/// already caught by the drain and forces a repaint.
pub(crate) fn bar_refresh_due(
    no_prompt: bool,
    output_since_bar: bool,
    since_output: Duration,
    since_bar: Duration,
) -> bool {
    no_prompt && output_since_bar && since_output >= BAR_QUIET && since_bar >= BAR_REFRESH
}

/// What the paint remembers between ticks.
pub(crate) struct Renderer {
    /// Animation clock for the per-pane loading spinner (advances ~8 frames/sec).
    /// A blank (still-booting) pane's spinner changes with this, so the tiled diff
    /// repaints just those cells; once every pane has painted, the frame no longer
    /// affects the master, so it stops driving repaints.
    anim_start: Instant,
    /// The spinner frame last painted by the passthrough startup splash, so it
    /// redraws only when the frame advances (not every tick).
    last_splash_frame: usize,
    /// When the startup splash was first drawn, which starts [`SPLASH_MIN`].
    /// Set once per run: a window opened later reuses the splash but not the
    /// hold, so it hands off as soon as its pane paints.
    splash_shown_at: Option<Instant>,
    /// The spin_frame value used by the last tiled composite. When any pane is
    /// still loading, a new spin_frame means new spinner content → must re-composite.
    last_tiled_spin: usize,
    last_bar: String,
    last_bar_paint: Instant,
    /// Whether a pane has written anything since the bar was last painted — the
    /// only case where the bar can have been clobbered. See
    /// [`bar_refresh_due`].
    output_since_bar: bool,
    /// When a pane last wrote, so the repair waits for it to go quiet.
    last_pane_output: Instant,
    /// The previous composited master, kept per-frame so tiled mode diffs. Reset
    /// to None (full repaint) on mode/layout/window changes.
    prev_master: Option<ansi::Screen>,
    /// A persistent scratch buffer written into each tiled frame. After every
    /// render it swaps with `prev_master` so neither allocation is freed: one
    /// holds the current frame (for diffing next tick) and the other is cleared
    /// and overwritten. At most one fresh allocation per full-repaint reset.
    tiled_buf: Option<ansi::Screen>,
    /// The last view identity (window / zoom / focused pane / overlay / size). A
    /// passthrough (single or zoomed) pane only gets the heavy repaint nudge
    /// (clear + resize) on a *real* transition — never on a routine `force_repaint`
    /// from bus/board/flash churn. Without this, a zoomed pane during a fleet run
    /// is cleared several times a second (every ctl request forces a repaint),
    /// which reads as paint corruption and makes text selection impossible (the
    /// clear wipes the drag). Sentinel start so the first frame counts as a change.
    last_view: (usize, bool, usize, bool, bool, bool, u16, u16),
}

impl Renderer {
    pub(crate) fn new() -> Renderer {
        Renderer {
            anim_start: Instant::now(),
            last_splash_frame: usize::MAX,
            splash_shown_at: None,
            last_tiled_spin: usize::MAX,
            last_bar: String::new(),
            last_bar_paint: Instant::now(),
            output_since_bar: false,
            last_pane_output: Instant::now(),
            prev_master: None,
            tiled_buf: None,
            last_view: (usize::MAX, false, usize::MAX, false, false, false, 0, 0),
        }
    }

    /// Drop the retained tiled frame, so the next composite is a full repaint
    /// (a mode, layout, window or size change).
    pub(crate) fn reset(&mut self) {
        self.prev_master = None;
    }

    /// Whether the startup logo has been up for [`SPLASH_MIN`]. False until it
    /// has been drawn at all: the first drain runs before the first paint, and
    /// handing off then is exactly how a fast shell hid the logo.
    pub(crate) fn splash_hold_over(&self) -> bool {
        self.splash_shown_at
            .is_some_and(|at| at.elapsed() >= SPLASH_MIN)
    }

    /// Render the active window (an overlay, the tiled compose+diff, or — in
    /// passthrough, whose bytes the drain already wrote — the splash or repaint
    /// nudge), then paint the bar. The composite and the bar are accumulated into
    /// one `frame` and emitted wrapped in synchronized output (§`SYNC_BEGIN`), so
    /// the outer terminal paints panes and bar atomically — no mid-frame tearing.
    /// An empty frame (idle tick) emits nothing, so the markers never spam.
    ///
    /// `force_repaint` is this tick's repaint request; `tiled_dirty` is whether
    /// the active window's drain changed a tiled pane.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint(
        &mut self,
        windows: &mut [Window],
        active: usize,
        views: &mut Views,
        selection: &Option<(usize, Selection)>,
        prompt: Option<&str>,
        world: &atrium::vendors::AgentState,
        board: &atrium::board::Board,
        bus: &atrium::bus::Bus,
        flash: &mut Option<(String, Instant)>,
        mut force_repaint: bool,
        tiled_dirty: bool,
        pane_output: bool,
        rows: u16,
        cols: u16,
        out: &mut impl std::io::Write,
    ) {
        let spin_frame = (self.anim_start.elapsed().as_millis() / 120) as usize;
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
            views.board,
            views.overview,
            views.log,
            rows,
            cols,
        );
        let view_changed = view_key != self.last_view;
        self.last_view = view_key;
        if views.overview {
            // The overview replaces the panes. Clamp the selection to the live
            // agent count (panes may have been reaped) and re-render on a repaint.
            let count: usize = windows.iter().map(|w| w.panes.len()).sum();
            views.overview_sel = views.overview_sel.min(count.saturating_sub(1));
            if force_repaint {
                let nodes = overview_nodes(windows, world);
                frame.extend_from_slice(
                    render_overview_panel(windows, bus, &nodes, views.overview_sel, rows, cols)
                        .as_bytes(),
                );
            }
        } else if views.board {
            // The board overlay replaces the panes. Re-render only on a repaint
            // (toggle, or a board write — every ctl request forces one), so idle
            // ticks leave the panel steady; the bar still paints below.
            if force_repaint {
                frame.extend_from_slice(
                    render_board_panel(
                        board,
                        bus,
                        rows,
                        cols,
                        views.feed_scroll,
                        views.decision_sel,
                    )
                    .as_bytes(),
                );
            }
        } else if views.log {
            // The activity log overlay: a live time-ordered merge of bus + board
            // + agent actions. Re-rendered on a repaint (toggle, scroll, or any
            // agent/ctl activity that forces one).
            if force_repaint {
                let now = agsess::sessions::now_ms();
                frame.extend_from_slice(
                    render_log_panel(
                        windows,
                        world,
                        board,
                        bus,
                        rows,
                        cols,
                        views.log_scroll,
                        now,
                    )
                    .as_bytes(),
                );
            }
        } else if windows[active].tiled() {
            let (cur_rows, cur_cols) = (rows as usize, cols as usize);
            let any_loading = windows[active].panes.iter().any(|p| !p.painted);
            let dirty =
                any_pane_dirty(tiled_dirty, any_loading, spin_frame != self.last_tiled_spin);
            let view_changed = self.prev_master.is_none();
            if needs_composite(dirty, view_changed, force_repaint) {
                // Reuse the scratch buffer's allocation when the size is unchanged;
                // reallocate only on a first frame or after a terminal resize.
                match self.tiled_buf.as_mut() {
                    Some(b) if b.rows() == cur_rows && b.cols() == cur_cols => b.clear(),
                    _ => self.tiled_buf = Some(ansi::Screen::new(cur_rows, cur_cols)),
                }
                render_tiled(
                    self.tiled_buf.as_mut().unwrap(),
                    &windows[active],
                    rows,
                    cols,
                    world,
                    spin_frame,
                );
                if let Some((win, sel)) = selection {
                    let w = &windows[active];
                    let rect = w
                        .tree
                        .rects(tiled_outer(rows, cols))
                        .into_iter()
                        .find(|(id, _)| *id == sel.pane_id);
                    if let (true, false, Some((_, rect))) = (*win == active, sel.is_empty(), rect) {
                        atrium::select::highlight(sel, self.tiled_buf.as_mut().unwrap(), &rect);
                    }
                }
                match &self.prev_master {
                    Some(prev) => {
                        frame.extend_from_slice(&prev.diff(self.tiled_buf.as_ref().unwrap()))
                    }
                    None => {
                        frame.extend_from_slice(&self.tiled_buf.as_ref().unwrap().render_full())
                    }
                };
                // Rotate buffers: tiled_buf (just written) becomes prev_master for
                // the next diff, and the old prev_master's allocation becomes the
                // next scratch. After a full-repaint reset (prev_master == None),
                // tiled_buf becomes None on this swap and is reallocated next tick.
                std::mem::swap(&mut self.prev_master, &mut self.tiled_buf);
                self.last_tiled_spin = spin_frame;
            }
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
                self.splash_shown_at.get_or_insert_with(Instant::now);
                if spin_frame != self.last_splash_frame {
                    draw_startup_splash(&mut frame, rows, cols, spin_frame);
                    self.last_splash_frame = spin_frame;
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
                repaint_focused(&mut windows[active], rows, cols, out);
            }
        }

        // 7. the bar (windows, with the active one starred)
        let infos = bar_infos(windows, active, world);
        if flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5))
        {
            *flash = None;
            force_repaint = true;
        }
        // A flash note wins; otherwise, if the bus has open `decision_needed`
        // escalations, surface them on the bar so you see them without opening the
        // `Ctrl+A b` panel — the bus's push channel to the human. A decision
        // addressed to a live teammate (`to=<role>`) is that agent's to answer, so
        // it is NOT counted here (the human still sees it in the board panel — the
        // broker keeps full visibility, just isn't urgently pinged for it).
        let decision_note = decision_note(bus, windows);
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
        let painted = if let Some(line) = prompt {
            format!(
                "\x1b[{};1H\x1b[2K\x1b[1;38;2;70;235;255m:\x1b[0m{line}\x1b[?25h",
                rows
            )
        } else {
            bar_paint(&infos, rows, cols as usize, note)
        };
        let mut bar_appended = false;
        if pane_output {
            self.output_since_bar = true;
            self.last_pane_output = Instant::now();
        }
        if force_repaint
            || splash_drawn
            || painted != self.last_bar
            || bar_refresh_due(
                prompt.is_none(),
                self.output_since_bar,
                self.last_pane_output.elapsed(),
                self.last_bar_paint.elapsed(),
            )
        {
            frame.extend_from_slice(painted.as_bytes());
            self.last_bar = painted;
            self.last_bar_paint = Instant::now();
            self.output_since_bar = false;
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
        if bar_appended && !views.board && !views.overview && !views.log && prompt.is_none() {
            let cur = if windows[active].tiled() {
                self.prev_master.as_ref().map(|m| m.cursor())
            } else {
                let fp = windows[active].tree.focus();
                windows[active].pane(fp).map(|p| p.term.screen().cursor())
            };
            if let Some(ansi::Cursor { row, col }) = cur {
                frame.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // (no_prompt, output_since_bar, ms since output, ms since the bar, want, why).
    #[test]
    fn the_bar_is_repaired_only_after_a_pane_printed_and_went_quiet() {
        let cases: &[(bool, bool, u64, u64, bool, &str)] = &[
            (true, true, 150, 500, true, "printed, then quiet: repair it"),
            (true, true, 900, 900, true, "long quiet: repair it"),
            (
                true,
                true,
                20,
                900,
                false,
                "still printing: the bar would only be damaged again, and a \
                 keystroke is waiting on the console host's frame",
            ),
            (
                true,
                true,
                500,
                499,
                false,
                "repaired recently enough; don't write twice",
            ),
            (
                true,
                false,
                5_000,
                5_000,
                false,
                "idle: nothing printed, so nothing can have damaged the bar — \
                 and an idle write costs the next keystroke ~16 ms on Windows",
            ),
            (
                false,
                true,
                5_000,
                5_000,
                false,
                "the command prompt owns the bar row; a repair would reflash it",
            ),
        ];
        for &(no_prompt, output, out_ms, bar_ms, want, why) in cases {
            assert_eq!(
                bar_refresh_due(
                    no_prompt,
                    output,
                    Duration::from_millis(out_ms),
                    Duration::from_millis(bar_ms)
                ),
                want,
                "{why}"
            );
        }
    }
}
