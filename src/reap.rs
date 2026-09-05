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

    /// A running process, not a corpse. `kill(pid, 0)` succeeds on a zombie, so
    /// process state has to be read to tell them apart. macOS reports an exiting
    /// or zombie process with `Z`, or an `E` flag, in `ps -o stat=`.
    /// Signal one process, not its group — used to put down a stuck watchdog,
    /// which is a lone process and not a pane tree.
    pub fn signal_pid(pid: u32, sig: i32) -> bool {
        pid != 0 && unsafe { kill(pid as i32, sig) == 0 }
    }

    pub fn pid_running(pid: u32) -> bool {
        if !pid_alive(pid) {
            return false;
        }
        match std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
        {
            Ok(o) => {
                let st = String::from_utf8_lossy(&o.stdout);
                let st = st.trim();
                !st.is_empty() && !st.starts_with('Z') && !st.contains('E')
            }
            // Cannot tell: assume running. Never tear down a live session on a
            // guess — a missed sweep is recoverable, a wrong kill is not.
            Err(_) => true,
        }
    }

    pub fn ignore_hup() {
        // SAFETY: setting a disposition to SIG_IGN.
        unsafe {
            signal(SIGHUP, SIG_IGN);
            signal(SIGINT, SIG_IGN);
        }
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

    /// Windows watchdogs are not spawned yet (the Job Object covers this case),
    /// so there is nothing to put down here.
    pub fn signal_pid(_pid: u32, _sig: i32) -> bool {
        false
    }

    /// No zombie state on Windows — a handle stops being signalled once the
    /// process exits — so running and alive are the same question.
    pub fn pid_running(pid: u32) -> bool {
        pid_alive(pid)
    }

    // --- Job Object teardown (kill-on-close) --------------------------------
    //
    // Windows has no process groups, but a Job Object with
    // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is strictly stronger than every unix
    // layer in this module combined: assign each pane process to one job, hold
    // its handle for amux's lifetime, and the kernel terminates every process in
    // the job when the last handle closes — however amux died, including
    // `TerminateProcess`, which nothing can catch. Children of assigned processes
    // inherit the job automatically, so grandchildren are covered without walking.
    //
    // Struct layouts and constants are transcribed from winnt.h. A wrong field
    // order or width here is silent memory corruption, not a compile error — the
    // `kill_on_close_*` test is the definitive check that they are right.

    use core::ffi::c_void;
    type Handle = *mut c_void;

    extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *mut c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(h: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    /// `JobObjectExtendedLimitInformation` information class.
    const JOB_EXTENDED_LIMIT_INFO: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const PROCESS_SET_QUOTA: u32 = 0x0100;
    const PROCESS_TERMINATE: u32 = 0x0001;

    // `usize` is correct for `SIZE_T` / `ULONG_PTR` on both 32- and 64-bit.
    #[repr(C)]
    struct IoCounters {
        read_ops: u64,
        write_ops: u64,
        other_ops: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }

    #[repr(C)]
    struct JobBasicLimitInformation {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct JobExtendedLimitInformation {
        basic: JobBasicLimitInformation,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    /// One-line warning naming the failing call and its error `code`, only under
    /// `AMUX_DEBUG` (a TUI can't print to the screen mid-run without corrupting it).
    /// A job failure degrades the cleanup guarantee; it never blocks a pane (R1).
    /// The caller captures `code` from `GetLastError` **immediately** after the
    /// failing FFI call — before any other Rust runs (e.g. `CloseHandle`, which
    /// would otherwise clobber the thread's last-error).
    fn debug_warn(what: &str, code: u32) {
        if std::env::var_os("AMUX_DEBUG").is_some() {
            eprint!("[amux-dbg job] {what} failed (GetLastError={code})\r\n");
        }
    }

    /// Create a Job Object with kill-on-close set. Returns a **null** handle on any
    /// failure — the caller then runs with the guarantee unavailable, never blocked.
    pub fn create_job() -> Handle {
        // `CreateJobObjectW(null, null)` yields a NON-inheritable handle, which is
        // required: an inherited handle held by a pane would keep the job open past
        // amux and silently defeat kill-on-close (R2).
        // SAFETY: FFI call with null attributes and null name.
        let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if job.is_null() {
            debug_warn("CreateJobObjectW", unsafe { GetLastError() });
            return job;
        }
        // A fully-zeroed extended-limit struct with only the kill-on-close flag.
        // SAFETY: every field is a plain integer, so all-zero is a valid value.
        let mut info: JobExtendedLimitInformation = unsafe { core::mem::zeroed() };
        info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` is a valid JOBOBJECT_EXTENDED_LIMIT_INFORMATION and `len`
        // is exactly its size, as the Win32 contract requires.
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JOB_EXTENDED_LIMIT_INFO,
                &mut info as *mut JobExtendedLimitInformation as *mut c_void,
                core::mem::size_of::<JobExtendedLimitInformation>() as u32,
            )
        };
        if ok == 0 {
            debug_warn("SetInformationJobObject", unsafe { GetLastError() });
            // SAFETY: closing the handle we just created.
            unsafe { CloseHandle(job) };
            return std::ptr::null_mut();
        }
        job
    }

    /// Assign the process `pid` to `job`; its descendants inherit the job. `false`
    /// on failure — notably `ERROR_ACCESS_DENIED` when amux is itself inside a
    /// non-nesting job (R1), which must degrade to a warning, never a refused pane.
    pub fn assign_to_job(job: Handle, pid: u32) -> bool {
        if job.is_null() || pid == 0 {
            return false;
        }
        // SAFETY: open the pane child for exactly the two rights
        // AssignProcessToJobObject needs, assign it, then close our process handle
        // — the JOB holds the process, not this handle.
        unsafe {
            let h = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if h.is_null() {
                debug_warn("OpenProcess", GetLastError());
                return false;
            }
            let ok = AssignProcessToJobObject(job, h) != 0;
            // Capture the error BEFORE CloseHandle, which would clobber last-error.
            let err = if ok { 0 } else { GetLastError() };
            CloseHandle(h);
            if !ok {
                debug_warn("AssignProcessToJobObject", err);
            }
            ok
        }
    }

    /// Close amux's handle to `job`; the kernel then fires kill-on-close.
    pub fn close_job(job: Handle) {
        if !job.is_null() {
            // SAFETY: closing a handle we own; drops amux's reference to the job.
            unsafe { CloseHandle(job) };
        }
    }
}

/// A session-lifetime container that guarantees no pane tree outlives amux.
///
/// On **Windows** it is a Job Object with `KILL_ON_JOB_CLOSE`: every pane assigned
/// to it — and every descendant, which inherit the job automatically — is
/// terminated by the kernel when amux's handle closes, however amux exits (normal
/// quit, panic, or an uncatchable `TerminateProcess`). This is the one death mode
/// no unix layer in this module can match. On **unix** it is a deliberate no-op:
/// the process-group teardown and the pipe-EOF watchdog already cover the tree,
/// and there is no Job Object to use.
///
/// Hold one for amux's whole run and [`assign`](SessionJob::assign) each pane
/// after spawn; dropping it (or the process exiting) fires the guarantee. A
/// Windows failure to create or configure the job yields an inert handle whose
/// `assign` is a no-op — panes still spawn, the guarantee is simply unavailable
/// (R1), never a refused pane.
pub struct SessionJob {
    #[cfg(not(unix))]
    raw: *mut core::ffi::c_void,
}

impl SessionJob {
    /// Create the container: a kill-on-close Job Object on Windows, nothing on unix.
    pub fn create() -> Self {
        #[cfg(unix)]
        {
            SessionJob {}
        }
        #[cfg(not(unix))]
        {
            SessionJob {
                raw: sys::create_job(),
            }
        }
    }

    /// Assign a pane process (by pid) so its whole tree is torn down with amux.
    /// `false` on unix (no-op) and on a Windows assignment failure (kept non-fatal).
    pub fn assign(&self, pid: u32) -> bool {
        #[cfg(unix)]
        {
            let _ = pid;
            false
        }
        #[cfg(not(unix))]
        {
            sys::assign_to_job(self.raw, pid)
        }
    }
}

#[cfg(not(unix))]
impl Drop for SessionJob {
    fn drop(&mut self) {
        // Closing amux's last handle fires KILL_ON_JOB_CLOSE. On a normal return
        // this is the clean path; on TerminateProcess the kernel closes it for us
        // — either way no pane tree is left behind.
        sys::close_job(self.raw);
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

/// Registry path for a session. Lives in the temp dir, keyed by amux's pid, so a
/// crashed session leaves a file a later `amux reap` can find and act on.
pub fn registry_path(pid: u32) -> PathBuf {
    registry_dir().join(format!("amux-session-{pid}.pids"))
}

/// Where session registries live.
///
/// NOT `std::env::temp_dir()`, which reads `$TMPDIR` - a variable the agent owns.
/// A nested amux resolves its PARENT's registry through this path to learn the
/// ceiling it must accept, so an env-derived location meant the ceiling was one
/// variable away from gone: `TMPDIR=/tmp/x amux --trust skip` walked the ancestry
/// correctly, then looked for the parent's registry under the CHILD's TMPDIR,
/// found nothing, and capped nothing. No double-fork required.
///
/// The whole point of moving the ceiling out of the environment onto disk is lost
/// if the PATH to that disk location is itself in the environment. A fixed
/// location cannot be redirected.
#[cfg(unix)]
fn registry_dir() -> PathBuf {
    PathBuf::from("/tmp")
}

/// Windows has no `$TMPDIR` redirection of the same shape, and the Job Object
/// bounds a pane's tree there regardless of what the registry says.
#[cfg(not(unix))]
fn registry_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Record this session's pane process groups. Rewritten whenever the set
/// changes, so a watchdog started early still learns about panes spawned later.
pub fn write_registry(
    path: &Path,
    pgids: &[u32],
    policy: &str,
    capped_by: Option<u32>,
) -> io::Result<()> {
    // The `policy=` line is what a NESTED amux reads to learn the ceiling it must
    // cap itself at. It lives on disk rather than in the environment on purpose:
    // an agent owns its own environment and can `env -u` any marker away, but it
    // would have to overtly edit this file - which the warden watches.
    //
    // `read_registry` parses lines as pids and drops anything unparseable, so this
    // line is invisible to it and older readers are unaffected.
    let mut body = format!("policy={policy}\n");
    // Declaring which session capped us is what lets a parent tell a LEGITIMATE
    // nested amux from one that double-forked to escape. Without it the warden
    // sees only "a descendant appeared" and cannot tell the two apart - which is
    // how enforcement would have killed a nested session that behaved perfectly.
    if let Some(parent) = capped_by {
        body.push_str(&format!("capped_by={parent}\n"));
    }

    for p in pgids {
        body.push_str(&format!("{p}\n"));
    }
    // Write-then-rename. `fs::write` truncates in place, so a crash partway
    // through leaves a half-written final line - and `read_registry` parses
    // tolerantly, so a truncated pid can still parse as a DIFFERENT, valid pid.
    // A later sweep would then killpg an unrelated process group. A rename is
    // atomic, so a reader sees either the old file or the new one.
    let tmp = path.with_extension("pids.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// The session that capped this one, if it declared one.
pub fn read_capped_by(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok().and_then(|t| {
        t.lines().find_map(|l| {
            l.trim()
                .strip_prefix("capped_by=")
                .and_then(|v| v.parse().ok())
        })
    })
}

/// The trust policy a session recorded, if any. Read by a nested amux to find
/// the ceiling it inherits.
pub fn read_policy(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().and_then(|t| {
        t.lines()
            .find_map(|l| l.trim().strip_prefix("policy=").map(str::to_string))
    })
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

/// Is a process actually *running*, as opposed to one that has exited and is
/// waiting to be reaped?
///
/// [`pid_alive`] cannot tell the difference: `kill(pid, 0)` succeeds on a zombie.
/// That is why a crashed session's registry was never swept — the sweep saw the
/// dead owner as alive and skipped it, precisely in the case the sweep exists
/// for. Anything deciding "is this session still here" must use this.
pub fn pid_running(pid: u32) -> bool {
    sys::pid_running(pid)
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
/// Kill watchdogs whose session is gone.
///
/// A watchdog blocked on its pipe exits by itself the moment amux dies, so this
/// should normally find nothing. One still present after its owner has gone is
/// stuck — an older build that polled liveness, or a pipe that never closed —
/// and is pure debris holding a process slot.
fn reap_orphan_watchdogs() -> usize {
    let out = match std::process::Command::new("ps")
        .args(["-eo", "pid=,args="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return 0,
    };
    let mut killed = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if !line.contains(WATCHDOG_FLAG) {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(pid) = it.next().and_then(|t| t.parse::<u32>().ok()) else {
            continue;
        };
        // `… --reap-watchdog <owner> <registry>`: the owner follows the flag.
        let owner = line
            .split_whitespace()
            .skip_while(|t| *t != WATCHDOG_FLAG)
            .nth(1)
            .and_then(|t| t.parse::<u32>().ok());
        let Some(owner) = owner else { continue };
        if pid_running(owner) {
            continue; // its session is still up; leave it alone
        }
        if sys::signal_pid(pid, sys::SIGKILL) {
            killed += 1;
        }
    }
    killed
}

/// Clean up after sessions that are already gone: `(sessions, watchdogs)`.
pub fn reap_stale() -> (usize, usize) {
    let dir = std::env::temp_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // Cannot read the temp dir: no registries to sweep, but a stuck watchdog
        // is found through the process table and is still worth clearing.
        Err(_) => return (0, reap_orphan_watchdogs()),
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
        if pid_running(owner) {
            continue; // a live session owns this one
        }
        // TERM every group, wait once, then KILL - the same policy as
        // `watchdog_main` and the main teardown. Firing both back to back made
        // the SIGTERM decorative on the one path that exists for crash recovery,
        // so nothing ever got the chance to exit cleanly.
        let pgids = read_registry(&path);
        for p in &pgids {
            term_tree(*p);
        }
        if !pgids.is_empty() {
            std::thread::sleep(GRACE);
        }
        for p in &pgids {
            kill_tree(*p);
        }
        let _ = std::fs::remove_file(&path);
        cleaned += 1;
    }
    (cleaned, reap_orphan_watchdogs())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The trap this module hit three times: `kill(pid, 0)` succeeds on a process
    /// that has already exited but not yet been reaped. Anything asking "is this
    /// session still alive" with that alone waits forever for a corpse — which is
    /// how a crashed session's registry survived every sweep.
    #[cfg(unix)]
    #[test]
    fn a_zombie_is_alive_but_not_running() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        // Let it exit, but deliberately do not reap it: it is now a zombie.
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            pid_alive(pid),
            "kill(pid, 0) is expected to succeed on a zombie"
        );
        assert!(
            !pid_running(pid),
            "a zombie must not count as a running session"
        );
        let _ = child.wait(); // reap it
        assert!(!pid_alive(pid), "a reaped process should be gone");
    }

    /// The definitive Windows check (acceptance test #1): assign a long-running
    /// child to a fresh Job Object, close the job's last handle, and the kernel
    /// must terminate the child. This is what proves the `KILL_ON_JOB_CLOSE` flag
    /// and — critically — the transcribed struct layout are correct: a wrong field
    /// order/width is silent memory corruption that only surfaces here, not at
    /// compile time.
    #[cfg(not(unix))]
    #[test]
    fn kill_on_close_terminates_an_assigned_process() {
        // `ping -n 601 127.0.0.1` sleeps ~600s without depending on stdin (unlike
        // `timeout`, which errors when stdin is redirected).
        let mut child = std::process::Command::new("ping")
            .args(["-n", "601", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleeper");
        let pid = child.id();
        {
            let job = SessionJob::create();
            assert!(
                job.assign(pid),
                "assign failed — if ERROR_ACCESS_DENIED, the test runner is inside \
                 a non-nesting job (R1); otherwise a real bug"
            );
            assert!(
                pid_alive(pid),
                "child must be running before the job closes"
            );
            // `job` drops here → its last handle closes → KILL_ON_JOB_CLOSE fires.
        }
        let mut gone = false;
        for _ in 0..50 {
            if !pid_alive(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Belt-and-suspenders so a failed assertion never leaks the sleeper.
        let _ = child.kill();
        let _ = child.wait();
        assert!(gone, "kill-on-close must terminate the assigned process");
    }
}
