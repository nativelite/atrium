//! Argv the client must refuse before anything is sent.

use super::*;
use crate::ctl::testutil::v;

#[test]
fn bus_pub_without_fields_is_an_error() {
    let err = build_request(&v(&["bus", "pub", "deploy"]), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("field=value"), "{err}");
}

#[test]
fn send_without_text_is_an_error() {
    let err = build_request(&v(&["send", "dev_1"]), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("text"), "{err}");
}

/// A spawn with no command after `--` is a request with an empty argv: the
/// server fills in the caller's own kind (codex from a codex pane, claude
/// from a claude pane and from anything else). It used to be refused.
#[test]
fn spawn_without_command_asks_for_the_callers_own_kind() {
    assert!(build_request(&v(&["spawn", "--role", "x"]), None).is_ok());
}

#[test]
fn board_set_needs_a_field() {
    // A key with no field=value is a clear client error, and a bad pair too.
    assert!(build_request(&v(&["board", "set", "auth"]), None)
        .unwrap_err()
        .to_string()
        .contains("field=value"));
    assert!(
        build_request(&v(&["board", "set", "auth", "nofieldeq"]), None)
            .unwrap_err()
            .to_string()
            .contains("field=value")
    );
}

#[test]
fn kill_without_target_is_an_error() {
    let err = build_request(&v(&["kill"]), None).unwrap_err().to_string();
    assert!(err.contains("target"), "{err}");
}

#[test]
fn audit_non_numeric_tail_is_an_error() {
    let err = build_request(&v(&["audit", "lots"]), None)
        .unwrap_err()
        .to_string();
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
        let err = build_request(&v(argv), None).unwrap_err().to_string();
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
        let err = build_request(&v(argv), None).unwrap_err().to_string();
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
        let err = build_request(&v(argv), None).unwrap_err().to_string();
        assert!(err.contains("once"), "{argv:?}: {err}");
    }
    let req = build_request(&v(&["bus", "feed", "--since", "3"]), None).unwrap();
    assert!(req.contains(r#""since":3"#), "{req}");
}

#[test]
fn ttl_and_tail_report_too_large_consistently() {
    // Over the lease cap (the server adds it to now_ms), past u64, or past
    // the parse type: all the same "too large", never "not a number".
    for argv in [
        &["board", "claim", "k", "--ttl", "9223372036854775"][..],
        &["board", "claim", "k", "--ttl", "99999999999999999999"],
        &["audit", "99999999999999999999"],
    ] {
        let err = build_request(&v(argv), None).unwrap_err().to_string();
        assert!(err.contains("too large"), "{argv:?}: {err}");
    }
    let err = build_request(&v(&["board", "claim", "k", "--ttl", "--x"]), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs"), "{err}");
}

#[test]
fn a_field_named_twice_is_an_error() {
    // The payload is an ordered list on the wire, so a repeated name reached
    // the server twice and `get` picked one silently (`--to` is a `to=` field).
    for argv in [
        &["bus", "pub", "work", "--to", "lead", "to=dev", "msg=x"][..],
        &["board", "set", "k", "a=1", "a=2"],
    ] {
        let err = build_request(&v(argv), None).unwrap_err().to_string();
        assert!(err.contains("twice"), "{argv:?}: {err}");
    }
}

/// A bad argv is a typed [`UsageError`]: it displays exactly its message (what
/// `ctl_cmd` prints after `atrium ctl: `) and travels as a `std::error::Error`.
#[test]
fn a_usage_error_displays_its_message_and_is_an_error() {
    let err = build_request(&v(&["frobnicate"]), None).unwrap_err();
    assert_eq!(err.message(), "unknown subcommand \"frobnicate\"");
    assert_eq!(err.to_string(), err.message());
    let boxed: Box<dyn std::error::Error> = Box::new(err);
    assert!(boxed.to_string().contains("frobnicate"));
}

// ---- ping: a liveness probe, bounded whatever the endpoint does ---------

#[test]
fn ping_timeout_is_milliseconds_within_bounds() {
    use std::time::Duration;
    assert_eq!(
        parse_ping(&v(&["ping"])),
        Ok(Duration::from_millis(PING_DEFAULT_TIMEOUT_MS))
    );
    assert_eq!(
        parse_ping(&v(&["ping", "--timeout", "250"])),
        Ok(Duration::from_millis(250))
    );
    for argv in [
        &["ping", "--timeout"][..],
        &["ping", "--timeout", "0"],
        &["ping", "--timeout", "30001"],
        &["ping", "--timeout", "soon"],
        &["ping", "--timeout", "99999999999999999999"],
        &["ping", "lead"],
    ] {
        assert!(parse_ping(&v(argv)).is_err(), "{argv:?}");
    }
}

/// Exit `0` only for a run loop's own answer; every other ending is `1` with
/// one line that says which.
#[test]
fn ping_exits_zero_only_when_the_run_loop_answered() {
    use std::io::{Error, ErrorKind};
    use std::time::Duration;
    let bound = Duration::from_secs(5);
    let pong = crate::ctl::reply_pong(1234).to_json();

    let (reply, alive) = (pong.clone(), ping_with(bound, move || Ok(pong)));
    assert_eq!(alive, Ping::Alive(reply.clone()));
    assert_eq!(alive.exit_code(), 0);
    assert_eq!(alive.line(), reply);

    let cases: Vec<(std::io::Result<String>, &str)> = vec![
        (
            Err(Error::new(ErrorKind::NotFound, "nothing there")),
            "no endpoint",
        ),
        (
            Err(Error::from(ErrorKind::ConnectionRefused)),
            "connection refused",
        ),
        (
            Err(Error::new(ErrorKind::BrokenPipe, "it hung up")),
            "it hung up",
        ),
        // The endpoint answered, but it is not a run loop answering `ping`:
        // an atrium too old to know the verb, another verb's reply, noise.
        (
            Ok(r#"{"ok":false,"err":"unknown cmd"}"#.to_string()),
            "refused: unknown cmd",
        ),
        (
            Ok(r#"{"ok":true,"tree":[]}"#.to_string()),
            "not with a ping reply",
        ),
        (Ok("hello\nworld".to_string()), "not with a ping reply"),
    ];
    for (outcome, says) in cases {
        let shown = format!("{outcome:?}");
        let ping = ping_with(bound, move || outcome);
        assert_eq!(ping.exit_code(), 1, "{shown}");
        let line = ping.line();
        assert!(line.contains(says), "{shown}: {line:?}");
        assert!(!line.contains('\n'), "one line: {line:?}");
    }
}

/// An exchange that never returns costs the probe its timeout and no more:
/// the bound is on the whole exchange, not on one read.
#[test]
fn ping_gives_up_at_its_timeout_and_exits_nonzero() {
    use std::time::{Duration, Instant};
    let started = Instant::now();
    let ping = ping_with(Duration::from_millis(100), || {
        std::thread::sleep(Duration::from_secs(20));
        Ok(String::new())
    });
    let took = started.elapsed();
    assert_eq!(ping, Ping::TimedOut(Duration::from_millis(100)));
    assert_eq!(ping.exit_code(), 1);
    assert!(ping.line().starts_with("timeout:"), "{}", ping.line());
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

/// A unique endpoint name per test (the pid alone would collide inside one
/// test process).
fn probe_address(test: &str) -> String {
    nlipc::address(&format!("atrium-ping-{test}"), std::process::id())
}

/// Against the real transport: an address nobody bound is "no endpoint" at
/// once, where a plain request retries the name for its whole deadline.
#[test]
fn ping_reports_no_endpoint_without_waiting_for_one() {
    use std::time::{Duration, Instant};
    let started = Instant::now();
    let ping = ping(
        &probe_address("absent"),
        r#"{"cmd":"ping"}"#,
        Duration::from_secs(10),
    );
    assert!(matches!(ping, Ping::NoEndpoint(_)), "{ping:?}");
    assert_eq!(ping.exit_code(), 1);
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Against the real transport: an endpoint whose owner accepts connections
/// and never serves them is a run loop that stopped turning. The probe times
/// out at its own bound, far inside the transport's.
#[test]
fn ping_times_out_on_an_endpoint_whose_loop_never_answers() {
    use std::time::{Duration, Instant};
    let address = probe_address("wedged");
    let _listener = crate::ipc::Listener::bind(&address).expect("bind");
    let started = Instant::now();
    let ping = ping(&address, r#"{"cmd":"ping"}"#, Duration::from_millis(300));
    let took = started.elapsed();
    assert_eq!(ping, Ping::TimedOut(Duration::from_millis(300)));
    assert_eq!(ping.exit_code(), 1);
    assert!(took < Duration::from_secs(4), "took {took:?}");
}

/// Against the real transport: a loop that polls its listener and answers is
/// alive, and the probe hands back its reply.
#[test]
fn ping_is_alive_when_the_loop_answers() {
    use std::time::{Duration, Instant};
    let address = probe_address("alive");
    let mut listener = crate::ipc::Listener::bind(&address).expect("bind");
    let probe = {
        let address = address.clone();
        std::thread::spawn(move || ping(&address, r#"{"cmd":"ping"}"#, Duration::from_secs(20)))
    };
    let pong = crate::ctl::reply_pong(42).to_json();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match listener.poll().expect("poll") {
            Some(line) => {
                assert_eq!(line, r#"{"cmd":"ping"}"#);
                listener.respond(&pong).expect("respond");
                break;
            }
            None => {
                assert!(Instant::now() < deadline, "the request never arrived");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    // Keep serving until the reply has drained to the client.
    while !probe.is_finished() {
        assert!(Instant::now() < deadline, "the probe never finished");
        let _ = listener.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    let ping = probe.join().expect("probe thread");
    assert_eq!(ping, Ping::Alive(pong));
    assert_eq!(ping.exit_code(), 0);
}
