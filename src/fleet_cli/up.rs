//! `atrium fleet up`: load a roster, show the operator what it will run and grant,
//! and bring it up once they acknowledge.

use super::clean::report_worktree_teardown;
use super::fsan;
use super::launch::spawn_fleet_window;
use super::preflight::{
    build_pool_line, ctl_without_spawner, deny_unbound, pane_cap_warning, preflight_context_mode,
};
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

/// `atrium fleet up <name>` — read the fleet file, build one tiled window with a
/// pane per agent (each in its `cwd`, under its identity, with its extra args),
/// and hand it to the run loop. Any error before spawning (no file, bad JSON,
/// unknown name, empty fleet, a bad grid, a missing `cwd`) is reported and
/// **nothing is spawned** — never a partial fleet.
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
    let located = match atrium::fleet::discover(&cwd) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("atrium fleet: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = match std::fs::read_to_string(&located.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("atrium fleet: cannot read {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleets = match atrium::fleet::parse(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("atrium fleet: {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleet = match fleets.get(name) {
        Some(f) => f.clone(),
        None => {
            let available = fleets
                .names()
                .iter()
                .map(|n| fsan(n))
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                "atrium fleet: no fleet named \"{}\" (available: {available})",
                fsan(name)
            );
            return ExitCode::FAILURE;
        }
    };
    // The global config's fleet defaults fill the keys this fleet left out
    // (its own keys and the environment still win), then the fleet's
    // `claude_aliases` are in force from here: the banner's non-claude
    // warnings, every spawn and later ctl spawns all judge a command by them.
    let fleet = atrium::config::get().apply_to_fleet(fleet);
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

    // A fleet may declare its own posture, and the command line wins when it says
    // anything. A fleet exists to run hands-off, so it NEEDS a posture — and
    // typing one on every launch is a flag you eventually get wrong. Declared in
    // the file it lives with the roster it applies to and is reviewable in a diff.
    //
    // Still a request, not an override: `set_trust_mode` publishes it, and the
    // ancestry cap has already lowered `trust` if this atrium is nested, so a fleet
    // asking for `skip` inside a `plan` session does not get it.
    // The file may switch the control plane on, as `--allow-ctl` does. A
    // coordinating fleet without ctl fails silently — panes come up and publish
    // into nothing — and that has already cost a run.
    let allow_ctl = allow_ctl || fleet.allow_ctl.unwrap_or(false);
    let trust = match (trust, fleet.trust.as_deref()) {
        (atrium::ctl::TrustMode::Off, Some(declared)) => {
            match atrium::ctl::TrustMode::from_policy_keyword(declared) {
                Some(m) => m,
                None => {
                    eprintln!(
                        "atrium fleet: fleet {name:?} declares trust {declared:?}, which is not \
                         one of plan, accept, automode, skip"
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
        (cli, _) => cli,
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
                    eprintln!(
                        "atrium fleet: agent \"{}\" declares trust {k:?}, which is not one of \
                         plan, accept, automode, skip",
                        fsan(&a.name)
                    );
                    return ExitCode::FAILURE;
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
    // order is chosen once, below. They are preflight warnings: loud, never fatal.
    let mut warnings: Vec<String> = Vec::new();
    for a in &fleet.agents {
        match atrium::ctl::vet_spawn_argv(&a.cmd) {
            atrium::ctl::ArgvVerdict::Refused(why) => {
                eprintln!("atrium fleet: agent \"{}\": {why}", fsan(&a.name));
                return ExitCode::FAILURE;
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
                eprintln!(
                    "atrium fleet: grid {:?} has {} cells but fleet \"{}\" has {n} agents",
                    fsan(spec),
                    g.total(),
                    fsan(name)
                );
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("atrium fleet: fleet \"{}\": {e}", fsan(name));
                return ExitCode::FAILURE;
            }
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
                eprintln!(
                    "atrium fleet: agent \"{}\": cwd {} is not a directory",
                    ap.label,
                    atrium::fleet::show_path(&g.given)
                );
                return ExitCode::FAILURE;
            }
        }
    }

    // Per-agent worktrees (#5): plan them now so they can be disclosed in the
    // banner, but touch no git until after the operator acknowledges. Active only
    // when the fleet asks for a worktree AND the cwd is a git repo — a non-repo
    // fleet (research, ops) is warned and left on the main tree, never blocked.
    let wt_plans = atrium::worktree::plan_worktrees(&fleet, name, &cwd);
    let wt_active = !wt_plans.is_empty() && atrium::worktree::is_git_repo(&cwd);

    // The disclosure, then the posture, then the verdict, then the Enter.
    //
    // Order is load-bearing and was measured: a six-agent roster's banner is
    // taller than a 24-row terminal, so whatever prints FIRST is what scrolls
    // away. The per-grant lines are the long, skimmable part; the posture, the
    // spawn capability and the verdict are the short, decisive part, so they go
    // last and are still on screen when the prompt appears.
    for line in plan.banner_lines() {
        eprintln!("atrium fleet: {line}");
    }
    // Always say the posture out loud. A fleet file can be authored by an agent
    // and skimmed by a human; a line naming what everything is about to run under
    // is the difference between reviewing it and assuming it.
    //
    // The compile budget is part of that posture: a fleet's builds are what
    // exhaust a machine, not its agents. Stated from the plan here; the pool
    // itself is created only once the operator approves (below).
    let (pool_suffix, pool_warning) = build_pool_line(
        atrium::buildpool::planned_size(fleet.build_jobs),
        atrium::buildpool::inherited(),
    );
    let memory = atrium::memguard::describe(
        atrium::memguard::planned_cap(fleet.memory_mb),
        atrium::memguard::enforcement(),
    );
    // Session-wide deny rules as every claude pane will carry them (built-ins
    // + ATRIUM_DENY + the fleet's); per-agent rules add to this.
    let mut session_deny = atrium::trust::parse_deny_env();
    session_deny.extend(fleet.deny.iter().cloned());
    let denied = atrium::trust::deny_args(&session_deny, &[]).len() - 1;
    eprintln!(
        "atrium fleet: \"{}\" starting {} agent(s) at trust {}{}{pool_suffix}, {memory}, \
         {denied} deny rules",
        fsan(name),
        fleet.agents.len(),
        trust.policy_label(),
        if allow_ctl { ", ctl on" } else { ", ctl OFF" }
    );
    // Deny rules only reach claude (codex has no equivalent flag). Say so when a
    // rule is aimed at an agent it can't bind, rather than let it read as enforced.
    let unbound = deny_unbound(&fleet);
    if !unbound.is_empty() {
        warnings.push(format!(
            "deny rules apply to claude agents only — not enforced for {}",
            unbound
                .iter()
                .map(|n| fsan(n))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(warning) = pool_warning {
        warnings.push(
            warning
                .strip_prefix("warning: ")
                .unwrap_or(&warning)
                .to_string(),
        );
    }
    // Name any agent that runs at a DIFFERENT posture than the session — part of
    // what the human approves (a mixed-model fleet often runs its haiku agents at
    // `accept` under an `automode` session).
    if !trust_overrides.is_empty() {
        let shown = trust_overrides
            .iter()
            .map(|(n, m)| format!("{n}={m}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("atrium fleet: per-agent trust overrides — {shown}");
    }

    // Who may create teammates is part of what the human approves, so say it.
    let spawners: Vec<String> = plan
        .agents
        .iter()
        .zip(fleet.agents.iter())
        .filter(|(_, a)| a.can_spawn.unwrap_or(false))
        .map(|(ap, _)| ap.label.clone())
        .collect();
    eprintln!(
        "atrium fleet: {} of {} may spawn teammates{}",
        spawners.len(),
        fleet.agents.len(),
        if spawners.is_empty() {
            String::new()
        } else {
            format!(" ({})", spawners.join(", "))
        }
    );
    // A control-plane fleet where nobody may spawn is almost always a
    // misconfigured lead: the coordinator comes up unable to create the very
    // teammates it exists to manage, and the gap stays invisible until the
    // first mid-run spawn is denied. Surface it here, where the roster can
    // still be fixed.
    if ctl_without_spawner(allow_ctl, spawners.len()) {
        warnings.push(
            "ctl is on but no agent may spawn teammates — if this fleet coordinates by \
             spawning, set \"can_spawn\": true on its lead"
                .to_string(),
        );
    }
    // The pane cap is a preflight warning, not a gate: the operator chose this
    // roster and the machine may well carry it. What the cap still does is refuse
    // a mid-run `ctl spawn` past it, and that is worth knowing before launch.
    if let Some(w) = pane_cap_warning(
        fleet.agents.len(),
        spawners.len(),
        atrium::resources::effective_cap(),
    ) {
        warnings.push(w);
    }
    // Per-agent worktrees: say which agents leave the main tree, where, and on
    // what branch — isolation the operator is approving as much as the dirs above.
    if !wt_plans.is_empty() {
        if wt_active {
            eprintln!(
                "atrium fleet: per-agent worktrees — {} off HEAD; auto-removed on exit only if \
                 clean + merged, else kept:",
                wt_plans.len()
            );
            for p in &wt_plans {
                let who = p
                    .agents
                    .iter()
                    .map(|a| fsan(a))
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "atrium fleet:   {} → {} (branch {}) [{who}]",
                    fsan(&p.name),
                    atrium::fleet::show_path(&p.dir),
                    fsan(&p.branch)
                );
            }
        } else {
            warnings.push(format!(
                "this fleet asks for per-agent worktrees, but {} is not a git repo — every \
                 agent shares the main tree",
                atrium::fleet::show_path(&cwd)
            ));
        }
    }
    // Every preflight warning, together, as the last thing before the verdict.
    let color = std::io::IsTerminal::is_terminal(&std::io::stderr())
        && std::env::var_os("NO_COLOR").is_none();
    for line in atrium::fleet::preflight_block(&warnings, color) {
        eprintln!("{line}");
    }
    // The last line before the block, so it cannot scroll: it NAMES the
    // destinations rather than counting them. A surviving summary line that
    // omits the payload is the one thing an operator is guaranteed to read and
    // the one thing that tells them nothing.
    eprintln!("atrium fleet: {}", plan.verdict());
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
    let mut wt_teardown: Option<(Vec<atrium::worktree::WorktreePlan>, String)> = None;
    if wt_active {
        let _ = atrium::worktree::prune(&cwd);
        let base = match atrium::worktree::base_ref(&cwd) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("atrium fleet: cannot read the repo's HEAD for worktrees: {e}");
                return ExitCode::FAILURE;
            }
        };
        let seeds = fleet.worktree_seed.as_deref().unwrap_or(&[]);
        for p in &wt_plans {
            match atrium::worktree::ensure(&cwd, p) {
                Ok(_) => {
                    for w in atrium::worktree::seed(&cwd, &p.dir, seeds) {
                        eprintln!("atrium fleet: worktree \"{}\": {w}", fsan(&p.name));
                    }
                    for w in atrium::worktree::junction_sibling_deps(&cwd, p) {
                        eprintln!("atrium fleet: worktree \"{}\": {w}", fsan(&p.name));
                    }
                }
                Err(e) => {
                    eprintln!(
                        "atrium fleet: could not create worktree \"{}\": {e}",
                        fsan(&p.name)
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
        wt_teardown = Some((wt_plans.clone(), base));
    }

    // Publish the trust policy before the fleet's panes are spawned (they read it
    // via `trust_mode()`), so every agent comes up under the resolved posture.
    set_trust_mode(trust);

    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("atrium: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
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
            eprintln!("atrium fleet: cannot start fleet \"{}\": {e}", fsan(name));
            return ExitCode::FAILURE;
        }
    };

    // New panes / splits opened later host a shell under the fleet's default
    // identity — a scratch pane in-role, not another copy of an agent.
    let scratch = vec![default_shell()];
    // ctl is opt-in for a fleet too (`atrium fleet up <name> --allow-ctl`), so the
    // roster can coordinate over the board/bus; without the flag it runs as before.
    let exit = run(
        &mut term,
        &scratch,
        fleet.identity.as_deref(),
        None,
        Some(window),
        allow_ctl,
        max_depth,
        trust,
        ctl_listener,
        // A fleet may declare a canonical topic vocabulary; when it does, the bus
        // runs strict. Absent ⇒ soft-gate.
        fleet.topics.clone(),
    );

    // Teardown, after the run loop has restored the normal screen. Each worktree
    // is removed ONLY if clean and fully merged; anything with uncommitted or
    // unmerged work is kept and its path + branch reported, so nothing is ever
    // destroyed. A final prune clears records for the ones that were removed.
    if let Some((plans, base)) = &wt_teardown {
        report_worktree_teardown(&cwd, plans, base);
        let _ = atrium::worktree::prune(&cwd);
    }
    exit
}

#[cfg(test)]
mod tests;
