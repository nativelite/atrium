//! Window and pane management from the keyboard: cycle and jump windows, open an
//! agent or shell window, split, move focus, zoom, and kill a pane.
//!
//! Carved out of `run()` (r10 audit B11 residue, step 3). The handlers are
//! unchanged; their effect on the loop is returned as a [`KeyOutcome`].

use crate::*;

/// Apply a window/pane `action` to `windows`; any other action is a no-op.
/// Keyboard-opened panes run under `launch`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_pane_action(
    action: &Action,
    windows: &mut Vec<Window>,
    active: &mut usize,
    launch: &Launch,
    rows: u16,
    cols: u16,
    out: &mut impl std::io::Write,
    flash: &mut Option<(String, Instant)>,
) -> KeyOutcome {
    let mut outcome = KeyOutcome::default();
    let full_repaint = KeyOutcome {
        reset_frame: true,
        repaint: true,
        ..KeyOutcome::default()
    };
    match *action {
        Action::NextPane => {
            let next = (*active + 1) % windows.len();
            switch_window(windows, active, next, rows, cols, out);
            outcome = full_repaint;
        }
        Action::PrevPane => {
            let prev = (*active + windows.len() - 1) % windows.len();
            switch_window(windows, active, prev, rows, cols, out);
            outcome = full_repaint;
        }
        Action::SwitchTo(i) => {
            if i < windows.len() {
                switch_window(windows, active, i, rows, cols, out);
                outcome = full_repaint;
            }
        }
        Action::NewPane => {
            match open_window(
                windows,
                active,
                launch.command,
                launch.identity,
                trust_mode(),
                rows,
                cols,
                out,
                flash,
            ) {
                Ok(()) => outcome.reset_frame = true,
                Err(e) => {
                    *flash = Some((
                        format!("cannot start {:?}: {e}", launch.command[0]),
                        Instant::now(),
                    ));
                }
            }
            outcome.repaint = true;
        }
        Action::NewShellPane => {
            // A plain shell in a new window — no identity, no trust posture
            // (it's not an agent), but it still gets the ctl env injected,
            // so you can run `atrium ctl board list` here and see it rendered.
            let shell = vec![default_shell()];
            match open_window(
                windows,
                active,
                &shell,
                None,
                atrium::ctl::TrustMode::Off,
                rows,
                cols,
                out,
                flash,
            ) {
                Ok(()) => outcome.reset_frame = true,
                Err(e) => {
                    *flash = Some((
                        format!("cannot start shell {:?}: {e}", shell[0]),
                        Instant::now(),
                    ));
                }
            }
            outcome.repaint = true;
        }
        Action::SplitH | Action::SplitV => {
            let dir = if matches!(action, Action::SplitH) {
                layout::Dir::Horizontal
            } else {
                layout::Dir::Vertical
            };
            split_focused(
                &mut windows[*active],
                dir,
                launch.command,
                rows,
                cols,
                launch.identity,
                flash,
            );
            resize_window(&mut windows[*active], rows, cols);
            outcome = full_repaint;
        }
        Action::MoveFocus(d) => {
            let outer = tiled_outer(rows, cols);
            windows[*active].tree.move_focus(to_move(d), outer);
            outcome = full_repaint;
        }
        Action::Zoom => {
            let w = &mut windows[*active];
            // Zoom only means something with more than one pane.
            if w.panes.len() > 1 {
                w.zoomed = !w.zoomed;
                resize_window(w, rows, cols);
                outcome = full_repaint;
            }
        }
        Action::KillPane => {
            let w = &mut windows[*active];
            let victim = w.tree.focus();
            if w.panes.len() > 1 {
                // Multi-pane window: close synchronously so focus leaves
                // the dying pane at once (the next keystrokes must route
                // to the survivor, not the corpse) and the frame re-tiles
                // without waiting for the async reap.
                if let Some(p) = w.pane_mut(victim) {
                    let _ = p.pty.kill();
                }
                w.tree.close(victim);
                w.panes.retain(|p| p.id != victim);
                w.zoomed = w.zoomed && w.panes.len() > 1;
                resize_window(w, rows, cols);
                outcome = full_repaint;
            } else if let Some(p) = w.pane_mut(victim) {
                // Sole pane: async kill; the reap step drops the window
                // and the app exits when the last window is gone (0.1).
                let _ = p.pty.kill();
            }
        }
        _ => {}
    }
    outcome
}
