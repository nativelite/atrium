//! Pane lookups.

use super::*;
use crate::ctl_server::testutil::crew;

/// A role names one live pane: a second spawn wearing it is refused, so
/// no pane can subscribe, post or be addressed as another.
#[test]
fn a_role_held_by_a_live_pane_is_not_free() {
    let (c, _) = crew();
    assert_eq!(role_holder("lead", &c), Some(AgentId(0)));
    assert_eq!(role_holder("reviewer", &c), Some(AgentId(2)));
    assert_eq!(role_holder("fixer", &c), None);
    assert_eq!(
        role_holder("pane 4", &c),
        None,
        "an unrolled pane's label is not a role"
    );
}

/// **Dropping your token must not promote you** (review #1).
///
/// `caller` is derived from the capability token and is never self-reported,
/// so `None` means exactly "unauthenticated". This arm returned `true`, which
/// inverted the gate: holding NO credential granted strictly more than
/// holding a worker's, so `env -u ATRIUM_TOKEN atrium ctl ...` promoted you.
///
/// This fails against the old code, which is the point — an earlier attempt
/// at an integration test for the same fix passed with the fix REVERTED,
/// because the elevated spawn was refused earlier by policy and never reached
/// this decision at all.
#[test]
fn an_unauthenticated_caller_is_never_the_operator() {
    assert!(!privilege_for(None, None));
    // Even if a pane were somehow associated, no token means no authority.
    assert!(!privilege_for(None, Some((None, 0))));
}

/// A token that matches no live pane is stale or forged — not the operator.
#[test]
fn a_token_resolving_to_no_live_pane_is_not_the_operator() {
    assert!(!privilege_for(Some(AgentId(7)), None));
}

/// **The session policy is a ceiling for everyone** (review #1, remainder).
///
/// A "privileged" caller used to bypass the cap entirely. But `-n N` gives
/// every pane no parent, so every pane in a mass-spawned session counted as
/// the operator — and could ask for `skip`, a full permission bypass, in a
/// session the human had set to `plan`.
#[test]
fn the_session_policy_caps_every_request() {
    use atrium::ctl::TrustMode;
    // Escalation is refused and explained, whoever asks.
    let (mode, note) = super::effective_mode(Some(TrustMode::Skip), TrustMode::Plan);
    assert_eq!(mode, TrustMode::Plan, "skip escaped a plan-mode session");
    assert!(note
        .expect("a capped request must say so")
        .contains("capped"));
}

/// De-escalation stays free: asking for LESS than the policy is always fine,
/// and is the useful half of `--mode`.
#[test]
fn a_request_below_the_policy_is_honoured() {
    use atrium::ctl::TrustMode;
    let (mode, note) = super::effective_mode(Some(TrustMode::Plan), TrustMode::Skip);
    assert_eq!(mode, TrustMode::Plan);
    assert!(note.is_none(), "de-escalation should not be flagged");
    // No request at all inherits the policy.
    let (mode, note) = super::effective_mode(None, TrustMode::Edits);
    assert_eq!(mode, TrustMode::Edits);
    assert!(note.is_none());
}

/// The intended rule, unchanged: a root pane is the operator, a spawned
/// worker is not. (Whether `-n N` should make every pane a root pane is a
/// separate, still-open question — see review finding #1's remainder.)
#[test]
fn a_root_pane_is_the_operator_and_a_worker_is_not() {
    assert!(privilege_for(Some(AgentId(1)), Some((None, 0))));
    assert!(!privilege_for(
        Some(AgentId(2)),
        Some((Some(AgentId(1)), 1))
    ));
}

/// A worker whose parent link is GONE is still a worker.
///
/// `atrium recover` resolves each worker's parent through the pane that
/// spawned it, so a parent that had already exited resolves to `None`. With
/// the gate keyed on the parent alone, that promoted the worker to operator
/// across a recovery — session-wide `send`/`kill`/`respawn` and the right to
/// delegate any identity in the vault. Depth is recorded independently and
/// survives, so requiring both is what closes it.
#[test]
fn a_worker_that_lost_its_parent_link_is_not_promoted_to_operator() {
    assert!(
        !privilege_for(Some(AgentId(3)), Some((None, 1))),
        "depth > 0 is a worker no matter what happened to its parent link"
    );
    assert!(!privilege_for(Some(AgentId(4)), Some((None, 7))));
}
