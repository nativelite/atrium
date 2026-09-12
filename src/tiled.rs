use crate::*;
use atrium::input::Dir;
use atrium::layout::{self, Rect};
use atrium::tile::{compose_into, AgentMark, PaneState, PaneView};
use std::io::Write;
use std::time::Instant;

/// The tiled drawing area: the whole terminal minus the bar row (last line).
pub(crate) fn tiled_outer(rows: u16, cols: u16) -> Rect {
    Rect {
        row: 0,
        col: 0,
        rows: (rows as usize).saturating_sub(1).max(1),
        cols: cols as usize,
    }
}

pub(crate) fn to_move(d: Dir) -> layout::Move {
    match d {
        Dir::Left => layout::Move::Left,
        Dir::Right => layout::Move::Right,
        Dir::Up => layout::Move::Up,
        Dir::Down => layout::Move::Down,
    }
}

/// Compose the active window's panes into `out` (pre-cleared, already sized to
/// the terminal). Borders + titles + liveness color + content; the caller diffs
/// `out` against the previous frame and writes only the changed bytes.
pub(crate) fn render_tiled(
    out: &mut ansi::Screen,
    w: &Window,
    rows: u16,
    cols: u16,
    world: &atrium::vendors::VendorWorlds,
    frame: usize,
) {
    let outer = tiled_outer(rows, cols);
    let rects = w.tree.rects(outer);
    let focus = w.tree.focus();
    let views: Vec<PaneView> = rects
        .iter()
        .filter_map(|(id, rect)| {
            w.pane(*id).map(|p| {
                // Liveness in priority order: focus, then a dead child, then
                // recent background activity, then plain idle. `index` is the
                // pane's 1-based position in the split's leaf order.
                let state = if *id == focus {
                    PaneState::Focused
                } else if p.exited {
                    PaneState::Exited
                } else if p.activity {
                    PaneState::Active
                } else {
                    PaneState::Idle
                };
                // Agent mark only for *unfocused* bound agent panes (§4.1): a
                // blocked agent in the focused pane needs no escalation. The
                // binder maps the pane's injected id to its live status; carries
                // status only — never any transcript text.
                let agent = if *id == focus {
                    None
                } else {
                    world
                        .status_for(p.session_id.as_deref())
                        .map(|status| AgentMark { status })
                };
                let index = w.panes.iter().position(|q| q.id == *id).unwrap_or(0) + 1;
                PaneView {
                    screen: p.term.screen(),
                    rect: *rect,
                    index,
                    title: &p.title,
                    state,
                    agent,
                    identity: p.identity.as_deref(),
                    painted: p.painted,
                }
            })
        })
        .collect();
    // Compose over the full terminal (rows), leaving the bar row untouched; the
    // bar is painted separately after the diff, exactly as in passthrough.
    compose_into(out, rows as usize, cols as usize, &views, frame)
}

/// Split the focused pane, spawning a new pane sized to what its half will be.
/// On spawn failure the split is abandoned and the reason lands in the bar.
pub(crate) fn split_focused(
    w: &mut Window,
    dir: layout::Dir,
    command: &[String],
    rows: u16,
    cols: u16,
    identity: Option<&str>,
    flash: &mut Option<(String, Instant)>,
) {
    let new_id = w.next_id;
    // Size the new pane roughly to a half's *inner* area (minus the border);
    // resize_window fixes it exactly right after the split.
    let (pr, pc) = (
        (rows.saturating_sub(1).max(1) / 2).saturating_sub(2),
        (cols / 2).saturating_sub(2),
    );
    // Splits inherit the active identity (§4). resolve failure lands in `flash`.
    match spawn_pane(
        command,
        pr.max(1),
        pc.max(1),
        new_id,
        identity,
        trust_mode(),
        flash,
    ) {
        Ok(pane) => {
            w.panes.push(pane);
            w.next_id += 1;
            w.tree.split(dir, new_id);
            w.zoomed = false; // a fresh split is always tiled
        }
        Err(e) => {
            *flash = Some((
                format!("cannot start {:?}: {e}", command[0]),
                Instant::now(),
            ));
            let _ = rows;
            let _ = cols;
        }
    }
}

/// Recompute every pane's rect and push the size to its pty and emulator. In
/// passthrough (single/zoomed) the focused pane owns the whole area; in tiled
/// mode each pane gets its rect.
pub(crate) fn resize_window(w: &mut Window, rows: u16, cols: u16) {
    if w.tiled() {
        let outer = tiled_outer(rows, cols);
        let rects = w.tree.rects(outer);
        for (id, rect) in rects {
            // Each pane wears a one-cell box border on every side, so its child
            // and emulator see the *inner* content area, not the bordered rect.
            let inner_rows = rect.rows.saturating_sub(2).max(1);
            let inner_cols = rect.cols.saturating_sub(2).max(1);
            if let Some(p) = w.pane_mut(id) {
                let _ = p.pty.resize(inner_rows as u16, inner_cols as u16);
                p.term.resize(inner_rows, inner_cols);
            }
        }
    } else {
        // Passthrough: the focused (or sole) pane fills the area above the bar.
        let ar = rows.saturating_sub(1).max(1);
        let focus = w.tree.focus();
        let sole = w.panes.len() == 1;
        for p in w.panes.iter_mut() {
            if p.id == focus || sole {
                let _ = p.pty.resize(ar, cols);
                p.term.resize(ar as usize, cols as usize);
            }
        }
    }
}

pub(crate) fn switch_window(
    windows: &mut [Window],
    active: &mut usize,
    to: usize,
    rows: u16,
    cols: u16,
    out: &mut impl Write,
) {
    if to == *active {
        return;
    }
    *active = to;
    for p in windows[to].panes.iter_mut() {
        p.activity = false;
    }
    // Re-assert the bar-protecting scroll region (rows-1) before clearing: a
    // passthrough app (claude) may have changed or reset the real terminal's
    // scroll region while it ran, and without restoring it the next pane can
    // scroll into the bar row.
    let _ = write!(
        out,
        "\x1b[1;{}r\x1b[2J\x1b[H",
        rows.saturating_sub(1).max(1)
    );
    let _ = out.flush();
    resize_window(&mut windows[to], rows, cols);
}

/// Passthrough repaint: nudge the focused pane's pty size so its terminal
/// repaints in full (ConPTY always does; Unix full-screen apps redraw on
/// SIGWINCH). Used on window switch and mode changes into passthrough.
pub(crate) fn repaint_focused(w: &mut Window, rows: u16, cols: u16, out: &mut impl Write) {
    // Re-assert the bar-protecting scroll region (see `switch_window`) so the
    // refreshed pane stays out of the bar row.
    let ar = rows.saturating_sub(1).max(1);
    let _ = write!(out, "\x1b[1;{ar}r");
    let focus = w.tree.focus();
    if let Some(p) = w.pane_mut(focus) {
        // Paint from OUR OWN emulator — never by asking the child to redraw.
        //
        // This used to clear the screen and then provoke a repaint by resizing
        // the pty `h-1` and straight back to `h`. That ends at the size the
        // child already had, so an app that repaints only on a real dimension
        // change — or one that is mid-turn and defers — correctly does nothing,
        // and the operator is left with a cleared screen showing only the bar.
        // Reported on macOS after pressing `g` on a board decision while that
        // agent was answering. It never bit Windows, where ConPTY delivers
        // resize differently and conhost forces a full repaint.
        //
        // `render_full` clears and reflects everything fed so far, so the
        // repaint is ours and cannot be declined. It is the same call the
        // splash handoff already depends on. The single resize is kept only so
        // the child's idea of its size stays correct; nothing now hangs on it.
        let _ = p.pty.resize(ar, cols);
        let full = p.term.screen().render_full();
        let _ = out.write_all(&full);
    }
    let _ = out.flush();
}
