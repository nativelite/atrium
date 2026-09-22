// ---------------------------------------------------------------------------
// wire: the byte-level rules. Pure, platform-independent, no syscalls.
//
// `allow(dead_code)`: this module and `chan` are the SHARED core, so a knob one
// platform needs (`DRAIN_DEADLINE` is a named-pipe concern; `Channel::reset`
// re-arms a pipe instance) reads as unused on the other. Keeping one core with a
// couple of platform-specific entry points is the point of the shape.
// ---------------------------------------------------------------------------
use std::time::{Duration, Instant};

/// Refuse an over-long request rather than buffering it without limit.
pub const MAX_REQUEST: usize = 64 * 1024;
/// Ceiling on one reply, asserted at BOTH ends.
///
/// It used to be a client-only rule: `respond` would frame a reply of any
/// size and the client refused past 8 MiB with `InvalidData`, so a `ctl
/// audit` that grew past the cap failed with an error blaming the channel
/// rather than an answer from atrium. The server now refuses first, with an
/// explicit reply naming the limit — a cap the writer does not know about is
/// the same class of bug as a fix applied to a writer but not its readers.
pub const MAX_REPLY: usize = 8 * 1024 * 1024;
/// The reply that a caller-supplied answer over [`MAX_REPLY`] is replaced
/// with, so the client gets a diagnosis instead of a channel error.
pub const TOO_LARGE_REPLY: &str =
    r#"{"ok":false,"error":"atrium reply exceeded 8388608 bytes and was not sent"}"#;
/// How long a client may hold a half-sent request before atrium drops it.
/// Wall-clock across ticks, not per read.
pub const CLIENT_DEADLINE: Duration = Duration::from_secs(2);
/// How long a reply may make **no progress at all** before atrium gives up on
/// the connection. Anchored on the last byte the kernel accepted, so a slow
/// but honest reader is served for as long as it keeps reading, while a
/// client that stops reading is dropped instead of held forever.
pub const WRITE_STALL: Duration = Duration::from_secs(5);
/// After the last byte is written, how long to wait for the client to close
/// (proof it read everything) before letting go anyway.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(5);
/// How long a refused client may be quiet before atrium stops holding the
/// connection open for it. Short: it exists only to keep the receive queue
/// empty long enough that closing delivers the refusal instead of a reset.
pub const DRAIN_GRACE: Duration = Duration::from_millis(250);
/// How many half-open clients may be parked at once. Bounded on purpose:
/// unbounded accept just moves exhaustion from the channel to the fd table.
pub const MAX_SLOTS: usize = 8;
/// How many replies may still be draining at once.
pub const MAX_OUTBOX: usize = 8;
/// How many connections to accept in a single tick. Deliberately no more
/// than half [`MAX_SLOTS`]: accepting more per tick than the pool can hold
/// turns eviction into a client-killer, because a burst that lands behind a
/// client whose request is already complete would push that client out
/// before it is ever read.
pub const ACCEPT_PER_TICK: usize = 4;
/// How many of [`MAX_OUTBOX`]'s slots refused (oversize) clients may occupy.
/// Reserving the rest means an attacker spraying oversize requests cannot
/// crowd out replies owed to legitimate callers.
pub const MAX_LINGER: usize = MAX_OUTBOX / 2;
/// Bytes a single `drain_input` call will swallow from a refused client. The
/// step budget alone allowed ~16 MB per tick, which is a far bigger per-tick
/// bite than anything else in the run loop takes.
pub const DRAIN_BUDGET: usize = 64 * 1024;

// -- client-side budgets -------------------------------------------------
//
// These used to be four unrelated literals: `Duration::from_secs(5)` at the
// Windows connect, a `stall`/`hard` pair inside the Windows `read_reply`,
// and a `STALL`/`HARD_DEADLINE` pair inside the unix one. Nothing tied them
// together, so "how long may a hostile endpoint hold `atrium ctl`?" had two
// different answers depending on the platform. One place now.

/// How long a client tolerates a server that sends **nothing at all**. This
/// is the bound that fires against a silent or wedged endpoint.
pub const REPLY_STALL: Duration = Duration::from_secs(5);
/// The ceiling on one whole client exchange — connect, request, reply —
/// measured once at the start and never re-armed, because a per-read timeout
/// that re-arms is how a dripping server held `atrium ctl` open forever.
///
/// 30 s, and the number is measured rather than wished for. atrium hands a
/// parked reply to the kernel once per 15 ms run-loop tick, one socket
/// buffer at a time; on this mac that delivers 120 623 bytes (a real `ctl
/// audit`) in 324 ms, 1 MB in 2.53 s and 4 MiB in 10.59 s — about 400 KB/s.
/// A full [`MAX_REPLY`] therefore needs roughly 21 s of honest transfer, so
/// a 5 s budget would have turned every reply over ~2 MB into `TimedOut`:
/// the reader/writer mismatch above, moved one layer out. 30 s leaves 1.4x
/// margin over the slowest legitimate case and still bounds a hostile
/// endpoint, which cannot exceed it at all.
pub const REPLY_DEADLINE: Duration = Duration::from_secs(30);
/// How long a client naps between non-blocking attempts. Well under the
/// 15 ms tick, so it costs no round trip in practice.
pub const CLIENT_POLL: Duration = Duration::from_millis(2);
/// How often the server re-checks that its endpoint is still its own.
///
/// A tripwire, not a lock: an attacker who hard-links the socket file aside,
/// unlinks it, binds their own, harvests a token and renames the original
/// back restores the exact `(dev, ino)` and slips through the window. 100 ms
/// keeps that window small — a `stat` is a couple of microseconds against a
/// 15 ms tick — without pretending it is closed.
pub const ENDPOINT_CHECK_EVERY: Duration = Duration::from_millis(100);
/// How often the server may report refused peers. The report used to be an
/// unconditional `eprint!` inside the accept loop, i.e. an unbounded
/// terminal write driven by whoever is connecting.
pub const REFUSAL_REPORT_EVERY: Duration = Duration::from_secs(1);
/// The reply an oversize request gets. A rejected client must hear *why*,
/// not just see the socket close.
pub const OVERSIZE_REPLY: &str = r#"{"ok":false,"error":"ctl request exceeded 65536 bytes"}"#;

/// The error `respond` hands back for a reply over [`MAX_REPLY`]. One
/// function rather than one copy per platform: this file already shipped a
/// cap the writer did not know about, and two hand-written copies of the
/// refusal is how the next divergence starts.
pub fn reply_too_large(len: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "ctl reply of {len} bytes exceeds the {MAX_REPLY}-byte limit; \
             the client was told, the answer was not sent"
        ),
    )
}

/// What one chunk of client bytes did to the request being assembled.
#[derive(Debug, PartialEq, Eq)]
pub enum Framed {
    /// A complete request line (newline stripped).
    Line(String),
    /// Not yet — keep the buffer, try again next tick.
    Incomplete,
    /// Over [`MAX_REQUEST`]; the caller owes the client an error reply.
    Overflow,
}

/// Accumulates a `\n`-framed request across however many reads it takes.
#[derive(Default)]
pub struct Framer {
    pending: Vec<u8>,
}

impl Framer {
    pub fn new() -> Framer {
        Framer {
            pending: Vec::new(),
        }
    }

    /// Feed one chunk.
    ///
    /// The size cap is checked on **both** exits. It used to be tested only
    /// on the no-newline path, which made the real limit `MAX_REQUEST` plus
    /// one whole read (69 631 bytes for a 4 KiB reader) instead of the
    /// documented 65 536 — a small hole, but a documented limit that is not
    /// the enforced limit is a lie the next reader builds on.
    pub fn feed(&mut self, chunk: &[u8]) -> Framed {
        if let Some(pos) = chunk.iter().position(|b| *b == b'\n') {
            if self.pending.len() + pos > MAX_REQUEST {
                self.pending.clear();
                return Framed::Overflow;
            }
            self.pending.extend_from_slice(&chunk[..pos]);
            let line = String::from_utf8_lossy(&self.pending)
                .trim_end_matches('\r')
                .to_string();
            self.pending.clear();
            // One request per connection: anything after the newline is not
            // ours to interpret, and feeding a tail back as a fresh request
            // is how the Windows path grew its own bug.
            return Framed::Line(line);
        }
        if self.pending.len() + chunk.len() > MAX_REQUEST {
            self.pending.clear();
            return Framed::Overflow;
        }
        self.pending.extend_from_slice(chunk);
        Framed::Incomplete
    }

    pub fn buffered(&self) -> usize {
        self.pending.len()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

/// A reply that may take several ticks to hand to the kernel.
#[derive(Default)]
pub struct OutQueue {
    buf: Vec<u8>,
    off: usize,
}

impl OutQueue {
    pub fn new() -> OutQueue {
        OutQueue {
            buf: Vec::new(),
            off: 0,
        }
    }

    /// Queue `line`, adding the framing newline if absent.
    pub fn queue_line(&mut self, line: &str) {
        self.buf.clear();
        self.off = 0;
        self.buf.extend_from_slice(line.as_bytes());
        if !line.ends_with('\n') {
            self.buf.push(b'\n');
        }
    }

    pub fn remaining(&self) -> &[u8] {
        &self.buf[self.off..]
    }

    pub fn advance(&mut self, n: usize) {
        self.off = (self.off + n).min(self.buf.len());
    }

    pub fn is_empty(&self) -> bool {
        self.off >= self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.off = 0;
    }
}

/// `now - since > limit`, saturating (a non-monotonic surprise reads as "not
/// expired" rather than panicking on the run loop).
pub fn expired(since: Instant, now: Instant, limit: Duration) -> bool {
    now.saturating_duration_since(since) > limit
}

#[cfg(test)]
mod tests;
