//! The per-connection state machine, over a scripted seam.

use super::*;
use crate::ipc::wire::DRAIN_BUDGET;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A connection whose syscalls are scripted: the seam that lets the same
/// tests drive the unix and the Windows state machines.
struct Fake {
    reads: VecDeque<Recv>,
    /// Bytes each successive `send` is allowed to accept.
    accepts: VecDeque<usize>,
    written: Vec<u8>,
    gone: bool,
}

impl Fake {
    fn new() -> Fake {
        Fake {
            reads: VecDeque::new(),
            accepts: VecDeque::new(),
            written: Vec::new(),
            gone: false,
        }
    }
}

impl Conn for Fake {
    fn recv(&mut self, _buf: &mut [u8]) -> Recv {
        self.reads.pop_front().unwrap_or(Recv::WouldBlock)
    }
    fn send(&mut self, buf: &[u8]) -> Sent {
        let cap = self.accepts.pop_front().unwrap_or(buf.len());
        let n = cap.min(buf.len());
        if n == 0 {
            return Sent::WouldBlock;
        }
        self.written.extend_from_slice(&buf[..n]);
        Sent::Wrote(n)
    }
    fn peer_gone(&mut self) -> bool {
        self.gone
    }
}

/// **D1 / D3, in the shared core.** A reply the kernel will not take in one
/// go must be parked and finished, never truncated. `write_all` returned
/// `Err(WouldBlock)` *after* a partial write, which is how `ctl audit`
/// delivered 8 192 of 120 631 bytes.
#[test]
fn channel_parks_a_reply_the_kernel_would_not_take() {
    let t0 = Instant::now();
    let mut fake = Fake::new();
    // 8 KiB, then a refusal, then the rest: exactly a full send buffer.
    fake.accepts.extend([8192, 0, 8192, 8192, 8192]);
    let mut ch = Channel::new(fake, t0);
    let body = "z".repeat(30_000);
    ch.queue(&body, t0);

    assert_eq!(ch.pump_write(t0), WriteOutcome::Pending);
    let mut ticks = 0;
    while ch.pump_write(t0 + Duration::from_millis(15 * ticks)) == WriteOutcome::Pending {
        ticks += 1;
        assert!(ticks < 100, "reply never finished");
    }
    let mut expect = body.into_bytes();
    expect.push(b'\n');
    let got = ch.conn_mut().written.clone();
    assert_eq!(got.len(), expect.len(), "the whole reply must be delivered");
    assert_eq!(got, expect);
}

/// The stall deadline is anchored on the last byte the kernel accepted, so a
/// slow-but-honest reader is served while a client that stopped reading is
/// dropped. Proven with injected instants — no sleeping.
#[test]
fn channel_write_stall_is_anchored_on_progress() {
    let t0 = Instant::now();
    let mut fake = Fake::new();
    fake.accepts.extend([10, 0, 0, 0]);
    let mut ch = Channel::new(fake, t0);
    ch.queue(&"z".repeat(100), t0);

    // A tick 4 s later that still moves bytes re-anchors the clock…
    assert_eq!(
        ch.pump_write(t0 + Duration::from_secs(4)),
        WriteOutcome::Pending
    );
    assert!(!ch.write_stalled(t0 + Duration::from_secs(8)));
    // …and only silence past WRITE_STALL from that point counts.
    assert!(ch.write_stalled(t0 + Duration::from_secs(10)));
}

/// Slot eviction and the half-sent-request deadline, again with no sleeping.
#[test]
fn channel_read_deadline_uses_injected_time() {
    let t0 = Instant::now();
    let mut fake = Fake::new();
    fake.reads.push_back(Recv::Data(4));
    let mut ch = Channel::new(fake, t0);
    let mut buf = *b"{\"cm";
    assert_eq!(ch.pump_read(&mut buf, t0), ReadOutcome::Pending);
    assert!(!ch.read_expired(t0 + Duration::from_millis(1999)));
    assert!(ch.read_expired(t0 + Duration::from_millis(2001)));
}

/// An oversize request is reported as a refusal, never handed upstream as a
/// fragment.
#[test]
fn channel_refuses_an_oversize_request_instead_of_fragmenting() {
    let t0 = Instant::now();
    let mut fake = Fake::new();
    for _ in 0..20 {
        fake.reads.push_back(Recv::Data(8192));
    }
    let mut ch = Channel::new(fake, t0);
    let mut buf = [b'x'; 8192];
    assert_eq!(ch.pump_read(&mut buf, t0), ReadOutcome::Oversize);
}

// =======================================================================
// Windows: the replica harness. `winmap` turns documented Win32 triples
// into the seam's vocabulary, and `Channel` sequences them — the same
// `Channel` the real Windows `Listener` drives. Driving it here with the
// documented return sequences is the closest thing to running Win32 that a
// mac allows; every assertion below is about logic that ships on Windows.
// =======================================================================

/// **Defect in both candidates.** `drain_input`'s only bound was the step
/// count, so one refused client could hand the run loop a ~16 MB read budget
/// in a single tick — far more than anything else in the loop takes, and a
/// size the attacker picks.
#[test]
fn drain_input_is_bounded_by_a_byte_budget() {
    /// A peer that always has more to say and never closes.
    struct Firehose {
        handed: usize,
    }
    impl Conn for Firehose {
        fn recv(&mut self, buf: &mut [u8]) -> Recv {
            self.handed += buf.len();
            Recv::Data(buf.len())
        }
        fn send(&mut self, buf: &[u8]) -> Sent {
            Sent::Wrote(buf.len())
        }
    }

    let mut ch = Channel::new(Firehose { handed: 0 }, Instant::now());
    let mut buf = [0u8; 4096];
    let (consumed, closed) = ch.drain_input(&mut buf);
    assert!(consumed && !closed);
    let handed = ch.conn_mut().handed;
    assert!(
        handed <= DRAIN_BUDGET,
        "one drain took {handed} bytes from a client that never stops; the \
         per-tick budget is {DRAIN_BUDGET}"
    );
}
