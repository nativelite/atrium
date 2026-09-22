//! End-to-end over a real unix socket: the public `Listener`/`request`
//! pair driven against a live endpoint.
//!
//! These live beside `tests` rather than under `sys_unix` on purpose: the
//! bodies resolve `super::` as `crate::ipc` (they name `io`, `wire` and the
//! public `Listener`), and `sys_unix` defines a `Listener` of its own.

use super::testutil::test_addr;
use super::wire::{MAX_OUTBOX, MAX_REPLY};
use super::*;
use std::thread;
use std::time::{Duration, Instant};

/// macOS/BSD cap `sockaddr_un.sun_path` at 104 bytes, so an over-long temp
/// path must fail fast with a clear error, not a cryptic late OS failure.
#[cfg(unix)]
#[test]
fn unix_bind_rejects_overlong_path() {
    let long = std::env::temp_dir()
        .join("x".repeat(200))
        .to_string_lossy()
        .into_owned();
    match Listener::bind(&long) {
        Ok(_) => panic!("overlong socket path must be rejected"),
        Err(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidInput),
    }
}

/// **A half-sent request must not stall the event loop** (review #5).
///
/// The accepted stream used to be set BLOCKING with a 2s read timeout, read
/// one byte at a time — so the timeout re-armed per byte and a client that
/// dripped bytes held atrium's single event loop for as long as it liked,
/// freezing every pane. Any process able to connect could do it.
///
/// This drives exactly that shape: connect, send half a line, and require
/// `poll()` to come back promptly with "nothing yet" rather than waiting for
/// the rest. Then finish the line and require the SAME request to complete —
/// proving the partial was resumed across ticks, not discarded.
#[cfg(unix)]
#[test]
fn a_half_sent_request_does_not_block_the_loop() {
    use std::io::Write as _;

    let addr = test_addr(9021);
    let mut listener = Listener::bind(&addr).expect("bind");

    let mut client = std::os::unix::net::UnixStream::connect(&addr).expect("connect");
    client.write_all(br#"{"cmd":"li"#).expect("partial write");
    client.flush().ok();

    // Poll a few times: each must return promptly and report idle.
    for _ in 0..3 {
        let t = Instant::now();
        let got = listener.poll().expect("poll");
        assert!(
            got.is_none(),
            "a half-sent request must not yield a request"
        );
        assert!(
            t.elapsed() < Duration::from_millis(250),
            "poll blocked for {:?} on a half-sent request — this is the DoS",
            t.elapsed()
        );
    }

    // Finish it; the buffered head must still be there.
    client.write_all(b"st\"}\n").expect("rest");
    client.flush().ok();

    let deadline = Instant::now() + Duration::from_secs(2);
    let line = loop {
        if let Some(l) = listener.poll().expect("poll") {
            break l;
        }
        assert!(Instant::now() < deadline, "completed request never arrived");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        line, r#"{"cmd":"list"}"#,
        "the partial head must be preserved across ticks, not dropped"
    );
}

/// **D1, the executed exploit.** `poll()` sets the accepted stream
/// non-blocking; `respond` then called `write_all`, which retries only on
/// `Interrupted`. On macOS's 8 192-byte `net.local.stream.sendspace` that
/// returned `Err(WouldBlock)` AFTER a partial write, so `atrium ctl audit`
/// delivered 8 192 of 120 631 bytes and the client printed half-written JSON
/// and exited FAILURE — after roughly 120 ctl commands on any session.
#[cfg(unix)]
#[test]
fn a_reply_larger_than_the_send_buffer_arrives_whole() {
    let addr = test_addr(9101);
    let mut server = Listener::bind(&addr).expect("bind");
    let addr2 = addr.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(request(&addr2, r#"{"cmd":"audit"}"#));
    });

    // The measured size of a real `atrium ctl audit` reply.
    let payload = format!(r#"{{"ok":true,"detail":"{}"}}"#, "a".repeat(120_600));
    let mut answered = false;
    let start = Instant::now();
    let got = loop {
        if server.poll().expect("poll").is_some() {
            server.respond(&payload).expect("respond");
            answered = true;
        }
        match rx.try_recv() {
            Ok(r) => break r,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(e) => panic!("client vanished: {e}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "client never got its reply (answered={answered})"
        );
        thread::sleep(Duration::from_millis(1));
    };
    let got = got.expect("ctl client failed");
    assert_eq!(
        got.len(),
        payload.len(),
        "reply truncated at {} of {} bytes",
        got.len(),
        payload.len()
    );
    assert_eq!(got, payload);
}

/// **D5, the executed exploit.** `accept()` used to be unreachable while one
/// client was mid-request, so connecting and sending NOTHING held the whole
/// control channel for CLIENT_DEADLINE. One silent connection per second
/// burned 2 s of channel time per second of attack: measured, 0 of 6
/// legitimate ctl calls succeeded over 30 s while an attacker held 32 fds.
/// This lands pre-authentication — the channel is wedged before a byte is
/// parsed — so any hosted agent could lock out the operator and every
/// sibling pane, `board claim`/`release` included.
#[cfg(unix)]
#[test]
fn silent_clients_cannot_lock_out_a_real_one() {
    use std::os::unix::net::UnixStream;

    let addr = test_addr(9102);
    let mut server = Listener::bind(&addr).expect("bind");

    // 32 connections that will never say a word, held open for the test.
    let attackers: Vec<UnixStream> = (0..32)
        .filter_map(|_| UnixStream::connect(&addr).ok())
        .collect();
    assert!(attackers.len() >= 16, "could not stage the attack");

    let addr2 = addr.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(request(&addr2, r#"{"cmd":"list"}"#));
    });

    let start = Instant::now();
    let got = loop {
        if server.poll().expect("poll").is_some() {
            server.respond(r#"{"ok":true}"#).expect("respond");
        }
        match rx.try_recv() {
            Ok(r) => break r,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(e) => panic!("client vanished: {e}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "silent clients locked the channel — the legitimate ctl call was \
             never even accepted"
        );
        thread::sleep(Duration::from_millis(1));
    };
    assert!(got.expect("ctl call failed").contains("\"ok\":true"));
    drop(attackers);
}

/// **D6, the executed exploit.** `read_reply` accumulated with no cap and
/// the 5 s read timeout re-armed on every read, so it never fired against a
/// server that kept sending: a hostile endpoint pushed 67 108 864 bytes with
/// no newline in 250 ms and `request` returned `Ok` with a 67 MB String.
#[cfg(unix)]
#[test]
fn client_refuses_an_unbounded_reply() {
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;

    let addr = test_addr(9103);
    let _ = std::fs::remove_file(&addr);
    let hostile = UnixListener::bind(&addr).expect("hostile bind");
    let server = thread::spawn(move || {
        if let Ok((mut s, _)) = hostile.accept() {
            let chunk = vec![b'x'; 65_536];
            // 64 MiB, and not one newline anywhere.
            for _ in 0..1024 {
                if s.write_all(&chunk).is_err() {
                    break;
                }
            }
        }
    });

    let start = Instant::now();
    let r = request(&addr, r#"{"cmd":"list"}"#);
    let e = r.expect_err("an unbounded reply must not be returned as success");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "the cap must fire quickly"
    );
    let _ = server.join();
    let _ = std::fs::remove_file(&addr);
}

/// **D7, the executed exploit.** EOF before a newline used to `break` out of
/// the read loop and return whatever had arrived as `Ok`, so a server that
/// died mid-reply handed the caller truncated JSON and no error at all.
#[cfg(unix)]
#[test]
fn client_reports_a_truncated_reply_as_an_error() {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixListener;

    let addr = test_addr(9104);
    let _ = std::fs::remove_file(&addr);
    let hostile = UnixListener::bind(&addr).expect("bind");
    let server = thread::spawn(move || {
        if let Ok((mut s, _)) = hostile.accept() {
            let mut scratch = [0u8; 256];
            let _ = s.read(&mut scratch);
            // Half a reply, then die.
            let _ = s.write_all(br#"{"ok":true,"tre"#);
        }
    });

    let e = request(&addr, r#"{"cmd":"list"}"#)
        .expect_err("a reply cut short must not be reported as success");
    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof, "{e}");
    let _ = server.join();
    let _ = std::fs::remove_file(&addr);
}

/// **D9.** An oversize request must be refused *with an answer* — the old
/// path closed the connection with nothing on it, which is
/// indistinguishable from a crashed atrium — and must never surface to the run
/// loop as a request.
#[cfg(unix)]
#[test]
fn an_oversize_request_is_refused_with_an_error_reply() {
    use super::wire::OVERSIZE_REPLY;
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

    let addr = test_addr(9105);
    let mut server = Listener::bind(&addr).expect("bind");
    let client = UnixStream::connect(&addr).expect("connect");
    let talker = thread::spawn(move || {
        let mut c = client;
        c.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut body = vec![b'x'; 70_000];
        body.push(b'\n');
        let _ = c.write_all(&body);
        let mut reply = String::new();
        let _ = c.read_to_string(&mut reply);
        reply
    });

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        assert!(
            server.poll().expect("poll").is_none(),
            "an oversize request must never reach the run loop"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let reply = talker.join().expect("client thread");
    assert!(
        reply.contains("exceeded"),
        "a refused client must be told why, got: {reply:?}"
    );
    assert_eq!(reply.trim_end(), OVERSIZE_REPLY);
}

/// **D8, detection.** A same-uid process can `unlink()` the ctl socket and
/// `bind()` its own in its place — the authority comes from the 0700 TMPDIR,
/// not from the socket's 0600 mode. Observed against the real binary: the
/// attacker received `{"caller":1,"token":"…","cmd":"list"}` on the wire, the
/// victim printed the attacker's forged reply and exited 0, and real atrium
/// served nothing thereafter. atrium cannot repair that from here, but it must
/// not fail to NOTICE it.
#[cfg(unix)]
#[test]
fn endpoint_replacement_is_detected() {
    use std::os::unix::net::UnixListener;

    let addr = test_addr(9106);
    let mut server = Listener::bind(&addr).expect("bind");
    let _ = server.poll();
    assert!(
        server.take_security_event().is_none(),
        "no alarm when intact"
    );

    // The hijack, exactly as the exploit does it.
    std::fs::remove_file(&addr).expect("unlink");
    let attacker = UnixListener::bind(&addr).expect("attacker bind");

    // The check is rate-limited to ~1/s so it costs the run loop nothing.
    thread::sleep(Duration::from_millis(1100));
    let _ = server.poll();
    let event = server
        .take_security_event()
        .expect("a replaced ctl endpoint must be reported");
    assert!(event.contains("replaced"), "{event}");
    assert!(
        server.take_security_event().is_none(),
        "the alarm is taken once"
    );

    drop(attacker);
    drop(server);
    let _ = std::fs::remove_file(&addr);
}

// =======================================================================
// Hardening round: the defects the judge found in BOTH candidate patches,
// plus the two probes that decided between them.
// =======================================================================

/// **JUDGE PROBE (D5, worst case).** The legitimate client connects FIRST
/// and its request is already complete; a burst of silent connections lands
/// behind it in the accept backlog before the server's next tick. A server
/// that accepts more per tick than its pool can hold turns eviction into a
/// client-killer: the ready client is pushed out before it is ever read.
/// This is D5 again through a different door — pre-authentication, any
/// hosted agent, and it kills `board claim`.
#[cfg(unix)]
#[test]
fn judge_probe_burst_must_not_evict_a_ready_client() {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

    let addr = test_addr(47002);
    let mut server = Listener::bind(&addr).expect("bind");

    let mut good = UnixStream::connect(&addr).expect("connect good");
    good.write_all(b"{\"cmd\":\"list\"}\n").expect("write");
    good.flush().ok();

    let mut squatters = Vec::new();
    for _ in 0..40 {
        if let Ok(s) = UnixStream::connect(&addr) {
            squatters.push(s);
        }
    }

    let mut served = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Some(line) = server.poll().expect("poll") {
            served = Some(line.clone());
            server.respond(r#"{"ok":true}"#).expect("respond");
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    good.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let mut reply = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match good.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                reply.extend_from_slice(&chunk[..n]);
                if reply.contains(&b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let reply = String::from_utf8_lossy(&reply).to_string();
    drop(squatters);
    assert_eq!(
        served.as_deref(),
        Some(r#"{"cmd":"list"}"#),
        "a client whose request was already complete must not be evicted by a \
         burst of silent connections that arrived after it"
    );
    assert!(
        reply.contains("\"ok\":true"),
        "the legitimate client must receive its reply, got {reply:?}"
    );
}

/// **JUDGE PROBE (G1).** The API says the caller responds before polling
/// again. If it does not — a `?` early-return, a panic caught upstream, a
/// future refactor of `apply_ctl` — does the channel recover, or is it dead
/// for the life of the process? A single-slot latch that only `respond` can
/// clear is a fail-dangerous shape on a control plane that gates
/// `board claim`.
#[cfg(unix)]
#[test]
fn judge_probe_poll_without_respond_must_not_brick_the_channel() {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;

    let addr = test_addr(47003);
    let mut server = Listener::bind(&addr).expect("bind");

    let mut first = UnixStream::connect(&addr).expect("connect");
    first.write_all(b"{\"cmd\":\"one\"}\n").expect("write");
    first.flush().ok();

    let mut got_first = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Some(l) = server.poll().expect("poll") {
            got_first = Some(l);
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(got_first.as_deref(), Some(r#"{"cmd":"one"}"#));
    // Deliberately DO NOT respond. Drop the client too.
    drop(first);

    let mut second = UnixStream::connect(&addr).expect("connect 2");
    second.write_all(b"{\"cmd\":\"two\"}\n").expect("write");
    second.flush().ok();

    let mut got_second = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if let Some(l) = server.poll().expect("poll") {
            got_second = Some(l);
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        got_second.as_deref(),
        Some(r#"{"cmd":"two"}"#),
        "one missed respond must not kill the control channel permanently"
    );
}

/// **Defect in both candidates.** `park_reply` used to make room with
/// `outbox.pop_front()`, so the ninth concurrent parked reply discarded the
/// OLDEST in-flight one mid-stream: a legitimate slow reader of a 120 KB
/// `ctl audit` lost its answer, and neither end saw an error — `respond`
/// returned `Ok` and the client got EOF without a newline. That is the D1
/// failure moved one layer out.
///
/// The rule now: the newest is refused, loudly; nobody's in-flight reply is
/// ever destroyed to make room for it.
#[cfg(unix)]
#[test]
fn a_full_outbox_refuses_the_newest_and_keeps_every_inflight_reply() {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

    let addr = test_addr(47004);
    let mut server = Listener::bind(&addr).expect("bind");
    // Larger than any platform's socket send buffer so every reply parks: a
    // Linux unix socket buffers ~212 KB, so a 200 KB reply went straight
    // through there and nothing was ever refused.
    let pad = "z".repeat(1_000_000);

    // MAX_OUTBOX + 1 clients, each with a complete request, none of which
    // reads a byte — so every reply parks.
    let n = MAX_OUTBOX + 1;
    let mut clients: Vec<UnixStream> = Vec::new();
    for i in 0..n {
        let mut c = UnixStream::connect(&addr).expect("connect");
        c.write_all(format!("{{\"cmd\":\"c{i}\"}}\n").as_bytes())
            .expect("write");
        c.flush().ok();
        clients.push(c);
    }

    let mut results: Vec<io::Result<()>> = Vec::new();
    let start = Instant::now();
    while results.len() < n && start.elapsed() < Duration::from_secs(4) {
        if let Some(line) = server.poll().expect("poll") {
            let id = line
                .trim_start_matches("{\"cmd\":\"")
                .trim_end_matches("\"}");
            results.push(server.respond(&format!(
                "{{\"ok\":true,\"who\":\"{id}\",\"pad\":\"{pad}\"}}"
            )));
        } else {
            thread::sleep(Duration::from_millis(2));
        }
    }
    assert_eq!(results.len(), n, "every request must be served");
    let refused: Vec<usize> = results
        .iter()
        .enumerate()
        .filter(|(_, r)| r.is_err())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        refused,
        vec![n - 1],
        "exactly the NEWEST reply may be refused when the outbox is full; \
         results = {:?}",
        results
            .iter()
            .map(|r| r.as_ref().err().map(|e| e.kind()))
            .collect::<Vec<_>>()
    );

    // Now let the parked readers drain, and require every one of them to get
    // its OWN reply, whole.
    let mut readers = Vec::new();
    for (i, c) in clients.drain(..).enumerate().take(MAX_OUTBOX) {
        readers.push(thread::spawn(move || {
            let mut c = c;
            c.set_read_timeout(Some(Duration::from_secs(6))).ok();
            let mut got = Vec::new();
            let mut b = [0u8; 65536];
            loop {
                match c.read(&mut b) {
                    Ok(0) => break,
                    Ok(k) => {
                        got.extend_from_slice(&b[..k]);
                        if got.contains(&b'\n') {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            (i, String::from_utf8_lossy(&got).to_string())
        }));
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) && server.outbox_len() > 0 {
        let _ = server.poll().expect("poll");
        thread::sleep(Duration::from_millis(2));
    }
    for r in readers {
        let (i, got) = r.join().expect("reader");
        assert!(
            got.contains(&format!("\"who\":\"c{i}\"")) && got.ends_with('\n'),
            "client c{i} lost its in-flight reply: {} bytes, ends_with_nl={}",
            got.len(),
            got.ends_with('\n')
        );
    }
}

/// **Defect in both candidates.** The reply cap was written by the readers
/// and unknown to the writer: `respond` framed a reply of any size and both
/// clients refused past `MAX_REPLY` with `InvalidData`, so an oversize
/// `ctl audit` failed with an error that blamed the channel. One constant,
/// asserted at both ends — and the client gets a diagnosis, not a hang-up.
#[cfg(unix)]
#[test]
fn the_server_refuses_its_own_oversize_reply_and_says_so() {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

    let addr = test_addr(47005);
    let mut server = Listener::bind(&addr).expect("bind");
    let mut client = UnixStream::connect(&addr).expect("connect");
    client.write_all(b"{\"cmd\":\"audit\"}\n").expect("write");
    client.flush().ok();

    let huge = "q".repeat(MAX_REPLY + 1);
    let mut outcome = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if server.poll().expect("poll").is_some() {
            outcome = Some(server.respond(&huge));
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let outcome = outcome.expect("the request must be served");
    let err = outcome.expect_err("a reply over MAX_REPLY must not be sent");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");

    client.set_read_timeout(Some(Duration::from_secs(3))).ok();
    let mut got = Vec::new();
    let mut b = [0u8; 4096];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) && !got.contains(&b'\n') {
        let _ = server.poll();
        match client.read(&mut b) {
            Ok(0) => break,
            Ok(k) => got.extend_from_slice(&b[..k]),
            Err(_) => break,
        }
    }
    let got = String::from_utf8_lossy(&got).to_string();
    assert!(
        got.contains("\"ok\":false") && got.contains("exceeded"),
        "the client must be told why, got {got:?}"
    );
    assert!(
        !got.contains("qqqq"),
        "not one byte of the oversize answer may reach the wire"
    );
}

/// **D6, the half a size cap does not cover.** A server that dribbles bytes
/// forever stays under `MAX_REPLY` indefinitely, so only a TOTAL deadline
/// stops it — a per-read timeout re-arms on every byte and never fires.
/// Driven through the shipping client with short budgets so it is a test and
/// not a nap.
#[cfg(unix)]
#[test]
fn a_dripping_reply_is_bounded_by_a_total_deadline() {
    use std::io::Write as _;
    use std::os::unix::net::{UnixListener, UnixStream};

    let addr = test_addr(47006);
    let _ = std::fs::remove_file(&addr);
    let listener = UnixListener::bind(&addr).expect("bind");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    let hostile = thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            // One byte at a time, forever, and never a newline.
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                if s.write_all(b"x").is_err() {
                    break;
                }
                let _ = s.flush();
                thread::sleep(Duration::from_millis(20));
            }
        }
    });

    let mut c = UnixStream::connect(&addr).expect("connect");
    c.set_nonblocking(true).expect("nonblocking");
    let start = Instant::now();
    // stall is generous (the drip always makes progress); the TOTAL budget is
    // the only thing that can end this.
    // Run the exchange on its own thread with a hard cap. If the total
    // deadline is ever removed this test would otherwise hang FOREVER —
    // `cargo test` has no per-test timeout and this repo has no CI to kill
    // it, so the failure mode of the guard would be a wedged machine rather
    // than a red test.
    let (tx, rx) = std::sync::mpsc::channel();
    let probe = thread::spawn(move || {
        let r = sys::exchange(
            &mut c,
            r#"{"cmd":"list"}"#,
            Duration::from_secs(5),
            Duration::from_millis(600),
        );
        let _ = tx.send(r.map(|_| ()).map_err(|e| (e.kind(), e.to_string())));
    });
    let outcome = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the total deadline must end the exchange; it did not return");
    let _ = probe.join();
    let e = outcome.expect_err("a reply that never ends must be an error");
    let took = start.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = hostile.join();
    let _ = std::fs::remove_file(&addr);
    assert_eq!(e.0, io::ErrorKind::TimedOut, "{}", e.1);
    assert!(
        e.1.contains("never finished"),
        "the TOTAL deadline must be what fired, not the stall: {}",
        e.1
    );
    assert!(
        took < Duration::from_secs(3),
        "the total deadline must fire on schedule, took {took:?}"
    );
}

/// **The other half of the client's two budgets.** The test above drives the
/// TOTAL deadline with a server that never stops talking. This drives the
/// STALL deadline with a server that never starts: it accepts the connection
/// and then says nothing at all, ever — a wedged atrium, or the endpoint
/// squatter of D8 sitting on the socket without answering.
///
/// Delete the stall check from `read_reply` and this case falls through to
/// the total budget instead. In shipping that is `REPLY_DEADLINE` 30 s
/// against `REPLY_STALL` 5 s, so `atrium ctl` would hang for half a minute on
/// a dead control plane. Nothing caught that removal before this test.
#[cfg(unix)]
#[test]
fn a_silent_endpoint_is_bounded_by_the_stall_deadline() {
    use std::os::unix::net::{UnixListener, UnixStream};

    let addr = test_addr(47010);
    let _ = std::fs::remove_file(&addr);
    let listener = UnixListener::bind(&addr).expect("bind");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    let mute = thread::spawn(move || {
        if let Ok((s, _)) = listener.accept() {
            // Hold it open and never write a byte.
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
            }
            drop(s);
        }
    });

    let mut c = UnixStream::connect(&addr).expect("connect");
    c.set_nonblocking(true).expect("nonblocking");
    let start = Instant::now();
    // Same watchdog reasoning as above: without it, deleting the stall check
    // wedges the run rather than reddening it.
    let (tx, rx) = std::sync::mpsc::channel();
    let probe = thread::spawn(move || {
        let r = sys::exchange(
            &mut c,
            r#"{"cmd":"list"}"#,
            Duration::from_millis(400),
            Duration::from_secs(20),
        );
        let _ = tx.send(r.map(|_| ()).map_err(|e| (e.kind(), e.to_string())));
    });
    let outcome = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the stall deadline must end the exchange; it did not return");
    let _ = probe.join();
    let took = start.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = mute.join();
    let _ = std::fs::remove_file(&addr);

    let e = outcome.expect_err("a server that never answers must be an error");
    assert_eq!(e.0, io::ErrorKind::TimedOut, "{}", e.1);
    assert!(
        e.1.contains("no reply"),
        "the STALL deadline must be what fired, not the 20 s total: {}",
        e.1
    );
    assert!(
        took < Duration::from_secs(3),
        "the stall deadline must fire on schedule, took {took:?}"
    );
}

/// **The outbox has to be big enough to be an outbox.**
/// `a_full_outbox_refuses_the_newest_and_keeps_every_inflight_reply` sizes
/// its client list as `MAX_OUTBOX + 1`, so it proves the refusal *policy* at
/// any capacity — including a capacity of one, which it passes happily. That
/// left the number itself unpinned: shrinking `MAX_OUTBOX` to 1 broke no
/// test, while turning every second concurrent slow reader into an error.
///
/// Panes are slow readers by nature. This pins the floor behaviourally: four
/// clients that have each asked and are each reading nothing must all be
/// given their reply, not refused.
#[cfg(unix)]
#[test]
fn several_slow_readers_are_served_at_once() {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;

    const CONCURRENT: usize = 4;
    let addr = test_addr(47011);
    let mut server = Listener::bind(&addr).expect("bind");
    let pad = "z".repeat(200_000);

    let mut clients: Vec<UnixStream> = Vec::new();
    for i in 0..CONCURRENT {
        let mut c = UnixStream::connect(&addr).expect("connect");
        c.write_all(format!("{{\"cmd\":\"c{i}\"}}\n").as_bytes())
            .expect("write");
        c.flush().ok();
        clients.push(c);
    }

    let mut served = 0usize;
    let start = Instant::now();
    while served < CONCURRENT && start.elapsed() < Duration::from_secs(4) {
        if server.poll().expect("poll").is_some() {
            server
                .respond(&format!("{{\"ok\":true,\"pad\":\"{pad}\"}}"))
                .unwrap_or_else(|e| {
                    panic!(
                        "reply {} of {CONCURRENT} was refused ({e}) — MAX_OUTBOX is \
                     {MAX_OUTBOX}, too small to hold the slow readers a pane \
                     tree produces",
                        served + 1
                    )
                });
            served += 1;
        }
    }
    drop(clients);
    let _ = std::fs::remove_file(&addr);
    assert_eq!(served, CONCURRENT, "not every slow reader was served");
}

/// **The OS fact behind the client's shape.** On macOS,
/// `setsockopt(SO_RCVTIMEO/SO_SNDTIMEO)` on a unix-domain stream whose peer
/// has already closed fails with EINVAL — while a complete, valid reply is
/// still sitting in the receive buffer and reads back fine. Any client that
/// configures its socket with `?` after connecting therefore throws away
/// good answers from a server that answered and hung up.
#[cfg(unix)]
#[test]
fn a_closed_peer_breaks_setsockopt_but_not_the_buffered_reply() {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::{UnixListener, UnixStream};

    let addr = test_addr(47007);
    let _ = std::fs::remove_file(&addr);
    let listener = UnixListener::bind(&addr).expect("bind");
    let client = UnixStream::connect(&addr).expect("connect");
    {
        let (mut s, _) = listener.accept().expect("accept");
        s.write_all(b"{\"ok\":true}\n").expect("write");
        s.flush().expect("flush");
    }
    thread::sleep(Duration::from_millis(120));

    let mut client = client;
    let rcv = client.set_read_timeout(Some(Duration::from_millis(100)));
    let nonblock = client.set_nonblocking(true);
    let mut buf = [0u8; 64];
    let n = client
        .read(&mut buf)
        .expect("the buffered reply is still readable");

    assert!(
        nonblock.is_ok(),
        "set_nonblocking must keep working on a closed peer: {nonblock:?}"
    );
    assert_eq!(
        &buf[..n],
        b"{\"ok\":true}\n",
        "the peer's bytes survive its close"
    );
    if cfg!(target_os = "macos") {
        let e = rcv.expect_err(
            "this test exists because macOS fails setsockopt on a closed peer; \
             if that changed, the client's comment needs updating",
        );
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
    }
    let _ = std::fs::remove_file(&addr);
}

/// **The constraint the whole file exists for, driven by the peer that
/// breaks it.** `respond()` used to `write_all` a reply onto a non-blocking
/// socket, and the Windows `respond()` called `FlushFileBuffers`, whose
/// documented contract on the server end of a named pipe is that it "does
/// not return until the client has read all buffered data from the pipe".
/// Either way the peer that never reads is the peer that freezes every pane.
///
/// So: a client that sends a real request and then reads NOTHING, ever,
/// while atrium tries to hand it a reply far larger than one send buffer. Both
/// `poll()` and `respond()` must come back inside a fraction of one 15 ms
/// tick, every tick, for the whole of `WRITE_STALL` — and then the hopeless
/// connection must be retired instead of held forever.
#[cfg(unix)]
#[test]
fn neither_poll_nor_respond_waits_on_a_client_that_never_reads() {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;

    // Its own nonce: 47011 is also used by a test that runs in parallel in the
    // same process, and the shared socket path failed one of them with
    // AddrInUse.
    let addr = test_addr(47013);
    let mut server = Listener::bind(&addr).expect("bind");

    // Connects, asks, and then never calls read(). Held open for the whole
    // test: this is the debugger-suspended client, not a disconnect.
    let mut deaf = UnixStream::connect(&addr).expect("connect");
    deaf.write_all(b"{\"cmd\":\"audit\"}\n").expect("write");
    deaf.flush().ok();

    // Far more than the ~8 KiB macOS send buffer, so the kernel refuses the
    // tail and the reply MUST be parked rather than waited on.
    let reply = format!("{{\"ok\":true,\"pad\":\"{}\"}}", "z".repeat(400_000));

    let budget = Duration::from_millis(50);
    let mut worst_poll = Duration::ZERO;
    let mut worst_respond = Duration::ZERO;
    let mut served = false;

    // Run past WRITE_STALL so the give-up path is exercised too, not just
    // the parking path.
    let deadline = Instant::now() + wire::WRITE_STALL + Duration::from_secs(1);
    while Instant::now() < deadline {
        let t = Instant::now();
        let got = server.poll().expect("poll");
        worst_poll = worst_poll.max(t.elapsed());
        assert!(
            t.elapsed() < budget,
            "poll() waited {:?} on a client that never reads",
            t.elapsed()
        );
        if got.is_some() {
            let t = Instant::now();
            server.respond(&reply).expect("respond");
            worst_respond = worst_respond.max(t.elapsed());
            assert!(
                t.elapsed() < budget,
                "respond() waited {:?} on a client that never reads",
                t.elapsed()
            );
            served = true;
        }
        thread::sleep(Duration::from_millis(2));
    }

    assert!(served, "the request must have been served at least once");
    assert_eq!(
        server.outbox_len(),
        0,
        "a reply to a client that never reads must be given up on at \
         WRITE_STALL, not held forever"
    );
    eprintln!("PROOF non-blocking: worst poll {worst_poll:?}, worst respond {worst_respond:?}");
    drop(deaf);
    let _ = std::fs::remove_file(&addr);
}

/// **No fd or slot leak.** A bounded accept pool is only a fix if the bound
/// is also released: parking connections instead of refusing them moves the
/// exhaustion from the channel to the fd table unless every parked
/// connection is eventually dropped.
///
/// Thousands of cycles of the attack shape — connect, stall, abandon —
/// interleaved with clients that send a whole request and then vanish
/// without reading the reply (the outbox path). Open fds are counted before
/// and after against a warmed-up baseline.
#[cfg(unix)]
#[test]
fn thousands_of_stall_and_abandon_cycles_leak_no_fds_or_slots() {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;

    fn open_fds() -> usize {
        std::fs::read_dir("/dev/fd")
            .map(|d| d.count())
            .expect("/dev/fd")
    }

    let addr = test_addr(47012);
    let mut server = Listener::bind(&addr).expect("bind");

    let cycle = |server: &mut Listener, i: usize| {
        let mut c = UnixStream::connect(&addr).expect("connect");
        match i % 3 {
            // Silent squatter: says nothing at all, then vanishes.
            0 => {}
            // Half a request, then vanishes mid-line.
            1 => {
                let _ = c.write_all(b"{\"cmd\":\"li");
            }
            // A whole request, then vanishes without reading the reply.
            _ => {
                let _ = c.write_all(b"{\"cmd\":\"list\"}\n");
            }
        }
        let _ = c.flush();
        if let Some(_line) = server.poll().expect("poll") {
            server.respond(r#"{"ok":true}"#).expect("respond");
        }
        drop(c);
    };

    // Warm up: let the pool, the outbox and the allocator reach steady state
    // before the baseline is taken, so the measurement is of the loop and
    // not of the first-use cost.
    for i in 0..300 {
        cycle(&mut server, i);
    }
    for _ in 0..40 {
        let _ = server.poll().expect("poll");
    }
    let before = open_fds();

    for i in 0..3000 {
        cycle(&mut server, i);
    }
    for _ in 0..40 {
        let _ = server.poll().expect("poll");
        thread::sleep(Duration::from_millis(1));
    }
    let after = open_fds();

    eprintln!(
        "PROOF no leak: fds {before} -> {after}, slots {}, outbox {}",
        server.slots_len(),
        server.outbox_len()
    );
    // Two independent properties, and BOTH have to hold for this to mean
    // anything: the pool is bounded (an attacker cannot make it grow) and
    // the pool is released (a finished connection actually leaves it).
    // Removing either one on its own is caught here — removing the
    // MAX_SLOTS eviction alone is not, because a peer that closes is
    // retired by the `Closed` arm, so the revert-check for this test breaks
    // both.
    assert!(
        after <= before + 2,
        "3000 connect/stall/abandon cycles leaked fds: {before} -> {after}"
    );
    assert!(
        server.slots_len() <= wire::MAX_SLOTS,
        "the accept pool must stay bounded, got {}",
        server.slots_len()
    );
    assert!(
        server.outbox_len() <= wire::MAX_OUTBOX,
        "the outbox must stay bounded, got {}",
        server.outbox_len()
    );
    let _ = std::fs::remove_file(&addr);
}
