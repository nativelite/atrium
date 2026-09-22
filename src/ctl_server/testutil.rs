//! Fixtures shared by the ctl_server test modules.

use crate::*;

/// root 0 (lead) -> 1 (builder), 2 (reviewer); root 3 is another fleet's lead.
pub(super) fn crew() -> (
    Vec<(AgentId, Option<String>)>,
    Vec<(AgentId, Option<AgentId>)>,
) {
    let c = vec![
        (AgentId(0), Some("lead".to_string())),
        (AgentId(1), Some("builder".to_string())),
        (AgentId(2), Some("reviewer".to_string())),
        (AgentId(3), None),
    ];
    let p = vec![
        (AgentId(0), None),
        (AgentId(1), Some(AgentId(0))),
        (AgentId(2), Some(AgentId(0))),
        (AgentId(3), None),
    ];
    (c, p)
}
