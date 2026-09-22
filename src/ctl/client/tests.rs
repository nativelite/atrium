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
