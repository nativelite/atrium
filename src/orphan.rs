//! The last backstop: find and kill pane trees whose amux is gone.
//!
//! # Why this exists at all
//!
//! Windows gets a kernel-enforced answer for free — a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (see [`crate::reap::SessionJob`]) —
//! and the kernel honours it however amux dies, `TerminateProcess` included.
//! Unix has no such container, so [`crate::reap`] reconstructs one out of
//! process groups, `setsid`, and a pipe-EOF watchdog. Every part of that
//! reconstruction is *advisory*, and two holes were measured on the author's
//! machine, not theorised:
//!
//! 1. **A pane carried no marker.** `AMUX_CTL`/`AMUX_PANE`/`AMUX_TOKEN` were
//!    injected only when `--allow-ctl` was on. Without it a pane child had no
//!    amux environment whatsoever, so nothing on the machine identified it as
//!    ours and nothing could find it after the fact.
//! 2. **The registry — the watchdog's only record — was deleted even on the one
//!    path that proved it was needed.** Teardown computed `survivors`, then
//!    unconditionally removed the registry file, printed a warning to a terminal
//!    it was about to tear down, and exited. The watchdog woke on pipe EOF, read
//!    an absent registry, got an empty list, and killed nothing.
//!
//! The cost was 75 stranded processes holding 526 ptys against a machine-wide
//! `kern.tty.ptmx_max` of 511. Past that line *nothing* on the machine can open
//! a pty — not amux, not a new terminal tab, not the test suite.
//!
//! # The rule
//!
//! A process is an orphan only if **all** of these hold; any ambiguity is a
//! skip, because the fail-safe direction is *do not kill*:
//!
//! 1. It is linked to a session key `<owner_pid>:<owner_start>` (see
//!    [`SessionKey`] and "Two markers" below).
//! 2. Its pgid equals its pid — a session leader, i.e. a pane root amux itself
//!    created through `setsid`, not some descendant that wandered in.
//! 3. The owner named by the key is not a live amux ([`classify_owner`]).
//!
//! ## Why not `ppid == 1`
//!
//! The cheap test is wrong, and the counterexample came off the owner's machine:
//!
//! ```text
//! 17686     1  ?Es  (amux)   <- owner, STUCK EXITING
//! 17736 17686 Ss+   sh       <- pane, ALIVE, ppid still points at the corpse
//! ```
//!
//! A child is reparented when its parent *finishes* exiting. An amux wedged in
//! `?E` never gets there, so its panes keep pointing at a corpse and `ppid == 1`
//! never becomes true — and that was the exact population that had to be cleared
//! by hand. [`Proc::ppid`] is carried for the report; it is never the test.
//!
//! ## Why the key is not a bare pid
//!
//! Pids are reused. A marker naming a dead owner's pid would, after enough
//! churn, point at a live unrelated process and license killing that session's
//! panes. So the key pins the owner's **start instant** as well
//! ([`start_token`]): microsecond-resolution on macOS, boot-relative ticks on
//! Linux. Same pid *and* same start instant is the same process.
//!
//! # Two markers, because one of them cannot see shells on macOS
//!
//! The approved design injects `AMUX_SESSION` into every pane and reads it back
//! from the process table. **Measured on macOS 26 (Darwin 25.6.0), that is only
//! half a mechanism:** `ps -E` prints the environment of ordinary binaries but
//! prints *nothing* for a SIP/platform binary. Verified here, same uid, same
//! session:
//!
//! ```text
//! $ env AMUX_SESSION=7:x ./a-rust-binary &  ps -ww -E -o command= -p $!
//! ./a-rust-binary TERM_PROGRAM=Apple_Terminal SHELL=/bin/zsh ...   <- env visible
//! $ env AMUX_SESSION=9:z /bin/sh -c 'sleep 5' & ps -ww -E -o command= -p $!
//! sleep 5                                                          <- env hidden
//! ```
//!
//! `/bin/sh`, `/bin/sleep` and `/usr/bin/perl` all hide it. A shell pane is
//! exactly the population that stranded in the incident, so an env-only sweep
//! would have been blind to the processes it was written to collect.
//!
//! So there are two markers and one predicate:
//!
//! * [`Marker::Env`] — `AMUX_SESSION` in the process's own environment. Always
//!   readable on Linux; on macOS readable for non-platform binaries (`node`,
//!   `claude`, a nested `amux`). Travels with the process and survives anything
//!   that happens to `/tmp`.
//! * [`Marker::Stamp`] — a file `amux-pane-<pid>.stamp` in
//!   [`crate::reap::registry_dir`] recording the session key **and the pane's
//!   own start token**. Covers every pane including `/bin/sh`. The start token
//!   is what makes a stale stamp harmless: a recycled pid has a different start
//!   instant, so the stamp simply stops matching and is pruned.
//!
//! Both feed the same [`Proc`], so [`select`] never learns which one found a
//! process — only the report says.
//!
//! # Shape: gather, decide, kill
//!
//! [`snapshot`] is impure and does no judging. [`select`] and
//! [`classify_owner`] are pure functions over a synthetic table and are where
//! the whole rule lives, so every condition and every fail-safe skip is unit
//! tested with no filesystem and no live processes. [`sweep`] is the only thing
//! that signals anything.
//!
//! # Where it runs, and where it must not
//!
//! At watchdog-death and on an explicit `amux reap` (plus `--reap-orphans` at
//! startup, opt-in for this first release). **Never on the 15 ms event loop**: a
//! prior version of this codebase ran `ps` per ancestry hop and the resulting
//! process storm showed up as timing flakiness in unrelated tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The pane marker. Non-secret and *meant* to be readable — unlike `AMUX_TOKEN`,
/// which is a capability. Injected into every pane unconditionally, independent
/// of `--allow-ctl`: a pane with no marker is a pane nothing can ever find.
pub const ENV_SESSION: &str = "AMUX_SESSION";

/// The argv flag that opts a launch into the startup sweep.
pub const REAP_FLAG: &str = "--reap-orphans";

// --- the key ---------------------------------------------------------------

/// Identifies one amux session: the owner's pid **and** the instant it started.
///
/// The start token is opaque and only ever compared for equality; it is not a
/// wall-clock time and is not comparable across platforms or reboots.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey {
    /// The pid of the amux that owns the pane.
    pub owner: u32,
    /// [`start_token`] of that pid, taken when the key was minted.
    pub started: u64,
}

impl SessionKey {
    /// The wire form: `<owner>:<started>`. No whitespace, so it survives being
    /// read back out of a `ps` line.
    pub fn encode(&self) -> String {
        format!("{}:{}", self.owner, self.started)
    }

    /// Parse the wire form. `None` for anything malformed, and for owner 0 —
    /// pid 0 is not a process anyone can own a pane.
    pub fn parse(s: &str) -> Option<SessionKey> {
        let (owner, started) = s.trim().split_once(':')?;
        let owner: u32 = owner.parse().ok()?;
        let started: u64 = started.parse().ok()?;
        if owner == 0 {
            return None;
        }
        Some(SessionKey { owner, started })
    }
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.encode())
    }
}

/// This session's key, computed **once** and reused for every spawn.
///
/// It must be stable for the life of the process: a key that changed between
/// spawns would leave panes claiming owners that never existed. `None` means the
/// platform could not supply a start token (Windows, where the Job Object makes
/// this whole module unnecessary) — callers then inject no marker rather than a
/// bare pid, which would be worse than none.
pub fn session_key() -> Option<SessionKey> {
    static KEY: OnceLock<Option<SessionKey>> = OnceLock::new();
    KEY.get_or_init(|| {
        let me = std::process::id();
        Some(SessionKey {
            owner: me,
            started: start_token(me)?,
        })
    })
    .clone()
}

// --- the pure decision layer -----------------------------------------------

/// Which marker linked a process to a session key. Reporting only — the rule is
/// identical either way.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Marker {
    /// `AMUX_SESSION` read out of the process's own environment.
    Env,
    /// An `amux-pane-<pid>.stamp` file whose recorded start token still matches.
    Stamp,
}

impl std::fmt::Display for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Marker::Env => "env",
            Marker::Stamp => "stamp",
        })
    }
}

/// One row of the process snapshot, already resolved to a session key (or not).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    /// Carried for the report only. See the module docs: `ppid == 1` is *not*
    /// the test, because an owner stuck in `?E` never reparents its children.
    pub ppid: u32,
    pub pgid: u32,
    /// The session that claims this process, if any.
    pub key: Option<SessionKey>,
    pub via: Marker,
}

/// What is known about the amux named by a session key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// Still there. Its panes are not orphans.
    Live,
    /// Provably not there any more: exited, a corpse, or the pid now belongs to
    /// a different process. Its panes are orphans.
    Dead,
    /// Could not be established. Treated exactly like `Live` — skip.
    Unknown,
}

/// The sweeping process's own identity, so a sweep can never collect itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Sweeper {
    pub pid: u32,
    pub pgid: u32,
}

/// Decide what a session key's owner is, from four independently-gathered
/// facts. Pure, so every combination is testable without a live process.
///
/// * `running` — [`crate::reap::pid_running`], **not** `pid_alive`: `kill(pid,0)`
///   succeeds on a zombie, and this repo has shipped that bug three times.
/// * `ident` — is the process at that pid the same executable file as ours?
///   `Some(false)` is "definitely a different image", `None` is "cannot tell".
///   From [`crate::warden::same_binary`], which compares `(st_dev, st_ino)` of
///   the kernel's executable, never `argv[0]` — `ps -o comm=` is argv[0] on
///   macOS and `exec -a amux /bin/sleep` forges it in one line.
/// * `start` — the pid's current start token, and `recorded` the one in the key.
///
/// The two checks are deliberately not symmetric:
///
/// * A start-token **mismatch** is conclusive: the pid was recycled, so the
///   original owner is gone. `Dead`.
/// * A start-token **match** is conclusive the other way — same pid *and* same
///   start instant is the same process — so a disagreeing identity check there
///   yields `Unknown`, not `Dead`. A differently-built amux (`./target/release/
///   amux` hosting a pane that launches `~/.cargo/bin/amux`) is a real, already
///   documented configuration in this codebase, and it must not become a reason
///   to kill a live session's panes.
/// * Identity only creates a `Dead` where the start token is unreadable, which
///   is the one case where it adds information rather than contradicting it.
pub fn classify_owner(
    running: bool,
    ident: Option<bool>,
    start: Option<u64>,
    recorded: u64,
) -> Owner {
    if !running {
        return Owner::Dead;
    }
    match start {
        Some(t) if t == recorded => match ident {
            Some(false) => Owner::Unknown,
            _ => Owner::Live,
        },
        Some(_) => Owner::Dead,
        None => match ident {
            Some(false) => Owner::Dead,
            _ => Owner::Unknown,
        },
    }
}

/// The orphan rule itself, on one process. All three conditions, plus the two
/// guards that stop a sweep collecting the sweeper.
pub fn is_orphan(p: &Proc, owner: Owner, me: Sweeper) -> bool {
    // (1) marked.
    if p.key.is_none() {
        return false;
    }
    // (2) a session leader: a pane root amux created with setsid, not a
    // descendant that inherited the marker and wandered into the group. Killing
    // by pgid is what `reap` does, so a non-leader would take its whole group
    // down with it — including processes that were never ours.
    if p.pgid != p.pid {
        return false;
    }
    // Never ourselves, never our own group, never init.
    if p.pid <= 1 || p.pid == me.pid || p.pgid == me.pgid {
        return false;
    }
    // (3) the owner is gone.
    matches!(owner, Owner::Dead)
}

/// Pick the orphans out of a snapshot.
///
/// `only` restricts the sweep to a single session key — what the watchdog uses,
/// so a dying amux only ever collects its own panes. `None` sweeps every dead
/// owner, which is what `amux reap` does and what self-heals an exhausted pty
/// pool.
///
/// `owner_of` is injected so this function is pure: the tests drive it with a
/// closure over a fixture and never touch a process table. Its answers are
/// memoised per key, so a hundred panes of one dead session cost one lookup.
pub fn select(
    procs: &[Proc],
    only: Option<&SessionKey>,
    owner_of: &mut dyn FnMut(&SessionKey) -> Owner,
    me: Sweeper,
) -> Vec<Victim> {
    let mut seen: HashMap<SessionKey, Owner> = HashMap::new();
    let mut out: Vec<Victim> = Vec::new();
    for p in procs {
        let Some(key) = p.key.as_ref() else { continue };
        if let Some(want) = only {
            if key != want {
                continue;
            }
        }
        let owner = match seen.get(key) {
            Some(o) => *o,
            None => {
                let o = owner_of(key);
                seen.insert(key.clone(), o);
                o
            }
        };
        if is_orphan(p, owner, me) {
            out.push(Victim {
                pid: p.pid,
                ppid: p.ppid,
                key: key.clone(),
                via: p.via,
            });
        }
    }
    // Deterministic, and deduplicated: the same pid can be found by both markers.
    out.sort_by_key(|v| v.pid);
    out.dedup_by_key(|v| v.pid);
    out
}

/// A process group the sweep decided to collect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Victim {
    pub pid: u32,
    pub ppid: u32,
    pub key: SessionKey,
    pub via: Marker,
}

impl std::fmt::Display for Victim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pid {} (ppid {}, owner {} gone, via {})",
            self.pid, self.ppid, self.key.owner, self.via
        )
    }
}

/// Strip [`REAP_FLAG`] from the front of an argument list.
///
/// Front-stripping, like every other amux meta-flag (`--identity`,
/// `--allow-ctl`, `-n`): everything after the hosted command belongs to the
/// hosted command and is never inspected.
pub fn parse_flag(args: &[String]) -> (bool, Vec<String>) {
    let mut on = false;
    let mut i = 0;
    while i < args.len() && args[i] == REAP_FLAG {
        on = true;
        i += 1;
    }
    (on, args[i..].to_vec())
}

/// Pull a session key out of a blob of `KEY=VALUE` tokens.
///
/// Split out and pure because both readers use it and both have a parsing
/// hazard worth testing: macOS `ps -E` appends the environment to the command
/// line with no delimiter, and Linux `environ` is NUL-separated.
fn marker_in<'a>(tokens: impl Iterator<Item = &'a str>) -> Option<SessionKey> {
    let prefix = format!("{ENV_SESSION}=");
    for tok in tokens {
        if let Some(v) = tok.strip_prefix(&prefix) {
            if let Some(k) = SessionKey::parse(v) {
                return Some(k);
            }
        }
    }
    None
}

// --- the impure layers ------------------------------------------------------

/// Where a pane's stamp file lives. Same fixed directory as the session
/// registry, and for the same reason: `std::env::temp_dir()` reads `$TMPDIR`,
/// which an agent in a pane owns.
pub fn stamp_path(pid: u32) -> PathBuf {
    crate::reap::registry_dir().join(format!("amux-pane-{pid}.stamp"))
}

/// Record that `pid` is a pane root of this session.
///
/// Written immediately after the pty spawn, so the pane is findable even when
/// the OS will not show its environment to `ps` (every SIP binary on macOS —
/// `/bin/sh` included, which is the shell every default pane runs).
///
/// Best effort by construction: a failure here loses a *marker*, never a pane.
/// The stamp records the pane's own start token, so a stale file left by a crash
/// cannot be turned into a kill order against whatever later inherits the pid.
pub fn stamp(pid: u32) {
    let Some(key) = session_key() else { return };
    let Some(started) = start_token(pid) else {
        return;
    };
    let body = format!("session={}\nstarted={started}\n", key.encode());
    let path = stamp_path(pid);
    // Write-then-rename, as the registry does: a reader must never see half a
    // line, because a truncated number still parses as a different valid one.
    let tmp = path.with_extension("stamp.tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Drop a pane's stamp — it exited, or we just tore it down.
pub fn unstamp(pid: u32) {
    let _ = std::fs::remove_file(stamp_path(pid));
}

/// Read one stamp file back. `None` if it is not ours, is malformed, or no
/// longer describes the process it names.
fn read_stamp(path: &std::path::Path) -> Option<(u32, SessionKey)> {
    let pid: u32 = path
        .file_name()?
        .to_str()?
        .strip_prefix("amux-pane-")?
        .strip_suffix(".stamp")?
        .parse()
        .ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let mut key = None;
    let mut started = None;
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix("session=") {
            key = SessionKey::parse(v);
        } else if let Some(v) = line.trim().strip_prefix("started=") {
            started = v.parse::<u64>().ok();
        }
    }
    let key = key?;
    // The anti-pid-reuse bind: the process at this pid must be the very process
    // that was stamped. A recycled pid has a different start instant and the
    // stamp is simply ignored (and pruned).
    if start_token(pid)? != started? {
        return None;
    }
    Some((pid, key))
}

/// Delete stamps that no longer describe a live process. Cheap — a readdir plus
/// two syscalls per file, no `ps`, no subprocess — so it is safe to run at
/// startup. It removes only files that describe nothing, so it can never
/// disturb another live session.
pub fn prune_stamps() -> usize {
    let dir = crate::reap::registry_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let path = e.path();
        let is_stamp = path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("amux-pane-") && s.ends_with(".stamp"));
        if !is_stamp {
            continue;
        }
        if read_stamp(&path).is_none() && std::fs::remove_file(&path).is_ok() {
            n += 1;
        }
    }
    n
}

/// Every process this machine will admit to that carries one of our markers.
///
/// ONE process-table read, never one per pid. Rows the platform cannot answer
/// for are simply absent, which costs a sweep, never a wrong kill.
pub fn snapshot() -> Vec<Proc> {
    let mut out = env_candidates();
    let have: std::collections::HashSet<u32> = out.iter().map(|p| p.pid).collect();
    for p in stamp_candidates() {
        // The env marker is on the process itself, so it wins where both exist.
        if !have.contains(&p.pid) {
            out.push(p);
        }
    }
    out
}

/// Candidates found through their stamp files. No `ps`: pgid and start come
/// straight from the kernel, one syscall each, for the handful of stamped pids.
fn stamp_candidates() -> Vec<Proc> {
    let dir = crate::reap::registry_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Some((pid, key)) = read_stamp(&path) else {
            continue;
        };
        let Some((ppid, pgid, _)) = proc_ids(pid) else {
            continue;
        };
        out.push(Proc {
            pid,
            ppid,
            pgid,
            key: Some(key),
            via: Marker::Stamp,
        });
    }
    out
}

/// Signal the orphans' process groups, hardest part last.
///
/// `only` = the watchdog's own session key; `None` = every dead owner.
/// TERM, one shared grace period, then KILL — the same policy as every other
/// teardown in [`crate::reap`], and for the same reason: a bare `SIGKILL`
/// guarantees the agent never reaps its own children.
pub fn sweep(only: Option<&SessionKey>) -> Vec<Victim> {
    let procs = snapshot();
    let me = Sweeper {
        pid: std::process::id(),
        pgid: own_pgid(),
    };
    let victims = select(&procs, only, &mut live_owner, me);
    for v in &victims {
        crate::reap::term_tree(v.pid);
    }
    if !victims.is_empty() {
        std::thread::sleep(crate::reap::GRACE);
        for v in &victims {
            crate::reap::kill_tree(v.pid);
            unstamp(v.pid);
        }
    }
    prune_stamps();
    victims
}

/// The live-system half of [`classify_owner`]: gather the three facts.
fn live_owner(key: &SessionKey) -> Owner {
    classify_owner(
        crate::reap::pid_running(key.owner),
        crate::warden::same_binary(key.owner),
        start_token(key.owner),
        key.started,
    )
}

// --- platform: start token, ids, and the environment ------------------------

/// An opaque token that changes whenever a pid is reused.
///
/// Never a wall-clock time and never compared for order — only for equality
/// against a token taken earlier for the same pid.
#[cfg(all(unix, target_os = "macos"))]
pub fn start_token(pid: u32) -> Option<u64> {
    proc_ids(pid).map(|(_, _, start)| start)
}

/// `(ppid, pgid, start token)` for one pid, or `None` if it cannot be read.
///
/// macOS: one `proc_pidinfo(PROC_PIDTBSDINFO)` — a syscall, not a subprocess, so
/// this is safe to call per candidate without the process storm that `ps`-per-pid
/// caused here before. The struct is transcribed from `<sys/proc_info.h>`, where
/// a wrong field order or width is silent memory corruption rather than a
/// compile error; `pbi_pid` is therefore checked against the pid we asked for,
/// which makes a bad layout fail *closed* (`None` → `Unknown` → skip) instead of
/// returning garbage. `layout_of_proc_bsdinfo_is_right` cross-checks all three
/// fields against `ps`.
#[cfg(all(unix, target_os = "macos"))]
fn proc_ids(pid: u32) -> Option<(u32, u32, u64)> {
    /// `PROC_PIDTBSDINFO` from `<sys/proc_info.h>`.
    const FLAVOR: i32 = 3;
    const MAXCOMLEN: usize = 16;

    #[repr(C)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [u8; MAXCOMLEN],
        pbi_name: [u8; 2 * MAXCOMLEN],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut core::ffi::c_void,
            buffersize: i32,
        ) -> i32;
    }

    if pid == 0 {
        return None;
    }
    // SAFETY: every field is a plain integer or a byte array, so all-zero is a
    // valid value; the call writes at most `buffersize` bytes into it.
    let mut info: ProcBsdInfo = unsafe { core::mem::zeroed() };
    let size = core::mem::size_of::<ProcBsdInfo>() as i32;
    // SAFETY: `info` is a live allocation of exactly `size` bytes, which is the
    // whole contract of the call.
    let n = unsafe {
        proc_pidinfo(
            pid as i32,
            FLAVOR,
            0,
            &mut info as *mut ProcBsdInfo as *mut core::ffi::c_void,
            size,
        )
    };
    // A short write means the kernel's struct is not the one transcribed here;
    // a pid that does not match means the same. Either way: cannot tell.
    if n != size || info.pbi_pid != pid {
        return None;
    }
    // Microseconds since the epoch, used only for equality — it is the finest
    // grain the kernel offers, which is what makes pid reuse detectable.
    let start = info
        .pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec);
    Some((info.pbi_ppid, info.pbi_pgid, start))
}

/// Linux (and other `/proc` unixes): field 22 of `/proc/<pid>/stat` is the
/// process's start time in clock ticks since boot — stable for the life of the
/// process and different for a recycled pid, which is all this needs. It is
/// deliberately NOT converted to wall clock: the token is opaque.
///
/// UNTESTED — written from `proc(5)`, not run on a Linux host. The parsing
/// hazard it does handle is the one that bites everyone: field 2 is the comm in
/// parentheses and may contain spaces and parentheses, so the split is at the
/// LAST `)`, never by whitespace from the start.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn start_token(pid: u32) -> Option<u64> {
    proc_ids(pid).map(|(_, _, start)| start)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn proc_ids(pid: u32) -> Option<(u32, u32, u64)> {
    if pid == 0 {
        return None;
    }
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_proc_stat(&text)
}

/// Split out so the `)`-in-comm hazard is testable without a Linux host: this
/// function is compiled and tested on every platform.
#[allow(dead_code)]
fn parse_proc_stat(text: &str) -> Option<(u32, u32, u64)> {
    let tail = &text[text.rfind(')')? + 1..];
    let f: Vec<&str> = tail.split_whitespace().collect();
    // `tail` starts at field 3 (state), so field N of proc(5) is `f[N - 3]`.
    let ppid: u32 = f.get(1)?.parse().ok()?; // field 4
    let pgid: u32 = f.get(2)?.parse().ok()?; // field 5
    let start: u64 = f.get(19)?.parse().ok()?; // field 22
    Some((ppid, pgid, start))
}

/// macOS: one `ps` for the whole table. `-E` appends the environment to the
/// command column, which is where the marker is read from.
///
/// Absolute path, not `ps`: `Command::new("ps")` resolves through `$PATH`, which
/// an agent in a pane owns — `warden` and `reap` both refuse to do that for the
/// same reason. A shimmed `ps` here fails *closed* (fewer candidates, no kills),
/// but it would still be a lever an agent should not have.
///
/// The hazard, stated plainly: `-E` puts argv and env in one column with no
/// delimiter, so a process whose *argv* contains the literal text
/// `AMUX_SESSION=<key>` is indistinguishable from one whose environment does.
/// That is a way to nominate yourself for collection, not a way to nominate
/// anyone else — the key must still name a dead amux, and the process must be
/// its own group leader.
#[cfg(all(unix, target_os = "macos"))]
fn env_candidates() -> Vec<Proc> {
    let Ok(o) = std::process::Command::new("/bin/ps")
        .args(["-eww", "-E", "-o", "pid=,ppid=,pgid=,command="])
        .output()
    else {
        return Vec::new();
    };
    if !o.status.success() {
        return Vec::new();
    }
    parse_ps_e(&String::from_utf8_lossy(&o.stdout))
}

/// Pure, so the whole macOS reader is testable from a captured `ps` transcript.
#[allow(dead_code)]
fn parse_ps_e(text: &str) -> Vec<Proc> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(pgid)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(pgid)) =
            (pid.parse::<u32>(), ppid.parse::<u32>(), pgid.parse::<u32>())
        else {
            continue;
        };
        let Some(key) = marker_in(it) else { continue };
        out.push(Proc {
            pid,
            ppid,
            pgid,
            key: Some(key),
            via: Marker::Env,
        });
    }
    out
}

/// Linux: `/proc/<pid>/environ` is the process's own environment with no argv
/// mixed in and no `ps` in the way — strictly better than the macOS path, and
/// it works for every binary rather than only the unprotected ones.
///
/// UNTESTED on a Linux host. `environ` is NUL-separated and unreadable for a
/// process this uid may not inspect, which lands on "no candidate" — a missed
/// sweep, never a wrong kill.
#[cfg(all(unix, not(target_os = "macos")))]
fn env_candidates() -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        let text = String::from_utf8_lossy(&raw);
        let Some(key) = marker_in(text.split('\0')) else {
            continue;
        };
        let Some((ppid, pgid, _)) = proc_ids(pid) else {
            continue;
        };
        out.push(Proc {
            pid,
            ppid,
            pgid,
            key: Some(key),
            via: Marker::Env,
        });
    }
    out
}

/// This process's own process-group id, so a sweep can never signal the group it
/// is running in.
#[cfg(unix)]
fn own_pgid() -> u32 {
    extern "C" {
        /// `pid_t getpgrp(void)` — cannot fail for the calling process.
        fn getpgrp() -> i32;
    }
    // SAFETY: a read-only query about ourselves; no arguments, no pointers.
    let g = unsafe { getpgrp() };
    if g < 0 {
        0
    } else {
        g as u32
    }
}

// --- Windows ----------------------------------------------------------------
//
// Deliberately inert, and this is not a gap. Windows already has the
// kernel-enforced version of everything above: a Job Object created at spawn
// with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (see `reap::SessionJob`), whose
// guarantee the kernel honours however amux dies — including `TerminateProcess`,
// which no handler can catch, and which is the exact death mode the unix
// reconstruction cannot cover. There is nothing left over for a marker-and-sweep
// to collect, so adding one would be untested Windows code buying nothing.
//
// Gated the same way `reap::spawn_watchdog`'s callers are: no session key means
// no marker is injected and no sweep is ever run, so Windows behaviour is
// byte-for-byte unchanged.

#[cfg(not(unix))]
pub fn start_token(_pid: u32) -> Option<u64> {
    None
}

#[cfg(not(unix))]
fn proc_ids(_pid: u32) -> Option<(u32, u32, u64)> {
    None
}

#[cfg(not(unix))]
fn env_candidates() -> Vec<Proc> {
    Vec::new()
}

#[cfg(not(unix))]
fn own_pgid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(owner: u32, started: u64) -> SessionKey {
        SessionKey { owner, started }
    }

    fn leader(pid: u32, k: Option<SessionKey>) -> Proc {
        Proc {
            pid,
            ppid: 1,
            pgid: pid,
            key: k,
            via: Marker::Env,
        }
    }

    const ME: Sweeper = Sweeper {
        pid: 4242,
        pgid: 4242,
    };

    // --- the key ------------------------------------------------------------

    /// A bare pid is not acceptable as a marker: after enough churn it points at
    /// a live unrelated process. The key must round-trip both halves.
    #[test]
    fn a_key_round_trips_and_carries_a_start_instant() {
        let k = key(17686, 1_759_000_000_123_456);
        assert_eq!(k.encode(), "17686:1759000000123456");
        assert_eq!(SessionKey::parse(&k.encode()), Some(k.clone()));
        assert_eq!(SessionKey::parse("17686"), None, "pid alone is not a key");
        assert_eq!(SessionKey::parse("0:5"), None, "pid 0 owns nothing");
        assert_eq!(SessionKey::parse("abc:5"), None);
        assert_eq!(SessionKey::parse("5:xyz"), None);
        assert!(
            !k.encode().contains(char::is_whitespace),
            "the key is read back out of a whitespace-split ps line"
        );
    }

    /// The key must be stable for the life of the process — a key that changed
    /// between spawns would leave panes claiming owners that never existed.
    #[cfg(unix)]
    #[test]
    fn the_session_key_is_computed_once_and_is_ours() {
        let a = session_key().expect("unix supplies a start token");
        let b = session_key().expect("unix supplies a start token");
        assert_eq!(a, b, "the session key must not move under a spawn");
        assert_eq!(a.owner, std::process::id());
    }

    // --- classify_owner: exhaustive, and every ambiguity is a skip -----------

    /// A dead owner is the whole point: its panes are collectable.
    #[test]
    fn a_gone_owner_is_dead() {
        assert_eq!(classify_owner(false, None, None, 7), Owner::Dead);
        assert_eq!(classify_owner(false, Some(true), Some(7), 7), Owner::Dead);
    }

    /// `pid_running`, not `pid_alive`. A zombie owner reports `running == false`
    /// (that is what `reap::pid_running` exists for) and its panes are orphans —
    /// the "amux stuck in `?E` with live panes" population from the incident.
    #[test]
    fn a_live_matching_owner_is_never_collected() {
        assert_eq!(classify_owner(true, Some(true), Some(7), 7), Owner::Live);
        assert_eq!(
            classify_owner(true, None, Some(7), 7),
            Owner::Live,
            "an unreadable identity must not demote a proven start-instant match"
        );
    }

    /// The pid was recycled: same number, different process. Conclusive.
    #[test]
    fn a_recycled_owner_pid_is_dead() {
        assert_eq!(classify_owner(true, Some(true), Some(9), 7), Owner::Dead);
        assert_eq!(classify_owner(true, Some(false), Some(9), 7), Owner::Dead);
        assert_eq!(classify_owner(true, None, Some(9), 7), Owner::Dead);
    }

    /// Identity may only create a `Dead` where the start token is unreadable.
    /// With a matching start instant it is contradicting proof, and the fail-safe
    /// direction is to skip — a differently-built amux is a real configuration.
    #[test]
    fn a_disagreeing_identity_never_outvotes_a_matching_start() {
        assert_eq!(
            classify_owner(true, Some(false), Some(7), 7),
            Owner::Unknown
        );
        assert_eq!(classify_owner(true, Some(false), None, 7), Owner::Dead);
    }

    /// Nothing established: skip. A missed sweep is recoverable; a wrong kill is
    /// not.
    #[test]
    fn an_unreadable_owner_is_unknown_not_dead() {
        assert_eq!(classify_owner(true, None, None, 7), Owner::Unknown);
        assert_eq!(classify_owner(true, Some(true), None, 7), Owner::Unknown);
    }

    // --- is_orphan: the three conditions ------------------------------------

    #[test]
    fn all_three_conditions_together_make_an_orphan() {
        let p = leader(100, Some(key(1, 1)));
        assert!(is_orphan(&p, Owner::Dead, ME));
    }

    /// (1) No marker, no opinion. This is the hole that made the incident
    /// unrecoverable: without `--allow-ctl` a pane carried no amux env at all.
    #[test]
    fn an_unmarked_process_is_never_an_orphan() {
        let p = leader(100, None);
        assert!(!is_orphan(&p, Owner::Dead, ME));
    }

    /// (2) Not a session leader. `reap` kills by process GROUP, so collecting a
    /// non-leader would take down every process in a group we never created.
    #[test]
    fn a_descendant_that_merely_inherited_the_marker_is_skipped() {
        let mut p = leader(100, Some(key(1, 1)));
        p.pgid = 55; // inherited the env, but lives in someone else's group
        assert!(!is_orphan(&p, Owner::Dead, ME));
    }

    /// (3) A live owner keeps its panes. And `Unknown` must behave exactly like
    /// `Live` — that is the fail-safe direction, stated as a test.
    #[test]
    fn a_live_or_unproven_owner_keeps_its_panes() {
        let p = leader(100, Some(key(1, 1)));
        assert!(!is_orphan(&p, Owner::Live, ME));
        assert!(!is_orphan(&p, Owner::Unknown, ME));
    }

    /// `ppid == 1` is not the test. The measured counterexample: an owner stuck
    /// in `?E` never finishes exiting, so its panes are never reparented — they
    /// still point at the corpse. A pane whose ppid is its dead owner must be
    /// collected all the same.
    #[test]
    fn a_pane_still_pointing_at_a_stuck_corpse_is_collected() {
        let p = Proc {
            pid: 17736,
            ppid: 17686, // the corpse, still not reparented onto init
            pgid: 17736,
            key: Some(key(17686, 42)),
            via: Marker::Stamp,
        };
        assert!(is_orphan(&p, Owner::Dead, ME));
    }

    /// A sweep must never collect the process running it, nor anything sharing
    /// its group — `amux reap` runs in the operator's shell group.
    #[test]
    fn a_sweep_never_collects_itself() {
        let me = leader(ME.pid, Some(key(1, 1)));
        assert!(!is_orphan(&me, Owner::Dead, ME));
        let sibling = Proc {
            pgid: ME.pgid,
            ..leader(9001, Some(key(1, 1)))
        };
        assert!(!is_orphan(&sibling, Owner::Dead, ME));
        let init = leader(1, Some(key(1, 1)));
        assert!(!is_orphan(&init, Owner::Dead, ME));
    }

    // --- select: the whole rule over a synthetic table -----------------------

    #[test]
    fn select_collects_only_dead_owners_and_asks_once_per_key() {
        let dead = key(10, 1);
        let live = key(20, 2);
        let procs = vec![
            leader(101, Some(dead.clone())),
            leader(102, Some(dead.clone())),
            leader(201, Some(live.clone())),
            leader(300, None),
        ];
        let mut asked = Vec::new();
        let mut owner_of = |k: &SessionKey| {
            asked.push(k.clone());
            if *k == dead {
                Owner::Dead
            } else {
                Owner::Live
            }
        };
        let got = select(&procs, None, &mut owner_of, ME);
        assert_eq!(
            got.iter().map(|v| v.pid).collect::<Vec<_>>(),
            vec![101, 102]
        );
        assert_eq!(
            asked,
            vec![dead, live],
            "one owner lookup per key, not per pane"
        );
    }

    /// The watchdog sweeps for ITS OWN key only. A dying amux collecting some
    /// other session's panes because they also look orphaned is not its business
    /// and is exactly the "agent-directed kill primitive" shape `warden` warns
    /// about.
    #[test]
    fn a_restricted_sweep_touches_only_its_own_session() {
        let mine = key(10, 1);
        let other = key(11, 1);
        let procs = vec![
            leader(101, Some(mine.clone())),
            leader(102, Some(other.clone())),
        ];
        let mut all_dead = |_: &SessionKey| Owner::Dead;
        let got = select(&procs, Some(&mine), &mut all_dead, ME);
        assert_eq!(got.iter().map(|v| v.pid).collect::<Vec<_>>(), vec![101]);
        let got = select(&procs, None, &mut all_dead, ME);
        assert_eq!(
            got.iter().map(|v| v.pid).collect::<Vec<_>>(),
            vec![101, 102],
            "an unrestricted sweep is what `amux reap` runs"
        );
    }

    /// Both markers can find the same pane; it must be signalled once.
    #[test]
    fn a_process_found_by_both_markers_is_reported_once() {
        let k = key(10, 1);
        let procs = vec![
            leader(101, Some(k.clone())),
            Proc {
                via: Marker::Stamp,
                ..leader(101, Some(k))
            },
        ];
        let mut dead = |_: &SessionKey| Owner::Dead;
        assert_eq!(select(&procs, None, &mut dead, ME).len(), 1);
    }

    // --- the readers --------------------------------------------------------

    /// macOS `ps -E` runs argv and env together in one column. The marker has to
    /// be found there, and a line without one must not become a candidate.
    #[test]
    fn the_ps_reader_finds_a_marker_after_the_command_line() {
        let text = "\
  101   1   101 /bin/sh -i PATH=/usr/bin AMUX_SESSION=17686:99 TERM=xterm
  102   1   102 /bin/sh -i PATH=/usr/bin TERM=xterm
  103 102   102 node server.js AMUX_SESSION=17686:99
  bad line
";
        let got = parse_ps_e(text);
        assert_eq!(got.len(), 2, "only the marked lines are candidates");
        assert_eq!(got[0].pid, 101);
        assert_eq!(got[0].pgid, 101);
        assert_eq!(got[0].key.as_ref().unwrap(), &key(17686, 99));
        assert_eq!(got[1].pid, 103);
        assert_eq!(
            got[1].pgid, 102,
            "a non-leader is still parsed, then skipped"
        );
        assert!(!is_orphan(&got[1], Owner::Dead, ME));
    }

    /// A malformed marker is not a licence to kill: it yields no candidate at
    /// all rather than a key with a defaulted owner.
    #[test]
    fn a_malformed_marker_yields_no_candidate() {
        assert!(parse_ps_e("  101   1   101 sh AMUX_SESSION=\n").is_empty());
        assert!(parse_ps_e("  101   1   101 sh AMUX_SESSION=0:1\n").is_empty());
        assert!(parse_ps_e("  101   1   101 sh XAMUX_SESSION=1:1\n").is_empty());
    }

    /// `/proc/<pid>/stat` field 2 is the comm in parentheses and may contain
    /// spaces AND parentheses, so the split is at the last `)`. Getting this
    /// wrong shifts every field and hands back another process's numbers.
    #[test]
    fn proc_stat_is_split_at_the_last_paren() {
        // pid 7, comm "(we ird)", state S, ppid 3, pgid 7, ... field 22 = 8899.
        let line = "7 ((we ird)) S 3 7 7 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 8899 0 0";
        assert_eq!(parse_proc_stat(line), Some((3, 7, 8899)));
        assert_eq!(parse_proc_stat("no parens here"), None);
    }

    // --- the flag -----------------------------------------------------------

    #[test]
    fn the_startup_flag_is_stripped_from_the_front_only() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_flag(&args(&["sh"])), (false, args(&["sh"])));
        assert_eq!(
            parse_flag(&args(&["--reap-orphans", "sh"])),
            (true, args(&["sh"]))
        );
        assert_eq!(
            parse_flag(&args(&["sh", "--reap-orphans"])),
            (false, args(&["sh", "--reap-orphans"])),
            "everything after the hosted command belongs to it"
        );
    }

    // --- the platform floor -------------------------------------------------

    /// The transcribed `proc_bsdinfo` layout, cross-checked against `ps`. A
    /// wrong field order or width here is silent memory corruption, not a
    /// compile error; three independent fields agreeing with an outside source
    /// is what says the transcription is right.
    #[cfg(all(unix, target_os = "macos"))]
    #[test]
    fn layout_of_proc_bsdinfo_is_right() {
        let me = std::process::id();
        let (ppid, pgid, start) = proc_ids(me).expect("proc_pidinfo answers for ourselves");
        let out = std::process::Command::new("/bin/ps")
            .args(["-o", "ppid=,pgid=", "-p", &me.to_string()])
            .output()
            .expect("ps");
        let text = String::from_utf8_lossy(&out.stdout);
        let mut it = text.split_whitespace();
        let ps_ppid: u32 = it.next().unwrap().parse().unwrap();
        let ps_pgid: u32 = it.next().unwrap().parse().unwrap();
        assert_eq!(ppid, ps_ppid, "pbi_ppid disagrees with ps");
        assert_eq!(pgid, ps_pgid, "pbi_pgid disagrees with ps");
        assert!(
            start > 1_600_000_000_000_000,
            "start token should be microseconds since the epoch, got {start}"
        );
        assert_eq!(proc_ids(0), None, "pid 0 is not a process");
    }

    /// The anti-pid-reuse bind, end to end on a real process: a stamp is only
    /// honoured while the pid it names is the process that was stamped.
    #[cfg(unix)]
    #[test]
    fn a_stamp_is_bound_to_the_process_it_named() {
        let dir = crate::reap::registry_dir();
        let me = std::process::id();
        let path = dir.join(format!("amux-pane-{me}.stamp"));
        let real = start_token(me).expect("our own start token");
        let k = key(999_001, 7);

        std::fs::write(&path, format!("session={}\nstarted={real}\n", k.encode())).unwrap();
        assert_eq!(
            read_stamp(&path),
            Some((me, k.clone())),
            "a stamp naming this very process is honoured"
        );

        // Same pid, a start instant that is not ours: a recycled pid. The stamp
        // must be ignored, and pruning must remove it.
        std::fs::write(
            &path,
            format!("session={}\nstarted={}\n", k.encode(), real + 1),
        )
        .unwrap();
        assert_eq!(read_stamp(&path), None, "a recycled pid must not match");
        prune_stamps();
        assert!(!path.exists(), "a stamp describing nothing must be pruned");
    }

    // --- end to end, against real processes ---------------------------------

    /// A session key naming a process that is definitively gone. Its start token
    /// is read while it is still in the table, so the key is a real one and not
    /// a fabricated pair — the point is to exercise `classify_owner` against the
    /// live system, not to hand it a number it cannot check.
    #[cfg(unix)]
    fn dead_owner_key() -> SessionKey {
        let mut c = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let owner = c.id();
        let started = start_token(owner).expect("start token while it is still listed");
        let _ = c.wait(); // reaped: the pid is now free and the owner is gone
        SessionKey { owner, started }
    }

    /// Start a real session leader (`setsid`, so pgid == pid — the same shape
    /// `pty` gives every pane), optionally carrying the marker in its
    /// environment.
    #[cfg(unix)]
    fn spawn_leader(cmd: &[&str], env: Option<&SessionKey>) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new(cmd[0]);
        c.args(&cmd[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(k) = env {
            c.env(ENV_SESSION, k.encode());
        }
        // SAFETY: `setsid` is async-signal-safe and touches no allocator or lock
        // — the only thing a `pre_exec` closure is allowed to do. This is exactly
        // what `pty` does for every pane.
        unsafe {
            c.pre_exec(|| {
                extern "C" {
                    fn setsid() -> i32;
                }
                setsid();
                Ok(())
            })
        };
        c.spawn().expect("spawn leader")
    }

    #[cfg(unix)]
    fn gone_within(pid: u32, ms: u64) -> bool {
        for _ in 0..(ms / 50) {
            if !crate::reap::pid_alive(pid) || !crate::reap::pid_running(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    /// The incident, reproduced and then cleaned up: a `/bin/sh` pane whose amux
    /// is gone. macOS will not show a SIP binary's environment to `ps`, so this
    /// pane is invisible to the env marker and is found through its stamp alone.
    #[cfg(unix)]
    #[test]
    fn a_shell_pane_whose_owner_is_gone_is_collected() {
        let key = dead_owner_key();
        let mut pane = spawn_leader(&["/bin/sh", "-c", "sleep 30"], None);
        let pid = pane.id();
        // Stamp it exactly as `spawn_pane_full` does, but for a session whose
        // owner is already dead.
        let started = start_token(pid).expect("the pane's own start token");
        std::fs::write(
            stamp_path(pid),
            format!("session={}\nstarted={started}\n", key.encode()),
        )
        .expect("write stamp");

        let victims = sweep(Some(&key));
        assert_eq!(
            victims.iter().map(|v| v.pid).collect::<Vec<_>>(),
            vec![pid],
            "the sweep must collect exactly the orphaned pane"
        );
        assert!(gone_within(pid, 2_000), "the pane must actually be dead");
        assert!(
            !stamp_path(pid).exists(),
            "a collected pane's stamp must not be left behind"
        );
        let _ = pane.kill();
        let _ = pane.wait();
    }

    /// The incident's actual shape, end to end: the owner is a **corpse**, not a
    /// reaped pid.
    ///
    /// `kill(owner, 0)` still succeeds on a zombie, which is why this repo has
    /// shipped the same bug three times and why the sweep must ask
    /// `reap::pid_running`. Swap `pid_running` for `pid_alive` in `live_owner`
    /// and this test fails with the pane still running: the corpse reads as a
    /// live owner and its panes are protected forever — exactly the population
    /// (`?Es (amux)` with live `sh` children) that had to be cleared by hand.
    #[cfg(unix)]
    #[test]
    fn a_pane_of_a_zombie_owner_is_collected() {
        let mut owner = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let opid = owner.id();
        let started = start_token(opid).expect("start token while it is still listed");
        // Deliberately NOT reaped: it is a corpse that still answers kill(pid, 0).
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(
            crate::reap::pid_alive(opid),
            "the owner must be a zombie for this test to mean anything"
        );
        let key = SessionKey {
            owner: opid,
            started,
        };

        let mut pane = spawn_leader(&["/bin/sh", "-c", "sleep 30"], None);
        let pid = pane.id();
        let pane_started = start_token(pid).expect("the pane's own start token");
        std::fs::write(
            stamp_path(pid),
            format!("session={}\nstarted={pane_started}\n", key.encode()),
        )
        .expect("write stamp");

        let victims = sweep(Some(&key));
        assert_eq!(
            victims.iter().map(|v| v.pid).collect::<Vec<_>>(),
            vec![pid],
            "a corpse owner's panes are orphans"
        );
        assert!(gone_within(pid, 2_000), "the pane must actually be dead");
        let _ = pane.kill();
        let _ = pane.wait();
        let _ = owner.wait();
    }

    /// The same thing through the OTHER marker, with no stamp file at all: the
    /// process carries `AMUX_SESSION` in its own environment and is found by
    /// reading the process table. Uses this test binary as the sleeper because
    /// it is an ordinary (non-platform) binary, which on macOS is the condition
    /// for `ps -E` disclosing an environment at all.
    #[cfg(unix)]
    #[test]
    fn an_env_marked_process_whose_owner_is_gone_is_collected() {
        let key = dead_owner_key();
        let exe = std::env::current_exe().expect("test binary path");
        let exe = exe.to_str().expect("utf-8 path").to_string();
        let mut pane = spawn_leader(
            &[
                &exe,
                "--ignored",
                "--exact",
                "orphan::tests::sleeper_for_the_sweep_tests",
            ],
            Some(&key),
        );
        let pid = pane.id();
        assert!(
            !stamp_path(pid).exists(),
            "this case must be carried by the environment alone"
        );
        // Give it a moment to exec, or `ps` reads the forked-but-not-exec'd image.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let found = snapshot()
            .into_iter()
            .find(|p| p.pid == pid)
            .expect("the marker must be readable from the process table");
        assert_eq!(found.via, Marker::Env);
        assert_eq!(found.key.as_ref(), Some(&key));

        let victims = sweep(Some(&key));
        assert_eq!(victims.iter().map(|v| v.pid).collect::<Vec<_>>(), vec![pid]);
        assert!(gone_within(pid, 2_000), "the pane must actually be dead");
        let _ = pane.kill();
        let _ = pane.wait();
    }

    /// The fail-safe direction, proved against live processes rather than a
    /// fixture: a pane whose owner is still running is left strictly alone, even
    /// though it is marked and is a session leader. This is the assertion that
    /// has to hold for the sweep to be safe to ship, and it is the one an
    /// over-eager rule breaks first.
    #[cfg(unix)]
    #[test]
    fn a_pane_of_a_live_owner_is_never_collected() {
        // This very test process stands in for the live amux: it is running, and
        // the key names it with its real start token.
        let key = session_key().expect("unix supplies a start token");
        let mut pane = spawn_leader(&["/bin/sh", "-c", "sleep 30"], Some(&key));
        let pid = pane.id();
        let started = start_token(pid).expect("the pane's own start token");
        std::fs::write(
            stamp_path(pid),
            format!("session={}\nstarted={started}\n", key.encode()),
        )
        .expect("write stamp");

        let victims = sweep(Some(&key));
        assert!(
            victims.is_empty(),
            "a live owner's panes must never be collected, got {victims:?}"
        );
        assert!(
            crate::reap::pid_running(pid),
            "the pane must still be running after the sweep"
        );
        unstamp(pid);
        let _ = pane.kill();
        let _ = pane.wait();
        // killpg too: `sh -c` forked a `sleep` into the same group.
        crate::reap::kill_tree(pid);
    }

    /// Not a test: a long-running body the two end-to-end tests re-exec this
    /// binary into, so they have a sleeper that is an ordinary binary (and so has
    /// a readable environment on macOS). `#[ignore]` keeps it out of the suite;
    /// it is only ever reached through `--ignored --exact`.
    #[cfg(unix)]
    #[test]
    #[ignore = "helper process for the orphan sweep tests, not a test"]
    fn sleeper_for_the_sweep_tests() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    /// Zombie awareness, one layer down: `killpg` is what `tree_alive` asks, and
    /// the teardown's "did anything survive?" check depends on its answer for a
    /// group that contains only a corpse. Measured here rather than assumed.
    #[cfg(unix)]
    #[test]
    fn killpg_against_a_zombie_only_group_reports_not_alive() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(
            crate::reap::pid_alive(pid),
            "a zombie still answers kill(0)"
        );
        assert!(
            !crate::reap::pid_running(pid),
            "but it is not running, which is what the sweep must use"
        );
        assert!(
            !crate::reap::tree_alive(pid),
            "killpg against a zombie-only group must not report a survivor"
        );
        let _ = child.wait();
    }
}
