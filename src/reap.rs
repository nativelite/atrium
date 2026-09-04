//! Process-**tree** teardown: kill what a pane spawned, not just the pane.
//!
//! [`pty::Pty::kill`] terminates the direct child only. An agent CLI spawns its
//! own subprocesses — MCP servers, language servers, node helpers — and those
//! survive, reparented onto `init`, running and billing. This is not a
//! signal-handling edge case: it happens on a clean `Ctrl+A q`, and a fleet
//! review measured 23 leaked agent processes holding 4.6 GB.
//!
//! Unix has no "kill this tree" primitive, but `pty` runs `setsid()` in
//! `pre_exec`, so every pane child is a session leader whose **process-group id
//! equals its pid**. One `killpg` therefore reaches the whole subtree.
//!
//! Two deliberate policy choices live here rather than in `pty`:
//!
//! - **`SIGTERM` before `SIGKILL`.** Today's bare `SIGKILL` is uncatchable, so
//!   it *guarantees* orphans: the agent never gets the chance to reap its own
//!   children. Terming first lets a well-behaved one tear its tree down itself.
//! - **Signal every group, then wait once.** A per-pane grace period would make
//!   teardown O(panes) in wall clock — ten panes at 750 ms is a seven-second
//!   quit. Signalling all of them before the single wait keeps it O(1).
//!
//! The residual gap, stated plainly: a grandchild that calls `setsid()` itself
//! leaves the group and this cannot reach it. macOS offers no container to catch
//! that (no cgroups, no Job Objects); the backstop is the registry sweep.

use std::time::Duration;

/// How long a pane's tree gets to exit on `SIGTERM` before `SIGKILL`. Long
/// enough for an agent to reap its children, short enough not to feel like a
/// hang on the way out.
pub const GRACE: Duration = Duration::from_millis(750);

/// Ask the process group led by `pid` to exit. Returns whether the signal was
/// delivered — `false` means the group is already gone, which is success.
pub fn term_tree(pid: u32) -> bool {
    sys::signal_group(pid, sys::SIGTERM)
}

/// Force the process group led by `pid` to exit. Call after [`GRACE`].
pub fn kill_tree(pid: u32) -> bool {
    sys::signal_group(pid, sys::SIGKILL)
}

/// Is any process still alive in the group led by `pid`? Signal 0 delivers
/// nothing and only reports reachability.
pub fn tree_alive(pid: u32) -> bool {
    sys::signal_group(pid, 0)
}

#[cfg(unix)]
mod sys {
    pub const SIGKILL: i32 = 9;
    pub const SIGTERM: i32 = 15;

    extern "C" {
        /// `int killpg(pid_t pgrp, int sig)` — signal every process in a group.
        fn killpg(pgrp: i32, sig: i32) -> i32;
    }

    pub fn signal_group(pid: u32, sig: i32) -> bool {
        if pid == 0 {
            return false;
        }
        // SAFETY: a plain signal send; `pid` is a pane child's pid, which is its
        // process-group id because `pty` makes every child a session leader.
        unsafe { killpg(pid as i32, sig) == 0 }
    }

    extern "C" {
        /// `int kill(pid_t pid, int sig)` — signal 0 delivers nothing and only
        /// reports whether the process can be signalled, i.e. whether it exists.
        fn kill(pid: i32, sig: i32) -> i32;
    }

    pub fn pid_alive(pid: u32) -> bool {
        // SAFETY: signal 0 sends nothing; this is a liveness probe.
        pid != 0 && unsafe { kill(pid as i32, 0) == 0 }
    }

    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    /// `SIG_IGN` is `(void *) 1`.
    const SIG_IGN: usize = 1;
    const SIGHUP: i32 = 1;
    const SIGINT: i32 = 2;

    pub fn ignore_hup() {
        // SAFETY: setting a disposition to SIG_IGN.
        unsafe {
            signal(SIGHUP, SIG_IGN);
            signal(SIGINT, SIG_IGN);
        }
    }

    extern "C" {
        fn getppid() -> i32;
    }

    /// Has our parent gone? True once we have been reparented away from it —
    /// which happens at the parent's death, regardless of zombie state.
    pub fn parent_changed(original: u32) -> bool {
        // SAFETY: getppid() takes no arguments and cannot fail.
        let now = unsafe { getppid() } as u32;
        now != original || now == 1
    }
}

#[cfg(not(unix))]
mod sys {
    pub const SIGKILL: i32 = 9;
    pub const SIGTERM: i32 = 15;

    /// Windows has no process groups in the POSIX sense. The equivalent bound on
    /// a tree is a Job Object created at spawn with
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, which the kernel honours however
    /// the parent dies — a stronger guarantee than this module can offer on
    /// unix. Tracked separately; until it lands, Windows teardown is unchanged
    /// (`Pty::kill` on the direct child) and this reports "nothing signalled"
    /// rather than pretending to have torn down a tree.
    pub fn signal_group(_pid: u32, _sig: i32) -> bool {
        false
    }

    /// Liveness via `OpenProcess`; a handle that cannot be opened for query
    /// means the process is gone.
    pub fn pid_alive(pid: u32) -> bool {
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const WAIT_TIMEOUT: u32 = 258;
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
            fn WaitForSingleObject(h: *mut core::ffi::c_void, ms: u32) -> u32;
            fn CloseHandle(h: *mut core::ffi::c_void) -> i32;
        }
        if pid == 0 {
            return false;
        }
        // SAFETY: opening a process for SYNCHRONIZE and closing it again.
        unsafe {
            let h = OpenProcess(SYNCHRONIZE, 0, pid);
            if h.is_null() {
                return false;
            }
            let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
            CloseHandle(h);
            alive
        }
    }

    /// No SIGHUP on Windows; console-close arrives via SetConsoleCtrlHandler,
    /// and the durable answer there is a Job Object. Nothing to do here.
    pub fn ignore_hup() {}

    /// Windows has no `getppid`, and no zombie state to work around: a handle
    /// stops being signalled once the process exits, so liveness is exact.
    pub fn parent_changed(original: u32) -> bool {
        !pid_alive(original)
    }
}

// --- session registry + watchdog -------------------------------------------
//
// Layers above cover an exit amux can *observe*: a quit, or a signal it handles.
// Nothing above survives `SIGKILL`, a panic, or an OOM kill — no handler runs,
// so no teardown happens and the whole tree leaks. The only thing that still
// works then is a second process noticing amux is gone.
//
// That watcher needs to know what to kill, which is the same durable list that
// lets a later run clean up wreckage from a crash: one registry file serves both.
// `amux reap` sweeps registries whose owner is dead.

use std::io;
use std::path::{Path, PathBuf};

/// The hidden argv flag that runs amux as its own watchdog.
pub const WATCHDOG_FLAG: &str = "--reap-watchdog";
/// How often the watchdog checks whether amux is still alive.
const WATCH_POLL: Duration = Duration::from_millis(250);

/// Registry path for a session. Lives in the temp dir, keyed by amux's pid, so a
/// crashed session leaves a file a later `amux reap` can find and act on.
pub fn registry_path(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("amux-session-{pid}.pids"))
}

/// Record this session's pane process groups. Rewritten whenever the set
/// changes, so a watchdog started early still learns about panes spawned later.
pub fn write_registry(path: &Path, pgids: &[u32]) -> io::Result<()> {
    let body: String = pgids.iter().map(|p| format!("{p}\n")).collect();
    std::fs::write(path, body)
}

/// Read a registry, ignoring anything unparseable — a half-written file must not
/// stop the rest of a tree being killed.
pub fn read_registry(path: &Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .map(|t| t.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// Is a process alive? Used to decide whether a session (or its registry) is stale.
pub fn pid_alive(pid: u32) -> bool {
    sys::pid_alive(pid)
}

/// The watchdog loop: wait for `parent` to disappear, then tear down every group
/// the registry names. Runs in a re-executed amux, detached from the terminal so
/// a closed window cannot take it down with the session it is meant to outlive.
pub fn watchdog_main(_parent: u32, registry: &Path) {
    // Wait on a PIPE, not on the parent's liveness.
    //
    // Two earlier attempts were wrong in instructive ways. `kill(parent, 0)`
    // succeeds on a zombie, so a SIGKILLed amux looks alive forever. `getppid()`
    // was meant to dodge that — children are reparented at death — but a zombie
    // amux that nobody has reaped still shows as the parent, so that hung too.
    //
    // A file descriptor cannot lie. amux holds the write end of this pipe (our
    // stdin) and never writes to it; the kernel closes every descriptor when a
    // process terminates, however it terminates. So this read returns EOF
    // exactly when amux is gone — no polling, no liveness heuristic, and no
    // zombie state to reason about.
    let mut buf = [0u8; 1];
    loop {
        match std::io::Read::read(&mut std::io::stdin(), &mut buf) {
            Ok(0) => break,    // EOF — amux is gone
            Ok(_) => continue, // stray byte; not a protocol, just ignore it
            Err(_) => break,
        }
    }
    let pgids = read_registry(registry);
    for p in &pgids {
        term_tree(*p);
    }
    if !pgids.is_empty() {
        std::thread::sleep(GRACE);
    }
    for p in &pgids {
        kill_tree(*p);
    }
    let _ = std::fs::remove_file(registry);
}

/// Kill the trees of every session whose amux is gone, and remove its registry.
/// Returns how many registries were cleaned. This is what recovers a machine
/// after a crash — or after a session that predates the watchdog entirely.
pub fn reap_stale() -> usize {
    let dir = std::env::temp_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut cleaned = 0;
    for e in entries.flatten() {
        let path = e.path();
        let owner = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("amux-session-"))
            .and_then(|n| n.strip_suffix(".pids"))
            .and_then(|n| n.parse::<u32>().ok());
        let Some(owner) = owner else { continue };
        if pid_alive(owner) {
            continue; // a live session owns this one
        }
        for p in read_registry(&path) {
            term_tree(p);
            kill_tree(p);
        }
        let _ = std::fs::remove_file(&path);
        cleaned += 1;
    }
    cleaned
}

/// Re-exec amux as a detached watchdog for this session.
///
/// The watchdog is amux's child, so when amux dies — however it dies — the
/// watchdog is reparented to `init` and keeps running long enough to notice and
/// clean up. It ignores SIGHUP so a closed terminal window cannot take down the
/// very process meant to survive that window.
pub fn spawn_watchdog(registry: &Path) -> io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let child = std::process::Command::new(exe)
        .arg(WATCHDOG_FLAG)
        .arg(std::process::id().to_string())
        .arg(registry)
        // The death signal: amux keeps the write end of this pipe for its whole
        // life and never writes to it. The caller MUST hold the returned Child
        // (and thus its stdin) alive, or the pipe closes immediately and the
        // watchdog fires while amux is still running.
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(child)
}

/// Detach the watchdog from the terminal's fate. Called in watchdog mode only.
pub fn ignore_terminal_signals() {
    sys::ignore_hup();
}
