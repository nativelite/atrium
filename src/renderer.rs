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
    /// The spin_frame value used by the last tiled composite. When any pane is
    /// still loading, a new spin_frame means new spinner content → must re-composite.
    last_tiled_spin: usize,
    last_bar: String,
    last_bar_paint: Instant,
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
            last_tiled_spin: usize::MAX,
            last_bar: String::new(),
            last_bar_paint: Instant::now(),
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
        world: &atrium::vendors::VendorWorlds,
        board: &atrium::board::Board,
        bus: &atrium::bus::Bus,
        flash: &mut Option<(String, Instant)>,
        mut force_repaint: bool,
        tiled_dirty: bool,
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
        if force_repaint
            || splash_drawn
            || painted != self.last_bar
            // The 500ms periodic refresh keeps the status bar's activity markers
            // live, but while the command prompt is open it would reflash the
            // prompt row twice a second — skip it there (the prompt repaints on
            // keystroke via `painted != last_bar`).
            || (prompt.is_none() && self.last_bar_paint.elapsed() >= Duration::from_millis(500))
        {
            frame.extend_from_slice(painted.as_bytes());
            self.last_bar = painted;
            self.last_bar_paint = Instant::now();
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
                self.prev_master.as_ref().map(|m| m.cursor)
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
    }
}
