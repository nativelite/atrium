//! Guards that read this transport's own source text: a rule the code
//! must keep obeying, checked against the shipping files themselves.
//! The readers live in `testutil`.

use super::testutil::{code_only, unix_sys_source, windows_sys_source};
use std::time::Duration;

/// **D2, guarded where a mac can guard it.** `FlushFileBuffers` on a handle
/// to the SERVER end of a named pipe "does not return until the client has
/// read all buffered data from the pipe", and `PIPE_NOWAIT` does not cover
/// it — the wait mode affects only ReadFile, WriteFile and ConnectNamedPipe.
/// It sat on the run-loop thread in `respond`, so any process that wrote a
/// request and never read the reply froze EVERY pane, with no ceiling; a
/// debugger-suspended client did it by accident.
///
/// The behaviour cannot be executed here, but its cause is a single call
/// that must never come back. `drain_outcome` and the instance state machine
/// above are what replaced it.
#[test]
fn the_run_loop_never_calls_flushfilebuffers() {
    // The BARE identifier, not `…(`: a reintroduced `extern "system"`
    // declaration has no call parenthesis and would sail past the narrower
    // needle. Assembled at compile time so it cannot match itself. Comments
    // are stripped first — the file explains at length *why* the call is
    // gone, and that prose must not be what the guard trips over.
    let name = concat!("Flush", "FileBuffers");
    assert!(
        !code_only().contains(name),
        "FlushFileBuffers blocks the single event loop on an unread pipe — \
         the drain state machine exists so it never has to be called, and \
         not even the extern declaration should come back"
    );
}

/// **The rule that keeps it fixed.** The unix client configures its socket
/// with `set_nonblocking` and nothing else. The needles are assembled at
/// compile time so the assertion cannot match itself.
#[test]
fn the_unix_client_uses_no_socket_timeouts() {
    let unix = unix_sys_source();
    for banned in [
        concat!("set_read", "_timeout"),
        concat!("set_write", "_timeout"),
    ] {
        assert!(
            !unix.contains(banned),
            "{banned} is back in the unix client: it re-arms per read, and it \
             fails with EINVAL exactly when a complete reply is already waiting"
        );
    }
    assert!(
        unix.contains("set_nonblocking(true)?"),
        "the unix client must put its socket in non-blocking mode"
    );
}

/// **G3.** Every client budget is one shared set in `wire`. They used to be
/// four unrelated literals — a `from_secs(5)` at the Windows connect, a
/// `stall`/`hard` pair in each platform's `read_reply` — so "how long may a
/// hostile endpoint hold `atrium ctl`?" had two different answers depending on
/// the platform.
#[test]
fn the_transport_budgets_live_in_one_place() {
    for (what, src) in [
        ("unix", unix_sys_source()),
        ("windows", windows_sys_source()),
    ] {
        assert!(
            !src.contains(concat!("Duration::", "from_secs(")),
            "{what} sys grew a hard-coded seconds literal again; the budgets \
             belong in `wire` where both platforms read the same number"
        );
        assert!(
            !src.contains(concat!("Duration::", "from_millis(")),
            "{what} sys grew a hard-coded milliseconds literal again"
        );
        for needed in ["REPLY_STALL", "REPLY_DEADLINE", "CLIENT_POLL"] {
            assert!(
                src.contains(needed),
                "{what} sys must take {needed} from `wire`"
            );
        }
    }
}

/// **Defect in both candidates.** A refused peer used to be announced with
/// an `eprint!` from inside the accept loop: an unbounded terminal write on
/// the run-loop thread, at whatever rate the attacker connects.
#[test]
fn refusals_are_not_printed_from_inside_the_accept_loop() {
    let unix = unix_sys_source();
    let accept = unix
        .split("fn accept_new(")
        .nth(1)
        .expect("accept_new")
        .split("\n        pub fn poll(")
        .next()
        .expect("up to poll");
    assert!(
        !accept.contains(concat!("epr", "int!")),
        "the accept loop must count refusals and let `report_refusals` \
         announce them at a bounded rate"
    );
    assert!(
        unix.contains("REFUSAL_REPORT_EVERY"),
        "refusal reporting must be rate limited"
    );
    // Naming the constant is not the same as honouring it: setting the
    // interval to zero restores the unbounded write that this guard exists
    // to prevent, and the source check above stays green through it.
    assert!(
        super::wire::REFUSAL_REPORT_EVERY >= Duration::from_millis(250),
        "a zero or near-zero refusal interval reinstates an unbounded \
         terminal write whose rate is chosen by whoever is connecting"
    );
}
