//! `atrium fleet up`: load a roster, show the operator what it will run and grant,
//! and bring it up once they acknowledge.

use super::clean::report_worktree_teardown;
use super::fsan;
use super::launch::spawn_fleet_window;
use super::preflight::{banner_lines, preflight_context_mode, BannerFacts};
use crate::*;

/// Is `rest` — the tokens left after atrium's own leading launch flags are
/// stripped — the top-level `up <name>` fleet alias?
///
/// `atrium fleet up <name> [flags]` is the canonical launch, but the flags-first
/// form operators actually reach for is `atrium --trust automode up <name>`: the
/// session posture reads left-to-right and `up <name>` names the fleet. Without a
/// dispatch for it, atrium parses `--trust automode` as a global flag and then
/// hosts `up` as a *program* — the fleet never launches through [`fleet_up`], so
/// its trust is never set and every agent comes up in regular mode. This is the
/// pure decision behind that dispatch (tested here; wired in `main`):
///
/// - `["up", <name>]` → `Some(Ok(name))` — launch that fleet with the leading
///   `--trust`/`--allow-ctl`/`--max-depth` atrium already parsed.
/// - `["up"]` or `["up", "-x", …]` → `Some(Err(usage))` — no fleet name.
/// - `["up", <name>, <extra>, …]` → `Some(Err(usage))` — flags belong BEFORE
///   `up`; point back at the leading form (or `atrium fleet up` for after-name
///   flags) rather than silently ignoring them.
/// - anything else → `None` — not the alias; host it as a program as before.
pub(crate) fn up_alias(rest: &[String]) -> Option<Result<&str, String>> {
    if rest.first().map(String::as_str) != Some("up") {
        return None;
    }
    match rest.get(1) {
        Some(name) if !name.starts_with('-') => {
            if rest.len() > 2 {
                Some(Err(format!(
                    "up: put session flags before `up` (e.g. `atrium --trust automode up {name}`), \
                     or use `atrium fleet up {name} --trust …`; unexpected {:?}",
                    rest[2]
                )))
            } else {
                Some(Ok(name.as_str()))
            }
        }
        _ => Some(Err(
            "up <name>: needs a fleet name (try `atrium fleet ls`)".to_string()
        )),
    }
}

/// Everything `fleet up` settles before the operator is asked: validated, and
/// nothing written yet. The banner is rendered from this value and the launch
/// runs from it, so what the operator approves is what starts.
pub(super) struct FleetLaunch<'a> {
    pub(super) name: &'a str,
    pub(super) cwd: std::path::PathBuf,
    /// The roster, with the global config's fleet defaults applied.
    pub(super) fleet: atrium::fleet::Fleet,
    pub(super) allow_ctl: bool,
    /// The session posture, capped to any enclosing atrium.
    pub(super) trust: atrium::ctl::TrustMode,
    /// `(agent, posture)` for each agent that runs at a posture other than the session's.
    pub(super) trust_overrides: Vec<(String, String)>,
    /// Preflight warnings found while planning; the banner adds its own after these.
    pub(super) warnings: Vec<String>,
    pub(super) grid: atrium::spawn::Grid,
    /// Every directory each agent is granted, resolved once.
    pub(super) plan: atrium::fleet::Plan,
    pub(super) wt_plans: Vec<atrium::worktree::WorktreePlan>,
    /// Worktrees were asked for and the cwd is a git repo.
    pub(super) wt_active: bool,
}

/// The worktrees an approved launch created, and the base ref they were cut
/// from: what the teardown after the run loop needs.
type Teardown = (Vec<atrium::worktree::WorktreePlan>, String);

/// `atrium fleet up <name>` — read the fleet file, build one tiled window with a
/// pane per agent (each in its `cwd`, under its identity, with its extra args),
/// and hand it to the run loop. Any error before spawning (no file, bad JSON,
/// unknown name, empty fleet, a bad grid, a missing `cwd`) is reported and
/// **nothing is spawned** — never a partial fleet.
///
/// The steps run in a fixed order and the order is part of the behavior: each
/// prompt and each session-wide setting happens exactly where it is called
/// below, and everything the operator must weigh is printed before they are
/// asked.
pub(crate) fn fleet_up(
    name: &str,
    allow_ctl: bool,
    max_depth: usize,
    trust: atrium::ctl::TrustMode,
) -> ExitCode {
    // A crashed fleet in this project is offered back before a fresh one is
    // started over it — the same one prompt a plain launch gives.
    if let Some(code) = crate::offer_resume(allow_ctl, max_depth, trust) {
        return code;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (located, fleet) = match load_fleet(name, &cwd) {
        Ok(loaded) => loaded,
        Err(line) => return fail(&line),
    };
    // The fleet's `claude_aliases` are in force from here: the banner's
    // non-claude warnings, every spawn and later ctl spawns all judge a command
    // by them.
    atrium::bind::set_claude_aliases(fleet.claude_aliases.clone());

    // Preflight: warn if context-mode plugin is missing — but ONLY when this
    // fleet actually opts in (provider == ContextMode). No-context fleets and
    // codex-only fleets must never see this nag. Best-effort, non-fatal.
    if fleet
        .context
        .as_ref()
        .map(|c| c.provider == atrium::context::Provider::ContextMode)
        .unwrap_or(false)
    {
        preflight_context_mode();
    }

    // The file may switch the control plane on, as `--allow-ctl` does. A
    // coordinating fleet without ctl fails silently — panes come up and publish
    // into nothing — and that has already cost a run.
    let allow_ctl = allow_ctl || fleet.allow_ctl.unwrap_or(false);
    let trust = match resolve_trust(name, trust, &fleet) {
        Ok(t) => t,
        Err(line) => return fail(&line),
    };
    // The SAME two gates the single-pane path applies. `fleet` is dispatched
    // before those, so without this a pane could escape its ceiling simply by
    // launching a fleet instead of a pane, and a fleet file declaring `skip`
    // would take full bypass with no confirmation shown to the human running it.
    // That matters most in the workflow this file is built for: an agent writes
    // the roster, a human reviews and runs it. The review is the approval, so the
    // posture must be impossible to slip past it.
    let trust = cap_trust_to_ancestor(trust);
    if trust == atrium::ctl::TrustMode::Skip && !confirm_skip_permissions() {
        eprintln!("atrium fleet: aborted.");
        return ExitCode::SUCCESS;
    }

    let launch = match plan_launch(name, cwd, &located, fleet, allow_ctl, trust) {
        Ok(launch) => launch,
        Err(line) => return fail(&line),
    };

    for line in banner_lines(&launch, &BannerFacts::read(&launch.fleet)) {
        eprintln!("{line}");
    }
    // Hold here until the operator acknowledges.
    //
    // Everything above is printed to the NORMAL screen buffer, and the run loop's
    // first act is `\x1b[?1049h\x1b[2J\x1b[H` - switch to the alternate screen and
    // clear. Nothing blocked in between, so the banner was drawn and hidden in the
    // same breath: an operator never saw the posture, the control plane state, or
    // who may spawn. The entire argument for putting those in the file rather than
    // in flags was that they would be VISIBLE at approval time, and they were not.
    //
    // A fleet file can be written by an agent and run by a human, and the running
    // IS the approval - so the approval should be an act, not an assumption. One
    // keypress is proportionate to starting N agents with filesystem access.
    //
    // Skipped when stdin is not a terminal (a script, CI) or `ATRIUM_YES=1`, since
    // there is nobody to ask; the banner still prints for the log.
    if !fleet_ack() {
        eprintln!("atrium fleet: aborted.");
        return ExitCode::SUCCESS;
    }

    let teardown = match approve(&launch) {
        Ok(teardown) => teardown,
        Err(line) => return fail(&line),
    };
    run_fleet(launch, max_depth, teardown)
}

/// Print `line` and fail the launch.
fn fail(line: &str) -> ExitCode {
    eprintln!("{line}");
    ExitCode::FAILURE
}

/// Find, read and parse the fleet file and take `name` out of it, with the
/// global config's fleet defaults applied. `Err` is the line to print.
fn load_fleet(
    name: &str,
    cwd: &std::path::Path,
) -> Result<(atrium::fleet::Located, atrium::fleet::Fleet), String> {
    let located = atrium::fleet::discover(cwd).map_err(|e| format!("atrium fleet: {e}"))?;
    let text = std::fs::read_to_string(&located.path)
        .map_err(|e| format!("atrium fleet: cannot read {}: {e}", located.path.display()))?;
    let fleets = atrium::fleet::parse(&text)
        .map_err(|e| format!("atrium fleet: {}: {e}", located.path.display()))?;
    let Some(fleet) = fleets.get(name) else {
        let available = fleets
            .names()
            .iter()
            .map(|n| fsan(n))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "atrium fleet: no fleet named \"{}\" (available: {available})",
            fsan(name)
        ));
    };
    // The global config's fleet defaults fill the keys this fleet left out
    // (its own keys and the environment still win).
    let fleet = atrium::config::get().apply_to_fleet(fleet.clone());
    Ok((located, fleet))
}

/// The session posture before the ancestry cap: the command line's when it
/// says anything, else the one the fleet file declares.
///
/// A fleet may declare its own posture, and the command line wins when it says
/// anything. A fleet exists to run hands-off, so it NEEDS a posture — and
/// typing one on every launch is a flag you eventually get wrong. Declared in
/// the file it lives with the roster it applies to and is reviewable in a diff.
///
/// Still a request, not an override: `set_trust_mode` publishes it, and the
/// ancestry cap has already lowered `trust` if this atrium is nested, so a fleet
/// asking for `skip` inside a `plan` session does not get it.
fn resolve_trust(
    name: &str,
    cli: atrium::ctl::TrustMode,
    fleet: &atrium::fleet::Fleet,
) -> Result<atrium::ctl::TrustMode, String> {
    match (cli, fleet.trust.as_deref()) {
        (atrium::ctl::TrustMode::Off, Some(declared)) => {
            atrium::ctl::TrustMode::from_policy_keyword(declared).ok_or_else(|| {
                format!(
                    "atrium fleet: fleet {name:?} declares trust {declared:?}, which is not \
                     one of plan, accept, automode, skip"
                )
            })
        }
        (cli, _) => Ok(cli),
    }
}

/// Validate the roster against the resolved posture and resolve everything it
/// grants, without writing anything or asking anyone. `Err` is the line to print.
fn plan_launch<'a>(
    name: &'a str,
    cwd: std::path::PathBuf,
    located: &atrium::fleet::Located,
    fleet: atrium::fleet::Fleet,
    allow_ctl: bool,
    trust: atrium::ctl::TrustMode,
) -> Result<FleetLaunch<'a>, String> {
    // Per-agent trust overrides (the mixed-model case). Validate each keyword now so
    // a typo fails fast, and compute the capped effective posture for the banner.
    // Each request is capped to the session ceiling exactly like a ctl-spawn
    // `--mode`: an agent may de-escalate (accept under an automode session, so a
    // haiku agent gets the acceptEdits allowlist) but never escalate past what the
    // human approved at launch.
    let mut trust_overrides: Vec<(String, String)> = Vec::new();
    for a in &fleet.agents {
        if let Some(k) = &a.trust {
            match atrium::ctl::TrustMode::from_policy_keyword(k) {
                Some(req) => {
                    let (eff, _note) = crate::effective_mode(Some(req), trust);
                    if eff != trust {
                        trust_overrides.push((fsan(&a.name), eff.policy_label().to_string()));
                    }
                }
                None => {
                    return Err(format!(
                        "atrium fleet: agent \"{}\" declares trust {k:?}, which is not one of \
                         plan, accept, automode, skip",
                        fsan(&a.name)
                    ));
                }
            }
        }
    }
    // Vet every agent's argv exactly as `ctl spawn` does. A fleet file's `cmd` was
    // spawned VERBATIM, so it could embed `--dangerously-skip-permissions`,
    // `--mcp-config`, `--plugin-dir` - the flags the spawn vetting exists to
    // refuse - and the launch bypassed the trust system entirely, whatever the
    // fleet declared and whatever the banner printed.
    //
    // That matters most in the workflow this file is built for: an agent writes
    // the roster, a human reviews and runs it. A flag buried in `cmd` that a skim
    // misses defeats the review, and no ceiling downstream can undo it.
    //
    // Its notes are collected rather than printed here: everything the operator
    // must weigh has to be on the screen at the Enter prompt, so the printing
    // order is chosen once, in the banner. They are preflight warnings: loud,
    // never fatal.
    let mut warnings: Vec<String> = Vec::new();
    for a in &fleet.agents {
        match atrium::ctl::vet_spawn_argv(&a.cmd) {
            atrium::ctl::ArgvVerdict::Refused(why) => {
                return Err(format!("atrium fleet: agent \"{}\": {why}", fsan(&a.name)));
            }
            atrium::ctl::ArgvVerdict::Ok { stripped, .. } if !stripped.is_empty() => {
                warnings.push(format!(
                    "agent \"{}\": ignoring {} — set the posture with \"trust\" instead",
                    fsan(&a.name),
                    fsan(&stripped.join(", "))
                ));
            }
            atrium::ctl::ArgvVerdict::Ok { .. } => {}
        }
    }

    // Grid: explicit `RxC` (must fit the agent count) or an auto balanced grid.
    // Checked BEFORE the banner: never ask a human to approve a launch that
    // cannot happen anyway.
    let n = fleet.agents.len();
    let grid = match &fleet.grid {
        Some(spec) => match atrium::spawn::Grid::parse_spec(spec) {
            Ok(g) if g.total() >= n => g,
            Ok(g) => {
                return Err(format!(
                    "atrium fleet: grid {:?} has {} cells but fleet \"{}\" has {n} agents",
                    fsan(spec),
                    g.total(),
                    fsan(name)
                ));
            }
            Err(e) => return Err(format!("atrium fleet: fleet \"{}\": {e}", fsan(name))),
        },
        None => atrium::spawn::Grid::balanced(n.max(2)),
    };

    // Resolve every directory this roster grants — ONCE. `plan` is what the
    // banner prints and what `spawn_fleet_window` launches with, so the two
    // cannot drift: resolving a second time inside the spawn is how an
    // acknowledged "inside" became a live grant on a credential store when a
    // symlink was re-pointed during the operator's Enter window.
    let anchor = atrium::fleet::anchor_for(
        &located.dir,
        &cwd,
        located.global,
        atrium::fleet::home_dir().as_deref(),
    );
    let plan = atrium::fleet::Plan::build(
        &fleet,
        &located.path,
        &located.dir,
        anchor,
        &atrium::fleet::Stores::live(),
    );

    // Validate every agent's cwd *before* spawning anything, so a bad path never
    // leaves a half-open fleet. `is_dir`, not "exists": a cwd naming a regular
    // file passes an existence check, then fails in the pty spawn with ENOTDIR
    // after raw mode is on and panes are already up.
    for ap in &plan.agents {
        if let Some(g) = &ap.cwd {
            if !g.given.is_dir() {
                return Err(format!(
                    "atrium fleet: agent \"{}\": cwd {} is not a directory",
                    ap.label,
                    atrium::fleet::show_path(&g.given)
                ));
            }
        }
    }

    // Per-agent worktrees (#5): plan them now so they can be disclosed in the
    // banner, but touch no git until after the operator acknowledges. Active only
    // when the fleet asks for a worktree AND the cwd is a git repo — a non-repo
    // fleet (research, ops) is warned and left on the main tree, never blocked.
    let wt_plans = atrium::worktree::plan_worktrees(&fleet, name, &cwd);
    let wt_active = !wt_plans.is_empty() && atrium::worktree::is_git_repo(&cwd);

    Ok(FleetLaunch {
        name,
        cwd,
        fleet,
        allow_ctl,
        trust,
        trust_overrides,
        warnings,
        grid,
        plan,
        wt_plans,
        wt_active,
    })
}

/// What the operator's acknowledgement switches on, in order: the fleet's deny
/// rules and memory ceiling, the compile pool, the worktrees off HEAD, and the
/// session posture. `Ok` carries what the teardown needs when worktrees were
/// created; `Err` is the line to print.
fn approve(launch: &FleetLaunch) -> Result<Option<Teardown>, String> {
    let fleet = &launch.fleet;
    // Approved: record the fleet's deny rules for every pane in the session.
    atrium::trust::set_fleet_deny(fleet.deny.clone());
    // Record the fleet's memory ceiling for the session guard.
    if let Some(mb) = fleet.memory_mb {
        atrium::memguard::set_fleet_mb(mb);
    }
    // Create the compile pool at the fleet's size, before any pane
    // spawns, so every agent inherits this one and not a lazily-made default.
    if atrium::buildpool::planned_size(fleet.build_jobs).is_some()
        && atrium::buildpool::init(fleet.build_jobs).is_none()
    {
        eprintln!(
            "atrium fleet: warning: could not create the build pool — agents' builds will \
             run unpooled"
        );
    }

    // Now that the operator has approved, materialize the worktrees off HEAD —
    // BEFORE raw mode (so any git warning is readable on the normal screen) and
    // BEFORE any pane spawns (so a failure here leaves nothing half-open). Prune
    // first to clear records orphaned by a prior crashed run. The plan + base ref
    // are kept for teardown after the run loop exits.
    let mut teardown: Option<Teardown> = None;
    if launch.wt_active {
        let cwd = &launch.cwd;
        let _ = atrium::worktree::prune(cwd);
        let base = atrium::worktree::base_ref(cwd)
            .map_err(|e| format!("atrium fleet: cannot read the repo's HEAD for worktrees: {e}"))?;
        let seeds = fleet.worktree_seed.as_deref().unwrap_or(&[]);
        for p in &launch.wt_plans {
            match atrium::worktree::ensure(cwd, p) {
                Ok(_) => {
                    for w in atrium::worktree::seed(cwd, &p.dir, seeds) {
                        eprintln!("atrium fleet: worktree \"{}\": {w}", fsan(&p.name));
                    }
                    for w in atrium::worktree::junction_sibling_deps(cwd, p) {
                        eprintln!("atrium fleet: worktree \"{}\": {w}", fsan(&p.name));
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "atrium fleet: could not create worktree \"{}\": {e}",
                        fsan(&p.name)
                    ));
                }
            }
        }
        teardown = Some((launch.wt_plans.clone(), base));
    }

    // Publish the trust policy before the fleet's panes are spawned (they read it
    // via `trust_mode()`), so every agent comes up under the resolved posture.
    set_trust_mode(launch.trust);
    Ok(teardown)
}

/// Enter the terminal, spawn the approved roster into one tiled window, run
/// the session, and tear the worktrees down after it.
fn run_fleet(launch: FleetLaunch, max_depth: usize, teardown: Option<Teardown>) -> ExitCode {
    let FleetLaunch {
        name,
        cwd,
        fleet,
        allow_ctl,
        trust,
        grid,
        plan,
        wt_plans,
        wt_active,
        ..
    } = launch;
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => return fail(&format!("atrium: stdin/stdout must be a terminal: {e}")),
    };
    let (rows, cols) = term.size().unwrap_or((24, 80));

    let mut flash: Option<(String, Instant)> = None;
    // Bind the ctl endpoint BEFORE spawning the fleet's panes, so each agent is
    // born with `ATRIUM_CTL`/`ATRIUM_PANE` in its env and can drive the board/bus.
    // (The default path binds inside run() after its single spawn; a fleet spawns
    // its whole roster up front, so it must publish the address first.)
    let ctl_listener = if allow_ctl {
        bind_ctl(&mut flash)
    } else {
        None
    };
    // When worktrees are active, hand the plans to the spawn so each member
    // pane's cwd is its worktree dir; an empty slice (inactive, or not a repo)
    // means every pane keeps its fleet-file cwd — today's behavior, unchanged.
    let wt_for_spawn: &[atrium::worktree::WorktreePlan] = if wt_active { &wt_plans } else { &[] };
    let window = match spawn_fleet_window(
        &fleet,
        &plan,
        grid,
        rows,
        cols,
        name,
        &cwd,
        wt_for_spawn,
        &mut flash,
    ) {
        Ok(w) => w,
        Err(e) => {
            return fail(&format!(
                "atrium fleet: cannot start fleet \"{}\": {e}",
                fsan(name)
            ))
        }
    };

    // New panes / splits opened later host a shell under the fleet's default
    // identity — a scratch pane in-role, not another copy of an agent.
    let scratch = vec![default_shell()];
    // ctl is opt-in for a fleet too (`atrium fleet up <name> --allow-ctl`), so the
    // roster can coordinate over the board/bus; without the flag it runs as before.
    let exit = run(
        &mut term,
        RunArgs {
            command: &scratch,
            identity: fleet.identity.as_deref(),
            grid: None,
            initial_window: Some(window),
            allow_ctl,
            max_depth,
            trust,
            ctl_listener,
            // A fleet may declare a canonical topic vocabulary; when it does, the bus
            // runs strict. Absent ⇒ soft-gate.
            canonical_topics: fleet.topics.clone(),
        },
    );

    // Teardown, after the run loop has restored the normal screen. Each worktree
    // is removed ONLY if clean and fully merged; anything with uncommitted or
    // unmerged work is kept and its path + branch reported, so nothing is ever
    // destroyed. A final prune clears records for the ones that were removed.
    if let Some((plans, base)) = &teardown {
        report_worktree_teardown(&cwd, plans, base);
        let _ = atrium::worktree::prune(&cwd);
    }
    exit
}

#[cfg(test)]
mod tests;
