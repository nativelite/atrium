//! The `ctl send` queue: deliveries held until the target is idle, the caps
//! that keep one wedged pane from growing it, and the flush the run loop calls.

use super::panes::{pane_by_agent, pane_by_agent_mut};
use crate::*;

/// A `ctl send` awaiting delivery. The design queues a task until the target is
/// **idle** (agsess-gated) rather than injecting into a live turn (Decision 4).
/// Once the target is ready we write the text, then — after a short beat so the
/// agent's TUI registers the line before the submit — write the Enter. The beat
/// mirrors the C0 spike, which split the text and `\r` with a delay.
pub(crate) struct PendingSend {
    /// Target pane's global agent id (already resolved + scope-checked).
    target: AgentId,
    text: String,
    /// When the send was accepted — a fallback so a target that never yields a
    /// derivable status (a shell, a not-yet-bound agent) still gets it.
    queued_at: Instant,
    /// How many bytes of `text` the target's pty has actually accepted.
    ///
    /// `write(2)` returns a COUNT and may legitimately take less than offered —
    /// a pty's input buffer is finite, and in canonical mode a single line is
    /// capped near 1 KB. The old code discarded that count, marked the send
    /// delivered, and submitted Enter regardless, so a long task arrived as a
    /// fragment while `ctl` had already replied `ok:true`.
    written: usize,
    /// `Some(t)` once the text has been written IN FULL; `t` gates the follow-up
    /// Enter. Never set on a partial write — Enter must not submit a fragment.
    text_written_at: Option<Instant>,
    /// What queued it: a `ctl send`, or a wake for a bus event. Each origin has
    /// its own cap, so a chatty topic can never crowd out an operator's send.
    origin: SendOrigin,
}

/// Where a queued send came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendOrigin {
    /// `ctl send`: free text, subtree-scoped.
    Ctl,
    /// A bus event delivered to a pane that was addressed or subscribed: a
    /// framed, sanitized headline (see [`wake_text`]).
    BusWake,
}

/// How long after writing the task text we send the Enter that submits it.
pub(crate) const SEND_ENTER_DELAY: Duration = Duration::from_millis(400);

/// If a target never yields a derivable agsess status (non-agent / unbound),
/// deliver anyway once the send has waited this long, so a queue never wedges.
pub(crate) const SEND_UNBOUND_FALLBACK: Duration = Duration::from_secs(2);

/// Most undelivered sends one target may hold. A pane wedged on an open dialog
/// never takes delivery, and each send can be a max-size ctl request, so an
/// uncapped queue grows for as long as the pane stays wedged. 32 is far past any
/// real backlog and bounds one target at about 2 MiB.
pub(crate) const MAX_PENDING_PER_TARGET: usize = 32;

/// Most undelivered bus wakes one target may hold, counted apart from sends:
/// a worker at the bus's own rate cap must not be able to fill the lead's queue
/// and get the operator's next `ctl send` refused. Small, because pending wakes
/// coalesce into one line anyway (see [`queue_send`]).
pub(crate) const MAX_PENDING_WAKES_PER_TARGET: usize = 8;

/// Longest a coalesced wake line grows before further events are summarized
/// as a pointer to the feed.
const WAKE_COALESCE_CHARS: usize = 4096;

/// Queue `text` for `target` unless that target already holds its origin's cap
/// of undelivered entries ([`MAX_PENDING_PER_TARGET`] sends,
/// [`MAX_PENDING_WAKES_PER_TARGET`] wakes). Returns whether it was queued.
///
/// Bus wakes coalesce: while an earlier wake for the same target has not begun
/// to be written, a new one is appended to it (` | ` between), so one idle
/// moment costs the target one turn however many events landed meanwhile. Past
/// [`WAKE_COALESCE_CHARS`] the line ends with a pointer to `bus feed` instead.
pub(super) fn queue_send(
    pending: &mut Vec<PendingSend>,
    target: AgentId,
    text: String,
    origin: SendOrigin,
) -> bool {
    if origin == SendOrigin::BusWake {
        if let Some(ps) = pending
            .iter_mut()
            .find(|ps| ps.target == target && ps.origin == SendOrigin::BusWake && ps.written == 0)
        {
            const MORE: &str = " | +more: atrium ctl bus feed";
            if ps.text.chars().count() + text.chars().count() + 3 <= WAKE_COALESCE_CHARS {
                ps.text.push_str(" | ");
                ps.text.push_str(&text);
            } else if !ps.text.ends_with(MORE) {
                ps.text.push_str(MORE);
            }
            return true;
        }
    }
    let cap = match origin {
        SendOrigin::Ctl => MAX_PENDING_PER_TARGET,
        SendOrigin::BusWake => MAX_PENDING_WAKES_PER_TARGET,
    };
    if pending
        .iter()
        .filter(|ps| ps.target == target && ps.origin == origin)
        .count()
        >= cap
    {
        return false;
    }
    pending.push(PendingSend {
        target,
        text,
        queued_at: Instant::now(),
        written: 0,
        text_written_at: None,
        origin,
    });
    true
}

/// Point every parent link that names `old` at `new`. A respawn mints its pane a
/// new agent id; its workers must follow, or they fall out of its subtree. Pure.
pub(crate) fn repoint_parents<'a>(
    links: impl Iterator<Item = &'a mut Option<AgentId>>,
    old: AgentId,
    new: AgentId,
) {
    for link in links.filter(|l| **l == Some(old)) {
        *link = Some(new);
    }
}

/// Hand the sends queued for a respawned pane's old id to its new one, each from
/// its first byte: whatever part reached the killed process died with it. The
/// wait restarts too, so a new process that has no status yet gets the same grace
/// before delivery as a freshly spawned one, not a send already past it.
pub(super) fn retarget_sends(
    pending: &mut [PendingSend],
    old: AgentId,
    new: AgentId,
    now: Instant,
) {
    for ps in pending.iter_mut().filter(|ps| ps.target == old) {
        ps.target = new;
        ps.written = 0;
        ps.text_written_at = None;
        ps.queued_at = now;
    }
}

/// Flush queued `ctl send`s (Decision 4: queue until the target is idle). For
/// each pending send: once the target reports a ready status (or the unbound
/// fallback elapses), write the text; a beat later write the Enter and drop it.
/// A vanished target is dropped. Returns whether anything was written (so the
/// caller can request a repaint).
pub(crate) fn flush_sends(
    pending: &mut Vec<PendingSend>,
    windows: &mut [Window],
    world: &atrium::vendors::AgentState,
) -> bool {
    if pending.is_empty() {
        return false;
    }
    let now = Instant::now();
    let mut wrote = false;
    pending.retain_mut(|ps| {
        match ps.text_written_at {
            None => {
                // Decide readiness from the target's live status, an open
                // dialog, and any unsent human draft (see atrium::deliver).
                let ready = match pane_by_agent(windows, ps.target) {
                    None => return false, // target gone — drop the send
                    Some(p) => {
                        let sid = p.session_id.as_deref();
                        atrium::deliver::ready(
                            world.status_for(sid),
                            world.awaiting_tool_for(sid),
                            p.draft.holds(now),
                            now.duration_since(ps.queued_at),
                            SEND_UNBOUND_FALLBACK,
                        )
                    }
                };
                if ready {
                    if let Some(p) = pane_by_agent_mut(windows, ps.target) {
                        // Write from where we left off and advance by what was
                        // actually accepted. A short write is normal, not an
                        // error: the pty's buffer is finite. Resuming across
                        // ticks — rather than looping here until the whole thing
                        // lands — is deliberate, because this runs on the single
                        // event loop and a blocking write into a full buffer
                        // would freeze every pane until the target drained it.
                        let bytes = ps.text.as_bytes();
                        match p.pty.write(&bytes[ps.written..]) {
                            Ok(0) => {} // took nothing this tick; try the next
                            Ok(n) => {
                                ps.written += n;
                                wrote = true;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(_) => return false, // target unwritable — drop it
                        }
                        // Only now is the send real. Enter waits for the last byte.
                        if ps.written >= bytes.len() {
                            ps.text_written_at = Some(now);
                        }
                    } else {
                        return false;
                    }
                }
                true
            }
            Some(t) => {
                if now.duration_since(t) >= SEND_ENTER_DELAY {
                    if let Some(p) = pane_by_agent_mut(windows, ps.target) {
                        let _ = p.pty.write(b"\r");
                        wrote = true;
                    }
                    false // delivered — drop
                } else {
                    true
                }
            }
        }
    });
    wrote
}

#[cfg(test)]
mod tests;
