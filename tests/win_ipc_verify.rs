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
        let addr = format!(r"\\.\pipe\amux-q1-{}", std::process::id());
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
use amux::ipc::{request, Listener};

fn addr(tag: &str) -> String {
    format!(r"\\.\pipe\amux-verify-{}-{tag}", std::process::id())
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
