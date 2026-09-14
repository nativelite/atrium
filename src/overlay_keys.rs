//! Keystrokes while a full-screen overlay (overview, board, activity log) is up.
//!
//! Carved out of `run()` (r10 audit B11): the three overlays each consumed every
//! action with its own cursor state, inline in the 1,800-line event loop. The
//! handlers are unchanged; their state now lives in [`Views`] and their effect on
//! the loop is returned as a [`KeyOutcome`] instead of mutating the loop's locals.

use crate::*;

/// Which overlay is up, and each overlay's cursor.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Views {
    /// The mission-control overview (`Ctrl+A o`): the agent tree colored by
    /// status with a selection cursor; `overview_sel` is the selected agent.
    pub(crate) overview: bool,
    pub(crate) overview_sel: usize,
    /// The full-screen board dashboard (`Ctrl+A b`). While on, the panes keep
    /// running (drained, emulated) but are not painted.
    pub(crate) board: bool,
    /// Scrollback offset for the board panel's bus feed: how many of the newest
    /// FYI events to skip (`0` = live). Reset when the panel opens.
    pub(crate) feed_scroll: usize,
    /// Which open decision is selected in the board panel. j/k move it; `r`
    /// resolves it; `g`/Enter jumps to the agent that raised it.
    pub(crate) decision_sel: usize,
    /// The activity log (`Ctrl+A a`): `log_scroll` is how many events back from
    /// the newest the view is scrolled (`0` = tailing).
    pub(crate) log: bool,
    pub(crate) log_scroll: usize,
}

impl Views {
    /// Is any overlay up (so keystrokes must not reach the panes)?
    pub(crate) fn any(&self) -> bool {
        self.overview || self.board || self.log
    }

    /// Open the board — one overlay at a time — at the live end of its feed
    /// with the first decision selected.
    pub(crate) fn open_board(&mut self) {
        *self = Views {
            board: true,
            feed_scroll: 0,
            decision_sel: 0,
            ..self.closed()
        };
    }

    /// Open the overview with the active window's focused agent selected, so
    /// Enter dives back into what you were watching.
    pub(crate) fn open_overview(
        &mut self,
        windows: &[Window],
        active: usize,
        world: &atrium::vendors::VendorWorlds,
    ) {
        let focus = windows[active].tree.focus();
        let sel = overview_nodes(windows, world)
            .iter()
            .position(|n| n.window == active && n.pane_id == focus)
            .unwrap_or(0);
        *self = Views {
            overview: true,
            overview_sel: sel,
            ..self.closed()
        };
    }

    /// Open the activity log, tailing (scrolled to the newest event).
    pub(crate) fn open_log(&mut self) {
        *self = Views {
            log: true,
            log_scroll: 0,
            ..self.closed()
        };
    }

    /// These cursors with every overlay closed.
    fn closed(&self) -> Views {
        Views {
            overview: false,
            board: false,
            log: false,
            ..*self
        }
    }
}

/// What an overlay keystroke asks of the event loop.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyOutcome {
    /// Quit atrium (`Ctrl+A q`).
    pub(crate) quit: bool,
    /// The view changed: drop the retained frame so the next paint is full.
    pub(crate) reset_frame: bool,
    /// Something visible changed: repaint this tick.
    pub(crate) repaint: bool,
}

impl KeyOutcome {
    const QUIT: KeyOutcome = KeyOutcome {
        quit: true,
        reset_frame: false,
        repaint: false,
    };
}

/// Handle `action` if an overlay is up. `None` means no overlay is open and the
/// action belongs to the panes; `Some` means it was consumed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_overlay_key(
    action: &Action,
    views: &mut Views,
    windows: &mut [Window],
    world: &atrium::vendors::VendorWorlds,
    board: &atrium::board::Board,
    bus: &mut atrium::bus::Bus,
    active: &mut usize,
    rows: u16,
    cols: u16,
    out: &mut impl std::io::Write,
    flash: &mut Option<(String, Instant)>,
) -> Option<KeyOutcome> {
    let mut outcome = KeyOutcome::default();
    // While the overview is open, keystrokes drive the selection cursor and
    // dive-in — not the panes. `Ctrl+A o` (toggle) and `Ctrl+A q` (quit)
    // still work via the scanner; everything else is consumed here.
    if views.overview {
        let count: usize = windows.iter().map(|w| w.panes.len()).sum();
        match action {
            Action::ToggleOverview => {
                views.overview = false;
                outcome.reset_frame = true;
                outcome.repaint = true;
            }
            Action::ToggleBoard => {
                // Switch straight from the overview to the board — one press,
                // no need to close the overview first (the board render hides
                // the cursor and clears the screen itself).
                views.open_board();
                outcome.reset_frame = true;
                outcome.repaint = true;
            }
            Action::ToggleLog => {
                views.open_log();
                outcome.reset_frame = true;
                outcome.repaint = true;
            }
            Action::Quit => return Some(KeyOutcome::QUIT),
            Action::MoveFocus(Dir::Up) => {
                views.overview_sel = views.overview_sel.saturating_sub(1);
                outcome.repaint = true;
            }
            Action::MoveFocus(Dir::Down) => {
                views.overview_sel = (views.overview_sel + 1).min(count.saturating_sub(1));
                outcome.repaint = true;
            }
            Action::Forward(b) => {
                let s = b.as_slice();
                if s == b"\r" || s == b"\n" {
                    // Dive into the selected agent: focus its pane, zoom it
                    // full-screen, and RESIZE it (as `Ctrl+A z` does) — the
                    // pane was a small tile, so without the resize it would
                    // paint into a corner.
                    let target = overview_nodes(windows, world)
                        .get(views.overview_sel)
                        .map(|n| (n.window, n.pane_id));
                    if let Some((wi, pid)) = target {
                        *active = wi;
                        windows[*active].tree.focus_pane(pid);
                        windows[*active].zoomed = windows[*active].panes.len() > 1;
                        resize_window(&mut windows[*active], rows, cols);
                        views.overview = false;
                        outcome.reset_frame = true;
                        outcome.repaint = true;
                    }
                } else if s == b"j" || s == b"\x1b[B" || s == b"\x1bOB" {
                    views.overview_sel = (views.overview_sel + 1).min(count.saturating_sub(1));
                    outcome.repaint = true;
                } else if s == b"k" || s == b"\x1b[A" || s == b"\x1bOA" {
                    views.overview_sel = views.overview_sel.saturating_sub(1);
                    outcome.repaint = true;
                } else if s == b"\x1b" {
                    views.overview = false;
                    outcome.reset_frame = true;
                    outcome.repaint = true;
                }
            }
            _ => {} // swallow every other command while the overview is up
        }
        return Some(outcome);
    }
    // While the board panel is up, keystrokes drive it (scroll / switch /
    // quit) rather than reaching the hidden panes. Mirrors the overview
    // block above; `Ctrl+A b` closes, `Ctrl+A o` switches to the overview.
    if views.board {
        // Snapshot the selected decision's seq + source before any mutation
        // (pending_decisions borrows the bus; resolving needs it free).
        let decisions_now = bus.pending_decisions();
        let dcount = decisions_now.len();
        let dsel = views.decision_sel.min(dcount.saturating_sub(1));
        let sel_seq = decisions_now.get(dsel).map(|e| e.seq);
        let sel_from = decisions_now.get(dsel).and_then(|e| e.from.clone());
        drop(decisions_now);
        let scroll_max = bus.tail(atrium::bus::RING_CAP).len().saturating_sub(1);
        // Up/down select a decision when there are any; otherwise they
        // scroll the FYI history. PgUp/PgDn always scroll it.
        let nav = |up: bool, sel: &mut usize, scroll: &mut usize| {
            if dcount > 0 {
                *sel = if up {
                    dsel.saturating_sub(1)
                } else {
                    (dsel + 1).min(dcount - 1)
                };
            } else if up {
                *scroll = (*scroll + 1).min(scroll_max);
            } else {
                *scroll = scroll.saturating_sub(1);
            }
        };
        match action {
            Action::ToggleBoard => {
                views.board = false;
                let _ = write!(out, "\x1b[?25h");
                outcome.reset_frame = true;
                outcome.repaint = true;
            }
            Action::ToggleOverview => {
                views.open_overview(windows, *active, world);
                outcome.reset_frame = true;
                outcome.repaint = true;
            }
            Action::ToggleLog => {
                views.open_log();
                let _ = write!(out, "\x1b[?25h");
                outcome.reset_frame = true;
                outcome.repaint = true;
            }
            Action::Quit => return Some(KeyOutcome::QUIT),
            Action::MoveFocus(Dir::Up) => {
                nav(true, &mut views.decision_sel, &mut views.feed_scroll);
                outcome.repaint = true;
            }
            Action::MoveFocus(Dir::Down) => {
                nav(false, &mut views.decision_sel, &mut views.feed_scroll);
                outcome.repaint = true;
            }
            Action::Forward(b) => {
                let s = b.as_slice();
                if s == b"k" || s == b"\x1b[A" || s == b"\x1bOA" {
                    nav(true, &mut views.decision_sel, &mut views.feed_scroll);
                    outcome.repaint = true;
                } else if s == b"j" || s == b"\x1b[B" || s == b"\x1bOB" {
                    nav(false, &mut views.decision_sel, &mut views.feed_scroll);
                    outcome.repaint = true;
                } else if s == b"\x1b[5~" {
                    views.feed_scroll = (views.feed_scroll + 10).min(scroll_max);
                    outcome.repaint = true;
                } else if s == b"\x1b[6~" {
                    views.feed_scroll = views.feed_scroll.saturating_sub(10);
                    outcome.repaint = true;
                } else if s == b"r" || s == b"R" {
                    // Resolve the selected decision (after you've answered it
                    // in the agent's pane); clear it from the awaiting list.
                    if let Some(seq) = sel_seq {
                        bus.resolve(seq);
                        *flash = Some((format!("resolved decision #{seq}"), Instant::now()));
                        views.decision_sel = views.decision_sel.min(dcount.saturating_sub(2));
                        outcome.repaint = true;
                    }
                } else if s == b"g" || s == b"\r" || s == b"\n" {
                    // Jump to the pane of the agent that raised the decision
                    // (by role), zoomed, and close the panel — go answer it.
                    let target = sel_from.as_ref().and_then(|role| {
                        windows.iter().enumerate().find_map(|(wi, w)| {
                            w.panes
                                .iter()
                                .find(|p| p.role.as_deref() == Some(role.as_str()))
                                .map(|p| (wi, p.id))
                        })
                    });
                    if let Some((wi, pid)) = target {
                        *active = wi;
                        windows[*active].tree.focus_pane(pid);
                        windows[*active].zoomed = windows[*active].panes.len() > 1;
                        resize_window(&mut windows[*active], rows, cols);
                        views.board = false;
                        let _ = write!(out, "\x1b[?25h");
                        outcome.reset_frame = true;
                        outcome.repaint = true;
                    } else {
                        *flash = Some((
                            format!("no pane for agent {}", sel_from.clone().unwrap_or_default()),
                            Instant::now(),
                        ));
                        outcome.repaint = true;
                    }
                } else if s == b"\x1b" {
                    views.board = false;
                    let _ = write!(out, "\x1b[?25h");
                    outcome.reset_frame = true;
                    outcome.repaint = true;
                }
            }
            _ => {} // swallow every other command while the board is up
        }
        return Some(outcome);
    }
    // While the activity log is up, keystrokes scroll it or switch views.
    if views.log {
        let total = collect_log(windows, world, board, bus).len();
        let scroll_max = total.saturating_sub(1);
        match action {
            Action::ToggleLog => {
                views.log = false;
                outcome.reset_frame = true;
                outcome.repaint = true;
            }
            Action::ToggleBoard => {
                views.open_board();
                outcome.reset_frame = true;
                outcome.repaint = true;
            }
            Action::ToggleOverview => {
                views.open_overview(windows, *active, world);
                outcome.reset_frame = true;
                outcome.repaint = true;
            }
            Action::Quit => return Some(KeyOutcome::QUIT),
            // Up/k = older (scroll back); down/j = newer; PgUp/PgDn ×10.
            Action::MoveFocus(Dir::Up) => {
                views.log_scroll = (views.log_scroll + 1).min(scroll_max);
                outcome.repaint = true;
            }
            Action::MoveFocus(Dir::Down) => {
                views.log_scroll = views.log_scroll.saturating_sub(1);
                outcome.repaint = true;
            }
            Action::Forward(b) => {
                let s = b.as_slice();
                if s == b"k" || s == b"\x1b[A" || s == b"\x1bOA" {
                    views.log_scroll = (views.log_scroll + 1).min(scroll_max);
                    outcome.repaint = true;
                } else if s == b"j" || s == b"\x1b[B" || s == b"\x1bOB" {
                    views.log_scroll = views.log_scroll.saturating_sub(1);
                    outcome.repaint = true;
                } else if s == b"\x1b[5~" {
                    views.log_scroll = (views.log_scroll + 10).min(scroll_max);
                    outcome.repaint = true;
                } else if s == b"\x1b[6~" {
                    views.log_scroll = views.log_scroll.saturating_sub(10);
                    outcome.repaint = true;
                } else if s == b"\x1b" {
                    views.log = false;
                    outcome.reset_frame = true;
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        return Some(outcome);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every overlay up with scrolled cursors — no real state, but the worst
    /// case for "one overlay at a time".
    fn everything_open() -> Views {
        Views {
            overview: true,
            overview_sel: 3,
            board: true,
            feed_scroll: 7,
            decision_sel: 2,
            log: true,
            log_scroll: 9,
        }
    }

    #[test]
    fn opening_the_board_closes_the_others_and_rewinds_only_its_own_cursors() {
        let mut v = everything_open();
        v.open_board();
        assert_eq!(
            v,
            Views {
                board: true,
                feed_scroll: 0,
                decision_sel: 0,
                overview: false,
                log: false,
                ..everything_open()
            }
        );
    }

    #[test]
    fn opening_the_log_closes_the_others_and_tails() {
        let mut v = everything_open();
        v.open_log();
        assert_eq!(
            v,
            Views {
                log: true,
                log_scroll: 0,
                overview: false,
                board: false,
                ..everything_open()
            }
        );
        assert!(v.any());
        v.log = false;
        assert!(!v.any());
    }
}
