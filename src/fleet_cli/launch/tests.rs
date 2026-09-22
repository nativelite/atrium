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
