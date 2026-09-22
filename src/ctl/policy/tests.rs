//! The spawn, argv, target and delegation guards.

use super::*;
use crate::ctl::testutil::v;
use crate::ctl::AgentId;

#[test]
fn sanitize_strips_dangerous_bypass_flag() {
    let (clean, stripped) = sanitize_spawn_argv(&v(&["claude", "--dangerously-skip-permissions"]));
    assert_eq!(clean, v(&["claude"]));
    assert_eq!(stripped, v(&["--dangerously-skip-permissions"]));
}

#[test]
fn sanitize_strips_permission_mode_and_its_value() {
    // `--flag value` form: both the flag and its value token go.
    let (clean, stripped) = sanitize_spawn_argv(&v(&[
        "claude",
        "--permission-mode",
        "bypassPermissions",
        "-r",
    ]));
    assert_eq!(clean, v(&["claude", "-r"]));
    assert_eq!(stripped, v(&["--permission-mode"]));
}

#[test]
fn sanitize_strips_allowedtools_including_eq_form() {
    // `--flag=value` form: the value rides in the same arg, no extra token.
    let (clean, stripped) = sanitize_spawn_argv(&v(&["claude", "--allowedTools=Bash(*)"]));
    assert_eq!(clean, v(&["claude"]));
    assert_eq!(stripped, v(&["--allowedTools"]));
}

#[test]
fn sanitize_leaves_benign_argv_untouched() {
    let (clean, stripped) = sanitize_spawn_argv(&v(&["claude", "--continue", "--model", "opus"]));
    assert_eq!(clean, v(&["claude", "--continue", "--model", "opus"]));
    assert!(stripped.is_empty());
}

#[test]
fn vet_refuses_the_flags_the_denylist_missed() {
    // Each of these reaches code execution outside the tool-permission
    // system, and none was in GOVERNED_FLAGS: an MCP server is a command
    // claude launches at startup, a plugin carries hooks, a settings file
    // can carry both.
    for bad in [
        "--mcp-config",
        "--plugin-dir",
        "--plugin-url",
        "--settings",
        "--allowed-tools", // claude's own alias of the governed --allowedTools
    ] {
        match vet_spawn_argv(&v(&["claude", bad, "x"])) {
            ArgvVerdict::Refused(why) => assert!(
                why.contains(bad),
                "refusal should name the flag, got {why:?}"
            ),
            ArgvVerdict::Ok { .. } => panic!("{bad} was accepted"),
        }
    }
}

#[test]
fn vet_allows_what_a_teammate_may_legitimately_choose() {
    match vet_spawn_argv(&v(&["claude", "--model", "opus", "--continue"])) {
        ArgvVerdict::Ok { argv, stripped } => {
            assert_eq!(argv, v(&["claude", "--model", "opus", "--continue"]));
            assert!(stripped.is_empty());
        }
        ArgvVerdict::Refused(why) => panic!("legitimate spawn refused: {why}"),
    }
}

#[test]
fn vet_still_strips_governed_flags_rather_than_refusing() {
    // A governed flag is atrium's to set, so asking for it is not an attack —
    // it is reported and removed, exactly as before.
    match vet_spawn_argv(&v(&["claude", "--dangerously-skip-permissions"])) {
        ArgvVerdict::Ok { argv, stripped } => {
            assert_eq!(argv, v(&["claude"]));
            assert_eq!(stripped, v(&["--dangerously-skip-permissions"]));
        }
        ArgvVerdict::Refused(why) => panic!("governed flag should strip, not refuse: {why}"),
    }
}

#[test]
fn vet_refuses_a_vendor_with_no_posture_mapping() {
    // aider and cursor-agent are agent stems atrium will bind, but it has no
    // way to cap what they may do, so a teammate must not be able to spawn one.
    for stem in ["aider", "cursor-agent", "gemini"] {
        assert!(
            matches!(vet_spawn_argv(&v(&[stem])), ArgvVerdict::Refused(_)),
            "{stem} is an agent stem with no posture mapping and should be refused"
        );
    }
    // A non-agent command is NOT this function's business: it carries no
    // permission flags to escape through, and whether it may be spawned at
    // all is the spawn allowlist's decision. Spawning a shell as a worker is
    // a supported, tested use.
    for stem in ["sh", "cmd", "bash"] {
        assert!(
            matches!(vet_spawn_argv(&v(&[stem])), ArgvVerdict::Ok { .. }),
            "{stem} should be left to the spawn allowlist"
        );
    }
}

#[test]
fn vet_accepts_a_configured_claude_alias() {
    // A second account's shim named in `claude_aliases` is claude: it takes
    // claude's flags and claude's posture. Before this, `is_agent_stem`
    // counted the alias as an agent while the flag policy matched only the
    // literal "claude", so `fleet up` refused every alias-based roster as
    // "no trust posture".
    // The name is the operator's, not a pattern: any stem they list is claude.
    crate::bind::set_claude_aliases(vec!["claude2".to_string()]);
    match vet_spawn_argv(&v(&["claude2", "--model", "opus"])) {
        ArgvVerdict::Ok { argv, stripped } => {
            assert_eq!(argv, v(&["claude2", "--model", "opus"]));
            assert!(stripped.is_empty());
        }
        ArgvVerdict::Refused(why) => panic!("alias refused: {why}"),
    }
    // The governed flags are stripped from an alias exactly as from claude.
    match vet_spawn_argv(&v(&["claude2", "--dangerously-skip-permissions"])) {
        ArgvVerdict::Ok { stripped, .. } => {
            assert_eq!(stripped, v(&["--dangerously-skip-permissions"]));
        }
        ArgvVerdict::Refused(why) => panic!("alias refused: {why}"),
    }
    // An unlisted flag on an alias is refused, as on claude.
    assert!(matches!(
        vet_spawn_argv(&v(&["claude2", "--mcp-config", "x"])),
        ArgvVerdict::Refused(_)
    ));
}

#[test]
fn allowlisted_agent_at_shallow_depth_is_allowed() {
    assert_eq!(evaluate_spawn(&v(&["claude"]), 0, 6, &[]), Ok(1));
    assert_eq!(
        evaluate_spawn(&v(&["claude", "--continue"]), 2, 6, &[]),
        Ok(3)
    );
}

#[test]
fn non_allowlisted_command_is_refused() {
    assert_eq!(
        evaluate_spawn(&v(&["rm", "-rf", "/"]), 0, 6, &[]),
        Err(SpawnDenied::NotAllowed("rm".to_string()))
    );
}

#[test]
fn extra_allow_admits_an_operator_approved_stem() {
    // `sh` is not a built-in agent, but ATRIUM_CTL_ALLOW=sh lets it spawn.
    assert_eq!(
        evaluate_spawn(&v(&["sh"]), 0, 6, &["sh".to_string()]),
        Ok(1)
    );
    // …and only the approved one; a different command is still refused.
    assert_eq!(
        evaluate_spawn(&v(&["bash"]), 0, 6, &["sh".to_string()]),
        Err(SpawnDenied::NotAllowed("bash".to_string()))
    );
}

#[test]
fn path_qualified_agent_matches_by_stem() {
    // A full path to claude still resolves to the "claude" stem.
    assert_eq!(evaluate_spawn(&v(&["/usr/bin/claude"]), 0, 6, &[]), Ok(1));
}

#[test]
fn depth_over_the_cap_is_refused() {
    assert_eq!(
        evaluate_spawn(&v(&["claude"]), 6, 6, &[]),
        Err(SpawnDenied::DepthExceeded {
            attempted: 7,
            max: 6
        })
    );
}

#[test]
fn empty_command_is_refused() {
    assert_eq!(
        evaluate_spawn(&[], 0, 6, &[]),
        Err(SpawnDenied::EmptyCommand)
    );
}

#[test]
fn resolve_target_by_id_and_role() {
    let panes = [
        (AgentId(0), Some("ceo".to_string())),
        (AgentId(1), Some("dev_1".to_string())),
        (AgentId(2), None),
    ];
    assert_eq!(resolve_target("1", &panes), Ok(AgentId(1)));
    assert_eq!(resolve_target("dev_1", &panes), Ok(AgentId(1)));
    assert!(resolve_target("9", &panes).unwrap_err().contains("no pane"));
    assert!(resolve_target("ghost", &panes)
        .unwrap_err()
        .contains("no pane"));
}

#[test]
fn resolve_target_flags_ambiguous_roles() {
    let panes = [
        (AgentId(1), Some("dev".to_string())),
        (AgentId(2), Some("dev".to_string())),
    ];
    assert!(resolve_target("dev", &panes)
        .unwrap_err()
        .contains("ambiguous"));
}

#[test]
fn in_subtree_walks_the_parent_chain() {
    // 0 (root) → 1 (lead) → 2, 3 (ICs); 4 is a sibling lead's IC.
    let a = AgentId;
    let parents = [
        (a(0), None),
        (a(1), Some(a(0))),
        (a(2), Some(a(1))),
        (a(3), Some(a(1))),
        (a(4), Some(a(5))),
        (a(5), Some(a(0))),
    ];
    assert!(in_subtree(a(2), a(1), &parents)); // IC is in its lead's subtree
    assert!(in_subtree(a(1), a(1), &parents)); // a pane is in its own subtree
    assert!(!in_subtree(a(4), a(1), &parents)); // a cousin is not
    assert!(in_subtree(a(4), a(0), &parents)); // everything is under the root
    assert!(!in_subtree(a(1), a(2), &parents)); // a parent is not under its child
}

#[test]
fn delegation_none_is_always_allowed() {
    assert!(delegation_allowed(None, None, None, false).is_ok());
    assert!(delegation_allowed(None, Some("work"), Some("prod"), false).is_ok());
}

#[test]
fn delegation_worker_may_pass_its_own_or_the_session_default() {
    // Caller holds "work"; session default is "team".
    assert!(delegation_allowed(Some("work"), Some("work"), Some("team"), false).is_ok());
    assert!(delegation_allowed(Some("team"), Some("work"), Some("team"), false).is_ok());
}

#[test]
fn delegation_worker_cannot_mint_an_unheld_identity() {
    let err = delegation_allowed(Some("prod"), Some("work"), Some("team"), false).unwrap_err();
    assert!(err.contains("prod"), "{err}");
    assert!(err.contains("delegate"), "{err}");
}

#[test]
fn delegation_operator_may_delegate_anything() {
    // privileged == the human root: any vault identity is theirs to grant.
    assert!(delegation_allowed(Some("prod"), None, Some("work"), true).is_ok());
}
