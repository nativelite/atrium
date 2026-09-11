use crate::*;
use atrium::layout::Tree;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

/// The `atrium fleet …` command family. `fleet up <name>` brings up a saved
/// roster; `fleet ls` lists the fleet names; anything else prints usage. Kept
/// separate from the hosted-program path — a fleet is atrium's own command, not a
/// child to run.
pub(crate) fn fleet_cmd(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("up") => match args.get(1) {
            Some(name) if !name.starts_with('-') => {
                // Anything after the name is atrium's own meta-flags — `--allow-ctl`
                // (so the fleet can coordinate over the control plane), `--trust
                // <policy>`, `--max-depth`. Parsed with the shared parser.
                match atrium::ctl::parse_flags(&args[2..]) {
                    Ok((allow_ctl, max_depth, trust, rest)) if rest.is_empty() => {
                        fleet_up(name, allow_ctl, max_depth, trust)
                    }
                    Ok((_, _, _, rest)) => {
                        eprintln!("atrium fleet up: unexpected argument {:?}", rest[0]);
                        ExitCode::FAILURE
                    }
                    Err(e) => {
                        eprintln!("atrium fleet up: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            _ => {
                eprintln!("atrium fleet up <name>: needs a fleet name (try `atrium fleet ls`)");
                ExitCode::FAILURE
            }
        },
        Some("ls") => fleet_ls(),
        Some("clean") => match args.get(1) {
            Some(name) if !name.starts_with('-') => fleet_clean(name),
            _ => {
                eprintln!("atrium fleet clean <name>: needs a fleet name (try `atrium fleet ls`)");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!(
                "usage: atrium fleet up <name> [--allow-ctl] [--trust <policy>] | \
                 atrium fleet clean <name> | atrium fleet ls"
            );
            ExitCode::FAILURE
        }
    }
}

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

/// Defang a fleet-file string before it reaches the terminal.
///
/// Every string in `atrium.fleet.json` is attacker-shaped in the workflow this
/// feature is built for - an agent writes the roster, a human reads the banner
/// and presses Enter - and the `json` parser decodes `\u001b`, so an agent name
/// or a path can carry a real ESC. Unfiltered it can clear the screen and
/// repaint a forged "every agent dir resolves inside" line over the disclosure
/// the human is about to approve.
pub(crate) fn fsan(s: &str) -> String {
    atrium::fleet::sanitize(s)
}

/// Path to the Claude Code settings file: ~/.claude/settings.json on all platforms.
fn settings_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.map(|h| h.join(".claude").join("settings.json"))
}

/// Returns `true` iff `v` is the JSON boolean `true`.
///
/// Strict boolean semantics: JSON numbers, strings, null, and arrays are all
/// `false` — this is not JavaScript truthiness. Only an explicit `true` in the
/// settings JSON counts as "enabled".
fn plugin_value_enabled(v: &json::Value) -> bool {
    v.as_bool().unwrap_or(false)
}

/// Check if context-mode@context-mode is enabled in ~/.claude/settings.json.
/// Returns true if enabled or settings not found/unreadable (best-effort).
/// Prints a warning and install hint if it's missing (but continues).
fn preflight_context_mode() {
    let path = match settings_path() {
        Some(p) => p,
        None => return, // No home dir, skip check
    };

    if !path.exists() {
        return; // Settings file doesn't exist yet, skip check
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return, // Can't read, skip check (best-effort)
    };

    let root = match json::parse(&text) {
        Ok(v) => v,
        Err(_) => return, // Malformed JSON, skip check
    };

    let obj = match root.as_object() {
        Some(o) => o,
        None => return, // Top-level not an object, skip check
    };

    let enabled = match obj.iter().find(|(k, _)| k == "enabledPlugins") {
        Some((_, v)) => v,
        None => return, // No enabledPlugins key, skip check
    };

    // enabledPlugins is either an array ["pkg@name", …] (older format) or an
    // object {"pkg@name": true, …} (current Claude Code format). Both are valid.
    let has_context_mode = if let Some(arr) = enabled.as_array() {
        arr.iter().any(|p| {
            p.as_str()
                .map_or(false, |s| s == "context-mode@context-mode")
        })
    } else if let Some(map) = enabled.as_object() {
        map.iter()
            .any(|(k, v)| k == "context-mode@context-mode" && plugin_value_enabled(v))
    } else {
        return; // unexpected shape, skip check
    };

    if !has_context_mode {
        eprintln!("atrium fleet: warning: context-mode@context-mode not in enabledPlugins");
        eprintln!("  run: /plugin install context-mode@context-mode");
    }
}

/// A control-plane-enabled fleet with no spawn-capable agent is almost always a
/// misconfigured lead: the coordinator comes up unable to create the very
/// teammates it exists to manage, and the gap stays invisible until the first
/// mid-run `ctl spawn` is denied. Worth a launch-time warning, where the roster
/// can still be fixed. `can_spawn` stays explicit and default-false by design —
/// this only surfaces the likely-misconfigured case, it grants nothing.
fn ctl_without_spawner(allow_ctl: bool, spawner_count: usize) -> bool {
    allow_ctl && spawner_count == 0
}

/// List the fleet names in the discovered fleet file, in file order. A missing
/// file or a malformed one is a clear error on stderr (non-zero exit).
pub(crate) fn fleet_ls() -> ExitCode {
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
    let names = fleets.names();
    if names.is_empty() {
        println!("(no fleets defined in {})", located.path.display());
    } else {
        // Defanged: the fleet file supplies these and the `json` parser decodes
        // `\u001b`, so a fleet name can carry a real ESC and clear the terminal
        // this is being read on. `fsan` escapes control characters and nothing
        // else - no truncation, no backslash doubling - so an ordinary name
        // still round-trips through `atrium fleet ls | xargs atrium fleet up`.
        for name in names {
            println!("{}", fsan(name));
        }
    }
    ExitCode::SUCCESS
}

/// `atrium fleet clean <name>` — reclaim the per-agent worktrees a fleet's run
/// left behind, now that they are merged.
///
/// Teardown at the end of `fleet up` already removes every worktree that is clean
/// and merged; the ones it *keeps* are exactly those with uncommitted or unmerged
/// work, reported so nothing is silently destroyed. After you merge (or discard)
/// that work, this command re-applies the same clean+merged test and removes the
/// ones that now pass — the deliberate second pass the teardown intentionally
/// refuses to make for you. Run it from the **same directory** the fleet was
/// launched in, so the worktree paths resolve identically. Never destroys a tree
/// that is still dirty or unmerged; those are reported and kept, as on exit.
pub(crate) fn fleet_clean(name: &str) -> ExitCode {
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
        Some(f) => f,
        None => {
            eprintln!("atrium fleet: no fleet named \"{}\"", fsan(name));
            return ExitCode::FAILURE;
        }
    };

    let plans = atrium::worktree::plan_worktrees(fleet, name, &cwd);
    if plans.is_empty() {
        println!(
            "atrium fleet: fleet \"{}\" declares no worktrees",
            fsan(name)
        );
        return ExitCode::SUCCESS;
    }
    if !atrium::worktree::is_git_repo(&cwd) {
        eprintln!(
            "atrium fleet: {} is not a git repo — nothing to clean",
            cwd.display()
        );
        return ExitCode::FAILURE;
    }
    let _ = atrium::worktree::prune(&cwd);
    let base = match atrium::worktree::base_ref(&cwd) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("atrium fleet: cannot read the repo's HEAD: {e}");
            return ExitCode::FAILURE;
        }
    };

    use atrium::worktree::Teardown;
    let mut kept = 0usize;
    for p in &plans {
        match atrium::worktree::remove_if_safe(&cwd, p, &base) {
            Ok(Teardown::Removed) => {
                println!("atrium fleet: removed worktree \"{}\"", fsan(&p.name))
            }
            Ok(Teardown::Missing) => {}
            Ok(Teardown::KeptDirty) => {
                kept += 1;
                eprintln!(
                    "atrium fleet: kept \"{}\" at {} (branch {}) — still has uncommitted changes",
                    fsan(&p.name),
                    atrium::fleet::show_path(&p.dir),
                    fsan(&p.branch)
                );
            }
            Ok(Teardown::KeptUnmerged) => {
                kept += 1;
                eprintln!(
                    "atrium fleet: kept \"{}\" at {} (branch {}) — still has unmerged commits",
                    fsan(&p.name),
                    atrium::fleet::show_path(&p.dir),
                    fsan(&p.branch)
                );
            }
            Err(e) => eprintln!("atrium fleet: worktree \"{}\": {e}", fsan(&p.name)),
        }
    }
    let _ = atrium::worktree::prune(&cwd);
    if kept > 0 {
        println!(
            "atrium fleet: {kept} worktree(s) kept — merge or discard their work, then re-run"
        );
    }
    ExitCode::SUCCESS
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
        match atrium::ctl::vet_spawn_argv(&a.cmd) {
            atrium::ctl::ArgvVerdict::Refused(why) => {
                eprintln!("atrium fleet: agent \"{}\": {why}", fsan(&a.name));
                return ExitCode::FAILURE;
            }
            atrium::ctl::ArgvVerdict::Ok { stripped, .. } if !stripped.is_empty() => {
                notes.push(format!(
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
    for note in &notes {
        eprintln!("atrium fleet: {note}");
    }
    // Always say the posture out loud. A fleet file can be authored by an agent
    // and skimmed by a human; a line naming what everything is about to run under
    // is the difference between reviewing it and assuming it.
    eprintln!(
        "atrium fleet: \"{}\" starting {} agent(s) at trust {}{}",
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
        eprintln!(
            "atrium fleet: warning: ctl is on but no agent may spawn teammates \
             — if this fleet coordinates by spawning, set \"can_spawn\": true on its lead"
        );
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
            eprintln!(
                "atrium fleet: warning: this fleet asks for per-agent worktrees, but {} is not a \
                 git repo — every agent shares the main tree",
                cwd.display()
            );
        }
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

/// Tear down a fleet's worktrees, printing a line for each that is kept (with a
/// reason) so the operator knows exactly what is left to merge or remove.
fn report_worktree_teardown(
    cwd: &std::path::Path,
    plans: &[atrium::worktree::WorktreePlan],
    base: &str,
) {
    use atrium::worktree::Teardown;
    for p in plans {
        match atrium::worktree::remove_if_safe(cwd, p, base) {
            Ok(Teardown::Removed) | Ok(Teardown::Missing) => {}
            Ok(Teardown::KeptDirty) => eprintln!(
                "atrium fleet: kept worktree \"{}\" at {} (branch {}) — it has uncommitted changes",
                fsan(&p.name),
                atrium::fleet::show_path(&p.dir),
                fsan(&p.branch)
            ),
            Ok(Teardown::KeptUnmerged) => eprintln!(
                "atrium fleet: kept worktree \"{}\" at {} (branch {}) — it has unmerged commits; \
                 merge or remove it deliberately",
                fsan(&p.name),
                atrium::fleet::show_path(&p.dir),
                fsan(&p.branch)
            ),
            Err(e) => eprintln!("atrium fleet: worktree \"{}\" teardown: {e}", fsan(&p.name)),
        }
    }
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
    agent: &atrium::fleet::Agent,
    disclosed: &atrium::fleet::AgentPlan,
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

/// Compute (and create) the stable per-fleet context directory.
///
/// Preference order:
///   1. `<cwd>\.atrium\ctx\<name>\` — local, beside the fleet file.
///   2. `%APPDATA%\atrium\ctx\<name>\` on Windows /
///      `$XDG_STATE_HOME/atrium/ctx/<name>/` on Unix
///      (falls back to `~/.local/state` when `XDG_STATE_HOME` is unset).
///
/// The directory is created (best-effort) before being returned; a failure
/// there is non-fatal — the fleet still launches, and spawn-wire injects
/// the path regardless so the agents can create their own files inside it.
pub(crate) fn fleet_ctx_dir(cwd: &std::path::Path, fleet_name: &str) -> std::path::PathBuf {
    // Sanitize for filesystem use: keep alphanumerics, hyphens, underscores,
    // and dots; replace everything else with '_'. An empty result gets a
    // placeholder so we never build a path with an empty component.
    let safe: String = fleet_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.is_empty() {
        "_fleet".to_string()
    } else {
        safe
    };

    // Primary: cwd-local .atrium/ctx/<name>/
    let primary = cwd.join(".atrium").join("ctx").join(&safe);
    if std::fs::create_dir_all(&primary).is_ok() {
        return primary;
    }

    // Fallback: user-scoped state directory.
    #[cfg(windows)]
    let fallback_base = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| cwd.to_path_buf());

    #[cfg(not(windows))]
    let fallback_base = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".local").join("state"))
                .unwrap_or_else(|_| cwd.to_path_buf())
        });

    let fallback = fallback_base.join("atrium").join("ctx").join(&safe);
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// Build the fleet's tiled window: one pane per agent, laid out on `grid`
/// (agents fill leaf ids `0..n` row-major). Each pane runs the agent's `cmd`
/// plus its fleet args (`--add-dir`/`--append-system-prompt`/`--model`/
/// `--effort`), under its identity (per-agent, else the fleet default), in its
/// resolved `cwd`. If any agent fails to spawn, the panes already started are
/// killed and the whole window is abandoned — never a partial fleet.
///
/// `fleet_name` and `cwd` are used to derive the shared context directory
/// (`ctx_dir`) via [`fleet_ctx_dir`]; spawn-wire threads `ctx_dir` into each
/// pane's environment via `context_env`.
pub(crate) fn spawn_fleet_window(
    fleet: &atrium::fleet::Fleet,
    plan: &atrium::fleet::Plan,
    grid: atrium::spawn::Grid,
    rows: u16,
    cols: u16,
    fleet_name: &str,
    cwd: &std::path::Path,
    worktrees: &[atrium::worktree::WorktreePlan],
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Window> {
    // Stable, writable directory shared by every pane in this fleet instance.
    // Computed once here so all panes agree on a single root; spawn-wire
    // picks this up and injects it as CONTEXT_MODE_DIR via context_env.
    let ctx_dir = fleet_ctx_dir(cwd, fleet_name);
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
        let (command, mut cwd, role) = fleet_launch(agent, disclosed);
        // If this agent belongs to a worktree, it runs in that worktree's dir —
        // atrium's own managed checkout off HEAD, overriding any fleet-file cwd so
        // the pane's gate and index are isolated from its teammates'. When
        // `worktrees` is empty (the common case) this never fires.
        if let Some(wt) = worktrees
            .iter()
            .find(|w| w.agents.iter().any(|a| a == &agent.name))
        {
            cwd = Some(wt.dir.to_string_lossy().into_owned());
        }
        // Per-agent context vars: derive the shared ctx_dir endpoint and the
        // agent's role name, then let context_env map (provider, share) →
        // CONTEXT_MODE_DIR / CONTEXT_MODE_SESSION_SUFFIX. Provider::None / absent context block → empty vec.
        let ctx_vars: Vec<(String, String)> = match &fleet.context {
            Some(cfg) => atrium::context::context_env(cfg, &ctx_dir, &agent.name),
            None => vec![],
        };
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
            &ctx_vars,
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

#[cfg(test)]
mod tests {
    use super::{ctl_without_spawner, plugin_value_enabled, preflight_context_mode, up_alias};

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn up_name_is_the_alias() {
        // `atrium --trust automode up context-build`: after the leading flags are
        // stripped, `rest` is `["up", "context-build"]` — the alias, launching
        // that fleet with the trust atrium already parsed.
        assert_eq!(
            up_alias(&v(&["up", "context-build"])),
            Some(Ok("context-build"))
        );
    }

    #[test]
    fn non_up_is_not_the_alias() {
        // A real hosted program is left alone (host it as before, never a fleet).
        assert_eq!(up_alias(&v(&["claude", "--model", "opus"])), None);
        assert_eq!(up_alias(&[]), None);
    }

    #[test]
    fn up_without_a_name_is_a_usage_error() {
        assert!(matches!(up_alias(&v(&["up"])), Some(Err(_))));
        // A flag where the name should be is not a name.
        assert!(matches!(up_alias(&v(&["up", "--trust"])), Some(Err(_))));
    }

    #[test]
    fn flags_after_the_name_are_rejected_with_a_pointer() {
        // Flags belong BEFORE `up`; trailing tokens are a usage error, not a
        // silent drop (dropping them is exactly how the posture went missing).
        let args = v(&["up", "context-build", "--allow-ctl"]);
        match up_alias(&args) {
            Some(Err(msg)) => {
                assert!(
                    msg.contains("before `up`"),
                    "message should redirect: {msg}"
                );
            }
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    #[test]
    fn preflight_silently_skips_missing_settings() {
        // The preflight check is best-effort: if settings.json doesn't exist,
        // no warning is printed. This test just verifies the function doesn't panic
        // when called — actual output verification would require mocking file I/O.
        preflight_context_mode();
    }

    // -- plugin_value_enabled pure seam --------------------------------------

    #[test]
    fn plugin_enabled_boolean_true_is_enabled() {
        assert!(plugin_value_enabled(&json::parse("true").unwrap()));
    }

    #[test]
    fn plugin_enabled_boolean_false_is_disabled() {
        assert!(!plugin_value_enabled(&json::parse("false").unwrap()));
    }

    #[test]
    fn plugin_enabled_null_is_disabled() {
        assert!(!plugin_value_enabled(&json::parse("null").unwrap()));
    }

    #[test]
    fn plugin_enabled_number_is_disabled() {
        // JS truthiness would consider 1 true — strict JSON boolean must not.
        assert!(!plugin_value_enabled(&json::parse("1").unwrap()));
    }

    #[test]
    fn plugin_enabled_string_is_disabled() {
        assert!(!plugin_value_enabled(&json::parse("\"yes\"").unwrap()));
    }

    #[test]
    fn ctl_fleet_without_a_spawner_warns() {
        // A coordinating fleet (ctl on) with no spawn-capable agent is the
        // misconfigured-lead case that stranded a real run.
        assert!(ctl_without_spawner(true, 0));
    }

    #[test]
    fn ctl_fleet_with_a_spawner_is_fine() {
        assert!(!ctl_without_spawner(true, 1));
    }

    #[test]
    fn no_ctl_never_warns() {
        // Without the control plane there are no teammates to spawn — silence.
        assert!(!ctl_without_spawner(false, 0));
    }
}
