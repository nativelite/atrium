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

/// The worktree a ctl spawn runs in, as `(cwd, norms)`, or the error reply when
/// it cannot be created.
fn worker_worktree(
    sp: &atrium::ctl::SpawnReq,
) -> Result<(Option<String>, Option<String>), atrium::ctl::Reply> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    worktree_spawn_params(&cwd, sp.worktree.as_deref())
        .map_err(|e| atrium::ctl::reply_err(&format!("spawn failed: {e}")))
}

/// Mark a freshly spawned pane as `caller`'s worker and build the spawn reply.
/// The one place a ctl spawn stamps its pane, for both placements.
fn adopt_worker(
    pane: &mut Pane,
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    note: Option<&str>,
) -> atrium::ctl::Reply {
    pane.role = sp.role.clone();
    pane.parent = caller;
    pane.depth = new_depth;
    pane.worktree = sp.worktree.clone();
    atrium::ctl::reply_spawned(
        pane.agent_id,
        sp.role.as_deref(),
        pane.session_id.as_deref(),
        note,
    )
}

/// `ctl spawn` (default): a visible worker in a brand-new window.
pub(crate) fn spawn_worker_window(
    cx: &mut CtlSession<'_>,
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
) -> atrium::ctl::Reply {
    let (rows, cols) = (cx.rows, cx.cols);
    let windows = &mut *cx.windows;
    let (wt_cwd, wt_norms) = match worker_worktree(sp) {
        Ok(pair) => pair,
        Err(reply) => return reply,
    };
    let mut flash = None;
    match spawn_window(
        PaneSpec {
            command: &sp.argv,
            id: 0,
            identity: sp.identity.as_deref(),
            cwd: wt_cwd.as_deref(),
            mode,
            extra_env: &[],
            extra_norms: wt_norms.as_deref(),
            deny: &[],
            codex: None,
        },
        rows,
        cols,
        &mut flash,
    ) {
        Ok(mut w) => {
            let reply = adopt_worker(&mut w.panes[0], sp, caller, new_depth, note);
            windows.push(w);
            // Size the new window's pane to its true rect now, so the agent
            // paints full-height immediately instead of at the rough spawn size
            // (which otherwise needs a manual terminal resize to correct).
            let last = windows.len() - 1;
            resize_window(&mut windows[last], rows, cols);
            reply
        }
        Err(e) => atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// `ctl spawn --here`: tile the worker *beside* the caller, in the caller's own
/// window, so a lead and its ICs sit in one view. Falls back to an error if the
/// caller's pane can't be located (nothing to sit beside). `--beside <pane>`
/// anchors on that pane instead when it is in the caller's window (the tmux
/// shim's `split-window -t %N`); otherwise the caller, and the reply says so.
pub(crate) fn spawn_worker_here(
    cx: &mut CtlSession<'_>,
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
) -> atrium::ctl::Reply {
    let (rows, cols) = (cx.rows, cx.cols);
    let windows = &mut *cx.windows;
    let Some(caller_id) = caller else {
        return atrium::ctl::reply_err(
            "`--here` needs a caller pane; run it from inside an atrium pane",
        );
    };
    // Locate the window holding the caller.
    let Some(wi) = windows
        .iter()
        .position(|w| w.panes.iter().any(|p| p.agent_id == caller_id))
    else {
        return atrium::ctl::reply_err("`--here`: caller pane not found (rerun without --here)");
    };

    let w = &mut windows[wi];
    // The pane the worker lands next to: `--beside <pane>` when that pane is in
    // the caller's window, else the caller itself. An anchor in another window
    // would not be a split of this one, so it falls back and the reply says so
    // rather than placing the worker somewhere the request did not name.
    let mut beside_note: Option<String> = None;
    let anchor_agent = match sp.beside {
        Some(b) if w.panes.iter().any(|p| p.agent_id == AgentId(b)) => AgentId(b),
        Some(b) => {
            beside_note = Some(format!(
                "--beside {b}: not a pane in the caller's window; placed beside the caller"
            ));
            caller_id
        }
        None => caller_id,
    };
    let note = match (note, beside_note.as_deref()) {
        (Some(n), Some(b)) => Some(format!("{n}; {b}")),
        (None, Some(b)) => Some(b.to_string()),
        (n, None) => n.map(str::to_string),
    };
    let note = note.as_deref();
    let new_id = w.next_id;
    // Rough half-cell inner size; the caller resizes the window right after.
    let (pr, pc) = (
        (rows.saturating_sub(1).max(1) / 2).saturating_sub(2),
        (cols / 2).saturating_sub(2),
    );
    let (wt_cwd, wt_norms) = match worker_worktree(sp) {
        Ok(pair) => pair,
        Err(reply) => return reply,
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
            codex: None,
        },
        pr.max(1),
        pc.max(1),
        &mut flash,
    ) {
        Ok(mut pane) => {
            let reply = adopt_worker(&mut pane, sp, caller, new_depth, note);
            // Re-tile the window into a balanced near-square grid over all its
            // panes (keeping their stable ids) with the worker BESIDE the anchor
            // (the caller, or `--beside`'s pane), rather than splitting the
            // caller alone or appending bottom-right.
            // A plain `split_pane` each time stacks every `--here` worker into
            // one column, so N of them degrade to an unusable `1×N` strip;
            // re-gridding keeps 2→1×2, 4→2×2, 6→2×3, 12→3×4, … balanced, in the
            // window's own orientation (rows, for a window `--here` started).
            // With `--size` (the shim's `-l 70%`) the anchor alone is split and
            // the worker takes that share of it; the ratio stays on the tree,
            // and later plain spawns re-grid only the region around their
            // anchor (`Tree::grow`), so a lead sized to 30 % keeps it while its
            // teammates tile inside the other 70 %.
            // Focus lands on the fresh worker, unless the caller said
            // `--no-focus` (the tmux shim's `-d`: a Claude Code lead spawning a
            // teammate must not move its human into it).
            let anchor = w
                .panes
                .iter()
                .find(|p| p.agent_id == anchor_agent)
                .map(|p| p.id)
                .unwrap_or(w.tree.focus());
            // A cell count is against the anchor's current width: `--here`
            // splits side by side, so the span is its columns.
            let share = sp.size.map(|sz| {
                let span = w
                    .tree
                    .rects(tiled_outer(rows, cols))
                    .into_iter()
                    .find(|(id, _)| *id == anchor)
                    .map(|(_, r)| r.cols)
                    .unwrap_or(cols as usize);
                sz.permille(span)
            });
            w.panes.push(pane);
            w.next_id += 1;
            w.tree = w.tree.grow(
                &mut w.grid_dir,
                anchor,
                new_id,
                layout::Dir::Vertical,
                share,
            );
            if sp.focus {
                w.tree.focus_pane(new_id);
            }
            w.zoomed = false;
            // Resize the whole window so both the caller and the fresh worker get
            // their exact inner rects — without this the worker keeps its rough
            // half-size and paints short (blank below), fixed only by a manual
            // terminal resize. Mirrors what the interactive split handlers do.
            resize_window(&mut windows[wi], rows, cols);
            reply
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
    let max_depth = cx.max_depth;
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
    // No command: the caller's own kind. A codex lead gets codex teammates
    // and a claude lead claude ones unless the spawn names the other after
    // `--`; from a pane that is no agent (a shell, the operator), claude.
    let mut notes: Vec<String> = Vec::new();
    if sp.argv.is_empty() {
        let own = caller
            .and_then(|cid| pane_by_agent(windows, cid))
            .filter(|p| atrium::bind::is_agent_stem(&p.title))
            .and_then(|p| p.argv.first().cloned())
            .unwrap_or_else(|| "claude".to_string());
        notes.push(format!(
            "no command given: spawned {own}, the caller's own kind"
        ));
        sp.argv = vec![own];
    }
    let stripped = match ctl::vet_spawn_argv(&sp.argv) {
        ctl::ArgvVerdict::Refused(why) => return ctl::reply_err(&why),
        ctl::ArgvVerdict::Ok { argv, stripped } => {
            sp.argv = argv;
            stripped
        }
    };
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
        spawn_worker_window(cx, &sp, caller, new_depth, effective, note)
    } else {
        spawn_worker_here(cx, &sp, caller, new_depth, effective, note)
    }
}

#[cfg(test)]
mod tests;
