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
    /// The border/title style for this state — the shared amux theme, so a
    /// tile border and a bar entry in the same color mean the same thing. Vivid
    /// truecolor for high contrast (focused must jump out from idle at a glance).
    fn style(self) -> Style {
        match self {
            PaneState::Focused => Style {
                bold: true,
                fg: crate::theme::FOCUSED, // bright cyan
                ..Style::default()
            },
            PaneState::Exited => Style {
                fg: crate::theme::EXITED, // red
                ..Style::default()
            },
            PaneState::Active => Style::default(),
            PaneState::Idle => Style {
                fg: crate::theme::IDLE, // dim grey (clearly recedes vs focused)
                ..Style::default()
            },
        }
    }
}

/// A bound agent's attention status, attached to a pane as a second, orthogonal
/// axis to [`PaneState`] (§4.1 of the 0.3 design). Populated only for unfocused,
/// bound agent panes; carries **status only** — never any transcript text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentMark {
    pub status: agsess::Status,
}

/// One pane to composite: its emulated screen, its rect in master coords, its
/// 1-based display index and title (both embedded in the top border), its
/// liveness [`PaneState`] (which tints the border), and an optional bound-agent
/// [`AgentMark`] (which can override the tint for an unfocused pane).
pub struct PaneView<'a> {
    pub screen: &'a Screen,
    pub rect: Rect,
    pub index: usize,
    pub title: &'a str,
    pub state: PaneState,
    /// The bound agent's status, if this is an unfocused agent pane amux bound.
    /// `None` for shells, focused panes, and agents still awaiting their
    /// transcript. Attention chrome (§4.2) reads only this.
    pub agent: Option<AgentMark>,
    /// The credential identity **name** this pane runs under, if any — the
    /// name the user typed (`work`, `wif:prod`), never the secret. Rendered as
    /// a colored `·<name>` tag after the `index:title` in the top border. The
    /// color is a redundant channel; the text is always drawn (a11y).
    pub identity: Option<&'a str>,
}

impl PaneView<'_> {
    fn focused(&self) -> bool {
        self.state == PaneState::Focused
    }

    /// The border/title style, composing local liveness with agent attention.
    ///
    /// Precedence (§4.1): `Focused`/`Exited` always keep their local style — focus
    /// must stay findable and a dead child is a hard fact that outranks an
    /// inference. Otherwise a bound agent's attention style wins over
    /// `Active`/`Idle`. Attention chrome applies to unfocused panes only, which
    /// falls out for free (a focused pane never reaches the agent branch).
    fn border_style(&self) -> Style {
        match self.state {
            PaneState::Focused | PaneState::Exited => self.state.style(),
            _ => match self.agent.map(|a| a.status) {
                // Amber: an agent is blocked on the human.
                Some(agsess::Status::WaitingApproval) => Style {
                    fg: crate::theme::WAITING,
                    ..Style::default()
                },
                // Waiting for a prompt is an idle state — grey, same as Idle.
                Some(agsess::Status::WaitingPrompt) => PaneState::Idle.style(),
                // A working agent is unremarkable — default (same as Active).
                Some(agsess::Status::Working) => Style::default(),
                // No mark, or Idle: fall back to the local liveness style.
                Some(agsess::Status::Idle) | None => self.state.style(),
            },
        }
    }

    /// The one-cell attention marker appended after the title (rung 2, §4.2):
    /// ` ? ` waiting-approval, ` ~ ` working, nothing otherwise. Attention chrome
    /// is for unfocused panes only, so a focused (or exited) pane never shows a
    /// badge — the same precedence [`border_style`](PaneView::border_style) uses.
    /// In practice the caller also leaves `agent: None` on the focused pane; this
    /// gate keeps the rule self-consistent regardless.
    fn title_badge(&self) -> &'static str {
        if matches!(self.state, PaneState::Focused | PaneState::Exited) {
            return "";
        }
        match self.agent.map(|a| a.status) {
            Some(agsess::Status::WaitingApproval) => "? ",
            Some(agsess::Status::Working) => "~ ",
            _ => "",
        }
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
pub fn compose(rows: usize, cols: usize, panes: &[PaneView], frame: usize) -> Screen {
    let mut master = Screen::new(rows, cols);

    for p in panes {
        // A pane that has not painted anything yet (freshly spawned agent still
        // booting) shows an animated "starting…" spinner instead of a dead blank
        // rect — so an initializing pane reads as *loading*, not *broken*.
        if screen_is_blank(p.screen) {
            draw_loading(&mut master, p, rows, cols, frame);
        } else {
            blit_inner(&mut master, p, rows, cols);
        }
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

/// Braille spinner frames for the per-pane loading state (advances ~8/sec).
const SPINNER: [char; 8] = ['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];

/// True if the pane's emulator has produced no visible content yet — its screen
/// is entirely default cells. Short-circuits on the first non-blank cell, so a
/// pane that has painted (the common case) is cheap; the full scan only runs
/// while a pane is still blank (its brief loading phase).
fn screen_is_blank(s: &Screen) -> bool {
    let blank = Cell::default();
    for r in 0..s.rows() {
        for c in 0..s.cols() {
            if s.cell(r, c) != blank {
                return false;
            }
        }
    }
    true
}

/// Draw an animated "⣾ starting <title>…" centered in the pane's inner area,
/// tinted with the pane's state style. `frame` advances the spinner. Clipped to
/// the inner box so a narrow pane never bleeds onto the border or neighbors.
fn draw_loading(master: &mut Screen, p: &PaneView, rows: usize, cols: usize, frame: usize) {
    let inner_row = p.rect.row + 1;
    let inner_col = p.rect.col + 1;
    let inner_rows = p.rect.rows.saturating_sub(2);
    let inner_cols = p.rect.cols.saturating_sub(2);
    if inner_rows == 0 || inner_cols == 0 {
        return;
    }
    let spin = SPINNER[frame % SPINNER.len()];
    let label = format!("{spin} starting {}…", p.title);
    let style = p.border_style();
    let mid_r = inner_row + inner_rows / 2;
    let llen = label.chars().count();
    let start_c = inner_col + inner_cols.saturating_sub(llen) / 2;
    for (i, ch) in label.chars().enumerate() {
        let c = start_c + i;
        if c < inner_col + inner_cols && mid_r < rows && c < cols {
            master.set(mid_r, c, Cell { ch, style });
        }
    }
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
    let style = p.border_style();
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

    // Title in the top edge: "┌ 2:claude ·work ──…──┐". The label sits one cell
    // in from the top-left corner, framed by a space each side, and is truncated
    // to leave room for the trailing corner. An unfocused bound agent appends a
    // one-cell attention marker after the title (rung 2, §4.2) — status only,
    // never any transcript text. When the pane carries a credential identity, a
    // `·<name>` tag follows in its own (per-identity) color — the **name** only,
    // never the secret, and the text is always drawn so the signal survives
    // without color (a11y).
    if cn >= 5 {
        let label = format!(" {}:{} {}", p.index, p.title, p.title_badge());
        // Available label columns: everything between the two corners.
        let avail = cn.saturating_sub(2);
        let start = c0 + 1;
        let mut col = 0usize; // columns consumed within `avail`
        for ch in label.chars().take(avail) {
            put(master, r0, start + col, ch);
            col += 1;
        }
        // The identity tag, in its own color, using whatever columns remain.
        if let Some(name) = p.identity {
            let tag = format!("·{name} ");
            let tag_style = Style {
                fg: Color::Indexed(crate::identity::palette_index(name)),
                ..Style::default()
            };
            for ch in tag.chars() {
                if col >= avail {
                    break;
                }
                if r0 < rows && start + col < cols {
                    master.set(
                        r0,
                        start + col,
                        Cell {
                            ch,
                            style: tag_style,
                        },
                    );
                }
                col += 1;
            }
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
            agent: None,
            identity: None,
        }
    }

    /// A pane view carrying a bound agent status (unfocused agent pane).
    fn agent_view<'a>(
        screen: &'a Screen,
        rect: Rect,
        index: usize,
        title: &'a str,
        state: PaneState,
        status: agsess::Status,
    ) -> PaneView<'a> {
        PaneView {
            screen,
            rect,
            index,
            title,
            state,
            agent: Some(AgentMark { status }),
            identity: None,
        }
    }

    /// A pane view carrying a credential identity name-tag.
    fn identity_view<'a>(
        screen: &'a Screen,
        rect: Rect,
        index: usize,
        title: &'a str,
        state: PaneState,
        identity: &'a str,
    ) -> PaneView<'a> {
        PaneView {
            screen,
            rect,
            index,
            title,
            state,
            agent: None,
            identity: Some(identity),
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
        let m = compose(5, 12, &panes, 0);
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
        let m = compose(5, 5, &panes, 0);
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
        let m = compose(5, 5, &panes, 0);
        let corner = m.cell(0, 0).style;
        assert!(corner.bold, "focused border should be bold");
        assert_eq!(corner.fg, crate::theme::FOCUSED, "focused border bright cyan");
        // The title shares the border style.
        assert_eq!(m.cell(0, 1).style.fg, crate::theme::FOCUSED);
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
        let m = compose(5, 5, &panes, 0);
        assert_eq!(
            m.cell(0, 0).style.fg,
            crate::theme::EXITED,
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
        let m = compose(5, 5, &panes, 0);
        assert_eq!(m.cell(0, 0).style.fg, crate::theme::IDLE, "idle border grey");
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
        let m = compose(6, 12, &panes, 0);
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
        let m = compose(10, 12, &panes, 0);
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
        let m = compose(5, 5, &panes, 0);
        // Corners intact; title clipped to the 3 cells between them.
        assert_eq!(m.cell(0, 0).ch, '┌');
        assert_eq!(m.cell(0, 4).ch, '┐');
        let mid: String = (1..4).map(|c| m.cell(0, c).ch).collect();
        assert_eq!(mid, " 9:"); // " 9:verylongtitle " truncated to 3 cells
    }

    // --- agent-aware chrome (0.3) ------------------------------------------

    #[test]
    fn unfocused_waiting_approval_pane_is_yellow_with_a_badge() {
        // A wide box so the badge fits: " 2:claude ? " needs room.
        let inner = filled(3, 18, ' ');
        let panes = vec![agent_view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 20,
            },
            2,
            "claude",
            PaneState::Idle, // unfocused, quiet — but its agent is blocked
            agsess::Status::WaitingApproval,
        )];
        let m = compose(5, 20, &panes, 0);
        // Border tinted bright yellow (Indexed 11), overriding the local Idle grey.
        assert_eq!(
            m.cell(0, 0).style.fg,
            crate::theme::WAITING,
            "waiting-approval border should be bright yellow"
        );
        // Title badge: " 2:claude ? " appears in the top edge.
        let top: String = (1..14).map(|c| m.cell(0, c).ch).collect();
        assert!(top.starts_with(" 2:claude ? "), "top edge was {top:?}");
    }

    #[test]
    fn focused_pane_with_waiting_agent_keeps_focus_style_no_escalation() {
        // A blocked agent in the *focused* pane needs no escalation — you are
        // already there. Focus style (bold bright cyan) wins; no yellow, no badge.
        let inner = filled(3, 16, ' ');
        let panes = vec![agent_view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 18,
            },
            1,
            "claude",
            PaneState::Focused,
            agsess::Status::WaitingApproval,
        )];
        let m = compose(5, 18, &panes, 0);
        let corner = m.cell(0, 0).style;
        assert!(corner.bold, "focused border stays bold");
        assert_eq!(
            corner.fg,
            crate::theme::FOCUSED,
            "focused border stays bright cyan, not yellow"
        );
        // No attention badge on a focused pane.
        let top: String = (1..14).map(|c| m.cell(0, c).ch).collect();
        assert!(!top.contains('?'), "focused pane shows no badge: {top:?}");
    }

    #[test]
    fn exited_pane_keeps_red_even_with_a_stale_agent_mark() {
        // A dead child is a hard local fact that outranks a stale inference.
        let inner = filled(3, 3, ' ');
        let panes = vec![agent_view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 12,
            },
            1,
            "claude",
            PaneState::Exited,
            agsess::Status::WaitingApproval,
        )];
        let m = compose(5, 12, &panes, 0);
        assert_eq!(m.cell(0, 0).style.fg, crate::theme::EXITED, "exited stays red");
    }

    #[test]
    fn working_agent_pane_gets_default_border_and_tilde_badge() {
        let inner = filled(3, 16, ' ');
        let panes = vec![agent_view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 18,
            },
            3,
            "claude",
            PaneState::Idle,
            agsess::Status::Working,
        )];
        let m = compose(5, 18, &panes, 0);
        // Working overrides Idle grey back to default (unremarkable).
        assert_eq!(
            m.cell(0, 0).style.fg,
            Color::Default,
            "working agent border is default"
        );
        let top: String = (1..14).map(|c| m.cell(0, c).ch).collect();
        assert!(top.starts_with(" 3:claude ~ "), "top edge was {top:?}");
    }

    #[test]
    fn waiting_prompt_agent_pane_is_grey_with_no_badge() {
        let inner = filled(3, 16, ' ');
        let panes = vec![agent_view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 18,
            },
            2,
            "claude",
            PaneState::Active, // recent output, but the turn ended cleanly
            agsess::Status::WaitingPrompt,
        )];
        let m = compose(5, 18, &panes, 0);
        // WaitingPrompt reads as idle — grey — overriding the Active default.
        assert_eq!(
            m.cell(0, 0).style.fg,
            crate::theme::IDLE,
            "waiting-prompt border is grey"
        );
        let top: String = (1..14).map(|c| m.cell(0, c).ch).collect();
        assert!(
            !top.contains('?') && !top.contains('~'),
            "no badge: {top:?}"
        );
    }

    // --- identity name-tag (0.4) -------------------------------------------

    #[test]
    fn identity_pane_renders_a_dot_name_tag_in_the_border() {
        // A wide box so " 2:claude ·work " all fits.
        let inner = filled(3, 20, ' ');
        let panes = vec![identity_view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 22,
            },
            2,
            "claude",
            PaneState::Idle,
            "work",
        )];
        let m = compose(5, 22, &panes, 0);
        // The top edge carries "index:title" then the "·work" identity tag.
        let top: String = (1..18).map(|c| m.cell(0, c).ch).collect();
        assert!(top.starts_with(" 2:claude ·work"), "top edge was {top:?}");
    }

    #[test]
    fn identity_tag_uses_the_tunable_palette_color_and_always_has_text() {
        let inner = filled(3, 20, ' ');
        let panes = vec![identity_view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 22,
            },
            1,
            "claude",
            PaneState::Idle,
            "work",
        )];
        let m = compose(5, 22, &panes, 0);
        // Find the "·" cell; it and the name must carry a palette color, never a
        // status hue, and the TEXT must be present regardless of color (a11y).
        let top: Vec<char> = (1..18).map(|c| m.cell(0, c).ch).collect();
        let dot = top.iter().position(|&c| c == '·').expect("· tag present");
        let dot_col = 1 + dot;
        let style = m.cell(0, dot_col).style;
        let expected = Color::Indexed(crate::identity::palette_index("work"));
        assert_eq!(
            style.fg, expected,
            "tag wears its per-identity palette color"
        );
        // The palette avoids the status hues.
        assert!(
            crate::identity::IDENTITY_PALETTE.contains(&12)
                || crate::identity::IDENTITY_PALETTE.contains(&13),
            "palette is the tunable blue/magenta family"
        );
        // Text survives even if a reader ignores color: "·work" is spelled out.
        let rendered: String = top.iter().collect();
        assert!(
            rendered.contains("·work"),
            "identity text present: {rendered:?}"
        );
    }

    #[test]
    fn a_wif_identity_reads_differently_from_a_static_one() {
        // A `wif:` identity surfaces its prefix so federated panes read apart
        // from static-key panes at a glance — the name, still never a secret.
        let inner = filled(3, 22, ' ');
        let panes = vec![identity_view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 24,
            },
            2,
            "claude",
            PaneState::Idle,
            "wif:prod",
        )];
        let m = compose(5, 24, &panes, 0);
        let top: String = (1..20).map(|c| m.cell(0, c).ch).collect();
        assert!(top.contains("·wif:prod"), "wif tag present: {top:?}");
    }

    #[test]
    fn no_identity_pane_has_no_tag() {
        let inner = filled(3, 18, ' ');
        let panes = vec![view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 20,
            },
            2,
            "claude",
            PaneState::Idle,
        )];
        let m = compose(5, 20, &panes, 0);
        let top: String = (1..18).map(|c| m.cell(0, c).ch).collect();
        assert!(!top.contains('·'), "no identity, no tag: {top:?}");
    }

    #[test]
    fn a_blank_pane_shows_an_animated_loading_spinner() {
        // A freshly-spawned pane whose emulator has painted nothing yet (an
        // all-default screen) shows "⣾ starting <title>…" centered, not a dead
        // blank rect — so it reads as loading, not broken.
        let blank = Screen::new(3, 10); // never painted
        let panes = vec![view(
            &blank,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 20,
            },
            2,
            "claude",
            PaneState::Idle,
        )];
        let m0 = compose(5, 20, &panes, 0);
        let whole: String = (0..5)
            .flat_map(|r| (0..20).map(move |c| (r, c)))
            .map(|(r, c)| m0.cell(r, c).ch)
            .collect();
        assert!(whole.contains("starting claude"), "no loading label:\n{whole}");
        assert!(whole.contains('⣾'), "frame 0 spinner missing");
        // The spinner advances with the frame counter (animation).
        let m1 = compose(5, 20, &panes, 1);
        let whole1: String = (0..5)
            .flat_map(|r| (0..20).map(move |c| (r, c)))
            .map(|(r, c)| m1.cell(r, c).ch)
            .collect();
        assert!(whole1.contains('⣽'), "frame 1 spinner should differ");
    }

    #[test]
    fn a_painted_pane_shows_its_content_not_the_spinner() {
        // Once a pane has any content, it blits normally — no loading spinner.
        let inner = filled(3, 18, 'Z');
        let panes = vec![view(
            &inner,
            Rect {
                row: 0,
                col: 0,
                rows: 5,
                cols: 20,
            },
            1,
            "claude",
            PaneState::Focused,
        )];
        let m = compose(5, 20, &panes, 3);
        assert_eq!(m.cell(1, 1).ch, 'Z'); // content, not a spinner
        let whole: String = (0..5)
            .flat_map(|r| (0..20).map(move |c| (r, c)))
            .map(|(r, c)| m.cell(r, c).ch)
            .collect();
        assert!(!whole.contains("starting"), "painted pane must not load");
    }
}
