//! The mod's health through the control plane (PLAN 0.48.0, S1 and S2): what
//! a pane's mod may say of its own failures and what that can do, and that a
//! pane whose mod is gone, or was set aside, is a pane text is typed into,
//! against real panes.

use super::super::panes::pane_by_agent_mut;
use super::super::sends::{backdate, flush_sends, stages};
use super::operator_tests::{err, ok, operators, s, Fx};
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

/// Health ticks a second apart over `[from, to]`, every live pane seen started.
fn ticks(fx: &mut Fx, from: u64, to: u64) -> Vec<HealthEvent> {
    let live: Vec<PaneSeen> = fx
        .windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| PaneSeen {
            pane: p.agent_id.0,
            started: true,
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
    // The window runs out: the pane is set aside, and stays a typed pane
    // even if a hello arrives after all.
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
    let (worker, worker_tok) = fx.agent_pane("worker", None);
    let hello = call(&mut fx, &worker_tok, HELLO);
    assert!(ok(&hello), "{hello}");
    assert!(fx.mods.has_cap(worker.0, "inbox"));
    assert!(ok(&send(&mut fx, &lead_tok, worker, "run the tests")));
    // The mod collects its own deliveries: atrium types nothing.
    flush(&mut fx);
    flush(&mut fx);
    assert_eq!(stages(&fx.pending)[0].2, "queued");
    // Its mod reports that the plugin interface changed under it.
    let told = call(
        &mut fx,
        &worker_tok,
        r#""cmd":"report","fail":"feature.inbox:1","fail_class":"api","fail_msg":"HooksError: no implementation for prompt.submit""#,
    );
    assert!(err(&told).starts_with(MOD_SET_ASIDE), "{told}");
    assert!(!fx.mods.has_cap(worker.0, "inbox"));
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
    let seen = call(
        &mut fx,
        &lead_tok,
        &format!(r#""cmd":"status","target":"{worker}""#),
    );
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
fn a_mod_that_stops_collecting_gives_its_queue_back_to_the_pty_path() {
    let mut fx = Fx::new();
    let (_lead, lead_tok) = fx.pane("lead", None);
    let (worker, worker_tok) = fx.agent_pane("worker", None);
    assert!(ok(&call(&mut fx, &worker_tok, HELLO)));
    assert!(ok(&send(&mut fx, &lead_tok, worker, "run the tests")));
    let hello_at = atrium::session_store::now_ms();
    // It never asks for its inbox. Three minutes on, the cap is taken back.
    let events = ticks(
        &mut fx,
        hello_at,
        hello_at + atrium::modstate::INBOX_SILENT_MS + 3_000,
    );
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
fn reported_failures_show_in_status_answer_and_metrics_and_touch_no_other_pane() {
    let mut fx = Fx::new();
    let (_lead, lead_tok) = fx.pane("lead", None);
    let (a, a_tok) = fx.agent_pane("a", None);
    let (b, b_tok) = fx.agent_pane("b", None);
    assert!(ok(&call(&mut fx, &a_tok, HELLO)));
    assert!(ok(&call(&mut fx, &b_tok, HELLO)));
    // A hostile message: escapes, a bidi override, far too long.
    let msg = format!("Error: boom \\u001b[31m\\u202e {}", "x".repeat(600));
    let told = call(
        &mut fx,
        &a_tok,
        &format!(
            r#""cmd":"report","status":"working","fail":"tool.call:3,whoami:1","fail_class":"hook","fail_msg":"{msg}""#
        ),
    );
    assert!(ok(&told), "four failures are counted, not acted on: {told}");
    let status = call(
        &mut fx,
        &lead_tok,
        &format!(r#""cmd":"status","target":"{a}""#),
    );
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
    // A pane with no mod has no `mod` key.
    let (plain, _) = fx.pane("shell", None);
    let none = call(
        &mut fx,
        &lead_tok,
        &format!(r#""cmd":"status","target":"{plain}""#),
    );
    assert!(none.get("mod").is_none(), "{none}");
    // The counter.
    let metrics = fx.apply(&operators(&[]), r#"{"cmd":"metrics"}"#);
    assert_eq!(metrics.get("mod_failures").and_then(Value::as_i64), Some(4));
    assert_eq!(metrics.get("mod_tripped").and_then(Value::as_i64), Some(0));
    // The fifth failure sets pane a's mod aside; pane b keeps everything.
    let fifth = call(
        &mut fx,
        &a_tok,
        r#""cmd":"report","fail":"tool.call:4,whoami:1""#,
    );
    assert!(err(&fifth).starts_with(MOD_SET_ASIDE), "{fifth}");
    let status = call(
        &mut fx,
        &lead_tok,
        &format!(r#""cmd":"status","target":"{a}""#),
    );
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
            r#""cmd":"report","fail":"x:1000000","fail_class":"api","fail_msg":"the interface changed, switch it off for everyone""#,
        );
    }
    let now = atrium::session_store::now_ms();
    let events = ticks(&mut fx, now, now + 5_000);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HealthEvent::Tripped { .. })),
        "{events:?}"
    );
    assert_eq!(fx.mods.tripped(), None);
    // The set-aside is on record.
    assert!(fx.log().iter().any(|e| s(e, "action") == "mod-set-aside"));
}

#[test]
fn a_child_lying_with_its_parent_is_the_same_voice_and_a_stranger_is_a_second() {
    let mut fx = Fx::new();
    let (parent, parent_tok) = fx.agent_pane("parent", None);
    let (_child, child_tok) = fx.agent_pane("child", Some(parent));
    let (_other, other_tok) = fx.agent_pane("other", None);
    const LIE: &str = r#""cmd":"report","fail":"x:1","fail_class":"api""#;
    let _ = call(&mut fx, &parent_tok, LIE);
    let _ = call(&mut fx, &child_tok, LIE);
    let now = atrium::session_store::now_ms();
    let events = ticks(&mut fx, now, now + 2_000);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HealthEvent::Tripped { .. })),
        "{events:?}"
    );
    // A pane the parent did not spawn says the same: two voices.
    let _ = call(&mut fx, &other_tok, LIE);
    let events = ticks(&mut fx, now + 3_000, now + 4_000);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HealthEvent::Tripped { .. })),
        "{events:?}"
    );
}

#[test]
fn a_malformed_failure_report_is_refused_whole() {
    let mut fx = Fx::new();
    let (a, a_tok) = fx.agent_pane("a", None);
    assert!(ok(&call(&mut fx, &a_tok, HELLO)));
    for body in [
        r#""cmd":"report","fail":"no-count""#,
        r#""cmd":"report","fail":"a b:1""#,
        r#""cmd":"report","fail":["a:1"]"#,
        r#""cmd":"report","fail":"a:1","fail_class":"catastrophe""#,
        r#""cmd":"report","fail":"a:1","fail_msg":7"#,
        r#""cmd":"report","fail_class":"api""#,
        r#""cmd":"report","fail_msg":"only words""#,
    ] {
        let v = call(&mut fx, &a_tok, body);
        assert!(!ok(&v), "{body} -> {v}");
    }
    assert_eq!(fx.mods.failures_for(a.0), None);
    assert_eq!(fx.mods.aside(a.0), None);
    // Nobody reports for someone else: the pane is the token's.
    let (b, _) = fx.agent_pane("b", None);
    let v = call(
        &mut fx,
        &a_tok,
        &format!(r#""cmd":"report","target":"{b}","fail":"a:9","fail_class":"api""#),
    );
    assert!(!ok(&v), "{v}");
    assert_eq!(fx.mods.aside(b.0), None);
}

#[test]
fn the_relaunch_gives_the_pane_a_new_process_under_a_new_id() {
    let mut fx = Fx::new();
    let (worker, _) = fx.agent_pane("worker", None);
    // A shell stands in for the claude that never started.
    let shell = pane_by_agent_mut(&mut fx.windows, worker)
        .expect("the pane")
        .title
        .clone();
    pane_by_agent_mut(&mut fx.windows, worker)
        .expect("the pane")
        .argv = vec![shell];
    fx.mods.expect_mod(worker.0);
    let mut hold = None;
    let reply = mod_relaunch(
        worker,
        &mut CtlSession {
            windows: &mut fx.windows,
            rows: 24,
            cols: 80,
            max_depth: fx.max_depth,
            extra_allow: &fx.extra_allow,
            pending: &mut fx.pending,
            world: &fx.world,
            session_identity: None,
            audit: &mut fx.audit,
            board: &mut fx.board,
            bus: &mut fx.bus,
            mods: &mut fx.mods,
            hold: &mut hold,
            held_inboxes: 0,
            flash: &mut fx.flash,
        },
    );
    let ctl::Reply::Spawned { pane: new_id, .. } = reply else {
        panic!("the relaunch is a respawn: {}", reply.to_json());
    };
    assert_ne!(new_id, worker);
    // The old id is forgotten and the new one was never expected: the
    // breaker has nothing left to relaunch a second time.
    assert!(!fx.mods.expects(worker.0));
    assert!(!fx.mods.expects(new_id.0));
    assert!(fx.log().iter().any(|e| s(e, "action") == "mod-relaunch"));
}
