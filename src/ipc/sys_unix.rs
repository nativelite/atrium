// ---------------------------------------------------------------------------
// Unix: a non-blocking `UnixListener` and a bounded pool of half-open clients.
// ---------------------------------------------------------------------------
use super::chan::{Channel, Conn, ReadOutcome, Recv, Sent, WriteOutcome};
use super::wire::{
    expired, reply_too_large, ACCEPT_PER_TICK, CLIENT_POLL, DRAIN_DEADLINE, DRAIN_GRACE,
    ENDPOINT_CHECK_EVERY, MAX_LINGER, MAX_OUTBOX, MAX_REPLY, MAX_SLOTS, OVERSIZE_REPLY,
    REFUSAL_REPORT_EVERY, REPLY_DEADLINE, REPLY_STALL, TOO_LARGE_REPLY,
};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

extern "C" {
    fn geteuid() -> u32;
}

/// The effective uid of the connected peer, if the OS can report it. `None`
/// means the credential could not be read — which the caller treats as a
/// REFUSAL, not as permission. An unverifiable peer is exactly the one to turn
/// away.
///
/// Linux/Android use `SO_PEERCRED`; macOS/BSD use `getpeereid`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(fd: std::os::unix::io::RawFd) -> Option<u32> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    extern "C" {
        fn getsockopt(
            fd: i32,
            level: i32,
            optname: i32,
            optval: *mut c_void,
            optlen: *mut u32,
        ) -> i32;
    }
    const SOL_SOCKET: i32 = 1;
    const SO_PEERCRED: i32 = 17;
    let mut cred = Ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<Ucred>() as u32;
    let r = unsafe {
        getsockopt(
            fd,
            SOL_SOCKET,
            SO_PEERCRED,
            &mut cred as *mut Ucred as *mut c_void,
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_uid(fd: std::os::unix::io::RawFd) -> Option<u32> {
    extern "C" {
        fn getpeereid(fd: i32, euid: *mut u32, egid: *mut u32) -> i32;
    }
    let mut euid = 0u32;
    let mut egid = 0u32;
    (unsafe { getpeereid(fd, &mut euid, &mut egid) } == 0).then_some(euid)
}

pub fn default_address(pid: u32) -> String {
    std::env::temp_dir()
        .join(format!("atrium-ctl-{pid}.sock"))
        .to_string_lossy()
        .into_owned()
}

/// The syscall half of one accepted connection.
struct Stream(UnixStream);

impl Conn for Stream {
    fn recv(&mut self, buf: &mut [u8]) -> Recv {
        match self.0.read(buf) {
            Ok(0) => Recv::Closed,
            Ok(n) => Recv::Data(n),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Recv::WouldBlock,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => Recv::Retry,
            Err(_) => Recv::Closed,
        }
    }

    fn send(&mut self, buf: &[u8]) -> Sent {
        match self.0.write(buf) {
            Ok(n) => Sent::Wrote(n),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Sent::WouldBlock,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => Sent::WouldBlock,
            Err(_) => Sent::Failed,
        }
    }
}

/// `(dev, ino)` of the bound socket file: the identity a hijacker cannot
/// forge by unlinking the path and binding their own socket over it.
fn socket_ident(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    /// `(dev, ino)` recorded immediately after bind.
    ident: Option<(u64, u64)>,
    /// Last time the endpoint identity was re-checked (at most ~1/s).
    checked: Instant,
    /// A pending endpoint-integrity alarm for the caller to surface.
    alarm: Option<String>,
    /// Whether the alarm has already been printed, so a permanent hijack
    /// does not spam the terminal once a second forever.
    alarm_reported: bool,
    /// Half-open clients: accepted, still assembling their request. Bounded,
    /// each with its own deadline. A single silent connection used to hold
    /// the whole channel for CLIENT_DEADLINE because `accept` was
    /// unreachable while one request was in flight — one connection per
    /// second was enough to lock out every pane and the operator.
    slots: VecDeque<Channel<Stream>>,
    /// The connection whose request was handed to the run loop.
    cur: Option<Channel<Stream>>,
    /// Peers turned away by the credential gate since the last report, and
    /// when that report went out. Counted rather than printed on the spot:
    /// an `eprint!` per refusal, inside the accept loop, is an unbounded
    /// terminal write whose rate is chosen by whoever is connecting.
    refused: usize,
    refusal_reported: Option<Instant>,
    /// Replies still being handed to the kernel a buffer at a time. The flag
    /// marks a *refused* client, which atrium keeps draining until it hangs up
    /// so the refusal is not lost to a reset (see `Channel::drain_input`).
    outbox: VecDeque<(Channel<Stream>, Option<Instant>)>,
}

impl Listener {
    pub fn bind(addr: &str) -> io::Result<Listener> {
        let path = PathBuf::from(addr);
        // A unix-domain socket path must fit in `sockaddr_un.sun_path`, which
        // holds fewer bytes on macOS/BSD (104, incl. the NUL) than on Linux
        // (108). macOS's per-user `$TMPDIR` (`/var/folders/…`) is long, so
        // guard explicitly and fail with an actionable message rather than a
        // cryptic OS error from deep inside `UnixListener::bind`.
        const SUN_PATH_MAX: usize = if cfg!(any(target_os = "linux", target_os = "android")) {
            108
        } else {
            104
        };
        // `>=`: one byte of `sun_path` is reserved for the trailing NUL.
        if path.as_os_str().len() >= SUN_PATH_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "atrium control-socket path is {} bytes, over the {}-byte \
                     unix-domain socket limit on this platform: {}",
                    path.as_os_str().len(),
                    SUN_PATH_MAX,
                    path.display()
                ),
            ));
        }
        // A stale socket file from a crashed prior run would block the bind.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        // Owner-only (0600) so the socket file itself is not connectable by
        // other users — defense in depth beneath the per-connection uid check.
        //
        // The result used to be discarded. A defense that silently does not
        // apply is worse than none, because everything above it assumes it
        // held: if the mode cannot be set, the socket is world-connectable and
        // atrium would have carried on serving on it. Refuse to bind instead,
        // and take the socket file with us so nothing is left listening.
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                let _ = std::fs::remove_file(&path);
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing to serve on {}: could not restrict it to this user ({e})",
                        path.display()
                    ),
                ));
            }
        }
        let ident = socket_ident(&path);
        Ok(Listener {
            listener,
            path,
            ident,
            checked: Instant::now(),
            alarm: None,
            alarm_reported: false,
            slots: VecDeque::new(),
            cur: None,
            refused: 0,
            refusal_reported: None,
            outbox: VecDeque::new(),
        })
    }

    /// Re-`stat` the endpoint at most once a second and raise an alarm if the
    /// path no longer names the socket atrium bound.
    ///
    /// A same-uid process can `unlink()` the path and `bind()` its own socket
    /// there — the authority to do that comes from the 0700 TMPDIR, not from
    /// the socket's 0600 mode, so the mode is no defense. Everything after
    /// that point talks to the attacker: it receives the pane token in the
    /// request and answers with whatever it likes. Detection is the scope
    /// here; authenticating the server to its clients is a separate decision
    /// and must not be faked with a non-cryptographic keyed hash.
    fn check_endpoint(&mut self, now: Instant) {
        if now.saturating_duration_since(self.checked) < ENDPOINT_CHECK_EVERY {
            return;
        }
        self.checked = now;
        let Some(bound) = self.ident else { return };
        let msg = match socket_ident(&self.path) {
            Some(cur) if cur == bound => {
                self.alarm_reported = false;
                return;
            }
            Some(_) => format!(
                "ctl endpoint {} was replaced by another socket — \
                 a same-user process may be intercepting ctl traffic",
                self.path.display()
            ),
            None => format!(
                "ctl endpoint {} disappeared — ctl clients cannot reach atrium",
                self.path.display()
            ),
        };
        if !self.alarm_reported {
            self.alarm_reported = true;
            eprint!("[atrium] SECURITY: {msg}\r\n");
        }
        self.alarm = Some(msg);
    }

    pub fn take_security_event(&mut self) -> Option<String> {
        self.alarm.take()
    }

    /// How many replies are still draining. Tests only: the outbox depth is
    /// the thing a "did the oldest in-flight reply survive?" test has to see.
    #[cfg(test)]
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }

    /// How many half-open clients are parked. Tests only.
    #[cfg(test)]
    pub fn slots_len(&self) -> usize {
        self.slots.len()
    }

    /// Push every parked reply one buffer further; retire the finished and
    /// the hopeless.
    fn service_outbox(&mut self, now: Instant) {
        let mut buf = [0u8; 4096];
        let mut keep = VecDeque::with_capacity(self.outbox.len());
        while let Some((mut ch, linger)) = self.outbox.pop_front() {
            let mut done = false;
            if let Some(born) = linger {
                // A refused client is still pushing the tail of its oversize
                // body. Keep the receive queue empty so the close that
                // follows delivers the refusal rather than a reset.
                let (consumed, closed) = ch.drain_input(&mut buf);
                if consumed {
                    ch.touch(now);
                }
                if closed || expired(born, now, DRAIN_DEADLINE) {
                    done = true;
                }
            }
            match ch.pump_write(now) {
                // Done: dropping the stream closes it, which is what tells
                // the client the reply is complete.
                WriteOutcome::Flushed => match linger {
                    None => done = true,
                    // Quiet for the grace period: it has had its answer.
                    Some(_) => done |= expired(ch.since(), now, DRAIN_GRACE),
                },
                WriteOutcome::Failed => done = true,
                WriteOutcome::Pending => {
                    // The stall clock is anchored on the last accepted byte,
                    // so this only drops a client that stopped reading.
                    done |= ch.write_stalled(now);
                }
            }
            if !done {
                keep.push_back((ch, linger));
            }
        }
        self.outbox = keep;
    }

    /// Park a reply that the kernel would not take in one go. `false` means
    /// there was no room and `ch` was dropped — the caller must surface that,
    /// never swallow it.
    ///
    /// This used to be `outbox.pop_front()`: at the ninth concurrent parked
    /// reply the OLDEST in-flight one was discarded mid-stream, so a
    /// legitimate slow reader of a 120 KB `ctl audit` lost its answer and
    /// neither end saw an error — `respond` returned `Ok` and the client got
    /// EOF without a newline. That is the D1 failure moved one layer out, and
    /// it is the one thing this function must never do.
    ///
    /// Room is made only from connections that are already dead (no write
    /// progress for `WRITE_STALL`) or from *refusals*, which rank below real
    /// replies; refusals additionally get at most [`MAX_LINGER`] slots, so an
    /// attacker spraying oversize requests cannot crowd out real callers.
    /// When neither yields a slot the NEWEST reply is the one refused, and
    /// the caller is told.
    fn park_reply(&mut self, ch: Channel<Stream>, linger: Option<Instant>, now: Instant) -> bool {
        if linger.is_some() {
            let refusals = self.outbox.iter().filter(|(_, l)| l.is_some()).count();
            if refusals >= MAX_LINGER || self.outbox.len() >= MAX_OUTBOX {
                return false;
            }
            self.outbox.push_back((ch, linger));
            return true;
        }
        if self.outbox.len() >= MAX_OUTBOX {
            let victim = self
                .outbox
                .iter()
                .position(|(c, l)| c.write_stalled(now) || l.is_some());
            match victim {
                Some(i) => {
                    self.outbox.remove(i);
                }
                None => return false,
            }
        }
        self.outbox.push_back((ch, linger));
        true
    }

    /// Report refused peers at most once per [`REFUSAL_REPORT_EVERY`].
    fn report_refusals(&mut self, now: Instant) {
        if self.refused == 0 {
            return;
        }
        let due = match self.refusal_reported {
            None => true,
            Some(t) => expired(t, now, REFUSAL_REPORT_EVERY),
        };
        if !due {
            return;
        }
        let n = std::mem::take(&mut self.refused);
        self.refusal_reported = Some(now);
        eprint!("[atrium] ipc: refused {n} peer(s) atrium could not vouch for\r\n");
    }

    /// Accept whoever is waiting, up to a few per tick.
    ///
    /// The slot pool is bounded and evicts, in order of preference, a client
    /// that has already blown its deadline and otherwise the oldest one. That
    /// is the property that matters: a stalled client can never stop a
    /// healthy client from being *accepted*.
    fn accept_new(&mut self, now: Instant) -> io::Result<()> {
        for _ in 0..ACCEPT_PER_TICK {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Peer-cred gate, FAILING CLOSED: `None` — the case where
                    // the OS could not vouch for the peer at all — is exactly
                    // when to refuse.
                    match peer_uid(stream.as_raw_fd()) {
                        Some(peer) if peer == unsafe { geteuid() } => {}
                        _ => {
                            // Counted, not printed here: see `report_refusals`.
                            self.refused += 1;
                            continue; // stream dropped here
                        }
                    }
                    stream.set_nonblocking(true)?;
                    if self.slots.len() >= MAX_SLOTS {
                        let victim = self
                            .slots
                            .iter()
                            .position(|c| c.read_expired(now))
                            .unwrap_or(0);
                        self.slots.remove(victim);
                    }
                    self.slots.push_back(Channel::new(Stream(stream), now));
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn poll(&mut self) -> io::Result<Option<String>> {
        let now = Instant::now();
        // The contract says the caller responds before polling again. If it
        // did not — a `?` early-return, a panic caught upstream, a future
        // refactor of the large `apply_ctl` above this — let the un-answered
        // client GO rather than latching the channel shut. A latch only
        // `respond` can clear turns one missed reply into a control plane
        // that is dead for the life of the process, and this control plane
        // gates `board claim`/`release`, the fleet's mutual exclusion.
        if let Some(ch) = self.cur.take() {
            let _ = self.park_reply(ch, None, now);
        }
        self.check_endpoint(now);
        // Replies owed to earlier clients go out before new work comes in.
        self.service_outbox(now);
        self.accept_new(now)?;
        self.report_refusals(now);
        debug_assert!(self.cur.is_none(), "released above");

        // One request outstanding at a time: the loop below returns the
        // instant it sets `cur`, so a second request cannot be handed out
        // before the caller responds. The parked clients keep their slots
        // and their deadlines meanwhile.
        let mut buf = [0u8; 8192];
        let mut i = 0;
        while i < self.slots.len() {
            let outcome = self.slots[i].pump_read(&mut buf, now);
            match outcome {
                ReadOutcome::Ready(line) => {
                    let ch = self.slots.remove(i).expect("index in range");
                    self.cur = Some(ch);
                    return Ok(Some(line));
                }
                ReadOutcome::Oversize => {
                    let mut ch = self.slots.remove(i).expect("index in range");
                    // Say why. A rejected request used to get no reply at all,
                    // so an oversize `ctl` looked exactly like a crashed atrium.
                    ch.queue(OVERSIZE_REPLY, now);
                    let _ = ch.pump_write(now);
                    // Best effort: if the refusal lane is full the connection
                    // is dropped here, which is the right outcome for a
                    // client that is already over the wire limit.
                    let _ = self.park_reply(ch, Some(now), now);
                }
                ReadOutcome::Closed => {
                    self.slots.remove(i);
                }
                ReadOutcome::Pending => {
                    if self.slots[i].read_expired(now) {
                        self.slots.remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn respond(&mut self, reply: &str) -> io::Result<()> {
        let now = Instant::now();
        let Some(mut ch) = self.cur.take() else {
            return Ok(());
        };
        // The size limit is the SERVER's too. It used to be enforced only by
        // the client, so an answer past the cap reached the wire and came
        // back to the caller as `InvalidData` from the channel instead of as
        // a diagnosis from atrium.
        if reply.len() > MAX_REPLY {
            ch.queue(TOO_LARGE_REPLY, now);
            let _ = ch.pump_write(now);
            let _ = self.park_reply(ch, None, now);
            return Err(reply_too_large(reply.len()));
        }
        ch.queue(reply, now);
        match ch.pump_write(now) {
            // Everything the kernel would take is taken; dropping the stream
            // flushes and closes.
            WriteOutcome::Flushed => Ok(()),
            WriteOutcome::Pending => {
                // The remainder is parked, NOT truncated. `write_all` on a
                // non-blocking socket returned Err(WouldBlock) *after* a
                // partial write, so any reply over one send buffer (8 KiB on
                // macOS) was silently cut in half — `ctl audit` delivered
                // 8 192 of 120 631 bytes and the client printed broken JSON.
                if self.park_reply(ch, None, now) {
                    Ok(())
                } else {
                    // No slot, and none that could be taken without killing
                    // someone else's in-flight reply. Drop THIS one and say
                    // so: the client sees EOF before a newline, which its
                    // `read_reply` reports as an error rather than as a
                    // truncated success.
                    Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "ctl reply could not be queued: too many replies are still \
                         draining to slow readers",
                    ))
                }
            }
            WriteOutcome::Failed => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ctl client went away before its reply was delivered",
            )),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Only unlink the path if it is still *our* socket: if it was
        // hijacked, the file belongs to someone else and is not ours to
        // remove.
        if self.ident.is_none() || socket_ident(&self.path) == self.ident {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub fn request(addr: &str, req: &str) -> io::Result<String> {
    let mut stream = UnixStream::connect(addr)?;
    // Non-blocking under deadlines computed ONCE — deliberately NOT
    // `set_read_timeout`/`set_write_timeout`. Two independent reasons, both
    // measured:
    //
    // 1. A socket timeout re-arms on every read, so an endpoint that keeps
    //    dribbling bytes is never timed out at all. That is the hole the
    //    67 MB reply came through, and the size cap alone does not close it:
    //    a drip that stays under `MAX_REPLY` can still hold `atrium ctl` — and
    //    the pane that ran it — open indefinitely.
    //
    // 2. On macOS, `setsockopt(SO_RCVTIMEO/SO_SNDTIMEO)` on a unix-domain
    //    stream whose peer has ALREADY closed fails with EINVAL, while the
    //    bytes that peer wrote sit in the receive buffer perfectly readable.
    //    Setting the timeout with `?` right after `connect` therefore turned
    //    "the server answered and hung up" into `InvalidInput: Invalid
    //    argument` and threw away a COMPLETE reply. `fcntl(F_SETFL,
    //    O_NONBLOCK)` has no such failure mode.
    stream.set_nonblocking(true)?;
    exchange(&mut stream, req, REPLY_STALL, REPLY_DEADLINE)
}

/// One request/reply exchange on an already non-blocking stream.
///
/// The budgets are parameters so the deadline behaviour is testable in
/// seconds rather than in `REPLY_DEADLINE`s; `request` passes the shipping
/// values from `wire`.
pub(super) fn exchange(
    stream: &mut UnixStream,
    req: &str,
    stall: Duration,
    hard: Duration,
) -> io::Result<String> {
    let start = Instant::now();
    let mut line = req.to_string();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    write_request(stream, line.as_bytes(), start, stall, hard)?;
    read_reply(stream, start, stall, hard)
}

fn silent_endpoint() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "no reply from atrium control channel",
    )
}

fn unfinished_reply() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "atrium control channel never finished its reply",
    )
}

/// Push the request out without blocking, under the same two budgets as the
/// reply. A server that accepts nothing must not hang the client either.
fn write_request(
    stream: &mut UnixStream,
    bytes: &[u8],
    start: Instant,
    stall: Duration,
    hard: Duration,
) -> io::Result<()> {
    let mut off = 0usize;
    let mut progress = start;
    while off < bytes.len() {
        match stream.write(&bytes[off..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the atrium control channel accepted no bytes",
                ))
            }
            Ok(n) => {
                off += n;
                progress = Instant::now();
                continue;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(e) => return Err(e),
        }
        let now = Instant::now();
        if expired(progress, now, stall) {
            return Err(silent_endpoint());
        }
        if expired(start, now, hard) {
            return Err(unfinished_reply());
        }
        std::thread::sleep(CLIENT_POLL);
    }
    Ok(())
}

/// Read one reply line.
///
/// Waiting is fine here in a way it never is on atrium's event loop — this
/// runs in a short-lived `atrium ctl` whose only job is to wait for this
/// answer — *as long as it is bounded*, which is exactly what a hostile
/// endpoint planted at the socket path would otherwise exploit. Three rules
/// the old version got wrong:
///
/// * **Bounded in size.** Nothing capped the buffer: a hostile server
///   returned `Ok` with a 67 MB `String` in 250 ms.
/// * **Bounded in time, in total.** The socket timeout re-armed on every
///   read, so a server that kept sending was never timed out.
/// * **EOF without a newline is an error.** It used to `break` and return
///   the partial as success, handing the caller truncated JSON with no
///   indication anything was wrong.
fn read_reply(
    stream: &mut UnixStream,
    start: Instant,
    stall: Duration,
    hard: Duration,
) -> io::Result<String> {
    let mut out: Vec<u8> = Vec::with_capacity(256);
    let mut buf = [0u8; 8192];
    let mut progress = start;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "atrium control channel closed before a complete reply",
                ))
            }
            Ok(n) => {
                if let Some(pos) = buf[..n].iter().position(|b| *b == b'\n') {
                    out.extend_from_slice(&buf[..pos]);
                    return Ok(String::from_utf8_lossy(&out)
                        .trim_end_matches('\r')
                        .to_string());
                }
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_REPLY {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "atrium control channel sent an oversize reply",
                    ));
                }
                progress = Instant::now();
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                std::thread::sleep(CLIENT_POLL);
            }
            Err(e) => return Err(e),
        }
        let now = Instant::now();
        if expired(progress, now, stall) {
            return Err(silent_endpoint());
        }
        if expired(start, now, hard) {
            return Err(unfinished_reply());
        }
    }
}
