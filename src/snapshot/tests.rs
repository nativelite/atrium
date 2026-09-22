//! Capturing panes into a snapshot, and finding the latest one.

use atrium::ctl::TrustMode;

/// A legacy snapshot whose atrium is still running is someone's live session,
/// not a candidate to recover — even when it is the newest file there.
#[test]
fn find_latest_skips_a_snapshot_whose_atrium_is_still_running() {
    let tmp = std::env::temp_dir().join(format!("atrium_snap_live_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("atrium-session-100.json"), b"{}").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(tmp.join("atrium-session-200.json"), b"{}").unwrap();
    let got = super::find_latest_snapshot(&tmp, |pid| pid == 200);
    assert_eq!(
        got.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
        Some("atrium-session-100.json".to_string()),
        "the running session (200) is newer but must be skipped"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn find_latest_picks_newest_json_and_ignores_others() {
    let tmp = std::env::temp_dir().join(format!("atrium_snap_latest_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // Non-matching files must be ignored.
    std::fs::write(tmp.join("atrium-session-1.pids"), b"pids").unwrap();
    std::fs::write(tmp.join("other.json"), b"{}").unwrap();
    // Two matching files — write B after A so B has mtime >= A.
    let a = tmp.join("atrium-session-100.json");
    let b = tmp.join("atrium-session-200.json");
    std::fs::write(&a, b"{}").unwrap();
    std::fs::write(&b, b"{}").unwrap();
    let result = super::find_latest_snapshot(&tmp, |_| false);
    assert!(result.is_some(), "must find a snapshot");
    let got = result.unwrap();
    assert!(
        got == a || got == b,
        "must return one of the two json files"
    );
    // The returned file's mtime must be >= the other's.
    let got_mt = std::fs::metadata(&got).unwrap().modified().unwrap();
    let other = if got == a { &b } else { &a };
    let other_mt = std::fs::metadata(other).unwrap().modified().unwrap();
    assert!(
        got_mt >= other_mt,
        "returned file must be newest: got {got_mt:?} vs {other_mt:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn find_latest_unreadable_dir_returns_none() {
    let missing = std::path::PathBuf::from("/no/such/directory/atrium_test_xyz");
    assert!(super::find_latest_snapshot(&missing, |_| false).is_none());
}

#[test]
fn find_latest_dir_with_no_matching_files_returns_none() {
    let tmp = std::env::temp_dir().join(format!("atrium_snap_none_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("atrium-session-1.pids"), b"pids").unwrap();
    std::fs::write(tmp.join("unrelated.json"), b"{}").unwrap();
    assert!(super::find_latest_snapshot(&tmp, |_| false).is_none());
    let _ = std::fs::remove_dir_all(&tmp);
}

// --- Bug 2: worktree field flows through the real snapshot mapping path ---

/// Guards the `p.worktree -> PaneCapture.worktree` mapping inside
/// `snapshot_if_changed` (main.rs:818-827, now via `capture_pane_fields`).
///
/// The original bug: `spawn_worker_window`/`spawn_worker_here` never set
/// `pane.worktree`, so every live snapshot saw `None` even for worktree
/// panes. The fix adds `pane.worktree = sp.worktree.clone()` in both spawn
/// paths. This test fails if `worktree` is dropped or hardcoded to `None`
/// in `capture_pane_fields` — the function `snapshot_if_changed` calls with
/// `p.worktree.clone()` as the final argument.
#[test]
fn capture_pane_fields_propagates_worktree() {
    let cap = super::capture_pane_fields(
        7,
        Some("fix".to_string()),
        vec!["claude".to_string()],
        Some("/work".to_string()),
        None,
        Some("sess-fix".to_string()),
        Some("fix".to_string()),
        vec!["git push".to_string()],
        false,
        1,
        Some(0),
        TrustMode::Edits,
        false,
        None,
        Vec::new(),
    );
    assert_eq!(
        cap.worktree.as_deref(),
        Some("fix"),
        "worktree must flow through capture_pane_fields unchanged"
    );
    // None must propagate too — a non-worktree pane must not invent a name.
    let cap_none = super::capture_pane_fields(
        8,
        None,
        vec!["bash".to_string()],
        None,
        None,
        None,
        None,
        Vec::new(),
        true,
        0,
        None,
        TrustMode::Off,
        false,
        None,
        Vec::new(),
    );
    assert!(
        cap_none.worktree.is_none(),
        "non-worktree pane must have None"
    );
}

// --- recovery restores the session policy, not just the layout ---------

/// Every capability field must survive the one mapping from a live `Pane` to
/// a `PaneCapture`. Dropping one here is exactly how the deny list and the
/// `can_spawn` bit went missing from recovery: the pane held them, the
/// snapshot never saw them.
#[test]
fn capture_pane_fields_propagates_the_capability_fields() {
    let cap = super::capture_pane_fields(
        2,
        Some("builder".to_string()),
        vec!["claude".to_string()],
        None,
        None,
        None,
        None,
        vec!["cargo test --workspace".to_string()],
        false,
        3,
        Some(1),
        TrustMode::Plan,
        false,
        None,
        Vec::new(),
    );
    assert_eq!(cap.deny, vec!["cargo test --workspace".to_string()]);
    assert!(
        !cap.can_spawn,
        "the roster withheld it; so must the snapshot"
    );
    assert_eq!(cap.depth, 3);
    assert_eq!(cap.parent_pane, Some(1));
    assert_eq!(cap.mode, Some(TrustMode::Plan));
}
