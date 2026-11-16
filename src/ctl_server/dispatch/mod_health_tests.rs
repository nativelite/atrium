//! The mod's health through the control plane (PLAN 0.48.0, S1 and S2): what
//! a pane's mod may say of its own failures and what that can do, who may say
//! it at all, and that a pane whose mod is gone, or was set aside, is a pane
//! text is typed into, against real panes.

use super::super::panes::pane_by_agent_mut;
use super::super::sends::{backdate, flush_sends, stages};
use super::operator_tests::{err, ok, op, operators, s, Fx, BRIDGE};
use super::*;
use atrium::modstate::{HealthEvent, PaneSeen, HELLO_WINDOW_MS, MOD_SET_ASIDE};
use json::Value;
use std::time::Duration;

fn as_pane(token: &str, body: &str) -> String {
    format!(r#"{{"token":"{token}",{body}}}"#)
}

fn call(fx: &mut Fx, token: &str, body: &str) -> Value {
    fx.apply(&operators(&[]), &as_pane(token, body))
}

const HELLO: &str = r#""cmd":"hello","mod":"0.3.0","caps":"status,inbox,answer""#;
/// Five failures of its own hooks: what sets a pane's mod aside.
const FIVE: &str = r#""cmd":"report","fail":"tool.call:5","fail_class":"hook","fail_at":"tool.call","fail_thrown":5"#;

/// Health ticks a second apart over `[from, to]`, every live pane seen started.
fn ticks(fx: &mut Fx, from: u64, to: u64) -> Vec<HealthEvent> {
    let live: Vec<PaneSeen> = fx
        .windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| PaneSeen {
            pane: p.agent_id.0,
            started: true,
            root: p.agent_id.0,
        })
        .collect();
    let mut all = Vec::new();
    let mut t = from;
    while t <= to {
        all.extend(fx.mods.health_tick(t, &live));
        t += 1_000;
    }
    all
}

fn tripped(events: &[HealthEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, HealthEvent::Tripped { .. }))
}

/// One pass of the delivery flush, with every pane drawn and quiet and every
/// queued entry old enough to be due.
fn flush(fx: &mut Fx) {
    for p in fx.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
        p.painted = true;
        p.last_activity = Instant::now()
            .checked_sub(Duration::from_secs(5))
            .expect("a clock that old");
    }
    backdate(&mut fx.pending, Duration::from_secs(10));
    flush_sends(
        &mut fx.pending,
        &mut fx.windows,
        &fx.world,
        &fx.mods,
        &mut fx.flash,
        &mut fx.audit,
        &mut fx.bus,
    );
}

fn send(fx: &mut Fx, from: &str, to: AgentId, text: &str) -> Value {
    call(
        fx,
        from,
        &format!(r#""cmd":"send","target":"{to}","text":"{text}""#),
    )
}

/// A claude pane atrium launched with the mod, which said hello.
fn modded(fx: &mut Fx, role: &str, parent: Option<AgentId>) -> (AgentId, String) {
    let (id, tok) = fx.agent_pane(role, parent);
    fx.mods.expect_mod(id.0);
    let hello = call(fx, &tok, HELLO);
    assert!(ok(&hello), "{hello}");
    (id, tok)
}

fn status_of(fx: &mut Fx, as_token: &str, pane: AgentId) -> Value {
    call(
        fx,
        as_token,
        &format!(r#""cmd":"status","target":"{pane}""#),
    )
}

#[test]
fn a_pane_launched_with_the_mod_that_never_says_hello_gets_its_send_typed() {
    let mut fx = Fx::new();
    let (_lead, lead_tok) = fx.pane("lead", None);
    let (worker, _) = fx.agent_pane("worker", None);
    // Launched with the mod; its hello never comes.
    fx.mods.expect_mod(worker.0);
    let sent = send(&mut fx, &lead_tok, worker, "run the tests");
    assert!(ok(&sent), "{sent}");
    assert_eq!(stages(&fx.pending).len(), 1);
    // No hello, no inbox cap: nothing waits on a mod. The send is typed at
    // the first flush that finds the pane ready, and submitted by the next.
    flush(&mut fx);
    assert_eq!(stages(&fx.pending)[0].2, "typed");
    flush(&mut fx);
    assert!(fx.pending.is_empty(), "delivered, not lost");
    // The window runs out: the pane is set aside.
    let events = ticks(&mut fx, 0, HELLO_WINDOW_MS);
    assert!(
        events.contains(&HealthEvent::SetAside {
            pane: worker.0,
            why: atrium::modstate::Aside::NoHello
        }),
        "{events:?}"
    );
}

#[test]
fn a_send_waiting_on_a_mod_that_is_set_aside_is_typed_not_lost() {
    let mut fx = Fx::new();
    let (_lead, lead_tok) = fx.pane("lead", None);
    let (worker, worker_tok) = modded(&mut fx, "worker", None);
    assert!(fx.mods.has_cap(worker.0, "inbox"));
    // Its mod reported `working` a moment ago.
    assert!(ok(&call(
        &mut fx,
        &worker_tok,
        r#""cmd":"report","status":"working""#
    )));
    assert!(ok(&send(&mut fx, &lead_tok, worker, "run the tests")));
    // The mod collects its own deliveries: atrium types nothing.
    flush(&mut fx);
    flush(&mut fx);
    assert_eq!(stages(&fx.pending)[0].2, "queued");
    // Its mod reports that its hooks keep failing.
    let told = call(&mut fx, &worker_tok, FIVE);
    assert!(err(&told).starts_with(MOD_SET_ASIDE), "{told}");
    assert!(!fx.mods.has_cap(worker.0, "inbox"));
    // The status its mod held is dropped at once, not in two minutes.
    let seen = status_of(&mut fx, &lead_tok, worker);
    assert_eq!(seen.get("status"), Some(&Value::Null), "{seen}");
    // The same queue entry is now the pty path's.
    flush(&mut fx);
    assert_eq!(stages(&fx.pending)[0].2, "typed");
    flush(&mut fx);
    assert!(fx.pending.is_empty(), "delivered, not lost");
    // And its inbox is refused in words the mod reads as "stop asking".
    let inbox = call(&mut fx, &worker_tok, r#""cmd":"inbox""#);
    assert!(err(&inbox).starts_with(MOD_SET_ASIDE), "{inbox}");
    // A bare status is still taken: atrium's status hooks send one through
    // this verb with no mod at all, and they are this pane's status now.
    let hooked = call(
        &mut fx,
        &worker_tok,
        r#""cmd":"report","status":"waiting-approval""#,
    );
    assert!(ok(&hooked), "{hooked}");
    let seen = status_of(&mut fx, &lead_tok, worker);
    assert_eq!(s(&seen, "status"), "waiting-approval", "{seen}");
    // What only a mod sends is the mod still talking: refused.
    let caption = call(
        &mut fx,
        &worker_tok,
        r#""cmd":"report","status":"working","doing":"x""#,
    );
    assert!(err(&caption).starts_with(MOD_SET_ASIDE), "{caption}");
    // A hello after that is granted nothing.
    let again = call(&mut fx, &worker_tok, HELLO);
    assert!(ok(&again), "{again}");
    assert_eq!(
        again
            .get("accepted")
            .and_then(Value::as_array)
            .map(<[Value]>::len),
        Some(0),
        "{again}"
    );
}

#[test]
fn a_mod_that_lost_its_inbox_keeps_the_rest_and_its_queue_is_typed() {
    let mut fx = Fx::new();
    let (_lead, lead_tok) = fx.pane("lead", None);
    let (worker, worker_tok) = modded(&mut fx, "worker", None);
    assert!(ok(&send(&mut fx, &lead_tok, worker, "run the tests")));
    flush(&mut fx);
    assert_eq!(stages(&fx.pending)[0].2, "queued");
    // This Claude Code has no `$.prompt.submit`: one feature is gone.
    let told = call(
        &mut fx,
        &worker_tok,
        r#""cmd":"report","fail":"feature.inbox:1","fail_class":"api","fail_at":"feature.inbox","fail_msg":"HooksError: no implementation for prompt.submit""#,
    );
    assert!(ok(&told), "one lost feature is not a broken mod: {told}");
    assert_eq!(fx.mods.aside(worker.0), None);
    assert!(!fx.mods.has_cap(worker.0, "inbox"));
    assert!(fx.mods.has_cap(worker.0, "status") && fx.mods.has_cap(worker.0, "answer"));
    // What was queued for the inbox is typed.
    flush(&mut fx);
    assert_eq!(stages(&fx.pending)[0].2, "typed");
    // Its status and its answer still count, and `status` says what it lost.
    assert!(ok(&call(
        &mut fx,
        &worker_tok,
        r#""cmd":"report","status":"working","answer":"done""#
    )));
    let seen = status_of(&mut fx, &lead_tok, worker);
    let m = seen.get("mod").expect("its mod is shown");
    assert_eq!(s(m, "state"), "ok", "{seen}");
    assert_eq!(
        m.get("lost").and_then(Value::as_array).map(<[Value]>::len),
        Some(1),
        "{seen}"
    );
    assert_eq!(s(&seen, "status"), "working");
}

#[test]
fn a_mod_that_stops_collecting_gives_its_queue_back_to_the_pty_path() {
    let mut fx = Fx::new();
    let (_lead, lead_tok) = fx.pane("lead", None);
    let (worker, worker_tok) = modded(&mut fx, "worker", None);
    assert!(ok(&send(&mut fx, &lead_tok, worker, "run the tests")));
    // It never asks for its inbox. Three minutes on, the cap is taken back.
    let events = ticks(&mut fx, 0, atrium::modstate::INBOX_SILENT_MS + 3_000);
    assert!(
        events.contains(&HealthEvent::InboxLost { pane: worker.0 }),
        "{events:?}"
    );
    flush(&mut fx);
    assert_eq!(stages(&fx.pending)[0].2, "typed");
    // Its status reports still count: only the inbox is gone.
    let reported = call(&mut fx, &worker_tok, r#""cmd":"report","status":"working""#);
    assert!(ok(&reported), "{reported}");
}

#[test]
fn only_a_pane_that_has_a_mod_may_report_a_mods_failures() {
    let mut fx = Fx::new();
    let (_lead, lead_tok) = fx.pane("lead", None);
    const LIE: &str =
        r#""cmd":"report","fail":"session.start:9","fail_class":"api","fail_at":"session.start""#;
    // A shell; a claude pane atrium launched without the mod; a codex pane.
    let (shell, shell_tok) = fx.pane("shell", None);
    let (bare, bare_tok) = fx.agent_pane("bare-claude", None);
    let (codex, codex_tok) = fx.agent_pane("codex", None);
    pane_by_agent_mut(&mut fx.windows, codex)
        .expect("pane")
        .argv = vec!["codex".to_string()];
    for (id, tok) in [(shell, &shell_tok), (bare, &bare_tok), (codex, &codex_tok)] {
        for _ in 0..3 {
            let v = call(&mut fx, tok, LIE);
            assert!(err(&v).contains("has no mod"), "{v}");
        }
        assert_eq!(fx.mods.failures_for(id.0), None);
        assert_eq!(fx.mods.aside(id.0), None);
        let seen = status_of(&mut fx, &lead_tok, id);
        assert!(seen.get("mod").is_none(), "{seen}");
        // Its `answer` says the same, which is how a parent's mod knows no
        // answer will ever come from it.
        let answer = call(
            &mut fx,
            &lead_tok,
            &format!(r#""cmd":"answer","target":"{id}""#),
        );
        assert!(ok(&answer) && answer.get("mod").is_none(), "{answer}");
        // The rest of such a request is refused with it: nothing changes.
        let v = call(
            &mut fx,
            tok,
            r#""cmd":"report","status":"working","fail":"x:1""#,
        );
        assert!(!ok(&v), "{v}");
        let seen = status_of(&mut fx, &lead_tok, id);
        assert_eq!(seen.get("status"), Some(&Value::Null), "{seen}");
    }
    // An operator is nobody's mod: `report` is not a verb it has.
    let v = fx.apply(&operators(&[]), &op(BRIDGE, LIE));
    assert!(!ok(&v), "{v}");
    // Three panes, nine reports: no counter moved, nothing tripped.
    let metrics = fx.apply(&operators(&[]), r#"{"cmd":"metrics"}"#);
    assert_eq!(metrics.get("mod_failures").and_then(Value::as_i64), Some(0));
    let events = ticks(&mut fx, 0, 5_000);
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(fx.mods.tripped(), None);
}

#[test]
fn reported_failures_show_in_status_answer_and_metrics_and_touch_no_other_pane() {
    let mut fx = Fx::new();
    let (_lead, lead_tok) = fx.pane("lead", None);
    let (a, a_tok) = modded(&mut fx, "a", None);
    let (b, b_tok) = modded(&mut fx, "b", None);
    // A hostile message: escapes, a bidi override, far too long.
    let msg = format!("Error: boom \\u001b[31m\\u202e {}", "x".repeat(600));
    let told = call(
        &mut fx,
        &a_tok,
        &format!(
            r#""cmd":"report","status":"working","fail":"tool.call:3,whoami:1","fail_class":"hook","fail_thrown":4,"fail_msg":"{msg}""#
        ),
    );
    assert!(ok(&told), "four failures are counted, not acted on: {told}");
    let status = status_of(&mut fx, &lead_tok, a);
    let m = status.get("mod").expect("the pane's mod is shown");
    assert_eq!(s(m, "state"), "ok");
    assert_eq!(m.get("failures").and_then(Value::as_i64), Some(4));
    assert_eq!(s(m, "class"), "hook");
    assert_eq!(
        m.get("hooks")
            .and_then(|h| h.get("tool.call"))
            .and_then(Value::as_i64),
        Some(3)
    );
    let shown = s(m, "error");
    assert!(
        shown.chars().count() <= atrium::modstate::FAIL_MSG_CAP,
        "{shown}"
    );
    assert!(
        !shown.contains('\u{1b}') && !shown.contains('\u{202e}'),
        "{shown:?}"
    );
    // `answer` carries the same.
    let answer = call(
        &mut fx,
        &lead_tok,
        &format!(r#""cmd":"answer","target":"{a}""#),
    );
    assert_eq!(
        answer
            .get("mod")
            .and_then(|m| m.get("failures"))
            .and_then(Value::as_i64),
        Some(4)
    );
    // The counter.
    let metrics = fx.apply(&operators(&[]), r#"{"cmd":"metrics"}"#);
    assert_eq!(metrics.get("mod_failures").and_then(Value::as_i64), Some(4));
    assert_eq!(metrics.get("mod_tripped").and_then(Value::as_i64), Some(0));
    // The fifth failure sets pane a's mod aside; pane b keeps everything.
    let fifth = call(
        &mut fx,
        &a_tok,
        r#""cmd":"report","fail":"tool.call:4,whoami:1","fail_thrown":5"#,
    );
    assert!(err(&fifth).starts_with(MOD_SET_ASIDE), "{fifth}");
    let status = status_of(&mut fx, &lead_tok, a);
    let m = status.get("mod").expect("shown");
    assert_eq!(s(m, "state"), "set-aside");
    assert_eq!(s(m, "why"), "it reported repeated failures");
    assert!(fx.mods.has_cap(b.0, "inbox"));
    assert!(ok(&call(
        &mut fx,
        &b_tok,
        r#""cmd":"report","status":"working""#
    )));
    // However much pane a goes on to claim, the breaker hears one voice.
    for _ in 0..20 {
        let _ = call(
            &mut fx,
            &a_tok,
            r#""cmd":"report","fail":"x:1000000","fail_class":"api","fail_at":"hello","fail_msg":"the interface changed, switch it off for everyone""#,
        );
    }
    let events = ticks(&mut fx, 0, 5_000);
    assert!(!tripped(&events), "{events:?}");
    assert_eq!(fx.mods.tripped(), None);
    // The set-aside is on record.
    assert!(fx.log().iter().any(|e| s(e, "action") == "mod-set-aside"));
}

#[test]
fn a_child_lying_with_its_parent_is_the_same_voice_and_a_stranger_is_a_second() {
    let mut fx = Fx::new();
    let (parent, parent_tok) = modded(&mut fx, "parent", None);
    let (_child, child_tok) = modded(&mut fx, "child", Some(parent));
    let (_other, other_tok) = modded(&mut fx, "other", None);
    let _ = call(&mut fx, &parent_tok, FIVE);
    let _ = call(&mut fx, &child_tok, FIVE);
    let events = ticks(&mut fx, 0, 2_000);
    assert!(!tripped(&events), "{events:?}");
    // A pane the parent did not spawn says the same: two voices. Enough for
    // this session, and never for the next: it is their word, not atrium's.
    let _ = call(&mut fx, &other_tok, FIVE);
    let events = ticks(&mut fx, 3_000, 4_000);
    assert!(
        events.iter().any(|e| matches!(
            e,
            HealthEvent::Tripped {
                remember: false,
                ..
            }
        )),
        "{events:?}"
    );
    // Every pane's mod learns it: the session's mod is backed out.
    let who = call(&mut fx, &other_tok, r#""cmd":"whoami""#);
    assert_eq!(who.get("mod_tripped"), Some(&Value::Bool(true)), "{who}");
}

#[test]
fn a_pane_respawned_through_ctl_is_the_voice_it_was() {
    let mut fx = Fx::new();
    let (_lead, lead_tok) = fx.pane("lead", None);
    let (worker, worker_tok) = fx.agent_pane("worker", None);
    let who = call(&mut fx, &worker_tok, r#""cmd":"whoami""#);
    assert_eq!(who.get("mod_tripped"), Some(&Value::Bool(false)), "{who}");
    // A shell stands in for the claude, so the respawn has something to run.
    let shell = pane_by_agent_mut(&mut fx.windows, worker)
        .expect("the pane")
        .title
        .clone();
    pane_by_agent_mut(&mut fx.windows, worker)
        .expect("the pane")
        .argv = vec![shell];
    let mut id = worker;
    let mut t = 0;
    // Launched with the mod, it misses its window; the human respawns it;
    // it misses again; and again. One seat, one voice.
    for round in 0..3 {
        fx.mods.expect_mod(id.0);
        let events = ticks(&mut fx, t, t + HELLO_WINDOW_MS + 1_000);
        assert!(fx.mods.aside(id.0).is_some(), "round {round}: {events:?}");
        assert!(!tripped(&events), "round {round}: {events:?}");
        t += HELLO_WINDOW_MS + 2_000;
        let v = call(
            &mut fx,
            &lead_tok,
            &format!(r#""cmd":"respawn","target":"{id}""#),
        );
        assert!(ok(&v), "{v}");
        let new_id = AgentId(v.get("pane").and_then(Value::as_i64).expect("a pane") as usize);
        assert_ne!(new_id, id);
        assert_eq!(fx.mods.aside(new_id.0), None, "a new process starts clean");
        id = new_id;
    }
    assert_eq!(fx.mods.tripped(), None);
    // A second seat missing its window is the second voice, and atrium saw
    // both itself: this one is remembered.
    let (other, _) = fx.agent_pane("other", None);
    fx.mods.expect_mod(other.0);
    let events = ticks(&mut fx, t, t + HELLO_WINDOW_MS + 1_000);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HealthEvent::Tripped { remember: true, .. })),
        "{events:?}"
    );
}

#[test]
fn a_malformed_failure_report_is_refused_whole() {
    let mut fx = Fx::new();
    let (a, a_tok) = modded(&mut fx, "a", None);
    for body in [
        r#""cmd":"report","fail":"no-count""#,
        r#""cmd":"report","fail":"a b:1""#,
        r#""cmd":"report","fail":["a:1"]"#,
        r#""cmd":"report","fail":"a:1","fail_class":"catastrophe""#,
        r#""cmd":"report","fail":"a:1","fail_msg":7"#,
        r#""cmd":"report","fail":"a:1","fail_at":"a b""#,
        r#""cmd":"report","fail_class":"api""#,
        r#""cmd":"report","fail_at":"hello""#,
        r#""cmd":"report","fail_msg":"only words""#,
    ] {
        let v = call(&mut fx, &a_tok, body);
        assert!(!ok(&v), "{body} -> {v}");
    }
    assert_eq!(fx.mods.failures_for(a.0), None);
    assert_eq!(fx.mods.aside(a.0), None);
    // Nobody reports for someone else: the pane is the token's.
    let (b, _) = modded(&mut fx, "b", None);
    let v = call(
        &mut fx,
        &a_tok,
        &format!(r#""cmd":"report","target":"{b}","fail":"a:9","fail_class":"api""#),
    );
    assert!(!ok(&v), "{v}");
    assert_eq!(fx.mods.aside(b.0), None);
}

#[test]
fn a_delivery_whose_reply_never_arrived_is_queued_again() {
    let mut fx = Fx::new();
    let (worker, _) = modded(&mut fx, "worker", None);
    // What a held `inbox --wait` was answered with, on a connection that was
    // already gone: a send, a wake, and a question.
    let items = vec![
        ctl::InboxItem {
            kind: "send".to_string(),
            text: "[atrium: from pane 0] run the tests".to_string(),
            id: None,
        },
        ctl::InboxItem {
            kind: "wake".to_string(),
            text: "[atrium bus #4] work: done".to_string(),
            id: None,
        },
        ctl::InboxItem {
            kind: "ask".to_string(),
            text: "what are you doing".to_string(),
            id: Some(3),
        },
    ];
    assert_eq!(requeue_inbox(&mut fx.pending, worker, &items), 2);
    // The mod's next poll collects them, in order, as it would have.
    let again = inbox_items(worker, &mut fx.pending, &mut fx.mods);
    assert_eq!(
        again
            .iter()
            .map(|i| (i.kind.as_str(), i.text.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("send", "[atrium: from pane 0] run the tests"),
            ("wake", "[atrium bus #4] work: done"),
        ]
    );
    // And if the mod has lost its inbox by then, they are typed.
    assert_eq!(requeue_inbox(&mut fx.pending, worker, &items), 2);
    let _ = ticks(&mut fx, 0, atrium::modstate::INBOX_SILENT_MS + 3_000);
    flush(&mut fx);
    assert_eq!(stages(&fx.pending)[0].2, "typed");
}

#[test]
fn a_hello_is_taken_only_from_a_pane_that_could_have_a_mod() {
    // Whose hello it is, by the three things the broker knows.
    for (expects, session_injects, claude, want) in [
        // atrium launched it with the mod: taken, whatever else.
        (true, true, true, HelloFrom::Launched),
        (true, false, true, HelloFrom::Launched),
        // A claude in a session that injects no mod: loaded by hand.
        (false, false, true, HelloFrom::HandLoaded),
        // A claude atrium launched WITHOUT the mod in a session that does
        // inject it (the breaker tripped, the folder was not loadable).
        (false, true, true, HelloFrom::Nobody),
        // A shell, another agent, a placeholder: never.
        (false, false, false, HelloFrom::Nobody),
        (false, true, false, HelloFrom::Nobody),
    ] {
        assert_eq!(
            hello_from(expects, session_injects, claude),
            want,
            "expects={expects} injects={session_injects} claude={claude}"
        );
    }
    // Through the verb: a shell and a codex pane say hello by hand.
    let mut fx = Fx::new();
    let (shell, shell_tok) = fx.pane("shell", None);
    let (codex, codex_tok) = fx.agent_pane("codex", None);
    pane_by_agent_mut(&mut fx.windows, codex)
        .expect("pane")
        .argv = vec!["codex".to_string()];
    for (id, tok) in [(shell, &shell_tok), (codex, &codex_tok)] {
        let v = call(&mut fx, tok, HELLO);
        assert!(err(&v).contains("its hello is not taken"), "{v}");
        // Final: a mod that somehow ran there stops, it does not retry.
        assert_eq!(v.get("final"), Some(&Value::Bool(true)), "{v}");
        assert!(!fx.mods.has_a_mod(id.0));
        assert!(!fx.mods.has_cap(id.0, "inbox"));
        // And so the request after it changes nothing either.
        let v = call(&mut fx, tok, FIVE);
        assert!(err(&v).contains("has no mod"), "{v}");
        assert_eq!(fx.mods.aside(id.0), None);
    }
    let events = ticks(&mut fx, 0, 3_000);
    assert!(events.is_empty(), "{events:?}");
    // A claude pane in this session (which injects no mod) loads one by
    // hand: taken, and it is nobody's voice and no evidence.
    // (The session-wide fact is a process global: skipped if some other
    // test in this process launched a pane with the mod.)
    if atrium::modstate::ever_injected() {
        return;
    }
    let (a, a_tok) = fx.agent_pane("a", None);
    let (_b, b_tok) = fx.agent_pane("b", None);
    assert!(ok(&call(&mut fx, &a_tok, HELLO)));
    assert!(ok(&call(&mut fx, &b_tok, HELLO)));
    assert!(fx.mods.has_cap(a.0, "inbox"));
    let _ = call(&mut fx, &a_tok, FIVE);
    let _ = call(&mut fx, &b_tok, FIVE);
    let events = ticks(&mut fx, 4_000, 8_000);
    assert!(!tripped(&events), "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HealthEvent::Healthy { .. })),
        "{events:?}"
    );
}

#[test]
fn the_set_aside_refusal_is_final_and_any_other_refusal_is_not() {
    let mut fx = Fx::new();
    let (worker, worker_tok) = modded(&mut fx, "worker", None);
    // An ordinary refusal: no marker, so the mod may ask again.
    let v = call(
        &mut fx,
        &worker_tok,
        r#""cmd":"report","ask":99,"reply":"x""#,
    );
    assert!(!ok(&v) && v.get("final").is_none(), "{v}");
    let v = call(&mut fx, &worker_tok, FIVE);
    assert_eq!(v.get("final"), Some(&Value::Bool(true)), "{v}");
    let v = call(&mut fx, &worker_tok, r#""cmd":"inbox""#);
    assert_eq!(v.get("final"), Some(&Value::Bool(true)), "{v}");
    assert!(fx.mods.aside(worker.0).is_some());
}

#[test]
fn a_held_reply_that_could_not_be_written_is_queued_again_and_recorded() {
    let mut fx = Fx::new();
    let (worker, _) = modded(&mut fx, "worker", None);
    let items = vec![ctl::InboxItem {
        kind: "send".to_string(),
        text: "[atrium: from pane 0] run the tests".to_string(),
        id: None,
    }];
    // What the run loop calls when `respond_parked` fails.
    let back = held_reply_lost(
        &mut fx.pending,
        &mut fx.audit,
        worker,
        &items,
        "the client left",
    );
    assert_eq!(back, 1);
    assert_eq!(stages(&fx.pending).len(), 1, "back in the queue");
    let row = fx
        .log()
        .into_iter()
        .rev()
        .find(|e| s(e, "action") == "inbox")
        .expect("recorded");
    assert!(
        row.to_string().contains("1 queued again") && row.to_string().contains("the client left"),
        "{row}"
    );
}
