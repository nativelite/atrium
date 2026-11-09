//! Soak: a modded-style fleet under load for a long stretch, watched for leaks
//! and stalls in the held inbox and the waking control channel (0.44.0,
//! `nativelite-ipc` 0.2). Ignored by default — it runs for `ATRIUM_SOAK_SECS`
//! (default 60) and the gate must stay fast:
//!
//! ```sh
//! ATRIUM_SOAK_SECS=600 cargo test --test atrium soak -- --ignored --nocapture
//! ```
//!
//! The shape: atrium `--allow-ctl -n 8 <shell>` in a pty. Each pane's shell
//! writes its own `ATRIUM_TOKEN` to a file as it starts, so the test can speak
//! for every pane from here (`atrium::ctl::build_request` + that token, the
//! same line `atrium ctl` would send) without typing into tiles. Every pane
//! registers as a modded pane (`hello mod=… caps=status,inbox`) and loops
//! `inbox --wait`, held open by the run loop (`serve_held_inboxes`); the lead
//! (pane 0, a root pane, so privileged) sends a numbered marker to a random pane
//! twice a second; the host (token-free, by pid, as a terminal would) asks
//! `list` five times a second. Every 5 s the atrium process's working set and
//! handle (fd) count are sampled.
//!
//! Fails on: RSS or handle growth over 20 % between the minute-1 sample and the
//! end (a run shorter than two minutes compares its midpoint), a marker not
//! received within 10 s of its send (or refused), a host request over 5 s or
//! refused, or a final `list` that does not answer within 1 s. The table it
//! prints is what `benches/README.md` records under "Soak".

use crate::support::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PANES: usize = 8;
/// The host's `list` cadence: five a second.
const LIST_EVERY: Duration = Duration::from_millis(200);
/// The lead's `send` cadence: two a second.
const SEND_EVERY: Duration = Duration::from_millis(500);
const SAMPLE_EVERY: Duration = Duration::from_secs(5);
/// A send must reach its waiting pane within this.
const DELIVERY_BUDGET: Duration = Duration::from_secs(10);
/// No host request may take longer than this.
const HOST_BUDGET: Duration = Duration::from_secs(5);
/// The final `list` must answer within this.
const FINAL_LIST_BUDGET: Duration = Duration::from_secs(1);
/// Resource growth tolerated between the baseline sample and the end.
const GROWTH_CAP: f64 = 0.20;
/// The baseline sample is the first at or after this (or the midpoint of a
/// shorter run).
const BASELINE_AT: Duration = Duration::from_secs(60);

fn soak_secs() -> u64 {
    std::env::var("ATRIUM_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
}

/// One resource sample of the atrium process: working set (bytes) and the
/// handle count (Windows) or open fds (Linux).
#[derive(Clone, Copy)]
struct Sample {
    at: Duration,
    rss: u64,
    handles: u64,
}

/// Working set in bytes and handle count of `pid`, via kernel32 (the same
/// query-only handle `benches/terminal.rs` opens for `GetProcessTimes`).
#[cfg(windows)]
fn probe(pid: u32) -> Option<(u64, u64)> {
    use core::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(h: *mut c_void) -> i32;
        fn GetProcessHandleCount(h: *mut c_void, count: *mut u32) -> i32;
        fn K32GetProcessMemoryInfo(
            h: *mut c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    let mut handles: u32 = 0;
    // SAFETY: a handle opened for query only; both out-pointers are to live
    // locals sized by type (`cb` says so for the struct); closed on every path.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let ok_handles = GetProcessHandleCount(h, &mut handles) != 0;
        let ok_memory = K32GetProcessMemoryInfo(h, &mut counters, counters.cb) != 0;
        CloseHandle(h);
        (ok_handles && ok_memory).then_some((counters.working_set_size as u64, handles as u64))
    }
}

/// Resident set in bytes (`VmRSS`) and open fds of `pid`, from `/proc`.
#[cfg(target_os = "linux")]
fn probe(pid: u32) -> Option<(u64, u64)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss_kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count() as u64;
    Some((rss_kb * 1024, fds))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn probe(_pid: u32) -> Option<(u64, u64)> {
    None
}

/// The request line `atrium ctl <args>` would send, as this pane: the token
/// spliced in where `ctl_cmd` would have read it from `ATRIUM_TOKEN` (the env
/// is process-wide; eight panes' clients share this process).
fn request_as(token: &str, args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let line = atrium::ctl::build_request(&args, None).unwrap();
    assert!(line.starts_with('{'), "a request is a JSON object: {line}");
    format!("{{\"token\":\"{token}\",{}", &line[1..])
}

/// The host's request: no token, no caller.
fn request_host(args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    atrium::ctl::build_request(&args, None).unwrap()
}

/// Every `soak-<n>` marker carried as a `send` item in an inbox reply.
fn markers_in(reply: &str) -> Vec<u64> {
    const KEY: &str = "\"text\":\"soak-";
    let mut out = Vec::new();
    let mut rest = reply;
    while let Some(i) = rest.find(KEY) {
        rest = &rest[i + KEY.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse() {
            out.push(n);
        }
    }
    out
}

fn is_ok(reply: &str) -> bool {
    reply.contains("\"ok\":true")
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Relative growth from `base` to `end`; a zero base grows nothing.
fn growth(base: u64, end: u64) -> f64 {
    if base == 0 {
        0.0
    } else {
        end as f64 / base as f64 - 1.0
    }
}

/// A directory the pane shells can write their tokens into. Space-free: the
/// Windows pane command is a `cmd /K` redirect, and a quoted path there would
/// have to survive both argv and cmd quoting.
fn token_dir() -> std::path::PathBuf {
    let name = format!("atrium-soak-{}", std::process::id());
    let candidates = [
        std::env::temp_dir(),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
    ];
    let base = candidates
        .iter()
        .find(|p| !p.to_string_lossy().contains(' '))
        .unwrap_or_else(|| {
            panic!("no space-free directory for the token files; set TMP/TMPDIR to one")
        });
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Pseudo-random pane picks: xorshift64, seeded from the clock. Nothing here
/// needs more than "not the same pane every time".
struct Rng(u64);

impl Rng {
    fn new() -> Rng {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        Rng(nanos | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Keep the pty drained for the whole run (a host that stops reading blocks a
/// child mid-write), remembering the tail for the setup waits.
fn drain_in_background(p: &pty::Pty) -> Arc<Mutex<Vec<u8>>> {
    const KEEP: usize = 256 * 1024;
    let tail = Arc::new(Mutex::new(Vec::new()));
    let mut reader = p.reader().unwrap();
    let sink = tail.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let mut t = sink.lock().unwrap();
                    t.extend_from_slice(&buf[..n]);
                    if t.len() > KEEP {
                        let cut = t.len() - KEEP;
                        t.drain(..cut);
                    }
                }
            }
        }
    });
    tail
}

fn wait_on_screen(tail: &Mutex<Vec<u8>>, needle: &[u8], deadline: Duration) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if contains(&tail.lock().unwrap(), needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// What the waiters record, shared with the lead and the report.
#[derive(Default)]
struct Waiters {
    /// `(marker, received at)`, in arrival order.
    received: Mutex<Vec<(u64, Instant)>>,
    replies: AtomicU64,
    /// Replies that carried no item: a hold the server let expire
    /// (`HOLD_DEADLINE`), answered empty for the pane to ask again.
    empty: AtomicU64,
    /// `ok:false` replies and transport errors, the first few kept verbatim.
    errors: AtomicU64,
    error_samples: Mutex<Vec<String>>,
}

impl Waiters {
    fn error(&self, what: String) {
        self.errors.fetch_add(1, Ordering::SeqCst);
        let mut s = self.error_samples.lock().unwrap();
        if s.len() < 8 {
            s.push(what);
        }
    }
}

#[test]
#[ignore = "a soak of ATRIUM_SOAK_SECS (default 60) seconds; run by hand: cargo test --test atrium soak -- --ignored --nocapture"]
fn soak_held_inboxes_and_waking_channel() {
    let duration = Duration::from_secs(soak_secs());
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let dir = token_dir();
    let dir_s = dir.to_string_lossy().to_string();

    // Each pane's shell writes its token as it starts, then stays interactive.
    let (shell, pane_args): (&str, Vec<String>) = if cfg!(windows) {
        (
            "cmd",
            vec![
                "/Q".into(),
                "/K".into(),
                "echo".into(),
                format!("%ATRIUM_TOKEN%>{dir_s}\\%ATRIUM_PANE%.tok"),
            ],
        )
    } else {
        (
            "sh",
            vec![
                "-c".into(),
                format!("echo $ATRIUM_TOKEN > '{dir_s}/'$ATRIUM_PANE.tok; exec sh -i"),
            ],
        )
    };
    let mut argv: Vec<&str> = vec!["--allow-ctl", "-n", "8", shell];
    argv.extend(pane_args.iter().map(String::as_str));
    let mut p = pty::Pty::spawn_full(atrium, &argv, 40, 160, &[], None).unwrap();
    let pid = p.pid();
    let address = atrium::ipc::address_for(pid);
    let tail = drain_in_background(&p);

    // All eight tiles drawn, then all eight tokens on disk.
    let last_label = format!("{PANES}:{shell}");
    assert!(
        wait_on_screen(&tail, last_label.as_bytes(), Duration::from_secs(30)),
        "the {PANES}-pane grid never finished drawing: {:?}",
        String::from_utf8_lossy(&tail.lock().unwrap())
    );
    let tokens: Vec<String> = {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let found: Vec<Option<String>> = (0..PANES)
                .map(|i| {
                    std::fs::read_to_string(dir.join(format!("{i}.tok")))
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| s.len() >= 32)
                })
                .collect();
            if found.iter().all(Option::is_some) {
                break found.into_iter().map(Option::unwrap).collect();
            }
            assert!(
                Instant::now() < end,
                "not every pane wrote its token to {dir_s}: {:?}",
                found.iter().map(Option::is_some).collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let _ = std::fs::remove_dir_all(&dir);

    // Every pane is a modded pane that takes its own deliveries.
    for (i, token) in tokens.iter().enumerate() {
        let line = request_as(token, &["hello", "mod=0.1.0", "caps=status,inbox"]);
        let reply = atrium::ipc::request(&address, &line).unwrap();
        assert!(
            is_ok(&reply) && reply.contains("\"inbox\""),
            "pane {i}'s hello was not taken with the inbox cap: {reply}"
        );
    }

    let stop = Arc::new(AtomicBool::new(false));
    let waiters = Arc::new(Waiters::default());
    let started = Instant::now();

    // Eight waiters, one per pane, each holding an `inbox --wait` open.
    for token in &tokens {
        let (stop, waiters, address) = (stop.clone(), waiters.clone(), address.clone());
        let line = request_as(token, &["inbox", "--wait"]);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match atrium::ipc::request_held(&address, &line, atrium::ipc::HOLD_DEADLINE) {
                    Ok(reply) if is_ok(&reply) => {
                        waiters.replies.fetch_add(1, Ordering::SeqCst);
                        let now = Instant::now();
                        let got = markers_in(&reply);
                        if got.is_empty() && !reply.contains("soak-stop") {
                            waiters.empty.fetch_add(1, Ordering::SeqCst);
                        }
                        let mut r = waiters.received.lock().unwrap();
                        r.extend(got.into_iter().map(|m| (m, now)));
                    }
                    Ok(reply) => {
                        waiters.error(reply);
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        waiters.error(format!("transport: {e}"));
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        });
    }

    // The lead: a numbered marker to a random pane, twice a second.
    let lead = {
        let (stop, address) = (stop.clone(), address.clone());
        let token = tokens[0].clone();
        std::thread::spawn(move || {
            let mut rng = Rng::new();
            let mut sent: Vec<(u64, usize, Instant)> = Vec::new();
            let mut refused: Vec<String> = Vec::new();
            let mut seq = 0u64;
            while !stop.load(Ordering::SeqCst) {
                seq += 1;
                let target = (rng.next() % PANES as u64) as usize;
                let marker = format!("soak-{seq}");
                let line = request_as(&token, &["send", &target.to_string(), &marker]);
                let at = Instant::now();
                match atrium::ipc::request(&address, &line) {
                    Ok(reply) if is_ok(&reply) => sent.push((seq, target, at)),
                    Ok(reply) => refused.push(format!("{marker} -> pane {target}: {reply}")),
                    Err(e) => refused.push(format!("{marker} -> pane {target}: transport: {e}")),
                }
                std::thread::sleep(SEND_EVERY);
            }
            // Wake every waiter so it sees the stop flag.
            for pane in 0..PANES {
                let line = request_as(&token, &["send", &pane.to_string(), "soak-stop"]);
                let _ = atrium::ipc::request(&address, &line);
            }
            (sent, refused)
        })
    };

    // The host: a token-free `list` five times a second, timed.
    let host = {
        let (stop, address) = (stop.clone(), address.clone());
        let line = request_host(&["list"]);
        std::thread::spawn(move || {
            let mut latencies: Vec<f64> = Vec::new();
            let mut errors: Vec<String> = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                let at = Instant::now();
                let outcome = atrium::ipc::request(&address, &line);
                latencies.push(ms(at.elapsed()));
                match outcome {
                    Ok(reply) if is_ok(&reply) => {}
                    Ok(reply) => errors.push(reply),
                    Err(e) => errors.push(format!("transport: {e}")),
                }
                std::thread::sleep(LIST_EVERY);
            }
            (latencies, errors)
        })
    };

    // The sampler, on this thread, for the whole run.
    let mut samples: Vec<Sample> = Vec::new();
    let mut next = started;
    while started.elapsed() < duration {
        if Instant::now() >= next {
            if let Some((rss, handles)) = probe(pid) {
                samples.push(Sample {
                    at: started.elapsed(),
                    rss,
                    handles,
                });
            }
            next += SAMPLE_EVERY;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::SeqCst);
    let (sent, refused) = lead.join().unwrap();
    let (mut host_latencies, host_errors) = host.join().unwrap();

    // Let the last sends land, then take the end sample.
    let settle_end = Instant::now() + DELIVERY_BUDGET;
    loop {
        let got = waiters.received.lock().unwrap().len();
        if got >= sent.len() || Instant::now() >= settle_end {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some((rss, handles)) = probe(pid) {
        samples.push(Sample {
            at: started.elapsed(),
            rss,
            handles,
        });
    }

    // The channel still answers a host promptly after all of that.
    let final_at = Instant::now();
    let final_reply = atrium::ipc::request(&address, &request_host(&["list"]));
    let final_ms = ms(final_at.elapsed());
    let final_ok =
        matches!(&final_reply, Ok(r) if is_ok(r)) && final_at.elapsed() <= FINAL_LIST_BUDGET;

    // Quit atrium; the held connections close with it and the waiters return.
    p.write(b"\x01q").unwrap();
    let quit_end = Instant::now() + Duration::from_secs(15);
    while p.try_wait().unwrap().is_none() && Instant::now() < quit_end {
        std::thread::sleep(Duration::from_millis(100));
    }
    if p.try_wait().unwrap().is_none() {
        let _ = p.kill();
    }

    // --- the reckoning -------------------------------------------------------
    let received = waiters.received.lock().unwrap().clone();
    let mut delivery_ms: Vec<f64> = Vec::new();
    let mut late: Vec<String> = Vec::new();
    for (seq, target, at) in &sent {
        match received.iter().find(|(m, _)| m == seq) {
            Some((_, when)) => {
                let took = when.saturating_duration_since(*at);
                delivery_ms.push(ms(took));
                if took > DELIVERY_BUDGET {
                    late.push(format!("soak-{seq} -> pane {target}: {:.0} ms", ms(took)));
                }
            }
            None => late.push(format!("soak-{seq} -> pane {target}: never received")),
        }
    }
    delivery_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    host_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let host_max = host_latencies.last().copied().unwrap_or(0.0);

    let first = samples.first().copied();
    let end = samples.last().copied();
    let baseline_at = BASELINE_AT.min(duration / 2);
    let baseline = samples
        .iter()
        .find(|s| s.at >= baseline_at)
        .copied()
        .or(first);
    let (rss_growth, handle_growth) = match (baseline, end) {
        (Some(b), Some(e)) => (growth(b.rss, e.rss), growth(b.handles, e.handles)),
        _ => (0.0, 0.0),
    };
    let fmt_sample = |s: Option<Sample>, f: fn(&Sample) -> String| {
        s.map(|s| f(&s)).unwrap_or_else(|| "n/a".to_string())
    };
    let rss_of = |s: &Sample| format!("{:.1}", mb(s.rss));
    let handles_of = |s: &Sample| s.handles.to_string();

    let os = std::env::consts::OS;
    let waiter_errors = waiters.errors.load(Ordering::SeqCst);
    let table = format!(
        "\n\
         | metric | value |\n\
         | --- | --- |\n\
         | run | {secs} s, {PANES} panes, {os} |\n\
         | rss MB start / baseline ({base_at} s) / end | {rss_start} / {rss_base} / {rss_end} ({rss_pct:+.1} %) |\n\
         | handles start / baseline / end | {h_start} / {h_base} / {h_end} ({h_pct:+.1} %) |\n\
         | host list requests | {host_n} ({host_err} refused or failed) |\n\
         | host p50 / p99 / max ms | {host_p50:.2} / {host_p99:.2} / {host_max:.2} |\n\
         | sends / received / late or lost / refused | {sends} / {recv} / {late} / {refused} |\n\
         | delivery p50 / p99 / max ms | {d_p50:.1} / {d_p99:.1} / {d_max:.1} |\n\
         | waiter replies / expired holds / errors | {w_replies} / {w_empty} / {w_err} |\n\
         | final list ms | {final_ms:.2} |\n",
        secs = duration.as_secs(),
        base_at = baseline.map(|b| b.at.as_secs()).unwrap_or(0),
        rss_start = fmt_sample(first, rss_of),
        rss_base = fmt_sample(baseline, rss_of),
        rss_end = fmt_sample(end, rss_of),
        rss_pct = rss_growth * 100.0,
        h_start = fmt_sample(first, handles_of),
        h_base = fmt_sample(baseline, handles_of),
        h_end = fmt_sample(end, handles_of),
        h_pct = handle_growth * 100.0,
        host_n = host_latencies.len(),
        host_err = host_errors.len(),
        host_p50 = pct(&host_latencies, 0.50),
        host_p99 = pct(&host_latencies, 0.99),
        sends = sent.len(),
        recv = delivery_ms.len(),
        late = late.len(),
        refused = refused.len(),
        d_p50 = pct(&delivery_ms, 0.50),
        d_p99 = pct(&delivery_ms, 0.99),
        d_max = delivery_ms.last().copied().unwrap_or(0.0),
        w_replies = waiters.replies.load(Ordering::SeqCst),
        w_empty = waiters.empty.load(Ordering::SeqCst),
        w_err = waiter_errors,
    );
    println!("soak_held_inboxes_and_waking_channel:{table}");
    if waiter_errors > 0 {
        println!(
            "waiter error samples: {:#?}",
            waiters.error_samples.lock().unwrap()
        );
    }

    let mut failures: Vec<String> = Vec::new();
    if cfg!(any(windows, target_os = "linux")) {
        if samples.len() < 2 {
            failures.push("fewer than two resource samples; the process probe failed".into());
        }
        if rss_growth > GROWTH_CAP {
            failures.push(format!(
                "rss grew {:.1} % from the baseline",
                rss_growth * 100.0
            ));
        }
        if handle_growth > GROWTH_CAP {
            failures.push(format!(
                "handle count grew {:.1} % from the baseline",
                handle_growth * 100.0
            ));
        }
    }
    if !late.is_empty() {
        failures.push(format!(
            "{} sends not received within {} s: {:?}",
            late.len(),
            DELIVERY_BUDGET.as_secs(),
            late.iter().take(8).collect::<Vec<_>>()
        ));
    }
    if !refused.is_empty() {
        failures.push(format!(
            "{} sends refused: {:?}",
            refused.len(),
            refused.iter().take(8).collect::<Vec<_>>()
        ));
    }
    if sent.is_empty() {
        failures.push("the lead sent nothing".into());
    }
    if host_max > ms(HOST_BUDGET) {
        failures.push(format!("a host request took {host_max:.0} ms"));
    }
    if !host_errors.is_empty() {
        failures.push(format!(
            "{} host requests refused or failed: {:?}",
            host_errors.len(),
            host_errors.iter().take(8).collect::<Vec<_>>()
        ));
    }
    if !final_ok {
        failures.push(format!(
            "the final list did not answer within {} ms: {final_ms:.0} ms, {final_reply:?}",
            FINAL_LIST_BUDGET.as_millis()
        ));
    }
    assert!(
        failures.is_empty(),
        "soak failed:\n  - {}\n{table}",
        failures.join("\n  - ")
    );
}
