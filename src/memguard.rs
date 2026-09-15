//! A memory ceiling for everything the panes run, so a session can't take the
//! machine down with it.
//!
//! The compile pool ([`crate::buildpool`]) bounds how many compiler jobs run at
//! once, but not how large they get, and nothing bounds a runaway test binary or
//! an agent's own tooling. The machine-killing failure is commit exhaustion:
//! once committed memory reaches the commit limit, every process on the box
//! starts failing allocations at once — atrium, the terminal, the browser.
//!
//! Every pane already runs inside the session Job Object ([`crate::reap::SessionJob`]),
//! and atrium itself does not, so a job memory limit caps the panes' committed
//! memory without ever capping atrium. The guard keeps that limit current on the
//! warden cadence:
//!
//! - **Dynamic by default.** The limit is what the panes use now plus the free
//!   commit left on the machine, minus a reserve for everything else. As other
//!   programs grow, the panes' ceiling shrinks with them, so the session can use
//!   the machine's spare memory but can never be the thing that exhausts it.
//! - **Settable.** `ATRIUM_MEMORY_MB` or a fleet's `memory_mb` sets a fixed cap,
//!   which still never exceeds the dynamic limit. `off` / `0` disables the guard.
//! - **Builds give way first.** At [`PRESSURE_PERCENT`] of the limit the guard
//!   kills the largest build process in the session — a compiler or build script,
//!   never an agent — and says so. The hard limit is the backstop for what that
//!   can't relieve: allocations past it fail inside the panes, not across the box.
//!
//! **Unix is soft.** There is no Job Object, and its closest equivalent (a
//! cgroup) isn't reliably available to an unprivileged process: a shell in a
//! terminal usually shares a cgroup it doesn't own with its siblings. So on Linux
//! and macOS the guard watches instead of capping: it finds each pane's processes
//! by session (every pane is a session leader), stops the largest build when the
//! machine drops into its reserve or the panes reach a fixed cap, and binds that
//! kill to the victim's start time. On Linux it also marks every build as the
//! kernel OOM killer's preferred victim, because there the failure isn't a
//! system-wide allocation collapse but the OOM killer choosing one process —
//! and it should choose a build, not atrium or the desktop. Nothing stops an
//! allocation between ticks, and the banner says `(soft)`.

use std::sync::OnceLock;

/// Operator override: a fixed cap in MiB, or `off`/`0` to disable the guard.
pub const ENV_MEMORY_MB: &str = "ATRIUM_MEMORY_MB";
/// Share of the limit at which the guard stops the largest build process.
pub const PRESSURE_PERCENT: u64 = 90;
/// The smallest reserve kept free for the rest of the machine.
const MIN_RESERVE: u64 = 2 * GIB;
/// The lowest limit ever set. Without a floor, an empty job on a machine
/// already inside its reserve computes a limit of 0, and the next pane can't
/// commit a byte. A fixed floor (not headroom over current use) leaves room to
/// start panes without letting a busy session creep upward tick by tick.
const MIN_LIMIT: u64 = GIB;
const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// How the panes' memory is capped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    /// No limit at all.
    Off,
    /// Track the machine's free commit (the default).
    Dynamic,
    /// A fixed ceiling in bytes, still bounded by the dynamic limit.
    Fixed(u64),
}

/// The cap from [`ENV_MEMORY_MB`] (`env`) and a fleet's `memory_mb`. The env
/// wins outright; an unparseable env value is ignored rather than read as off,
/// so a typo can't silently remove the ceiling.
pub fn parse_cap(env: Option<&str>, fleet_mb: Option<u64>) -> Cap {
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        if v.eq_ignore_ascii_case("off") {
            return Cap::Off;
        }
        if let Ok(mb) = v.parse::<u64>() {
            return if mb == 0 {
                Cap::Off
            } else {
                Cap::Fixed(mb.saturating_mul(MIB))
            };
        }
    }
    match fleet_mb {
        Some(0) => Cap::Off,
        Some(mb) => Cap::Fixed(mb.saturating_mul(MIB)),
        None => Cap::Dynamic,
    }
}

/// Commit kept free for everything that isn't a pane: a tenth of the commit
/// limit, and never less than [`MIN_RESERVE`].
pub fn reserve(commit_limit: u64) -> u64 {
    (commit_limit / 10).max(MIN_RESERVE)
}

/// The job memory limit to set, or `None` for no limit. `job_used` is the panes'
/// committed memory now; `commit_limit` / `commit_available` are the machine's.
pub fn job_limit(cap: Cap, job_used: u64, commit_limit: u64, commit_available: u64) -> Option<u64> {
    // The floor is on the dynamic term only: it stops the machine-derived limit
    // collapsing to 0, while an operator's explicit cap is honoured as given.
    let dynamic = job_used
        .saturating_add(commit_available.saturating_sub(reserve(commit_limit)))
        .max(MIN_LIMIT);
    match cap {
        Cap::Off => None,
        Cap::Dynamic => Some(dynamic),
        Cap::Fixed(bytes) => Some(bytes.min(dynamic)),
    }
}

/// Whether a process may be stopped to relieve pressure: anything a build runs
/// — cargo and its compilers ([`crate::buildpool::is_build_image`]) plus the
/// linkers and C/C++ compilers they drive, since the link step is often the
/// single largest process in a build. Never an agent, a shell or a tool.
pub fn is_victim_image(name: &str) -> bool {
    if crate::buildpool::is_build_image(name) {
        return true;
    }
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let lower = file.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    matches!(
        stem,
        "link"
            | "lld-link"
            | "rust-lld"
            | "ld"
            | "ld.lld"
            | "ld64.lld"
            | "mold"
            | "cl"
            | "cc"
            | "c++"
            | "gcc"
            | "g++"
            | "clang"
            | "clang++"
    )
}

/// Whether `used` has reached [`PRESSURE_PERCENT`] of `limit`.
pub fn under_pressure(used: u64, limit: u64) -> bool {
    used.saturating_mul(100) >= limit.saturating_mul(PRESSURE_PERCENT)
}

/// One process the guard is watching: a session-job member (Windows) or a
/// process in one of the panes' sessions (unix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub pid: u32,
    /// Private committed bytes (Windows) or resident bytes (unix).
    pub private: u64,
    /// Image path or name.
    pub image: String,
    /// The process's start time (unix), which binds a later kill to this exact
    /// process; 0 on Windows, where the kill is bound by a handle instead.
    pub start: u64,
}

/// The process to stop under pressure: the largest build process
/// ([`is_victim_image`]). `None` when no build is running — agents and shells
/// are never chosen.
pub fn victim(members: &[Member]) -> Option<&Member> {
    members
        .iter()
        .filter(|m| is_victim_image(&m.image))
        .max_by_key(|m| m.private)
}

/// A human-readable byte count, e.g. `2.1 GiB`.
pub fn show_bytes(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{} MiB", bytes / MIB)
    }
}

/// How firmly this platform can hold the ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// A kernel limit the panes cannot exceed (Windows job memory limit).
    Hard,
    /// atrium watches and stops builds, but nothing stops an allocation
    /// between ticks (Linux, macOS).
    Soft,
    /// No guard at all.
    None,
}

/// This platform's [`Enforcement`].
pub fn enforcement() -> Enforcement {
    if cfg!(windows) {
        Enforcement::Hard
    } else if cfg!(any(target_os = "linux", target_os = "macos")) {
        Enforcement::Soft
    } else {
        Enforcement::None
    }
}

/// The cap as the fleet banner states it, e.g. `memory guard on`. A soft guard
/// says so, and an unsupported platform says there is none, rather than
/// claiming a ceiling that isn't there.
pub fn describe(cap: Cap, enforcement: Enforcement) -> String {
    let soft = if enforcement == Enforcement::Soft {
        " (soft)"
    } else {
        ""
    };
    match (enforcement, cap) {
        (Enforcement::None, _) => "no memory guard on this platform".to_string(),
        (_, Cap::Off) => "memory guard OFF".to_string(),
        (_, Cap::Dynamic) => format!("memory guard on{soft}"),
        (_, Cap::Fixed(bytes)) => format!("memory capped at {}{soft}", show_bytes(bytes)),
    }
}

/// Soft pressure (unix): the machine has dropped into its reserve, or — with a
/// fixed cap — the panes have reached [`PRESSURE_PERCENT`] of it. There is no
/// kernel ceiling to track, so this is the whole decision.
pub fn soft_pressure(cap: Cap, panes_used: u64, mem_total: u64, mem_available: u64) -> bool {
    let machine = mem_available < reserve(mem_total);
    match cap {
        Cap::Off => false,
        Cap::Dynamic => machine,
        Cap::Fixed(bytes) => machine || under_pressure(panes_used, bytes),
    }
}

/// The `oom_score_adj` a build process gets on Linux, so that if the kernel's
/// OOM killer does fire it takes a build before atrium, the terminal or the
/// desktop. Raising the score is allowed without privilege; lowering is not.
pub const BUILD_OOM_SCORE_ADJ: i32 = 800;

/// `(session id, start time, rss pages, comm)` from a Linux `/proc/<pid>/stat`
/// line. `comm` sits in parentheses and may itself contain spaces and `)`, so
/// fields are counted from the LAST `)`. Pure, so it is tested everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat(text: &str) -> Option<(u32, u64, u64, String)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let f: Vec<&str> = text[close + 1..].split_whitespace().collect();
    // `f[0]` is field 3 (state), so field N of proc(5) is `f[N - 3]`.
    let session = f.get(3)?.parse().ok()?; // field 6
    let start = f.get(19)?.parse().ok()?; // field 22
    let rss_pages = f.get(21)?.parse().ok()?; // field 24
    Some((session, start, rss_pages, comm))
}

/// The cap a fleet with `memory_mb` would run under, before it is recorded.
pub fn planned_cap(fleet_mb: Option<u64>) -> Cap {
    parse_cap(std::env::var(ENV_MEMORY_MB).ok().as_deref(), fleet_mb)
}

/// A fleet file's `memory_mb`, set once before the session starts.
static FLEET_MB: OnceLock<u64> = OnceLock::new();

/// Record a fleet's `memory_mb` for the session guard. First call wins.
pub fn set_fleet_mb(mb: u64) {
    let _ = FLEET_MB.set(mb);
}

/// This session's cap, from the environment and the fleet file.
pub fn session_cap() -> Cap {
    if !supported() {
        return Cap::Off;
    }
    parse_cap(
        std::env::var(ENV_MEMORY_MB).ok().as_deref(),
        FLEET_MB.get().copied(),
    )
}

/// Whether this platform has any guard, hard or soft.
pub fn supported() -> bool {
    enforcement() != Enforcement::None
}

/// The guard's memory between ticks.
#[derive(Default)]
pub struct Guard {
    /// The limit last set on the job, so an unchanged limit is not re-set.
    #[cfg_attr(unix, allow(dead_code))]
    applied: Option<Option<u64>>,
    /// Pressure with nothing to stop has been reported this streak.
    stuck_reported: bool,
}

impl Guard {
    pub fn new() -> Guard {
        Guard::default()
    }

    /// Recompute and apply the limit, and relieve pressure. `panes` are the live
    /// pane pids (unix finds each pane's processes by its session). Returns a
    /// message for the operator when the guard acted or can't; `None` when all is
    /// well.
    pub fn tick(&mut self, job: &crate::reap::SessionJob, panes: &[u32]) -> Option<String> {
        let cap = session_cap();
        #[cfg(unix)]
        {
            let _ = job;
            self.tick_soft(cap, panes)
        }
        #[cfg(not(unix))]
        {
            let _ = panes;
            self.tick_hard(cap, job)
        }
    }

    /// Unix: no kernel ceiling, so watch and act. Every tick, builds in the
    /// panes are marked as the OOM killer's preferred victims (Linux); under
    /// [`soft_pressure`] the largest build is stopped, bound to its start time.
    #[cfg(unix)]
    fn tick_soft(&mut self, cap: Cap, panes: &[u32]) -> Option<String> {
        if cap == Cap::Off {
            return None;
        }
        let procs = unix::pane_processes(panes)?;
        for p in procs.iter().filter(|p| is_victim_image(&p.image)) {
            unix::prefer_for_oom(p.pid);
        }
        let (total, available) = unix::memory()?;
        let used: u64 = procs.iter().map(|m| m.private).sum();
        if !soft_pressure(cap, used, total, available) {
            self.stuck_reported = false;
            return None;
        }
        let why = match cap {
            Cap::Fixed(bytes) if under_pressure(used, bytes) => format!(
                "panes at {} of their {} cap",
                show_bytes(used),
                show_bytes(bytes)
            ),
            _ => format!(
                "machine down to {} available of {}",
                show_bytes(available),
                show_bytes(total)
            ),
        };
        match victim(&procs) {
            Some(v) if unix::kill_bound(v.pid, v.start) => {
                self.stuck_reported = false;
                Some(format!(
                    "memory guard: stopped {} (pid {}, {}) — {why}",
                    image_file(&v.image),
                    v.pid,
                    show_bytes(v.private)
                ))
            }
            _ if !self.stuck_reported => {
                self.stuck_reported = true;
                Some(format!("memory guard: {why} and no build to stop"))
            }
            _ => None,
        }
    }

    /// Windows: keep the session job's memory limit current, and stop the
    /// largest build under pressure through a handle-bound kill.
    #[cfg(not(unix))]
    fn tick_hard(&mut self, cap: Cap, job: &crate::reap::SessionJob) -> Option<String> {
        if cap == Cap::Off {
            if self.applied != Some(None) && job.set_memory_limit(None) {
                self.applied = Some(None);
            }
            return None;
        }
        let members = members(job)?;
        let (commit_limit, commit_available) = crate::resources::system_commit()?;
        let used: u64 = members.iter().map(|m| m.private).sum();
        let limit = job_limit(cap, used, commit_limit, commit_available)?;
        // Re-apply only on a real change (1% or 64 MiB), not on every tick's
        // jitter; a tightening always applies.
        let stale = match self.applied {
            Some(Some(prev)) => limit < prev || limit.abs_diff(prev) > (prev / 100).max(64 * MIB),
            _ => true,
        };
        if stale && job.set_memory_limit(Some(limit)) {
            self.applied = Some(Some(limit));
        }
        if !under_pressure(used, limit) {
            self.stuck_reported = false;
            return None;
        }
        // The kill re-checks membership and the build image through the handle it
        // kills with, so a victim that exited and had its pid recycled is safe.
        match victim(&members) {
            // Same predicate the victim was chosen by: a narrower one here would
            // pick a linker and then refuse to stop it.
            Some(v) if job.terminate_member(v.pid, &is_victim_image) => {
                self.stuck_reported = false;
                Some(format!(
                    "memory guard: stopped {} (pid {}, {}) — panes at {} of their {} limit",
                    image_file(&v.image),
                    v.pid,
                    show_bytes(v.private),
                    show_bytes(used),
                    show_bytes(limit)
                ))
            }
            _ if !self.stuck_reported => {
                self.stuck_reported = true;
                Some(format!(
                    "memory guard: panes at {} of their {} limit and no build to stop — \
                     agents may start failing allocations",
                    show_bytes(used),
                    show_bytes(limit)
                ))
            }
            _ => None,
        }
    }
}

/// The last path component of an image path.
fn image_file(image: &str) -> &str {
    image.rsplit(['/', '\\']).next().unwrap_or(image)
}

/// Every process in the job with its private commit and image. `None` if the
/// job can't be listed.
#[cfg(not(unix))]
fn members(job: &crate::reap::SessionJob) -> Option<Vec<Member>> {
    Some(
        job.pids()?
            .into_iter()
            .filter_map(|pid| {
                Some(Member {
                    pid,
                    private: crate::reap::private_bytes(pid)?,
                    image: crate::reap::image_name(pid).unwrap_or_default(),
                    start: 0,
                })
            })
            .collect(),
    )
}

/// The unix backend: a process scan by pane session (every pane is a session
/// leader — `pty` runs `setsid()` — so its session id is its pid and everything
/// it starts inherits it), the machine's available memory, and a kill bound to
/// the victim's start time. Linux is run by the test suite; macOS is compiled
/// for its targets but UNTESTED at runtime.
#[cfg(unix)]
mod unix {
    use super::Member;
    #[allow(unused_imports)]
    use core::ffi::{c_long, c_void};

    /// Linux: every process whose session is one of the panes'.
    #[cfg(target_os = "linux")]
    pub fn pane_processes(panes: &[u32]) -> Option<Vec<Member>> {
        if panes.is_empty() {
            return Some(Vec::new());
        }
        let page = page_size();
        let mut out = Vec::new();
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            // A process can exit between the listing and the read: skip it.
            let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let Some((session, start, rss_pages, comm)) = super::parse_stat(&text) else {
                continue;
            };
            if panes.contains(&session) {
                out.push(Member {
                    pid,
                    private: rss_pages.saturating_mul(page),
                    image: comm,
                    start,
                });
            }
        }
        Some(out)
    }

    #[cfg(target_os = "linux")]
    fn page_size() -> u64 {
        extern "C" {
            fn sysconf(name: i32) -> c_long;
        }
        /// `_SC_PAGESIZE` on Linux (glibc and musl).
        const SC_PAGESIZE: i32 = 30;
        // SAFETY: a pure query with no pointers.
        let n = unsafe { sysconf(SC_PAGESIZE) };
        if n > 0 {
            n as u64
        } else {
            4096
        }
    }

    /// Linux: `(MemTotal, MemAvailable)` in bytes from `/proc/meminfo`.
    #[cfg(target_os = "linux")]
    pub fn memory() -> Option<(u64, u64)> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb = |key: &str| -> Option<u64> {
            let line = text.lines().find(|l| l.starts_with(key))?;
            line[key.len()..]
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        };
        Some((kb("MemTotal:")? * 1024, kb("MemAvailable:")? * 1024))
    }

    /// Linux: make `pid` a preferred victim of the kernel's OOM killer, so a
    /// build dies before atrium, the terminal or the desktop. Raising the score
    /// needs no privilege; a failed write (the process just exited) is ignored.
    #[cfg(target_os = "linux")]
    pub fn prefer_for_oom(pid: u32) {
        let path = format!("/proc/{pid}/oom_score_adj");
        let current: i32 = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(i32::MAX);
        if current < super::BUILD_OOM_SCORE_ADJ {
            let _ = std::fs::write(&path, super::BUILD_OOM_SCORE_ADJ.to_string());
        }
    }

    /// Linux: SIGKILL `pid` only while it is still the process that started at
    /// `start`. A pidfd pins the process it was opened on, so once the start
    /// time checks out through it, the signal can only reach that process; if it
    /// exited first, the send fails instead of hitting whoever reused the pid. A
    /// kernel without pidfds (before 5.3) falls back to check-then-kill, which
    /// leaves a narrow reuse race.
    #[cfg(target_os = "linux")]
    pub fn kill_bound(pid: u32, start: u64) -> bool {
        extern "C" {
            // Variadic in C; it must be declared variadic here too.
            fn syscall(num: c_long, ...) -> c_long;
            fn close(fd: i32) -> i32;
            fn kill(pid: i32, sig: i32) -> i32;
        }
        // Unified syscall numbers (asm-generic), the same on x86_64 and aarch64.
        const SYS_PIDFD_SEND_SIGNAL: c_long = 424;
        const SYS_PIDFD_OPEN: c_long = 434;
        const SIGKILL: i32 = 9;
        if pid == 0 {
            return false;
        }
        // SAFETY: pidfd_open(pid, flags = 0) takes no pointers.
        let fd = unsafe { syscall(SYS_PIDFD_OPEN, pid as c_long, 0 as c_long) };
        if fd < 0 {
            return crate::orphan::start_token(pid) == Some(start)
                // SAFETY: a plain signal send.
                && unsafe { kill(pid as i32, SIGKILL) } == 0;
        }
        let fd = fd as i32;
        let ok = crate::orphan::start_token(pid) == Some(start)
            // SAFETY: pidfd_send_signal(fd, sig, info = NULL, flags = 0).
            && unsafe {
                syscall(
                    SYS_PIDFD_SEND_SIGNAL,
                    fd as c_long,
                    SIGKILL as c_long,
                    core::ptr::null::<c_void>(),
                    0 as c_long,
                )
            } == 0;
        // SAFETY: closing the pidfd opened above.
        unsafe { close(fd) };
        ok
    }

    /// macOS: every process whose session is one of the panes'.
    #[cfg(target_os = "macos")]
    pub fn pane_processes(panes: &[u32]) -> Option<Vec<Member>> {
        extern "C" {
            fn proc_listallpids(buffer: *mut c_void, buffersize: i32) -> i32;
            fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, size: i32)
                -> i32;
            fn proc_name(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
            fn getsid(pid: i32) -> i32;
        }
        /// `PROC_PIDTASKINFO` from `<sys/proc_info.h>`.
        const PROC_PIDTASKINFO: i32 = 4;
        /// `struct proc_taskinfo`, transcribed from `<sys/proc_info.h>`.
        #[repr(C)]
        struct ProcTaskInfo {
            virtual_size: u64,
            resident_size: u64,
            total_user: u64,
            total_system: u64,
            threads_user: u64,
            threads_system: u64,
            policy: i32,
            faults: i32,
            pageins: i32,
            cow_faults: i32,
            messages_sent: i32,
            messages_received: i32,
            syscalls_mach: i32,
            syscalls_unix: i32,
            csw: i32,
            threadnum: i32,
            numrunning: i32,
            priority: i32,
        }
        if panes.is_empty() {
            return Some(Vec::new());
        }
        let mut pids = vec![0i32; 8192];
        // SAFETY: the buffer is exactly `buffersize` bytes.
        let n = unsafe {
            proc_listallpids(
                pids.as_mut_ptr() as *mut c_void,
                (pids.len() * core::mem::size_of::<i32>()) as i32,
            )
        };
        if n <= 0 {
            return None;
        }
        let mut out = Vec::new();
        for &pid in &pids[..(n as usize).min(pids.len())] {
            // SAFETY: a read-only query on a pid.
            let sid = unsafe { getsid(pid) };
            if sid < 0 || !panes.contains(&(sid as u32)) {
                continue;
            }
            // SAFETY: all-integer struct, so zeroed is valid; the call writes at
            // most `size` bytes and a short write is rejected below.
            let mut info: ProcTaskInfo = unsafe { core::mem::zeroed() };
            let size = core::mem::size_of::<ProcTaskInfo>() as i32;
            let got = unsafe {
                proc_pidinfo(
                    pid,
                    PROC_PIDTASKINFO,
                    0,
                    &mut info as *mut ProcTaskInfo as *mut c_void,
                    size,
                )
            };
            if got != size {
                continue;
            }
            let Some(start) = crate::orphan::start_token(pid as u32) else {
                continue;
            };
            let mut name = [0u8; 256];
            // SAFETY: `name` is 256 bytes, the size passed.
            let len =
                unsafe { proc_name(pid, name.as_mut_ptr() as *mut c_void, name.len() as u32) };
            out.push(Member {
                pid: pid as u32,
                private: info.resident_size,
                image: String::from_utf8_lossy(&name[..len.max(0) as usize]).into_owned(),
                start,
            });
        }
        Some(out)
    }

    /// macOS: `(hw.memsize, the free share of it)`. `kern.memorystatus_level`
    /// is the percentage of memory the kernel considers available — the same
    /// signal its own memory-pressure handling uses.
    #[cfg(target_os = "macos")]
    pub fn memory() -> Option<(u64, u64)> {
        extern "C" {
            fn sysctlbyname(
                name: *const core::ffi::c_char,
                oldp: *mut c_void,
                oldlenp: *mut usize,
                newp: *mut c_void,
                newlen: usize,
            ) -> i32;
        }
        let mut total: u64 = 0;
        let mut len = core::mem::size_of::<u64>();
        // SAFETY: null-terminated name; `total`/`len` describe a u64 buffer.
        let ok = unsafe {
            sysctlbyname(
                b"hw.memsize\0".as_ptr() as *const core::ffi::c_char,
                &mut total as *mut u64 as *mut c_void,
                &mut len,
                core::ptr::null_mut(),
                0,
            )
        } == 0;
        let mut level: i32 = 0;
        let mut len2 = core::mem::size_of::<i32>();
        // SAFETY: as above, for an int.
        let ok2 = unsafe {
            sysctlbyname(
                b"kern.memorystatus_level\0".as_ptr() as *const core::ffi::c_char,
                &mut level as *mut i32 as *mut c_void,
                &mut len2,
                core::ptr::null_mut(),
                0,
            )
        } == 0;
        (ok && ok2 && total > 0).then(|| (total, total / 100 * level.clamp(0, 100) as u64))
    }

    /// macOS has no OOM-score knob; its pressure handling is the kernel's own.
    #[cfg(target_os = "macos")]
    pub fn prefer_for_oom(_pid: u32) {}

    /// macOS: SIGKILL `pid` only if its start time still matches. No pidfd, so
    /// a narrow check-then-kill reuse race remains.
    #[cfg(target_os = "macos")]
    pub fn kill_bound(pid: u32, start: u64) -> bool {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        pid != 0
            && crate::orphan::start_token(pid) == Some(start)
            // SAFETY: a plain signal send.
            && unsafe { kill(pid as i32, 9) } == 0
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn pane_processes(_panes: &[u32]) -> Option<Vec<Member>> {
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn memory() -> Option<(u64, u64)> {
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn prefer_for_oom(_pid: u32) {}
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn kill_bound(_pid: u32, _start: u64) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_wins_and_zero_or_off_disables() {
        assert_eq!(parse_cap(Some("2048"), Some(512)), Cap::Fixed(2048 * MIB));
        assert_eq!(parse_cap(Some("off"), Some(512)), Cap::Off);
        assert_eq!(parse_cap(Some("0"), None), Cap::Off);
        assert_eq!(parse_cap(None, Some(0)), Cap::Off);
        assert_eq!(parse_cap(None, Some(512)), Cap::Fixed(512 * MIB));
        assert_eq!(parse_cap(None, None), Cap::Dynamic);
        // A typo keeps the ceiling rather than removing it.
        assert_eq!(parse_cap(Some("lots"), None), Cap::Dynamic);
    }

    #[test]
    fn the_reserve_is_a_tenth_with_a_floor() {
        assert_eq!(reserve(69 * GIB), 69 * GIB / 10);
        assert_eq!(reserve(8 * GIB), MIN_RESERVE);
    }

    #[test]
    fn the_dynamic_limit_leaves_the_reserve_free() {
        // The machine from the OOM: 69 GiB commit limit, 16 GiB committed, panes
        // using 3 GiB. Panes may grow into free commit, minus a 6.9 GiB reserve.
        let limit = job_limit(Cap::Dynamic, 3 * GIB, 69 * GIB, 53 * GIB).unwrap();
        assert_eq!(limit, 3 * GIB + 53 * GIB - 69 * GIB / 10);
        // The rest of the machine grows: the panes' ceiling shrinks with it.
        let tighter = job_limit(Cap::Dynamic, 3 * GIB, 69 * GIB, 20 * GIB).unwrap();
        assert!(tighter < limit);
        // Already inside the reserve: no room to grow at all.
        assert_eq!(
            job_limit(Cap::Dynamic, 3 * GIB, 69 * GIB, GIB),
            Some(3 * GIB)
        );
    }

    /// Review #1: an empty job on a machine already inside its reserve used to
    /// compute a limit of 0, so the next pane couldn't commit a byte. The floor
    /// keeps room to start panes without letting a busy session creep upward.
    #[test]
    fn the_limit_never_collapses_below_the_floor() {
        assert_eq!(job_limit(Cap::Dynamic, 0, 69 * GIB, GIB), Some(MIN_LIMIT));
        // A fixed cap on a machine inside its reserve is bounded by the floored
        // dynamic limit, not by 0.
        assert_eq!(
            job_limit(Cap::Fixed(8 * GIB), 0, 69 * GIB, GIB),
            Some(MIN_LIMIT)
        );
        // An operator's explicit cap below the floor is honoured as given.
        assert_eq!(
            job_limit(Cap::Fixed(64 * MIB), 0, 69 * GIB, 50 * GIB),
            Some(64 * MIB)
        );
        // Above the floor, pressure still pins the limit to what's in use: no
        // per-tick headroom for a busy session to grow into.
        assert_eq!(
            job_limit(Cap::Dynamic, 20 * GIB, 69 * GIB, GIB),
            Some(20 * GIB)
        );
        assert!(!under_pressure(0, MIN_LIMIT));
    }

    /// Review #2: the link step is often the largest process in a build, and a
    /// guard that can't pick it does nothing exactly when a link OOMs.
    #[test]
    fn linkers_and_c_compilers_can_be_the_victim() {
        for image in [
            r"C:\VS\link.exe",
            "lld-link.exe",
            "rust-lld",
            "ld",
            "ld.lld",
            "mold",
            "cl.exe",
            "cc",
            "gcc",
            "g++",
            "clang",
            "clang++.exe",
            "rustc.exe",
        ] {
            assert!(is_victim_image(image), "{image}");
        }
        for image in [
            "claude.exe",
            "node",
            "powershell.exe",
            "cmd.exe",
            "linker-notes",
        ] {
            assert!(!is_victim_image(image), "{image}");
        }
        let members = vec![
            Member {
                pid: 1,
                private: 3 * GIB,
                image: r"C:\rust\rustc.exe".into(),
                start: 0,
            },
            Member {
                pid: 2,
                private: 6 * GIB,
                image: r"C:\VS\link.exe".into(),
                start: 0,
            },
        ];
        assert_eq!(victim(&members).map(|v| v.pid), Some(2));
    }

    #[test]
    fn a_fixed_cap_never_exceeds_the_dynamic_limit() {
        assert_eq!(
            job_limit(Cap::Fixed(8 * GIB), GIB, 69 * GIB, 53 * GIB),
            Some(8 * GIB)
        );
        let dynamic = job_limit(Cap::Dynamic, GIB, 69 * GIB, 8 * GIB).unwrap();
        assert_eq!(
            job_limit(Cap::Fixed(32 * GIB), GIB, 69 * GIB, 8 * GIB),
            Some(dynamic)
        );
        assert_eq!(job_limit(Cap::Off, GIB, 69 * GIB, 53 * GIB), None);
    }

    #[test]
    fn pressure_starts_at_ninety_percent() {
        assert!(!under_pressure(89, 100));
        assert!(under_pressure(90, 100));
        assert!(under_pressure(150, 100));
        assert!(under_pressure(0, 0));
    }

    #[test]
    fn only_a_build_is_ever_the_victim() {
        let m = |pid, gib: u64, image: &str| Member {
            pid,
            private: gib * GIB,
            image: image.to_string(),
            start: 0,
        };
        let members = vec![
            m(1, 9, r"C:\npm\claude.exe"),
            m(2, 2, r"C:\rust\rustc.exe"),
            m(3, 4, r"C:\rust\rustc.exe"),
            m(4, 1, r"C:\t\build-script-build.exe"),
        ];
        // The biggest process is the agent; the biggest BUILD is chosen.
        assert_eq!(victim(&members).map(|v| v.pid), Some(3));
        assert_eq!(victim(&members[..1]), None);
    }

    #[test]
    fn the_banner_never_claims_a_guard_that_is_not_there() {
        use Enforcement::*;
        assert_eq!(describe(Cap::Dynamic, Hard), "memory guard on");
        assert_eq!(
            describe(Cap::Fixed(32 * GIB), Hard),
            "memory capped at 32.0 GiB"
        );
        assert_eq!(describe(Cap::Off, Hard), "memory guard OFF");
        // A soft guard must say it is soft.
        assert_eq!(describe(Cap::Dynamic, Soft), "memory guard on (soft)");
        assert_eq!(
            describe(Cap::Fixed(GIB), Soft),
            "memory capped at 1.0 GiB (soft)"
        );
        for cap in [Cap::Dynamic, Cap::Fixed(GIB), Cap::Off] {
            assert_eq!(describe(cap, None), "no memory guard on this platform");
        }
    }

    /// Linux, live: a build in a pane's session is found by session, marked as
    /// the OOM killer's preferred victim, and stopped only when the kill is bound
    /// to its real start time.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_panes_build_is_found_marked_and_stopped_only_when_bound() {
        use std::os::unix::process::CommandExt;
        let dir = std::env::temp_dir().join(format!("atrium-memguard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("rustc");
        std::os::unix::fs::symlink("/bin/sleep", &fake).unwrap();
        let mut cmd = std::process::Command::new(&fake);
        cmd.arg("30");
        // Its own session, as every pane is: the session id is its pid.
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            cmd.pre_exec(|| {
                extern "C" {
                    fn setsid() -> i32;
                }
                if setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn fake rustc");
        let pid = child.id();
        std::thread::sleep(std::time::Duration::from_millis(200));

        let procs = unix::pane_processes(&[pid]).expect("scan");
        let found = procs.iter().find(|m| m.pid == pid).cloned();
        let foreign = unix::pane_processes(&[pid + 1_000_000]).expect("scan");
        unix::prefer_for_oom(pid);
        let adj = std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj")).unwrap_or_default();
        let start = found.as_ref().map(|m| m.start).unwrap_or(0);
        let wrong = unix::kill_bound(pid, start + 1);
        let alive_after_wrong = child.try_wait().ok().flatten().is_none();
        let right = unix::kill_bound(pid, start);
        let status = child.wait().ok();
        let _ = std::fs::remove_dir_all(&dir);

        let found = found.expect("a process in the pane's session must be found");
        assert_eq!(found.image, "rustc");
        assert!(found.private > 0, "resident size must be read");
        assert!(
            !foreign.iter().any(|m| m.pid == pid),
            "another session's scan must not see it"
        );
        assert_eq!(adj.trim(), BUILD_OOM_SCORE_ADJ.to_string());
        assert!(
            !wrong && alive_after_wrong,
            "a mismatched start time must not kill"
        );
        assert!(right, "the bound kill must land");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.and_then(|s| s.signal()), Some(9));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix_memory_reads_a_plausible_budget() {
        let (total, available) = unix::memory().expect("memory");
        assert!(
            total > GIB / 4 && available <= total,
            "{total} / {available}"
        );
    }

    #[test]
    fn soft_pressure_is_the_reserve_or_the_fixed_cap() {
        // 32 GiB machine: reserve is 3.2 GiB.
        let total = 32 * GIB;
        assert!(!soft_pressure(Cap::Dynamic, 20 * GIB, total, 8 * GIB));
        assert!(
            soft_pressure(Cap::Dynamic, 0, total, 3 * GIB),
            "inside the reserve"
        );
        // A fixed cap adds its own trigger at 90%.
        assert!(soft_pressure(
            Cap::Fixed(10 * GIB),
            9 * GIB,
            total,
            20 * GIB
        ));
        assert!(!soft_pressure(
            Cap::Fixed(10 * GIB),
            8 * GIB,
            total,
            20 * GIB
        ));
        assert!(
            soft_pressure(Cap::Fixed(10 * GIB), 0, total, GIB),
            "reserve still applies"
        );
        assert!(!soft_pressure(Cap::Off, 99 * GIB, total, 0));
    }

    #[test]
    fn a_stat_line_parses_even_when_comm_has_spaces_and_parens() {
        // Real shape of /proc/<pid>/stat: pid (comm) state ppid pgrp session ...
        let mut fields: Vec<String> = (3..=52).map(|n| n.to_string()).collect();
        fields[0] = "S".into(); // field 3
        fields[3] = "4242".into(); // field 6: session
        fields[19] = "987654".into(); // field 22: starttime
        fields[21] = "2560".into(); // field 24: rss pages
        let line = format!("1234 (evil) (rustc x) {}", fields.join(" "));
        assert_eq!(
            parse_stat(&line),
            Some((4242, 987654, 2560, "evil) (rustc x".to_string()))
        );
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn bytes_read_naturally() {
        assert_eq!(show_bytes(512 * MIB), "512 MiB");
        assert_eq!(show_bytes(GIB + GIB / 10), "1.1 GiB");
    }
}
