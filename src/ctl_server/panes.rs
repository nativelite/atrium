//! Looking panes up by agent id and deciding what a caller may do to them.

use crate::*;

/// Find a hosted pane by its global agent id (immutable / mutable).
pub(crate) fn pane_by_agent(windows: &[Window], id: AgentId) -> Option<&Pane> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .find(|p| p.agent_id == id)
}

pub(crate) fn pane_by_agent_mut(windows: &mut [Window], id: AgentId) -> Option<&mut Pane> {
    windows
        .iter_mut()
        .flat_map(|w| w.panes.iter_mut())
        .find(|p| p.agent_id == id)
}

/// `(agent_id, role)` for every live pane — the candidate set for target
/// resolution.
pub(crate) fn ctl_candidates(windows: &[Window]) -> Vec<(AgentId, Option<String>)> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| (p.agent_id, p.role.clone()))
        .collect()
}

/// `(agent_id, parent)` for every live pane — the spawn-tree edges the subtree
/// guard walks.
pub(crate) fn ctl_parents(windows: &[Window]) -> Vec<(AgentId, Option<AgentId>)> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| (p.agent_id, p.parent))
        .collect()
}

/// The **human** controls everything (Decision 3). A caller is human-privileged
/// when it has no attributed pane, or when its pane is a *root* (one atrium opened,
/// `parent == None`, depth 0) — i.e. where the operator sits. A spawned worker
/// (depth > 0) is scoped to its own subtree.
pub(crate) fn caller_privileged(windows: &[Window], caller: Option<AgentId>) -> bool {
    // Look the caller up, then decide in a pure function so the DECISION can be
    // tested without constructing panes (each of which needs a live pty). This
    // gate had no tests at all: when its two `None` arms were flipped from
    // `true` to `false` — a security fix — nothing in the suite changed, which is
    // precisely the problem.
    let pane_root = caller
        .and_then(|id| pane_by_agent(windows, id))
        .map(|p| (p.parent, p.depth));
    privilege_for(caller, pane_root)
}

/// Resolve a spawn's permission mode against the session policy.
///
/// The session policy is a CEILING, and it now holds for everyone — including a
/// root pane. It used to be bypassed outright for a "privileged" caller
/// (`Some(req) if privileged => req`), on the reading that the root pane is the
/// human and the human may elevate. Two things break that:
///
/// 1. `-n N` and `--grid` give every pane no parent, so EVERY pane in a
///    mass-spawned session was classified as the operator. An agent could then
///    ask for `skip` — a full permission bypass — in a session the human had
///    deliberately set to `plan`.
/// 2. atrium's own documentation calls `--trust` "the mode spawned agents run in,
///    and the ceiling they are capped at". A ceiling that a pane can exceed is
///    not a ceiling, and the human cannot tell from the outside which panes could.
///
/// So the bypass is gone. `--mode` may still DE-ESCALATE freely (asking for less
/// than the policy is always allowed), which is the useful half; it can no longer
/// escalate. To run agents at a higher posture, the human sets it at launch,
/// where it is a visible, deliberate choice rather than something a pane can
/// request. Returns the effective mode and a note when a request was capped.
pub(crate) fn effective_mode(
    requested: Option<atrium::ctl::TrustMode>,
    policy: atrium::ctl::TrustMode,
) -> (atrium::ctl::TrustMode, Option<String>) {
    match requested {
        None => (policy, None),
        Some(req) if req.rank() <= policy.rank() => (req, None),
        Some(req) => (
            policy,
            Some(format!(
                "capped to {} (session policy); nothing may elevate itself to {} \
                 — set the policy at launch with --trust",
                policy.policy_label(),
                req.policy_label()
            )),
        ),
    }
}

/// The privilege decision, given only what it needs.
///
/// `pane_parent` says what the caller's capability token resolved to:
/// - `None` — no such live pane (a stale or forged token), or no caller at all
/// - `Some((None, 0))` — a live root pane the human opened
/// - `Some((Some(_), _))` — a live pane spawned by another: a worker
/// - `Some((None, d))` with `d > 0` — a worker that has *lost* its parent link
///
/// That last case is why the depth is part of the decision and not just the
/// parent. A worker's parent link can go missing: `atrium recover` resolves each
/// worker's parent through the pane that spawned it, and a parent which had
/// already exited (so it is no longer in the window) resolves to `None`. Keying
/// the gate on the parent alone then promoted that worker to **operator** on the
/// next recovery — handing it session-wide `send`/`kill`/`respawn` and the right
/// to delegate any identity in the vault. Depth is recorded independently, so
/// requiring both closes it. The doc above has described a root as "parent ==
/// None, depth 0" since this gate was written; this is the code catching up.
pub(crate) fn privilege_for(
    caller: Option<AgentId>,
    pane_root: Option<(Option<AgentId>, usize)>,
) -> bool {
    match caller {
        // No authenticated caller is NOT the operator. This arm used to return
        // `true`, which inverted the gate: holding no credential granted strictly
        // more than holding a worker's, so `env -u ATRIUM_TOKEN atrium ctl ...`
        // promoted you. `caller` is derived from the capability token, never
        // self-reported, so `None` means exactly "unauthenticated" and must be
        // the least trusted state, not the most.
        None => false,
        Some(_) => match pane_root {
            Some((parent, depth)) => parent.is_none() && depth == 0,
            // A token resolving to no live pane is stale or forged, not the
            // operator. Same inversion as above.
            None => false,
        },
    }
}

/// The agsess status label the ctl protocol reports (stable strings the calling
/// agent can match on).
pub(crate) fn status_label(s: agsess::Status) -> &'static str {
    match s {
        agsess::Status::Working => "working",
        agsess::Status::WaitingApproval => "waiting-approval",
        agsess::Status::WaitingPrompt => "waiting-prompt",
        agsess::Status::Idle => "idle",
    }
}

/// The label a pane publishes and subscribes under: its role, else `pane N`.
/// One derivation for `bus pub`, `bus sub` and wake matching, so they agree.
pub(crate) fn pane_label(id: AgentId, role: Option<&str>) -> String {
    role.filter(|r| !r.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("pane {}", id.0 + 1))
}

/// The live pane already wearing `role`, if any. Pure over the candidate set.
pub(crate) fn role_holder(role: &str, candidates: &[(AgentId, Option<String>)]) -> Option<AgentId> {
    candidates
        .iter()
        .find(|(_, r)| r.as_deref() == Some(role))
        .map(|(id, _)| *id)
}

#[cfg(test)]
mod tests;
