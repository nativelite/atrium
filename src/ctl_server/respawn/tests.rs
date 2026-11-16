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

/// A live placeholder that pane `maker` made and is the parent of.
fn slot(maker: usize) -> Slot {
    Slot {
        placeholder: true,
        parent: Some(AgentId(maker)),
        owner: Some(AgentId(maker)),
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

/// A lead that was respawned between the split and the respawn-pane is a new
/// process under a new id. Its children follow it (the parent link is moved),
/// but a placeholder its old process made is nobody's to claim: not the new
/// id's, and not the old one's, should anything ever present it.
#[test]
fn a_placeholder_does_not_follow_its_maker_through_a_respawn() {
    let (old, new) = (AgentId(3), AgentId(7));
    let moved = Slot {
        parent: Some(new),
        ..slot(3)
    };
    assert!(
        !is_own_placeholder(new, Some(moved)),
        "the new process never made it"
    );
    assert!(
        !is_own_placeholder(old, Some(moved)),
        "the old id is not its parent"
    );
    // A maker that is gone altogether (its pane exited): the link is stale.
    assert!(!is_own_placeholder(new, Some(slot(3))));
    // A placeholder with no recorded maker is nobody's.
    assert!(!is_own_placeholder(
        old,
        Some(Slot {
            owner: None,
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

const ASKS: [Asked; 8] = [
    Asked::Nothing,
    Asked::Default,
    Asked::DontAsk,
    Asked::Mode(TrustMode::Off),
    Asked::Mode(TrustMode::Plan),
    Asked::Mode(TrustMode::Edits),
    Asked::Mode(TrustMode::Auto),
    Asked::Mode(TrustMode::Skip),
];

fn lower(a: TrustMode, b: TrustMode) -> TrustMode {
    if a.rank() <= b.rank() {
        a
    } else {
        b
    }
}

/// The full matrix, launch trust x lead mode x what the line asked: 200
/// cases, each held to the rules that make a teammate never less held than
/// its lead, its session or its own request.
#[test]
fn the_posture_matrix_holds_every_teammate_to_its_lead_its_session_and_its_request() {
    use TrustMode::{Off, Plan};
    for policy in MODES {
        for lead in MODES {
            let governed = lower(lead, policy);
            for asked in ASKS {
                let got = teammate_mode(asked, lead, policy);
                let case = format!("launch {policy:?}, lead {lead:?}, asked {asked:?}: {got:?}");
                // Never above the lead pane, never above the session.
                assert!(got.mode.rank() <= lead.rank(), "{case}");
                assert!(got.mode.rank() <= policy.rank(), "{case}");
                // A flag atrium writes itself only where the spawn writes none.
                if got.flag.is_some() {
                    assert_eq!(got.mode, Off, "{case}");
                }
                match asked {
                    Asked::Nothing => {
                        assert_eq!((got.mode, got.flag), (governed, None), "{case}");
                    }
                    // Passed on as written, unless the lead is held to plan.
                    Asked::Default | Asked::DontAsk => {
                        let word = if asked == Asked::Default {
                            "default"
                        } else {
                            "dontAsk"
                        };
                        if governed == Plan {
                            assert_eq!((got.mode, got.flag), (Plan, None), "{case}");
                        } else {
                            assert_eq!((got.mode, got.flag), (Off, Some(word)), "{case}");
                        }
                    }
                    // Plan asked is plan got, whatever governs.
                    Asked::Mode(Plan) => {
                        let plan = (got.mode, got.flag) == (Plan, None)
                            || (got.mode, got.flag) == (Off, Some("plan"));
                        assert!(plan, "{case}");
                        assert_eq!(got.flag.is_some(), governed == Off, "{case}");
                    }
                    Asked::Mode(m) => {
                        assert!(got.mode.rank() <= m.rank(), "{case}");
                        assert_eq!(got.mode, lower(m, governed), "{case}");
                        assert_eq!(got.flag, None, "{case}");
                        // Whatever was held back is said.
                        assert_eq!(got.note.is_some(), got.mode != m, "{case}");
                    }
                }
            }
        }
    }
}

/// The cases a person would name, one row each: launch trust, the lead
/// pane's mode, what the line asked, and the mode and the flag that result.
#[test]
fn a_teammates_mode_in_the_cases_that_matter() {
    use Asked::{Default, DontAsk, Mode, Nothing};
    use TrustMode::*;
    let table: &[(TrustMode, TrustMode, Asked, TrustMode, Option<&str>)] = &[
        // A session at accept, a lead at accept.
        (Edits, Edits, Nothing, Edits, None),
        (Edits, Edits, Mode(Edits), Edits, None),
        (Edits, Edits, Mode(Auto), Edits, None),
        (Edits, Edits, Mode(Skip), Edits, None),
        (Edits, Edits, Mode(Plan), Plan, None),
        // The human switched the lead down to default: its teammates ask too.
        (Edits, Edits, Default, Off, Some("default")),
        (Edits, Edits, DontAsk, Off, Some("dontAsk")),
        (Auto, Auto, Default, Off, Some("default")),
        (Skip, Skip, Default, Off, Some("default")),
        // Automode and skip sessions.
        (Auto, Auto, Mode(Auto), Auto, None),
        (Auto, Auto, Mode(Skip), Auto, None),
        (Auto, Auto, Mode(Edits), Edits, None),
        (Skip, Skip, Mode(Skip), Skip, None),
        (Skip, Skip, Mode(Plan), Plan, None),
        (Skip, Auto, Mode(Edits), Edits, None),
        // A lead the fleet holds to plan, in a session launched higher.
        (Edits, Plan, Nothing, Plan, None),
        (Edits, Plan, Mode(Edits), Plan, None),
        (Skip, Plan, Mode(Skip), Plan, None),
        (Edits, Plan, Default, Plan, None),
        (Edits, Plan, DontAsk, Plan, None),
        // A session launched at plan.
        (Plan, Plan, Default, Plan, None),
        (Plan, Plan, Mode(Skip), Plan, None),
        (Plan, Plan, Mode(Plan), Plan, None),
        // No `--trust`: atrium governs nothing, and still a plan request is
        // plan and a default request is default.
        (Off, Off, Nothing, Off, None),
        (Off, Off, Mode(Skip), Off, None),
        (Off, Off, Mode(Edits), Off, None),
        (Off, Off, Mode(Plan), Off, Some("plan")),
        (Off, Off, Default, Off, Some("default")),
        (Off, Off, DontAsk, Off, Some("dontAsk")),
        // A lead with no posture of its own in a governed session.
        (Edits, Off, Mode(Plan), Off, Some("plan")),
        (Edits, Off, Mode(Skip), Off, None),
        (Plan, Off, Mode(Plan), Off, Some("plan")),
        // A lead recorded above the session (it cannot be) is still capped.
        (Edits, Skip, Mode(Skip), Edits, None),
        (Edits, Skip, Nothing, Edits, None),
    ];
    for (policy, lead, asked, mode, flag) in table.iter().copied() {
        let got = teammate_mode(asked, lead, policy);
        assert_eq!(
            (got.mode, got.flag),
            (mode, flag),
            "launch {policy:?}, lead {lead:?}, asked {asked:?}"
        );
    }
    // What was held back is said, and how the audit row names the result.
    let got = teammate_mode(Mode(Skip), Edits, Edits);
    assert!(got.note.unwrap().contains("held to its lead pane's accept"));
    let got = teammate_mode(Mode(Skip), Skip, Edits);
    assert!(got.note.unwrap().contains("capped to accept"));
    assert_eq!(teammate_mode(Mode(Plan), Off, Off).label(), "plan");
    assert_eq!(teammate_mode(Default, Auto, Auto).label(), "default");
    assert_eq!(teammate_mode(DontAsk, Auto, Auto).label(), "dontAsk");
    assert_eq!(teammate_mode(Nothing, Auto, Auto).label(), "automode");
    assert_eq!(teammate_mode(Mode(Plan), Off, Off).note, None);
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
