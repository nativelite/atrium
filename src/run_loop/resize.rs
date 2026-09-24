//! Phase 5 of a tick: follow the terminal's size.

use super::RunState;
use crate::*;

impl RunState<'_> {
    pub(super) fn track_resize(&mut self) {
        // 5. resize propagation
        if self.last_size_check.elapsed() >= Duration::from_millis(150) {
            self.last_size_check = Instant::now();
            if let Some((r, c)) = self.size_watch.latest() {
                if adopt_size((r, c), (self.rows, self.cols)) {
                    self.rows = r;
                    self.cols = c;
                    // Reset the scroll region for the new height, then wipe the
                    // whole screen. The wipe is what fixes the tiled-resize
                    // garble: when the terminal shrinks, cells from the previous,
                    // larger frame lie outside the new master and would otherwise
                    // linger; and a tiled recompose diffs against the retained
                    // frame, so without this clear + `renderer.reset()` the first frame
                    // at the new size can paint over stale geometry. This mirrors
                    // the clear `switch_window`/`repaint_focused` already emit.
                    let _ = write!(self.out, "\x1b[1;{}r\x1b[2J\x1b[H", self.rows - 1);
                    // Resize every window's panes (pty + emulator) to their new
                    // inner rects at the new size — tiled windows recompute the
                    // grid, passthrough windows refit the sole/focused pane.
                    // `resize_window` is the single helper both initial layout
                    // and resize share, so their inner-rect math cannot drift.
                    for w in self.windows.iter_mut() {
                        resize_window(w, self.rows, self.cols);
                    }
                    // Force a full redraw of the focused passthrough pane at the new
                    // size with a *second* resize (to size-1, then size). A single
                    // resize can be missed by an app mid-boot or mid-reconnect —
                    // claude keeps drawing at the stale size, ending up in a corner
                    // of the larger terminal. The extra resize guarantees a fresh
                    // SIGWINCH/redraw ONLY because the two sizes differ, which holds
                    // because this branch is gated on `r >= 3` (`adopt_size`; so `ar >= 2` and
                    // `ar - 1 != ar`): ConPTY emits no event for a same-size resize,
                    // so lowering that floor silently voids the guarantee.
                    // (The `force_repaint` path below only nudges
                    // once the pane has painted, so it misses exactly the boot
                    // window the founder hit; this covers it.)
                    if !self.windows[self.active].tiled() {
                        let ar = self.rows.saturating_sub(1).max(1);
                        let focus = self.windows[self.active].tree.focus();
                        if let Some(p) = self.windows[self.active].pane_mut(focus) {
                            // The first resize is only the redraw nudge and may fail
                            // harmlessly. The emulator follows the SECOND, so the pty
                            // and emulator are never left at different sizes; a
                            // failure is surfaced instead of desyncing silently
                            // (r10 audit B8).
                            let _ = p.pty.resize(ar.saturating_sub(1).max(1), self.cols);
                            match p.pty.resize(ar, self.cols) {
                                Ok(()) => p.term.resize(ar as usize, self.cols as usize),
                                Err(e) => {
                                    self.flash =
                                        Some((format!("pane resize failed: {e}"), Instant::now()))
                                }
                            }
                        }
                    }
                    // Force a full recompose at the new size: tiled repaints via
                    // `render_full` (which re-clears + paints every cell),
                    // passthrough nudges the focused pty to redraw.
                    self.renderer.reset();
                    self.force_repaint = true;
                }
            }
        }
    }
}

/// Whether a size the terminal reported, `(r, c)`, replaces the current
/// `(rows, cols)`. Pure.
///
/// Windows Terminal's ConPTY reports a few rows FEWER once atrium is
/// in the alternate screen buffer (a persistent reservation), and
/// adopting it makes atrium redraw short — leaving a strip of stale
/// content below the bar (the initial full-screen draw was correct).
/// Treat a small height-only shrink as that reservation and keep the
/// current size; real resizes (width change, growth, or a large
/// shrink) still apply.
///
/// The `r >= 3` floor is what makes the passthrough double resize in
/// `track_resize` work: see the comment there.
pub(super) fn adopt_size((r, c): (u16, u16), (rows, cols): (u16, u16)) -> bool {
    let altscreen_reserve = c == cols && r < rows && rows - r <= ALT_SCREEN_RESERVE_ROWS;
    (r, c) != (rows, cols) && r >= 3 && !altscreen_reserve
}
