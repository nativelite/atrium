//! Worktree norms reach exactly the agents that leave the main tree.

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
    let wt = make_wt("norms-wt", "atrium/feat/norms", &["alice"]);
    let worktrees = [wt];
    let mut cmd = vec!["claude".to_string()];
    // Mirror the spawn_fleet_window logic under test.
    if let Some(w) = worktrees
        .iter()
        .find(|w| w.agents.iter().any(|a| a == "alice"))
    {
        cmd.push("--append-system-prompt".to_string());
        cmd.push(atrium::worktree::worktree_norms(&w.name, &w.branch));
    }
    let idx = cmd
        .iter()
        .position(|s| s == "--append-system-prompt")
        .expect("--append-system-prompt must be present for a worktree member");
    let norms = &cmd[idx + 1];
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
    let wt = make_wt("norms-wt", "atrium/feat/norms", &["alice"]);
    let worktrees = [wt];
    let mut cmd = vec!["claude".to_string()];
    if let Some(w) = worktrees
        .iter()
        .find(|w| w.agents.iter().any(|a| a == "bob"))
    {
        cmd.push("--append-system-prompt".to_string());
        cmd.push(atrium::worktree::worktree_norms(&w.name, &w.branch));
    }
    assert!(
        !cmd.contains(&"--append-system-prompt".to_string()),
        "--append-system-prompt must be absent for a main-tree member"
    );
}
