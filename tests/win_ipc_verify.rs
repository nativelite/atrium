//! Windows IPC verification — the measurements only a Windows box can make.
//!
//! `src/ipc.rs`'s Windows named-pipe channel was written on a Mac against MSDN and
//! never executed. This file replaces the guesses with observations:
//!
//! - **Q1** the raw `ReadFile` `(ok, n, err)` triples a `PIPE_NOWAIT` *server*
//!   handle returns in three client states, vs what `winmap::read_outcome` assumes.
//! - **Q2** byte-mode delivery of a request larger than the 8 KiB pipe buffer
//!   (no `ERROR_MORE_DATA`, no lost head — that regression is D4).
//! - **Q3** that removing `FlushFileBuffers` kept the reply intact (a) to a slow
//!   reader and (b) without letting a non-reading client block the run loop (D2).
//!
//! Q2/Q3 also double as the Step-3 defect demos: run against `main` they hang or
//! truncate; here they pass. Observed values are printed — run with
//! `cargo test --test win_ipc_verify -- --nocapture`.
#![cfg(windows)]

use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Q1 — raw ReadFile triples on a PIPE_NOWAIT server handle (standalone FFI).
// ---------------------------------------------------------------------------
mod q1 {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::time::Duration;

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    const PIPE_NOWAIT: u32 = 0x0000_0001;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buf: u32,
            in_buf: u32,
            timeout: u32,
            sec: *mut c_void,
        ) -> Handle;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            sec: *mut c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn ConnectNamedPipe(h: Handle, overlapped: *mut c_void) -> i32;
        fn DisconnectNamedPipe(h: Handle) -> i32;
        fn ReadFile(h: Handle, buf: *mut u8, len: u32, read: *mut u32, ov: *mut c_void) -> i32;
        fn WriteFile(h: Handle, buf: *const u8, len: u32, wrote: *mut u32, ov: *mut c_void) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
    }

    /// One `ReadFile` on the server: `(ok, n, err)`.
    fn read_triple(server: Handle) -> (i32, u32, u32) {
        let mut buf = [0u8; 8192];
        let mut n = 0u32;
        let ok = unsafe {
            ReadFile(
                server,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        let err = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        (ok, n, err)
    }

    /// How `winmap::read_outcome` (ipc.rs) classifies a triple — mirrored here
    /// because that module is private. Kept in lockstep with the source.
    fn winmap_says(ok: i32, n: u32, err: u32) -> &'static str {
        const ERROR_MORE_DATA: u32 = 234;
        const ERROR_NO_DATA: u32 = 232;
        const ERROR_PIPE_LISTENING: u32 = 536;
        if ok != 0 {
            return if n > 0 { "Data" } else { "WouldBlock" };
        }
        match err {
            ERROR_MORE_DATA if n > 0 => "Data",
            ERROR_MORE_DATA => "WouldBlock",
            ERROR_NO_DATA | ERROR_PIPE_LISTENING => "WouldBlock",
            _ => "Closed",
        }
    }

    #[test]
    fn q1_readfile_triples_on_a_nowait_server_handle() {
        let addr = format!(r"\\.\pipe\atrium-q1-{}", std::process::id());
        let waddr = wide(&addr);
        let server = unsafe {
            CreateNamedPipeW(
                waddr.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
                PIPE_UNLIMITED_INSTANCES,
                8192,
                8192,
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(
            server != INVALID_HANDLE_VALUE,
            "CreateNamedPipeW failed: {}",
            unsafe { GetLastError() }
        );

        let client = unsafe {
            CreateFileW(
                waddr.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(
            client != INVALID_HANDLE_VALUE,
            "client CreateFileW failed: {}",
            unsafe { GetLastError() }
        );
        // Server-side accept (NOWAIT): with a client already on the line this
        // returns 0 + ERROR_PIPE_CONNECTED (535).
        let cok = unsafe { ConnectNamedPipe(server, std::ptr::null_mut()) };
        let cerr = if cok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        println!("[Q1] ConnectNamedPipe: ok={cok} err={cerr} (535=ERROR_PIPE_CONNECTED)");

        // State 1: client connected, sent nothing.
        let s1 = read_triple(server);
        println!(
            "[Q1] state1 (nothing sent):   ok={} n={} err={}  -> winmap={}",
            s1.0,
            s1.1,
            s1.2,
            winmap_says(s1.0, s1.1, s1.2)
        );

        // State 2: client sent a partial line (no '\n').
        let msg = b"partial-no-newline";
        let mut wrote = 0u32;
        unsafe {
            WriteFile(
                client,
                msg.as_ptr(),
                msg.len() as u32,
                &mut wrote,
                std::ptr::null_mut(),
            )
        };
        std::thread::sleep(Duration::from_millis(50));
        let s2 = read_triple(server);
        println!(
            "[Q1] state2 (partial, no \\n): ok={} n={} err={}  -> winmap={}",
            s2.0,
            s2.1,
            s2.2,
            winmap_says(s2.0, s2.1, s2.2)
        );

        // State 3: client has closed its handle.
        unsafe { CloseHandle(client) };
        std::thread::sleep(Duration::from_millis(50));
        let s3 = read_triple(server);
        println!(
            "[Q1] state3 (client closed):  ok={} n={} err={}  -> winmap={}",
            s3.0,
            s3.1,
            s3.2,
            winmap_says(s3.0, s3.1, s3.2)
        );

        unsafe {
            DisconnectNamedPipe(server);
            CloseHandle(server);
        }

        // The code's load-bearing assumptions (report a mismatch loudly):
        assert_eq!(
            winmap_says(s1.0, s1.1, s1.2),
            "WouldBlock",
            "state1 must map to WouldBlock, got triple {s1:?}"
        );
        assert_eq!(
            winmap_says(s2.0, s2.1, s2.2),
            "Data",
            "state2 must deliver the partial bytes, got triple {s2:?}"
        );
        assert_eq!(
            s2.1 as usize,
            msg.len(),
            "state2 must read the whole partial payload, got n={}",
            s2.1
        );
        assert_eq!(
            winmap_says(s3.0, s3.1, s3.2),
            "Closed",
            "state3 (client closed) must map to Closed, got triple {s3:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Q2 / Q3 — through the public Listener/request API (the real path).
// ---------------------------------------------------------------------------
use atrium::ipc::{request, Listener};

fn addr(tag: &str) -> String {
    format!(r"\\.\pipe\atrium-verify-{}-{tag}", std::process::id())
}

/// Drive the server (non-blocking poll/respond) until `f` says done or `within`.
fn pump<F: FnMut(&mut Listener) -> bool>(
    server: &mut Listener,
    within: Duration,
    mut f: F,
) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f(server) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

/// **Q2 — byte mode delivers a 20 KB request whole** (D4: message mode dropped
/// the head of anything over the 8 KiB buffer). The framer must reassemble the
/// stream; the server must see every byte.
#[test]
fn q2_byte_mode_delivers_a_20kb_request_intact() {
    // Sweep across the 8 KiB pipe-buffer boundary to localize any failure: which
    // sizes does the byte-mode server reassemble intact? Tight server polling (no
    // sleep) so client/server pacing can't be blamed. Each size gets its own bind.
    let sizes = [1024usize, 8192, 8193, 12 * 1024, 20 * 1024, 32 * 1024];
    let mut results: Vec<(usize, bool, usize)> = Vec::new();
    for (k, &size) in sizes.iter().enumerate() {
        let a = addr(&format!("q2-{k}"));
        let mut server = Listener::bind(&a).expect("bind");
        let payload = "X".repeat(size);
        let p2 = payload.clone();
        let a2 = a.clone();
        let client = std::thread::spawn(move || request(&a2, &p2));

        let mut seen: Option<String> = None;
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            match server.poll() {
                Ok(Some(req)) => {
                    let _ = server.respond("ok");
                    seen = Some(req);
                    break;
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        // Drain so the client can finish.
        let d2 = Instant::now() + Duration::from_secs(2);
        while Instant::now() < d2 {
            let _ = server.poll();
        }
        let _ = client.join();
        let got_len = seen.as_ref().map(|s| s.len()).unwrap_or(0);
        let ok = seen.as_deref() == Some(payload.as_str());
        println!("[Q2] size={size:>6}  received={ok}  bytes_seen={got_len}");
        results.push((size, ok, got_len));
    }
    let failures: Vec<_> = results.iter().filter(|(_, ok, _)| !*ok).collect();
    assert!(
        failures.is_empty(),
        "byte-mode reassembly failed at sizes {:?} (all are < MAX_REQUEST=64KiB, so should be accepted)",
        failures.iter().map(|(s, _, seen)| (*s, *seen)).collect::<Vec<_>>()
    );
}

/// **Q3a — a reply larger than the pipe buffer survives a normal read.** The
/// draining state machine (what replaced FlushFileBuffers) must flush a parked
/// reply across polls without loss.
#[test]
fn q3a_large_reply_survives_the_drain() {
    let a = addr("q3a");
    let mut server = Listener::bind(&a).expect("bind");
    let reply = "R".repeat(16 * 1024); // 16 KB > 8 KiB pipe buffer -> must park + drain
    let a2 = a.clone();
    let client = std::thread::spawn(move || request(&a2, "hello"));

    let r2 = reply.clone();
    let _ = pump(&mut server, Duration::from_secs(10), move |s| {
        if let Ok(Some(_req)) = s.poll() {
            s.respond(&r2).expect("respond");
            return true;
        }
        false
    });
    // Drain the parked reply to the reader.
    let _ = pump(&mut server, Duration::from_secs(5), |s| {
        let _ = s.poll();
        false
    });
    let got = client.join().expect("client thread").expect("reply");
    println!(
        "[Q3a] reply_bytes_received={} of {}",
        got.len(),
        reply.len()
    );
    assert_eq!(
        got, reply,
        "the large reply was truncated by the drain path"
    );
}

/// **Q3b — a non-reading client must NOT block the run loop (D2).** On `main`,
/// respond() calls FlushFileBuffers, which does not return until the client reads;
/// a client that connects, sends a request, and then never reads freezes the
/// server thread — every pane in every window. Here respond() parks and returns,
/// so the server keeps serving. Run against `main` this test HANGS (that is the
/// demonstration); here it completes fast.
#[test]
fn q3b_a_non_reading_client_does_not_block_the_loop() {
    use std::os::windows::ffi::OsStrExt;

    let a = addr("q3b");
    let mut server = Listener::bind(&a).expect("bind");

    // A deadbeat client: connect, write a request, then sleep forever WITHOUT
    // ever reading the reply. Raw pipe client so we fully control the read side.
    let a2 = a.clone();
    let _deadbeat = std::thread::spawn(move || {
        use std::ffi::c_void;
        type Handle = *mut c_void;
        const GENERIC_READ: u32 = 0x8000_0000;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const OPEN_EXISTING: u32 = 3;
        const INVALID: Handle = usize::MAX as Handle;
        #[link(name = "kernel32")]
        extern "system" {
            fn CreateFileW(
                n: *const u16,
                a: u32,
                s: u32,
                sec: *mut c_void,
                d: u32,
                f: u32,
                t: Handle,
            ) -> Handle;
            fn WriteFile(h: Handle, b: *const u8, l: u32, w: *mut u32, o: *mut c_void) -> i32;
        }
        let w: Vec<u16> = std::ffi::OsStr::new(&a2).encode_wide().chain([0]).collect();
        // Retry briefly until the pipe is available.
        let mut h = INVALID;
        for _ in 0..200 {
            h = unsafe {
                CreateFileW(
                    w.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if h != INVALID {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(h != INVALID, "deadbeat could not open the pipe");
        let req = b"deadbeat-request\n";
        let mut wrote = 0u32;
        unsafe {
            WriteFile(
                h,
                req.as_ptr(),
                req.len() as u32,
                &mut wrote,
                std::ptr::null_mut(),
            )
        };
        // Never read. Hold the handle open, sleeping.
        std::thread::sleep(Duration::from_secs(30));
        let _ = &h; // keep the handle alive
    });

    // Server: receive the deadbeat's request and respond. On main respond() blocks
    // here forever. Then prove the loop is still alive by serving a SECOND, normal
    // client end-to-end — all under a wall-clock bound.
    let t0 = Instant::now();
    let got_deadbeat = pump(&mut server, Duration::from_secs(8), |s| {
        if let Ok(Some(req)) = s.poll() {
            // This respond() is the D2 trigger: a reply to a client that will
            // never read it.
            let _ = s.respond("ack");
            return req.contains("deadbeat-request");
        }
        false
    });
    assert!(got_deadbeat, "server never saw the deadbeat request");
    let after_respond = t0.elapsed();
    println!(
        "[Q3b] respond() to a non-reading client returned after {:?} (main: never)",
        after_respond
    );

    // The loop must still serve. A fresh, well-behaved client must round-trip.
    let a3 = a.clone();
    let good = std::thread::spawn(move || request(&a3, "healthy"));
    let mut served_second = false;
    let _ = pump(&mut server, Duration::from_secs(8), |s| {
        if let Ok(Some(_req)) = s.poll() {
            let _ = s.respond("ok");
            served_second = true;
        }
        false
    });
    let _ = pump(&mut server, Duration::from_secs(2), |s| {
        let _ = s.poll();
        false
    });
    let good_reply = good.join().expect("good client thread");
    println!(
        "[Q3b] second client served={served_second} reply={:?}",
        good_reply
    );
    assert!(
        served_second,
        "the run loop was blocked — a non-reading client froze it (D2)"
    );
    assert!(
        after_respond < Duration::from_secs(8),
        "respond() to a non-reading client took {after_respond:?} — it blocked (D2)"
    );
}

/// **The real `atrium ctl` binary sends a >8 KiB request end to end.** Everything
/// above drives `request()`/`Listener` in-process; this drives the *actual*
/// `atrium ctl` executable as a separate process (no PTY needed — it reads its
/// endpoint from `ATRIUM_CTL`), so the D4 fix is proven through the shipped binary,
/// not just the library. On the pre-fix code this deadlocks; here the server
/// receives the full 20 KB payload and the client exits success.
#[test]
fn real_atrium_ctl_binary_delivers_a_20kb_request() {
    use std::process::{Command, Stdio};

    let a = addr("realbin");
    let mut server = Listener::bind(&a).expect("bind");
    let payload = "X".repeat(20 * 1024); // 20 KB, no interior newline

    let mut child = Command::new(env!("CARGO_BIN_EXE_atrium"))
        .args(["ctl", "send", "1", &payload])
        .env("ATRIUM_CTL", &a)
        .env("ATRIUM_PANE", "1")
        .env("ATRIUM_TOKEN", "test-token")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the real `atrium ctl` binary");

    let mut seen: Option<String> = None;
    let served = pump(&mut server, Duration::from_secs(15), |s| {
        if let Ok(Some(req)) = s.poll() {
            let _ = s.respond(r#"{"ok":true}"#);
            seen = Some(req);
            return true;
        }
        false
    });
    let _ = pump(&mut server, Duration::from_secs(2), |s| {
        let _ = s.poll();
        false
    });
    let out = child.wait_with_output().expect("wait for atrium ctl");
    let req = seen.unwrap_or_default();
    println!(
        "[REALBIN] served={served} request_bytes={} exit={:?}",
        req.len(),
        out.status.code()
    );
    assert!(
        served,
        "the real `atrium ctl` binary never delivered its request"
    );
    assert!(
        req.contains(&payload),
        "the 20 KB payload was not delivered intact through the real binary (req_len={}, exit={:?})",
        req.len(),
        out.status.code()
    );
    // Bonus end-to-end: with an ok reply, the client exits success.
    assert_eq!(
        out.status.code(),
        Some(0),
        "atrium ctl did not exit success"
    );
}

// ---------------------------------------------------------------------------
// D5 / D6 / D7 — the Windows counterparts of the unix-only exploit tests.
//   D6/D7 need a MALICIOUS SERVER (atrium is the client, via request());
//   D5 needs silent CLIENTS against atrium's real Listener.
// Extra care on the 8 KiB pipe-buffer boundary: the reply path crosses the same
// edge D4 deadlocked on, so a truncated LARGE reply is where a silent bug hides.
// ---------------------------------------------------------------------------
mod exploits {
    use super::{addr, request, Listener};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::time::{Duration, Instant};

    type Handle = *mut c_void;
    const INVALID: Handle = usize::MAX as Handle;
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateNamedPipeW(
            n: *const u16,
            o: u32,
            m: u32,
            mx: u32,
            ob: u32,
            ib: u32,
            t: u32,
            s: *mut c_void,
        ) -> Handle;
        fn ConnectNamedPipe(h: Handle, ov: *mut c_void) -> i32;
        fn DisconnectNamedPipe(h: Handle) -> i32;
        fn CreateFileW(
            n: *const u16,
            a: u32,
            sh: u32,
            s: *mut c_void,
            d: u32,
            f: u32,
            t: Handle,
        ) -> Handle;
        fn ReadFile(h: Handle, b: *mut u8, l: u32, r: *mut u32, ov: *mut c_void) -> i32;
        fn WriteFile(h: Handle, b: *const u8, l: u32, w: *mut u32, ov: *mut c_void) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetLastError() -> u32;
        fn FlushFileBuffers(h: Handle) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
    }

    /// Block (on the TEST peer only — never atrium's loop) until the client has read
    /// everything we sent, so a following disconnect can't discard unread bytes.
    /// This makes the truncation tests test "got the partial then EOF" and the
    /// control test test "got the whole honest reply".
    unsafe fn flush_to_client(server: Handle) {
        FlushFileBuffers(server);
    }

    /// A BLOCKING byte-mode named-pipe server — a scripted test peer, not atrium's
    /// NOWAIT run loop, so it may block on its own thread. Returns the handle as a
    /// `usize` so it can cross a thread boundary (a raw pointer is not `Send`).
    fn blocking_server(a: &str) -> usize {
        let w = wide(a);
        let h = unsafe {
            CreateNamedPipeW(
                w.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE,
                PIPE_UNLIMITED_INSTANCES,
                65536,
                65536,
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(h != INVALID, "CreateNamedPipeW failed: {}", unsafe {
            GetLastError()
        });
        h as usize
    }

    /// Accept one client and drain its request line (`\n`-terminated).
    unsafe fn accept_and_read_request(server: Handle) {
        ConnectNamedPipe(server, std::ptr::null_mut()); // blocks for a client / ERROR_PIPE_CONNECTED
        let mut buf = [0u8; 8192];
        loop {
            let mut n = 0u32;
            let ok = ReadFile(
                server,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            );
            if ok == 0 || n == 0 {
                break;
            }
            if buf[..n as usize].contains(&b'\n') {
                break;
            }
        }
    }

    /// Write all of `bytes`; `false` once the peer is gone.
    unsafe fn send_all(h: Handle, bytes: &[u8]) -> bool {
        let mut off = 0usize;
        while off < bytes.len() {
            let mut w = 0u32;
            let ok = WriteFile(
                h,
                bytes[off..].as_ptr(),
                (bytes.len() - off) as u32,
                &mut w,
                std::ptr::null_mut(),
            );
            if ok == 0 || w == 0 {
                return false;
            }
            off += w as usize;
        }
        true
    }

    /// D7 core: a malicious server sends `reply_len` bytes with **no** trailing
    /// newline, then closes. `request()` must return an ERROR (`UnexpectedEof`),
    /// never a truncated `Ok`.
    fn truncated_reply_must_err(reply_len: usize, label: &str) {
        let a = format!(r"\\.\pipe\atrium-d7-{}-{label}", std::process::id());
        let sh = blocking_server(&a);
        let srv = std::thread::spawn(move || unsafe {
            let server = sh as Handle;
            accept_and_read_request(server);
            let body = vec![b'R'; reply_len];
            let _ = send_all(server, &body); // partial: no '\n'
            flush_to_client(server); // client provably receives the partial bytes…
            DisconnectNamedPipe(server); // …then EOF with no newline
            CloseHandle(server);
        });
        let got = request(&a, "hi");
        let _ = srv.join();
        println!(
            "[D7:{label}] reply_len={reply_len} -> {:?}",
            got.as_ref().map(String::len).map_err(std::io::Error::kind)
        );
        assert!(
            got.is_err(),
            "[D7:{label}] a {reply_len}B reply with no newline must be an ERROR, not Ok({:?})",
            got.ok().map(|s| s.len())
        );
    }

    #[test]
    fn d7_small_truncated_reply_is_an_error() {
        truncated_reply_must_err(20, "small");
    }

    #[test]
    fn d7_large_truncated_reply_across_the_buffer_is_an_error() {
        // 12 KiB crosses the 8 KiB pipe buffer — the exact edge D4 deadlocked on.
        // A truncated large reply must still error, not return the fragment as Ok.
        truncated_reply_must_err(12 * 1024, "large");
    }

    #[test]
    fn d7_control_a_full_large_reply_is_ok() {
        // Guard against a false-positive: a large HONEST reply (WITH newline) must
        // succeed intact, so the truncation checks above aren't just "large=error".
        let a = format!(r"\\.\pipe\atrium-d7-{}-ctrl", std::process::id());
        let sh = blocking_server(&a);
        let srv = std::thread::spawn(move || unsafe {
            let server = sh as Handle;
            accept_and_read_request(server);
            let mut body = vec![b'R'; 12 * 1024];
            body.push(b'\n');
            let _ = send_all(server, &body);
            flush_to_client(server); // wait for the client to read the whole reply
            DisconnectNamedPipe(server);
            CloseHandle(server);
        });
        let got = request(&a, "hi").expect("a full large reply must succeed");
        let _ = srv.join();
        assert_eq!(
            got.len(),
            12 * 1024,
            "[D7 control] an honest 12 KiB reply must arrive intact"
        );
    }

    /// D6: a malicious server streams a reply past `MAX_REPLY` (8 MiB) with no
    /// newline. `request()` must refuse it (`InvalidData`), never accumulate
    /// forever or OOM.
    #[test]
    fn d6_an_unbounded_reply_is_refused() {
        let a = format!(r"\\.\pipe\atrium-d6-{}", std::process::id());
        let sh = blocking_server(&a);
        let srv = std::thread::spawn(move || unsafe {
            let server = sh as Handle;
            accept_and_read_request(server);
            let chunk = vec![b'R'; 64 * 1024];
            let mut sent = 0usize;
            while sent < 10 * 1024 * 1024 {
                if !send_all(server, &chunk) {
                    break; // client gave up and closed — the cap fired
                }
                sent += chunk.len();
            }
            DisconnectNamedPipe(server);
            CloseHandle(server);
        });
        let got = request(&a, "hi");
        let _ = srv.join();
        println!(
            "[D6] unbounded reply -> {:?}",
            got.as_ref().map(String::len).map_err(std::io::Error::kind)
        );
        assert!(
            got.is_err(),
            "[D6] an unbounded reply must be refused, got Ok({:?})",
            got.ok().map(|s| s.len())
        );
    }

    /// D5: silent clients must not lock out a real one. They fill the bounded
    /// instance pool, but each is capped by `CLIENT_DEADLINE`, so a real client is
    /// still served (within the deadline, not never).
    #[test]
    fn d5_silent_clients_cannot_lock_out_a_real_one() {
        let a = addr("d5");
        let mut server = Listener::bind(&a).expect("bind");
        // Stage more silent clients than the pool holds; each connects and says
        // nothing. Pump so the server parks them in Reading slots.
        let mut silent: Vec<Handle> = Vec::new();
        for _ in 0..12 {
            let w = wide(&a);
            let h = unsafe {
                CreateFileW(
                    w.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if h != INVALID {
                silent.push(h);
            }
            for _ in 0..3 {
                let _ = server.poll();
            }
        }
        // A real client must still be served — within the client deadline.
        let a2 = a.clone();
        let good = std::thread::spawn(move || request(&a2, "healthy"));
        let mut served = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_req)) = server.poll() {
                let _ = server.respond("ok");
                served = true;
                break;
            }
        }
        let drain = Instant::now() + Duration::from_secs(2);
        while Instant::now() < drain {
            let _ = server.poll();
        }
        let reply = good.join().expect("good client thread");
        for h in silent {
            unsafe { CloseHandle(h) };
        }
        println!("[D5] staged silent clients; real client served={served} reply={reply:?}");
        assert!(
            served && reply.is_ok(),
            "[D5] silent clients locked out a real client (served={served}, reply={reply:?})"
        );
    }
}
