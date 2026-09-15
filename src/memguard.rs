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
//! Windows only: unix has no Job Object, and its closest equivalent (a cgroup) is
//! not available to an unprivileged process everywhere. On unix the guard is
//! inert and says so in the fleet banner.

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

/// One process in the session job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub pid: u32,
    /// Private committed bytes.
    pub private: u64,
    /// Image path or name.
    pub image: String,
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

/// The cap as the fleet banner states it, e.g. `memory guard on`. Unsupported
/// platforms say so rather than claiming a guard that isn't there.
pub fn describe(cap: Cap, supported: bool) -> String {
    match (supported, cap) {
        (false, _) => "no memory guard on this platform".to_string(),
        (true, Cap::Off) => "memory guard OFF".to_string(),
        (true, Cap::Dynamic) => "memory guard on".to_string(),
        (true, Cap::Fixed(bytes)) => format!("memory capped at {}", show_bytes(bytes)),
    }
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

/// Whether this platform can enforce the guard.
pub fn supported() -> bool {
    cfg!(windows)
}

/// The guard's memory between ticks.
#[derive(Default)]
pub struct Guard {
    /// The limit last set on the job, so an unchanged limit is not re-set.
    applied: Option<Option<u64>>,
    /// Pressure with nothing to stop has been reported this streak.
    stuck_reported: bool,
}

impl Guard {
    pub fn new() -> Guard {
        Guard::default()
    }

    /// Recompute and apply the limit, and relieve pressure. Returns a message for
    /// the operator when the guard acted or can't; `None` when all is well.
    pub fn tick(&mut self, job: &crate::reap::SessionJob) -> Option<String> {
        let cap = session_cap();
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
fn members(job: &crate::reap::SessionJob) -> Option<Vec<Member>> {
    Some(
        job.pids()?
            .into_iter()
            .filter_map(|pid| {
                Some(Member {
                    pid,
                    private: crate::reap::private_bytes(pid)?,
                    image: crate::reap::image_name(pid).unwrap_or_default(),
                })
            })
            .collect(),
    )
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
            },
            Member {
                pid: 2,
                private: 6 * GIB,
                image: r"C:\VS\link.exe".into(),
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
        assert_eq!(describe(Cap::Dynamic, true), "memory guard on");
        assert_eq!(
            describe(Cap::Fixed(32 * GIB), true),
            "memory capped at 32.0 GiB"
        );
        assert_eq!(describe(Cap::Off, true), "memory guard OFF");
        for cap in [Cap::Dynamic, Cap::Fixed(GIB), Cap::Off] {
            assert_eq!(describe(cap, false), "no memory guard on this platform");
        }
    }

    #[test]
    fn bytes_read_naturally() {
        assert_eq!(show_bytes(512 * MIB), "512 MiB");
        assert_eq!(show_bytes(GIB + GIB / 10), "1.1 GiB");
    }
}
