//! What the audit log records for each reply.

use super::*;

// -- audit_outcome over the typed Reply (r10 B12) ------------------------

#[test]
fn audit_outcome_names_the_salient_ids_from_the_typed_reply() {
    use atrium::ctl::{self, AgentId};
    assert_eq!(
        audit_outcome(&ctl::reply_err("nope")),
        (false, "nope".to_string())
    );
    assert_eq!(
        audit_outcome(&ctl::reply_spawned(AgentId(3), Some("dev"), None, None)),
        (true, "pane=3".to_string())
    );
    assert_eq!(
        audit_outcome(&ctl::reply_status_one(AgentId(2), None, 0, None)),
        (true, "pane=2".to_string())
    );
    assert_eq!(
        audit_outcome(&ctl::reply_killed(&[AgentId(1), AgentId(4)])),
        (true, "killed=1,4".to_string())
    );
    assert_eq!(
        audit_outcome(&ctl::reply_sent(AgentId(5), false)),
        (true, "target=5".to_string())
    );
    assert_eq!(
        audit_outcome(&ctl::reply_bus_resolved(7, true)),
        (true, String::new())
    );
}
