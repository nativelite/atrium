//! The human board/bus views.

use super::*;
use crate::ctl::{reply_board_claim, reply_bus_topics};

#[test]
fn render_bus_topics_lists_topic_and_count() {
    let json = reply_bus_topics(vec![("deploy".to_string(), 3)]);
    let view = render_bus(&json.to_json(), true).expect("topics reply must render");
    assert!(view.contains("deploy"), "names the topic: {view:?}");
    assert!(view.contains("subs=3"), "shows the count: {view:?}");
}

#[test]
fn render_bus_topics_empty_is_explicit() {
    let json = reply_bus_topics(vec![]);
    assert_eq!(
        render_bus(&json.to_json(), true).as_deref(),
        Some("  (no active topics)")
    );
}

#[test]
fn board_claim_denied_view_names_the_holder() {
    // A denied claim renders the current holder so a loser knows to move on.
    let reply = reply_board_claim(
        "cli",
        &crate::board::Claim::Denied {
            holder: "scout".to_string(),
            lease_ms: 1100,
        },
    );
    let view = render_board(&reply.to_json(), true).expect("claim reply renders");
    assert!(view.contains("held by scout"), "view was: {view}");
    // A granted claim shows the confirmation.
    let e = crate::board::Entry {
        claimed_by: Some("cli".to_string()),
        lease_ms: 1100,
        ..Default::default()
    };
    let granted = reply_board_claim("cli", &crate::board::Claim::Granted(e));
    let gview = render_board(&granted.to_json(), true).expect("granted renders");
    assert!(gview.contains("claimed cli"), "view was: {gview}");
}

#[test]
fn board_view_renders_status_color_and_clickable_url() {
    let reply = r#"{"ok":true,"board":[{"key":"auth","by":"dev_1","ms":1,"fields":{"status":"DONE","owner":"Max","url":"https://x.io"}}]}"#;
    // Colored output (TTY path).
    let view = render_board(reply, true).unwrap();
    assert!(view.contains("auth"), "key present");
    assert!(view.contains("owner=Max"));
    assert!(view.contains("\x1b[38;5;10m"), "DONE is green");
    // The url is wrapped as an OSC-8 hyperlink, not shown as bare text only.
    assert!(
        view.contains("\x1b]8;;https://x.io\x1b\\"),
        "url is clickable"
    );
    assert!(view.contains("(by dev_1)"));
    // Plain output (piped/redirected path) — no escape sequences at all.
    let plain = render_board(reply, false).unwrap();
    assert!(plain.contains("auth"), "key present in plain");
    assert!(plain.contains("status=DONE"), "status visible in plain");
    assert!(plain.contains("owner=Max"), "other fields in plain");
    assert!(plain.contains("(by dev_1)"), "author in plain");
    assert!(!plain.contains('\x1b'), "no escape codes in plain output");
}

#[test]
fn board_view_handles_empty_and_missing() {
    assert_eq!(
        render_board(r#"{"ok":true,"board":[]}"#, true).unwrap(),
        "  (board is empty)"
    );
    assert!(render_board(r#"{"ok":true,"key":"x","entry":null}"#, true)
        .unwrap()
        .contains("not on the board"));
    // A non-board (or failed) reply → None, so the caller prints raw JSON.
    assert!(render_board(r#"{"ok":true,"pane":3}"#, true).is_none());
    assert!(render_board(r#"{"ok":false,"err":"nope"}"#, true).is_none());
}

#[test]
fn status_color_map() {
    assert_eq!(status_sgr("DONE"), "\x1b[38;5;10m");
    assert_eq!(status_sgr("Blocked on api"), "\x1b[38;5;9m");
    assert_eq!(status_sgr("wip"), "\x1b[38;5;14m");
    assert_eq!(status_sgr("waiting"), "\x1b[38;5;11m");
    assert_eq!(status_sgr("anything else"), "\x1b[0m");
    assert!(is_url("https://x") && !is_url("Max"));
}
