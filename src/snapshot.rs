//! Session snapshots: capturing the live panes, writing a snapshot when the
//! window state changes, and finding the latest one an older atrium left.

use crate::*;

/// The one place a live [`Pane`] becomes a [`atrium::session::PaneCapture`].
/// The literal names every field, so a field added to `PaneCapture` and not
/// mapped here is a compile error rather than a silently thinner snapshot.
/// `parent_pane` is the parent's pane id, resolved by the caller (a pane stores
/// its parent as a per-process `AgentId`, which a snapshot must not carry).
///
/// It takes the pane itself, not its fields: the old 15-argument form let two
/// adjacent `Option<String>`s (`cwd`, `identity`) swap without a complaint.
fn capture_pane(p: &Pane, parent_pane: Option<usize>) -> atrium::session::PaneCapture {
    atrium::session::PaneCapture {
        id: p.id,
        role: p.role.clone(),
        argv: p.argv.clone(),
        cwd: p.cwd.clone(),
        identity: p.identity.clone(),
        session_id: p.session_id.clone(),
        // Set by spawn_worker_* via sp.worktree.
        worktree: p.worktree.clone(),
        deny: p.deny.clone(),
        can_spawn: p.can_spawn,
        depth: p.depth,
        parent_pane,
        mode: Some(p.mode),
        kickoff: p.kickoff,
        norms: p.norms.clone(),
        context_env: p.context_env.clone(),
    }
}

/// Write a session snapshot when the window state has changed since the last
/// write, or when the heartbeat is due. Returns whether a write landed, so the
/// caller can move the warden's baseline to it.
///
/// The heartbeat is what lets a later launch tell a running session from a dead
/// one: a live atrium rewrites its file every `HEARTBEAT_MS` even when nothing
/// changed, so a stale `saved_at_ms` means the writer is gone even if its pid has
/// since been reused (see `session_store::liveness`). `last` holds the snapshot
/// *without* the timestamp, so the change check still compares content.
///
/// `last` advances only when the write lands, so a failed save is retried on the
/// next check instead of being forgotten; the error is returned for the caller to
/// surface (r10 audit B8 — it used to be dropped, and `atrium recover` silently
/// had nothing to restore).
pub(crate) fn snapshot_if_changed(
    windows: &[Window],
    path: &std::path::Path,
    last: &mut Option<atrium::session::Snapshot>,
    last_write_ms: &mut u64,
    meta: &atrium::session::SessionMeta,
) -> std::io::Result<bool> {
    let w = match windows.first() {
        Some(w) => w,
        None => return Ok(false),
    };
    // A pane's `parent` is an `AgentId`, minted per process and re-minted on
    // recovery — storing one would name a *different* pane in the recovered
    // session. Resolve it to the parent's pane id, which is stable inside the
    // snapshot. A parent in another window, or one that has already exited,
    // resolves to `None`; the pane's restored `depth` still holds the recursion
    // guard, which is what `--max-depth` actually checks.
    let parent_pane = |parent: Option<atrium::ctl::AgentId>| -> Option<usize> {
        let parent = parent?;
        w.panes.iter().find(|p| p.agent_id == parent).map(|p| p.id)
    };
    let snap = atrium::session::capture(
        w.tree.ids(),
        w.tree.focus(),
        w.panes
            .iter()
            .map(|p| capture_pane(p, parent_pane(p.parent)))
            .collect(),
        atrium::session::policy(),
    );
    let mut snap = snap;
    snap.meta = meta.clone();
    let now = atrium::session_store::now_ms();
    let heartbeat_due = now.saturating_sub(*last_write_ms) >= atrium::session_store::HEARTBEAT_MS;
    if last.as_ref() == Some(&snap) && !heartbeat_due {
        return Ok(false);
    }
    // Re-created on every write rather than once at startup: a deleted directory
    // (a cleanup, or tampering) must not turn into a silent stream of failures.
    if let Some(dir) = path.parent() {
        atrium::session_store::ensure_private_dir(dir)?;
    }
    let mut stamped = snap.clone();
    stamped.meta.saved_at_ms = Some(now);
    atrium::session::save(path, &stamped)?;
    *last = Some(snap);
    *last_write_ms = now;
    Ok(true)
}

/// Scan `dir` — the temp directory older atriums saved snapshots to — for
/// `atrium-session-<pid>.json` files and return the most recently modified one
/// whose atrium is no longer `running`, or `None`.
///
/// Only used to *name* a legacy file for `atrium recover --snapshot`, never to pick
/// one silently. Skipping live writers matters even for that: an older atrium that
/// is still running keeps its snapshot the newest file there, and pointing the
/// operator at their own running session as "the one to recover" is exactly
/// backwards.
pub(crate) fn find_latest_snapshot(
    dir: &std::path::Path,
    running: impl Fn(u32) -> bool,
) -> Option<std::path::PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("atrium-session-") || !name.ends_with(".json") {
            continue;
        }
        let writer = name
            .trim_start_matches("atrium-session-")
            .trim_end_matches(".json")
            .parse::<u32>()
            .ok();
        if writer.is_some_and(&running) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if best.as_ref().map_or(true, |(t, _)| modified > *t) {
                    best = Some((modified, entry.path()));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests;
