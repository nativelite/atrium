//! The `ctl` control channel: a tiny, zero-dependency, **local** request/reply
//! transport — a Windows named pipe or a unix-domain socket, chosen at compile
//! time. It carries one JSON request line and one JSON reply line per client
//! connection; amux ("the server") owns the endpoint and drains it from its run
//! loop without blocking, and each `amux ctl <cmd>` invocation is a short-lived
//! client ([`request`]).
//!
//! The API is deliberately small:
//! * [`default_address`] — the per-process endpoint amux binds and injects as
//!   `AMUX_CTL` into every pane it spawns.
//! * [`Listener::bind`] / [`Listener::poll`] / [`Listener::respond`] — the
//!   server side. `poll` is **non-blocking**: it returns `Ok(None)` when no
//!   request is ready, so it fits the 15 ms run-loop tick with no thread (the
//!   C0 spike proved this on Windows via `PIPE_NOWAIT`; unix uses a
//!   non-blocking `accept`). One request is outstanding at a time — the caller
//!   must [`respond`](Listener::respond) before the next `poll`.
//! * [`request`] — the client: connect, send one line, read one reply line.
//!
//! Framing is one `\n`-terminated line each way. This is a control plane for a
//! handful of agents, not a high-throughput bus; serving one request per tick is
//! plenty and keeps the state machine trivial.
//!
//! **Same-user endpoint (peer-cred).** The endpoint is restricted to the user who
//! launched amux: on unix the socket is 0600 and every accepted connection's peer
//! uid is checked against `geteuid` (`SO_PEERCRED` / `getpeereid`) — a mismatched
//! client is dropped before its request is read; on Windows the pipe is created
//! with a security descriptor granting access only to the current user and
//! SYSTEM. This is defense in depth *beneath* the per-pane capability token
//! (`AMUX_TOKEN`), which remains the primary authenticator; together they close
//! the cross-user connect vector on a shared machine.

use std::io;

/// The endpoint amux binds for this process, unique per pid so two amux
/// instances never collide. Injected as `AMUX_CTL` into spawned panes.
pub fn default_address() -> String {
    sys::default_address(std::process::id())
}

/// The server endpoint. Owns the OS handle/socket; one request is outstanding at
/// a time (poll → respond → poll …).
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
    /// `Ok(None)`. After a `Some(_)` the caller MUST call [`respond`](Listener::respond)
    /// before polling again.
    pub fn poll(&mut self) -> io::Result<Option<String>> {
        self.sys.poll()
    }

    /// Send the reply line for the request the last [`poll`](Listener::poll)
    /// returned, and release the connection so the endpoint can accept the next
    /// client. A trailing newline is added if absent.
    pub fn respond(&mut self, reply: &str) -> io::Result<()> {
        self.sys.respond(reply)
    }
}

/// Client: connect to `addr`, send `req` (one line), return the reply line
/// (newline trimmed). Used by `amux ctl <cmd>`.
pub fn request(addr: &str, req: &str) -> io::Result<String> {
    sys::request(addr, req)
}

// ---------------------------------------------------------------------------
// Windows: a single named-pipe instance, reused per client. Connection detection
// and reads are non-blocking (`PIPE_NOWAIT`); the reply write flushes before the
// disconnect so the client always sees it. Mirrors the verified C0 spike.
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::thread;
    use std::time::Duration;

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
        fn FlushFileBuffers(handle: Handle) -> i32;
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
    const PIPE_TYPE_MESSAGE: u32 = 0x0000_0004;
    const PIPE_READMODE_MESSAGE: u32 = 0x0000_0002;
    const PIPE_NOWAIT: u32 = 0x0000_0001;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;
    const ERROR_NO_DATA: i32 = 232;
    const ERROR_PIPE_NOT_CONNECTED: i32 = 233;
    const ERROR_BROKEN_PIPE: i32 = 109;
    const ERROR_PIPE_CONNECTED: i32 = 535;
    const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
    }

    fn last() -> i32 {
        io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    pub fn default_address(pid: u32) -> String {
        format!(r"\\.\pipe\amux-ctl-{pid}")
    }

    pub struct Listener {
        handle: Handle,
        connected: bool,
    }

    // The handle is owned solely by this Listener, used only from the run loop.
    unsafe impl Send for Listener {}

    impl Listener {
        fn create_instance(addr: &[u16]) -> io::Result<Handle> {
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
            let h = unsafe {
                CreateNamedPipeW(
                    addr.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_NOWAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    8192,
                    8192,
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
            let handle = Self::create_instance(&addr)?;
            Ok(Listener {
                handle,
                connected: false,
            })
        }

        pub fn poll(&mut self) -> io::Result<Option<String>> {
            if !self.connected {
                // NOWAIT: returns immediately. A waiting client shows up as
                // ERROR_PIPE_CONNECTED; ERROR_PIPE_LISTENING / no-data means
                // "still nobody", which is the common idle path.
                let r = unsafe { ConnectNamedPipe(self.handle, std::ptr::null_mut()) };
                if r != 0 {
                    self.connected = true;
                } else {
                    let e = last();
                    if e == ERROR_PIPE_CONNECTED {
                        self.connected = true;
                    } else {
                        return Ok(None);
                    }
                }
            }
            // Connected: try one non-blocking read.
            let mut buf = [0u8; 8192];
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
            if ok != 0 && n > 0 {
                let line = String::from_utf8_lossy(&buf[..n as usize])
                    .trim_end_matches(['\r', '\n'])
                    .to_string();
                Ok(Some(line))
            } else {
                let e = last();
                if e == ERROR_BROKEN_PIPE || e == ERROR_PIPE_NOT_CONNECTED {
                    // Client vanished before sending; recycle the instance.
                    self.recycle();
                }
                Ok(None) // ERROR_NO_DATA: connected but nothing yet — try next tick.
            }
        }

        pub fn respond(&mut self, reply: &str) -> io::Result<()> {
            let mut line = reply.to_string();
            if !line.ends_with('\n') {
                line.push('\n');
            }
            let bytes = line.as_bytes();
            let mut written = 0u32;
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            unsafe {
                FlushFileBuffers(self.handle);
            }
            self.recycle();
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Drop the current client and re-arm the same instance to listen again.
        fn recycle(&mut self) {
            unsafe {
                DisconnectNamedPipe(self.handle);
            }
            self.connected = false;
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            unsafe {
                DisconnectNamedPipe(self.handle);
                CloseHandle(self.handle);
            }
        }
    }

    pub fn request(addr: &str, req: &str) -> io::Result<String> {
        let name = wide(addr);
        // The server serves one client at a time; a brief busy/absent window
        // between clients is normal — retry a bounded number of times.
        let mut h = INVALID_HANDLE_VALUE;
        for _ in 0..100 {
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
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            return Err(io::Error::from_raw_os_error(e));
        }
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "amux control channel did not answer",
            ));
        }

        let mut mode = PIPE_READMODE_MESSAGE;
        unsafe {
            SetNamedPipeHandleState(h, &mut mode, std::ptr::null_mut(), std::ptr::null_mut());
        }

        let mut line = req.to_string();
        if !line.ends_with('\n') {
            line.push('\n');
        }
        let bytes = line.as_bytes();
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                h,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = last();
            unsafe { CloseHandle(h) };
            return Err(io::Error::from_raw_os_error(e));
        }

        // Read the reply, tolerating the small window before the server writes.
        let mut buf = [0u8; 8192];
        let mut out = String::new();
        for _ in 0..600 {
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
            if ok != 0 && n > 0 {
                out = String::from_utf8_lossy(&buf[..n as usize])
                    .trim_end_matches(['\r', '\n'])
                    .to_string();
                break;
            }
            let e = last();
            if e == ERROR_NO_DATA {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            unsafe { CloseHandle(h) };
            return Err(io::Error::from_raw_os_error(e));
        }
        unsafe { CloseHandle(h) };
        if out.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no reply from amux control channel",
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Unix: a non-blocking `UnixListener`; each client is one short-lived stream.
// ---------------------------------------------------------------------------
#[cfg(unix)]
mod sys {
    use std::io::{self, Read, Write};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::time::Duration;

    extern "C" {
        fn geteuid() -> u32;
    }

    /// The effective uid of the connected peer, if the OS can report it. `None`
    /// means the credential couldn't be read (caller then does not reject on it).
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
        let mut cred = Ucred { pid: 0, uid: 0, gid: 0 };
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
            .join(format!("amux-ctl-{pid}.sock"))
            .to_string_lossy()
            .into_owned()
    }

    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
        cur: Option<UnixStream>,
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
                        "amux control-socket path is {} bytes, over the {}-byte \
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
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            Ok(Listener {
                listener,
                path,
                cur: None,
            })
        }

        pub fn poll(&mut self) -> io::Result<Option<String>> {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    // Peer-cred gate: reject a client owned by a different user
                    // outright — do not even read its request. The pane token is
                    // still the primary auth; this closes the cross-user connect
                    // vector on a shared machine. A credential we can't read (None)
                    // does not reject (the token remains the gate).
                    if let Some(peer) = peer_uid(stream.as_raw_fd()) {
                        if peer != unsafe { geteuid() } {
                            // Drop the stream (disconnect) and report idle.
                            return Ok(None);
                        }
                    }
                    // The client sends immediately then waits; a short blocking
                    // read of one line is safe and keeps framing simple.
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                    let line = read_line(&mut stream)?;
                    self.cur = Some(stream);
                    Ok(Some(line))
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e),
            }
        }

        pub fn respond(&mut self, reply: &str) -> io::Result<()> {
            if let Some(mut stream) = self.cur.take() {
                let mut line = reply.to_string();
                if !line.ends_with('\n') {
                    line.push('\n');
                }
                stream.write_all(line.as_bytes())?;
                stream.flush()?;
            }
            Ok(())
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn read_line(stream: &mut UnixStream) -> io::Result<String> {
        let mut buf = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    if byte[0] == b'\n' {
                        break;
                    }
                    buf.push(byte[0]);
                }
                Err(e) => return Err(e),
            }
            if buf.len() > 65536 {
                break; // never grow unbounded on a malformed client
            }
        }
        Ok(String::from_utf8_lossy(&buf)
            .trim_end_matches('\r')
            .to_string())
    }

    pub fn request(addr: &str, req: &str) -> io::Result<String> {
        let mut stream = UnixStream::connect(addr)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut line = req.to_string();
        if !line.ends_with('\n') {
            line.push('\n');
        }
        stream.write_all(line.as_bytes())?;
        stream.flush()?;
        read_line(&mut stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// A unique endpoint per test (tests run in parallel; a shared per-pid name
    /// would collide). Platform-appropriate: a named pipe on Windows, a temp
    /// socket path on unix.
    fn test_addr(nonce: u32) -> String {
        let pid = std::process::id();
        #[cfg(windows)]
        {
            format!(r"\\.\pipe\amux-ctl-test-{pid}-{nonce}")
        }
        #[cfg(unix)]
        {
            std::env::temp_dir()
                .join(format!("amux-ctl-test-{pid}-{nonce}.sock"))
                .to_string_lossy()
                .into_owned()
        }
    }

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

        let reply = client.join().expect("client thread");
        assert!(reply.contains("\"ok\":true"), "client saw: {reply}");
    }

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

    #[test]
    fn poll_is_non_blocking_when_idle() {
        let addr = test_addr(2);
        let mut server = Listener::bind(&addr).expect("bind");
        // With no client, poll must return immediately with None, not hang.
        for _ in 0..10 {
            assert_eq!(server.poll().expect("poll"), None);
        }
    }
}
