//! `ctl spawn`: a visible worker in a new window, or `--here` beside the caller.

use super::dispatch::CtlSession;
use super::panes::{ctl_candidates, effective_mode, pane_by_agent, role_holder};
use crate::*;
use atrium::ctl;

/// Resolve an optional ad-hoc worktree name into `(cwd, norms)` for a ctl spawn.
///
/// When `wt_name` is `Some`, creates the worktree (idempotent) and returns its
/// directory as cwd plus the behavioral norms text. `None` → `Ok((None, None))`,
/// which leaves the spawned pane in atrium's own cwd with no extra norms.
/// Returns `Err` (with a human-readable message) when the worktree cannot be
/// created — the caller must surface this as a ctl error rather than spawning
/// a child into a directory that does not exist.
///
/// `junction_sibling_deps` is best-effort: failures are silently ignored so a
/// missing junction never prevents the worktree from being usable.
pub(crate) fn worktree_spawn_params(
    cwd: &std::path::Path,
    wt_name: Option<&str>,
) -> Result<(Option<String>, Option<String>), String> {
    let Some(name) = wt_name else {
        return Ok((None, None));
    };
    let plan = atrium::worktree::plan_for(cwd, name);
    atrium::worktree::ensure(cwd, &plan)
        .map_err(|e| format!("could not create worktree '{name}': {e}"))?;
    let _ = atrium::worktree::junction_sibling_deps(cwd, &plan);
    let dir = plan.dir.to_string_lossy().into_owned();
    let norms = atrium::worktree::worktree_norms(&plan.name, &plan.branch);
    Ok((Some(dir), Some(norms)))
}

/// `ctl spawn` (default): a visible worker in a brand-new window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_worker_window(
    windows: &mut Vec<Window>,
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    rows: u16,
    cols: u16,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
    job: &atrium::reap::SessionJob,
) -> atrium::ctl::Reply {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (wt_cwd, wt_norms) = match worktree_spawn_params(&cwd, sp.worktree.as_deref()) {
        Ok(pair) => pair,
        Err(e) => return atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    };
    let mut flash = None;
    match spawn_window(
        &sp.argv,
        rows,
        cols,
        windows.len(),
        sp.identity.as_deref(),
        mode,
        &mut flash,
        Some(job),
        wt_cwd.as_deref(),
        wt_norms.as_deref(),
    ) {
        Ok(mut w) => {
            let pane = &mut w.panes[0];
            pane.role = sp.role.clone();
            pane.parent = caller;
            pane.depth = new_depth;
            pane.worktree = sp.worktree.clone();
            let agent_id = pane.agent_id;
            let session = pane.session_id.clone();
            windows.push(w);
            // Size the new window's pane to its true rect now, so the agent
            // paints full-height immediately instead of at the rough spawn size
            // (which otherwise needs a manual terminal resize to correct).
            let last = windows.len() - 1;
            resize_window(&mut windows[last], rows, cols);
            atrium::ctl::reply_spawned(agent_id, sp.role.as_deref(), session.as_deref(), note)
        }
        Err(e) => atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// `ctl spawn --here`: tile the worker *beside* the caller, in the caller's own
/// window, so a lead and its ICs sit in one view. Falls back to an error if the
/// caller's pane can't be located (nothing to sit beside).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_worker_here(
    windows: &mut [Window],
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    rows: u16,
    cols: u16,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
    job: &atrium::reap::SessionJob,
) -> atrium::ctl::Reply {
    let Some(caller_id) = caller else {
        return atrium::ctl::reply_err(
            "`--here` needs a caller pane; run it from inside an atrium pane",
        );
    };
    // Locate the window holding the caller and that caller's per-window pane id.
    let Some((wi, caller_pane_id)) = windows.iter().enumerate().find_map(|(i, w)| {
        w.panes
            .iter()
            .find(|p| p.agent_id == caller_id)
            .map(|p| (i, p.id))
    }) else {
        return atrium::ctl::reply_err("`--here`: caller pane not found (rerun without --here)");
    };

    let w = &mut windows[wi];
    let new_id = w.next_id;
    // Rough half-cell inner size; the caller resizes the window right after.
    let (pr, pc) = (
        (rows.saturating_sub(1).max(1) / 2).saturating_sub(2),
        (cols / 2).saturating_sub(2),
    );
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (wt_cwd, wt_norms) = match worktree_spawn_params(&cwd, sp.worktree.as_deref()) {
        Ok(pair) => pair,
        Err(e) => return atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    };
    let mut flash = None;
    match spawn_pane_full(
        PaneSpec {
            command: &sp.argv,
            id: new_id,
            identity: sp.identity.as_deref(),
            cwd: wt_cwd.as_deref(),
            mode,
            extra_env: &[],
            extra_norms: wt_norms.as_deref(),
            deny: &[],
        },
        pr.max(1),
        pc.max(1),
        &mut flash,
    ) {
        Ok(mut pane) => {
            pane.role = sp.role.clone();
            pane.parent = caller;
            pane.depth = new_depth;
            pane.worktree = sp.worktree.clone();
            let agent_id = pane.agent_id;
            let session = pane.session_id.clone();
            // Enroll this ctl-spawned pane in the session Job synchronously (same
            // #94 guarantee as spawn_window; idempotent, unix no-op, graceful).
            job.assign(pane.pty.pid());
            w.panes.push(pane);
            w.next_id += 1;
            // Re-tile the WHOLE window into a balanced near-square grid over all
            // its panes (keeping their stable ids), rather than just splitting the
            // caller side-by-side. A plain `split_pane` each time stacks every
            // `--here` worker into one column, so N of them degrade to an
            // unusable `1×N` strip; re-gridding keeps 2→1×2, 4→2×2, 6→2×3,
            // 12→3×4, … balanced. Focus lands on the fresh worker.
            let _ = caller_pane_id; // (kept for the error message above)
            let ids: Vec<usize> = w.panes.iter().map(|p| p.id).collect();
            w.tree = Tree::grid_from_ids(&ids);
            w.tree.focus_pane(new_id);
            w.zoomed = false;
            // Resize the whole window so both the caller and the fresh worker get
            // their exact inner rects — without this the worker keeps its rough
            // half-size and paints short (blank below), fixed only by a manual
            // terminal resize. Mirrors what the interactive split handlers do.
            resize_window(&mut windows[wi], rows, cols);
            atrium::ctl::reply_spawned(agent_id, sp.role.as_deref(), session.as_deref(), note)
        }
        Err(e) => atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// `ctl spawn`: the guards, in order — may this caller create teammates, is the
/// role free, is the argv one a teammate may run, the effective trust mode, the
/// depth cap, the pane cap, and credential delegation — then a new window, or
/// `--here` beside the caller.
pub(crate) fn spawn(
    mut sp: atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> atrium::ctl::Reply {
    let windows = &mut *cx.windows;
    let (rows, cols, max_depth, job) = (cx.rows, cx.cols, cx.max_depth, cx.job);
    let (extra_allow, session_identity) = (cx.extra_allow, cx.session_identity);
    // Capability check, FIRST. Creating teammates used to be implied by
    // having ctl access at all - so every agent in a fleet could do it,
    // and in a seven-agent review fleet all seven could, when only the
    // lead should. A depth cap bounds how FAR fan-out goes; it never says
    // who may start it.
    //
    // Before the depth cap and the trust ceiling, not instead of them: a
    // pane that may spawn is still capped on both.
    if let Some(c) = caller {
        let allowed = pane_by_agent(windows, c)
            .map(|p| p.can_spawn)
            .unwrap_or(false);
        if !allowed {
            return ctl::reply_err(
                "this agent is not permitted to create teammates \
                     (set \"can_spawn\": true for it in the fleet file)",
            );
        }
    }
    // A role names exactly one live pane. The bus keys subscriptions by
    // role, and a bus event wakes a pane because *its label* subscribed
    // — so a second pane wearing an existing role would subscribe on
    // the first one's behalf and route wakes into it, and its own posts
    // would read as the first one's. `--to` addressing would be
    // ambiguous too. Refused for everyone, the operator included: the
    // collision is what is unsafe, not who caused it.
    if let Some(role) = sp.role.as_deref() {
        if let Some(holder) = role_holder(role, &ctl_candidates(windows)) {
            return ctl::reply_err(&format!(
                "role {role:?} already names pane {holder}; roles are unique while a \
                     pane lives (kill it, wait for it to exit, or pick another name)"
            ));
        }
    }
    // atrium owns the permission posture. Two layers, both surfaced (never
    // silent) in the reply `note`:
    //
    //  1. Strip RAW claude permission flags the agent slipped into the argv
    //     (`--dangerously-skip-permissions`, `--permission-mode`, …). Agents
    //     request a mode through `--mode`, not raw flags, so atrium stays the
    //     single source of truth.
    //  2. Resolve the effective mode from the per-spawn `--mode` under the
    //     session policy: the **operator** (the human's root pane) may set
    //     ANY mode (elevation is the human directing); a non-operator
    //     **worker** is capped at the policy — it may match or de-escalate
    //     but never elevate itself. No `--mode` ⇒ inherit the policy.
    // Vet before anything else: an unknown vendor or a flag a teammate
    // may not choose is refused outright, not quietly cleaned. Stripping
    // only ever covered three names, and the surface it guards — MCP
    // servers, plugin dirs, settings files, all of which execute code at
    // startup — is far larger than that.
    let stripped = match ctl::vet_spawn_argv(&sp.argv) {
        ctl::ArgvVerdict::Refused(why) => return ctl::reply_err(&why),
        ctl::ArgvVerdict::Ok { argv, stripped } => {
            sp.argv = argv;
            stripped
        }
    };
    let mut notes: Vec<String> = Vec::new();
    if !stripped.is_empty() {
        notes.push(format!(
            "ignored raw {} — request a mode with --mode instead",
            stripped.join(", ")
        ));
    }
    let policy = trust_mode();
    let (effective, cap_note) = effective_mode(sp.mode, policy);
    if let Some(n) = cap_note {
        notes.push(n);
    }
    let note = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    let note = note.as_deref();
    let caller_depth = caller
        .and_then(|cid| pane_by_agent(windows, cid))
        .map(|p| p.depth)
        .unwrap_or(0);
    let new_depth = match ctl::evaluate_spawn(&sp.argv, caller_depth, max_depth, extra_allow) {
        Ok(d) => d,
        Err(denied) => return ctl::reply_err(&denied.message()),
    };
    // Pane cap (host-resource guard): the depth guard bounds recursion; this
    // bounds total *breadth* so a runaway fan-out can't exhaust the machine
    // (agent processes dominate RAM, not atrium). Host-derived, overridable
    // with ATRIUM_MAX_PANES.
    let live = windows.iter().map(|w| w.panes.len()).sum::<usize>();
    let cap = atrium::resources::effective_cap();
    if live >= cap {
        return ctl::reply_err(&format!(
            "pane cap reached ({live}/{cap}) — reap an agent or raise it with ATRIUM_MAX_PANES"
        ));
    }
    // Credential-delegation guard (§5): a worker may only pass down an
    // identity it itself holds (its own or the session default); the
    // operator delegates anything.
    let caller_identity = caller
        .and_then(|cid| pane_by_agent(windows, cid))
        .and_then(|p| p.identity.clone());
    if let Err(msg) = ctl::delegation_allowed(
        sp.identity.as_deref(),
        caller_identity.as_deref(),
        session_identity,
        privileged,
    ) {
        return ctl::reply_err(&msg);
    }
    if sp.new_window {
        spawn_worker_window(
            windows, &sp, caller, new_depth, rows, cols, effective, note, job,
        )
    } else {
        spawn_worker_here(
            windows, &sp, caller, new_depth, rows, cols, effective, note, job,
        )
    }
}
