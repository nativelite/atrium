//! Argv the client must refuse before anything is sent.

use super::*;
use crate::ctl::testutil::v;

#[test]
fn bus_pub_without_fields_is_an_error() {
    let err = build_request(&v(&["bus", "pub", "deploy"]), None).unwrap_err();
    assert!(err.contains("field=value"), "{err}");
}

#[test]
fn send_without_text_is_an_error() {
    let err = build_request(&v(&["send", "dev_1"]), None).unwrap_err();
    assert!(err.contains("text"), "{err}");
}

#[test]
fn spawn_without_command_is_an_error() {
    let err = build_request(&v(&["spawn", "--role", "x"]), None).unwrap_err();
    assert!(err.contains("needs a command"), "{err}");
}

#[test]
fn board_set_needs_a_field() {
    // A key with no field=value is a clear client error, and a bad pair too.
    assert!(build_request(&v(&["board", "set", "auth"]), None)
        .unwrap_err()
        .contains("field=value"));
    assert!(
        build_request(&v(&["board", "set", "auth", "nofieldeq"]), None)
            .unwrap_err()
            .contains("field=value")
    );
}

#[test]
fn kill_without_target_is_an_error() {
    let err = build_request(&v(&["kill"]), None).unwrap_err();
    assert!(err.contains("target"), "{err}");
}

#[test]
fn audit_non_numeric_tail_is_an_error() {
    let err = build_request(&v(&["audit", "lots"]), None).unwrap_err();
    assert!(err.contains("number"), "{err}");
}

#[test]
fn a_value_flag_never_swallows_the_next_flag() {
    // `--role --here` used to set role="--here" and drop the placement; every
    // flag that takes a name refuses a flag in its place.
    for argv in [
        &["spawn", "--role", "--here", "--", "claude"][..],
        &["spawn", "--identity", "--here", "--", "claude"],
        &["spawn", "--worktree", "--role", "r", "--", "claude"],
        &["respawn", "dev", "--worktree", "--x"],
        &["bus", "pub", "work", "--to", "--decision", "item=X"],
    ] {
        let err = build_request(&v(argv), None).unwrap_err();
        assert!(err.contains("needs"), "{argv:?}: {err}");
    }
}

#[test]
fn bus_pub_to_still_takes_a_role() {
    let req = build_request(
        &v(&["bus", "pub", "work", "--to", "lead", "--decision", "item=X"]),
        None,
    )
    .unwrap();
    assert!(req.contains(r#""to":"lead""#), "{req}");
    assert!(req.contains(r#""kind":"decision_needed""#), "{req}");
}

#[test]
fn oversized_numbers_are_errors_not_wraps() {
    // u64 * 1000 overflowed (a debug panic) and `as i64` wrapped to a negative
    // lease; the audit tail wrapped to -1.
    for argv in [
        &["board", "claim", "k", "--ttl", "18446744073709552"][..],
        &["board", "claim", "k", "--ttl", "9300000000000000"],
        &["audit", "18446744073709551615"],
    ] {
        let err = build_request(&v(argv), None).unwrap_err();
        assert!(err.contains("too large"), "{argv:?}: {err}");
    }
    let req = build_request(&v(&["board", "claim", "k", "--ttl", "60"]), None).unwrap();
    assert!(req.contains(r#""ttl_ms":60000"#), "{req}");
}

#[test]
fn bus_feed_since_given_twice_is_an_error() {
    // Two `--since` used to send the `since` key twice.
    for argv in [
        &["bus", "feed", "--since", "3", "--since=9"][..],
        &["bus", "feed", "--since=3", "--since", "9"],
    ] {
        let err = build_request(&v(argv), None).unwrap_err();
        assert!(err.contains("once"), "{argv:?}: {err}");
    }
    let req = build_request(&v(&["bus", "feed", "--since", "3"]), None).unwrap();
    assert!(req.contains(r#""since":3"#), "{req}");
}
