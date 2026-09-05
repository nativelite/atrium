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
            if self.known_sessions.insert(pid) {
                out.push(Alert {
                    kind: "warden-session-appeared",
                    detail: format!("another amux session (pid {pid}) started alongside this one"),
                });
            }
        }
        out
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
    let me = std::process::id();
    let mut cur = parent_of(me)?;
    // Bounded: a pathological or looping tree must not hang startup.
    for _ in 0..64 {
        if cur <= 1 {
            return None;
        }
        if is_amux(cur) {
            return Some(cur);
        }
        cur = parent_of(cur)?;
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

#[cfg(unix)]
fn parent_of(pid: u32) -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(unix)]
fn is_amux(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()
        .map(|o| {
            let comm = String::from_utf8_lossy(&o.stdout);
            Path::new(comm.trim())
                .file_name()
                .map(|n| n == "amux")
                .unwrap_or(false)
        })
        .unwrap_or(false)
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
