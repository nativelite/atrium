//! Turning an approved roster into panes: each agent's argv and directory, the
//! fleet's context directory, and the tiled window.

use crate::*;

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
    //
    // The governed permission flags in `cmd` are dropped here, which is what the
    // banner's "ignoring …" line promises: atrium sets the posture itself. Only
    // `cmd` is sanitized — the fields appended after it are the file's own
    // values, and a prompt may legitimately mention a flag's name.
    let (cmd, _) = atrium::ctl::sanitize_spawn_argv(&agent.cmd);
    let mut argv = agent.args(&dirs);
    argv.splice(..agent.cmd.len(), cmd);
    (argv, cwd, disclosed.label.clone())
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

/// Where an agent that belongs to a worktree runs, and what it is told about
/// it: the worktree's directory (which replaces any fleet-file `cwd`) and the
/// worktree's behavioral norms. `None` for an agent that stays on the main tree.
pub(super) fn worktree_placement(
    worktrees: &[atrium::worktree::WorktreePlan],
    agent: &str,
) -> Option<(String, String)> {
    worktrees
        .iter()
        .find(|w| w.agents.iter().any(|a| a == agent))
        .map(|wt| {
            (
                wt.dir.to_string_lossy().into_owned(),
                atrium::worktree::worktree_norms(&wt.name, &wt.branch),
            )
        })
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
        // Per-agent trust: an agent may override the session posture (capped to it),
        // so a haiku agent can run at `accept` under an `automode` session. No
        // override → the session mode, exactly as before.
        let agent_mode = match agent.trust.as_deref() {
            Some(k) => {
                crate::effective_mode(atrium::ctl::TrustMode::from_policy_keyword(k), trust_mode())
                    .0
            }
            None => trust_mode(),
        };
        let (command, mut cwd, role) = fleet_launch(agent, disclosed);
        // If this agent belongs to a worktree, it runs in that worktree's dir —
        // atrium's own managed checkout off HEAD, overriding any fleet-file cwd so
        // the pane's gate and index are isolated from its teammates'. When
        // `worktrees` is empty (the common case) this never fires.
        let mut pane_norms: Option<String> = None;
        if let Some((dir, norms)) = worktree_placement(worktrees, &agent.name) {
            cwd = Some(dir);
            // Worktree-specific behavioral norms are passed to spawn_pane_full so
            // they are folded into a SINGLE --append-system-prompt block with the
            // ctl directive (last-wins on the installed claude — two separate flags
            // silently drop the first).
            pane_norms = Some(norms);
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
            PaneSpec {
                command: &command,
                id,
                identity,
                cwd: cwd.as_deref(),
                mode: agent_mode,
                extra_env: &ctx_vars,
                extra_norms: pane_norms.as_deref(),
                deny: &agent.deny,
            },
            cell_rows,
            cell_cols,
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
                // `fleet_launch` appends the kickoff as the last argument; a resume
                // needs to know that to leave it out.
                pane.kickoff = agent.kickoff.is_some();
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
mod tests;
