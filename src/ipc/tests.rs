//! The public transport API, plus the Windows sequencing replayed
//! through pure `chan` so it is covered on every platform.

use super::chan::{Channel, Conn, ReadOutcome, Recv, Sent, WriteOutcome};
use super::testutil::{test_addr, windows_sys_source};
use super::winmap::{ERROR_BROKEN_PIPE, ERROR_MORE_DATA, ERROR_NO_DATA};
use super::*;
use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn idle_ms_is_monotonic_and_saturating() {
    let t0 = Instant::now();
    // now after the stamp → the elapsed millis.
    assert_eq!(idle_ms(t0, t0 + Duration::from_millis(4_000)), 4_000);
    // just-active pane (now == last) → 0.
    assert_eq!(idle_ms(t0, t0), 0);
    // now BEFORE the stamp (a non-monotonic surprise) → 0, never a wrap.
    assert_eq!(idle_ms(t0 + Duration::from_millis(4_000), t0), 0);
}

/// A pipe whose scripted `ReadFile`/`WriteFile` results are the documented
/// Win32 triples.
#[derive(Default)]
struct WinPipe {
    reads: VecDeque<(i32, Vec<u8>, i32)>,
    writes: VecDeque<(i32, usize, i32)>,
    written: Vec<u8>,
    /// What the NOWAIT `ReadFile` behind `peer_gone` reports: the client has
    /// closed its handle, so `GetLastError` is ERROR_BROKEN_PIPE.
    hung_up: bool,
}

impl Conn for WinPipe {
    fn recv(&mut self, buf: &mut [u8]) -> Recv {
        match self.reads.pop_front() {
            Some((ok, data, err)) => {
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                winmap::read_outcome(ok, n, err)
            }
            None => winmap::read_outcome(0, 0, ERROR_NO_DATA),
        }
    }
    fn send(&mut self, buf: &[u8]) -> Sent {
        let (ok, cap, err) = self.writes.pop_front().unwrap_or((1, buf.len(), 0));
        let n = cap.min(buf.len());
        self.written.extend_from_slice(&buf[..n]);
        winmap::write_outcome(ok, n, err)
    }
    fn peer_gone(&mut self) -> bool {
        let err = if self.hung_up {
            ERROR_BROKEN_PIPE
        } else {
            ERROR_NO_DATA
        };
        winmap::drain_outcome(0, 0, err)
    }
}

/// The decision the Windows `service_writes` makes for an instance in
/// `Phase::Writing` whose `pump_write` came back `Pending`, lifted verbatim
/// so a mac can drive it: recycle, or keep waiting?
fn win_writing_gives_up(ch: &mut Channel<WinPipe>, now: Instant) -> bool {
    ch.peer_gone() || ch.write_stalled(now)
}

/// **A Windows-only stall this round found and closed.** `write_outcome`
/// maps ERROR_NO_DATA (232) at `WriteFile` to `WouldBlock`, not `Failed`,
/// on purpose: 232 is documented as "the pipe is being closed", but a
/// NOWAIT pipe reports a *full buffer* through a path that is not cleanly
/// separable from it, and guessing "closed" would drop a reply the client
/// is still owed. Guessing "full" only costs time — but on its own it costs
/// the FULL `WRITE_STALL` (5 s) per instance, so eight clients that connect,
/// ask and die take the entire eight-instance pool out for five seconds.
///
/// The ambiguity is resolved by asking instead of guessing: `peer_gone` is a
/// NOWAIT `ReadFile` whose ERROR_BROKEN_PIPE is unambiguous.
#[test]
fn windows_replica_gives_up_at_once_on_a_reader_that_hung_up() {
    let t0 = Instant::now();

    // Live client, buffer full: the write is parked and the instance is
    // KEPT — this is the slow-but-honest reader, and dropping it would be
    // the truncation defect all over again.
    let mut live = Channel::new(
        WinPipe {
            writes: VecDeque::from(vec![(0, 0, ERROR_NO_DATA)]),
            ..WinPipe::default()
        },
        t0,
    );
    live.queue("{\"ok\":true}", t0);
    assert_eq!(live.pump_write(t0), WriteOutcome::Pending);
    assert!(
        !win_writing_gives_up(&mut live, t0 + Duration::from_secs(4)),
        "a client that is merely slow must keep its instance"
    );

    // Same write result, but the client has closed its handle.
    let mut dead = Channel::new(
        WinPipe {
            writes: VecDeque::from(vec![(0, 0, ERROR_NO_DATA)]),
            hung_up: true,
            ..WinPipe::default()
        },
        t0,
    );
    dead.queue("{\"ok\":true}", t0);
    assert_eq!(dead.pump_write(t0), WriteOutcome::Pending);
    assert!(
        win_writing_gives_up(&mut dead, t0 + Duration::from_millis(15)),
        "a client that hung up must free its pipe instance on the very next \
         tick, not after the full WRITE_STALL"
    );

    // `win_writing_gives_up` is a LIFT of a decision that lives in
    // cfg(windows) code this mac cannot execute — so on its own it would
    // stay green while the shipping code drifted away from it, which is
    // precisely the "test that passes with the fix reverted" shape. Anchor
    // it: the shipping Writing arm must make the same call.
    let writing = windows_sys_source()
        .split("Phase::Writing => match")
        .nth(1)
        .expect("the Writing arm of service_writes")
        .split("Phase::Draining =>")
        .next()
        .expect("up to the Draining arm");
    assert!(
        writing.contains("peer_gone()"),
        "the shipping Windows Writing arm must ask whether the peer hung up, \
         not wait out WRITE_STALL on every ambiguous WriteFile result"
    );
}

/// **D4, end to end on the shipping state machine.** A real
/// `ctl send reviewer <20 KB>` builds a 20 055-byte request. Message mode
/// used to hand the run loop a 3 671-byte tail that began mid-payload — and
/// a crafted tail that happens to be valid JSON would have EXECUTED while
/// the head, and the audit `detail`, were discarded.
#[test]
fn windows_replica_reassembles_a_20kb_request() {
    let payload = "p".repeat(20_000);
    let req = format!(r#"{{"cmd":"send","target":"reviewer","text":"{payload}"}}"#);
    let mut wire: Vec<u8> = req.clone().into_bytes();
    wire.push(b'\n');
    assert!(wire.len() > 16_384, "the exploit needs three reads");

    let mut reads = VecDeque::new();
    let mut off = 0;
    while wire.len() - off > 8192 {
        // Every read but the last reports ERROR_MORE_DATA with a full buffer.
        reads.push_back((0, wire[off..off + 8192].to_vec(), ERROR_MORE_DATA));
        off += 8192;
    }
    reads.push_back((1, wire[off..].to_vec(), 0));

    let t0 = Instant::now();
    let mut ch = Channel::new(
        WinPipe {
            reads,
            ..WinPipe::default()
        },
        t0,
    );
    // Tick like the run loop does: a WouldBlock only means "next tick".
    let mut buf = [0u8; 8192];
    let mut got = None;
    for tick in 0..8 {
        match ch.pump_read(&mut buf, t0 + Duration::from_millis(15 * tick)) {
            ReadOutcome::Ready(line) => {
                got = Some(line);
                break;
            }
            ReadOutcome::Pending => continue,
            other => panic!("the request was destroyed: {other:?}"),
        }
    }
    let line = got.expect("the request never arrived at all");
    assert_eq!(
        line.len(),
        req.len(),
        "a {}-byte FRAGMENT reached the run loop instead of the {}-byte \
         request: it begins {:?}",
        line.len(),
        req.len(),
        &line[..line.len().min(40)]
    );
    assert_eq!(line, req);
}

/// **D2 / D3.** A reply larger than the 8 KiB pipe buffer is delivered over
/// as many ticks as it takes, with `WriteFile` reporting SUCCESS/zero in
/// between — and with nothing anywhere that waits on the client, which is
/// what `FlushFileBuffers` did on the run-loop thread.
#[test]
fn windows_replica_delivers_a_reply_larger_than_the_pipe_buffer() {
    let t0 = Instant::now();
    let reply = format!(r#"{{"ok":true,"detail":"{}"}}"#, "d".repeat(120_600));
    let mut writes = VecDeque::new();
    for i in 0..40 {
        // Alternate: a full buffer accepted, then SUCCESS-with-zero.
        writes.push_back((1, 8192, 0));
        if i % 2 == 0 {
            writes.push_back((1, 0, 0));
        }
    }
    let mut ch = Channel::new(
        WinPipe {
            writes,
            ..WinPipe::default()
        },
        t0,
    );
    ch.queue(&reply, t0);
    let mut tick = 0u32;
    loop {
        match ch.pump_write(t0 + Duration::from_millis(15 * u64::from(tick))) {
            WriteOutcome::Flushed => break,
            WriteOutcome::Pending => {
                tick += 1;
                assert!(tick < 200, "reply never finished");
            }
            WriteOutcome::Failed => panic!("write failed"),
        }
    }
    let mut expect = reply.into_bytes();
    expect.push(b'\n');
    assert_eq!(ch.conn_mut().written.len(), expect.len());
    assert_eq!(ch.conn_mut().written, expect);
}

// =======================================================================
// The real transports.
// =======================================================================

/// Full roundtrip over the real transport: bind a listener, dial it from a
/// client thread, and confirm the server sees the request and the client
/// sees the reply. This is the C0 spike promoted to a permanent test.
#[test]
fn roundtrip_request_reply() {
    let addr = test_addr(1);
    let mut server = Listener::bind(&addr).expect("bind");
    let addr2 = addr.clone();

    let client = thread::spawn(move || request(&addr2, r#"{"cmd":"list"}"#).expect("request"));

    // Serve one request within a bounded number of non-blocking ticks.
    let mut reply_sent = false;
    for _ in 0..400 {
        if let Some(req) = server.poll().expect("poll") {
            assert!(req.contains("\"list\""), "server saw: {req}");
            server.respond(r#"{"ok":true,"tree":[]}"#).expect("respond");
            reply_sent = true;
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(reply_sent, "server never saw the request");

    // Keep ticking: the reply drains through `poll` like the run loop does.
    for _ in 0..200 {
        let _ = server.poll();
        thread::sleep(Duration::from_millis(2));
    }
    let reply = client.join().expect("client thread");
    assert!(reply.contains("\"ok\":true"), "client saw: {reply}");
}

#[test]
fn poll_is_non_blocking_when_idle() {
    let addr = test_addr(2);
    let mut server = Listener::bind(&addr).expect("bind");
    // With no client, poll must return immediately with None, not hang.
    for _ in 0..10 {
        assert_eq!(server.poll().expect("poll"), None);
    }
}
