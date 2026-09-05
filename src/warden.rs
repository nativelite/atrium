//! The warden: make evasion visible, since it cannot be made impossible.
//!
//! amux is **not a sandbox**, and this module exists because pretending
//! otherwise would be the more dangerous mistake. amux sets a permission
//! posture; claude enforces it. An agent that can execute commands can run
//! `claude --dangerously-skip-permissions` directly, with no amux involved — so
//! "stop an agent escaping through amux" is a subset of "the agent can run
//! anything", which amux has no power to solve.
//!
//! What amux *can* do is refuse the easy path and notice the hard one.
//!
//! # Why the cap hangs off ancestry, not the environment
//!
//! The obvious design — mark every pane with `AMUX_AGENT=1`, or inherit the
//! session policy through an env var, and have amux refuse to elevate when it
//! sees the marker — does not work. The agent owns its own environment:
//! `env -u AMUX_AGENT amux --trust skip` defeats it in one command. That is the
//! same bug class as a privilege gate that grants authority when a token is
//! *absent*: it trusts the untrusted party to leave evidence in place.
//!
//! A launch confirmation fares no better. `--trust skip` does prompt, but an
//! agent spawning amux owns that child's stdin and can answer its own prompt.
//!
//! Process ancestry is the one signal with teeth: a process cannot unset its own
//! parent. It is still escapable — double-fork and you are reparented to init,
//! losing the trail — but that is a deliberate, unmistakable act rather than an
//! accident or a one-flag shortcut. So: cap on ancestry, and let the warden watch
//! for the deliberate case.
//!
//! # What the warden watches
//!
//! - **amux's own binary.** The sharpest attack is not evading the restrictions
//!   but *removing* them: edit amux, rebuild, and the ceiling is whatever you
//!   say it is. A digest taken at startup catches that.
//! - **The session registry.** It is what the ancestry cap reads to learn the
//!   parent's policy, so tampering with it is tampering with the ceiling.
//! - **Unaccounted sessions.** An amux appearing that our ancestry check did not
//!   explain is the double-fork case above.
//!
//! None of these are gates. They are tripwires, and they report through the
//! machinery that already exists: the audit log, a `decision_needed` on the bus,
//! and the status bar. If you want true isolation, that is an OS-level boundary —
//! a separate uid per agent, or a container — not something amux can provide.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a newly-seen descendant has to declare that it accepted our ceiling
/// before the warden treats it as an escapee. A legitimately nested amux writes
/// that record on its first loop tick; this is slack for the race, not patience
/// with evasion.
const ESCAPEE_GRACE: Duration = Duration::from_secs(5);

/// Does this pid still exist? A session that has already gone needs neither an
/// alert nor a kill.
fn pid_exists(pid: u32) -> bool {
    crate::reap::pid_alive(pid)
}

/// Something the warden noticed. Never fatal on its own — the operator decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// Short machine-ish kind, used as the audit action and the bus field.
    pub kind: &'static str,
    /// Human-readable detail naming what changed.
    pub detail: String,
}

/// A cheap content digest.
///
/// FNV-1a over the bytes: not a cryptographic hash, and deliberately not
/// presented as one. It answers "did this file change since we looked", which is
/// the question here. An adversary who can rewrite the binary can also compute a
/// collision against any unkeyed digest, so a stronger function would buy
/// confidence it has not earned — the honest defense against that is the
/// operator seeing the alert, not the hash being SHA.
pub fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn digest_file(path: &Path) -> Option<u64> {
    std::fs::read(path).ok().map(|b| digest(&b))
}

/// Snapshots taken at startup, compared on every check.
pub struct Warden {
    exe: PathBuf,
    exe_digest: Option<u64>,
    registry: PathBuf,
    registry_digest: Option<u64>,
    /// Session registries present when we started, plus our own — anything new
    /// is a session that appeared alongside us.
    known_sessions: HashSet<u32>,
    /// New sessions that are descendants of ours: the escape the ancestry cap
    /// could not catch, and the only thing enforcement acts on.
    escapees: HashSet<u32>,
    /// Descendants seen but not yet judged - see `ESCAPEE_GRACE`.
    pending: std::collections::HashMap<u32, Instant>,
}

impl Warden {
    /// Snapshot the world as amux starts.
    pub fn new(registry: PathBuf) -> Warden {
        let exe = std::env::current_exe().unwrap_or_default();
        let exe_digest = digest_file(&exe);
        let mut known_sessions = session_pids();
        known_sessions.insert(std::process::id());
        Warden {
            exe,
            exe_digest,
            registry_digest: digest_file(&registry),
            registry,
            known_sessions,
            escapees: HashSet::new(),
            pending: std::collections::HashMap::new(),
        }
    }

    /// Re-read what we snapshotted. Returns one alert per change, and updates the
    /// baseline so a single event is reported once rather than every tick.
    pub fn check(&mut self) -> Vec<Alert> {
        let mut out = Vec::new();

        if let Some(before) = self.exe_digest {
            match digest_file(&self.exe) {
                Some(now) if now != before => {
                    self.exe_digest = Some(now);
                    out.push(Alert {
                        kind: "warden-binary-changed",
                        detail: format!(
                            "amux's own binary changed while running ({}); the \
                             permission ceiling is only as trustworthy as this file",
                            self.exe.display()
                        ),
                    });
                }
                None => {
                    self.exe_digest = None;
                    out.push(Alert {
                        kind: "warden-binary-gone",
                        detail: format!("amux's own binary is unreadable ({})", self.exe.display()),
                    });
                }
                _ => {}
            }
        }

        // The registry is what the ancestry cap reads to learn a parent's policy,
        // so editing it is editing the ceiling. amux rewrites it only when its
        // pane set changes, and updates the baseline then — a change we did not
        // make is what shows up here.
        if let Some(now) = digest_file(&self.registry) {
            if self.registry_digest.is_some_and(|before| before != now) {
                self.registry_digest = Some(now);
                out.push(Alert {
                    kind: "warden-registry-changed",
                    detail: format!(
                        "the session registry changed unexpectedly ({})",
                        self.registry.display()
                    ),
                });
            }
        }

        for pid in session_pids() {
            if !self.known_sessions.insert(pid) {
                continue;
            }
            // Only a DESCENDANT is interesting. Any new amux registry used to
            // alert here, which meant opening amux in a second terminal fired a
            // decision at the first - normal use reported as an intrusion. A
            // tripwire that cries at legitimate behaviour is worse than none,
            // because you learn to dismiss it.
            //
            // A session nested under us is what the ancestry cap exists for; one
            // that is nested but did NOT get capped is the double-fork escape,
            // which is exactly what is worth interrupting for.
            if !is_descendant_of(pid, std::process::id()) {
                continue;
            }
            // A descendant is not yet an escapee. A legitimately nested amux caps
            // itself against our policy and records `capped_by=<us>`; one that
            // double-forked to evade never does. Judging on descent ALONE would
            // condemn the well-behaved case — and under enforcement, kill it.
            //
            // The record is written on the child's first loop tick, so a session
            // seen moments ago may simply not have got there yet. Hold it in
            // `pending` and decide after the grace period rather than racing it.
            self.pending.entry(pid).or_insert_with(Instant::now);
        }

        // Now judge anything whose grace period has expired.
        let me = std::process::id();
        let ripe: Vec<u32> = self
            .pending
            .iter()
            .filter(|(_, seen)| seen.elapsed() >= ESCAPEE_GRACE)
            .map(|(pid, _)| *pid)
            .collect();
        for pid in ripe {
            self.pending.remove(&pid);
            if crate::reap::read_capped_by(&crate::reap::registry_path(pid)) == Some(me) {
                continue; // capped against our policy: behaving exactly as intended
            }
            if !pid_exists(pid) {
                continue; // gone already; nothing to report or act on
            }
            self.escapees.insert(pid);
            out.push(Alert {
                kind: "warden-session-appeared",
                detail: format!(
                    "an amux session (pid {pid}) is running underneath this one without \
                     accepting its ceiling"
                ),
            });
        }
        out
    }

    /// Sessions judged to be uncapped descendants - the only thing enforcement
    /// will ever act on.
    pub fn escapees(&self) -> Vec<u32> {
        self.escapees.iter().copied().collect()
    }

    /// Tear down an escapee's whole process tree.
    ///
    /// Deliberately NOT automatic. The warden's judgement is a heuristic over a
    /// process table, and killing on a heuristic can destroy work the operator
    /// wanted - a legitimately nested amux, or a misread ancestry. Enforcement is
    /// opt-in via `AMUX_WARDEN=enforce`, so the default stays "tell the human"
    /// and the human keeps the decision the bus already puts in front of them.
    pub fn enforce(&mut self, pid: u32) -> bool {
        let mut acted = false;
        for p in crate::reap::read_registry(&crate::reap::registry_path(pid)) {
            crate::reap::term_tree(p);
            crate::reap::kill_tree(p);
            acted = true;
        }
        // The session process itself, not only its panes.
        crate::reap::term_tree(pid);
        crate::reap::kill_tree(pid);
        self.escapees.remove(&pid);
        acted
    }

    /// Is enforcement switched on? NO by default - `AMUX_WARDEN=enforce` opts in.
    ///
    /// It was briefly the default, on the argument that an escapee was specific
    /// enough to act on. A review showed it is not, and killing was reaching the
    /// wrong targets:
    ///
    /// - `capped_by` records only the NEAREST amux ancestor while descent is
    ///   walked up to 64 hops, so a correctly-capped GRANDCHILD looks like an
    ///   escapee to its grandparent and its whole tree dies ~5s in. No attacker,
    ///   default config, and reachable in the exact shape of a fleet whose agents
    ///   run amux.
    /// - The judgement itself rests on paths the agent owns: the registry is
    ///   addressed via `$TMPDIR` and the ancestry walk shells out to a
    ///   `$PATH`-resolved `ps`, both failing open.
    ///
    /// Killing on a heuristic is only defensible when the heuristic is sound.
    /// This one is not yet, so the default returns to telling the human, who
    /// already has the decision on the bus.
    pub fn enforcing() -> bool {
        std::env::var("AMUX_WARDEN")
            .map(|v| v.eq_ignore_ascii_case("enforce"))
            .unwrap_or(false)
    }

    /// Called after amux rewrites its own registry, so a legitimate write is not
    /// reported as tampering.
    pub fn registry_rewritten(&mut self) {
        self.registry_digest = digest_file(&self.registry);
    }
}

/// Pids of every amux session that has a registry file on this host.
fn session_pids() -> HashSet<u32> {
    let mut out = HashSet::new();
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return out;
    };
    for e in entries.flatten() {
        if let Some(pid) = e
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("amux-session-"))
            .and_then(|n| n.strip_suffix(".pids"))
            .and_then(|n| n.parse::<u32>().ok())
        {
            out.insert(pid);
        }
    }
    out
}

/// The pid of the nearest ancestor process that is itself an amux, if any.
///
/// This is the cap's foundation: a process cannot unset its own parent, so
/// unlike an environment marker it cannot be shrugged off with `env -u`. It is
/// not unbreakable — a double-fork orphans the child onto `init` and erases the
/// trail — but doing so is deliberate and visible, which is what the warden's
/// unaccounted-session check is for.
#[cfg(unix)]
pub fn amux_ancestor() -> Option<u32> {
    let table = proc_table();
    let mut cur = std::process::id();
    // Check each PARENT's name, never our own — the first version compared the
    // current entry's name while carrying the parent's pid, so amux matched
    // itself on the first hop and reported its own parent as the ancestor.
    for _ in 0..64 {
        let (ppid, _) = *table.get(&cur)?;
        if ppid <= 1 {
            return None;
        }
        let (_, parent_name) = table.get(&ppid)?;
        if parent_name == "amux" {
            return Some(ppid);
        }
        cur = ppid;
    }
    None
}

#[cfg(not(unix))]
pub fn amux_ancestor() -> Option<u32> {
    // Windows has no cheap ppid walk without more FFI than this is worth yet;
    // the Job Object already bounds a pane's tree there. Reported as "no
    // ancestor" rather than pretending to have checked.
    None
}

/// One snapshot of the process table: pid -> (ppid, command name).
///
/// Taken with a SINGLE `ps` call. The first version walked ancestry with one
/// `ps -p <pid>` per hop — up to 64 subprocesses per lookup, per new session,
/// every few seconds. That is a process storm, and it showed up immediately as
/// timing flakiness in the test suite, which runs many amux instances at once. A
/// watchdog that loads the machine it is watching is a bad watchdog.
#[cfg(unix)]
fn proc_table() -> std::collections::HashMap<u32, (u32, String)> {
    let mut out = std::collections::HashMap::new();
    let Ok(o) = std::process::Command::new("ps")
        .args(["-eo", "pid=,ppid=,comm="])
        .output()
    else {
        return out;
    };
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        let comm = it.next().unwrap_or_default();
        let name = Path::new(comm)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        out.insert(pid, (ppid, name));
    }
    out
}

/// Is `pid` a descendant of `ancestor`? Tells an amux nested under us (the escape
/// worth interrupting for) from one started in another terminal (normal, and not
/// the warden's business).
#[cfg(unix)]
fn is_descendant_of(pid: u32, ancestor: u32) -> bool {
    let table = proc_table();
    let mut cur = pid;
    for _ in 0..64 {
        let Some((ppid, _)) = table.get(&cur) else {
            return false;
        };
        if *ppid == ancestor {
            return true;
        }
        if *ppid <= 1 {
            return false;
        }
        cur = *ppid;
    }
    false
}

#[cfg(not(unix))]
fn is_descendant_of(_pid: u32, _ancestor: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_notices_any_change() {
        assert_eq!(digest(b"abc"), digest(b"abc"));
        assert_ne!(digest(b"abc"), digest(b"abd"));
        assert_ne!(digest(b""), digest(b"a"));
    }

    /// The binary check is the one that matters most: the sharpest attack is not
    /// evading the ceiling but editing amux so there is no ceiling to evade.
    #[test]
    fn a_changed_file_is_reported_once_not_every_tick() {
        let dir = std::env::temp_dir().join(format!("amux-warden-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry");
        std::fs::write(&reg, "1234\n").unwrap();

        let mut w = Warden::new(reg.clone());
        assert!(
            w.check()
                .iter()
                .all(|a| a.kind != "warden-registry-changed"),
            "an untouched registry must not alert"
        );

        std::fs::write(&reg, "1234\n5678\n").unwrap();
        let alerts = w.check();
        assert!(
            alerts.iter().any(|a| a.kind == "warden-registry-changed"),
            "a tampered registry must alert: {alerts:?}"
        );
        // Baseline moves, so the same change is not re-reported forever — an
        // alert that repeats every tick trains the operator to ignore it.
        assert!(
            w.check()
                .iter()
                .all(|a| a.kind != "warden-registry-changed"),
            "the same change was reported twice"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A legitimate rewrite by amux itself is not tampering.
    #[test]
    fn our_own_registry_rewrite_is_not_an_alert() {
        let dir = std::env::temp_dir().join(format!("amux-warden-own-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry");
        std::fs::write(&reg, "1\n").unwrap();
        let mut w = Warden::new(reg.clone());
        std::fs::write(&reg, "1\n2\n").unwrap();
        w.registry_rewritten();
        assert!(
            w.check()
                .iter()
                .all(|a| a.kind != "warden-registry-changed"),
            "amux's own write was reported as tampering"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On this host amux is not launched from inside another amux, so the cap
    /// must not fire. (The positive case needs a real nested launch and is
    /// covered by the integration test.)
    #[cfg(unix)]
    #[test]
    fn a_normal_launch_has_no_amux_ancestor() {
        assert_eq!(amux_ancestor(), None);
    }
}
