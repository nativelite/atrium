// ---------------------------------------------------------------------------
// chan: the per-connection state machine, over a syscall seam. Pure.
// ---------------------------------------------------------------------------
use super::wire::{expired, Framed, Framer, OutQueue, CLIENT_DEADLINE, DRAIN_BUDGET, WRITE_STALL};
use std::time::Instant;

/// One non-blocking read attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Recv {
    /// `n` bytes are in the buffer.
    Data(usize),
    /// Nothing available; the connection is fine. Come back next tick.
    WouldBlock,
    /// Interrupted before anything was read; try again immediately.
    Retry,
    /// Peer closed, or the connection is unusable.
    Closed,
}

/// One non-blocking write attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    /// `n` bytes were accepted by the kernel. `n` may legitimately be less
    /// than offered — that is the whole reason this type exists.
    Wrote(usize),
    /// The kernel took nothing; the buffer is full. Retry next tick.
    WouldBlock,
    /// The connection is unusable.
    Failed,
}

/// The syscall seam. Implementations do nothing but call the OS and classify
/// the result; every decision made from those results lives in [`Channel`].
pub trait Conn {
    fn recv(&mut self, buf: &mut [u8]) -> Recv;
    fn send(&mut self, buf: &[u8]) -> Sent;
    /// Has the peer hung up? Only Windows needs this (to know a reply was
    /// consumed before `DisconnectNamedPipe` discards it); unix learns the
    /// same thing by simply closing.
    fn peer_gone(&mut self) -> bool {
        false
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    Pending,
    Ready(String),
    /// Over the cap: the caller owes this client an error reply and then a
    /// close. Never a silent drop, never a fragment handed upstream.
    Oversize,
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Pending,
    Flushed,
    Failed,
}

/// One client connection: the bytes owed in each direction plus the deadline
/// anchor. Deliberately ignorant of sockets, pipes and clocks — `now` is
/// passed in, so slot eviction and stall handling are testable without
/// sleeping.
pub struct Channel<C: Conn> {
    conn: C,
    framer: Framer,
    out: OutQueue,
    since: Instant,
}

/// Bound on steps per pump so a client that streams without ever sending a
/// newline cannot own the tick. `Framer` caps the buffer at MAX_REQUEST, so
/// this only guards a pathological zero-progress `Retry` loop.
const MAX_PUMP_STEPS: usize = 4096;

impl<C: Conn> Channel<C> {
    pub fn new(conn: C, now: Instant) -> Channel<C> {
        Channel {
            conn,
            framer: Framer::new(),
            out: OutQueue::new(),
            since: now,
        }
    }

    pub fn conn_mut(&mut self) -> &mut C {
        &mut self.conn
    }

    /// Re-arm for a fresh client on the same underlying connection (Windows
    /// reuses pipe instances; unix does not).
    pub fn reset(&mut self, now: Instant) {
        self.framer.clear();
        self.out.clear();
        self.since = now;
    }

    pub fn buffered(&self) -> usize {
        self.framer.buffered()
    }

    pub fn since(&self) -> Instant {
        self.since
    }

    /// A client that has held a half-sent request past [`CLIENT_DEADLINE`].
    pub fn read_expired(&self, now: Instant) -> bool {
        expired(self.since, now, CLIENT_DEADLINE)
    }

    /// A reply that has made no progress at all for [`WRITE_STALL`]. The
    /// anchor moves on every accepted byte, so this fires only on a client
    /// that has genuinely stopped reading.
    pub fn write_stalled(&self, now: Instant) -> bool {
        expired(self.since, now, WRITE_STALL)
    }

    /// Read whatever is available without ever waiting.
    pub fn pump_read(&mut self, buf: &mut [u8], _now: Instant) -> ReadOutcome {
        for _ in 0..MAX_PUMP_STEPS {
            match self.conn.recv(buf) {
                Recv::Data(0) => return ReadOutcome::Closed,
                Recv::Data(n) => {
                    let n = n.min(buf.len());
                    match self.framer.feed(&buf[..n]) {
                        Framed::Line(line) => return ReadOutcome::Ready(line),
                        Framed::Overflow => return ReadOutcome::Oversize,
                        Framed::Incomplete => continue,
                    }
                }
                Recv::Retry => continue,
                Recv::WouldBlock => return ReadOutcome::Pending,
                Recv::Closed => return ReadOutcome::Closed,
            }
        }
        ReadOutcome::Pending
    }

    /// Queue a reply and re-anchor the stall deadline.
    pub fn queue(&mut self, reply: &str, now: Instant) {
        self.out.queue_line(reply);
        self.since = now;
    }

    /// Hand the kernel as much of the queued reply as it will take, right
    /// now, without waiting for the client to read.
    ///
    /// This is the fix for the truncation: `write_all` retries only on
    /// `Interrupted`, so on a non-blocking socket it returned `WouldBlock`
    /// *after a partial write* and the reply was silently cut at one send
    /// buffer (8 192 bytes on macOS). Anything the kernel will not take now
    /// stays queued for the next tick, and the connection stays open.
    pub fn pump_write(&mut self, now: Instant) -> WriteOutcome {
        for _ in 0..MAX_PUMP_STEPS {
            if self.out.is_empty() {
                return WriteOutcome::Flushed;
            }
            match self.conn.send(self.out.remaining()) {
                Sent::Wrote(0) | Sent::WouldBlock => return WriteOutcome::Pending,
                Sent::Wrote(n) => {
                    self.out.advance(n);
                    self.since = now; // progress: the stall clock restarts
                }
                Sent::Failed => return WriteOutcome::Failed,
            }
        }
        WriteOutcome::Pending
    }

    pub fn out_empty(&self) -> bool {
        self.out.is_empty()
    }

    /// Read and throw away whatever the peer is still sending; `true` once it
    /// has closed.
    ///
    /// Needed for a client whose request was refused mid-flight: it is still
    /// pushing the tail of an oversize body, and if atrium simply stopped
    /// reading and closed, the unread bytes would turn the close into a reset
    /// and the client would lose the error reply it is owed. So keep the
    /// receive queue empty while the refusal goes out, and let go once the
    /// client hangs up (or the drain deadline expires).
    /// Returns `(consumed anything, peer closed)`.
    ///
    /// Bounded by [`DRAIN_BUDGET`] bytes as well as by the step count: the
    /// step count alone let one refused client hand the run loop a ~16 MB
    /// read budget in a single tick, which is far more than anything else in
    /// the loop takes and is chosen by the attacker, not by atrium.
    pub fn drain_input(&mut self, buf: &mut [u8]) -> (bool, bool) {
        let mut consumed = false;
        let mut budget = DRAIN_BUDGET;
        for _ in 0..MAX_PUMP_STEPS {
            if budget == 0 {
                return (consumed, false);
            }
            let take = budget.min(buf.len());
            match self.conn.recv(&mut buf[..take]) {
                Recv::Data(n) => {
                    consumed |= n > 0;
                    budget = budget.saturating_sub(n.max(1));
                }
                Recv::Retry => continue,
                Recv::WouldBlock => return (consumed, false),
                Recv::Closed => return (consumed, true),
            }
        }
        (consumed, false)
    }

    /// Re-anchor the deadline without touching the buffers.
    pub fn touch(&mut self, now: Instant) {
        self.since = now;
    }

    pub fn peer_gone(&mut self) -> bool {
        self.conn.peer_gone()
    }
}

#[cfg(test)]
mod tests;
