//! atrium's launch meta-flags, trust modes and the agent directive.

use super::*;
use crate::ctl::testutil::v;
use crate::ctl::DEFAULT_MAX_DEPTH;

#[test]
fn flags_default_off_and_pass_command_through() {
    let (allow, depth, trust, rest) = parse_flags(&v(&["claude", "--continue"])).unwrap();
    assert!(!allow);
    assert_eq!(trust, TrustMode::Off);
    assert_eq!(depth, DEFAULT_MAX_DEPTH);
    assert_eq!(rest, v(&["claude", "--continue"]));
}

#[test]
fn flags_allow_ctl_and_max_depth() {
    let (allow, depth, _yolo, rest) =
        parse_flags(&v(&["--allow-ctl", "--max-depth", "3", "claude"])).unwrap();
    assert!(allow);
    assert_eq!(depth, 3);
    assert_eq!(rest, v(&["claude"]));
}

#[test]
fn flags_trust_is_parsed() {
    let (allow, _depth, trust, rest) =
        parse_flags(&v(&["--allow-ctl", "--trust", "claude"])).unwrap();
    assert!(allow);
    assert_eq!(trust, TrustMode::Edits);
    assert_eq!(rest, v(&["claude"]));
}

#[test]
fn flags_skip_permissions_is_parsed() {
    let (_allow, _depth, trust, rest) = parse_flags(&v(&["--skip-permissions", "claude"])).unwrap();
    assert_eq!(trust, TrustMode::Skip);
    assert_eq!(rest, v(&["claude"]));
}

#[test]
fn flags_trust_takes_a_policy_keyword() {
    // `--trust automode|plan|accept` sets the session policy; the keyword is
    // consumed and the command (`claude`) is left in `rest`.
    for (policy, want) in [
        ("automode", TrustMode::Auto),
        ("skip", TrustMode::Skip),
        ("plan", TrustMode::Plan),
        ("accept", TrustMode::Edits),
    ] {
        let (_a, _d, trust, rest) =
            parse_flags(&v(&["--allow-ctl", "--trust", policy, "claude"])).unwrap();
        assert_eq!(trust, want, "--trust {policy}");
        assert_eq!(rest, v(&["claude"]), "--trust {policy} leaves the command");
    }
    // `--trust=plan` spelling.
    let (_a, _d, trust, rest) = parse_flags(&v(&["--trust=plan", "claude"])).unwrap();
    assert_eq!(trust, TrustMode::Plan);
    assert_eq!(rest, v(&["claude"]));
}

#[test]
fn flags_bare_trust_is_accept_and_keeps_the_command() {
    // A bare `--trust` followed by a non-policy token = accept, and the token
    // is the hosted command (not eaten as a policy).
    let (_a, _d, trust, rest) = parse_flags(&v(&["--trust", "claude"])).unwrap();
    assert_eq!(trust, TrustMode::Edits);
    assert_eq!(rest, v(&["claude"]));
}

#[test]
fn flags_trust_and_skip_permissions_conflict() {
    assert!(
        parse_flags(&v(&["--trust", "--skip-permissions", "claude"]))
            .unwrap_err()
            .contains("once")
    );
    assert!(
        parse_flags(&v(&["--skip-permissions", "--trust", "claude"]))
            .unwrap_err()
            .contains("once")
    );
    // Two policy spellings at once is also a conflict.
    assert!(
        parse_flags(&v(&["--trust", "plan", "--trust", "automode", "claude"]))
            .unwrap_err()
            .contains("once")
    );
}

#[test]
fn trust_mode_rank_orders_by_autonomous_power() {
    // Off < plan < accept < automode < skip.
    assert!(TrustMode::Off.rank() < TrustMode::Plan.rank());
    assert!(TrustMode::Plan.rank() < TrustMode::Edits.rank());
    assert!(TrustMode::Edits.rank() < TrustMode::Auto.rank());
    assert!(TrustMode::Auto.rank() < TrustMode::Skip.rank());
    // Keyword ↔ label round-trip for every mode.
    for m in [
        TrustMode::Off,
        TrustMode::Plan,
        TrustMode::Edits,
        TrustMode::Auto,
        TrustMode::Skip,
    ] {
        assert_eq!(TrustMode::from_policy_keyword(m.policy_label()), Some(m));
    }
    // `automode` and `skip` are DISTINCT (the 0.18.0 conflation bug).
    assert_ne!(
        TrustMode::from_policy_keyword("automode"),
        TrustMode::from_policy_keyword("skip")
    );
}

#[test]
fn agent_directive_has_no_shim_breaking_chars() {
    // Defense in depth (see AGENT_CTL_DIRECTIVE): the directive rides the
    // Windows batch shim. `pty` encodes it safely today; keeping the text free
    // of cmd metacharacters means an argv-only quoter can't break it either.
    for c in ['"', '`', '&', '|', '<', '>', '^', '%'] {
        assert!(
            !AGENT_CTL_DIRECTIVE.contains(c),
            "directive contains shim-breaking {c:?}"
        );
    }
}

#[test]
fn agent_directive_teaches_claim_before_work() {
    // The behavioral half of the claim protocol: agents are told to claim
    // before building and release when done, else the primitive goes unused.
    assert!(AGENT_CTL_DIRECTIVE.contains("board claim"));
    assert!(AGENT_CTL_DIRECTIVE.contains("board release"));
}

#[test]
fn flags_max_depth_zero_is_unlimited() {
    let (_, depth, _, _) = parse_flags(&v(&["--allow-ctl", "--max-depth", "0", "claude"])).unwrap();
    assert_eq!(depth, usize::MAX);
}

#[test]
fn flags_stop_at_command_so_child_keeps_its_flags() {
    // A `--max-depth` after the command belongs to the child, untouched.
    let (allow, _, _, rest) =
        parse_flags(&v(&["--allow-ctl", "claude", "--max-depth", "9"])).unwrap();
    assert!(allow);
    assert_eq!(rest, v(&["claude", "--max-depth", "9"]));
}

#[test]
fn flags_bad_max_depth_errors() {
    let err = parse_flags(&v(&["--max-depth", "lots"])).unwrap_err();
    assert!(err.contains("--max-depth"), "{err}");
}
