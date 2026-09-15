//! One compile pool per atrium session, shared by every pane.
//!
//! A fleet's agents each run their own builds, and cargo sizes each build to the
//! whole machine: seven worktrees running `cargo test --workspace` at once start
//! seven builds of sixteen jobs on a sixteen-core box. That is what exhausted
//! system commit and took atrium down with it. atrium's pane cap can't see it,
//! because the cost is not the agents but what the agents compile.
//!
//! The fix is the one `make` settled decades ago: a **jobserver**. atrium creates
//! one token pool per session and hands its address to every pane in
//! `CARGO_MAKEFLAGS`. cargo (and `cc` inside build scripts) takes a token before
//! starting a compiler job and returns it after, so every build in the session
//! shares one budget, however many agents start one. Measured on Windows with
//! cargo 1.96: a pool of N caps concurrent jobs at N + 1 per cargo invocation
//! (each cargo keeps one implicit token), and neither `-j 16` nor
//! `CARGO_BUILD_JOBS=16` gets past it.
//!
//! The pool is a named semaphore on Windows and a named FIFO on unix — the two
//! transports the `jobserver` crate cargo uses reads from
//! `--jobserver-auth=<name>` / `--jobserver-auth=fifo:<path>`.
//!
//! **Tokens leak.** A client that is killed while holding tokens never returns
//! them (measured: a hard-killed cargo left a 3-token pool at 0). The pool then
//! degrades — every cargo still runs on its implicit token — but never recovers
//! on its own, so [`refill`] tops it back up whenever no build process is running
//! in the session.
//!
//! A session started inside another atrium's pane inherits that pool through its
//! environment and does not create its own, so nesting shares one budget too.

use std::sync::OnceLock;

/// Operator override for the pool size: a job count, or `off`/`0` to disable.
pub const ENV_BUILD_JOBS: &str = "ATRIUM_BUILD_JOBS";
/// The variable cargo reads its jobserver from (before `MAKEFLAGS`/`MFLAGS`).
/// Only this one is set: `MAKEFLAGS` would also switch `make` into parallel mode.
pub const ENV_CARGO_MAKEFLAGS: &str = "CARGO_MAKEFLAGS";

/// The pool size to create, or `None` to run without one.
///
/// `env` is [`ENV_BUILD_JOBS`] and wins outright, as the other resource overrides
/// do — the operator knows their box. `fleet` is a fleet file's `build_jobs`.
/// `off`, `0` or a `0` fleet value disables the pool; an unparseable env value is
/// ignored rather than treated as off, so a typo can't silently remove the limit.
pub fn pool_size(env: Option<&str>, fleet: Option<usize>, default: usize) -> Option<usize> {
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        if v.eq_ignore_ascii_case("off") {
            return None;
        }
        if let Ok(n) = v.parse::<usize>() {
            return (n > 0).then_some(n);
        }
    }
    match fleet {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(default.max(1)),
    }
}

/// The `CARGO_MAKEFLAGS` value that points a client at the pool named `auth`.
/// Both spellings, as the `jobserver` crate itself writes them, so an older
/// client that only knows `--jobserver-fds` still finds it.
pub fn makeflags(auth: &str) -> String {
    format!("-j --jobserver-fds={auth} --jobserver-auth={auth}")
}

/// The jobserver address in a `MAKEFLAGS`-style string, if it names one.
/// The last `--jobserver-auth=` wins, then the last `--jobserver-fds=`, matching
/// the `jobserver` crate; an empty value names nothing.
pub fn jobserver_auth(flags: &str) -> Option<&str> {
    ["--jobserver-auth=", "--jobserver-fds="]
        .iter()
        .find_map(|prefix| {
            flags
                .split_whitespace()
                .filter_map(|arg| arg.strip_prefix(prefix))
                .last()
        })
        .filter(|v| !v.is_empty())
}

/// Whether a process image is one that takes pool tokens: cargo, the compilers
/// it drives, or a build script (which may run `cc` against the pool).
/// Accepts a bare name or a full path, with or without `.exe`, any case. Linux
/// truncates `comm` to 15 bytes, which still leaves `build-script-` intact.
pub fn is_build_image(name: &str) -> bool {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let lower = file.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    matches!(stem, "cargo" | "rustc" | "rustdoc" | "clippy-driver")
        || stem.starts_with("build-script-")
}

/// How many tokens to put back: the shortfall, but only while nothing that could
/// be holding them is running. A token held by a live build is not a leak.
pub fn tokens_to_restore(available: usize, size: usize, build_running: bool) -> usize {
    if build_running {
        0
    } else {
        size.saturating_sub(available)
    }
}

/// The owning pid encoded in a pool FIFO's file name
/// (`atrium-build-<pid>-<nonce>.fifo`), or `None` for any other file. Only the
/// unix sweep uses it; it is compiled everywhere so it is tested everywhere.
#[cfg_attr(not(unix), allow(dead_code))]
fn stale_owner(file_name: &str) -> Option<u32> {
    let rest = file_name
        .strip_prefix("atrium-build-")?
        .strip_suffix(".fifo")?;
    let (pid, nonce) = rest.split_once('-')?;
    if nonce.is_empty() || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    pid.parse().ok()
}

/// A token pool owned by this process.
pub struct BuildPool {
    size: usize,
    auth: String,
    sys: sys::Pool,
}

impl BuildPool {
    /// Create a pool of `size` tokens under a fresh, unguessable name.
    pub fn create(size: usize) -> std::io::Result<BuildPool> {
        let size = size.max(1);
        let nonce = crate::uid::token().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "no OS entropy for a pool name")
        })?;
        let name = format!("atrium-build-{}-{}", std::process::id(), &nonce[..16]);
        let (sys, auth) = sys::Pool::create(&name, size)?;
        Ok(BuildPool { size, auth, sys })
    }

    /// Tokens the pool was created with.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The `CARGO_MAKEFLAGS` value a pane needs to use this pool.
    pub fn makeflags(&self) -> String {
        makeflags(&self.auth)
    }

    /// Tokens not currently held, or `None` if the pool can't be queried.
    pub fn available(&self) -> Option<usize> {
        self.sys.available()
    }

    /// Return up to `n` tokens to the pool, never past its size. Returns how many
    /// went back.
    pub fn restore(&self, n: usize) -> usize {
        (0..n)
            .take_while(|_| self.sys.release_one(self.size))
            .count()
    }

    /// Remove anything that outlives the process (the unix FIFO's path).
    pub fn cleanup(&self) {
        self.sys.cleanup();
    }
}

/// The session's pool: `Some` only when this process created one. `None` when the
/// pool is disabled, when creating it failed (builds then run unpooled, as they
/// did before), or when an enclosing session's pool is already inherited.
static SESSION: OnceLock<Option<BuildPool>> = OnceLock::new();

/// Create the session pool if it doesn't exist yet. `fleet_jobs` is a fleet
/// file's `build_jobs`; the first call wins, so the fleet path calls this before
/// it spawns anything, and every other path gets the default lazily.
pub fn init(fleet_jobs: Option<usize>) -> Option<&'static BuildPool> {
    SESSION
        .get_or_init(|| BuildPool::create(planned_size(fleet_jobs)?).ok())
        .as_ref()
}

/// Whether this process already sits inside an atrium session's pool (its
/// `CARGO_MAKEFLAGS` names a jobserver), in which case it creates none.
pub fn inherited() -> bool {
    std::env::var(ENV_CARGO_MAKEFLAGS).is_ok_and(|f| jobserver_auth(&f).is_some())
}

/// The size [`init`] would create, without creating anything: `None` when the
/// pool is inherited or disabled. Lets the fleet banner state the budget before
/// the operator approves, while the pool itself only exists once they have.
pub fn planned_size(fleet_jobs: Option<usize>) -> Option<usize> {
    if inherited() {
        return None;
    }
    pool_size(
        std::env::var(ENV_BUILD_JOBS).ok().as_deref(),
        fleet_jobs,
        crate::resources::default_build_jobs(),
    )
}

/// The pane environment entry for the session pool, creating it on first use.
pub fn pane_env() -> Option<(String, String)> {
    init(None).map(|p| (ENV_CARGO_MAKEFLAGS.to_string(), p.makeflags()))
}

/// Put back tokens leaked by killed builds. `job` is the session job and `panes`
/// the live pane pids, which must all be in it: a pane outside the job could be
/// building unseen, so an incomplete view restores nothing. Returns how many
/// tokens were restored.
pub fn refill(job: &crate::reap::SessionJob, panes: &[u32]) -> usize {
    let Some(pool) = SESSION.get().and_then(Option::as_ref) else {
        return 0;
    };
    // Read the count BEFORE looking for builds. A build that starts in between is
    // then visible and blocks the refill; one that finishes in between has
    // already returned its tokens, and `restore` stops at the pool size.
    let Some(available) = pool.available() else {
        return 0;
    };
    if available >= pool.size() {
        return 0;
    }
    match sys::build_running(job, panes) {
        Some(running) => pool.restore(tokens_to_restore(available, pool.size(), running)),
        None => 0,
    }
}

/// Remove the session pool's on-disk trace, if it has one. Called at teardown.
pub fn cleanup() {
    if let Some(pool) = SESSION.get().and_then(Option::as_ref) {
        pool.cleanup();
    }
}

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;
    type Handle = *mut c_void;

    const ERROR_ALREADY_EXISTS: u32 = 183;
    /// `SemaphoreBasicInformation` for `NtQuerySemaphore`.
    const SEMAPHORE_BASIC_INFORMATION: i32 = 0;

    #[repr(C)]
    struct SemaphoreBasicInformation {
        current_count: i32,
        maximum_count: i32,
    }

    extern "system" {
        fn CreateSemaphoreW(
            attrs: *mut c_void,
            initial: i32,
            maximum: i32,
            name: *const u16,
        ) -> Handle;
        fn ReleaseSemaphore(sem: Handle, increment: i32, previous: *mut i32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    #[link(name = "ntdll")]
    extern "system" {
        /// The only way to read a semaphore's count without taking a token.
        fn NtQuerySemaphore(
            sem: Handle,
            class: i32,
            info: *mut c_void,
            len: u32,
            ret_len: *mut u32,
        ) -> i32;
    }

    pub struct Pool {
        sem: Handle,
    }

    // SAFETY: a semaphore handle is a kernel object reference; every operation on
    // it is thread-safe, and the pool never mutates the handle after creation.
    unsafe impl Send for Pool {}
    unsafe impl Sync for Pool {}

    impl Pool {
        /// A named semaphore holding `size` tokens. The auth string is the name
        /// itself, which the `jobserver` crate opens with `OpenSemaphoreA`, so the
        /// name must be ASCII — [`super::BuildPool::create`] guarantees that.
        pub fn create(name: &str, size: usize) -> std::io::Result<(Pool, String)> {
            let count = i32::try_from(size).unwrap_or(i32::MAX);
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            // SAFETY: null attributes (a non-inheritable handle, so panes reach the
            // pool by name only) and a null-terminated UTF-16 name.
            let sem =
                unsafe { CreateSemaphoreW(core::ptr::null_mut(), count, count, wide.as_ptr()) };
            if sem.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            // An existing object of that name belongs to someone else: its count is
            // theirs, so refuse it rather than share a stranger's pool.
            // SAFETY: read immediately after the call that set it.
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                // SAFETY: closing the handle just returned.
                unsafe { CloseHandle(sem) };
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "build pool name already in use",
                ));
            }
            Ok((Pool { sem }, name.to_string()))
        }

        pub fn available(&self) -> Option<usize> {
            let mut info = SemaphoreBasicInformation {
                current_count: 0,
                maximum_count: 0,
            };
            // SAFETY: `info` is a live SEMAPHORE_BASIC_INFORMATION and `len` is its
            // exact size; the handle carries SEMAPHORE_QUERY_STATE (CreateSemaphoreW
            // returns full access).
            let status = unsafe {
                NtQuerySemaphore(
                    self.sem,
                    SEMAPHORE_BASIC_INFORMATION,
                    &mut info as *mut SemaphoreBasicInformation as *mut c_void,
                    core::mem::size_of::<SemaphoreBasicInformation>() as u32,
                    core::ptr::null_mut(),
                )
            };
            (status == 0).then(|| info.current_count.max(0) as usize)
        }

        /// One token back. Fails (`false`) once the count is at the maximum, which
        /// is what makes an over-eager refill harmless.
        pub fn release_one(&self, _size: usize) -> bool {
            // SAFETY: a valid semaphore handle; a null previous-count is allowed.
            unsafe { ReleaseSemaphore(self.sem, 1, core::ptr::null_mut()) != 0 }
        }

        pub fn cleanup(&self) {}

        /// Take one token without waiting, as a client would. Tests only.
        #[cfg(test)]
        pub fn try_take(&self) -> bool {
            extern "system" {
                fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
            }
            // SAFETY: a valid semaphore handle; a zero timeout never blocks.
            unsafe { WaitForSingleObject(self.sem, 0) == 0 }
        }
    }

    impl Drop for Pool {
        fn drop(&mut self) {
            // SAFETY: closing the handle this pool owns.
            unsafe { CloseHandle(self.sem) };
        }
    }

    /// Whether any process in the session job is a build process. `None` when the
    /// job can't be listed or a live pane is missing from it.
    pub fn build_running(job: &crate::reap::SessionJob, panes: &[u32]) -> Option<bool> {
        let members = job.pids()?;
        if panes.iter().any(|p| !members.contains(p)) {
            return None;
        }
        Some(
            members.iter().any(|&pid| {
                crate::reap::image_name(pid).is_some_and(|n| super::is_build_image(&n))
            }),
        )
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;

    #[cfg(target_os = "macos")]
    type ModeT = u16;
    #[cfg(not(target_os = "macos"))]
    type ModeT = u32;

    /// `FIONREAD`: bytes waiting in the FIFO, which is the token count.
    #[cfg(target_os = "macos")]
    const FIONREAD: core::ffi::c_ulong = 0x4004_667f;
    #[cfg(not(target_os = "macos"))]
    const FIONREAD: core::ffi::c_ulong = 0x541b;

    extern "C" {
        fn mkfifo(path: *const core::ffi::c_char, mode: ModeT) -> i32;
        // Variadic in C; it must be declared variadic here too.
        fn ioctl(fd: i32, request: core::ffi::c_ulong, ...) -> i32;
    }

    /// The FIFO and one open read/write end. The open end is what keeps the
    /// tokens alive: a FIFO's buffered bytes are discarded when its last
    /// descriptor closes, so the pool holds one for the whole session.
    pub struct Pool {
        path: PathBuf,
        file: File,
    }

    impl Pool {
        /// A private FIFO in the temp directory, pre-filled with `size` tokens.
        /// The auth string is `fifo:<path>`, which the `jobserver` crate opens
        /// read/write. UNTESTED at runtime — written from the jobserver protocol
        /// and `mkfifo(3)`, compiled on unix targets but not run on a unix host.
        pub fn create(name: &str, size: usize) -> std::io::Result<(Pool, String)> {
            sweep_stale(&std::env::temp_dir());
            let path = std::env::temp_dir().join(format!("{name}.fifo"));
            let c = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
            // SAFETY: a null-terminated path; 0600 so only this user can reach it.
            if unsafe { mkfifo(c.as_ptr(), 0o600) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Read/write so the open never blocks waiting for a peer.
            let opened = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path);
            let mut file = match opened {
                Ok(f) => f,
                Err(e) => {
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
            };
            if let Err(e) = file.write_all(&vec![b'+'; size]) {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
            let auth = format!("fifo:{}", path.display());
            Ok((Pool { path, file }, auth))
        }

        pub fn available(&self) -> Option<usize> {
            let mut n: i32 = 0;
            // SAFETY: FIONREAD writes one int through the pointer.
            let rc = unsafe { ioctl(self.file.as_raw_fd(), FIONREAD, &mut n as *mut i32) };
            (rc == 0).then(|| n.max(0) as usize)
        }

        /// One token back, but never past `size`: a FIFO has no maximum of its own.
        ///
        /// The check and the write are two syscalls, so a client returning its
        /// own token in between would leave one token too many. The count is
        /// re-read after the write and the extra byte taken back, which narrows
        /// that window to the gap between two reads rather than closing it — the
        /// jobserver-over-FIFO protocol offers nothing atomic.
        pub fn release_one(&self, size: usize) -> bool {
            use std::io::Read;
            if self.available().map_or(true, |n| n >= size) {
                return false;
            }
            if (&self.file).write_all(b"+").is_err() {
                return false;
            }
            if self.available().is_some_and(|n| n > size) {
                // More than `size` bytes are buffered, so this read can't block.
                let _ = (&self.file).read(&mut [0u8; 1]);
                return false;
            }
            true
        }

        pub fn cleanup(&self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Remove pool FIFOs left by sessions that are gone. A session that crashes,
    /// or exits down a path that skips teardown, leaves its FIFO in the temp
    /// directory; each new pool clears those whose owning pid is dead.
    fn sweep_stale(dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(owner) = super::stale_owner(&name.to_string_lossy()) else {
                continue;
            };
            if !crate::reap::pid_alive(owner) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Whether a build process is running anywhere for this user. Unix has no
    /// job to scope the search to, so this is deliberately wider than the session:
    /// an unrelated build only delays a refill, it can't cause a wrong one.
    pub fn build_running(_job: &crate::reap::SessionJob, _panes: &[u32]) -> Option<bool> {
        running_images().map(|names| names.iter().any(|n| super::is_build_image(n)))
    }

    #[cfg(target_os = "linux")]
    fn running_images() -> Option<Vec<String>> {
        let entries = std::fs::read_dir("/proc").ok()?;
        Some(
            entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .bytes()
                        .all(|b| b.is_ascii_digit())
                })
                .filter_map(|e| std::fs::read_to_string(e.path().join("comm")).ok())
                .map(|s| s.trim_end().to_string())
                .collect(),
        )
    }

    #[cfg(target_os = "macos")]
    fn running_images() -> Option<Vec<String>> {
        extern "C" {
            fn proc_listallpids(buffer: *mut core::ffi::c_void, buffersize: i32) -> i32;
            fn proc_name(pid: i32, buffer: *mut core::ffi::c_void, buffersize: u32) -> i32;
        }
        let mut pids = vec![0i32; 8192];
        // SAFETY: the buffer is `len * 4` bytes, exactly the size passed.
        let n = unsafe {
            proc_listallpids(
                pids.as_mut_ptr() as *mut core::ffi::c_void,
                (pids.len() * core::mem::size_of::<i32>()) as i32,
            )
        };
        if n <= 0 {
            return None;
        }
        let mut out = Vec::new();
        for &pid in &pids[..(n as usize).min(pids.len())] {
            let mut buf = [0u8; 256];
            // SAFETY: `buf` is 256 bytes, the size passed.
            let len = unsafe {
                proc_name(
                    pid,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    buf.len() as u32,
                )
            };
            if len > 0 {
                out.push(String::from_utf8_lossy(&buf[..len as usize]).into_owned());
            }
        }
        Some(out)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn running_images() -> Option<Vec<String>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins_and_can_disable() {
        assert_eq!(pool_size(Some("6"), Some(12), 16), Some(6));
        assert_eq!(pool_size(Some(" off "), Some(12), 16), None);
        assert_eq!(pool_size(Some("OFF"), None, 16), None);
        assert_eq!(pool_size(Some("0"), Some(12), 16), None);
    }

    #[test]
    fn a_typo_in_the_env_does_not_remove_the_limit() {
        assert_eq!(pool_size(Some("sixteen"), None, 16), Some(16));
        assert_eq!(pool_size(Some(""), Some(4), 16), Some(4));
    }

    #[test]
    fn fleet_value_then_default() {
        assert_eq!(pool_size(None, Some(4), 16), Some(4));
        assert_eq!(pool_size(None, Some(0), 16), None);
        assert_eq!(pool_size(None, None, 16), Some(16));
        assert_eq!(pool_size(None, None, 0), Some(1));
    }

    #[test]
    fn makeflags_round_trips_through_the_parser() {
        let flags = makeflags("atrium-build-1-abc");
        assert_eq!(jobserver_auth(&flags), Some("atrium-build-1-abc"));
        let fifo = makeflags("fifo:/tmp/atrium-build-1-abc.fifo");
        assert_eq!(
            jobserver_auth(&fifo),
            Some("fifo:/tmp/atrium-build-1-abc.fifo")
        );
    }

    #[test]
    fn jobserver_auth_follows_the_jobserver_crate() {
        assert_eq!(jobserver_auth("-j8"), None);
        assert_eq!(jobserver_auth(""), None);
        assert_eq!(jobserver_auth("--jobserver-auth="), None);
        assert_eq!(
            jobserver_auth("--jobserver-auth=a --jobserver-auth=b"),
            Some("b")
        );
        assert_eq!(jobserver_auth("--jobserver-fds=3,4"), Some("3,4"));
        assert_eq!(
            jobserver_auth("--jobserver-fds=f --jobserver-auth=a"),
            Some("a")
        );
    }

    #[test]
    fn build_images_are_recognised() {
        for name in [
            "cargo",
            "cargo.exe",
            "RUSTC.EXE",
            r"C:\Users\x\.cargo\bin\rustc.exe",
            "/usr/bin/rustdoc",
            "clippy-driver",
            "build-script-build.exe",
            "build-script-bu", // Linux comm, truncated to 15 bytes
        ] {
            assert!(is_build_image(name), "{name}");
        }
        for name in [
            "claude.exe",
            "node",
            "cargo-watcher-gui",
            "rust-analyzer",
            "carg",
        ] {
            assert!(!is_build_image(name), "{name}");
        }
    }

    #[test]
    fn only_pool_fifos_are_candidates_for_the_stale_sweep() {
        assert_eq!(
            stale_owner("atrium-build-4242-0123abcdef012345.fifo"),
            Some(4242)
        );
        for other in [
            "atrium-build-4242-0123abcdef012345", // not a fifo name
            "atrium-build-x-0123.fifo",           // no pid
            "atrium-build-4242-.fifo",            // no nonce
            "atrium-build-4242-zz.fifo",          // nonce not hex
            "atrium-session-4242.pids",           // the crash registry, never ours
        ] {
            assert_eq!(stale_owner(other), None, "{other}");
        }
    }

    #[test]
    fn a_running_build_blocks_the_refill() {
        assert_eq!(tokens_to_restore(0, 3, true), 0);
        assert_eq!(tokens_to_restore(0, 3, false), 3);
        assert_eq!(tokens_to_restore(2, 3, false), 1);
        assert_eq!(tokens_to_restore(3, 3, false), 0);
        assert_eq!(tokens_to_restore(5, 3, false), 0);
    }

    #[cfg(windows)]
    #[test]
    fn a_leaked_token_is_restored_but_never_past_the_size() {
        let pool = BuildPool::create(3).expect("create pool");
        assert_eq!(pool.available(), Some(3));
        // Two clients take tokens and are "killed": nothing gives them back.
        assert!(pool.sys.try_take());
        assert!(pool.sys.try_take());
        assert_eq!(pool.available(), Some(1));
        // Restoring more than the shortfall stops at the size.
        assert_eq!(pool.restore(5), 2);
        assert_eq!(pool.available(), Some(3));
        assert_eq!(pool.restore(1), 0);
    }

    #[cfg(windows)]
    #[test]
    fn the_pool_name_is_what_clients_open() {
        let pool = BuildPool::create(2).expect("create pool");
        let auth = jobserver_auth(&pool.makeflags()).expect("auth").to_string();
        assert!(auth.starts_with("atrium-build-"), "{auth}");
        assert!(auth.is_ascii());
        // A second pool under the same name is refused, not shared.
        assert!(sys::Pool::create(&auth, 2).is_err());
    }
}
