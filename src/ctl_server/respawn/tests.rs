//! Restarting a pane: the argv a respawn replays.

use super::*;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// A respawn restarts the agent that was running — its model, its kickoff —
/// as a new session with no permission flags of its own.
#[test]
fn a_respawned_claude_keeps_its_arguments_but_not_its_session() {
    let argv = args(&[
        "claude",
        "--model",
        "opus",
        "--resume",
        "abc",
        "--dangerously-skip-permissions",
        "--session-id=def",
        "-c",
        "You lead. Read PLAN.md.",
    ]);
    assert_eq!(
        respawn_argv(&argv, "claude"),
        args(&["claude", "--model", "opus", "You lead. Read PLAN.md."])
    );
}

#[test]
fn a_respawned_shell_keeps_a_dash_c_command() {
    let argv = args(&["sh", "-c", "make watch"]);
    assert_eq!(respawn_argv(&argv, "sh"), argv);
}

#[test]
fn a_pane_with_no_recorded_command_respawns_its_title() {
    assert_eq!(respawn_argv(&[], "claude"), args(&["claude"]));
}

// ---- a teammate into its placeholder ------------------------------------------

use atrium::ctl::TrustMode;
use atrium::tmux_shim::Asked;

fn slot(parent: usize) -> Slot {
    Slot {
        placeholder: true,
        parent: Some(AgentId(parent)),
        exited: false,
    }
}

/// The whole rule for which pane a `respawn-pane` may replace: a live pane
/// with the placeholder mark whose parent is the caller.
#[test]
fn only_the_callers_own_live_placeholder_may_become_a_teammate() {
    let lead = AgentId(3);
    assert!(is_own_placeholder(lead, Some(slot(3))));
    // No such pane.
    assert!(!is_own_placeholder(lead, None));
    // A pane that runs a command: an agent, a shell, a teammate already
    // started (the mark is cleared when one starts). The lead's own child
    // or not, it is never re-pointed.
    assert!(!is_own_placeholder(
        lead,
        Some(Slot {
            placeholder: false,
            ..slot(3)
        })
    ));
    // Another pane's placeholder: a sibling's, a child's own, a root's.
    assert!(!is_own_placeholder(lead, Some(slot(4))));
    assert!(!is_own_placeholder(AgentId(4), Some(slot(3))));
    assert!(!is_own_placeholder(
        lead,
        Some(Slot {
            parent: None,
            ..slot(3)
        })
    ));
    // One that was killed and waits to be reaped.
    assert!(!is_own_placeholder(
        lead,
        Some(Slot {
            exited: true,
            ..slot(3)
        })
    ));
}

const MODES: [TrustMode; 5] = [
    TrustMode::Off,
    TrustMode::Plan,
    TrustMode::Edits,
    TrustMode::Auto,
    TrustMode::Skip,
];

/// For every asked mode, lead mode and launch trust: the teammate is never
/// above its lead pane's mode, never above the session's, and never above
/// what it asked for.
#[test]
fn a_teammates_mode_is_never_above_its_lead_the_session_or_its_request() {
    for policy in MODES {
        for lead in MODES {
            let (inherit, _) = teammate_mode(Asked::LeadsOwn, lead, policy);
            assert!(inherit.rank() <= lead.rank() && inherit.rank() <= policy.rank());
            for asked in MODES {
                let (mode, note) = teammate_mode(Asked::Mode(asked), lead, policy);
                assert!(mode.rank() <= lead.rank(), "{asked:?} {lead:?} {policy:?}");
                assert!(
                    mode.rank() <= policy.rank(),
                    "{asked:?} {lead:?} {policy:?}"
                );
                assert!(mode.rank() <= asked.rank(), "{asked:?} {lead:?} {policy:?}");
                // Whatever was held back is said.
                let lowered = mode != asked;
                assert_eq!(note.is_some(), lowered, "{asked:?} {lead:?} {policy:?}");
            }
        }
    }
}

#[test]
fn a_teammates_mode_in_the_cases_that_matter() {
    use TrustMode::*;
    // A skip request under an accept session: accept, and the reply says so.
    let (mode, note) = teammate_mode(Asked::Mode(Skip), Edits, Edits);
    assert_eq!(mode, Edits);
    assert!(note.unwrap().contains("held to its lead pane's accept"));
    // A lead the fleet holds to plan, in a session launched at accept: plan.
    assert_eq!(teammate_mode(Asked::Mode(Edits), Plan, Edits).0, Plan);
    assert_eq!(teammate_mode(Asked::Mode(Skip), Plan, Skip).0, Plan);
    // `--plan-mode-required`, or a plan request: plan, below any lead.
    assert_eq!(teammate_mode(Asked::Mode(Plan), Skip, Skip), (Plan, None));
    assert_eq!(teammate_mode(Asked::Mode(Plan), Edits, Edits), (Plan, None));
    // A request for less than the lead has is honoured.
    assert_eq!(teammate_mode(Asked::Mode(Edits), Auto, Skip), (Edits, None));
    // No request, or `default`: the lead pane's own mode, capped like it.
    assert_eq!(teammate_mode(Asked::LeadsOwn, Edits, Edits), (Edits, None));
    assert_eq!(teammate_mode(Asked::LeadsOwn, Plan, Edits), (Plan, None));
    assert_eq!(teammate_mode(Asked::LeadsOwn, Skip, Skip), (Skip, None));
    // A session with no `--trust` adds no permission flag for anyone.
    for asked in [Asked::LeadsOwn, Asked::Mode(Skip), Asked::Mode(Plan)] {
        assert_eq!(teammate_mode(asked, Off, Off).0, Off);
    }
    // A lead recorded above the session (it cannot be, by `effective_mode`
    // at its own spawn) would still be capped here.
    let (mode, note) = teammate_mode(Asked::Mode(Skip), Skip, Edits);
    assert_eq!(mode, Edits);
    assert!(note.unwrap().contains("capped to accept"));
}

/// A teammate runs under its lead pane's own identity, and only that: the
/// request names none, and the delegation rule a spawn is held to would
/// refuse any other.
#[test]
fn a_teammates_identity_is_its_leads_own_and_never_one_the_lead_does_not_hold() {
    // The lead's own, whatever the session's default is; none when it has none.
    assert_eq!(
        teammate_identity(Some("work"), None),
        Ok(Some("work".to_string()))
    );
    assert_eq!(
        teammate_identity(Some("work"), Some("prod")),
        Ok(Some("work".to_string()))
    );
    assert_eq!(teammate_identity(None, Some("prod")), Ok(None));
    assert_eq!(teammate_identity(None, None), Ok(None));
    // What the rule would say to a lead reaching for an identity that is
    // neither its own nor the session's: no. `teammate_identity` has no
    // input through which a request could ask for one.
    assert!(ctl::delegation_allowed(Some("prod"), Some("work"), None, false).is_err());
    assert!(ctl::delegation_allowed(Some("prod"), None, Some("default"), false).is_err());
    // Its own identity is not a delegation to a new one.
    assert!(ctl::delegation_allowed(Some("work"), Some("work"), None, false).is_ok());
}
