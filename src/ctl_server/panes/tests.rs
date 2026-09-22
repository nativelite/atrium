//! Pane lookups.

use super::*;
use crate::ctl_server::testutil::crew;

/// A role names one live pane: a second spawn wearing it is refused, so
/// no pane can subscribe, post or be addressed as another.
#[test]
fn a_role_held_by_a_live_pane_is_not_free() {
    let (c, _) = crew();
    assert_eq!(role_holder("lead", &c), Some(AgentId(0)));
    assert_eq!(role_holder("reviewer", &c), Some(AgentId(2)));
    assert_eq!(role_holder("fixer", &c), None);
    assert_eq!(
        role_holder("pane 4", &c),
        None,
        "an unrolled pane's label is not a role"
    );
}
