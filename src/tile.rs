//! The tiled-mode compositor: draw a full box border around every pane, embed
//! each pane's index + title in its top edge, blit its emulated
//! [`ansi::Screen`] into the box's inner area, tint the border by liveness, and
//! report where the real cursor should be parked.
//!
//! This is pure grid math — screens in, a master screen out — so it is
//! unit-testable without a terminal or a pty. The run loop diffs the master
//! against the previous frame ([`ansi::Screen::diff`]) and writes only the
//! changed bytes, the same discipline the status bar already uses.
//!
//! Panes tile edge-to-edge (the layout reserves no gutter), so adjacent boxes
//! simply abut: `┐┌` at a top seam, `││` down a shared column. That is
//! intended — every pane owns and draws its own four-sided border.
//!
//! Fidelity note: tiling is the *emulated* path. A pane that needs pixel-exact
//! rendering (a TUI mid-redraw, wide/CJK glyphs, sixel) takes the escape hatch
//! — `Ctrl+A z` zoom drops it back to raw passthrough. See the 0.2 design doc,
//! §1 and the vterm README's fidelity boundaries.

use crate::layout::Rect;
use ansi::{Cell, Color, Screen, Style};

// The box-drawing characters that frame each pane.
const TOP_LEFT: char = '┌';
const TOP_RIGHT: char = '┐';
const BOTTOM_LEFT: char = '└';
const BOTTOM_RIGHT: char = '┘';
const HORIZONTAL: char = '─';
const VERTICAL: char = '│';

/// A pane's liveness, computed by the caller and used to tint its border and
/// title. Checked in priority order: focus wins, then a dead child, then recent
/// background activity, then plain idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    /// The focused pane — bright cyan, bold.
    Focused,
    /// The child process has exited — red.
    Exited,
    /// Output arrived recently while unfocused — normal (default) style.
    Active,
    /// Quiet and unfocused — grey.
    Idle,
}

impl PaneState {
    /// The border/title style for this state (my 0.2.1 color spec).
    fn style(self) -> Style {
        match self {
            PaneState::Focused => Style {
                bold: true,
                fg: Color::Indexed(14), // bright cyan
                ..Style::default()
            },
            PaneState::Exited => Style {
                fg: Color::Indexed(1), // red
                ..Style::default()
            },
            PaneState::Active => Style::default(),
            PaneState::Idle => Style {
                fg: Color::Indexed(8), // grey (no `dim` attribute assumed)
                ..Style::default()
            },
        }
    }
}

/// One pane to composite: its emulated screen, its rect in master coords, its
/// 1-based display index and title (both embedded in the top border), and its
/// liveness [`PaneState`] (which tints the border).
pub struct PaneView<'a> {
    pub screen: &'a Screen,
    pub rect: Rect,
    pub index: usize,
    pub title: &'a str,
    pub state: PaneState,
}

impl PaneView<'_> {
    fn focused(&self) -> bool {
        self.state == PaneState::Focused
    }
}

/// Compose `panes` into a fresh master [`Screen`] of `rows x cols`. Each pane is
/// drawn as a full box border (its index + title in the top edge) with its
/// screen blitted into the inner area, inset one cell on every side. Border and
/// title are tinted by the pane's [`PaneState`]. `bar_row` (0-based) is left
/// blank — the caller paints the existing status bar there after diffing,
/// exactly as in passthrough mode.
///
/// Returns the master screen with its `cursor` set to the focused pane's
/// cursor, translated into the focused pane's *inner* (bordered) coordinates.
pub fn compose(rows: usize, cols: usize, panes: &[PaneView]) -> Screen {
    let mut master = Screen::new(rows, cols);

    for p in panes {
        blit_inner(&mut master, p, rows, cols);
        draw_border(&mut master, p, rows, cols);
    }

    // Park the cursor at the focused pane's cursor, translated into the inner
    // area (offset by the one-cell border) and clamped inside that inner box.
    if let Some(f) = panes.iter().find(|p| p.focused()) {
        let (cr, cc) = f.screen.cursor;
        // Inner area origin and size (border eats one cell on each side).
        let inner_rows = f.rect.rows.saturating_sub(2);
        let inner_cols = f.rect.cols.saturating_sub(2);
        let r = f.rect.row + 1 + cr.min(inner_rows.saturating_sub(1));
        let c = f.rect.col + 1 + cc.min(inner_cols.saturating_sub(1));
        master.cursor = (r.min(rows.saturating_sub(1)), c.min(cols.saturating_sub(1)));
    }

    master
}

/// Blit a pane's screen into its inner area, inset one cell from the rect on
/// every side, truncating anything past the inner bounds (the pane should be
/// sized to the inner area, but a resize race could leave it momentarily larger
/// — clip rather than overwrite the border or neighbors).
fn blit_inner(master: &mut Screen, p: &PaneView, rows: usize, cols: usize) {
    let inner_row = p.rect.row + 1;
    let inner_col = p.rect.col + 1;
    let inner_rows = p.rect.rows.saturating_sub(2).min(p.screen.rows());
    let inner_cols = p.rect.cols.saturating_sub(2).min(p.screen.cols());
    for r in 0..inner_rows {
        for c in 0..inner_cols {
            let (mr, mc) = (inner_row + r, inner_col + c);
            if mr < rows && mc < cols {
                master.set(mr, mc, p.screen.cell(r, c));
            }
        }
    }
}

/// Draw the pane's four-sided box border, embedding `index:title` in the top
/// edge (truncated by the border if the pane is narrow). Border and title share
/// the pane's state style. A rect narrower/shorter than 2 cells still draws what
/// fits — `set` clips out-of-bounds writes.
fn draw_border(master: &mut Screen, p: &PaneView, rows: usize, cols: usize) {
    let style = p.state.style();
    let (r0, c0) = (p.rect.row, p.rect.col);
    let (rn, cn) = (p.rect.rows, p.rect.cols);
    if rn == 0 || cn == 0 {
        return;
    }
    let last_row = r0 + rn - 1;
    let last_col = c0 + cn - 1;

    let put = |m: &mut Screen, r: usize, c: usize, ch: char| {
        if r < rows && c < cols {
            m.set(r, c, Cell { ch, style });
        }
    };

    // Top and bottom horizontal runs.
    for c in c0..=last_col {
        put(master, r0, c, HORIZONTAL);
        put(master, last_row, c, HORIZONTAL);
    }
    // Left and right vertical runs.
    for r in r0..=last_row {
        put(master, r, c0, VERTICAL);
        put(master, r, last_col, VERTICAL);
    }
    // Corners (overwrite the runs).
    put(master, r0, c0, TOP_LEFT);
    put(master, r0, last_col, TOP_RIGHT);
    put(master, last_row, c0, BOTTOM_LEFT);
    put(master, last_row, last_col, BOTTOM_RIGHT);

    // Title in the top edge: "┌ 2:claude ──…──┐". The label sits one cell in
    // from the top-left corner, framed by a space each side, and is truncated
    // to leave room for the trailing corner.
    if cn >= 5 {
        let label = format!(" {}:{} ", p.index, p.title);
        // Available label columns: everything between the two corners.
        let avail = cn.saturating_sub(2);
        let start = c0 + 1;
        for (i, ch) in label.chars().take(avail).enumerate() {
            put(master, r0, start + i, ch);
        }
    }
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

    fn view<'a>(
        screen: &'a Screen,
        rect: Rect,
        index: usize,
        title: &'a str,
        state: PaneState,
    ) -> PaneView<'a> {
        PaneView {
            screen,
            rect,
            index,
            title,
            state,
        }
    }

    #[test]
    fn a_boxed_pane_draws_corners_and_its_title_in_the_top_edge() {
        // A single 5x12 pane titled "claude" as index 2.
        let inner = filled(3, 10, 'X');
        let panes = vec![view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 12,
            },
            2,
            "claude",
            PaneState::Idle,
        )];
        let m = compose(5, 12, &panes);
        // Corners.
        assert_eq!(m.cell(0, 0).ch, '┌');
        assert_eq!(m.cell(0, 11).ch, '┐');
        assert_eq!(m.cell(4, 0).ch, '└');
        assert_eq!(m.cell(4, 11).ch, '┘');
        // Title in the top edge, one cell in from the corner: " 2:claude ".
        let top: String = (1..11).map(|c| m.cell(0, c).ch).collect();
        assert!(top.starts_with(" 2:claude "), "top edge was {top:?}");
    }

    #[test]
    fn content_blits_at_the_inset_offset_not_over_the_border() {
        let inner = filled(3, 3, 'X');
        let panes = vec![view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 5,
            },
            1,
            "sh",
            PaneState::Idle,
        )];
        let m = compose(5, 5, &panes);
        // Border, not 'X', at the rect's top-left corner.
        assert_eq!(m.cell(0, 0).ch, '┌');
        assert_ne!(m.cell(0, 0).ch, 'X');
        // Content 'X' at the inset origin (rect.row+1, rect.col+1).
        assert_eq!(m.cell(1, 1).ch, 'X');
        assert_eq!(m.cell(3, 3).ch, 'X');
    }

    #[test]
    fn focused_pane_border_is_bold_bright_cyan() {
        let inner = filled(3, 3, ' ');
        let panes = vec![view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 5,
            },
            1,
            "sh",
            PaneState::Focused,
        )];
        let m = compose(5, 5, &panes);
        let corner = m.cell(0, 0).style;
        assert!(corner.bold, "focused border should be bold");
        assert_eq!(corner.fg, Color::Indexed(14), "focused border bright cyan");
        // The title shares the border style.
        assert_eq!(m.cell(0, 1).style.fg, Color::Indexed(14));
    }

    #[test]
    fn exited_pane_border_is_red() {
        let inner = filled(3, 3, ' ');
        let panes = vec![view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 5,
            },
            1,
            "sh",
            PaneState::Exited,
        )];
        let m = compose(5, 5, &panes);
        assert_eq!(
            m.cell(0, 0).style.fg,
            Color::Indexed(1),
            "exited border red"
        );
    }

    #[test]
    fn idle_pane_border_is_grey() {
        let inner = filled(3, 3, ' ');
        let panes = vec![view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 5,
            },
            1,
            "sh",
            PaneState::Idle,
        )];
        let m = compose(5, 5, &panes);
        assert_eq!(m.cell(0, 0).style.fg, Color::Indexed(8), "idle border grey");
    }

    #[test]
    fn cursor_parks_at_the_focused_pane_inner_coords() {
        let mut inner = filled(4, 4, ' ');
        inner.cursor = (1, 2);
        let other = filled(4, 4, ' ');
        let panes = vec![
            view(
                &other,
                Rect {
                    row: 0,
                    col: 0,
                    rows: 6,
                    cols: 6,
                },
                1,
                "a",
                PaneState::Idle,
            ),
            view(
                &inner,
                Rect {
                    row: 0,
                    col: 6,
                    rows: 6,
                    cols: 6,
                },
                2,
                "b",
                PaneState::Focused,
            ),
        ];
        let m = compose(6, 12, &panes);
        // Focused rect origin (0,6), inner origin (1,7), cursor (1,2) -> (2,9).
        assert_eq!(m.cursor, (2, 9));
    }

    #[test]
    fn two_by_two_composes_four_boxed_panes_without_overlap() {
        // Four 5x6 boxes filling a 10x12 grid, one per quadrant.
        let a = filled(3, 4, 'A');
        let b = filled(3, 4, 'B');
        let c = filled(3, 4, 'C');
        let d = filled(3, 4, 'D');
        let rect = |row, col| Rect {
            row,
            col,
            rows: 5,
            cols: 6,
        };
        let panes = vec![
            view(&a, rect(0, 0), 1, "a", PaneState::Idle),
            view(&b, rect(0, 6), 2, "b", PaneState::Idle),
            view(&c, rect(5, 0), 3, "c", PaneState::Idle),
            view(&d, rect(5, 6), 4, "d", PaneState::Focused),
        ];
        let m = compose(10, 12, &panes);
        // Each quadrant shows its own content at its inset origin.
        assert_eq!(m.cell(1, 1).ch, 'A');
        assert_eq!(m.cell(1, 7).ch, 'B');
        assert_eq!(m.cell(6, 1).ch, 'C');
        assert_eq!(m.cell(6, 7).ch, 'D');
        // Each quadrant has its own top-left corner (boxes abut, no overlap).
        assert_eq!(m.cell(0, 0).ch, '┌');
        assert_eq!(m.cell(0, 6).ch, '┌');
        assert_eq!(m.cell(5, 0).ch, '┌');
        assert_eq!(m.cell(5, 6).ch, '┌');
    }

    #[test]
    fn narrow_pane_truncates_the_title_with_the_border() {
        // A 5-col box has only 3 inner-edge cells for the title.
        let inner = filled(3, 3, ' ');
        let panes = vec![view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 5,
            },
            9,
            "verylongtitle",
            PaneState::Idle,
        )];
        let m = compose(5, 5, &panes);
        // Corners intact; title clipped to the 3 cells between them.
        assert_eq!(m.cell(0, 0).ch, '┌');
        assert_eq!(m.cell(0, 4).ch, '┐');
        let mid: String = (1..4).map(|c| m.cell(0, c).ch).collect();
        assert_eq!(mid, " 9:"); // " 9:verylongtitle " truncated to 3 cells
    }
}
