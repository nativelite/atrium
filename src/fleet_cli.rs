use crate::*;
use amux::layout::Tree;
use std::process::ExitCode;
use std::time::Instant;

/// The `amux fleet …` command family. `fleet up <name>` brings up a saved
/// roster; `fleet ls` lists the fleet names; anything else prints usage. Kept
/// separate from the hosted-program path — a fleet is amux's own command, not a
/// child to run.
pub(crate) fn fleet_cmd(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("up") => match args.get(1) {
            Some(name) if !name.starts_with('-') => {
                // Anything after the name is amux's own meta-flags — `--allow-ctl`
                // (so the fleet can coordinate over the control plane), `--trust
                // <policy>`, `--max-depth`. Parsed with the shared parser.
                match amux::ctl::parse_flags(&args[2..]) {
                    Ok((allow_ctl, max_depth, trust, rest)) if rest.is_empty() => {
                        fleet_up(name, allow_ctl, max_depth, trust)
                    }
                    Ok((_, _, _, rest)) => {
                        eprintln!("amux fleet up: unexpected argument {:?}", rest[0]);
                        ExitCode::FAILURE
                    }
                    Err(e) => {
                        eprintln!("amux fleet up: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            _ => {
                eprintln!("amux fleet up <name>: needs a fleet name (try `amux fleet ls`)");
                ExitCode::FAILURE
            }
        },
        Some("ls") => fleet_ls(),
        _ => {
            eprintln!(
                "usage: amux fleet up <name> [--allow-ctl] [--trust <policy>] | amux fleet ls"
            );
            ExitCode::FAILURE
        }
    }
}

/// Defang a fleet-file string before it reaches the terminal.
///
/// Every string in `amux.fleet.json` is attacker-shaped in the workflow this
/// feature is built for - an agent writes the roster, a human reads the banner
/// and presses Enter - and the `json` parser decodes `\u001b`, so an agent name
/// or a path can carry a real ESC. Unfiltered it can clear the screen and
/// repaint a forged "every agent dir resolves inside" line over the disclosure
/// the human is about to approve.
pub(crate) fn fsan(s: &str) -> String {
    amux::fleet::sanitize(s)
}

/// List the fleet names in the discovered fleet file, in file order. A missing
/// file or a malformed one is a clear error on stderr (non-zero exit).
pub(crate) fn fleet_ls() -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let located = match amux::fleet::discover(&cwd) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("amux fleet: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = match std::fs::read_to_string(&located.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux fleet: cannot read {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleets = match amux::fleet::parse(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("amux fleet: {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let names = fleets.names();
    if names.is_empty() {
        println!("(no fleets defined in {})", located.path.display());
    } else {
        // Defanged: the fleet file supplies these and the `json` parser decodes
        // `\u001b`, so a fleet name can carry a real ESC and clear the terminal
        // this is being read on. `fsan` escapes control characters and nothing
        // else - no truncation, no backslash doubling - so an ordinary name
        // still round-trips through `amux fleet ls | xargs amux fleet up`.
        for name in names {
            println!("{}", fsan(name));
        }
    }
    ExitCode::SUCCESS
}

/// `amux fleet up <name>` — read the fleet file, build one tiled window with a
/// pane per agent (each in its `cwd`, under its identity, with its extra args),
/// and hand it to the run loop. Any error before spawning (no file, bad JSON,
/// unknown name, empty fleet, a bad grid, a missing `cwd`) is reported and
/// **nothing is spawned** — never a partial fleet.
pub(crate) fn fleet_up(
    name: &str,
    allow_ctl: bool,
    max_depth: usize,
    trust: amux::ctl::TrustMode,
) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let located = match amux::fleet::discover(&cwd) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("amux fleet: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = match std::fs::read_to_string(&located.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux fleet: cannot read {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleets = match amux::fleet::parse(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("amux fleet: {}: {e}", located.path.display());
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
                "amux fleet: no fleet named \"{}\" (available: {available})",
                fsan(name)
            );
            return ExitCode::FAILURE;
        }
    };

    // A fleet may declare its own posture, and the command line wins when it says
    // anything. A fleet exists to run hands-off, so it NEEDS a posture — and
    // typing one on every launch is a flag you eventually get wrong. Declared in
    // the file it lives with the roster it applies to and is reviewable in a diff.
    //
    // Still a request, not an override: `set_trust_mode` publishes it, and the
    // ancestry cap has already lowered `trust` if this amux is nested, so a fleet
    // asking for `skip` inside a `plan` session does not get it.
    // The file may switch the control plane on, as `--allow-ctl` does. A
    // coordinating fleet without ctl fails silently — panes come up and publish
    // into nothing — and that has already cost a run.
    let allow_ctl = allow_ctl || fleet.allow_ctl.unwrap_or(false);
    let trust = match (trust, fleet.trust.as_deref()) {
        (amux::ctl::TrustMode::Off, Some(declared)) => {
            match amux::ctl::TrustMode::from_policy_keyword(declared) {
                Some(m) => m,
                None => {
                    eprintln!(
                        "amux fleet: fleet {name:?} declares trust {declared:?}, which is not \
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
    if trust == amux::ctl::TrustMode::Skip && !confirm_skip_permissions() {
        eprintln!("amux fleet: aborted.");
        return ExitCode::SUCCESS;
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
    // order is chosen once, below.
    let mut notes: Vec<String> = Vec::new();
    for a in &fleet.agents {
        match amux::ctl::vet_spawn_argv(&a.cmd) {
            amux::ctl::ArgvVerdict::Refused(why) => {
                eprintln!("amux fleet: agent \"{}\": {why}", fsan(&a.name));
                return ExitCode::FAILURE;
            }
            amux::ctl::ArgvVerdict::Ok { stripped, .. } if !stripped.is_empty() => {
                notes.push(format!(
                    "agent \"{}\": ignoring {} — set the posture with \"trust\" instead",
                    fsan(&a.name),
                    fsan(&stripped.join(", "))
                ));
            }
            amux::ctl::ArgvVerdict::Ok { .. } => {}
        }
    }

    // Grid: explicit `RxC` (must fit the agent count) or an auto balanced grid.
    // Checked BEFORE the banner: never ask a human to approve a launch that
    // cannot happen anyway.
    let n = fleet.agents.len();
    let grid = match &fleet.grid {
        Some(spec) => match amux::spawn::Grid::parse_spec(spec) {
            Ok(g) if g.total() >= n => g,
            Ok(g) => {
                eprintln!(
                    "amux fleet: grid {:?} has {} cells but fleet \"{}\" has {n} agents",
                    fsan(spec),
                    g.total(),
                    fsan(name)
                );
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("amux fleet: fleet \"{}\": {e}", fsan(name));
                return ExitCode::FAILURE;
            }
        },
        None => amux::spawn::Grid::balanced(n.max(2)),
    };

    // Resolve every directory this roster grants — ONCE. `plan` is what the
    // banner prints and what `spawn_fleet_window` launches with, so the two
    // cannot drift: resolving a second time inside the spawn is how an
    // acknowledged "inside" became a live grant on a credential store when a
    // symlink was re-pointed during the operator's Enter window.
    let anchor = amux::fleet::anchor_for(
        &located.dir,
        &cwd,
        located.global,
        amux::fleet::home_dir().as_deref(),
    );
    let plan = amux::fleet::Plan::build(
        &fleet,
        &located.path,
        &located.dir,
        anchor,
        &amux::fleet::Stores::live(),
    );

    // Validate every agent's cwd *before* spawning anything, so a bad path never
    // leaves a half-open fleet. `is_dir`, not "exists": a cwd naming a regular
    // file passes an existence check, then fails in the pty spawn with ENOTDIR
    // after raw mode is on and panes are already up.
    for ap in &plan.agents {
        if let Some(g) = &ap.cwd {
            if !g.given.is_dir() {
                eprintln!(
                    "amux fleet: agent \"{}\": cwd {} is not a directory",
                    ap.label,
                    amux::fleet::show_path(&g.given)
                );
                return ExitCode::FAILURE;
            }
        }
    }

    // The disclosure, then the posture, then the verdict, then the Enter.
    //
    // Order is load-bearing and was measured: a six-agent roster's banner is
    // taller than a 24-row terminal, so whatever prints FIRST is what scrolls
    // away. The per-grant lines are the long, skimmable part; the posture, the
    // spawn capability and the verdict are the short, decisive part, so they go
    // last and are still on screen when the prompt appears.
    for line in plan.banner_lines() {
        eprintln!("amux fleet: {line}");
    }
    for note in &notes {
        eprintln!("amux fleet: {note}");
    }
    // Always say the posture out loud. A fleet file can be authored by an agent
    // and skimmed by a human; a line naming what everything is about to run under
    // is the difference between reviewing it and assuming it.
    eprintln!(
        "amux fleet: \"{}\" starting {} agent(s) at trust {}{}",
        fsan(name),
        fleet.agents.len(),
        trust.policy_label(),
        if allow_ctl { ", ctl on" } else { ", ctl OFF" }
    );

    // Who may create teammates is part of what the human approves, so say it.
    let spawners: Vec<String> = plan
        .agents
        .iter()
        .zip(fleet.agents.iter())
        .filter(|(_, a)| a.can_spawn.unwrap_or(false))
        .map(|(ap, _)| ap.label.clone())
        .collect();
    eprintln!(
        "amux fleet: {} of {} may spawn teammates{}",
        spawners.len(),
        fleet.agents.len(),
        if spawners.is_empty() {
            String::new()
        } else {
            format!(" ({})", spawners.join(", "))
        }
    );
    // The last line before the block, so it cannot scroll: it NAMES the
    // destinations rather than counting them. A surviving summary line that
    // omits the payload is the one thing an operator is guaranteed to read and
    // the one thing that tells them nothing.
    eprintln!("amux fleet: {}", plan.verdict());
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
    // Skipped when stdin is not a terminal (a script, CI) or `AMUX_YES=1`, since
    // there is nobody to ask; the banner still prints for the log.
    if !fleet_ack() {
        eprintln!("amux fleet: aborted.");
        return ExitCode::SUCCESS;
    }
    // Publish the trust policy before the fleet's panes are spawned (they read it
    // via `trust_mode()`), so every agent comes up under the resolved posture.
    set_trust_mode(trust);

    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (rows, cols) = term.size().unwrap_or((24, 80));

    let mut flash: Option<(String, Instant)> = None;
    // Bind the ctl endpoint BEFORE spawning the fleet's panes, so each agent is
    // born with `AMUX_CTL`/`AMUX_PANE` in its env and can drive the board/bus.
    // (The default path binds inside run() after its single spawn; a fleet spawns
    // its whole roster up front, so it must publish the address first.)
    let ctl_listener = if allow_ctl {
        bind_ctl(&mut flash)
    } else {
        None
    };
    let window = match spawn_fleet_window(&fleet, &plan, grid, rows, cols, &mut flash) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("amux fleet: cannot start fleet \"{}\": {e}", fsan(name));
            return ExitCode::FAILURE;
        }
    };

    // New panes / splits opened later host a shell under the fleet's default
    // identity — a scratch pane in-role, not another copy of an agent.
    let scratch = vec![default_shell()];
    // ctl is opt-in for a fleet too (`amux fleet up <name> --allow-ctl`), so the
    // roster can coordinate over the board/bus; without the flag it runs as before.
    run(
        &mut term,
        &scratch,
        fleet.identity.as_deref(),
        None,
        Some(window),
        allow_ctl,
        max_depth,
        trust,
        ctl_listener,
    )
}

/// The argv and working directory one fleet agent is actually spawned with.
///
/// Split out as a pure seam because this is the join between what the operator
/// approved and what runs. The directories come from the disclosed plan - the
/// paths already resolved through symlinks - NOT from the file's strings
/// re-resolved here. Re-resolving was a second, independent computation of the
/// same question, and a symlink re-pointed between the banner and the spawn made
/// the two answers differ: the operator acknowledged "inside" and the child got
/// a credential store. `spawn_fleet_window` is no longer handed the fleet
/// directory at all, so it cannot resolve anything a second time even by
/// accident.
pub(crate) fn fleet_launch(
    agent: &amux::fleet::Agent,
    disclosed: &amux::fleet::AgentPlan,
) -> (Vec<String>, Option<String>, String) {
    let dirs: Vec<String> = disclosed
        .add_dirs
        .iter()
        .map(|g| g.given.to_string_lossy().into_owned())
        .collect();
    let cwd = disclosed
        .cwd
        .as_ref()
        .map(|g| g.given.to_string_lossy().into_owned());
    // The role label comes from the plan too, and for the same reason it is
    // defanged there: `role` is painted straight into the status bar and the
    // overview by a painter that filters nothing, so an agent name carrying an
    // ESC would repaint the live TUI - the crossing the banner closes one screen
    // earlier.
    (agent.args(&dirs), cwd, disclosed.label.clone())
}

/// Build the fleet's tiled window: one pane per agent, laid out on `grid`
/// (agents fill leaf ids `0..n` row-major). Each pane runs the agent's `cmd`
/// plus its fleet args (`--add-dir`/`--append-system-prompt`/`--model`/
/// `--effort`), under its identity (per-agent, else the fleet default), in its
/// resolved `cwd`. If any agent fails to spawn, the panes already started are
/// killed and the whole window is abandoned — never a partial fleet.
pub(crate) fn spawn_fleet_window(
    fleet: &amux::fleet::Fleet,
    plan: &amux::fleet::Plan,
    grid: amux::spawn::Grid,
    rows: u16,
    cols: u16,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Window> {
    let tree = Tree::grid(grid.rows, grid.cols);
    // Rough per-cell inner size (minus the one-cell border on each side); the
    // caller's resize_window fixes it exactly right after.
    let cell_rows = ((rows.saturating_sub(1).max(1) as usize / grid.rows.max(1)).saturating_sub(2))
        .max(1) as u16;
    let cell_cols = ((cols as usize / grid.cols.max(1)).saturating_sub(2)).max(1) as u16;

    let mut panes: Vec<Pane> = Vec::with_capacity(fleet.agents.len());
    // Zipped, not indexed: `plan` holds exactly one entry per agent in the same
    // order, and zipping makes that structural instead of a subscript that a
    // later edit can knock out of alignment.
    for ((id, agent), disclosed) in fleet.agents.iter().enumerate().zip(plan.agents.iter()) {
        // A fleet agent may NOT create teammates unless its entry says so. The
        // roster is the definition of the run, so the capability belongs in it -
        // and the safe default is the one that surprises nobody.
        let can_spawn = agent.can_spawn.unwrap_or(false);
        let (command, cwd, role) = fleet_launch(agent, disclosed);
        // Per-agent identity else the fleet default.
        let identity = agent.identity.as_deref().or(fleet.identity.as_deref());
        match spawn_pane_full(
            &command,
            cell_rows,
            cell_cols,
            id,
            identity,
            cwd.as_deref(),
            trust_mode(),
            flash,
        ) {
            Ok(mut pane) => {
                // Tag the pane with the agent's fleet name as its role, so the
                // board/bus attribution (`by`/`from`), the overview, and the bar
                // all show the persona (`lead`, `ada`) instead of `pane N`.
                pane.role = Some(role.clone());
                // Capability, from the roster. `spawn_pane_full` defaults to the
                // human-pane behaviour (true); a fleet agent gets only what its
                // entry declares.
                pane.can_spawn = can_spawn;
                panes.push(pane);
            }
            Err(e) => {
                for p in panes.iter_mut() {
                    let _ = p.pty.kill();
                }
                return Err(e);
            }
        }
    }
    Ok(Window {
        panes,
        tree,
        zoomed: false,
        next_id: fleet.agents.len(),
    })
}
