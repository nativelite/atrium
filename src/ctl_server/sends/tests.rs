//! The send queue across respawns, its caps, and wake coalescing.

use super::*;

/// A respawn mints the pane a new agent id. Its workers name their parent by
/// id, so they must follow it, or a restarted lead below the root could no
/// longer steer the team it built.
#[test]
fn a_respawned_panes_workers_follow_it_to_its_new_id() {
    let (old, new, other) = (AgentId(4), AgentId(9), AgentId(2));
    let mut parents = vec![Some(old), Some(other), None, Some(old)];
    repoint_parents(parents.iter_mut(), old, new);
    assert_eq!(parents, vec![Some(new), Some(other), None, Some(new)]);
}

/// Sends queued for the old process go to the new one, from the start: a
/// fragment already written into the dead pty is not continued mid-text.
#[test]
fn sends_queued_for_a_respawned_pane_are_delivered_whole_to_its_new_process() {
    let (old, new, other) = (AgentId(4), AgentId(9), AgentId(2));
    let mut pending = Vec::new();
    assert!(queue_send(
        &mut pending,
        old,
        "resume from PLAN.md".into(),
        SendOrigin::Ctl
    ));
    assert!(queue_send(
        &mut pending,
        other,
        "keep going".into(),
        SendOrigin::Ctl
    ));
    pending[0].written = 6;
    pending[0].text_written_at = Some(Instant::now());
    let later = pending[0].queued_at + SEND_UNBOUND_FALLBACK;
    retarget_sends(&mut pending, old, new, later);
    assert_eq!(pending[0].target, new);
    assert_eq!(pending[0].queued_at, later);
    assert_eq!(pending[0].written, 0);
    assert!(pending[0].text_written_at.is_none());
    assert_eq!(pending[1].target, other);
}

#[test]
fn a_target_that_never_takes_delivery_cannot_grow_the_queue_without_bound() {
    // A pane wedged on an open dialog never becomes ready, so its sends are
    // never drained; each one can carry a max-size ctl request. The queue per
    // target is capped, and one wedged target doesn't block the others.
    let mut pending = Vec::new();
    let wedged = AgentId(1);
    for _ in 0..MAX_PENDING_PER_TARGET {
        assert!(queue_send(
            &mut pending,
            wedged,
            "task".into(),
            SendOrigin::Ctl
        ));
    }
    assert!(!queue_send(
        &mut pending,
        wedged,
        "one too many".into(),
        SendOrigin::Ctl
    ));
    assert_eq!(pending.len(), MAX_PENDING_PER_TARGET);
    assert!(queue_send(
        &mut pending,
        AgentId(2),
        "other".into(),
        SendOrigin::Ctl
    ));
}

#[test]
fn bus_wakes_have_their_own_cap_and_never_consume_send_slots() {
    let mut pending = Vec::new();
    let t = AgentId(7);
    // The first wake queues; the rest coalesce into it while it is unwritten.
    for i in 0..20 {
        assert!(queue_send(
            &mut pending,
            t,
            format!("w{i}"),
            SendOrigin::BusWake
        ));
    }
    assert_eq!(pending.len(), 1, "coalesced");
    assert!(pending[0].text.starts_with("w0 | w1 | w2"));
    // Once delivery has begun, new wakes queue behind it, up to their cap
    // (the one in flight counts).
    pending[0].written = 1;
    for _ in 1..MAX_PENDING_WAKES_PER_TARGET {
        pending.last_mut().unwrap().written = 1;
        assert!(queue_send(
            &mut pending,
            t,
            "later".into(),
            SendOrigin::BusWake
        ));
    }
    pending.last_mut().unwrap().written = 1;
    assert!(!queue_send(
        &mut pending,
        t,
        "one too many".into(),
        SendOrigin::BusWake
    ));
    // A send is still accepted: wakes do not count against it.
    assert!(queue_send(
        &mut pending,
        t,
        "operator".into(),
        SendOrigin::Ctl
    ));
}

#[test]
fn a_coalesced_wake_stops_growing_at_the_limit_and_points_at_the_feed() {
    let mut pending = Vec::new();
    let t = AgentId(1);
    let big = "x".repeat(WAKE_COALESCE_CHARS - 10);
    assert!(queue_send(&mut pending, t, big, SendOrigin::BusWake));
    assert!(queue_send(
        &mut pending,
        t,
        "y".repeat(50),
        SendOrigin::BusWake
    ));
    assert!(queue_send(
        &mut pending,
        t,
        "z".repeat(50),
        SendOrigin::BusWake
    ));
    assert_eq!(pending.len(), 1);
    assert!(pending[0].text.ends_with("+more: atrium ctl bus feed"));
    assert!(pending[0].text.chars().count() < WAKE_COALESCE_CHARS + 64);
    assert_eq!(pending[0].text.matches("+more").count(), 1);
}

/// `ctl inbox` takes a pane's queued deliveries, oldest first, sends and
/// wakes alike, and leaves every other pane's and anything the pty path has
/// begun to type.
#[test]
fn take_for_hands_a_pane_its_untyped_deliveries_and_nothing_else() {
    let mut pending = Vec::new();
    assert!(queue_send(
        &mut pending,
        AgentId(1),
        "first".to_string(),
        SendOrigin::Ctl
    ));
    assert!(queue_send(
        &mut pending,
        AgentId(2),
        "other".to_string(),
        SendOrigin::Ctl
    ));
    assert!(queue_send(
        &mut pending,
        AgentId(1),
        "[atrium bus #1 fyi] second".to_string(),
        SendOrigin::BusWake
    ));
    pending[0].written = 2; // the pty path began typing this one
    let taken = take_for(&mut pending, AgentId(1));
    assert_eq!(
        taken,
        vec![(
            SendOrigin::BusWake,
            "[atrium bus #1 fyi] second".to_string()
        )]
    );
    assert_eq!(
        pending.len(),
        2,
        "the partly typed send and the other pane's stay"
    );
    assert!(take_for(&mut pending, AgentId(3)).is_empty());
    pending[0].written = 0;
    assert_eq!(take_for(&mut pending, AgentId(1)).len(), 1);
    assert_eq!(pending.len(), 1);
}

/// A pane whose app turned bracketed paste on gets a send as a paste, newlines
/// inside and all; one that did not, and every Windows pane, gets the bare
/// text. The Enter is never part of the bytes: it follows after the beat.
#[test]
fn a_send_is_typed_as_a_paste_only_into_a_pane_that_asked_for_paste_mode() {
    let text = "line one\nline two";
    assert_eq!(typed_bytes(text, false), text.as_bytes());
    let pasted = typed_bytes(text, true);
    if cfg!(unix) {
        assert_eq!(pasted, b"\x1b[200~line one\nline two\x1b[201~");
    } else {
        assert_eq!(pasted, text.as_bytes());
    }
    assert!(!pasted.ends_with(b"\r"));
}

/// An unbound codex or claude pane is typed into once its screen shows the
/// shortcuts hint, however it is styled; a loading screen is not enough,
/// until the prompt wait runs out. A shell has no hint and is never held.
#[test]
fn an_agent_pane_is_held_until_its_prompt_hint_is_on_screen() {
    let mut loading = vterm::Term::new(6, 40);
    loading.feed(b">_ OpenAI Codex (v0.0.0)\r\n   loading\r\n");
    let mut prompt = vterm::Term::new(6, 40);
    prompt.feed(b"\x1b[1m>_ OpenAI Codex\x1b[0m\r\n\x1b[2m\xe2\x80\xba\x1b[0m Ask Codex to do anything\r\n  \x1b[2m? for shortcuts\x1b[0m\r\n");
    assert!(!screen_has(loading.screen(), "? for shortcuts"));
    assert!(screen_has(prompt.screen(), "? for shortcuts"));
    let soon = Duration::from_secs(3);
    for title in ["codex", "claude"] {
        assert!(!prompt_up(title, loading.screen(), soon), "{title}");
        assert!(prompt_up(title, prompt.screen(), soon), "{title}");
        assert!(
            prompt_up(title, loading.screen(), SEND_PROMPT_WAIT),
            "{title}"
        );
    }
    assert!(prompt_up("bash", loading.screen(), Duration::ZERO));
}
