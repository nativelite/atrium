//! A hard memory cap for the panes on Linux, when atrium owns its cgroup.
//!
//! The soft guard ([`crate::memguard`]) watches and stops builds, but nothing
//! stops an allocation between its ticks. A cgroup v2 `memory.max` does: the
//! kernel refuses the panes' memory past it, reclaims and throttles first at
//! `memory.high`, and if that isn't enough its cgroup OOM killer picks a victim
//! *inside the cgroup* — where the soft guard has already raised every build's
//! `oom_score_adj`, so the victim is a build.
//!
//! **This needs a cgroup atrium owns.** An unprivileged process may only move
//! processes within a subtree it can write, and cgroup v2 forbids processes in
//! a cgroup that delegates controllers to children. A shell in a terminal
//! usually sits in a cgroup shared with its siblings that it doesn't own, so the
//! cap is only taken when atrium's own cgroup is delegated to it and holds no
//! process but atrium — what `systemd-run --user --scope -p Delegate=yes atrium …`
//! gives it. Then atrium splits it into two leaves:
//!
//! ```text
//! <atrium's scope>/            memory controller enabled for children
//!   atrium/   atrium itself    (uncapped: the host must survive its panes)
//!   panes/    every pane tree  memory.high + memory.max
//! ```
//!
//! Anything short of that and [`PaneCgroup::establish`] changes nothing and
//! returns `None`, leaving the soft guard in charge.

use std::path::{Path, PathBuf};

/// Where the unified (v2) hierarchy is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The unified-hierarchy path in a `/proc/<pid>/cgroup` text: the `0::<path>`
/// line. `None` on a v1-only or hybrid host without one.
pub fn unified_path(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::trim)
        .filter(|p| p.starts_with('/'))
}

/// One line on stderr under `ATRIUM_DEBUG` (the TUI owns the screen otherwise),
/// for a cgroup step that failed in a way worth knowing about.
fn debug_warn(what: &str) {
    if std::env::var_os("ATRIUM_DEBUG").is_some() {
        eprint!("[atrium-dbg cgroup] {what}\r\n");
    }
}

/// Whether this session already holds a capped cgroup — without taking one.
pub fn held() -> bool {
    SESSION.get().is_some_and(Option::is_some)
}

/// Whether a `cgroup.controllers` text offers the memory controller.
pub fn has_memory_controller(text: &str) -> bool {
    text.split_whitespace().any(|c| c == "memory")
}

/// Whether a `cgroup.procs` text holds exactly `pid` and nothing else.
pub fn only_process(text: &str, pid: u32) -> bool {
    let mut procs = text.split_whitespace();
    procs.next() == Some(&pid.to_string()) && procs.next().is_none()
}

/// Whether [`PaneCgroup::establish`] would succeed here, without changing
/// anything: the fleet banner uses it to say "hard" or "soft" before a pane
/// exists. Checks the same conditions — memory controller, atrium alone in its
/// cgroup — plus that this uid owns the directory.
#[cfg(target_os = "linux")]
pub fn can_establish() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(own) = std::fs::read_to_string("/proc/self/cgroup").ok() else {
        return false;
    };
    let Some(path) = unified_path(&own) else {
        return false;
    };
    let dir = Path::new(CGROUP_ROOT).join(path.trim_start_matches('/'));
    let owned = matches!(
        (std::fs::metadata(&dir), std::fs::metadata("/proc/self")),
        (Ok(d), Ok(me)) if d.uid() == me.uid()
    );
    owned
        && std::fs::read_to_string(dir.join("cgroup.controllers"))
            .is_ok_and(|t| has_memory_controller(&t))
        && std::fs::read_to_string(dir.join("cgroup.procs"))
            .is_ok_and(|t| only_process(&t, std::process::id()))
}

#[cfg(not(target_os = "linux"))]
pub fn can_establish() -> bool {
    false
}

/// The session's capped leaf, if it has one.
static SESSION: std::sync::OnceLock<Option<PaneCgroup>> = std::sync::OnceLock::new();

/// The session's capped cgroup, taken on first use — which must come before the
/// first pane spawns: a pane inherits atrium's cgroup, and once it is there
/// atrium is no longer alone in it and can't take it (measured: the initial
/// pane spawned before the session job existed, and the cap never engaged).
/// `None` when the memory guard is off or atrium doesn't own its cgroup.
#[cfg(target_os = "linux")]
pub fn session() -> Option<&'static PaneCgroup> {
    SESSION
        .get_or_init(|| {
            (crate::memguard::session_cap() != crate::memguard::Cap::Off)
                .then(PaneCgroup::establish)
                .flatten()
        })
        .as_ref()
}

#[cfg(not(target_os = "linux"))]
pub fn session() -> Option<&'static PaneCgroup> {
    None
}

/// The panes' leaf of a cgroup atrium owns.
#[derive(Debug)]
pub struct PaneCgroup {
    panes: PathBuf,
}

impl PaneCgroup {
    /// Take the hard cap if atrium owns its cgroup (see the module docs), or
    /// change nothing and return `None`.
    pub fn establish() -> Option<PaneCgroup> {
        let me = std::process::id();
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let dir = Path::new(CGROUP_ROOT).join(unified_path(&own)?.trim_start_matches('/'));
        Self::establish_in(&dir, me)
    }

    /// [`establish`](Self::establish) against an explicit cgroup directory.
    fn establish_in(dir: &Path, me: u32) -> Option<PaneCgroup> {
        let controllers = std::fs::read_to_string(dir.join("cgroup.controllers")).ok()?;
        if !has_memory_controller(&controllers) {
            return None;
        }
        // Anyone else in here is not ours to move, and would block enabling
        // controllers for children.
        if !only_process(&std::fs::read_to_string(dir.join("cgroup.procs")).ok()?, me) {
            return None;
        }
        let host = dir.join("atrium");
        let panes = dir.join("panes");
        let undo = |moved: bool| {
            // If atrium can't be moved back, "nothing changed" no longer holds:
            // it now lives in `atrium/` with no cap. Say so rather than hide it.
            if moved && std::fs::write(dir.join("cgroup.procs"), me.to_string()).is_err() {
                debug_warn("could not move atrium back after a failed cgroup setup");
            }
            let _ = std::fs::remove_dir(&host);
            let _ = std::fs::remove_dir(&panes);
        };
        // mkdir succeeds only where we may write: this is the delegation test.
        if std::fs::create_dir(&host).is_err() {
            return None;
        }
        if std::fs::create_dir(&panes).is_err() {
            undo(false);
            return None;
        }
        if std::fs::write(host.join("cgroup.procs"), me.to_string()).is_err() {
            undo(false);
            return None;
        }
        if std::fs::write(dir.join("cgroup.subtree_control"), "+memory").is_err()
            || !panes.join("memory.max").exists()
        {
            undo(true);
            return None;
        }
        Some(PaneCgroup { panes })
    }

    /// Move a pane (and so, from here on, everything it starts) into the capped
    /// leaf. `false` if the write is refused.
    pub fn assign(&self, pid: u32) -> bool {
        pid != 0 && std::fs::write(self.panes.join("cgroup.procs"), pid.to_string()).is_ok()
    }

    /// Set `(memory.high, memory.max)` — see [`crate::memguard::cgroup_limits`]
    /// for how they are sized — or lift both with `None`. `high` is written
    /// first, so the kernel starts reclaiming before a tighter hard limit lands.
    /// Returns whether the two limits took; a swap-limit failure is surfaced
    /// under `ATRIUM_DEBUG` rather than folded in, so it can't make the caller
    /// re-apply limits the kernel already holds.
    ///
    /// Swap is closed to the panes while capped (`memory.swap.max` 0, where the
    /// kernel accounts swap). `memory.max` counts RAM only: with swap open, a
    /// pane past its limit is swapped out instead of stopped — measured in WSL,
    /// a 300 MB process in a 100 MB leaf simply ran on — and a swapping build is
    /// the machine-freezing failure this cap exists to prevent.
    ///
    /// Residual, stated plainly: past `memory.max` the kernel's cgroup OOM
    /// killer chooses the victim, preferring builds the guard has marked with a
    /// raised `oom_score_adj` — but a build started since the last 3 s tick
    /// isn't marked yet, and then the kernel's own choice (largest first) could
    /// be an agent. `memory.high` throttling the panes before `memory.max` makes
    /// that gap hard to reach, not impossible.
    pub fn set_limits(&self, limits: Option<(u64, u64)>) -> bool {
        let (high, max, swap) = match limits {
            Some((high, max)) => (high.to_string(), max.to_string(), "0"),
            None => ("max".to_string(), "max".to_string(), "max"),
        };
        let high_ok = std::fs::write(self.panes.join("memory.high"), &high).is_ok();
        let max_ok = std::fs::write(self.panes.join("memory.max"), &max).is_ok();
        let swap_file = self.panes.join("memory.swap.max");
        if swap_file.exists() && std::fs::write(&swap_file, swap).is_err() {
            debug_warn("memory.swap.max write refused; capped panes may swap");
        }
        high_ok && max_ok
    }

    /// The panes' current memory use, as the kernel accounts it for the cap.
    pub fn usage(&self) -> Option<u64> {
        std::fs::read_to_string(self.panes.join("memory.current"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The capped leaf's directory.
    pub fn dir(&self) -> &Path {
        &self.panes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unified_path_is_the_zero_line() {
        assert_eq!(
            unified_path("0::/user.slice/app.scope\n"),
            Some("/user.slice/app.scope")
        );
        // Hybrid hosts list v1 controllers first.
        assert_eq!(
            unified_path("12:memory:/x\n1:name=systemd:/y\n0::/init.scope\n"),
            Some("/init.scope")
        );
        assert_eq!(unified_path("12:memory:/x\n"), None);
        assert_eq!(unified_path(""), None);
    }

    /// The undo path, without privilege: a plain directory dressed as a
    /// delegated cgroup passes every check but never grows the kernel's
    /// `memory.max`, so setup must fail, return `None`, and remove the leaf.
    #[test]
    fn a_setup_that_cannot_finish_is_undone() {
        let me = std::process::id();
        let dir = std::env::temp_dir().join(format!("atrium-cgroup-undo-{me}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        std::fs::write(dir.join("cgroup.procs"), format!("{me}\n")).unwrap();
        let got = PaneCgroup::establish_in(&dir, me);
        let panes_left = dir.join("panes").exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            got.is_none(),
            "a setup that can't finish must not report a cap"
        );
        assert!(!panes_left, "the panes leaf must be removed again");
    }

    #[test]
    fn delegation_needs_memory_and_an_empty_cgroup_but_us() {
        assert!(has_memory_controller("cpu memory pids"));
        assert!(!has_memory_controller("cpu pids"));
        assert!(only_process("4242\n", 4242));
        assert!(
            !only_process("4242\n4243\n", 4242),
            "a sibling isn't ours to move"
        );
        assert!(!only_process("4243\n", 4242));
        assert!(!only_process("", 4242));
    }

    /// Outside a cgroup atrium owns, nothing is touched. A test binary under
    /// cargo shares its cgroup with cargo, so this is the normal case here.
    #[cfg(target_os = "linux")]
    #[test]
    fn without_ownership_establish_changes_nothing() {
        let own = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let Some(path) = unified_path(&own) else {
            return; // a v1-only host: nothing to establish against
        };
        let dir = Path::new(CGROUP_ROOT).join(path.trim_start_matches('/'));
        let procs = std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default();
        if only_process(&procs, std::process::id()) {
            return; // genuinely delegated: the ignored live test covers it
        }
        let had_panes = dir.join("panes").exists();
        assert!(PaneCgroup::establish().is_none());
        assert_eq!(
            dir.join("panes").exists(),
            had_panes,
            "nothing may be created"
        );
    }

    /// Live, and it rearranges this process's cgroup, so it only runs when asked
    /// and only inside a delegated scope:
    /// `systemd-run --user --scope -p Delegate=yes cargo test --lib cgroup -- --ignored`
    /// (cargo and the test binary then share the scope, so run the test binary
    /// directly under systemd-run instead; see docs/fleets.md).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a delegated cgroup: run the test binary under systemd-run --user --scope -p Delegate=yes"]
    fn a_delegated_scope_caps_a_pane_hard() {
        let cg = PaneCgroup::establish().expect("establish in a delegated scope");
        // A child that tries to hold 300 MB inside a 100 MB leaf.
        let mut child = std::process::Command::new("/usr/bin/python3")
            .args([
                "-c",
                "import time; time.sleep(0.5); b = b'x' * (300 * 1024 * 1024); time.sleep(30)",
            ])
            .spawn()
            .expect("spawn python3");
        assert!(cg.assign(child.id()), "assign");
        assert!(
            cg.set_limits(Some((90 * 1024 * 1024, 100 * 1024 * 1024))),
            "set limits"
        );
        // Regression guard for the line that makes the cap real: with swap open
        // the process just swaps and runs on.
        let swap = std::fs::read_to_string(cg.dir().join("memory.swap.max")).unwrap_or_default();
        let status = child.wait().expect("wait");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(swap.trim(), "0", "swap must be closed to capped panes");
        assert_eq!(
            status.signal(),
            Some(9),
            "the cgroup OOM killer must stop it"
        );
        assert!(cg.set_limits(None), "lift");
    }
}
