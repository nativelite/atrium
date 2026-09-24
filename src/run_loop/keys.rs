//! Phase 1 of a tick: wait for input, then turn keystrokes into actions.

use super::{Flow, RunState};
use crate::*;

impl RunState<'_> {
    /// Wait for a key or pane output — woken the moment either arrives — or at
    /// most a tick. `None` when the terminal is gone (the loop then ends).
    pub(super) fn wait_for_input(&mut self) -> Option<Vec<u8>> {
        match &self.keys {
            Some(keys) => {
                self.wake.wait(LOOP_TICK);
                keys.take().ok()
            }
            // No key thread could be started: wait on the keyboard as before.
            None => self.term.read_bytes(Duration::from_millis(15)).ok(),
        }
    }

    /// Feed one read of keystrokes through the command prompt and the prefix
    /// scanner, and act on each action. [`Flow::Exit`] when one quits.
    pub(super) fn handle_keys(&mut self, bytes: &[u8]) -> Flow {
        if !bytes.is_empty() && std::env::var_os("ATRIUM_DEBUG").is_some() {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            eprint!("[atrium-dbg stdin {}]\r\n", hex.join(" "));
        }
        // The command prompt captures keystrokes: while it is open, keys edit the
        // command line (not the panes), and the scanner is fed nothing. Enter opens
        // the typed command in a new pane; Esc/Ctrl+C cancels.
        let feed: &[u8] = if self.prompt.is_some() {
            feed_prompt(
                &mut self.prompt,
                bytes,
                &mut self.windows,
                &mut self.active,
                &self.launch,
                self.rows,
                self.cols,
                &mut self.out,
                &mut self.flash,
            )
            .apply(&mut self.renderer, &mut self.force_repaint);
            b""
        } else {
            bytes
        };
        for action in self.scanner.feed(feed) {
            if self.handle_action(action) == Flow::Exit {
                return Flow::Exit;
            }
        }
        Flow::Continue
    }

    /// One action from the scanner: the prompt's, an overlay's, or the panes'.
    fn handle_action(&mut self, action: Action) -> Flow {
        // Keys that arrived in the same read as the `Ctrl+A :` that opened
        // the prompt belong to it (a paste or a fast typist), not the pane.
        if let (Some(_), Action::Forward(b)) = (&self.prompt, &action) {
            feed_prompt(
                &mut self.prompt,
                b,
                &mut self.windows,
                &mut self.active,
                &self.launch,
                self.rows,
                self.cols,
                &mut self.out,
                &mut self.flash,
            )
            .apply(&mut self.renderer, &mut self.force_repaint);
            return Flow::Continue;
        }
        if let Some(outcome) = handle_overlay_key(
            &action,
            &mut self.views,
            &mut self.windows,
            &self.world,
            &self.board,
            &mut self.bus,
            &mut self.active,
            self.rows,
            self.cols,
            &mut self.out,
            &mut self.flash,
        ) {
            if outcome.apply(&mut self.renderer, &mut self.force_repaint) {
                self.deliberate_exit = true;
                return Flow::Exit;
            }
            return Flow::Continue;
        }
        match action {
            Action::Forward(b) => {
                if let Some(p) = self.windows[self.active].focused_mut() {
                    p.draft.observe(&b, Instant::now());
                    let _ = p.pty.write(&b);
                }
            }
            Action::NextPane
            | Action::PrevPane
            | Action::SwitchTo(_)
            | Action::NewPane
            | Action::NewShellPane
            | Action::SplitH
            | Action::SplitV
            | Action::MoveFocus(_)
            | Action::Zoom
            | Action::KillPane => {
                handle_pane_action(
                    &action,
                    &mut self.windows,
                    &mut self.active,
                    &self.launch,
                    self.rows,
                    self.cols,
                    &mut self.out,
                    &mut self.flash,
                )
                .apply(&mut self.renderer, &mut self.force_repaint);
            }
            // With no overlay up these only open one (closing, and switching
            // between overlays, is `handle_overlay_key`'s). Opening is a full
            // repaint: the overlay replaces the panes.
            Action::ToggleBoard => {
                self.views.open_board();
                self.renderer.reset();
                self.force_repaint = true;
            }
            Action::OpenPrompt => {
                // Start the command prompt; subsequent keystrokes edit the line
                // (handled above the scanner) until Enter/Esc.
                self.prompt = Some(String::new());
                self.force_repaint = true;
            }
            Action::ToggleOverview => {
                self.views
                    .open_overview(&self.windows, self.active, &self.world);
                let _ = write!(self.out, "\x1b[?25h");
                self.renderer.reset();
                self.force_repaint = true;
            }
            Action::ToggleLog => {
                self.views.open_log();
                let _ = write!(self.out, "\x1b[?25h");
                self.renderer.reset();
                self.force_repaint = true;
            }
            Action::ToggleMouse => self.toggle_mouse(),
            Action::MouseClick { .. }
            | Action::MouseDrag { .. }
            | Action::MouseRelease { .. }
            | Action::MouseScroll { .. } => {
                handle_mouse(
                    &action,
                    &mut self.windows,
                    self.active,
                    &mut self.selection,
                    self.rows,
                    self.cols,
                    &mut self.out,
                    &mut self.flash,
                )
                .apply(&mut self.renderer, &mut self.force_repaint);
            }
            Action::Quit => {
                if std::env::var_os("ATRIUM_DEBUG").is_some() {
                    eprint!("[atrium-dbg quit-received]\r\n");
                }
                self.deliberate_exit = true;
                return Flow::Exit;
            }
        }
        Flow::Continue
    }

    /// `Ctrl+A m`: turn atrium's mouse capture on or off.
    fn toggle_mouse(&mut self) {
        self.mouse_on = !self.mouse_on;
        // Keep the terminal's capture and the scanner's parsing in
        // lockstep — set both, and don't leave one on if the other
        // fails.
        if self.term.set_mouse(self.mouse_on).is_ok() {
            self.scanner.set_mouse(self.mouse_on);
            self.flash = Some((
                if self.mouse_on {
                    "mouse: ON — click focuses, drag selects within a tile (copies), wheel scrolls"
                        .to_string()
                } else {
                    "mouse: OFF — native drag to select / copy".to_string()
                },
                Instant::now(),
            ));
        } else {
            self.mouse_on = !self.mouse_on; // revert on failure
            self.flash = Some(("mouse mode unavailable here".to_string(), Instant::now()));
        }
        self.force_repaint = true;
    }
}
