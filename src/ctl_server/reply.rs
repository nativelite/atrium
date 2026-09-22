//! Replies built from the live state: subscriptions, the audit log, the tree.

use super::panes::{ctl_parents, status_label};
use crate::*;

/// A subscriber's current topic set as a `Vec` (for the `sub`/`unsub` reply).
pub(crate) fn current_subs(bus: &atrium::bus::Bus, who: &str) -> Vec<String> {
    bus.subscriptions(who)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

/// Truncate a string to `n` chars (char-safe), appending `…` when cut. Keeps a
/// bad-request line short in the audit log.
pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// `(ok, note)` for the audit record: the error on failure, or a compact success
/// tag naming the salient id(s). Read from the typed [`atrium::ctl::Reply`] — it
/// used to re-parse the server's own JSON reply by string keys (r10 audit B12).
pub(crate) fn audit_outcome(reply: &atrium::ctl::Reply) -> (bool, String) {
    use atrium::ctl::Reply;
    let note = match reply {
        Reply::Err(msg) => return (false, msg.clone()),
        Reply::Spawned { pane, .. } | Reply::StatusOne { pane, .. } => format!("pane={pane}"),
        Reply::Killed(ids) => format!(
            "killed={}",
            ids.iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        Reply::Sent { target, .. } => format!("target={target}"),
        _ => String::new(),
    };
    (true, note)
}

/// Serve a `ctl audit` read: the in-memory log, subtree-scoped. The operator
/// sees everything; a worker sees only entries issued from within its own
/// subtree (operator/`None`-caller entries are hidden from it).
pub(crate) fn audit_reply(
    audit: &atrium::audit::Audit,
    windows: &[Window],
    caller: Option<AgentId>,
    privileged: bool,
    tail: Option<usize>,
) -> atrium::ctl::Reply {
    let entries = if privileged {
        audit.view(tail, |_| true)
    } else if let Some(root) = caller {
        let parents = ctl_parents(windows);
        audit.view(tail, |e| match e.caller {
            Some(c) => atrium::ctl::in_subtree(c, root, &parents),
            None => false, // operator actions are hidden from a worker
        })
    } else {
        // An UNAUTHENTICATED caller (no token, so `caller == None`) used to land
        // here, and this branch was byte-identical to the privileged one - so
        // holding no credential returned the entire fleet-wide audit trail,
        // including the operator actions the branch above deliberately hides
        // from a worker. That is the same "absent means most trusted" inversion
        // `privilege_for` was fixed to remove, left standing in one call site.
        Vec::new()
    };
    atrium::ctl::reply_audit(entries, audit.oldest_seq(), audit.latest_seq())
}

/// Serialize the spawn tree as a `list`/`status` reply. `root == Some(id)` limits
/// it to that pane's subtree (subtree-scoped status); `None` is the whole tree.
pub(crate) fn reply_tree(
    windows: &[Window],
    world: &atrium::vendors::AgentState,
    root: Option<AgentId>,
) -> atrium::ctl::Reply {
    let parents = ctl_parents(windows);
    let mut panes: Vec<&Pane> = windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .filter(|p| match root {
            None => true,
            Some(r) => atrium::ctl::in_subtree(p.agent_id, r, &parents),
        })
        .collect();
    panes.sort_by_key(|p| p.agent_id);
    // One monotonic reading for the whole snapshot so every node's idle is
    // measured against the same instant (no per-pane clock drift within a reply).
    let now = std::time::Instant::now();
    let nodes: Vec<atrium::ctl::TreeNode> = panes
        .iter()
        .map(|p| atrium::ctl::TreeNode {
            id: p.agent_id,
            parent: p.parent,
            role: p.role.as_deref(),
            title: &p.title,
            depth: p.depth,
            status: world.status_for(p.session_id.as_deref()).map(status_label),
            idle_ms: atrium::ipc::idle_ms(p.last_activity, now),
        })
        .collect();
    atrium::ctl::reply_list(&nodes)
}
