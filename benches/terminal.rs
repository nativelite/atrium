//! atrium's performance benchmark: key echo latency, idle CPU and output-flood
//! throughput, each against the bare shell atrium hosts.
//!
//! Run: `cargo bench --bench terminal` (a release build). Under `cargo test`
//! this target only builds and returns, so the local gate stays fast.
//!
//! How it measures, so the numbers mean what they say:
//! - atrium (or the bare shell) runs on a real pseudo-terminal, and this harness
//!   plays the terminal: it types into the pty and reads atrium's screen output.
//! - Output is read on a thread of its own and **timestamped where it arrives**.
//!   A harness that polls with sleeps adds its own latency: an earlier one-off
//!   measured a bare shell at 1.6 ms that was really 0.1 ms.
//! - Key echo: write one key, time until that key comes back, erase it, repeat.
//! - Idle CPU: the process's own CPU time over a quiet window, as % of one core.
//! - Flood: a pane prints a deterministic, agent-style log file (coloured test
//!   output); time from launch until every pane exits, as MB/s. The pane blocks
//!   when atrium falls behind, so this is atrium's end-to-end consumption rate.
//! - Feed: the same log fed straight into one `vterm::Term`, in process: the
//!   emulator's own cost, separated from ptys, threads and the loop.
//!
//! Knobs (environment):
//! - `ATRIUM_BENCH_SAMPLES` (key echoes per scenario, default 100)
//! - `ATRIUM_BENCH_IDLE_S` (idle window, default 10)
//! - `ATRIUM_BENCH_FLOOD_MB` (per pane, default 16)
//! - `ATRIUM_BENCH_ONLY` (`latency`, `idle`, `feed` or `flood`)

use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const ATRIUM: &str = env!("CARGO_BIN_EXE_atrium");

fn main() {
    // `cargo test --all-targets` runs bench targets too; only `cargo bench`
    // passes `--bench`. Build-check under test, measure under bench.
    if !std::env::args().any(|a| a == "--bench") {
        println!("terminal bench: skipped (run `cargo bench --bench terminal`)");
        return;
    }
    let only = std::env::var("ATRIUM_BENCH_ONLY").ok();
    let wants = |what: &str| only.as_deref().map_or(true, |o| o == what);
    let samples = env_num("ATRIUM_BENCH_SAMPLES", 100);
    let idle_s = env_num("ATRIUM_BENCH_IDLE_S", 10);
    let flood_mb = env_num("ATRIUM_BENCH_FLOOD_MB", 16);

    println!("# atrium terminal benchmark\n");
    println!(
        "- atrium {} ({}), {} logical cores, {}",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        std::env::consts::OS
    );
    println!("- output timestamped on a reader thread; {samples} key echoes per scenario\n");

    let shell = Shell::native();
    if wants("latency") {
        println!("## Key echo latency\n");
        println!("| scenario | p50 ms | p90 ms | p99 ms | max ms | samples |");
        println!("| --- | --- | --- | --- | --- | --- |");
        for (label, prog, args) in shell.scenarios() {
            match key_echo(&prog, &args, samples) {
                Some(s) => println!(
                    "| {label} | {:.2} | {:.2} | {:.2} | {:.2} | {} |",
                    s.pct(50),
                    s.pct(90),
                    s.pct(99),
                    s.max(),
                    s.len()
                ),
                None => println!("| {label} | failed to echo | | | | |"),
            }
        }
        println!();
    }
    if wants("idle") {
        println!("## Idle CPU ({idle_s} s window)\n");
        println!("| scenario | CPU % of one core |");
        println!("| --- | --- |");
        for (label, prog, args) in shell.scenarios() {
            match idle_cpu(&prog, &args, Duration::from_secs(idle_s as u64)) {
                Some(pct) => println!("| {label} | {pct:.2} |"),
                None => println!("| {label} | unavailable on this platform |"),
            }
        }
        println!();
    }
    if wants("feed") {
        println!("## Emulator feed ({flood_mb} MB into one vterm::Term, in process)\n");
        println!("| grid | seconds | MB/s |");
        println!("| --- | --- | --- |");
        let file = flood_file(flood_mb);
        let bytes = std::fs::read(&file).expect("read flood file");
        let _ = std::fs::remove_file(&file);
        for (rows, cols) in [(24usize, 80usize), (40, 160), (60, 240)] {
            let mut term = vterm::Term::new(rows, cols);
            let start = Instant::now();
            // Chunked the way a pane reader delivers output.
            for chunk in bytes.chunks(8192) {
                term.feed(chunk);
            }
            let secs = start.elapsed().as_secs_f64();
            println!(
                "| {rows}x{cols} | {secs:.2} | {:.1} |",
                flood_mb as f64 / secs
            );
        }
        println!();
    }
    if wants("flood") {
        println!("## Output flood ({flood_mb} MB of coloured log output per pane)\n");
        println!("| scenario | seconds | MB/s |");
        println!("| --- | --- | --- |");
        let file = flood_file(flood_mb);
        for (label, prog, args, panes) in shell.floods(&file) {
            match flood(&prog, &args) {
                Some(secs) => println!(
                    "| {label} | {secs:.2} | {:.1} |",
                    (flood_mb * panes) as f64 / secs
                ),
                None => println!("| {label} | did not finish | |"),
            }
        }
        let _ = std::fs::remove_file(&file);
        println!();
    }
}

fn env_num(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// The shell atrium hosts on this platform, and the scenarios built from it.
struct Shell {
    prog: &'static str,
    interactive: Vec<&'static str>,
}

impl Shell {
    fn native() -> Shell {
        if cfg!(windows) {
            Shell {
                prog: "cmd",
                interactive: vec!["/Q"],
            }
        } else {
            Shell {
                prog: "sh",
                interactive: vec!["-i"],
            }
        }
    }

    /// (label, program, args) for the interactive scenarios.
    fn scenarios(&self) -> Vec<(String, String, Vec<String>)> {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut bare = vec![];
        bare.extend(own(&self.interactive));
        let mut one = vec![self.prog.to_string()];
        one.extend(own(&self.interactive));
        let mut eight = vec!["-n".to_string(), "8".to_string(), self.prog.to_string()];
        eight.extend(own(&self.interactive));
        vec![
            (format!("bare {}", self.prog), self.prog.to_string(), bare),
            ("atrium, 1 pane".to_string(), ATRIUM.to_string(), one),
            (
                "atrium, 8 tiled panes".to_string(),
                ATRIUM.to_string(),
                eight,
            ),
        ]
    }

    /// (label, program, args, panes) for the flood scenarios over `file`.
    fn floods(&self, file: &std::path::Path) -> Vec<(String, String, Vec<String>, usize)> {
        let path = file.display().to_string();
        let (cat, catargs): (String, Vec<String>) = if cfg!(windows) {
            ("cmd".into(), vec!["/C".into(), format!("type \"{path}\"")])
        } else {
            ("cat".into(), vec![path.clone()])
        };
        let mut one = vec![cat.clone()];
        one.extend(catargs.clone());
        let mut eight = vec!["-n".to_string(), "8".to_string(), cat.clone()];
        eight.extend(catargs.clone());
        vec![
            (format!("bare {cat}"), cat, catargs, 1),
            ("atrium, 1 pane".to_string(), ATRIUM.to_string(), one, 1),
            (
                "atrium, 8 tiled panes".to_string(),
                ATRIUM.to_string(),
                eight,
                8,
            ),
        ]
    }
}

/// A program on a pty plus a thread timestamping everything it prints.
struct Session {
    pty: pty::Pty,
    out: Receiver<(Instant, Vec<u8>)>,
}

impl Session {
    fn start(prog: &str, args: &[String]) -> Option<Session> {
        let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
        let pty = pty::Pty::spawn(prog, &argrefs, 40, 160).ok()?;
        let mut reader = pty.reader().ok()?;
        let (tx, out) = channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send((Instant::now(), buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Some(Session { pty, out })
    }

    /// Discard output for `d`.
    fn drain(&self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            let _ = self.out.recv_timeout(Duration::from_millis(20));
        }
    }

    /// Quit cleanly (atrium: Ctrl+A q), killing whatever is left.
    fn stop(mut self, is_atrium: bool) {
        if is_atrium {
            let _ = self.pty.write(b"\x01q");
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                if matches!(self.pty.try_wait(), Ok(Some(_))) {
                    return;
                }
                self.drain(Duration::from_millis(50));
            }
        }
        let _ = self.pty.kill();
    }
}

/// Settle time before measuring: the shell's prompt, and atrium's startup logo
/// hold (1.2 s) plus margin.
const SETTLE: Duration = Duration::from_secs(4);

fn key_echo(prog: &str, args: &[String], samples: usize) -> Option<Stats> {
    let mut s = Session::start(prog, args)?;
    s.drain(SETTLE);
    let mut lat = Vec::with_capacity(samples);
    for _ in 0..samples {
        s.drain(Duration::from_millis(30));
        let sent = Instant::now();
        s.pty.write(b"z").ok()?;
        let mut seen = Vec::new();
        let arrived = loop {
            match s.out.recv_timeout(Duration::from_secs(2)) {
                Ok((at, bytes)) => {
                    seen.extend_from_slice(&bytes);
                    if seen.contains(&b'z') {
                        break Some(at);
                    }
                }
                Err(_) => break None,
            }
        };
        let _ = s.pty.write(b"\x08");
        let at = arrived?;
        lat.push(at.duration_since(sent).as_secs_f64() * 1000.0);
    }
    s.stop(prog == ATRIUM);
    Some(Stats::new(lat))
}

fn idle_cpu(prog: &str, args: &[String], window: Duration) -> Option<f64> {
    let s = Session::start(prog, args)?;
    s.drain(SETTLE);
    let pid = s.pty.pid();
    let before = cpu_ms(pid);
    let start = Instant::now();
    s.drain(window);
    let after = cpu_ms(pid);
    let wall = start.elapsed().as_secs_f64() * 1000.0;
    s.stop(prog == ATRIUM);
    Some((after? - before?) / wall * 100.0)
}

fn flood(prog: &str, args: &[String]) -> Option<f64> {
    let start = Instant::now();
    let mut s = Session::start(prog, args)?;
    let limit = Duration::from_secs(600);
    while start.elapsed() < limit {
        if matches!(s.pty.try_wait(), Ok(Some(_))) {
            return Some(start.elapsed().as_secs_f64());
        }
        let _ = s.out.recv_timeout(Duration::from_millis(5));
    }
    let _ = s.pty.kill();
    None
}

/// A deterministic, agent-style log: timestamps, pane tags, coloured PASS/FAIL,
/// long test paths. `mb` MiB, written to the temp directory.
fn flood_file(mb: usize) -> std::path::PathBuf {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("atrium-bench-flood-{}.log", std::process::id()));
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).expect("flood file"));
    let target = mb * 1024 * 1024;
    let mut written = 0usize;
    let mut i = 0u64;
    while written < target {
        let (colour, verdict) = if i % 17 == 0 {
            ("31", "FAIL")
        } else {
            ("32", "PASS")
        };
        let line = format!(
            "2026-09-15T12:{:02}:{:02}.{:03}Z [agent-{}] \x1b[{colour}m{verdict}\x1b[0m \
             tests::suite_{:03}::case_{:06}_handles_input_without_panicking ... ok ({} ms)\r\n",
            (i / 60_000) % 60,
            (i / 1000) % 60,
            i % 1000,
            i % 8,
            (i / 97) % 1000,
            i,
            (i * 7) % 250
        );
        f.write_all(line.as_bytes()).expect("write flood file");
        written += line.len();
        i += 1;
    }
    f.flush().expect("flush flood file");
    path
}

/// Sorted samples with nearest-rank percentiles.
struct Stats(Vec<f64>);

impl Stats {
    fn new(mut v: Vec<f64>) -> Stats {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Stats(v)
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn pct(&self, p: usize) -> f64 {
        if self.0.is_empty() {
            return f64::NAN;
        }
        let rank = ((p * self.0.len() + 99) / 100).max(1);
        self.0[rank.min(self.0.len()) - 1]
    }
    fn max(&self) -> f64 {
        self.0.last().copied().unwrap_or(f64::NAN)
    }
}

/// Total CPU time (user + kernel) of `pid`, in milliseconds.
#[cfg(windows)]
fn cpu_ms(pid: u32) -> Option<f64> {
    use core::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn CloseHandle(h: *mut c_void) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    let ticks = |t: &FileTime| ((t.high as u64) << 32 | t.low as u64) as f64 / 10_000.0;
    // SAFETY: a handle opened for query only, four out-structs sized by type,
    // closed on every path.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let (mut c, mut e, mut k, mut u) = Default::default();
        let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0;
        CloseHandle(h);
        ok.then(|| ticks(&k) + ticks(&u))
    }
}

/// Total CPU time (user + system) of `pid`, in milliseconds, from `/proc`.
#[cfg(target_os = "linux")]
fn cpu_ms(pid: u32) -> Option<f64> {
    extern "C" {
        fn sysconf(name: i32) -> i64;
    }
    const SC_CLK_TCK: i32 = 2;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name; utime and stime are 14 and 15.
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    // SAFETY: sysconf with a valid name has no preconditions.
    let hz = unsafe { sysconf(SC_CLK_TCK) } as f64;
    (hz > 0.0).then(|| (utime + stime) * 1000.0 / hz)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn cpu_ms(_pid: u32) -> Option<f64> {
    None
}
