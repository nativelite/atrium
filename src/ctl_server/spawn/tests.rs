//! Placing a spawned worker in its own worktree.

use super::*;

// -- worktree_spawn_params error-path -------------------------------------

/// A non-git directory must produce a clear `Err` rather than silently
/// returning a path that was never created (the original bug).
#[test]
fn worktree_spawn_params_non_git_dir_returns_err() {
    let tmp = std::env::temp_dir().join("atrium_test_non_git");
    std::fs::create_dir_all(&tmp).ok();
    let result = worktree_spawn_params(&tmp, Some("my-wt"));
    assert!(
        result.is_err(),
        "expected Err for non-git dir, got Ok: {result:?}"
    );
    let msg = result.unwrap_err();
    assert!(
        msg.contains("could not create worktree"),
        "message should identify the failure: {msg}"
    );
}

/// A real git repo must produce `Ok` with a `Some` dir that actually exists.
#[test]
fn worktree_spawn_params_git_repo_yields_ok_and_creates_dir() {
    let tmp = std::env::temp_dir().join("atrium_test_git_repo");
    // Clean slate so the test is idempotent.
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // Initialise a minimal git repo with one commit so `worktree add` has HEAD.
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&tmp)
            .output()
    };
    git(&["init"]).unwrap();
    // An explicit identity: on a machine with no global git user (a fresh
    // Linux box, CI) the commit silently fails and there is no HEAD to branch.
    let commit = git(&[
        "-c",
        "user.name=atrium-test",
        "-c",
        "user.email=atrium-test@example.invalid",
        "commit",
        "--allow-empty",
        "-m",
        "init",
    ])
    .unwrap();
    assert!(commit.status.success(), "fixture commit failed: {commit:?}");
    let result = worktree_spawn_params(&tmp, Some("test-wt"));
    assert!(result.is_ok(), "expected Ok for valid git repo: {result:?}");
    let (dir, norms) = result.unwrap();
    let dir = dir.expect("dir must be Some");
    let norms = norms.expect("norms must be Some");
    assert!(
        std::path::Path::new(&dir).exists(),
        "worktree directory must have been created: {dir}"
    );
    assert!(!norms.is_empty(), "norms must not be empty");
    // Cleanup.
    let _ = std::fs::remove_dir_all(&tmp);
}
