//! Mouse actions while atrium's own capture is on (`Ctrl+A m`): click to focus,
//! drag to select within one tile, wheel to scroll the hovered tile.
//!
//! Carved out of `run()` (r10 audit B11, step 4a). The click and wheel arms each
//! carried their own copy of the tile hit-test, and the wheel arm re-derived
//! [`screen_to_pane_local`] inline; both now share [`tile_at`] and that helper.
//! The handlers are otherwise unchanged, and their effect on the loop is returned
//! as a [`KeyOutcome`].

use crate::overlay_keys::KeyOutcome;
use crate::*;
use atrium::input::encode_sgr_wheel;
use atrium::layout::Rect;
use atrium::select::Selection;

/// The tile under a 1-based pointer cell `(col, row)`: its pane id and rect,
/// plus the pointer as a 0-based master cell `(mx, my)`. `None` off every tile
/// (the bar row, or outside `outer`).
pub(crate) fn tile_at(
    tree: &Tree,
    outer: Rect,
    col: u16,
    row: u16,
) -> Option<(usize, Rect, usize, usize)> {
    let mx = col.saturating_sub(1) as usize;
    let my = row.saturating_sub(1) as usize;
    tree.rects(outer)
        .into_iter()
        .find(|(_, r)| my >= r.row && my < r.row + r.rows && mx >= r.col && mx < r.col + r.cols)
        .map(|(id, r)| (id, r, mx, my))
}

/// Apply a mouse `action` to the active window; any other action is a no-op.
/// `selection` is the drag in progress, with the window it started in.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_mouse(
    action: &Action,
    windows: &mut [Window],
    active: usize,
    selection: &mut Option<(usize, Selection)>,
    rows: u16,
    cols: u16,
    out: &mut impl std::io::Write,
    flash: &mut Option<(String, Instant)>,
) -> KeyOutcome {
    let mut outcome = KeyOutcome::default();
    match *action {
        Action::MouseClick { col, row } => {
            let w = &mut windows[active];
            if w.tiled() {
                if let Some((id, rect, mx, my)) =
                    tile_at(&w.tree, tiled_outer(rows, cols), col, row)
                {
                    *selection = Some((active, Selection::start(id, mx, my, &rect)));
                    if let Some(p) = w.pane_mut(id) {
                        if p.mouse_wanted {
                            if let Some((cx, cy)) = screen_to_pane_local(mx, my, &rect) {
                                let _ = p.pty.write(encode_sgr_click(cx, cy).as_bytes());
                            }
                        }
                    }
                    if w.tree.focus_pane(id) {
                        outcome.reset_frame = true;
                        outcome.repaint = true;
                    }
                }
            } else if let Some(p) = w.focused_mut() {
                if p.mouse_wanted {
                    let _ = p
                        .pty
                        .write(encode_sgr_click(col as usize, row as usize).as_bytes());
                }
            }
        }
        Action::MouseDrag { col, row } => {
            // Extend a tile selection; the head is clamped into the tile it
            // started in, however far the pointer strays.
            if let Some((win, sel)) = selection.as_mut() {
                let w = &windows[active];
                if *win == active && w.tiled() {
                    let rect = w
                        .tree
                        .rects(tiled_outer(rows, cols))
                        .into_iter()
                        .find(|(id, _)| *id == sel.pane_id);
                    if let Some((_, rect)) = rect {
                        let prev = sel.head;
                        let mx = col.saturating_sub(1) as usize;
                        let my = row.saturating_sub(1) as usize;
                        sel.drag(mx, my, &rect);
                        if sel.head != prev {
                            outcome.repaint = true;
                        }
                    }
                }
            }
        }
        Action::MouseRelease { .. } => {
            // End a tile selection: copy its text unless it was a click.
            if let Some((win, sel)) = selection.take() {
                if win == active && !sel.is_empty() {
                    if let Some(p) = windows[active].pane(sel.pane_id) {
                        let text = atrium::select::text(&sel, p.term.screen());
                        let n = text.chars().count();
                        atrium::select::copy(&text, out);
                        *flash = Some((format!("copied {n} chars"), Instant::now()));
                    }
                }
                outcome.repaint = true;
            }
        }
        Action::MouseScroll { up, col, row } => {
            // Route a wheel notch to the tile under the cursor (not necessarily
            // the focused one) so you scroll whatever you're hovering. Forward it
            // to that pane's app only if it wants the mouse, so a bare shell never
            // gets stray bytes.
            let w = &mut windows[active];
            if w.tiled() {
                if let Some((id, rect, mx, my)) =
                    tile_at(&w.tree, tiled_outer(rows, cols), col, row)
                {
                    if let Some(p) = w.pane_mut(id) {
                        if p.mouse_wanted {
                            // Master cell → the pane's inner (bordered) 1-based
                            // coords: content is inset one cell.
                            if let Some((cx, cy)) = screen_to_pane_local(mx, my, &rect) {
                                let _ = p.pty.write(encode_sgr_wheel(up, cx, cy).as_bytes());
                            }
                        }
                    }
                }
            } else if let Some(p) = w.focused_mut() {
                // Passthrough / zoom: the sole pane fills the area above the bar;
                // forward with the original coordinates.
                if p.mouse_wanted {
                    let _ = p
                        .pty
                        .write(encode_sgr_wheel(up, col as usize, row as usize).as_bytes());
                }
            }
        }
        _ => {}
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outer() -> Rect {
        Rect {
            row: 0,
            col: 0,
            rows: 23,
            cols: 80,
        }
    }

    #[test]
    fn tile_at_finds_the_tile_under_the_pointer_on_either_side_of_a_seam() {
        let mut tree = Tree::new(0);
        tree.split(layout::Dir::Vertical, 1);
        let rects = tree.rects(outer());
        let right = rects.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert!(right.col > 0, "a vertical split puts pane 1 to the right");

        // 1-based pointer cells vs 0-based master cells: the right tile's first
        // column is 1-based `right.col + 1`; the cell before it is the left tile's.
        let first_right = right.col as u16 + 1;
        let (id, rect, mx, my) = tile_at(&tree, outer(), first_right, 1).unwrap();
        assert_eq!((id, rect, mx, my), (1, right, right.col, 0));
        assert_eq!(tile_at(&tree, outer(), first_right - 1, 1).unwrap().0, 0);
        assert_eq!(tile_at(&tree, outer(), 80, 23).unwrap().0, 1);
    }

    #[test]
    fn tile_at_misses_the_bar_row_and_clamps_a_zero_coordinate() {
        let tree = Tree::new(7);
        // Row 24 (1-based) is the bar: outside the 23-row tiled area.
        assert_eq!(tile_at(&tree, outer(), 1, 24), None);
        assert_eq!(tile_at(&tree, outer(), 81, 1), None);
        // A malformed 0 coordinate saturates onto the first cell, not a wrap.
        assert_eq!(
            tile_at(&tree, outer(), 0, 0).map(|t| (t.0, t.2, t.3)),
            Some((7, 0, 0))
        );
    }
}
