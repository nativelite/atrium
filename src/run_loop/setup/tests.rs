//! Session setup.

use super::*;

/// A recovery re-installs the merged deny list as the fleet's, and `run()`
/// re-reads `ATRIUM_DENY` on top of it. Without the dedupe the recorded list
/// grows by the env entries once per recovery generation.
#[test]
fn a_recorded_deny_list_does_not_grow_across_recoveries() {
    let merged = vec![
        "git push".to_string(),
        "cargo bench".to_string(),
        "git push".to_string(),
    ];
    assert_eq!(
        dedup_preserving_order(merged),
        vec!["git push".to_string(), "cargo bench".to_string()],
        "repeats collapse, first-seen order survives"
    );
}
