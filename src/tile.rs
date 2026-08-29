//! The tiled-mode compositor: blit every pane's emulated [`ansi::Screen`] into
//! one master screen at its rect offset, draw the dividers that sit in the
//! gutters the layout reserved, highlight the focused pane's edges, and report
//! where the real cursor should be parked.
//!
//! This is pure grid math — screens in, a master screen out — so it is
//! unit-testable without a terminal or a pty. The run loop diffs the master
//! against the previous frame ([`ansi::Screen::diff`]) and writes only the
//! changed bytes, the same discipline the status bar already uses.
//!
//! Fidelity note: tiling is the *emulated* path. A pane that needs pixel-exact
//! rendering (a TUI mid-redraw, wide/CJK glyphs, sixel) takes the escape hatch
//! — `Ctrl+A z` zoom drops it back to raw passthrough. See the 0.2 design doc,
//! §1 and the vterm README's fidelity boundaries.

use crate::layout::Rect;
use ansi::{Cell, Color, Screen, Style};

/// The character painted in divider gutters between panes.
const DIVIDER: char = '│';
const DIVIDER_H: char = '─';

/// One pane to composite: its emulated screen, its rect in master coords, and
/// whether it holds focus.
pub struct PaneView<'a> {
    pub screen: &'a Screen,
    pub rect: Rect,
    pub focused: bool,
}

/// Compose `panes` into a fresh master [`Screen`] of `rows x cols`. Cells
/// outside every pane rect are divider gutters (drawn where a divider column /
/// row sits between two panes, blank elsewhere). The focused pane's gutter
/// edges are drawn reversed to mark focus. `bar_row` (0-based) is left blank —
/// the caller paints the existing status bar there after diffing, exactly as in
/// passthrough mode.
///
/// Returns the master screen with its `cursor` set to the focused pane's
/// cursor, translated into master coordinates (clamped into the pane rect).
pub fn compose(rows: usize, cols: usize, panes: &[PaneView]) -> Screen {
    let mut master = Screen::new(rows, cols);

    // 1. Blit each pane's content at its offset.
    for p in panes {
        blit(&mut master, p.screen, p.rect);
    }

    // 2. Draw dividers in the gutters. A gutter cell is any master cell that is
    //    directly between two pane rects — we detect it structurally by drawing
    //    a divider on the column just right of every pane that has a right
    //    neighbor, and the row just below every pane that has a bottom
    //    neighbor. Focused-pane edges are reversed to highlight focus.
    for p in panes {
        draw_edges(&mut master, p, panes, rows, cols);
    }

    // 3. Park the cursor at the focused pane's cursor in master coords.
    if let Some(f) = panes.iter().find(|p| p.focused) {
        let (cr, cc) = f.screen.cursor;
        let r = (f.rect.row + cr).min(f.rect.row + f.rect.rows.saturating_sub(1));
        let c = (f.rect.col + cc).min(f.rect.col + f.rect.cols.saturating_sub(1));
        master.cursor = (r.min(rows.saturating_sub(1)), c.min(cols.saturating_sub(1)));
    }

    master
}

/// Copy a pane's screen into the master at `rect`, truncating anything past the
/// rect's bounds (a pane should already be sized to its rect, but a resize race
/// could leave it momentarily larger — clip rather than overwrite neighbors).
fn blit(master: &mut Screen, src: &Screen, rect: Rect) {
    let rows = rect.rows.min(src.rows());
    let cols = rect.cols.min(src.cols());
    for r in 0..rows {
        for c in 0..cols {
            master.set(rect.row + r, rect.col + c, src.cell(r, c));
        }
    }
}

/// Draw the divider gutters on a pane's right and bottom edges when a neighbor
/// abuts across the reserved gap. The focused pane draws its dividers reversed
/// so the eye finds the active tile.
fn draw_edges(master: &mut Screen, p: &PaneView, all: &[PaneView], rows: usize, cols: usize) {
    let style = if p.focused {
        Style {
            reverse: true,
            fg: Color::Default,
            ..Style::default()
        }
    } else {
        Style::default()
    };

    // Right divider column: at col = rect.col + rect.cols, spanning the pane's
    // rows, iff there is any pane whose left edge is one past that column.
    let right_col = p.rect.col + p.rect.cols;
    if right_col < cols && has_neighbor_right(p, all) {
        for r in p.rect.row..(p.rect.row + p.rect.rows).min(rows) {
            master.set(r, right_col, Cell { ch: DIVIDER, style });
        }
    }
    // Bottom divider row.
    let bottom_row = p.rect.row + p.rect.rows;
    if bottom_row < rows && has_neighbor_below(p, all) {
        for c in p.rect.col..(p.rect.col + p.rect.cols).min(cols) {
            master.set(
                bottom_row,
                c,
                Cell {
                    ch: DIVIDER_H,
                    style,
                },
            );
        }
    }
}

/// True if some other pane's left edge sits exactly one column right of `p`
/// (i.e. across `p`'s reserved right gutter) and their rows overlap.
fn has_neighbor_right(p: &PaneView, all: &[PaneView]) -> bool {
    let gutter = p.rect.col + p.rect.cols;
    all.iter().any(|o| {
        !std::ptr::eq(o.screen, p.screen)
            && o.rect.col == gutter + 1
            && rows_overlap(p.rect, o.rect)
    })
}

/// True if some other pane's top edge sits one row below `p`'s bottom gutter and
/// their columns overlap.
fn has_neighbor_below(p: &PaneView, all: &[PaneView]) -> bool {
    let gutter = p.rect.row + p.rect.rows;
    all.iter().any(|o| {
        !std::ptr::eq(o.screen, p.screen)
            && o.rect.row == gutter + 1
            && cols_overlap(p.rect, o.rect)
    })
}

fn rows_overlap(a: Rect, b: Rect) -> bool {
    a.row < b.row + b.rows && b.row < a.row + a.rows
}

fn cols_overlap(a: Rect, b: Rect) -> bool {
    a.col < b.col + b.cols && b.col < a.col + a.cols
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(rows: usize, cols: usize, ch: char) -> Screen {
        let mut s = Screen::new(rows, cols);
        for r in 0..rows {
            for c in 0..cols {
                s.set(
                    r,
                    c,
                    Cell {
                        ch,
                        style: Style::default(),
                    },
                );
            }
        }
        s
    }

    #[test]
    fn two_panes_composite_at_their_offsets_with_a_divider_between() {
        // Left pane 'L' at cols 0..3, right pane 'R' at cols 4..7, divider col 3.
        let left = filled(4, 3, 'L');
        let right = filled(4, 3, 'R');
        let panes = vec![
            PaneView {
                screen: &left,
                rect: Rect {
                    row: 0,
                    col: 0,
                    rows: 4,
                    cols: 3,
                },
                focused: true,
            },
            PaneView {
                screen: &right,
                rect: Rect {
                    row: 0,
                    col: 4,
                    rows: 4,
                    cols: 3,
                },
                focused: false,
            },
        ];
        let m = compose(4, 7, &panes);
        // Left content.
        assert_eq!(m.cell(0, 0).ch, 'L');
        assert_eq!(m.cell(3, 2).ch, 'L');
        // Divider column.
        assert_eq!(m.cell(0, 3).ch, '│');
        assert_eq!(m.cell(3, 3).ch, '│');
        // Right content.
        assert_eq!(m.cell(0, 4).ch, 'R');
        assert_eq!(m.cell(3, 6).ch, 'R');
    }

    #[test]
    fn focused_pane_divider_is_reversed() {
        let left = filled(2, 2, 'L');
        let right = filled(2, 2, 'R');
        let panes = vec![
            PaneView {
                screen: &left,
                rect: Rect {
                    row: 0,
                    col: 0,
                    rows: 2,
                    cols: 2,
                },
                focused: true,
            },
            PaneView {
                screen: &right,
                rect: Rect {
                    row: 0,
                    col: 3,
                    rows: 2,
                    cols: 2,
                },
                focused: false,
            },
        ];
        let m = compose(2, 5, &panes);
        // The focused (left) pane owns the divider at col 2, drawn reversed.
        assert!(
            m.cell(0, 2).style.reverse,
            "focused divider should be reversed"
        );
    }

    #[test]
    fn cursor_parks_at_the_focused_pane_in_master_coords() {
        let mut left = filled(4, 4, 'L');
        left.cursor = (1, 2);
        let right = filled(4, 4, 'R');
        let panes = vec![
            PaneView {
                screen: &left,
                rect: Rect {
                    row: 0,
                    col: 0,
                    rows: 4,
                    cols: 4,
                },
                focused: false,
            },
            PaneView {
                screen: &right,
                rect: Rect {
                    row: 0,
                    col: 5,
                    rows: 4,
                    cols: 4,
                },
                focused: true,
            },
        ];
        // Right pane cursor at its origin -> master (0, 5).
        let m = compose(4, 9, &panes);
        assert_eq!(m.cursor, (0, 5));
    }

    #[test]
    fn horizontal_divider_row_drawn_between_stacked_panes() {
        let top = filled(2, 4, 'T');
        let bottom = filled(2, 4, 'B');
        let panes = vec![
            PaneView {
                screen: &top,
                rect: Rect {
                    row: 0,
                    col: 0,
                    rows: 2,
                    cols: 4,
                },
                focused: false,
            },
            PaneView {
                screen: &bottom,
                rect: Rect {
                    row: 3,
                    col: 0,
                    rows: 2,
                    cols: 4,
                },
                focused: false,
            },
        ];
        let m = compose(5, 4, &panes);
        assert_eq!(m.cell(0, 0).ch, 'T');
        assert_eq!(m.cell(2, 0).ch, '─'); // divider row
        assert_eq!(m.cell(3, 0).ch, 'B');
    }
}
