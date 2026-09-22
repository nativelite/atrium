//! The `ctl` control channel: a tiny, zero-dependency, **local** request/reply
//! transport — a Windows named pipe or a unix-domain socket, chosen at compile
//! time. It carries one JSON request line and one JSON reply line per client
//! connection; atrium ("the server") owns the endpoint and drains it from its run
//! loop without blocking, and each `atrium ctl <cmd>` invocation is a short-lived
//! client ([`request`]).
//!
//! The API is deliberately small:
//! * [`default_address`] — the per-process endpoint atrium binds and injects as
//!   `ATRIUM_CTL` into every pane it spawns.
//! * [`Listener::bind`] / [`Listener::poll`] / [`Listener::respond`] — the
//!   server side. `poll` is **non-blocking**: it returns `Ok(None)` when no
//!   request is ready, so it fits the 15 ms run-loop tick with no thread. One
//!   request is handed out at a time — the caller must
//!   [`respond`](Listener::respond) before the next `poll` yields another — but
//!   several half-open clients are accepted and parked concurrently, so no
//!   single stalled connection can wedge the channel.
//! * [`request`] — the client: connect, send one line, read one reply line.
//!
//! Framing is one `\n`-terminated line each way. This is a control plane for a
//! handful of agents, not a high-throughput bus.
//!
//! # The shape that keeps this honest
//!
//! Everything that can be wrong at the *byte* level lives in two dependency-free
//! modules — `wire` (framing, output queue, deadlines) and `chan` (the
//! per-connection state machine over a syscall seam) — plus `winmap`, which
//! turns documented Win32 `(ok, n, GetLastError)` triples into that seam's
//! vocabulary. All three compile on every platform and are exercised by the same
//! unit tests, so the Windows sequencing that cannot be executed on a mac is
//! still covered by tests that run there. The `sys` modules below hold only
//! syscalls and lifetime.
//!
//! **Same-user endpoint (peer-cred).** The endpoint is restricted to the user who
//! launched atrium: on unix the socket is 0600 and every accepted connection's peer
//! uid is checked against `geteuid` (`SO_PEERCRED` / `getpeereid`) — a mismatched
//! client is dropped before its request is read; on Windows the pipe is created
//! with a security descriptor granting access only to the current user and
//! SYSTEM, and with `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first instance, so a
//! process that pre-created the name makes atrium's `bind` fail loudly instead of
//! letting atrium attach as a second instance of someone else's pipe. This is
//! defense in depth *beneath* the per-pane capability token (`ATRIUM_TOKEN`), which
//! remains the primary authenticator.
//!
//! **What none of that stops — and why that is by design.** Peer-cred, mode 0600
//! and the SDDL all admit a process running as the *same user* — which is exactly
//! what a hosted agent is. That is deliberate: **defending against a same-user
//! attacker is an explicit non-goal.** atrium's security scope is fleet/agent safety
//! (a pane cannot impersonate a sibling or claim operator; credentials stay
//! scoped; a *different* user on a shared box is kept out). A process already
//! running as you has won by far easier paths — it can read your env, your files,
//! `~/.claude.json`, your OS vault — so there is nothing this channel can protect
//! that such an attacker does not already hold. Concretely: a same-uid process can
//! replace the unix endpoint (atrium raises a tripwire it happens to have there; see
//! [`Listener::take_security_event`]) or create an additional Windows pipe instance
//! under the same name and race atrium for clients (no tripwire on Windows — a named
//! pipe has no `(dev,ino)` to re-stat). Both are the same **same-user non-goal**,
//! not open defects. Truly closing them needs the server to *authenticate* itself
//! to its clients — real crypto, out of scope for a single-user local tool, and
//! deliberately not faked here with a non-cryptographic keyed hash.

use std::io;

/// The endpoint atrium binds for this process, unique per pid so two atrium
/// instances never collide. Injected as `ATRIUM_CTL` into spawned panes.
pub fn default_address() -> String {
    sys::default_address(std::process::id())
}

/// Idle duration in **milliseconds** from a **monotonic** clock: `now - last`,
/// saturating to `0` when `now` precedes `last`.
///
/// The daemon stamps a pane's last activity as a [`std::time::Instant`] when it
/// reads output from that pane, and computes idle here at status-reply time.
/// `Instant` is monotonic — unlike wall-clock `SystemTime`, an NTP step or clock
/// skew can never make idle jump forward or go negative — which is precisely why
/// the status field is derived from it. Pure (no clock read of its own), so it
/// unit-tests with two constructed instants; `=0` for a just-active pane.
pub fn idle_ms(last: std::time::Instant, now: std::time::Instant) -> u64 {
    now.saturating_duration_since(last).as_millis() as u64
}

/// The server endpoint. Owns the OS handle/socket and the parked connections.
pub struct Listener {
    sys: sys::Listener,
}

impl Listener {
    /// Bind the control endpoint at `addr`. On unix a stale socket file at the
    /// path is removed first; the socket file is unlinked on drop.
    pub fn bind(addr: &str) -> io::Result<Listener> {
        Ok(Listener {
            sys: sys::Listener::bind(addr)?,
        })
    }

    /// Non-blocking: the next complete request line if one has arrived, else
    /// `Ok(None)`. After a `Some(_)` the caller SHOULD call
    /// [`respond`](Listener::respond) before polling again — that is the
    /// contract, and it is how a caller gets its reply delivered.
    ///
    /// It is not, however, a latch. If a `poll` follows a `poll` with no
    /// `respond` between them (an early `?`, a panic caught upstream, a future
    /// refactor of `apply_ctl`), the un-answered client is released and the
    /// channel keeps serving. It used to be released only by `respond`, so a
    /// single missed reply killed the control plane — and this control plane
    /// gates `board claim`/`release`, the fleet's mutual exclusion.
    ///
    /// Also re-checks, a few times a second, that the endpoint still names the
    /// object atrium bound — see
    /// [`take_security_event`](Listener::take_security_event).
    pub fn poll(&mut self) -> io::Result<Option<String>> {
        self.sys.poll()
    }

    /// Send the reply line for the request the last [`poll`](Listener::poll)
    /// returned. A trailing newline is added if absent.
    ///
    /// **Never blocks.** What the kernel accepts now is written now; any
    /// remainder is parked and flushed by later [`poll`](Listener::poll) calls,
    /// with the connection held open until it drains (or a client that has
    /// stopped reading trips the stall deadline and is dropped). `Ok(())`
    /// therefore means *accepted for delivery*, not *the client has read it* —
    /// waiting for the latter is exactly what froze the run loop.
    ///
    /// Two error cases, both of which the caller can simply log:
    /// * `InvalidData` — the reply is over `wire::MAX_REPLY`. It was NOT sent;
    ///   the client is told so explicitly rather than left to fail on its own
    ///   cap. The size limit belongs to the writer as much as to the reader.
    /// * `WouldBlock` / `BrokenPipe` — no room to park this reply behind the
    ///   ones already draining, or the client is already gone. The client sees
    ///   EOF before a newline, which its `read_reply` reports as an error.
    pub fn respond(&mut self, reply: &str) -> io::Result<()> {
        self.sys.respond(reply)
    }

    /// Take the last endpoint-integrity alarm, if one fired.
    ///
    /// On unix the ctl socket path lives in a directory the user owns, so a
    /// same-uid process can `unlink()` it and `bind()` its own socket in its
    /// place: the file mode is no defense, because the authority to unlink comes
    /// from the directory. atrium records the socket's `(dev, ino)` at bind time
    /// and re-`stat`s the path a few times a second; a mismatch means the name
    /// no longer refers to atrium's socket. It is a tripwire, not a lock: an
    /// attacker who replaces the socket, harvests a token and restores the
    /// original inside the check interval is not seen at all, and by the time
    /// the alarm fires the token has already gone to the squatter. That is a
    /// security event for the
    /// operator to see, not something this layer can repair — authenticating the
    /// server to its clients is a separate decision.
    pub fn take_security_event(&mut self) -> Option<String> {
        self.sys.take_security_event()
    }

    /// Replies still draining to slow readers. Tests only.
    #[cfg(all(test, unix))]
    pub fn outbox_len(&self) -> usize {
        self.sys.outbox_len()
    }

    /// Half-open clients parked in the accept pool. Tests only: a leak test has
    /// to be able to see that the pool returns to a steady state.
    #[cfg(all(test, unix))]
    pub fn slots_len(&self) -> usize {
        self.sys.slots_len()
    }
}

/// Client: connect to `addr`, send `req` (one line), return the reply line
/// (newline trimmed). Used by `atrium ctl <cmd>`.
///
/// The reply is bounded (`wire::MAX_REPLY`) and deadlined, and a connection that
/// ends before a newline is an error, never a truncated success.
pub fn request(addr: &str, req: &str) -> io::Result<String> {
    sys::request(addr, req)
}

#[allow(dead_code)]
mod chan;
#[allow(dead_code)]
mod winmap;
#[allow(dead_code)]
mod wire;

#[cfg(windows)]
mod sys_windows;
#[cfg(windows)]
use sys_windows as sys;
#[cfg(unix)]
mod sys_unix;
#[cfg(unix)]
use sys_unix as sys;

#[cfg(all(test, unix))]
mod exploit_replays;
#[cfg(test)]
mod guards;
#[cfg(test)]
mod tests;
#[cfg(all(test, unix))]
mod tests_unix;
#[cfg(test)]
mod testutil;
