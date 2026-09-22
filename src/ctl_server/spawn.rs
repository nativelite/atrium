//! `ctl spawn`: a visible worker in a new window, or `--here` beside the caller.

use crate::*;

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
