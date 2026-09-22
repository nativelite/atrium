//! Recovering a session: `atrium recover`, the launch-time resume offer,
//! the posture a resume runs at, the consent notice, and the argv each
//! pane is relaunched with.

use crate::*;

/// Was one of `names` typed on the recover command line (bare or `=`-glued)?
///
/// `ctl::parse_flags` returns a value for every flag, defaulted, so it cannot say
/// whether the operator *chose* one. Recovery needs that distinction: an explicit
/// flag must win over the snapshot, while an absent one must defer to it rather
/// than silently overwriting the recorded session with a default.
fn flag_typed(args: &[String], names: &[&str]) -> bool {
    args.iter().any(|a| {
        names.contains(&a.as_str())
            || names
                .iter()
                .any(|n| a.starts_with(&format!("{n}=")) && n.starts_with("--"))
    })
}

/// The trust ceiling a recovery runs at: an explicitly typed flag wins, else the
/// posture the snapshot recorded, else the parsed default. The result is still
/// capped to any enclosing session by `cap_trust_to_ancestor` at the call site —
/// a snapshot is data on disk and must never be a way to climb.
fn recovered_trust(
    typed: bool,
    parsed: atrium::ctl::TrustMode,
    recorded: Option<atrium::ctl::TrustMode>,
) -> atrium::ctl::TrustMode {
    if typed {
        return parsed;
    }
    recorded.unwrap_or(parsed)
}

/// One pane's posture on recovery: what it ran at before, capped to the session
/// ceiling. A pane that sat below the ceiling (a fleet agent's own `trust`, a
/// de-escalated worker) comes back where it was instead of being promoted to the
/// ceiling, and a snapshot naming a *higher* posture is capped, not obeyed.
fn recovered_pane_mode(
    recorded: Option<atrium::ctl::TrustMode>,
    ceiling: atrium::ctl::TrustMode,
) -> atrium::ctl::TrustMode {
    effective_mode(recorded, ceiling).0
}

/// A one-line, human-readable account of what a recovery is about to restore —
/// or, for a pre-policy snapshot, what it cannot.
///
/// `policy` is the **effective** policy (what will actually be installed), not
/// the raw record: a trust mode the ancestor cap lowered must read as lowered.
/// `trust_from_snapshot` marks the posture as coming from the file rather than
/// the command line, because that is the one value a recovery can raise above
/// the flags the operator typed — and the snapshot is a file in a shared temp
/// directory. Printed *before* the skip confirmation, so it informs the only
/// prompt the operator gets.
fn recovery_notice(
    panes: &[atrium::session::PaneRecord],
    policy: Option<&atrium::session::PolicyRecord>,
    trust_from_snapshot: bool,
    ceiling: atrium::ctl::TrustMode,
) -> String {
    let mut parts = vec![format!("{} pane(s)", panes.len())];
    let head = match policy {
        Some(p) => {
            if let Some(t) = p.trust {
                let source = if trust_from_snapshot {
                    " (from the snapshot)"
                } else {
                    ""
                };
                parts.push(format!("trust {}{source}", t.policy_label()));
            }
            if p.allow_ctl {
                // An unlimited guard is stored clamped to `i64::MAX` (see
                // `session::num`); printing that as a 19-digit number reads as
                // corruption rather than as "no limit".
                let depth = if p.max_depth > 1_000_000 {
                    "unlimited".to_string()
                } else {
                    p.max_depth.to_string()
                };
                parts.push(format!("ctl on (max depth {depth})"));
            }
            if !p.deny.is_empty() {
                parts.push(format!("{} session deny rule(s)", p.deny.len()));
            }
            if let Some(j) = p.build_jobs {
                parts.push(format!("build pool {j}"));
            }
            if let Some(mb) = p.memory_mb {
                parts.push(format!("memory cap {mb} MB"));
            }
            format!("atrium recover: restoring {}", parts.join(", "))
        }
        None => format!(
            "atrium recover: restoring {} — this snapshot predates the policy block, so the \
             session's deny rules, compile pool and memory cap are NOT restored; \
             pass them on the command line if the session had them",
            parts.join(", ")
        ),
    };
    // Then every pane: what it will run and with what it is allowed to do. The
    // summary line alone left out the part a hostile snapshot actually controls —
    // the commands, and each pane's spawn right and deny rules — so the operator
    // was approving a count. Every string is from the file, so every string is
    // defanged before it reaches the terminal, as the fleet banner does.
    let mut lines = vec![head];
    for (n, record) in panes.iter().enumerate() {
        let who = record
            .role
            .as_deref()
            .map(atrium::fleet::sanitize)
            .unwrap_or_else(|| format!("pane {}", n + 1));
        lines.push(format!("  [{who}] {}", argv_summary(&resume_argv(record))));
        let mut facts = Vec::new();
        if let Some(cwd) = &record.cwd {
            facts.push(format!("cwd {}", atrium::fleet::sanitize(cwd)));
        }
        if let Some(id) = &record.identity {
            facts.push(format!("identity {}", atrium::fleet::sanitize(id)));
        }
        facts.push(format!(
            "mode {}",
            recovered_pane_mode(record.mode, ceiling).policy_label()
        ));
        facts.push(if record.can_spawn {
            "may spawn teammates".to_string()
        } else {
            "cannot spawn".to_string()
        });
        if !record.deny.is_empty() {
            facts.push(format!("{} own deny rule(s)", record.deny.len()));
        }
        // Shown, not just named: both come from the file and both reach the agent.
        if let Some(norms) = &record.norms {
            let flat = atrium::fleet::sanitize(&norms.replace(['\n', '\r'], " "));
            let preview: String = flat.chars().take(60).collect();
            facts.push(format!(
                "worktree instructions \"{preview}…\" ({} chars)",
                norms.chars().count()
            ));
        }
        for (name, value) in &record.context_env {
            facts.push(format!("{name}={}", atrium::fleet::sanitize(value)));
        }
        lines.push(format!("      {}", facts.join(" · ")));
        let (_, dropped) = atrium::ctl::sanitize_spawn_argv(&record.argv);
        if !dropped.is_empty() {
            lines.push(format!(
                "      ignored from the saved command: {}",
                dropped.join(", ")
            ));
        }
    }
    lines.join("\n")
}

/// A pane's command for the consent notice: every flag as written, short values
/// verbatim, and anything long or multi-word — a system prompt, a kickoff —
/// reduced to its length. Defanged, since it comes from the snapshot file.
fn argv_summary(argv: &[String]) -> String {
    argv.iter()
        .enumerate()
        .map(|(i, a)| {
            let a = atrium::fleet::sanitize(a);
            if i == 0 {
                atrium::bind::command_stem(&a)
            } else if a.starts_with('-') || (a.chars().count() <= 40 && !a.contains(' ')) {
                a
            } else {
                format!("<{} chars>", a.chars().count())
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `atrium recover` — load the most recent session snapshot and relaunch every
/// pane with its session resumed. A missing snapshot prints a clear message
/// and exits cleanly rather than panicking.
pub(crate) fn recover_cmd(args: &[String]) -> ExitCode {
    if args.iter().any(|a| atrium::help::wants(a)) {
        print!("{}", atrium::help::RECOVER);
        return ExitCode::SUCCESS;
    }
    // recover's own flags first; everything else is the session flags `parse_flags`
    // knows (`--trust`, `--allow-ctl`, `--max-depth`).
    let mut list = false;
    let mut explicit: Option<std::path::PathBuf> = None;
    let mut flags: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list = true,
            "--snapshot" => match args.get(i + 1) {
                Some(path) => {
                    explicit = Some(path.into());
                    i += 1;
                }
                None => {
                    eprintln!("atrium recover: --snapshot needs a path");
                    return ExitCode::FAILURE;
                }
            },
            a if a.starts_with("--snapshot=") => explicit = Some(a["--snapshot=".len()..].into()),
            a => flags.push(a.to_string()),
        }
        i += 1;
    }
    let (allow_ctl, max_depth, trust, rest) = match atrium::ctl::parse_flags(&flags) {
        Ok(quad) => quad,
        Err(msg) => {
            eprintln!("atrium recover: {msg}");
            return ExitCode::FAILURE;
        }
    };
    if !rest.is_empty() {
        eprintln!("atrium recover: unexpected argument {:?}", rest[0]);
        eprint!("{}", atrium::help::RECOVER);
        return ExitCode::FAILURE;
    }
    let Ok(cwd) = std::env::current_dir() else {
        eprintln!("atrium recover: the current directory no longer exists — cd into the project");
        return ExitCode::FAILURE;
    };
    let project = atrium::session_store::project_id(&cwd);
    let dir = atrium::session_store::state_root()
        .map(|root| atrium::session_store::project_dir(&root, &project));
    if list {
        print_sessions(&project, dir.as_deref());
        return ExitCode::SUCCESS;
    }
    // Before anything is chosen, asked or marked: a resume needs a terminal to
    // run in, and its confirmation needs a person at one. Run from a script or an
    // agent's shell, `recover` used to read end-of-input as consent, mark the
    // operator's crashed session closed, and only then fail on the terminal.
    {
        use std::io::IsTerminal;
        if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
            eprintln!("atrium recover: needs a terminal (to confirm the resume, and to run it)");
            return ExitCode::FAILURE;
        }
    }
    let now = atrium::session_store::now_ms();
    let (path, snap) = match explicit {
        Some(path) => match atrium::session::load(&path) {
            Ok(snap) => (path, snap),
            Err(e) => {
                eprintln!("atrium recover: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            let stored = dir
                .as_deref()
                .map(|d| atrium::session_store::list(d, &project, now).sessions)
                .unwrap_or_default();
            match atrium::session_store::recoverable(&stored, now, atrium::reap::pid_running) {
                Some(found) => (found.path.clone(), found.snapshot.clone()),
                None => {
                    eprintln!("atrium recover: no saved session for this project ({project})");
                    // Snapshots used to live in the shared temp directory. Name the
                    // newest one there rather than scan it: picking by mtime from a
                    // directory every session and test writes to is exactly how the
                    // wrong session used to get restored.
                    if let Some(old) = find_latest_snapshot(
                        &atrium::reap::registry_dir(),
                        atrium::reap::pid_running,
                    ) {
                        eprintln!(
                            "atrium recover: an older atrium kept snapshots in the temp directory; \
                             the newest is {}\n  restore it with: atrium recover --snapshot \"{}\"",
                            old.display(),
                            old.display()
                        );
                    }
                    return ExitCode::SUCCESS;
                }
            }
        }
    };
    if atrium::session_store::liveness(&snap.meta, now, atrium::reap::pid_running)
        == atrium::session_store::Liveness::Running
    {
        eprintln!(
            "atrium recover: that session is still running (atrium pid {}); resuming it would \
             start a second copy of the same agents on the same transcripts",
            snap.meta
                .pid
                .map_or_else(|| "?".to_string(), |p| p.to_string())
        );
        return ExitCode::FAILURE;
    }
    if snap.panes.is_empty() {
        eprintln!("atrium recover: snapshot has no panes — nothing to recover");
        return ExitCode::SUCCESS;
    }
    if !snap.meta.clean && snap.meta.pid.is_some_and(atrium::reap::pid_running) {
        eprintln!(
            "atrium recover: its atrium (pid {}) still exists but stopped updating — it may be \
             hung. Resuming starts a second copy of the same agents.",
            snap.meta.pid.unwrap_or(0)
        );
    }
    let plan = plan_resume(
        &snap,
        Some(&path),
        allow_ctl,
        max_depth,
        trust,
        flag_typed(&flags, &["--trust", "--skip-permissions"]),
        flag_typed(&flags, &["--max-depth"]),
    );
    // Before the confirmation, not after: this line is what the operator needs in
    // order to answer it.
    eprintln!("{}", plan.notice);
    if !confirm_resume("atrium recover: resume this session?") {
        eprintln!("atrium recover: not resumed.");
        return ExitCode::SUCCESS;
    }
    resume_session(snap, plan, Some(&path))
}

/// What a resume will install, settled before anything is asked or spawned so the
/// operator is shown exactly that.
struct ResumePlan {
    trust: atrium::ctl::TrustMode,
    allow_ctl: bool,
    max_depth: usize,
    /// The policy as it will actually be installed: the snapshot's guards, with
    /// the trust/ctl/depth values settled here. Recorded verbatim for the resumed
    /// session, so its own snapshots carry it forward unchanged — reading it back
    /// off the live globals would lose a `build_jobs` that was inherited rather
    /// than re-created, one recovery generation at a time.
    applied: Option<atrium::session::PolicyRecord>,
    /// The one-line account shown before the confirmation.
    notice: String,
}

/// What a resume's bus and board will bring back, for the consent notice. Their
/// contents are as writable by hosted agents as the snapshot, and agents act on
/// bus messages, so the operator is told they are coming rather than finding out.
/// `None` when the source has neither (or is outside the store, which never seeds).
fn sidecar_summary(source: &std::path::Path) -> Option<String> {
    let root = atrium::session_store::state_root()?;
    if !atrium::session_store::is_in_store(source, &root) {
        return None;
    }
    let bus_path = atrium::session_store::sidecar(source, "bus");
    let board_path = atrium::session_store::sidecar(source, "board");
    let mut parts = Vec::new();
    if bus_path.is_file() {
        let bus = atrium::bus::Bus::with_file(bus_path);
        parts.push(format!(
            "bus: {} event(s), {} open decision(s)",
            bus.tail(atrium::bus::RING_CAP).len(),
            bus.pending_decisions().len()
        ));
    }
    if board_path.is_file() {
        let board = atrium::board::Board::with_file(board_path);
        parts.push(format!("board: {} entr(ies)", board.list().len()));
    }
    (!parts.is_empty()).then(|| {
        format!(
            "  restoring the team's {} — written by the session's agents; treat as such",
            parts.join(", ")
        )
    })
}

/// Settle a resume's posture. A typed flag wins; an omitted one defers to the
/// snapshot instead of overwriting it with a default. The trust ceiling is still
/// capped to any enclosing atrium session — a snapshot is data on disk and must
/// never be a way to climb above one.
fn plan_resume(
    snap: &atrium::session::Snapshot,
    source: Option<&std::path::Path>,
    allow_ctl: bool,
    max_depth: usize,
    trust: atrium::ctl::TrustMode,
    trust_typed: bool,
    depth_typed: bool,
) -> ResumePlan {
    let policy = snap.policy.as_ref();
    let trust = recovered_trust(trust_typed, trust, policy.and_then(|p| p.trust));
    let allow_ctl = allow_ctl || policy.is_some_and(|p| p.allow_ctl);
    let max_depth = if depth_typed {
        max_depth
    } else {
        policy.map_or(max_depth, |p| p.max_depth)
    };
    let trust = cap_trust_to_ancestor(trust);
    let applied = policy.map(|p| atrium::session::PolicyRecord {
        deny: p.deny.clone(),
        claude_aliases: p.claude_aliases.clone(),
        build_jobs: p.build_jobs,
        memory_mb: p.memory_mb,
        trust: Some(trust),
        allow_ctl,
        max_depth,
        topics: p.topics.clone(),
    });
    let mut notice = recovery_notice(&snap.panes, applied.as_ref(), !trust_typed, trust);
    if let Some(line) = source.and_then(sidecar_summary) {
        notice.push('\n');
        notice.push_str(&line);
    }
    ResumePlan {
        trust,
        allow_ctl,
        max_depth,
        applied,
        notice,
    }
}

/// Read a `[Y/n]` answer: anything but an explicit no is yes. Pure, so the rule is
/// testable without a terminal.
fn resume_answer(line: &str) -> bool {
    !matches!(line.trim().to_ascii_lowercase().as_str(), "n" | "no")
}

/// Ask the operator to confirm a resume. Always a real answer from a terminal:
/// `ATRIUM_YES` is deliberately not honoured here. It exists so scripts can skip
/// a fleet's banner, and an operator who keeps it set would otherwise have a
/// crashed session's snapshot — its commands and posture — applied behind their
/// back the next time they started a fleet.
fn confirm_resume(question: &str) -> bool {
    eprint!("{question} [Y/n] ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        // Zero bytes is end-of-input (Ctrl+D, Ctrl+Z, a closed pipe): nobody
        // answered, and nobody answering is not consent. Enter is a line.
        Ok(0) | Err(_) => false,
        Ok(_) => resume_answer(&line),
    }
}

/// Is this atrium running inside another atrium's pane? The pane marker, or —
/// where the platform can tell — process ancestry. The marker alone is an
/// environment variable an agent can unset; ancestry is the check it cannot, and
/// is used wherever it exists (not on Windows yet, see `warden::atrium_ancestor`).
/// Neither is a security boundary on its own: what they prevent is an agent that
/// simply runs `atrium` having its session filed, pruned and offered as the
/// operator's.
pub(crate) fn nested_in_atrium() -> bool {
    std::env::var_os(atrium::ctl::ENV_PANE).is_some()
        || matches!(
            atrium::warden::atrium_ancestor(),
            atrium::warden::Ancestry::Atrium(_)
        )
}

/// Should a launch offer to resume at all? Not when switched off
/// (`ATRIUM_RESUME_OFFER=0`, which the test suite sets), not from inside an
/// atrium pane (an agent launching atrium must not be handed the operator's
/// crashed session), and not without a terminal to ask on.
fn offer_enabled(offer_env: Option<&str>, in_pane: bool, interactive: bool) -> bool {
    offer_env != Some("0") && !in_pane && interactive
}

/// A human-scale age: `42s`, `7m`, `3h`, `2d`.
fn human_age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// On launch in a project whose last session crashed, offer to resume it — one
/// prompt that is both the resume and the approval. `Some(code)` when the launch
/// became a resume; `None` to launch normally. Declining marks that session
/// closed so it is not asked about again (`atrium recover` still has it).
pub(crate) fn offer_resume(
    allow_ctl: bool,
    max_depth: usize,
    trust: atrium::ctl::TrustMode,
) -> Option<ExitCode> {
    use std::io::IsTerminal;
    let offer_env = std::env::var(atrium::session_store::ENV_RESUME_OFFER).ok();
    if !offer_enabled(
        offer_env.as_deref(),
        nested_in_atrium(),
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
    ) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let project = atrium::session_store::project_id(&cwd);
    let root = atrium::session_store::state_root()?;
    let now = atrium::session_store::now_ms();
    let stored = atrium::session_store::list(
        &atrium::session_store::project_dir(&root, &project),
        &project,
        now,
    )
    .sessions;
    let found = atrium::session_store::offer(&stored, now, atrium::reap::pid_running)?;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let plan = plan_resume(
        &found.snapshot,
        Some(&found.path),
        allow_ctl,
        max_depth,
        trust,
        flag_typed(&argv, &["--trust", "--skip-permissions"]),
        flag_typed(&argv, &["--max-depth"]),
    );
    let meta = &found.snapshot.meta;
    eprintln!(
        "atrium: the last session in this project did not exit cleanly (saved {} ago).",
        meta.saved_at_ms
            .map_or_else(|| "?".to_string(), |t| human_age(now.saturating_sub(t)))
    );
    if meta.pid.is_some_and(atrium::reap::pid_running) {
        eprintln!(
            "atrium: its atrium (pid {}) still exists but stopped updating — it may be hung. \
             Resuming starts a second copy of the same agents.",
            meta.pid.unwrap_or(0)
        );
    }
    eprintln!("{}", plan.notice);
    if confirm_resume("Resume it?") {
        return Some(resume_session(
            found.snapshot.clone(),
            plan,
            Some(&found.path),
        ));
    }
    if let Err(e) = atrium::session_store::mark_closed(&found.path) {
        eprintln!("atrium: warning: {e}; this session may be offered again");
    }
    eprintln!("atrium: starting a new session (`atrium recover --list` still has that one).");
    None
}

/// `atrium recover --list`: this project's saved sessions, newest first, with
/// what each would restore — and any file that could not be read, since an
/// unreadable snapshot may be a tampered one.
fn print_sessions(project: &str, dir: Option<&std::path::Path>) {
    let Some(dir) = dir else {
        eprintln!(
            "atrium recover: no state directory could be determined (set {})",
            atrium::session_store::ENV_STATE_DIR
        );
        return;
    };
    let now = atrium::session_store::now_ms();
    let listing = atrium::session_store::list(dir, project, now);
    let (stored, bad) = (&listing.sessions, &listing.unreadable);
    println!("sessions for {project}");
    println!("  in {}", dir.display());
    if stored.is_empty() && bad.is_empty() && listing.suspect.is_empty() {
        println!("  (none)");
    }
    for s in stored {
        let meta = &s.snapshot.meta;
        let state = match atrium::session_store::liveness(meta, now, atrium::reap::pid_running) {
            atrium::session_store::Liveness::Running => "running",
            atrium::session_store::Liveness::Crashed => "crashed",
            atrium::session_store::Liveness::Closed => "closed",
        };
        let age = meta
            .saved_at_ms
            .map_or_else(|| "?".to_string(), |t| human_age(now.saturating_sub(t)));
        let trust = s
            .snapshot
            .policy
            .as_ref()
            .and_then(|p| p.trust)
            .map_or("-", |t| t.policy_label());
        let roles: Vec<&str> = s
            .snapshot
            .panes
            .iter()
            .filter_map(|p| p.role.as_deref())
            .collect();
        // Role strings come from the file: defanged, or an ESC in one could
        // repaint this listing into something reassuring.
        let roles: Vec<String> = roles.into_iter().map(atrium::fleet::sanitize).collect();
        let roles = if roles.is_empty() {
            String::new()
        } else {
            format!("  [{}]", roles.join(", "))
        };
        println!(
            "  {state:<8} {age:>4} ago  {} pane(s)  trust {trust}{roles}",
            s.snapshot.panes.len()
        );
        println!("           {}", s.path.display());
    }
    for (path, e) in bad {
        println!("  unreadable  {}: {e}", path.display());
    }
    for (path, why) in &listing.suspect {
        println!(
            "  suspect     {}: {why} — never offered; restore only with --snapshot, after \
             reading it",
            path.display()
        );
    }
}

/// Resume `snap` under `plan`: the skip confirmation, the session guards, every
/// pane with its own posture and capability, then the run loop. `source` is the
/// file it came from; a file in the store is marked closed once the operator has
/// confirmed, so the next launch does not offer it again. If a pane then fails to
/// start, the session is still reachable with a bare `atrium recover`, which also
/// takes closed sessions.
fn resume_session(
    snap: atrium::session::Snapshot,
    plan: ResumePlan,
    source: Option<&std::path::Path>,
) -> ExitCode {
    let ResumePlan {
        trust,
        allow_ctl,
        max_depth,
        applied,
        ..
    } = plan;
    let policy = snap.policy.clone();
    if trust == atrium::ctl::TrustMode::Skip && !confirm_skip_permissions() {
        eprintln!("atrium recover: aborted.");
        return ExitCode::SUCCESS;
    }
    // Checked again here, after the prompts: the answer can take a while, and a
    // second terminal offered the same crash may have resumed it meanwhile.
    if let Some(src) = source {
        if let (Ok(cwd), Some(root)) =
            (std::env::current_dir(), atrium::session_store::state_root())
        {
            let project = atrium::session_store::project_id(&cwd);
            let now = atrium::session_store::now_ms();
            let stored = atrium::session_store::list(
                &atrium::session_store::project_dir(&root, &project),
                &project,
                now,
            )
            .sessions;
            let me = atrium::session_store::Stored {
                path: src.to_path_buf(),
                snapshot: snap.clone(),
            };
            if atrium::session_store::superseded(&stored, &me, now, atrium::reap::pid_running) {
                eprintln!(
                    "atrium recover: these agents are already running in another atrium \
                     session — not starting a second copy"
                );
                return ExitCode::FAILURE;
            }
        }
    }
    if let (Some(src), Some(root)) = (source, atrium::session_store::state_root()) {
        if atrium::session_store::is_in_store(src, &root) {
            if let Err(e) = atrium::session_store::mark_closed(src) {
                eprintln!("atrium recover: warning: {e}; this session may be offered again");
            }
        }
    }
    set_trust_mode(trust);
    let topics = applied.as_ref().and_then(|p| p.topics.clone());
    if let Some(applied) = applied {
        atrium::session::set_policy(applied);
    }
    // The run loop opens this session's bus and board beside its new snapshot;
    // this is what lets it start them from the old session's instead of empty.
    if let Some(src) = source {
        atrium::session_store::set_resume_source(src);
        // Copied now, not when the run loop opens them: until this session writes
        // its first snapshot, another atrium starting in the project could prune
        // the source and its bus and board would silently start empty.
        if let Some((_, file, _)) = session_location() {
            atrium::session_store::seed_sidecar(file, "bus");
            atrium::session_store::seed_sidecar(file, "board");
        }
    }
    // Re-install the session guards BEFORE any pane spawns, in the same order
    // `fleet up` does: the deny list and memory ceiling are read at each spawn,
    // and the compile pool must exist before the first agent inherits it.
    if let Some(p) = &policy {
        if !p.deny.is_empty() {
            atrium::trust::set_fleet_deny(p.deny.clone());
        }
        // Before any spawn: the panes' commands are judged claude (or not) by
        // the same aliases the session ran with.
        if !p.claude_aliases.is_empty() {
            atrium::bind::set_claude_aliases(p.claude_aliases.clone());
        }
        if let Some(mb) = p.memory_mb {
            atrium::memguard::set_fleet_mb(mb);
        }
        // Guarded the way `fleet up` guards it: `planned_size` is `None` when the
        // pool is disabled or already inherited from an enclosing session, and
        // neither is a failure worth warning about.
        if let Some(jobs) = p.build_jobs {
            if atrium::buildpool::planned_size(Some(jobs)).is_some()
                && atrium::buildpool::init(Some(jobs)).is_none()
            {
                eprintln!(
                    "atrium recover: warning: could not rebuild the build pool — agents' \
                     builds will run unpooled"
                );
            }
        }
    }
    let mut flash: Option<(String, Instant)> = None;
    let ctl_listener = if allow_ctl {
        bind_ctl(&mut flash)
    } else {
        None
    };
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("atrium recover: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (rows, cols) = term.size().unwrap_or((24, 80));
    let pane_rows = rows.saturating_sub(1).max(1);
    let cell_rows = (pane_rows as usize / snap.panes.len().max(1)).max(1) as u16;
    let mut panes: Vec<Pane> = Vec::with_capacity(snap.panes.len());
    for record in &snap.panes {
        let argv = resume_argv(record);
        let place = resume_place(record, |d| std::path::Path::new(d).is_dir());
        if let Some(w) = &place.warning {
            eprintln!("atrium recover: {w}");
        }
        match spawn_pane_full(
            PaneSpec {
                command: &argv,
                id: record.id,
                identity: record.identity.as_deref(),
                cwd: place.cwd.as_deref(),
                // The pane's own posture, capped to the ceiling — not the ceiling
                // itself, which would promote every de-escalated pane.
                mode: recovered_pane_mode(record.mode, trust_mode()),
                // What the original spawn folded in beyond argv: the worktree
                // instructions, and the context-store variables (allowlisted on
                // read, so a snapshot cannot hand a pane any other environment).
                extra_env: &record.context_env,
                extra_norms: place.norms.as_deref(),
                // The agent's own deny rules. `argv` never carried them: the
                // `--disallowedTools` flags are built here, at spawn, from the
                // roster — so a recovery that replayed argv alone handed every
                // pane back the commands its fleet entry had taken away.
                deny: &record.deny,
            },
            cell_rows,
            cols,
            &mut flash,
        ) {
            Ok(mut pane) => {
                pane.role = record.role.clone();
                pane.worktree = record.worktree.clone();
                // The transcript id. `--resume <id>` is a user session argument,
                // so the spawn path injects no id of its own and would leave the
                // pane unbound: no agent status, and a snapshot with nothing to
                // resume next time or to tell a second copy by. Resuming keeps
                // writing the same transcript, so the recorded id stays right.
                if pane.session_id.is_none() {
                    pane.session_id = record.session_id.clone();
                }
                // The kickoff was dropped from this pane's command iff it resumed a
                // transcript; a pane that started fresh still carries it.
                pane.kickoff = record.kickoff && !kickoff_dropped(record);
                // Capability and spawn-depth, from the snapshot. `spawn_pane_full`
                // defaults to the human-pane behaviour (`can_spawn: true`, depth
                // 0); leaving those defaults gave every recovered fleet agent the
                // right to create teammates that its roster had withheld, and
                // restarted the `--max-depth` guard at zero for deep workers.
                pane.can_spawn = record.can_spawn;
                pane.depth = record.depth;
                panes.push(pane);
            }
            Err(e) => {
                // Name the pane and where it was to start: the error alone
                // blamed the command for a directory that was missing.
                let (what, where_) = (
                    pane_label(record),
                    place
                        .cwd
                        .as_deref()
                        .map(|d| format!(" in {d}"))
                        .unwrap_or_default(),
                );
                eprintln!(
                    "atrium recover: cannot start {what} ({:?}){where_}: {e}",
                    argv[0]
                );
                for p in panes.iter_mut() {
                    let _ = p.pty.kill();
                }
                return ExitCode::FAILURE;
            }
        }
    }
    // Re-point each worker at its parent. Agent ids were re-minted by the spawns
    // above, so the snapshot's stored PANE ids are mapped to the new agent ids
    // here, once every pane exists. `panes` is index-aligned with `snap.panes`:
    // the loop pushes one per record and aborts on the first failure.
    let new_ids: Vec<(usize, atrium::ctl::AgentId)> =
        panes.iter().map(|p| (p.id, p.agent_id)).collect();
    for (pane, record) in panes.iter_mut().zip(snap.panes.iter()) {
        if let Some(parent_pane) = record.parent_pane {
            pane.parent = new_ids
                .iter()
                .find(|(id, _)| *id == parent_pane)
                .map(|(_, agent)| *agent);
        }
    }
    // Both id sets, not just the layout's: a pane record carrying an id above
    // every layout id would otherwise collide with the next `Ctrl+A c`.
    let max_id = snap
        .layout
        .ids
        .iter()
        .chain(snap.panes.iter().map(|p| &p.id))
        .copied()
        .max()
        .unwrap_or(0);
    let mut tree = Tree::grid_from_ids(&snap.layout.ids);
    tree.focus_pane(snap.layout.focus);
    let window = Window {
        panes,
        tree,
        zoomed: false,
        next_id: max_id + 1,
    };
    run(
        &mut term,
        &[default_shell()],
        None,
        None,
        Some(window),
        allow_ctl,
        max_depth,
        trust,
        ctl_listener,
        // A fleet's declared topics keep the bus strict after a resume, as `fleet
        // up` made it.
        topics,
    )
}

/// Where a recovered pane runs and which worktree instructions it keeps: its
/// recorded directory and norms while that directory still exists. When it is
/// gone — a worktree the fleet reaped after its item shipped — the pane starts
/// in the project directory *without* the instructions, which would tell it it
/// sits in a worktree it does not, and `warning` says so. Recovery used to hand
/// the missing directory straight to the spawn and abort the whole session on
/// its "file not found", blaming the command. Pure: `dir_exists` is injected.
struct ResumePlace {
    cwd: Option<String>,
    norms: Option<String>,
    warning: Option<String>,
}

fn resume_place(
    record: &atrium::session::PaneRecord,
    dir_exists: impl Fn(&str) -> bool,
) -> ResumePlace {
    match record.cwd.as_deref() {
        Some(dir) if !dir_exists(dir) => ResumePlace {
            cwd: None,
            norms: None,
            warning: Some(format!(
                "{}: its directory {dir} is gone; starting in the project directory without its worktree instructions",
                pane_label(record)
            )),
        },
        _ => ResumePlace {
            cwd: record.cwd.clone(),
            norms: record.norms.clone(),
            warning: None,
        },
    }
}

/// How a recovery message names a pane: its role, else its command.
fn pane_label(record: &atrium::session::PaneRecord) -> String {
    record
        .role
        .clone()
        .or_else(|| record.argv.first().cloned())
        .unwrap_or_else(|| format!("pane {}", record.id))
}

/// Build the recovery argv for one pane. For claude panes with a stored
/// session id, strips `--continue` and appends `--resume <id>` so the agent
/// resumes its conversation. Non-claude panes are relaunched verbatim.
fn resume_argv(record: &atrium::session::PaneRecord) -> Vec<String> {
    // The saved command is data from a file every hosted agent can write. Posture
    // is atrium's to set, from the policy the operator just confirmed — so the
    // governed permission flags never ride along, exactly as `fleet up` and
    // `ctl spawn` strip them from what they launch.
    // A kickoff is replayed only when the agent starts fresh. Resuming a
    // transcript, the agent is mid-task; its opening instructions arriving again
    // as a new turn is how resumed fleet agents re-read CLAUDE.md and went back to
    // step one.
    //
    // Only for claude, where a `--resume` is appended below. Another vendor's pane
    // may carry a session id atrium adopted, but nothing here resumes it: that pane
    // starts a new session, and its kickoff is exactly what it needs.
    let saved: &[String] = match record.argv.split_last() {
        Some((_, rest)) if kickoff_dropped(record) => rest,
        _ => &record.argv,
    };
    let (mut argv, _) = atrium::ctl::sanitize_spawn_argv(saved);
    if argv.is_empty() {
        return argv;
    }
    if !atrium::bind::is_claude_stem(&atrium::bind::command_stem(&argv[0])) {
        return argv;
    }
    if let Some(id) = &record.session_id {
        // A pane that was itself resumed carries `--resume <id>` in its argv
        // already; appending a second one is a malformed command line.
        argv = strip_session_args(&argv);
        argv.push("--resume".to_string());
        argv.push(id.clone());
    }
    argv
}

/// Does a resume leave this pane's kickoff out? Only when the pane has one and a
/// claude transcript will actually be resumed. Shared by `resume_argv` (which
/// drops it) and the resume spawn (which records whether the new command still
/// carries it), so the two can never disagree.
fn kickoff_dropped(record: &atrium::session::PaneRecord) -> bool {
    record.kickoff
        && record.session_id.is_some()
        && record
            .argv
            .first()
            .is_some_and(|c| atrium::bind::is_claude_stem(&atrium::bind::command_stem(c)))
}

/// Remove every session-selecting argument — `--continue`/`-c`, and
/// `--resume`/`-r`/`--session-id` with their value, in either the separate or
/// the `=` form — so exactly one `--resume` can be appended.
pub(crate) fn strip_session_args(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        let head = a.split('=').next().unwrap_or(a);
        match head {
            "--continue" | "-c" if i > 0 => i += 1,
            "--resume" | "-r" | "--session-id" if i > 0 => {
                i += if !a.contains('=') && i + 1 < argv.len() {
                    2
                } else {
                    1
                };
            }
            _ => {
                out.push(a.clone());
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
