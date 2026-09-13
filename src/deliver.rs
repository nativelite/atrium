//! When a queued `ctl send` (or bus wake) may be typed into its target pane.
//!
//! Delivery writes the text and then an Enter into the target's pty — the same
//! bytes a human would type. That is only safe when nobody else is using the
//! prompt: two failures were observed live, both from typing at the wrong time.
//!
//! * **A human draft.** The agent sat at its prompt (`WaitingPrompt`), the human
//!   was mid-sentence in that pane, and the delivery appended itself to the half
//!   typed line and submitted both. [`Draft`] tracks unsent human input per pane.
//! * **An open dialog.** A question or plan approval was on screen. agsess decays
//!   any quiet session to `Idle` after a minute, the delivery went in, and its
//!   Enter picked the highlighted option — approving a plan nobody approved. The
//!   `awaiting_tool` input (agsess [`AgentSession::awaiting_tool`]) holds delivery
//!   while the transcript ends on an unresolved `tool_use`, however old.
//!
//! Pure over its inputs (status, flags, instants) so every rule is unit-tested.
//!
//! [`AgentSession::awaiting_tool`]: agsess::AgentSession::awaiting_tool

use std::time::{Duration, Instant};

/// A draft untouched this long is treated as abandoned, so a stray keystroke in
/// a pane nobody returns to cannot hold that pane's queue forever. Long enough
/// that pausing mid-message to think never releases a delivery into it.
pub const DRAFT_ABANDON: Duration = Duration::from_secs(300);

/// Unsent human input in one pane, inferred from the bytes atrium forwards.
///
/// atrium cannot read the agent's input box, so this is a conservative
/// approximation: any typing marks the prompt dirty; a submit (Enter) or a clear
/// (Ctrl+C) marks it clean. Keys that don't edit the prompt — a lone Esc (which
/// interrupts a turn), focus reports, mouse reports — leave it as it was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Draft {
    dirty: bool,
    last_key: Option<Instant>,
}

impl Draft {
    /// Update from one run of bytes forwarded to the pane at `now`.
    pub fn observe(&mut self, bytes: &[u8], now: Instant) {
        match classify(bytes) {
            Key::Neutral => {}
            Key::Clear => {
                self.dirty = false;
                self.last_key = Some(now);
            }
            Key::Edit => {
                self.dirty = true;
                self.last_key = Some(now);
            }
        }
    }

    /// Should a delivery into this pane wait? True while there is unsent input
    /// typed within [`DRAFT_ABANDON`].
    pub fn holds(&self, now: Instant) -> bool {
        match (self.dirty, self.last_key) {
            (true, Some(t)) => now.saturating_duration_since(t) < DRAFT_ABANDON,
            _ => false,
        }
    }
}

enum Key {
    /// Doesn't touch the prompt's contents.
    Neutral,
    /// Submits or clears the prompt.
    Clear,
    /// Leaves text (or a dialog selection in progress) in the prompt.
    Edit,
}

fn classify(b: &[u8]) -> Key {
    const PASTE_END: &[u8] = b"\x1b[201~";
    if b.is_empty()
        || b == b"\x1b"
        || b == b"\x1b[I"
        || b == b"\x1b[O"
        || b.starts_with(b"\x1b[<")
        || b.starts_with(b"\x1b[M")
    {
        return Key::Neutral;
    }
    if b.ends_with(b"\x03") {
        return Key::Clear;
    }
    // Enter submits — but a bracketed paste may end in a newline without
    // submitting, and Alt+Enter (ESC CR) inserts a newline in the prompt.
    let enter = b.ends_with(b"\r") || b.ends_with(b"\n");
    if enter && !b.ends_with(PASTE_END) && !b.ends_with(b"\x1b\r") && !b.ends_with(b"\x1b\n") {
        return Key::Clear;
    }
    Key::Edit
}

/// Is a queued delivery ready to type into its target?
///
/// * `status` — the target's agsess status, `None` when the pane is unbound.
/// * `awaiting_tool` — the transcript ends on an unresolved `tool_use`.
/// * `draft_holds` — [`Draft::holds`] for the target pane.
/// * `waited` / `unbound_fallback` — an unbound pane (a shell, an agent atrium
///   could not bind) never yields a status, so it gets the send after a beat.
pub fn ready(
    status: Option<agsess::Status>,
    awaiting_tool: bool,
    draft_holds: bool,
    waited: Duration,
    unbound_fallback: Duration,
) -> bool {
    if awaiting_tool || draft_holds {
        return false;
    }
    match status {
        Some(agsess::Status::WaitingPrompt) | Some(agsess::Status::Idle) => true,
        Some(_) => false, // Working / WaitingApproval
        None => waited >= unbound_fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsess::Status;

    const FALLBACK: Duration = Duration::from_secs(2);

    fn typed(chunks: &[&[u8]]) -> (Draft, Instant) {
        let t0 = Instant::now();
        let mut d = Draft::default();
        for c in chunks {
            d.observe(c, t0);
        }
        (d, t0)
    }

    #[test]
    fn untouched_pane_never_holds() {
        let (d, t0) = typed(&[]);
        assert!(!d.holds(t0));
    }

    #[test]
    fn typing_holds_until_submit() {
        let (mut d, t0) = typed(&[b"h", b"ello the"]);
        assert!(d.holds(t0 + Duration::from_secs(60)));
        d.observe(b"\r", t0);
        assert!(!d.holds(t0));
    }

    #[test]
    fn a_run_that_ends_in_enter_submits() {
        let (d, t0) = typed(&[b"fast typist\r"]);
        assert!(!d.holds(t0));
    }

    #[test]
    fn ctrl_c_clears_the_draft() {
        let (d, t0) = typed(&[b"never mind", b"\x03"]);
        assert!(!d.holds(t0));
    }

    #[test]
    fn newline_inserting_keys_do_not_count_as_submit() {
        // Alt+Enter, and a bracketed paste whose content ends in a newline.
        let (d, t0) = typed(&[b"line one", b"\x1b\r"]);
        assert!(d.holds(t0));
        let (d, t0) = typed(&[b"\x1b[200~pasted\n\x1b[201~"]);
        assert!(d.holds(t0));
    }

    #[test]
    fn non_editing_input_leaves_the_draft_alone() {
        // Esc interrupts a turn; focus and mouse reports are not typing.
        for k in [
            &b"\x1b"[..],
            b"\x1b[I",
            b"\x1b[O",
            b"\x1b[<0;5;3M",
            b"\x1b[M !!",
        ] {
            let (d, t0) = typed(&[k]);
            assert!(!d.holds(t0), "{k:?} dirtied a clean prompt");
            let (d, t0) = typed(&[b"draft", k]);
            assert!(d.holds(t0), "{k:?} cleared a draft");
        }
    }

    #[test]
    fn an_abandoned_draft_stops_holding() {
        let (d, t0) = typed(&[b"stray"]);
        assert!(d.holds(t0 + DRAFT_ABANDON - Duration::from_secs(1)));
        assert!(!d.holds(t0 + DRAFT_ABANDON));
    }

    #[test]
    fn open_dialog_blocks_even_when_status_decayed_to_idle() {
        // The plan-approval bug: Idle + unresolved tool_use must NOT deliver.
        assert!(!ready(
            Some(Status::Idle),
            true,
            false,
            Duration::ZERO,
            FALLBACK
        ));
        assert!(!ready(
            Some(Status::WaitingPrompt),
            true,
            false,
            Duration::ZERO,
            FALLBACK
        ));
    }

    #[test]
    fn human_draft_blocks_an_otherwise_ready_prompt() {
        assert!(!ready(
            Some(Status::WaitingPrompt),
            false,
            true,
            Duration::ZERO,
            FALLBACK
        ));
        assert!(!ready(None, false, true, FALLBACK * 10, FALLBACK));
    }

    #[test]
    fn status_rules_unchanged_when_nothing_blocks() {
        assert!(ready(
            Some(Status::WaitingPrompt),
            false,
            false,
            Duration::ZERO,
            FALLBACK
        ));
        assert!(ready(
            Some(Status::Idle),
            false,
            false,
            Duration::ZERO,
            FALLBACK
        ));
        assert!(!ready(
            Some(Status::Working),
            false,
            false,
            FALLBACK * 10,
            FALLBACK
        ));
        assert!(!ready(
            Some(Status::WaitingApproval),
            false,
            false,
            FALLBACK * 10,
            FALLBACK
        ));
        assert!(!ready(
            None,
            false,
            false,
            FALLBACK - Duration::from_millis(1),
            FALLBACK
        ));
        assert!(ready(None, false, false, FALLBACK, FALLBACK));
    }
}
