//! Helpers shared by the ctl test modules.

pub(super) fn v(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

// ---- W1: bus topics reply + render ----------------------------------
