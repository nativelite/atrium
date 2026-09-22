// ---------------------------------------------------------------------------
// Windows: a small pool of named-pipe instances, each an independent state
// machine over the shared `chan` core.
//
// Three things changed here, all of them parity with the unix side:
//   * `FlushFileBuffers` is gone. On the server end of a named pipe it does not
//     return until the client has read everything — an unbounded block on the
//     single run loop, triggerable by any client that simply stops reading (a
//     debugger-suspended `atrium ctl` does it by accident). The invariant it was
//     protecting (`DisconnectNamedPipe` discards unread data) is now held by
//     state: after the last byte is written the instance sits in `Draining`
//     until a non-blocking `ReadFile` reports ERROR_BROKEN_PIPE (the client
//     closed, so it read everything) or DRAIN_DEADLINE expires.
//   * The pipe is BYTE mode, not MESSAGE mode. The protocol frames on `\n`, so
//     message mode bought nothing and was the sole source of ERROR_MORE_DATA —
//     the code path that used to discard the head of any request over 8 KiB.
//     The ERROR_MORE_DATA mapping is kept anyway, and now keeps the bytes.
//   * One instance is no longer the whole channel: a client that connects and
//     says nothing occupies one slot with its own deadline, not the endpoint.
// ---------------------------------------------------------------------------
use super::chan::{Channel, Conn, ReadOutcome, Recv, Sent, WriteOutcome};
use super::winmap::{self, Accepted};
use super::wire::{
    expired, reply_too_large, CLIENT_POLL, DRAIN_DEADLINE, MAX_REPLY, MAX_SLOTS, OVERSIZE_REPLY,
    REPLY_DEADLINE, REPLY_STALL, TOO_LARGE_REPLY,
};
use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::thread;
use std::time::{Duration, Instant};

type Handle = *mut c_void;

#[link(name = "kernel32")]
extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_buf: u32,
        in_buf: u32,
        default_timeout: u32,
        sec: *mut c_void,
    ) -> Handle;
    fn ConnectNamedPipe(handle: Handle, overlapped: *mut c_void) -> i32;
    fn DisconnectNamedPipe(handle: Handle) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sec: *mut c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn ReadFile(
        handle: Handle,
        buf: *mut u8,
        len: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        handle: Handle,
        buf: *const u8,
        len: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn SetNamedPipeHandleState(
        handle: Handle,
        mode: *mut u32,
        max_collect: *mut u32,
        collect_timeout: *mut u32,
    ) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

// Security-descriptor plumbing so the control pipe is openable only by the
// user who created it (+ SYSTEM) — the Windows analog of unix peer-cred.
#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: i32,
        info: *mut c_void,
        len: u32,
        ret_len: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, out: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl: *const u16,
        revision: u32,
        psd: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_USER_CLASS: i32 = 1; // TOKEN_INFORMATION_CLASS::TokenUser
const SDDL_REVISION_1: u32 = 1;

/// SECURITY_ATTRIBUTES: nLength, lpSecurityDescriptor, bInheritHandle.
#[repr(C)]
struct SecurityAttributes {
    len: u32,
    sd: *mut c_void,
    inherit: i32,
}

/// SID_AND_ATTRIBUTES: the first (and only relevant) member of TOKEN_USER; its
/// `sid` points into the same buffer GetTokenInformation filled.
#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

/// Read a NUL-terminated wide (UTF-16) string the OS allocated.
unsafe fn read_wide(mut p: *const u16) -> String {
    let mut units = Vec::new();
    while *p != 0 {
        units.push(*p);
        p = p.add(1);
    }
    String::from_utf16_lossy(&units)
}

/// Build the SDDL for a DACL that grants full access only to the current user
/// and LocalSystem, e.g. `D:P(A;;GA;;;S-1-5-21-…)(A;;GA;;;SY)`. `None` if the
/// user SID can't be resolved (caller then falls back to the default DACL).
fn current_user_sddl() -> Option<Vec<u16>> {
    // GetCurrentProcess returns a pseudo-handle; no CloseHandle needed for it.
    let proc = unsafe { GetCurrentProcess() };
    let mut token: Handle = std::ptr::null_mut();
    if unsafe { OpenProcessToken(proc, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    // First call sizes the buffer; second fills it.
    let mut need = 0u32;
    unsafe { GetTokenInformation(token, TOKEN_USER_CLASS, std::ptr::null_mut(), 0, &mut need) };
    if need == 0 {
        unsafe { CloseHandle(token) };
        return None;
    }
    let mut buf = vec![0u8; need as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buf.as_mut_ptr() as *mut c_void,
            need,
            &mut need,
        )
    };
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return None;
    }
    // The buffer begins with a SID_AND_ATTRIBUTES whose `sid` points inside it.
    let sa = unsafe { &*(buf.as_ptr() as *const SidAndAttributes) };
    let mut sid_str: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sa.sid, &mut sid_str) } == 0 || sid_str.is_null() {
        return None;
    }
    let sid = unsafe { read_wide(sid_str) };
    unsafe { LocalFree(sid_str as *mut c_void) };
    // P = protected (no inheritance); GA = generic all; SY = LocalSystem.
    let sddl = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)");
    Some(wide(&sddl))
}

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
/// Only the *first* instance may claim the name: a second process that tries
/// to add an instance to `\\.\pipe\atrium-ctl-<pid>` fails instead of quietly
/// serving atrium's clients. This is the Windows answer to the unix endpoint
/// hijack — prevention rather than the after-the-fact detection a filesystem
/// path can offer.
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
/// BYTE mode both ways: the wire is `\n`-framed, and message mode's only
/// contribution was ERROR_MORE_DATA.
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_NOWAIT: u32 = 0x0000_0001;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
/// The per-instance pipe buffer, both directions (passed to `CreateNamedPipeW`).
/// **Load-bearing for every write.** `PIPE_NOWAIT` writes are *all-or-nothing*:
/// a `WriteFile` whose length exceeds the free buffer space writes ZERO bytes,
/// not a partial. So every write must be capped to this so it can fit once the
/// reader has drained — otherwise any request or reply larger than one buffer
/// writes nothing forever and the exchange deadlocks (measured on Windows: the
/// failure was a hard wall at exactly 8192 bytes). Readers drain the whole
/// buffer each pass, so a capped chunk always fits after a drain.
const PIPE_BUF: u32 = 8192;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_NO_DATA: i32 = 232;
const ERROR_BROKEN_PIPE: i32 = 109;
const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;

/// How many pipe instances serve the endpoint at once. One instance meant one
/// silent client could hold the entire control plane; this is the Windows
/// half of the bounded slot pool.
const INSTANCES: usize = MAX_SLOTS;

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
}

fn last() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

pub fn default_address(pid: u32) -> String {
    format!(r"\\.\pipe\atrium-ctl-{pid}")
}

/// The syscall half of one pipe instance: calls the OS, classifies the
/// result with [`winmap`], decides nothing.
struct PipeConn {
    handle: Handle,
}

impl Conn for PipeConn {
    fn recv(&mut self, buf: &mut [u8]) -> Recv {
        let mut n = 0u32;
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        let err = if ok == 0 { last() } else { 0 };
        winmap::read_outcome(ok, n as usize, err)
    }

    fn send(&mut self, buf: &[u8]) -> Sent {
        // Cap to one pipe buffer: PIPE_NOWAIT writes are all-or-nothing, so a
        // larger request would write zero into a buffer that cannot hold it and
        // the reply would never leave (see PIPE_BUF). pump_write loops, so the
        // rest goes out on the following passes as the client drains.
        let take = (buf.len() as u32).min(PIPE_BUF);
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                take,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        let err = if ok == 0 { last() } else { 0 };
        winmap::write_outcome(ok, written as usize, err)
    }

    fn peer_gone(&mut self) -> bool {
        let mut scratch = [0u8; 64];
        let mut n = 0u32;
        let ok = unsafe {
            ReadFile(
                self.handle,
                scratch.as_mut_ptr(),
                scratch.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        let err = if ok == 0 { last() } else { 0 };
        winmap::drain_outcome(ok, n as usize, err)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Not connected; `ConnectNamedPipe` is retried each tick.
    Idle,
    /// A client is attached and its request is being assembled.
    Reading,
    /// Its request was handed to the run loop; awaiting `respond`.
    Serving,
    /// A reply is queued and partially written.
    Writing,
    /// Every byte is in the pipe; waiting for the client to close so the
    /// disconnect cannot discard it.
    Draining,
}

struct Instance {
    handle: Handle,
    phase: Phase,
    chan: Channel<PipeConn>,
}

pub struct Listener {
    instances: Vec<Instance>,
    /// Index of the instance whose request the caller still owes a reply.
    serving: Option<usize>,
}

// The handles are owned solely by this Listener, used only from the run loop.
unsafe impl Send for Listener {}

impl Listener {
    fn create_instance(addr: &[u16], first: bool) -> io::Result<Handle> {
        // Restrict the endpoint to the current user (+SYSTEM): another user's
        // process cannot open the control pipe even on a shared machine. This
        // is the Windows analog of the unix peer-uid check. If the descriptor
        // can't be built we fall back to the default DACL — never fail the bind
        // over this defense-in-depth layer (the pane token is the primary gate).
        let mut psd: *mut c_void = std::ptr::null_mut();
        let mut sec_ptr: *mut c_void = std::ptr::null_mut();
        let mut sa = SecurityAttributes {
            len: std::mem::size_of::<SecurityAttributes>() as u32,
            sd: std::ptr::null_mut(),
            inherit: 0,
        };
        if let Some(sddl) = current_user_sddl() {
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut psd,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 && !psd.is_null() {
                sa.sd = psd;
                sec_ptr = &mut sa as *mut SecurityAttributes as *mut c_void;
            }
        }
        let mut open_mode = PIPE_ACCESS_DUPLEX;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let h = unsafe {
            CreateNamedPipeW(
                addr.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUF,
                PIPE_BUF,
                0,
                sec_ptr,
            )
        };
        // The kernel copies the descriptor into the object at create time, so
        // the local SD can be freed immediately after the call.
        if !psd.is_null() {
            unsafe { LocalFree(psd) };
        }
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(h)
    }

    pub fn bind(addr: &str) -> io::Result<Listener> {
        let addr = wide(addr);
        let now = Instant::now();
        // The first instance must succeed (it also claims the name); the rest
        // are capacity, so a failure there costs throughput, not the bind.
        let mut instances = Vec::with_capacity(INSTANCES);
        for i in 0..INSTANCES {
            match Self::create_instance(&addr, i == 0) {
                Ok(handle) => instances.push(Instance {
                    handle,
                    phase: Phase::Idle,
                    chan: Channel::new(PipeConn { handle }, now),
                }),
                Err(e) if i == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(Listener {
            instances,
            serving: None,
        })
    }

    /// Drop the current client and re-arm this instance to listen again.
    fn recycle(&mut self, i: usize, now: Instant) {
        unsafe {
            DisconnectNamedPipe(self.instances[i].handle);
        }
        self.instances[i].phase = Phase::Idle;
        self.instances[i].chan.reset(now);
    }

    /// Move every instance that owes bytes (or is waiting to be released) one
    /// step forward. Never waits on a client.
    fn service_writes(&mut self, now: Instant) {
        for i in 0..self.instances.len() {
            match self.instances[i].phase {
                Phase::Writing => match self.instances[i].chan.pump_write(now) {
                    WriteOutcome::Flushed => self.instances[i].phase = Phase::Draining,
                    WriteOutcome::Failed => self.recycle(i, now),
                    WriteOutcome::Pending => {
                        // Two ways out, and the cheap one first.
                        //
                        // `write_outcome` maps ERROR_NO_DATA (232) at
                        // WriteFile to WouldBlock rather than to Failed,
                        // deliberately: 232 is documented as "the pipe is
                        // being closed", but a NOWAIT pipe also reports a
                        // full buffer through the same code path, and
                        // guessing "closed" there would DROP a reply the
                        // client is still owed. Guessing "full" instead only
                        // costs time — so the ambiguity is resolved by
                        // asking, not by choosing: `peer_gone` is a NOWAIT
                        // ReadFile whose ERROR_BROKEN_PIPE is unambiguous.
                        //
                        // Without it, eight clients that connect, ask and
                        // die would each hold a pipe instance for the full
                        // WRITE_STALL, which is the whole pool for 5 s.
                        if self.instances[i].chan.peer_gone()
                            || self.instances[i].chan.write_stalled(now)
                        {
                            self.recycle(i, now);
                        }
                    }
                },
                Phase::Draining => {
                    let gone = self.instances[i].chan.peer_gone();
                    if gone || expired(self.instances[i].chan.since(), now, DRAIN_DEADLINE) {
                        self.recycle(i, now);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn poll(&mut self) -> io::Result<Option<String>> {
        let now = Instant::now();
        // The contract says the caller responds before polling again. If it
        // did not, release the instance instead of latching the endpoint
        // shut: a latch only `respond` can clear turns one missed reply into
        // a control plane that is dead for the life of the process. The
        // client sees the pipe close, which its `read_reply` reports as an
        // error rather than as a truncated success.
        if let Some(i) = self.serving.take() {
            self.recycle(i, now);
        }
        self.service_writes(now);
        debug_assert!(self.serving.is_none(), "released above");

        // One request outstanding at a time: the instance that carries it
        // leaves `Phase::Reading`, so the loop below cannot hand out a
        // second. The other instances keep their clients and deadlines.

        // Attach any waiting client to a free instance.
        for i in 0..self.instances.len() {
            if self.instances[i].phase != Phase::Idle {
                continue;
            }
            let r = unsafe { ConnectNamedPipe(self.instances[i].handle, std::ptr::null_mut()) };
            let err = if r == 0 { last() } else { 0 };
            match winmap::connect_outcome(r, err) {
                Accepted::Connected => {
                    self.instances[i].chan.reset(now);
                    self.instances[i].phase = Phase::Reading;
                }
                Accepted::Recycle => self.recycle(i, now),
                Accepted::Listening => {}
            }
        }

        // Read from every attached client; hand back the first complete line.
        let mut buf = [0u8; 8192];
        for i in 0..self.instances.len() {
            if self.instances[i].phase != Phase::Reading {
                continue;
            }
            match self.instances[i].chan.pump_read(&mut buf, now) {
                ReadOutcome::Ready(line) => {
                    self.instances[i].phase = Phase::Serving;
                    self.serving = Some(i);
                    return Ok(Some(line));
                }
                ReadOutcome::Oversize => {
                    // Answer, then close — a fragment must never reach the
                    // run loop, and a refusal must never be silent.
                    self.instances[i].chan.queue(OVERSIZE_REPLY, now);
                    self.instances[i].phase = Phase::Writing;
                }
                ReadOutcome::Closed => self.recycle(i, now),
                ReadOutcome::Pending => {
                    if self.instances[i].chan.read_expired(now) {
                        self.recycle(i, now);
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn respond(&mut self, reply: &str) -> io::Result<()> {
        let now = Instant::now();
        let Some(i) = self.serving.take() else {
            return Ok(());
        };
        // The size limit is the SERVER's too — see the unix `respond`.
        if reply.len() > MAX_REPLY {
            self.instances[i].chan.queue(TOO_LARGE_REPLY, now);
            self.instances[i].phase = Phase::Writing;
            let _ = self.instances[i].chan.pump_write(now);
            return Err(reply_too_large(reply.len()));
        }
        self.instances[i].chan.queue(reply, now);
        match self.instances[i].chan.pump_write(now) {
            WriteOutcome::Flushed => {
                self.instances[i].phase = Phase::Draining;
                Ok(())
            }
            WriteOutcome::Pending => {
                // Parked: later polls flush the rest. The old code called
                // FlushFileBuffers here and blocked the whole multiplexer.
                self.instances[i].phase = Phase::Writing;
                Ok(())
            }
            WriteOutcome::Failed => {
                let e = io::Error::last_os_error();
                self.recycle(i, now);
                Err(e)
            }
        }
    }

    /// Always `None` on Windows — by design, because the threat it would
    /// report is a **same-user non-goal**, not an unguarded hole.
    ///
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` is real and worth having: it makes
    /// atrium's own `bind` fail loudly if the name already exists, so atrium can
    /// never silently attach as a second instance of an attacker's pipe,
    /// inheriting the attacker's DACL and pipe type. What it does **not** do
    /// is stop the reverse: the flag only fails *the caller's* create, so a
    /// same-user process can create additional instances of
    /// `\\.\pipe\atrium-ctl-<pid>` *without* the flag once atrium holds the
    /// name, and race atrium for connecting clients.
    ///
    /// A named pipe has no `(dev, ino)` to re-`stat`, so the unix path's
    /// tripwire has no analogue here. But this is the **same-user** case, which
    /// is outside atrium's threat model (a process running as you has already won
    /// by easier paths — env, files, `~/.claude.json`, the OS vault). So this
    /// is a documented non-goal, not an open defect: `None` is the honest,
    /// intended answer. The unix side merely *happens* to have a cheap tripwire
    /// (path re-stat) and uses it; the absence of one here changes nothing about
    /// what atrium is defending. Truly closing it needs server-authenticates-to-
    /// client crypto — over-engineering for a single-user local tool.
    pub fn take_security_event(&mut self) -> Option<String> {
        None
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        for inst in &self.instances {
            unsafe {
                DisconnectNamedPipe(inst.handle);
                CloseHandle(inst.handle);
            }
        }
    }
}

pub fn request(addr: &str, req: &str) -> io::Result<String> {
    let name = wide(addr);
    // One budget for the whole exchange, from `wire`, the same constant the
    // unix client uses — not a literal that happens to be five.
    let start = Instant::now();
    let deadline = start + REPLY_DEADLINE;
    let mut h = INVALID_HANDLE_VALUE;
    while Instant::now() < deadline {
        h = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            break;
        }
        let e = last();
        if e == ERROR_PIPE_BUSY || e == ERROR_FILE_NOT_FOUND {
            thread::sleep(CLIENT_POLL);
            continue;
        }
        return Err(io::Error::from_raw_os_error(e));
    }
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "atrium control channel did not answer",
        ));
    }

    // Byte mode, matching the server: the framing is the newline. NOWAIT as
    // well, so `read_reply`'s deadlines are real — the handle `CreateFileW`
    // returns is a *blocking* one by default, which is why the old
    // ERROR_NO_DATA retry loop could never fire and a server that answered
    // nothing hung `atrium ctl` for as long as it liked.
    let mut mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
    unsafe {
        SetNamedPipeHandleState(h, &mut mode, std::ptr::null_mut(), std::ptr::null_mut());
    }

    let mut line = req.to_string();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    let mut off = 0usize;
    while off < line.len() {
        let mut written = 0u32;
        // Cap to one pipe buffer: a PIPE_NOWAIT write larger than the free
        // buffer space writes ZERO (all-or-nothing), so a request over one
        // buffer would never send. Chunking to PIPE_BUF lets each write fit
        // once the server has drained the previous chunk (see PIPE_BUF).
        let end = (off + PIPE_BUF as usize).min(line.len());
        let chunk = &line.as_bytes()[off..end];
        let ok = unsafe {
            WriteFile(
                h,
                chunk.as_ptr(),
                chunk.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        let e = if ok == 0 { last() } else { 0 };
        if ok == 0 && e != ERROR_NO_DATA {
            unsafe { CloseHandle(h) };
            return Err(io::Error::from_raw_os_error(e));
        }
        if written == 0 {
            if Instant::now() >= deadline {
                unsafe { CloseHandle(h) };
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "atrium control channel would not accept the request",
                ));
            }
            thread::sleep(CLIENT_POLL);
            continue;
        }
        off += written as usize;
    }

    let out = read_reply(h, start, REPLY_STALL, REPLY_DEADLINE);
    unsafe { CloseHandle(h) };
    out
}

/// Read one reply line: bounded by [`MAX_REPLY`], deadlined on *stall* (so a
/// large honest reply is not cut off) with an absolute ceiling, and an error
/// — never a truncated success — if the pipe ends before the newline.
fn read_reply(h: Handle, start: Instant, stall: Duration, hard: Duration) -> io::Result<String> {
    let mut last_progress = start;
    let mut out: Vec<u8> = Vec::with_capacity(256);
    let mut buf = [0u8; 8192];
    loop {
        let mut n = 0u32;
        let ok = unsafe {
            ReadFile(
                h,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        let e = if ok == 0 { last() } else { 0 };
        let n = n as usize;
        if n > 0 {
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
            last_progress = Instant::now();
            continue;
        }
        if ok == 0 && (e == ERROR_BROKEN_PIPE || e == super::winmap::ERROR_PIPE_NOT_CONNECTED) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "atrium control channel closed before a complete reply",
            ));
        }
        if ok == 0 && e != ERROR_NO_DATA && e != super::winmap::ERROR_MORE_DATA {
            return Err(io::Error::from_raw_os_error(e));
        }
        let now = Instant::now();
        if expired(last_progress, now, stall) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no reply from atrium control channel",
            ));
        }
        if expired(start, now, hard) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "atrium control channel never finished its reply",
            ));
        }
        thread::sleep(CLIENT_POLL);
    }
}
