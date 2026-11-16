//! Worktree norms reach exactly the agents that leave the main tree.

use super::worktree_placement;

// -- worktree norms injection --------------------------------------------

fn make_wt(name: &str, branch: &str, agents: &[&str]) -> atrium::worktree::WorktreePlan {
    atrium::worktree::WorktreePlan {
        name: name.to_string(),
        branch: branch.to_string(),
        dir: std::path::PathBuf::from("/tmp/wt"),
        agents: agents.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn norms_block_appended_for_worktree_member() {
    // What `spawn_fleet_window` gives a worktree member: that worktree's
    // directory as its cwd, and norms naming the worktree and its branch
    // (folded into the pane's single system-prompt block by the spawn).
    let worktrees = [make_wt("norms-wt", "atrium/feat/norms", &["alice"])];
    let (dir, norms) =
        worktree_placement(&worktrees, "alice").expect("a worktree member is placed in it");
    assert_eq!(
        dir,
        std::path::PathBuf::from("/tmp/wt")
            .to_string_lossy()
            .into_owned()
    );
    assert!(
        norms.contains("norms-wt"),
        "norms must name the worktree: {norms}"
    );
    assert!(
        norms.contains("atrium/feat/norms"),
        "norms must name the branch: {norms}"
    );
}

#[test]
fn norms_block_absent_for_main_tree_member() {
    // An agent NOT listed in any WorktreePlan stays on the main tree and
    // must NOT receive the worktree norms.
    let worktrees = [make_wt("norms-wt", "atrium/feat/norms", &["alice"])];
    assert_eq!(worktree_placement(&worktrees, "bob"), None);
}

/// The child is launched with the paths that were DISCLOSED, not with the
/// file's strings resolved a second time.
///
/// Reverting this (mapping `Grant::raw` back through `resolve_dir`) is the
/// change that reopens the drift between the banner and the launch, and it
/// is invisible to every unit test of the classifier itself - the
/// classification stays correct while the spawn ignores it.
#[test]
fn a_fleet_agent_is_launched_with_the_paths_that_were_disclosed() {
    use atrium::fleet::{Agent, AgentPlan, Field, Grant, Reach};
    let g = |raw: &str, given: &str, field| Grant {
        field,
        raw: raw.to_string(),
        given: std::path::PathBuf::from(given),
        real: Some(std::path::PathBuf::from(given)),
        exists: true,
        reach: Reach::Outside,
        holds: None,
    };
    let agent = Agent {
        name: "lead\u{1b}[2J".to_string(),
        cmd: vec!["claude".to_string()],
        cwd: Some("./work".to_string()),
        add_dirs: vec!["./ctx".to_string()],
        ..Default::default()
    };
    let disclosed = AgentPlan {
        label: "lead".to_string(),
        identity: None,
        opaque: None,
        cwd: Some(g("./work", "/real/work", Field::Cwd)),
        add_dirs: vec![g("./ctx", "/real/ctx", Field::AddDir)],
    };
    let (argv, cwd, role) = super::fleet_launch(&agent, &disclosed);
    assert_eq!(
        cwd.as_deref(),
        Some("/real/work"),
        "cwd must be the resolved one"
    );
    assert_eq!(argv, vec!["claude", "--add-dir", "/real/ctx"]);
    // The pane label is the DEFANGED one from the plan, not the raw name:
    // `role` is painted into the status bar and the overview unfiltered.
    assert_eq!(role, "lead");
    assert!(!role.chars().any(|c| c.is_control()));
}

// -- headless flags ------------------------------------------------------

fn argv(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|s| s.to_string()).collect()
}

#[test]
fn an_attached_launch_keeps_every_token_for_the_session_parser() {
    let args = argv(&["--allow-ctl", "--trust", "accept"]);
    assert_eq!(super::take_headless_flags(&args), Ok((None, args.clone())));
}

#[test]
fn headless_alone_is_the_default_size_and_leaves_the_session_flags() {
    assert_eq!(super::HEADLESS_DEFAULT_SIZE, (50, 200));
    assert_eq!(
        super::take_headless_flags(&argv(&["--allow-ctl", "--headless", "--max-depth", "2"])),
        Ok((Some((50, 200)), argv(&["--allow-ctl", "--max-depth", "2"])))
    );
}

#[test]
fn size_is_rows_then_cols_in_either_spelling_and_either_order_of_flags() {
    for tokens in [
        &["--headless", "--size", "40x120"][..],
        &["--size", "40x120", "--headless"][..],
        &["--headless", "--size=40X120"][..],
    ] {
        assert_eq!(
            super::take_headless_flags(&argv(tokens)),
            Ok((Some((40, 120)), vec![])),
            "{tokens:?}"
        );
    }
}

#[test]
fn a_size_without_headless_or_without_a_value_or_twice_is_refused() {
    let err = |tokens: &[&str]| super::take_headless_flags(&argv(tokens)).unwrap_err();
    assert!(err(&["--size", "40x120"]).contains("--headless"));
    assert!(err(&["--headless", "--size"]).contains("<rows>x<cols>"));
    assert!(err(&["--headless", "--size", "40x120", "--size=50x200"]).contains("twice"));
    // The value is consumed as the size, never left to be read as a flag.
    assert!(err(&["--headless", "--size", "--allow-ctl"]).contains("--allow-ctl"));
}

#[test]
fn a_size_is_two_whole_numbers_inside_the_bounds() {
    let ok = super::parse_headless_size;
    assert_eq!(ok("50x200"), Ok((50, 200)));
    assert_eq!(ok("3x10"), Ok((3, 10)), "the floor");
    assert_eq!(ok("500x1000"), Ok((500, 1000)), "the ceiling");
    for bad in [
        "",
        "x",
        "50",
        "50x",
        "x200",
        "50x200x3",
        "50*200",
        "200,50",
        " 50x200",
        "50 x 200",
        "+50x200",
        "-5x200",
        "5.0x200",
        "0x0",
        "2x200",
        "50x9",
        "501x200",
        "50x1001",
        "70000x200",
        "50x200 ",
    ] {
        let err = ok(bad).expect_err(bad);
        assert!(
            err.contains("<rows>x<cols>") && err.contains(&format!("{bad:?}")),
            "{bad:?} -> {err}"
        );
    }
}

#[test]
fn headless_is_refused_without_the_control_channel() {
    let err = super::headless_gate(false, true).unwrap_err();
    assert!(
        err.contains("--allow-ctl") && err.contains("allow_ctl"),
        "{err}"
    );
}

#[test]
fn headless_is_refused_when_nobody_approved_the_launch_in_advance() {
    let err = super::headless_gate(true, false).unwrap_err();
    assert!(err.contains("ATRIUM_YES"), "{err}");
    assert_eq!(super::headless_gate(true, true), Ok(()));
    // Unreachable comes first: approving a session nobody can reach is no use.
    assert!(super::headless_gate(false, false)
        .unwrap_err()
        .contains("--allow-ctl"));
}

/// The fleet banner says a governed flag in `cmd` is ignored, so the launch
/// must actually drop it — with its value — and keep everything else.
#[test]
fn a_fleet_agent_is_launched_without_the_flags_the_banner_ignored() {
    use atrium::fleet::{Agent, AgentPlan};
    let agent = Agent {
        name: "lead".to_string(),
        cmd: vec![
            "claude".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--permission-mode".to_string(),
            "bypassPermissions".to_string(),
            "--allowedTools=Bash(*)".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ],
        prompt: Some("--allowedTools is atrium's to set".to_string()),
        ..Default::default()
    };
    let disclosed = AgentPlan {
        label: "lead".to_string(),
        identity: None,
        opaque: None,
        cwd: None,
        add_dirs: vec![],
    };
    let (argv, _, _) = super::fleet_launch(&agent, &disclosed);
    assert_eq!(
        argv,
        vec![
            "claude",
            "--model",
            "opus",
            "--append-system-prompt",
            "--allowedTools is atrium's to set",
        ]
    );
}
