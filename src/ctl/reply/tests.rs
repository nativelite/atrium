//! Reply builders and their JSON shape.

use super::*;
use crate::ctl::render::render_bus;
use crate::ctl::AgentId;
use json::Value;

#[test]
fn reply_bus_topics_shape_is_topic_and_subs() {
    let json = reply_bus_topics(vec![("deploy".to_string(), 2), ("billing".to_string(), 0)]);
    let parsed = json::parse(&json.to_json()).unwrap();
    assert_eq!(parsed.get("ok").and_then(Value::as_bool), Some(true));
    let arr = parsed.get("topics").and_then(Value::as_array).unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0].get("topic").and_then(Value::as_str), Some("deploy"));
    assert_eq!(arr[0].get("subs").and_then(Value::as_i64), Some(2));
    assert_eq!(arr[1].get("topic").and_then(Value::as_str), Some("billing"));
    assert_eq!(arr[1].get("subs").and_then(Value::as_i64), Some(0));
}

#[test]
fn reply_status_one_carries_idle_ms_additively() {
    let json = reply_status_one(AgentId(2), Some("working"), 7_000);
    let v = json::parse(&json.to_json()).unwrap();
    assert_eq!(v.get("pane").and_then(Value::as_i64), Some(2));
    assert_eq!(v.get("status").and_then(Value::as_str), Some("working"));
    assert_eq!(v.get("idle_ms").and_then(Value::as_i64), Some(7_000));
}

// ---- W1: zero-subscriber warning is STDERR-only, never in stdout -----

/// The warning is a client-side STDERR advisory derived from the reply, NOT
/// part of the rendered stdout view — so stdout stays byte-clean for consumers
/// that `json::parse` a `bus pub` reply (locked criterion [1]).
#[test]
fn zero_sub_warning_fires_with_exact_text_on_zero_subscribers() {
    let reply = r#"{"ok":true,"event":{"seq":7,"topic":"ghost","kind":"fyi","from":"dev_1","fields":{"m":"hi"}},"subscribers":0}"#;
    assert_eq!(
        zero_sub_warning(reply).as_deref(),
        Some("warning: published to 'ghost' with 0 subscribers")
    );
    // And it is NOT smuggled into the stdout render.
    let view = render_bus(reply, true).expect("pub reply must render");
    assert!(
        !view.contains("warning:"),
        "warning must not be on stdout: {view:?}"
    );
}

#[test]
fn zero_sub_warning_silent_when_subscribers_present() {
    let reply = r#"{"ok":true,"event":{"seq":7,"topic":"deploy","kind":"fyi","from":"dev_1","fields":{"m":"hi"}},"subscribers":2}"#;
    assert_eq!(
        zero_sub_warning(reply),
        None,
        "no warning when there are subs"
    );
}

/// Backward compatibility: an old daemon reply with NO `subscribers` field
/// must never warn, and the event still renders on stdout unchanged.
#[test]
fn zero_sub_warning_backward_compatible_without_field() {
    let reply = r#"{"ok":true,"event":{"seq":7,"topic":"deploy","kind":"fyi","from":"dev_1","fields":{"m":"hi"}}}"#;
    assert_eq!(zero_sub_warning(reply), None, "legacy reply must not warn");
    let view = render_bus(reply, true).expect("legacy pub reply must still render");
    assert!(
        view.contains("deploy"),
        "legacy reply still shows the event: {view:?}"
    );
}

// ---- W2: idle rendering helpers (pure idle_ms lives in ipc.rs) -------

#[test]
fn humanize_idle_scales_by_magnitude() {
    assert_eq!(humanize_idle(0), "0s");
    assert_eq!(humanize_idle(12_000), "12s");
    assert_eq!(humanize_idle(59_000), "59s");
    assert_eq!(humanize_idle(60_000), "1m");
    assert_eq!(humanize_idle(4 * 60_000), "4m");
    assert_eq!(humanize_idle(3_600_000), "1h");
    assert_eq!(humanize_idle(3_600_000 + 3 * 60_000), "1h3m");
    assert_eq!(humanize_idle(2 * 86_400_000), "2d");
    assert_eq!(humanize_idle(86_400_000 + 3_600_000), "1d1h");
}

#[test]
fn idle_render_threshold_keeps_active_panes_quiet() {
    // A pane idle under the threshold should not be surfaced as idle; at or
    // past it, it should. (The pure monotonic idle_ms is unit-tested in ipc.rs.)
    assert!(5_000 < IDLE_RENDER_MIN_MS);
    assert!(20_000 >= IDLE_RENDER_MIN_MS);
}

#[test]
fn reply_killed_lists_the_torn_down_panes() {
    let r = reply_killed(&[AgentId(1), AgentId(2), AgentId(3)]);
    let v = json::parse(&r.to_json()).unwrap();
    assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        v.get("killed").and_then(Value::as_array).map(<[_]>::len),
        Some(3)
    );
}

#[test]
fn reply_builders_are_valid_json() {
    let spawned = reply_spawned(AgentId(3), Some("dev_1"), Some("abc-123"), None);
    let v = json::parse(&spawned.to_json()).unwrap();
    assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(v.get("pane").and_then(Value::as_i64), Some(3));
    assert!(matches!(v.get("note"), Some(Value::Null)));

    // With a note, it rides along as a string field.
    let noted = reply_spawned(
        AgentId(3),
        None,
        None,
        Some("stripped --dangerously-skip-permissions"),
    );
    let nv = json::parse(&noted.to_json()).unwrap();
    assert_eq!(
        nv.get("note").and_then(Value::as_str),
        Some("stripped --dangerously-skip-permissions")
    );

    let nodes = [TreeNode {
        id: AgentId(0),
        parent: None,
        role: Some("ceo"),
        title: "claude",
        depth: 0,
        status: Some("working"),
        idle_ms: 4_000,
    }];
    let listed = reply_list(&nodes);
    let v = json::parse(&listed.to_json()).unwrap();
    assert_eq!(
        v.get("tree").and_then(Value::as_array).map(<[_]>::len),
        Some(1)
    );
    // W2 criterion [4]: idle_ms is additive; baseline node keys are untouched.
    let node = &v.get("tree").and_then(Value::as_array).unwrap()[0];
    assert_eq!(node.get("idle_ms").and_then(Value::as_i64), Some(4_000));
    for key in ["id", "parent", "role", "title", "depth", "status"] {
        assert!(node.get(key).is_some(), "baseline key {key:?} must remain");
    }

    let err = reply_err("nope");
    let v = json::parse(&err.to_json()).unwrap();
    assert_eq!(v.get("ok").and_then(Value::as_bool), Some(false));
}
