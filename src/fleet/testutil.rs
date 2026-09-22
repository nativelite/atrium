//! Helpers shared by the fleet test modules.

pub(super) fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

// --- parse ---------------------------------------------------------------
