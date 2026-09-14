//! The per-tick safety net: the crash registry, the session snapshot, the
//! warden's tripwires and (unix) the orphan watchdog.
//!
//! Carved out of `run()` (r10 audit B11, step 4b). This was loop phase 0b, whose
//! fourteen locals were read and written nowhere else in the loop; they now live
//! in [`SafetyNet`] and the phase is [`SafetyNet::tick`]. The logic is unchanged.

use crate::*;

/// What keeps a crash from orphaning pane trees, and what notices tampering.
pub(crate) struct SafetyNet {
    /// The crash registry: the pane process groups a watchdog should kill if
    /// this process dies without running any teardown at all.
    registry_path: std::path::PathBuf,
    snapshot_path: std::path::PathBuf,
    last_snapshot: Option<atrium::session::Snapshot>,
    last_snapshot_check: Instant,
    /// The warden: tripwires, not gates. atrium cannot stop an agent that can run
    /// commands from launching an unconstrained one (it could run claude directly
    /// with no atrium at all), so what it CAN do is notice - its own binary being
    /// edited to remove the ceiling, the registry that ceiling is read from being
    /// tampered with, or a session appearing that the ancestry cap did not explain.
    warden: atrium::warden::Warden,
    last_warden_check: Instant,
    cap_notice_raised: bool,
    /// The live pane pids last written to the registry.
    registered: Vec<u32>,
    /// `registered` is not yet on disk.
    registry_dirty: bool,
    /// The last registry write failed (surfaced once per streak).
    registry_failing: bool,
    /// Earliest next registry write after a failure.
    registry_retry_at: Option<Instant>,
    /// A failing session snapshot is surfaced once per failure streak.
    snapshot_failing: bool,
    /// Held for the whole run: dropping this Child closes the pipe atrium uses as
    /// its death signal, which would fire the watchdog early. Unix only — on
    /// Windows the Job Object replaces it (a watchdog there can't signal a
    /// process group and would block on its pipe forever, R5).
    #[cfg(unix)]
    watchdog: Option<std::process::Child>,
    #[cfg(unix)]
    watchdog_failing: bool,
    #[cfg(unix)]
    watchdog_retry_at: Option<Instant>,
}

impl SafetyNet {
    pub(crate) fn new() -> SafetyNet {
        let registry_path = atrium::reap::registry_path(std::process::id());
        SafetyNet {
            snapshot_path: atrium::reap::registry_dir()
                .join(format!("atrium-session-{}.json", std::process::id())),
            last_snapshot: None,
            last_snapshot_check: Instant::now(),
            warden: atrium::warden::Warden::new(registry_path.clone()),
            last_warden_check: Instant::now(),
            cap_notice_raised: false,
            registered: Vec::new(),
            registry_dirty: false,
            registry_failing: false,
            registry_retry_at: None,
            snapshot_failing: false,
            #[cfg(unix)]
            watchdog: None,
            #[cfg(unix)]
            watchdog_failing: false,
            #[cfg(unix)]
            watchdog_retry_at: None,
            registry_path,
        }
    }

    /// The crash registry file, for the teardown to settle.
    pub(crate) fn registry_path(&self) -> &std::path::Path {
        &self.registry_path
    }

    /// Keep the crash registry current. Rewritten only when the pane set changes,
    /// so an idle session does no filesystem work; the watchdog re-reads it at
    /// teardown, which is how panes opened later are covered. Returns whether
    /// something visible changed (repaint this tick).
    pub(crate) fn tick(
        &mut self,
        windows: &[Window],
        session_job: &atrium::reap::SessionJob,
        ctl_audit: &mut atrium::audit::Audit,
        bus: &mut atrium::bus::Bus,
        flash: &mut Option<(String, Instant)>,
    ) -> bool {
        let mut repaint = false;
        let cur: Vec<u32> = windows
            .iter()
            .flat_map(|w| w.panes.iter().map(|p| p.pty.pid()))
            .filter(|p| *p != 0)
            .collect();
        if cur != self.registered {
            // Assign any newly-appeared pane to the session job so its whole
            // tree is torn down with atrium (no-op on unix). Only the new pids,
            // so a process is never re-assigned.
            for pid in cur.iter().filter(|p| !self.registered.contains(p)) {
                session_job.assign(*pid);
            }
            self.registered = cur;
            self.registry_dirty = true;
            self.registry_retry_at = None;
            let saved = snapshot_if_changed(windows, &self.snapshot_path, &mut self.last_snapshot);
            surface_once(&mut self.snapshot_failing, saved, "session snapshot", flash);
            self.last_snapshot_check = Instant::now();
        }
        // Persist it. A failed write is neither dropped nor recorded as done
        // (r10 audit B2): the warden keeps its baseline, the failure is
        // surfaced once per streak (audit + bus + bar), and it is retried on
        // the warden cadence until it lands.
        if self.registry_dirty && self.registry_retry_at.map_or(true, |t| Instant::now() >= t) {
            let written = atrium::reap::write_registry(
                &self.registry_path,
                &self.registered,
                trust_mode().policy_label(),
            );
            match self.warden.record_registry_write(&written) {
                None => {
                    if self.registry_failing {
                        *flash = Some(("crash registry updated".to_string(), Instant::now()));
                        repaint = true;
                    }
                    self.registry_dirty = false;
                    self.registry_failing = false;
                    self.registry_retry_at = None;
                }
                Some(alert) => {
                    if !self.registry_failing {
                        self.registry_failing = true;
                        ctl_audit.record(None, alert.kind, &alert.detail, false, "");
                        // The bar carries it whether or not the bus accepts it.
                        let _ = bus.publish(
                            "warden",
                            atrium::bus::Kind::DecisionNeeded,
                            None,
                            &[("msg".to_string(), alert.detail.clone())],
                            agsess::sessions::now_ms(),
                        );
                        *flash = Some((alert.detail, Instant::now()));
                        repaint = true;
                    }
                    self.registry_retry_at = Some(Instant::now() + WARDEN_INTERVAL);
                }
            }
        }
        if self.last_snapshot_check.elapsed() >= SNAPSHOT_INTERVAL {
            self.last_snapshot_check = Instant::now();
            let saved = snapshot_if_changed(windows, &self.snapshot_path, &mut self.last_snapshot);
            surface_once(&mut self.snapshot_failing, saved, "session snapshot", flash);
        }
        if self.last_warden_check.elapsed() >= WARDEN_INTERVAL {
            self.last_warden_check = Instant::now();
            // The live pane pids double as the second descent signal: `pty`
            // makes each pane a session leader, so anything the agent spawns
            // in a pane carries that session id even after a double-fork has
            // erased its parent link.
            let mut alerts = self.warden.check(&self.registered);
            // The ancestry cap fired before the bus existed; raise it once now.
            if let Some(note) = CAP_NOTICE.get() {
                if !self.cap_notice_raised {
                    self.cap_notice_raised = true;
                    alerts.push(atrium::warden::Alert {
                        kind: "warden-nested-atrium",
                        detail: note.clone(),
                    });
                }
            }
            for alert in alerts {
                ctl_audit.record(None, alert.kind, &alert.detail, false, "");
                // A decision, not an FYI: these are exactly the events that
                // should stop the operator rather than scroll past them.
                // If the bus refuses it (rate/size cap), the audit record above
                // is a log, not an alert: put the decision in the bar instead
                // of losing the operator-facing signal (r10 audit B8).
                if let Err(e) = bus.publish(
                    "warden",
                    atrium::bus::Kind::DecisionNeeded,
                    None,
                    &[("msg".to_string(), alert.detail.clone())],
                    agsess::sessions::now_ms(),
                ) {
                    *flash = Some((
                        format!("warden: {} (bus refused: {e})", alert.detail),
                        Instant::now(),
                    ));
                }
                repaint = true;
            }
            // There is no enforcement branch here any more, and that is a
            // decision rather than an omission: `ATRIUM_WARDEN=enforce` used to
            // tear down anything judged an escapee, and both the judgement and
            // the kill were unsound. `warden`'s module docs carry the full
            // reasoning; the short version is that under a correct ancestry
            // rule the enforceable set is empty, the only sessions left to
            // accuse are indistinguishable from an ordinary reparenting, and
            // the kill target was read out of a file the accused could write.
        }
        // The watchdog is the unix answer to a death no handler can catch.
        // Windows does not need it and must not run it: the durable fix there
        // is a Job Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, which the
        // kernel honours however atrium dies. A watchdog on Windows would spawn
        // a second atrium that cannot signal a process group (`reap`'s
        // non-unix `signal_group` returns false) and would sit blocked on its
        // pipe forever. The Job Object attaches just above (`session_job`),
        // in this same pane-set-changed block.
        #[cfg(unix)]
        if self.watchdog.is_none()
            && !self.registered.is_empty()
            && self.watchdog_retry_at.map_or(true, |t| Instant::now() >= t)
        {
            // A failed spawn is retried on the warden cadence (not every tick)
            // and surfaced once, so a safety net that never comes up is visible
            // (r10 audit B8).
            let spawned = atrium::reap::spawn_watchdog(&self.registry_path);
            self.watchdog_retry_at = spawned.is_err().then(|| Instant::now() + WARDEN_INTERVAL);
            self.watchdog = surface_once(
                &mut self.watchdog_failing,
                spawned,
                "orphan watchdog (pane trees may outlive a crash)",
                flash,
            );
        }
        repaint
    }
}
