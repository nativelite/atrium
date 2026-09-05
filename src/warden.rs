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
//! # The same-uid ceiling, stated once and plainly
//!
//! A pane's agent runs as the **same unix user** as amux. Every file amux can
//! read, the agent can read; every file a nested amux can write, the agent can
//! forge; and one process cannot constrain or attest another's future behaviour
//! at the same uid without an OS boundary (a second uid, a container, a Job
//! Object). So **there is no mechanism here that proves a descendant is
//! honouring the ceiling.** Four were designed and attacked before this code was
//! written — a parent-held pipe fd inherited by every descendant, a held ctl
//! connection with a kernel-stamped peer pid, session-id plus executable-inode
//! attribution, and a census of the postures actually running in our pty
//! sessions. Each is defeated by one line from a pane (`closerange`, a
//! double-fork, `setsid`, or writing `.claude/settings.local.json` and launching
//! with a clean argv), and each *added* a way to kill a correctly-behaving
//! session. None of them belongs here.
//!
//! What this module does instead is narrower and true: it establishes **descent
//! and executable identity from kernel state**, uses nothing the watched process
//! chose to write, and **reports**. It never kills.
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
//! # Identity is the kernel's, not `ps`
//!
//! An ancestry walk is only as good as its answer to "is that hop an amux?".
//! This used to be `ps -eo comm=` compared against the string `"amux"`. Measured
//! on macOS 25.6.0: `ps -o comm=` prints **argv[0]**, which the watched process
//! chooses. `(exec -a amux /bin/sleep 40)` from a pane shows up as `comm=amux`
//! with `args=amux 40` while `proc_pidpath` reports `/bin/sleep`. That is a
//! one-line ceiling bypass with no double-fork: the shim shadows the real parent,
//! the nested amux reads the *shim's* registry (which the agent also writes),
//! and inherits whatever ceiling the agent put there — or none at all.
//!
//! So a hop counts as amux only when the **kernel's** executable for that pid is
//! the same file as ours, compared by `(st_dev, st_ino)`:
//! `proc_pidpath` + `stat` on macOS, `stat("/proc/<pid>/exe")` on Linux. A fake
//! intermediate must then *be* the real amux binary — in which case it applies
//! the cap itself, which is the outcome we wanted. Two limits, both real:
//! a differently-built amux (`./target/release/amux` hosting a pane in which
//! `~/.cargo/bin/amux` is launched) is **not** recognised, so no ceiling is
//! applied — that case is now reported rather than silently ignored; and
//! rewriting the binary in place keeps the inode, which is what the startup
//! digest below exists for and is the only thing that speaks to it.
//!
//! # What the warden watches
//!
//! - **amux's own binary.** The sharpest attack is not evading the restrictions
//!   but *removing* them: edit amux, rebuild, and the ceiling is whatever you
//!   say it is. A digest taken at startup catches that.
//! - **The session registry.** It is what the ancestry cap reads to learn the
//!   parent's policy, so tampering with it — or deleting it — is tampering with
//!   the ceiling.
//! - **Nested sessions it cannot account for.** An amux inside one of our pty
//!   sessions whose ancestry does not lead back to us is the double-fork case.
//!
//! None of these are gates. They are tripwires, and they report through the
//! machinery that already exists: the audit log, a `decision_needed` on the bus,
//! and the status bar. If you want true isolation, that is an OS-level boundary —
//! a separate uid per agent, or a container — not something amux can provide.
//!
//! # Why there is no enforcement any more
//!
//! `AMUX_WARDEN=enforce` used to tear down the tree of anything judged an
//! escapee. It is gone, for two reasons that are not going to be fixed by a
//! better heuristic.
//!
//! First, under a correct ancestry rule **the enforceable set is empty by
//! construction.** Walk up from any process whose ppid chain still reaches us:
//! either we are its nearest amux ancestor — in which case it ran our cap — or
//! some intermediate amux is, in which case it is that session's business, not
//! ours. (Judging it anyway is precisely the shipped bug that SIGKILLed
//! correctly-nested grandchildren ~5s in.) The only sessions left to accuse are
//! the ones whose chain is *broken*, and a broken chain is exactly what an
//! ordinary reparenting looks like: a pane's launcher shell exiting while the
//! amux it started keeps running is indistinguishable from a deliberate
//! double-fork. Killing on that is killing on a coin flip.
//!
//! Second, the kill was steerable by the party it was aimed at. `enforce` read
//! the *suspect's own* registry — a mode-0644 file at a fixed path, named after a
//! pid the agent chooses — and `killpg`'d every group id it listed. One
//! `printf 'policy=skip\n<victim pgid>\n' > /tmp/amux-session-<some pid>.pids`
//! turned the warden into an agent-directed kill primitive. A tripwire that can
//! be pointed at a target is worse than no tripwire.

#[cfg(unix)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// How far the ancestry walk goes before giving up. A bound, not a policy: it
/// stops a corrupt or cyclic table from spinning. Exhausting it yields
/// [`Ancestry::Unknown`], never "no ancestor" — see [`walk_to_amux`].
#[cfg(unix)]
const MAX_HOPS: usize = 64;

/// Something the warden noticed. Never fatal on its own — the operator decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// Short machine-ish kind, used as the audit action and the bus field.
    pub kind: &'static str,
    /// Human-readable detail naming what changed.
    pub detail: String,
}

/// What an ancestry walk could establish. Four states, because collapsing the
/// last two into "no ancestor" is how the ceiling came to fail open: a `ps` that
/// could not be run returned an empty table, an empty table meant "no amux
/// ancestor", and "no amux ancestor" meant no ceiling at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    /// The nearest ancestor running *this* amux binary, by `(dev, ino)`.
    Amux(u32),
    /// The walk completed and there is no amux ancestor: a top-level session.
    NoneFound,
    /// The walk could not be completed — the process table was unreadable, a hop
    /// vanished, or a hop's executable could not be resolved. Callers must treat
    /// this as "unproven", never as "not nested".
    Unknown,
    /// This platform has no implementation. A stated gap, distinct from a failed
    /// attempt so it can be reported once rather than every launch.
    Unsupported,
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
    /// Session registries present when we started, plus our own, plus every one
    /// we have since reached a definite verdict on. Anything not in here is a
    /// session we have not judged yet. Only the unix descent check reads it.
    #[cfg_attr(not(unix), allow(dead_code))]
    known_sessions: HashSet<u32>,
    /// Sessions we could not judge, so the "could not establish" alert is raised
    /// once per pid rather than every tick. They stay OUT of `known_sessions` on
    /// purpose: a transient failure should resolve itself on a later tick.
    #[cfg_attr(not(unix), allow(dead_code))]
    unknown_reported: HashSet<u32>,
    /// Whether the one-time "no descent check on this platform" note has been
    /// raised. Silence must not be mistaken for "nothing found". Only the
    /// non-unix arm of `check_descendants` reads it, hence the allow.
    #[cfg_attr(unix, allow(dead_code))]
    unsupported_reported: bool,
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
            unknown_reported: HashSet::new(),
            unsupported_reported: false,
        }
    }

    /// Re-read what we snapshotted. Returns one alert per change, and updates the
    /// baseline so a single event is reported once rather than every tick.
    ///
    /// `pane_pids` is this session's live pane processes — the same list that is
    /// written to the crash registry. `pty` runs `setsid()` in `pre_exec`, so each
    /// one is a session leader whose sid is its own pid, and everything the agent
    /// spawns in that pane inherits that sid. That is the second descent signal
    /// (see [`Warden::judge_session`]), and it is why this takes an argument.
    pub fn check(&mut self, pane_pids: &[u32]) -> Vec<Alert> {
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
        match digest_file(&self.registry) {
            Some(now) => {
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
            None => {
                // DELETION was silent here, and deletion is the stronger attack:
                // `rm /tmp/amux-session-<us>.pids` leaves a nested amux with no
                // policy to read, and an absent policy used to mean no ceiling.
                // Reporting only on a changed file is the fail-open-on-absent-
                // input shape this codebase has shipped before.
                if self.registry_digest.take().is_some() {
                    out.push(Alert {
                        kind: "warden-registry-gone",
                        detail: format!(
                            "the session registry was deleted ({}); a nested amux \
                             can no longer read this session's policy",
                            self.registry.display()
                        ),
                    });
                }
            }
        }

        out.extend(self.check_descendants(pane_pids));
        out
    }

    /// Called after amux rewrites its own registry, so a legitimate write is not
    /// reported as tampering.
    pub fn registry_rewritten(&mut self) {
        self.registry_digest = digest_file(&self.registry);
    }

    /// Look for amux sessions we cannot account for.
    ///
    /// The candidate set is deliberately small: only pids that own a session
    /// registry, and only ones we have not already judged. In steady state that
    /// is empty, so this costs nothing — no process table, no syscalls at all.
    #[cfg(unix)]
    fn check_descendants(&mut self, pane_pids: &[u32]) -> Vec<Alert> {
        let me = std::process::id();
        let fresh: Vec<u32> = session_pids()
            .into_iter()
            .filter(|p| *p != me && !self.known_sessions.contains(p))
            .collect();
        if fresh.is_empty() {
            return Vec::new();
        }

        // One process-table snapshot serves every candidate on this tick. The
        // previous shape re-forked `ps` inside each ancestry walk; the fork is
        // only ever paid on a tick where a new session appeared, but paying it
        // once per candidate instead of once per tick was still needless.
        let Some(table) = ProcTable::snapshot() else {
            // Could not read the table at all: every candidate is unproven. Say
            // so — an unreadable table must not read as "nothing nested here".
            return fresh
                .into_iter()
                .filter_map(|pid| self.report_unknown(pid, "the process table could not be read"))
                .collect();
        };
        let pane_sids = pane_session_ids(pane_pids);
        let mut ids = ExeIds::new();

        let mut out = Vec::new();
        for pid in fresh {
            if !crate::reap::pid_running(pid) {
                // Gone already: nothing to report, and no verdict to remember —
                // the pid could be reused, and a reused pid deserves a fresh look.
                continue;
            }
            match judge_session(pid, me, &table, &pane_sids, &mut ids) {
                Verdict::Accounted => {
                    self.known_sessions.insert(pid);
                }
                Verdict::Unaccounted(detail) => {
                    self.known_sessions.insert(pid);
                    out.push(Alert {
                        kind: "warden-nested-unaccounted",
                        detail,
                    });
                }
                Verdict::Unproven(why) => {
                    if let Some(a) = self.report_unknown(pid, why) {
                        out.push(a);
                    }
                }
            }
        }
        out
    }

    /// Windows has no ppid walk or session-id notion here, and the Job Object
    /// bounds a pane's *lifetime* rather than its *posture* — see
    /// [`amux_ancestor`]. Report the gap once rather than returning an empty
    /// list every tick, which would read as "checked, all clear".
    #[cfg(not(unix))]
    fn check_descendants(&mut self, _pane_pids: &[u32]) -> Vec<Alert> {
        if self.unsupported_reported {
            return Vec::new();
        }
        self.unsupported_reported = true;
        vec![Alert {
            kind: "warden-descent-unsupported",
            detail: "nested-session detection is not implemented on this platform; \
                     amux applies no permission ceiling to a nested amux here"
                .to_string(),
        }]
    }

    /// Raise the "could not establish" alert at most once per pid. The pid stays
    /// unjudged so a later tick can resolve it.
    #[cfg(unix)]
    fn report_unknown(&mut self, pid: u32, why: &str) -> Option<Alert> {
        if !self.unknown_reported.insert(pid) {
            return None;
        }
        Some(Alert {
            kind: "warden-descent-unknown",
            detail: format!(
                "could not establish whether the amux session at pid {pid} is nested \
                 under this one ({why}); treating it as unproven, not as safe"
            ),
        })
    }
}

/// Measure a candidate, then hand the measurements to [`verdict_for`].
///
/// The two halves are separate so the decision can be tested against every
/// combination of inputs without a real process tree: every false positive this
/// module has shipped was a decision made on an input that meant something else.
#[cfg(unix)]
fn judge_session(
    pid: u32,
    me: u32,
    table: &ProcTable,
    pane_sids: &HashSet<u32>,
    ids: &mut ExeIds,
) -> Verdict {
    let ancestry = table.nearest_amux_ancestor(pid, ids);
    // Only measured when it can change the answer, since it is a syscall per
    // candidate and the ancestry alone settles most cases.
    let in_our_panes = match ancestry {
        Ancestry::NoneFound => session_id(pid).map(|sid| pane_sids.contains(&sid)),
        _ => None,
    };
    verdict_for(pid, me, ancestry, in_our_panes)
}

/// Decide a single candidate, using only kernel-derived measurements.
///
/// Nothing the candidate wrote is consulted. The predecessor of this rule read
/// `capped_by=<parent pid>` out of the candidate's own registry — a
/// self-declaration in a mode-0644 file at a fixed path, which any process at the
/// same uid can write, including one that never capped and one that is not amux
/// at all.
///
/// The replacement is the parent applying **the same predicate the child
/// applies**: the nearest ancestor running this exact binary. If that is us, then
/// the child's own `cap_trust_to_ancestor` walked the same chain with the same
/// `(dev, ino)` test and reached the same answer, so it read our policy. What
/// that does NOT prove — say it in full — is that it *honoured* what it read: a
/// rebuilt binary is exactly that case, and the startup digest is the only thing
/// that speaks to it.
///
/// `in_our_panes` is "does this pid's session id belong to one of our live
/// panes", with `None` for "could not be read". It is only consulted when the
/// ancestry walk found no amux ancestor at all.
#[cfg(unix)]
fn verdict_for(pid: u32, me: u32, ancestry: Ancestry, in_our_panes: Option<bool>) -> Verdict {
    match ancestry {
        Ancestry::Amux(a) if a == me => Verdict::Accounted,
        // Nested under an amux that is not us. If that one is under us, it capped
        // against our policy and this one capped against its — a chain of
        // ceilings, each link the business of its own parent. Judging it
        // ourselves is the bug that killed correct grandchildren: `capped_by`
        // recorded the NEAREST ancestor while descent was walked 64 hops, so a
        // perfectly-behaved depth-3 session looked uncapped and, under
        // enforcement, had its whole tree torn down about five seconds in.
        Ancestry::Amux(_) => Verdict::Accounted,
        Ancestry::NoneFound => match in_our_panes {
            // No amux ancestor, but our session id: `pty` gives each pane its own
            // session via `setsid()`, everything the agent spawns there inherits
            // it, and — measured on this host — a plain double-fork moves ppid to
            // 1 while leaving sid untouched. So this is the escape the ancestry
            // cap cannot see, and the one signal that still sees it.
            Some(true) => Verdict::Unaccounted(format!(
                "an amux session (pid {pid}) is running inside one of this session's \
                 panes but its ancestry does not lead back here, so no ceiling was \
                 applied to it. That is what a double-fork looks like; it is also \
                 what an ordinary reparenting looks like, so this is a report, not \
                 an accusation"
            )),
            // Neither ancestry nor session id connects it to us: an amux in
            // another terminal, which is normal use and none of our business.
            // Alerting on it was a shipped false positive, and a tripwire that
            // fires on legitimate behaviour is worse than none.
            Some(false) => Verdict::Accounted,
            None => Verdict::Unproven("its session id could not be read"),
        },
        Ancestry::Unknown => Verdict::Unproven("the ancestry walk did not complete"),
        // No implementation on this platform; the gap is reported once by
        // `check_descendants`, not once per candidate.
        Ancestry::Unsupported => Verdict::Accounted,
    }
}

/// The outcome of judging one candidate session.
#[cfg(unix)]
enum Verdict {
    /// Kernel state explains it. Remember it and stay quiet.
    Accounted,
    /// Kernel state says it is inside us and did not come through the cap.
    Unaccounted(String),
    /// Kernel state was not available. Report and re-judge later — never silently
    /// treat an absent signal as "fine".
    Unproven(&'static str),
}

/// Pids of every amux session that has a registry file on this host.
///
/// This scanned `std::env::temp_dir()` while [`crate::reap::registry_path`]
/// writes to a FIXED directory (`/tmp` on unix, deliberately not `$TMPDIR`,
/// which the agent owns). On macOS `$TMPDIR` is a per-user
/// `/var/folders/…/T/`, so the two never met: this returned the empty set on
/// every tick and the whole nested-session check was dead code on the platform
/// amux is developed on. Ask `reap` where registries live rather than guessing.
fn session_pids() -> HashSet<u32> {
    session_pids_in(&crate::reap::registry_dir())
}

fn session_pids_in(dir: &Path) -> HashSet<u32> {
    let mut out = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
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

/// The nearest ancestor process that is itself an amux, if any.
///
/// This is the cap's foundation: a process cannot unset its own parent, so
/// unlike an environment marker it cannot be shrugged off with `env -u`. It is
/// not unbreakable — a double-fork orphans the child onto `init` and erases the
/// trail — but doing so is deliberate, and it leaves the session-id trail the
/// warden's nested-session check reads.
///
/// Returns four states on purpose. The previous version returned `Option`, and
/// mapped every failure — no `/bin/ps`, an unparseable table, a vanished hop —
/// onto `None`, which the caller read as "not nested" and therefore "no
/// ceiling". A ceiling that disappears when its input cannot be read is not a
/// ceiling.
#[cfg(unix)]
pub fn amux_ancestor() -> Ancestry {
    let Some(table) = ProcTable::snapshot() else {
        return Ancestry::Unknown;
    };
    table.nearest_amux_ancestor(std::process::id(), &mut ExeIds::new())
}

#[cfg(not(unix))]
pub fn amux_ancestor() -> Ancestry {
    // A stated decision, not an oversight. `reap::SessionJob` puts every pane in
    // a Job Object with KILL_ON_JOB_CLOSE, and job membership is inherited and
    // cannot be left — so a nested amux on Windows is already contained. But a
    // job bounds a tree's LIFETIME, not its POSTURE: a nested amux inside the job
    // can still be launched at `--trust skip`, it simply cannot outlive us. So
    // Windows has no permission ceiling today. Doing it properly means
    // `QueryFullProcessImageNameW` for identity and `NtQueryInformationProcess`
    // (or a toolhelp snapshot) for ancestry; that is a real piece of work and it
    // has not been done, which is what `Unsupported` says.
    Ancestry::Unsupported
}

/// One snapshot of the process table: pid -> ppid.
///
/// Taken with a SINGLE `ps` call. The first version walked ancestry with one
/// `ps -p <pid>` per hop — up to 64 subprocesses per lookup, per new session,
/// every few seconds. That is a process storm, and it showed up immediately as
/// timing flakiness in the test suite, which runs many amux instances at once. A
/// watchdog that loads the machine it is watching is a bad watchdog.
///
/// It no longer asks for `comm`, which removes a parsing hazard (macOS `comm`
/// is a path and may contain spaces) *and* the reason the hazard mattered: names
/// are no longer what identity is decided on. See the module docs — `comm` is
/// argv[0] on macOS and `PR_SET_NAME`-settable on Linux, i.e. attacker-chosen.
#[cfg(unix)]
struct ProcTable {
    parent: HashMap<u32, u32>,
}

#[cfg(unix)]
impl ProcTable {
    /// `None` when the table could not be read. Emphatically not "an empty
    /// table": an empty table is indistinguishable from a machine with no
    /// processes, and callers turned that into "no ancestor, so no ceiling".
    fn snapshot() -> Option<ProcTable> {
        // Absolute path, not `ps`. `Command::new("ps")` resolves through `$PATH`,
        // which the agent owns - so a pane could put its own `ps` first and have
        // the ancestry walk report whatever it liked, or simply make the lookup
        // fail. Both failed OPEN: an empty table means "no amux ancestor", which
        // means no ceiling. The ceiling must not be reachable only through a tool
        // the constrained party can replace.
        let o = std::process::Command::new("/bin/ps")
            .args(["-eo", "pid=,ppid="])
            .output()
            .ok()?;
        if !o.status.success() {
            return None;
        }
        let mut parent = HashMap::new();
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let mut it = line.split_whitespace();
            let (Some(pid), Some(ppid)) = (it.next(), it.next()) else {
                continue;
            };
            let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
                continue;
            };
            parent.insert(pid, ppid);
        }
        // A table with nothing in it is a failed read wearing a disguise.
        if parent.is_empty() {
            return None;
        }
        Some(ProcTable { parent })
    }

    fn nearest_amux_ancestor(&self, start: u32, ids: &mut ExeIds) -> Ancestry {
        let parent_of = |pid: u32| self.parent.get(&pid).copied();
        walk_to_amux(&parent_of, &mut |pid| ids.is_amux(pid), start)
    }
}

/// Walk `start`'s ancestry for the nearest process running this amux binary.
///
/// Split out from the table and the identity check so both can be driven from a
/// synthetic fixture in the tests: the three ways this has been wrong (matching
/// the starting process itself, reporting "none" when a hop could not be read,
/// and answering for a grandchild that belongs to an intermediate) are all
/// properties of this walk, and all are now covered.
///
/// `is_amux` returns `None` for "cannot tell", which propagates to
/// [`Ancestry::Unknown`] rather than being read as "not an amux". The difference
/// is the whole fail-closed story: "not an amux" continues the walk and can end
/// in `NoneFound`, i.e. no ceiling.
#[cfg(unix)]
fn walk_to_amux(
    parent_of: &dyn Fn(u32) -> Option<u32>,
    is_amux: &mut dyn FnMut(u32) -> Option<bool>,
    start: u32,
) -> Ancestry {
    let mut cur = start;
    for _ in 0..MAX_HOPS {
        // Check each PARENT, never `start` itself — the first version compared
        // the current entry while carrying the parent's pid, so amux matched
        // itself on the first hop and reported its own parent as the ancestor.
        let Some(ppid) = parent_of(cur) else {
            return Ancestry::Unknown;
        };
        if ppid <= 1 || ppid == cur {
            return Ancestry::NoneFound;
        }
        match is_amux(ppid) {
            Some(true) => return Ancestry::Amux(ppid),
            Some(false) => {}
            None => return Ancestry::Unknown,
        }
        cur = ppid;
    }
    // Out of hops: a cycle or a table longer than any real ancestry. Unproven,
    // not "no ancestor" — the fail-closed direction.
    Ancestry::Unknown
}

/// Memoised "is this pid running the same executable file as us?".
///
/// Identity is `(st_dev, st_ino)` of the KERNEL's executable for the pid, not a
/// name and not a path string. See the module docs for why a name is worthless
/// here (`exec -a amux /bin/sleep`, verified on this host).
#[cfg(unix)]
struct ExeIds {
    mine: Option<Option<(u64, u64)>>,
    seen: HashMap<u32, Option<bool>>,
}

#[cfg(unix)]
impl ExeIds {
    fn new() -> ExeIds {
        ExeIds {
            mine: None,
            seen: HashMap::new(),
        }
    }

    /// `None` means "cannot tell", which the walk turns into `Unknown`.
    fn is_amux(&mut self, pid: u32) -> Option<bool> {
        let mine = *self
            .mine
            .get_or_insert_with(|| exe_identity(std::process::id()));
        // If we cannot identify our OWN executable there is nothing to compare
        // against, and every answer would be a guess.
        let mine = mine?;
        if let Some(hit) = self.seen.get(&pid) {
            return *hit;
        }
        let ans = exe_identity(pid).map(|id| id == mine);
        self.seen.insert(pid, ans);
        ans
    }
}

/// Is the process at `pid` running the **same executable file** as this one?
///
/// `None` means "cannot tell" — a vanished process, a denied read — and callers
/// must treat it as exactly that, never as "not amux". This is the module's
/// identity check, exported so the orphan sweep
/// ([`crate::orphan::classify_owner`]) asks the same question the ancestry walk
/// does rather than reinventing it as a name comparison: `ps -o comm=` is
/// `argv[0]` on macOS and `(exec -a amux /bin/sleep 40)` forges it in one line,
/// verified on this host.
#[cfg(unix)]
pub fn same_binary(pid: u32) -> Option<bool> {
    ExeIds::new().is_amux(pid)
}

/// Windows has no ancestry or identity plumbing here yet — see [`amux_ancestor`]
/// — and the Job Object makes the sweep that asks this question unnecessary.
#[cfg(not(unix))]
pub fn same_binary(_pid: u32) -> Option<bool> {
    None
}

/// `(st_dev, st_ino)` of the executable the kernel says `pid` is running.
///
/// `None` on any failure — a vanished process, a denied read — which callers
/// must treat as "cannot tell", never as "not amux".
#[cfg(unix)]
fn exe_identity(pid: u32) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(exe_path(pid)?).ok()?;
    Some((md.dev(), md.ino()))
}

/// macOS: `proc_pidpath` returns the kernel's vnode path for the process, which
/// argv rewriting and `exec -a` cannot touch. It lives in libSystem, so this
/// needs no dependency — the same raw `extern "C"` style as `reap`/`signals`.
#[cfg(all(unix, target_os = "macos"))]
fn exe_path(pid: u32) -> Option<PathBuf> {
    /// `PROC_PIDPATHINFO_MAXSIZE` from `<sys/proc_info.h>`.
    const PATH_MAX_BYTES: usize = 4 * 1024;
    extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut core::ffi::c_void, buffersize: u32) -> i32;
    }
    let mut buf = vec![0u8; PATH_MAX_BYTES];
    // SAFETY: `buf` is a live allocation of exactly `buffersize` bytes, which is
    // the whole contract; the call only writes into it and returns a length.
    let n = unsafe {
        proc_pidpath(
            pid as i32,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(PathBuf::from(String::from_utf8(buf).ok()?))
}

/// Linux: `/proc/<pid>/exe` is a magic link to the running image, so `stat`ting
/// it (following the link) gives the same `(dev, ino)` without a readlink.
///
/// UNTESTED — written from documentation, not run on a Linux host. Two things to
/// re-check before relying on it: the link is `PTRACE_MODE_READ_FSCREDS`-gated,
/// so a hardened `ptrace_scope` or a `hidepid` mount can deny it even at the same
/// uid, and that denial arrives here as `None` → `Unknown` → a report, which is
/// the fail-closed direction but is also noise if it happens routinely.
/// Non-Linux, non-macOS unix has no `/proc/<pid>/exe` at all and lands on the
/// same `None`.
#[cfg(all(unix, not(target_os = "macos")))]
fn exe_path(pid: u32) -> Option<PathBuf> {
    let p = PathBuf::from(format!("/proc/{pid}/exe"));
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// The session id of `pid`, or `None` if it could not be read (the process is
/// gone, or the platform does not answer).
///
/// Session id is the second descent signal and the only one that survives a
/// double-fork. There is no syscall to JOIN an existing session — a process's sid
/// is inherited through fork or becomes its own pid after `setsid()` — so unlike
/// `capped_by` it cannot be asserted, only inherited. Measured on this host:
/// after a plain double-fork, ppid became 1 while sid was unchanged.
///
/// It is not containment. One `setsid()` sheds it, and that is the same act that
/// already defeats `reap`'s process-group teardown, so the residual is one this
/// codebase has already written down.
#[cfg(unix)]
fn session_id(pid: u32) -> Option<u32> {
    extern "C" {
        /// `pid_t getsid(pid_t pid)` — -1/ESRCH for a process that is gone.
        fn getsid(pid: i32) -> i32;
    }
    // SAFETY: a read-only query on a pid; no pointers, no state.
    let sid = unsafe { getsid(pid as i32) };
    if sid < 0 {
        None
    } else {
        Some(sid as u32)
    }
}

/// The session ids of our live panes.
///
/// Only panes that verify as session leaders (`getsid(p) == p`) count. A pane in
/// the middle of exiting answers ESRCH — measured here, 30 such processes on the
/// review host at once, all `?Es` — and treating that as a signal would alert on
/// ordinary teardown. Dropping it instead means the sid check simply does not see
/// that pane, which loses a report rather than inventing one; the module's whole
/// stance is that a missing signal is not evidence of safety.
///
/// Recomputed from the CURRENT pane list every time, never accumulated: a pane
/// pid can be reused after the pane exits, and a stale sid would then match an
/// unrelated process.
#[cfg(unix)]
fn pane_session_ids(pane_pids: &[u32]) -> HashSet<u32> {
    pane_pids
        .iter()
        .filter(|p| **p != 0)
        .filter(|p| session_id(**p) == Some(**p))
        .copied()
        .collect()
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
            w.check(&[])
                .iter()
                .all(|a| a.kind != "warden-registry-changed"),
            "an untouched registry must not alert"
        );

        std::fs::write(&reg, "1234\n5678\n").unwrap();
        let alerts = w.check(&[]);
        assert!(
            alerts.iter().any(|a| a.kind == "warden-registry-changed"),
            "a tampered registry must alert: {alerts:?}"
        );
        // Baseline moves, so the same change is not re-reported forever — an
        // alert that repeats every tick trains the operator to ignore it.
        assert!(
            w.check(&[])
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
            w.check(&[])
                .iter()
                .all(|a| a.kind != "warden-registry-changed"),
            "amux's own write was reported as tampering"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deleting the registry is the stronger tamper — it is how you make a nested
    /// amux find no policy to obey — and it used to be reported as nothing at all,
    /// because only `Some(digest)` was ever compared.
    #[test]
    fn a_deleted_registry_is_reported() {
        let dir = std::env::temp_dir().join(format!("amux-warden-rm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry");
        std::fs::write(&reg, "1\n").unwrap();
        let mut w = Warden::new(reg.clone());
        std::fs::remove_file(&reg).unwrap();
        let alerts = w.check(&[]);
        assert!(
            alerts.iter().any(|a| a.kind == "warden-registry-gone"),
            "a deleted registry must alert: {alerts:?}"
        );
        // ...and only once.
        assert!(
            w.check(&[])
                .iter()
                .all(|a| a.kind != "warden-registry-gone"),
            "the same deletion was reported twice"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The escapee scan reads the directory `reap` actually writes to. It read
    /// `$TMPDIR` while registries went to `/tmp`; on macOS those differ, so the
    /// scan enumerated nothing and the whole check was dead code.
    #[test]
    fn session_pids_reads_the_registry_directory() {
        let fake = std::process::id() + 1_000_000; // never a live pid
        let path = crate::reap::registry_path(fake);
        std::fs::write(&path, "policy=plan\n").unwrap();
        let found = session_pids();
        let _ = std::fs::remove_file(&path);
        assert!(
            found.contains(&fake),
            "a registry written by reap::registry_path must be found by session_pids"
        );
    }

    // --- the ancestry walk, on a synthetic table ----------------------------
    //
    // Every way this has been wrong is a property of the walk, so the walk is
    // tested directly rather than through a live process tree.

    #[cfg(unix)]
    fn walk(edges: &[(u32, u32)], amux: &[u32], unknown: &[u32], start: u32) -> Ancestry {
        let map: HashMap<u32, u32> = edges.iter().copied().collect();
        let amux: HashSet<u32> = amux.iter().copied().collect();
        let unknown: HashSet<u32> = unknown.iter().copied().collect();
        walk_to_amux(
            &|pid| map.get(&pid).copied(),
            &mut |pid| {
                if unknown.contains(&pid) {
                    None
                } else {
                    Some(amux.contains(&pid))
                }
            },
            start,
        )
    }

    /// amux matching ITSELF on the first hop, and so reporting its own parent as
    /// its ancestor, is a bug this walk shipped once.
    #[cfg(unix)]
    #[test]
    fn the_walk_never_matches_the_starting_process() {
        // 50 is amux and its parent 40 is an ordinary shell under init.
        assert_eq!(
            walk(&[(50, 40), (40, 1)], &[50], &[], 50),
            Ancestry::NoneFound
        );
    }

    /// A -> pane -> B -> shell -> C, all correctly nested. C's ceiling is B's
    /// business. Judging C against A is exactly the mismatch that SIGKILLed
    /// correctly-behaving grandchildren: `capped_by` recorded the nearest ancestor
    /// while descent was walked 64 hops.
    #[cfg(unix)]
    #[test]
    fn depth_three_delegates_to_the_nearest_amux() {
        let edges = [(50, 40), (40, 30), (30, 20), (20, 10), (10, 1)];
        assert_eq!(walk(&edges, &[10, 30, 50], &[], 50), Ancestry::Amux(30));
        assert_eq!(walk(&edges, &[10, 30, 50], &[], 30), Ancestry::Amux(10));
    }

    /// A hop we cannot identify must not be silently read as "not an amux",
    /// because that lets the walk run off the top and answer "no ancestor" —
    /// which the cap turns into "no ceiling".
    #[cfg(unix)]
    #[test]
    fn an_unresolvable_hop_is_unknown_not_none() {
        let edges = [(50, 40), (40, 30), (30, 1)];
        assert_eq!(walk(&edges, &[30], &[40], 50), Ancestry::Unknown);
        // The same shape with the hop resolvable finds the real ancestor, so the
        // Unknown above is about the missing answer and nothing else.
        assert_eq!(walk(&edges, &[30], &[], 50), Ancestry::Amux(30));
    }

    /// A pid missing from the snapshot ends the walk as unproven. Returning
    /// "no ancestor" here is how an unreadable `ps` became "no ceiling".
    #[cfg(unix)]
    #[test]
    fn a_hop_missing_from_the_table_is_unknown() {
        assert_eq!(walk(&[(50, 40)], &[], &[], 50), Ancestry::Unknown);
    }

    /// A corrupt table must terminate, and must do so on the unproven side.
    #[cfg(unix)]
    #[test]
    fn a_cycle_terminates_as_unknown() {
        assert_eq!(walk(&[(50, 40), (40, 50)], &[], &[], 50), Ancestry::Unknown);
    }

    // --- the verdict, on measurements that cannot be forged ----------------
    //
    // These are the false-positive cases. Every one of them has a real-world
    // shape, and two of them have already shipped as bugs.

    #[cfg(unix)]
    fn verdict(a: Ancestry, panes: Option<bool>) -> Verdict {
        verdict_for(4242, 7, a, panes)
    }

    /// The ordinary nested session: we are its nearest amux ancestor, so it ran
    /// our cap. Nothing it wrote is consulted to reach this.
    #[cfg(unix)]
    #[test]
    fn a_session_nested_directly_under_us_is_accounted_for() {
        assert!(matches!(
            verdict(Ancestry::Amux(7), None),
            Verdict::Accounted
        ));
    }

    /// A -> B -> C. C's ceiling is B's business, and A must not judge it. This is
    /// the bug that shipped: it SIGKILLed correctly-nested grandchildren.
    #[cfg(unix)]
    #[test]
    fn a_grandchild_belongs_to_the_intermediate_not_to_us() {
        assert!(matches!(
            verdict(Ancestry::Amux(99), None),
            Verdict::Accounted
        ));
    }

    /// An amux in a second terminal has neither our ancestry nor our session id.
    /// Alerting on that also shipped once: normal use reported as an intrusion.
    #[cfg(unix)]
    #[test]
    fn an_unrelated_amux_elsewhere_is_silent() {
        assert!(matches!(
            verdict(Ancestry::NoneFound, Some(false)),
            Verdict::Accounted
        ));
    }

    /// The escape the ancestry cap cannot see: the parent link is gone but the
    /// session id still places it inside one of our panes.
    #[cfg(unix)]
    #[test]
    fn a_detached_session_inside_our_panes_is_reported() {
        let v = verdict(Ancestry::NoneFound, Some(true));
        match v {
            Verdict::Unaccounted(d) => {
                assert!(d.contains("4242"), "the alert must name the pid: {d}");
                // The wording must not overclaim: an ordinary reparenting looks
                // identical, and this is why nothing kills on it.
                assert!(d.contains("report, not"), "the alert must not accuse: {d}");
            }
            _ => panic!("a detached nested session must be reported"),
        }
    }

    /// Absent input is the shape this codebase keeps failing open on: an
    /// unreadable table, a hop whose executable cannot be resolved, a session id
    /// that will not read. None of them may mean "fine".
    #[cfg(unix)]
    #[test]
    fn an_unestablished_signal_is_unproven_not_fine() {
        assert!(matches!(
            verdict(Ancestry::Unknown, None),
            Verdict::Unproven(_)
        ));
        assert!(matches!(
            verdict(Ancestry::NoneFound, None),
            Verdict::Unproven(_)
        ));
    }

    // --- the kernel-derived primitives --------------------------------------

    /// Identity is the executable FILE, not the name a process chose. Verified on
    /// this host: `(exec -a amux /bin/sleep 40)` reports `comm=amux` to `ps` while
    /// `proc_pidpath` reports `/bin/sleep`, so a name test hands an agent the
    /// ancestry walk for one line.
    #[cfg(unix)]
    #[test]
    fn exe_identity_is_the_file_not_the_name() {
        let me = exe_identity(std::process::id()).expect("our own executable must resolve");
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec -a amux sleep 5"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the impostor");
        let pid = child.id();
        // `exec` replaces the shell in place, so the pid is the sleep itself.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let theirs = exe_identity(pid);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            theirs.is_some(),
            "a live same-uid child's executable must be resolvable"
        );
        assert_ne!(
            theirs,
            Some(me),
            "a process calling itself amux must not be identified as amux"
        );
    }

    /// The property the second descent signal rests on: a session id is inherited
    /// through fork and cannot be joined, so a child is in our session even when
    /// its parent link is gone.
    #[cfg(unix)]
    #[test]
    fn a_child_inherits_our_session_id() {
        let mine = session_id(std::process::id()).expect("our own session id must be readable");
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 5"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn a child");
        let pid = child.id();
        let theirs = session_id(pid);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(theirs, Some(mine), "a forked child shares our session");
    }

    /// A pid that does not exist answers "cannot tell", not a session id — the
    /// distinction the fail-closed path depends on.
    #[cfg(unix)]
    #[test]
    fn a_dead_pid_has_no_session_id() {
        assert_eq!(session_id(std::process::id() + 1_000_000), None);
    }

    /// A pane only contributes a session id when the kernel confirms it leads
    /// that session. Our own process is (usually) not a session leader, and a
    /// pid that does not exist never is — neither may widen the descent test.
    #[cfg(unix)]
    #[test]
    fn only_verified_session_leaders_become_pane_sids() {
        let dead = std::process::id() + 1_000_000;
        assert!(!pane_session_ids(&[dead]).contains(&dead));
        assert!(pane_session_ids(&[0]).is_empty());
    }

    /// On this host amux is not launched from inside another amux, so the cap
    /// must not fire. It must also not report "unknown": that would mean the
    /// process table or our own executable could not be read, which is a real
    /// problem worth failing this test over. (The positive case needs a real
    /// nested launch and is covered by the integration test.)
    #[cfg(unix)]
    #[test]
    fn a_normal_launch_has_no_amux_ancestor() {
        assert_eq!(amux_ancestor(), Ancestry::NoneFound);
    }
}
