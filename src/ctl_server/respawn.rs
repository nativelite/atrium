//! `ctl respawn`: restart a pane's process in place, as a new session.

use super::dispatch::{scope_denied, CtlSession};
use super::panes::{ctl_candidates, effective_mode, pane_by_agent, pane_by_agent_mut};
use super::sends::{repoint_parents, retarget_sends};
use super::spawn::worktree_spawn_params;
use crate::*;
use atrium::ctl;

/// `ctl respawn`: replace a target's process with a fresh one in the same tile —
/// same command (as a new session), posture, deny rules, context and place, or a
/// named worktree — keeping its role, parent and depth. Its children and queued
/// sends follow it to its new agent id.
pub(crate) fn respawn(
    rr: ctl::RespawnReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    let (windows, pending) = (&mut *cx.windows, &mut *cx.pending);
    let (rows, cols) = (cx.rows, cx.cols);
    let candidates = ctl_candidates(windows);
    let id = match ctl::resolve_target(&rr.target, &candidates) {
        Ok(id) => id,
        Err(e) => return ctl::reply_err(&e),
    };
    if let Some(deny) = scope_denied(windows, caller, privileged, id) {
        return deny;
    }
    // Capture the info we need before mutating the pane.
    let (pane_slot_id, cmd, identity_name, deny, mode, context_env, place) = {
        let Some(p) = pane_by_agent(windows, id) else {
            return ctl::reply_err("respawn: target pane not found");
        };
        (
            p.id,
            respawn_argv(&p.argv, &p.title),
            p.identity.clone(),
            p.deny.clone(),
            p.mode,
            p.context_env.clone(),
            (p.cwd.clone(), p.norms.clone(), p.worktree.clone()),
        )
    };
    // Where the new process runs. A named worktree is created (idempotent)
    // and used; otherwise the pane restarts where it was, with its own
    // worktree instructions. It used to land in atrium's cwd with none, so
    // restarting a worktree agent moved it onto the main tree.
    let (new_cwd, norms, worktree) = match rr.worktree.as_deref() {
        Some(name) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            match worktree_spawn_params(&cwd, Some(name)) {
                Ok((c, n)) => (c, n, Some(name.to_string())),
                Err(e) => return ctl::reply_err(&format!("spawn failed: {e}")),
            }
        }
        None => place,
    };
    // The replacement is born at its tile's inner size, not the
    // terminal's. Spawned at the terminal's size, its emulator was larger
    // than the tile it is blitted into: the text was clipped on the right
    // and the bottom rows — claude's input box — sat outside the copied
    // rectangle until a manual terminal resize re-tiled the window.
    let Some(wi) = windows
        .iter()
        .position(|w| w.panes.iter().any(|p| p.agent_id == id))
    else {
        return ctl::reply_err("respawn: target pane not found");
    };
    let (pane_rows, pane_cols) = pane_inner_size(
        windows[wi].tiled(),
        &windows[wi].tree,
        pane_slot_id,
        rows,
        cols,
    );
    let mut flash = None;
    let new_pane = match spawn_pane_full(
        PaneSpec {
            command: &cmd,
            id: pane_slot_id,
            identity: identity_name.as_deref(),
            cwd: new_cwd.as_deref(),
            // The pane's own posture and deny rules survive a respawn. They
            // used to be replaced by the session ceiling and no rules at all,
            // so respawning a restricted fleet agent quietly lifted its limits.
            mode: effective_mode(Some(mode), trust_mode()).0,
            // The pane's context store, so a restarted agent keeps its
            // knowledge base (a fresh session, not a fresh memory).
            extra_env: &context_env,
            extra_norms: norms.as_deref(),
            deny: &deny,
        },
        pane_rows,
        pane_cols,
        &mut flash,
    ) {
        Ok(p) => p,
        Err(e) => return ctl::reply_err(&format!("respawn failed: {e}")),
    };
    // In-place replacement: kill the old child then swap in the new pty,
    // term, and session fields. Role/parent/depth/can_spawn are preserved.
    let p = pane_by_agent_mut(windows, id).unwrap();
    let _ = p.pty.kill();
    // The old reader goes with the old child, and before its `Pty` drops
    // (see `Pane::inbox`). Kept, it went on reading the dead pty, and since
    // the loop only starts a reader for a pane with none, nothing the new
    // process wrote was ever shown.
    p.inbox = None;
    p.pty = new_pane.pty;
    p.term = new_pane.term;
    p.filter = new_pane.filter;
    p.exited = false;
    p.session_id = new_pane.session_id.clone();
    p.launch_ms = new_pane.launch_ms;
    p.cwd = new_pane.cwd;
    // What the snapshot records must describe the process now running: its
    // command, where it runs, its worktree instructions and context
    // variables. The kickoff is kept: a respawn starts a new session, and a
    // fresh fleet agent needs its opening instructions (the same rule a
    // recovery follows when it cannot resume a transcript).
    p.argv = new_pane.argv;
    p.norms = new_pane.norms;
    p.context_env = new_pane.context_env;
    p.worktree = worktree;
    p.mode = new_pane.mode;
    p.agent_id = new_pane.agent_id;
    p.token = new_pane.token;
    p.activity = false;
    p.painted = false;
    let new_id = p.agent_id;
    let role = p.role.clone();
    // Re-tile the window so the new pane (and every sibling) holds its
    // exact inner rect — the backstop `ctl spawn --here` already relies on.
    resize_window(&mut windows[wi], rows, cols);
    repoint_parents(
        windows
            .iter_mut()
            .flat_map(|w| w.panes.iter_mut())
            .map(|p| &mut p.parent),
        id,
        new_id,
    );
    retarget_sends(pending, id, new_id, Instant::now());
    ctl::reply_spawned(
        new_id,
        role.as_deref(),
        new_pane.session_id.as_deref(),
        None,
    )
}

/// The command a `ctl respawn` relaunches: the pane's own recorded command, as a
/// new session. The governed permission flags are stripped (the pane's `mode`
/// sets its posture at spawn), and for claude every session-selecting argument
/// goes too, so the spawn mints a fresh `--session-id` rather than resuming the
/// conversation the restart exists to leave. Other programs keep their arguments
/// verbatim: `-c` is a command string to a shell, not a session. A pane with no
/// recorded command falls back to its title. Pure.
pub(crate) fn respawn_argv(argv: &[String], title: &str) -> Vec<String> {
    let (argv, _) = atrium::ctl::sanitize_spawn_argv(argv);
    match argv.first() {
        None => vec![title.to_string()],
        Some(c) if atrium::bind::is_claude_stem(&atrium::bind::command_stem(c)) => {
            strip_session_args(&argv)
        }
        Some(_) => argv,
    }
}

#[cfg(test)]
mod tests;
