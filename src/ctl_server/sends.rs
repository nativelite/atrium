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
    /// The target's startup screen this send was last reported held behind on
    /// the bar ([`atrium::vendors::dialog_markers`]), so each is said once.
    held_behind: Option<&'static str>,
    /// The bytes this entry writes, fixed when its first byte is written
    /// ([`delivery_bytes`]). The form depends on the target's live state (did
    /// its app turn paste mode on), and `written` counts into these bytes: a
    /// form recomputed on a later tick could be shorter than `written`, or
    /// carry on mid-way through a different encoding. `None` until then.
    wire: Option<Vec<u8>>,
    /// Not started before this: set when an earlier delivery to the same
    /// target has just been submitted, so the agent can take that prompt
    /// before the next one is typed.
    not_before: Option<Instant>,
    /// Who caused it, so a delivery that is dropped can be reported to them.
    by: SentBy,
}

/// Who a queued delivery is from: the pane whose request queued it, or the
/// operator's label. Neither for what atrium queues itself (a decision's
/// wake, the notice of a dropped delivery).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SentBy {
    pub(crate) pane: Option<AgentId>,
    pub(crate) operator: Option<String>,
}

/// Where a queued send came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendOrigin {
    /// `ctl send`: free text, subtree-scoped.
    Ctl,
    /// A bus event delivered to a pane that was addressed or subscribed: a
    /// framed, sanitized headline (see [`wake_text`]).
    BusWake,
    /// `ctl command`: one allowlisted slash command, typed as it is (no
    /// header, no paste) so the agent CLI reads it as its own command.
    Command,
}

/// How long after writing the task text we send the Enter that submits it.
pub(crate) const SEND_ENTER_DELAY: Duration = Duration::from_millis(400);

/// How long after one delivery's Enter the next delivery to the same pane
/// waits before its readiness is even asked. The Enter starts a turn, and the
/// status that says so (a transcript line, a redraw) takes a moment to show:
/// without the beat a second delivery was judged against the state the first
/// one had just ended.
pub(crate) const POST_ENTER_HOLD: Duration = Duration::from_millis(1500);

/// If a target never yields a derivable agsess status (non-agent / unbound),
/// deliver anyway once the send has waited this long, so a queue never wedges.
pub(crate) const SEND_UNBOUND_FALLBACK: Duration = Duration::from_secs(2);

/// How long an unbound pane's screen must have been still, after it drew
/// something, before a send is typed into it ([`atrium::deliver::ready`]'s
/// `settled`). A TUI bringing up its first screen loses what is typed at it
/// (codex drains pending input before its interactive screen; a cooked tty
/// keeps one line), and the two-second beat alone caught codex mid-start.
/// Half a second after the last byte: a welcome screen is drawn in one burst,
/// a startup that is still printing keeps the send waiting.
pub(crate) const SEND_SETTLE: Duration = Duration::from_millis(500);

/// The bytes a delivery types for `text` into a pane whose app has bracketed
/// paste on (`paste_wanted`): what a terminal sends for a paste, the text
/// between `ESC[200~` and `ESC[201~`, so a multi-line text lands as one
/// insertion and the Enter after it submits. Typed as keystrokes, codex's
/// composer took a seven-line block as a run of lines, lost its tail and never
/// submitted (measured, codex 0.163.0 on Linux); as a paste the same block was
/// submitted whole. A pane that did not ask for paste mode (a plain shell, a
/// program reading raw input) gets the bare text, as before. Windows gets the
/// bare text too: its console host turns input into key records, and the
/// markers have no key to become.
/// Longest an unbound pane of a known vendor is held for its prompt hint
/// ([`atrium::vendors::prompt_marker`]) before the settle gate alone decides:
/// a TUI that never shows its prompt (not signed in, a dialog, a hung start)
/// still gets the send rather than wedging its queue. Generous, because a
/// send typed into a TUI that is still starting is lost, and a slow start is
/// real: codex's startup makes network requests (plugin sync, account
/// limits) and was measured taking minutes under a bad certificate path.
pub(crate) const SEND_PROMPT_WAIT: Duration = Duration::from_secs(300);

/// Is `needle` on the pane's screen? Each row is read as the glyphs it shows
/// (the right half of a wide glyph skipped), so a hint drawn with attributes
/// around it still matches; a needle split across two rows does not, and the
/// prompt hints are short enough never to wrap.
pub(crate) fn screen_has(screen: &ansi::Screen, needle: &str) -> bool {
    let (rows, cols) = (screen.rows(), screen.cols());
    let mut line = String::with_capacity(cols);
    for r in 0..rows {
        line.clear();
        for c in 0..cols {
            let cell = screen.cell(r, c);
            if cell.width != ansi::CellWidth::Continuation {
                line.push(cell.ch);
            }
        }
        if line.contains(needle) {
            return true;
        }
    }
    false
}

/// Does `pane`'s screen show its vendor's prompt hint, or is there no hint to
/// wait for? `waited` past [`SEND_PROMPT_WAIT`] counts as shown — unless one
/// of the vendor's own startup screens ([`atrium::vendors::dialog_markers`])
/// is up, which holds the send however long it has waited: a brief typed at
/// codex's hooks review picked its first item and was lost (measured, 0.163.0).
fn prompt_up(title: &str, screen: &ansi::Screen, waited: Duration) -> bool {
    let Some(vendor) = atrium::vendors::vendor_for_stem(title) else {
        return true;
    };
    if dialog_up(vendor, screen).is_some() {
        return false;
    }
    match atrium::vendors::prompt_marker(vendor) {
        Some(hint) => waited >= SEND_PROMPT_WAIT || screen_has(screen, hint),
        None => true,
    }
}

/// The startup screen of `vendor`'s that `screen` shows, if any: the marker
/// text, for the bar.
fn dialog_up(vendor: agsess::Vendor, screen: &ansi::Screen) -> Option<&'static str> {
    atrium::vendors::dialog_markers(vendor)
        .iter()
        .copied()
        .find(|m| screen_has(screen, m))
}

/// May a queued delivery be typed into a pane's pty? Into an agent's pane,
/// yes; into any other only in a `skip` session ([`atrium::ctl::text_may_land`]
/// for a pane's text, with no inbox left to take it). An operator's text is
/// never queued for a pane that is not an agent's, at any trust.
pub(crate) fn may_type_into(agent_pane: bool, policy: atrium::ctl::TrustMode) -> bool {
    atrium::ctl::text_may_land(agent_pane, false, atrium::ctl::TextFrom::Pane, policy)
}

/// How a delivery's text is written into a pane's pty. The Enter that
/// submits it is written afterwards, once, by [`flush_sends`]; so the one
/// question for each form is whether a line break **inside** the text could
/// submit early and let a later line open a fresh prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypeAs {
    /// A bracketed paste, `ESC[200~ text ESC[201~`: the text is one insertion
    /// and its line breaks are text. For a TUI that parses the markers (codex,
    /// claude, an app that turned the mode on), on unix.
    Paste,
    /// One line: every line break becomes ` ↵ ` and a tab a space, so no byte
    /// that could submit, complete or move is written before the Enter. For
    /// an agent's pane where no paste can be relied on: every agent pane on
    /// Windows (the console host turns input into key records and the markers
    /// have no key to become), and an agent CLI that did not ask for paste
    /// mode on unix.
    Flat,
    /// The text as it is. For a `ctl command`, whose text is one validated
    /// word, and for a pane that is not an agent's (reached in a `skip`
    /// session alone, where a shell is typed into as it always was) unless
    /// its app turned paste mode on.
    Raw,
}

/// The form a delivery into this pane takes. `app_paste`: the pane's app
/// turned bracketed paste on; `cli_paste`: its agent CLI is one that parses a
/// paste whether or not atrium saw it turn the mode on (codex, claude). Pure.
///
/// * a command: raw, everywhere;
/// * an agent's pane: a paste on unix when the app or the CLI takes one,
///   otherwise (and always on Windows) one flat line;
/// * a pane that is not an agent's: as it was before 0.47.0, a paste on unix
///   when its app turned the mode on, otherwise the raw text.
pub(crate) fn type_as(
    agent_pane: bool,
    origin: SendOrigin,
    app_paste: bool,
    cli_paste: bool,
) -> TypeAs {
    if origin == SendOrigin::Command {
        TypeAs::Raw
    } else if cfg!(unix) && (app_paste || (agent_pane && cli_paste)) {
        TypeAs::Paste
    } else if agent_pane {
        TypeAs::Flat
    } else {
        TypeAs::Raw
    }
}

/// What a line break becomes in a [`TypeAs::Flat`] delivery.
pub(crate) const FLAT_BREAK: &str = " \u{21b5} ";

/// The bytes written for `text` in the form `how`. Pure.
pub(crate) fn delivery_bytes(text: &str, how: TypeAs) -> Vec<u8> {
    match how {
        TypeAs::Paste => typed_bytes(text, true),
        TypeAs::Raw => text.as_bytes().to_vec(),
        TypeAs::Flat => {
            let mut out = String::with_capacity(text.len());
            let mut chars = text.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\r' => {
                        if chars.peek() == Some(&'\n') {
                            chars.next();
                        }
                        out.push_str(FLAT_BREAK);
                    }
                    '\n' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}' => {
                        out.push_str(FLAT_BREAK)
                    }
                    '\t' => out.push(' '),
                    c => out.push(c),
                }
            }
            out.into_bytes()
        }
    }
}

/// Has an unbound pane of a known agent CLI gone the whole
/// [`SEND_PROMPT_WAIT`] without showing its prompt hint? Then whatever is on
/// its screen is not a prompt (a sign-in, a dialog atrium has no marker for, a
/// hung start), and a delivery typed there would be keystrokes into it: a
/// digit in a role picks a numbered option, the Enter confirms the
/// highlighted one. The delivery is dropped instead, and the bar says so.
/// (Until 0.47.0 it was typed after the wait.)
pub(crate) fn prompt_never_came(title: &str, screen: &ansi::Screen, waited: Duration) -> bool {
    atrium::vendors::vendor_for_stem(title)
        .and_then(atrium::vendors::prompt_marker)
        .is_some_and(|hint| waited >= SEND_PROMPT_WAIT && !screen_has(screen, hint))
}

pub(crate) fn typed_bytes(text: &str, paste_wanted: bool) -> Vec<u8> {
    if cfg!(unix) && paste_wanted {
        let mut v = Vec::with_capacity(text.len() + 12);
        v.extend_from_slice(b"\x1b[200~");
        v.extend_from_slice(text.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        text.as_bytes().to_vec()
    }
}

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
    by: SentBy,
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
        SendOrigin::Ctl | SendOrigin::Command => MAX_PENDING_PER_TARGET,
        SendOrigin::BusWake => MAX_PENDING_WAKES_PER_TARGET,
    };
    if pending
        .iter()
        .filter(|ps| {
            ps.target == target
                && (ps.origin == SendOrigin::BusWake) == (origin == SendOrigin::BusWake)
        })
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
        held_behind: None,
        wire: None,
        not_before: None,
        by,
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
        ps.wire = None;
        ps.not_before = None;
        ps.queued_at = now;
    }
}

/// Why a queued delivery was dropped without being typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropReason {
    /// The target pane is gone.
    Gone,
    /// The target is not an agent's pane and nothing is left to collect the
    /// delivery (its mod went with a respawn): typing it would be running it.
    NotAnAgent,
    /// An agent CLI with no status never showed its prompt.
    NoPrompt,
    /// The pane's terminal refused the bytes.
    Unwritable,
}

impl DropReason {
    /// The reason in words, fixed by atrium: nothing of the delivery's text.
    pub(crate) fn words(self, target: AgentId) -> String {
        match self {
            DropReason::Gone => format!("pane {target} has exited"),
            DropReason::NotAnAgent => format!(
                "pane {target} is not an agent's pane and the mod that was to collect the \
                 delivery is gone; nothing is typed into a shell below --trust skip"
            ),
            DropReason::NoPrompt => format!(
                "pane {target} did not show its prompt within {} s; answer what it is \
                 showing, then send again",
                SEND_PROMPT_WAIT.as_secs()
            ),
            DropReason::Unwritable => format!("pane {target}'s terminal took no input"),
        }
    }
}

/// What one tick does with one queued entry.
enum Step {
    Keep,
    /// Its Enter was written: done.
    Delivered,
    Drop(DropReason),
}

/// Flush queued deliveries (Decision 4: queue until the target is idle). For
/// each: once the target is ready, write the text; a beat later write the
/// Enter that submits it, and drop the entry. Returns whether anything was
/// written (so the caller can request a repaint).
///
/// **One delivery at a time per pane.** An entry is "in flight" from its first
/// byte to its Enter. While one is, every other entry for that pane waits,
/// and after the Enter they wait [`POST_ENTER_HOLD`] more before readiness is
/// asked again. So an Enter always submits exactly the entry it belongs to:
/// two sends ready in the same tick are text, Enter, text, Enter, and a
/// `command` never shares a line with a send or a wake. (Each entry used to
/// be judged alone, and two that became ready together were both written
/// before either Enter.)
///
/// **A dropped delivery is reported.** An entry that cannot be typed (the
/// pane is gone, is no longer one text may be typed into, or never showed its
/// prompt) is said on the bar, written to the audit log under whoever sent
/// it, and, when the sender is a pane that can take one, announced to it by
/// one `fyi` on the `atrium` topic whose words are atrium's own. That notice
/// has no sender, so if it is dropped in turn the audit row is all there is:
/// it cannot loop.
pub(crate) fn flush_sends(
    pending: &mut Vec<PendingSend>,
    windows: &mut [Window],
    world: &atrium::vendors::AgentState,
    mods: &atrium::modstate::ModState,
    flash: &mut Option<(String, Instant)>,
    audit: &mut atrium::audit::Audit,
    bus: &mut atrium::bus::Bus,
) -> bool {
    if pending.is_empty() {
        return false;
    }
    let now = Instant::now();
    let policy = trust_mode();
    let mut wrote = false;
    // Panes with an entry between its first byte and its Enter.
    let mut in_flight: Vec<AgentId> = pending
        .iter()
        .filter(|ps| ps.started())
        .map(|ps| ps.target)
        .collect();
    // Panes whose Enter was written this tick.
    let mut entered: Vec<AgentId> = Vec::new();
    let mut dropped: Vec<(AgentId, SendOrigin, SentBy, DropReason)> = Vec::new();
    let mut keep: Vec<bool> = Vec::with_capacity(pending.len());
    for ps in pending.iter_mut() {
        let step = flush_one(
            ps,
            windows,
            world,
            mods,
            flash,
            (now, policy),
            &mut in_flight,
            &mut entered,
            &mut wrote,
        );
        match step {
            Step::Keep => keep.push(true),
            Step::Delivered => keep.push(false),
            Step::Drop(why) => {
                dropped.push((ps.target, ps.origin, ps.by.clone(), why));
                keep.push(false);
            }
        }
    }
    let mut kept = keep.into_iter();
    pending.retain(|_| kept.next().unwrap_or(true));
    for ps in pending.iter_mut().filter(|ps| entered.contains(&ps.target)) {
        ps.not_before = Some(now + POST_ENTER_HOLD);
    }
    for (target, origin, by, why) in dropped {
        report_drop(
            target, origin, &by, why, pending, windows, mods, flash, audit, bus, policy, now,
        );
    }
    wrote
}

impl PendingSend {
    /// Between its first byte and its Enter.
    fn started(&self) -> bool {
        self.wire.is_some() || self.text_written_at.is_some()
    }
}

/// One entry, one tick: see [`flush_sends`].
#[allow(
    clippy::too_many_arguments,
    reason = "the loop's state, lent to one step; a struct would only rename it"
)]
fn flush_one(
    ps: &mut PendingSend,
    windows: &mut [Window],
    world: &atrium::vendors::AgentState,
    mods: &atrium::modstate::ModState,
    flash: &mut Option<(String, Instant)>,
    (now, policy): (Instant, atrium::ctl::TrustMode),
    in_flight: &mut Vec<AgentId>,
    entered: &mut Vec<AgentId>,
    wrote: &mut bool,
) -> Step {
    // The Enter of an entry whose text is fully written: its own, and the
    // only thing written to this pane on this tick.
    if let Some(t) = ps.text_written_at {
        if now.duration_since(t) < SEND_ENTER_DELAY {
            return Step::Keep;
        }
        if let Some(p) = pane_by_agent_mut(windows, ps.target) {
            let _ = p.pty.write(b"\r");
            *wrote = true;
            note_delivered(ps.origin);
        }
        entered.push(ps.target);
        return Step::Delivered;
    }
    if ps.wire.is_none() {
        // Not started. Another entry for this pane is in flight, or was
        // submitted a moment ago: wait behind it.
        if in_flight.contains(&ps.target) || entered.contains(&ps.target) {
            return Step::Keep;
        }
        // A pane whose mod takes its own deliveries (`ctl inbox`) is never
        // typed into: the item waits in the queue for the mod to collect it,
        // and the engine's own prompt queue decides when it starts a turn.
        if mods.has_cap(ps.target.0, "inbox") {
            return Step::Keep;
        }
        let Some(p) = pane_by_agent(windows, ps.target) else {
            return Step::Drop(DropReason::Gone);
        };
        // The last guard before anything is typed. A queued delivery was
        // accepted for a pane text may land in (`ctl::text_may_land`), and
        // one way that can stop being true: the target's mod was to collect
        // it over `inbox`, and the mod is gone (the pane was respawned). What
        // is left is a shell, and typing into a shell is running a command.
        let agent_pane = super::dispatch::is_agent_pane(p);
        if !may_type_into(agent_pane, policy) {
            return Step::Drop(DropReason::NotAnAgent);
        }
        let sid = p.session_id.as_deref();
        // The target's own startup screen holds the send for as long as it is
        // up; the bar says so, once per screen.
        let dialog =
            atrium::vendors::vendor_for_stem(&p.title).and_then(|v| dialog_up(v, p.term.screen()));
        if dialog != ps.held_behind {
            if let Some(d) = dialog {
                *flash = Some((
                    format!(
                        "send to pane {} held: it is waiting on codex's \"{d}\" screen; \
                         answer it there",
                        p.agent_id
                    ),
                    now,
                ));
            }
            ps.held_behind = dialog;
        }
        let waited = now.duration_since(ps.queued_at);
        // An unbound agent CLI that never showed its prompt: not typed into
        // whatever it is showing instead.
        if world.status_for(sid).is_none()
            && dialog.is_none()
            && prompt_never_came(&p.title, p.term.screen(), waited)
        {
            return Step::Drop(DropReason::NoPrompt);
        }
        if ps.not_before.is_some_and(|t| now < t) {
            return Step::Keep;
        }
        // Drawn, nothing more arrived for the settle beat, and the agent's
        // prompt hint is on screen: the app's own prompt is up, not a loading
        // screen that happens to sit still. Then readiness from the target's
        // live status, an open dialog, and any unsent human draft (see
        // atrium::deliver).
        let settled = p.painted
            && now.duration_since(p.last_activity) >= SEND_SETTLE
            && prompt_up(&p.title, p.term.screen(), waited);
        let ready = atrium::deliver::ready(
            world.status_for(sid),
            world.awaiting_tool_for(sid),
            p.draft.holds(now),
            waited,
            SEND_UNBOUND_FALLBACK,
            settled,
        );
        if !ready {
            return Step::Keep;
        }
        // The bytes, decided once, here: a paste where the pane takes one,
        // one flat line into any other agent's pane, the raw text into a
        // pane that is not an agent's (see `type_as`).
        let cli_paste =
            atrium::vendors::vendor_for_stem(&p.title).is_some_and(atrium::vendors::takes_paste);
        let how = type_as(agent_pane, ps.origin, p.paste_wanted, cli_paste);
        ps.wire = Some(delivery_bytes(&ps.text, how));
        in_flight.push(ps.target);
    }
    // Write from where we left off and advance by what was actually accepted.
    // A short write is normal, not an error: the pty's buffer is finite.
    // Resuming across ticks, rather than looping here until the whole thing
    // lands, is deliberate, because this runs on the single event loop and a
    // blocking write into a full buffer would freeze every pane until the
    // target drained it.
    let Some(p) = pane_by_agent_mut(windows, ps.target) else {
        return Step::Drop(DropReason::Gone);
    };
    let wire = ps.wire.as_deref().unwrap_or(&[]);
    let rest = wire.get(ps.written..).unwrap_or(&[]);
    match p.pty.write(rest) {
        Ok(n) => {
            ps.written += n;
            *wrote |= n > 0;
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) => {}
        Err(_) => return Step::Drop(DropReason::Unwritable),
    }
    if ps.written >= wire.len() {
        // Only now is the send real. Enter waits for the last byte.
        ps.text_written_at = Some(now);
    } else if ps.written == 0 {
        // Nothing was taken: the entry has not started, and its form is
        // decided again when it does. (The mod may still collect it.)
        ps.wire = None;
    }
    Step::Keep
}

/// Report one dropped delivery: the bar, the audit log, and the sender.
#[allow(
    clippy::too_many_arguments,
    reason = "the session's parts a report touches, lent for one call"
)]
fn report_drop(
    target: AgentId,
    origin: SendOrigin,
    by: &SentBy,
    why: DropReason,
    pending: &mut Vec<PendingSend>,
    windows: &[Window],
    mods: &atrium::modstate::ModState,
    flash: &mut Option<(String, Instant)>,
    audit: &mut atrium::audit::Audit,
    bus: &mut atrium::bus::Bus,
    policy: atrium::ctl::TrustMode,
    now: Instant,
) {
    let kind = match origin {
        SendOrigin::Ctl => "send",
        SendOrigin::BusWake => "wake",
        SendOrigin::Command => "command",
    };
    let words = why.words(target);
    *flash = Some((format!("a {kind} was dropped: {words}"), now));
    let detail = format!("target={target} kind={kind}");
    match &by.operator {
        Some(label) => audit.record_as(label, "delivery-dropped", &detail, false, &words),
        None => audit.record(by.pane, "delivery-dropped", &detail, false, &words),
    };
    // The sender, when it is a live pane a line may be delivered to. Anyone
    // else (an operator, a pane that has exited, a shell) has the audit row.
    let Some(sender) = by.pane.and_then(|id| pane_by_agent(windows, id)) else {
        return;
    };
    let reachable = atrium::ctl::text_may_land(
        super::dispatch::is_agent_pane(sender),
        mods.has_cap(sender.agent_id.0, "inbox"),
        atrium::ctl::TextFrom::Pane,
        policy,
    );
    if !reachable {
        return;
    }
    let fields = vec![
        ("kind".to_string(), "delivery-dropped".to_string()),
        ("target".to_string(), target.to_string()),
        (
            "to".to_string(),
            super::panes::pane_label(sender.agent_id, sender.role.as_deref()),
        ),
        (
            "msg".to_string(),
            format!("your {kind} for pane {target} was dropped and nothing was typed: {words}"),
        ),
    ];
    let Ok(event) = bus.publish(
        "atrium",
        atrium::bus::Kind::Fyi,
        None,
        &fields,
        atrium::session_store::now_ms(),
    ) else {
        return;
    };
    // No sender: if this notice is dropped in turn, nothing is announced.
    queue_send(
        pending,
        sender.agent_id,
        super::wake::wake_text(&event, "atrium"),
        SendOrigin::BusWake,
        SentBy::default(),
    );
}

/// Hand `target` every delivery queued for it that the pty path has not begun
/// to type, oldest first, removing them from the queue: what `ctl inbox` gives
/// a pane's mod. A send already partly written stays on the pty path, so no
/// text ever lands twice.
/// Drop every delivery queued for `target`, written or not, without
/// counting it as delivered: what a teammate launch does for the
/// placeholder's id before a teammate takes it over. Returns how many.
pub(crate) fn drop_for(pending: &mut Vec<PendingSend>, target: AgentId) -> usize {
    let before = pending.len();
    pending.retain(|ps| ps.target != target);
    before - pending.len()
}

pub(crate) fn take_for(
    pending: &mut Vec<PendingSend>,
    target: AgentId,
) -> Vec<(SendOrigin, String)> {
    let mut taken = Vec::new();
    pending.retain(|ps| {
        if ps.target == target && ps.written == 0 {
            taken.push((ps.origin, ps.text.clone()));
            note_delivered(ps.origin);
            false
        } else {
            true
        }
    });
    taken
}

/// Bus wake lines handed to a pane this session: submitted into its pty, or
/// taken by its mod's `inbox`. What `ctl metrics` reports. One add per
/// delivered wake, at the moment the entry leaves the queue; nothing is
/// counted on a tick that delivers nothing.
static WAKES_DELIVERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_delivered(origin: SendOrigin) {
    if origin == SendOrigin::BusWake {
        WAKES_DELIVERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How many bus wake lines have been delivered since the session started.
pub(super) fn wakes_delivered() -> u64 {
    WAKES_DELIVERED.load(std::sync::atomic::Ordering::Relaxed)
}

/// A new session starts counting from zero.
pub(super) fn reset_wakes_delivered() {
    WAKES_DELIVERED.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Age every queued delivery by `by`, as if it had waited that long: its
/// time in the queue, the beat before its Enter, and the hold after an
/// earlier delivery's Enter.
#[cfg(test)]
pub(crate) fn backdate(pending: &mut [PendingSend], by: Duration) {
    for ps in pending {
        ps.queued_at = ps.queued_at.checked_sub(by).expect("a clock that old");
        ps.text_written_at = ps
            .text_written_at
            .map(|t| t.checked_sub(by).expect("a clock"));
        ps.not_before = ps.not_before.map(|t| t.checked_sub(by).expect("a clock"));
    }
}

/// Where each queued delivery stands, oldest first: `queued` (nothing
/// written), `writing` (some bytes), `typed` (all of it, Enter still to come).
#[cfg(test)]
pub(crate) fn stages(pending: &[PendingSend]) -> Vec<(AgentId, SendOrigin, &'static str)> {
    pending
        .iter()
        .map(|ps| {
            let stage = match (ps.text_written_at, &ps.wire) {
                (Some(_), _) => "typed",
                (None, Some(_)) => "writing",
                (None, None) => "queued",
            };
            (ps.target, ps.origin, stage)
        })
        .collect()
}

#[cfg(test)]
mod tests;
