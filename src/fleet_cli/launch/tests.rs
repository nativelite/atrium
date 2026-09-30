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
