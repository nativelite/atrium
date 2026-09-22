//! The checks and banner lines `fleet up` prints before asking the operator.

use std::path::PathBuf;

/// Path to the Claude Code settings file: ~/.claude/settings.json on all platforms.
fn settings_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.map(|h| h.join(".claude").join("settings.json"))
}

/// Returns `true` iff `v` is the JSON boolean `true`.
///
/// Strict boolean semantics: JSON numbers, strings, null, and arrays are all
/// `false` — this is not JavaScript truthiness. Only an explicit `true` in the
/// settings JSON counts as "enabled".
fn plugin_value_enabled(v: &json::Value) -> bool {
    v.as_bool().unwrap_or(false)
}

/// Check if context-mode@context-mode is enabled in ~/.claude/settings.json.
/// Returns true if enabled or settings not found/unreadable (best-effort).
/// Prints a warning and install hint if it's missing (but continues).
pub(super) fn preflight_context_mode() {
    let path = match settings_path() {
        Some(p) => p,
        None => return, // No home dir, skip check
    };

    if !path.exists() {
        return; // Settings file doesn't exist yet, skip check
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return, // Can't read, skip check (best-effort)
    };

    let root = match json::parse(&text) {
        Ok(v) => v,
        Err(_) => return, // Malformed JSON, skip check
    };

    let obj = match root.as_object() {
        Some(o) => o,
        None => return, // Top-level not an object, skip check
    };

    let enabled = match obj.iter().find(|(k, _)| k == "enabledPlugins") {
        Some((_, v)) => v,
        None => return, // No enabledPlugins key, skip check
    };

    // enabledPlugins is either an array ["pkg@name", …] (older format) or an
    // object {"pkg@name": true, …} (current Claude Code format). Both are valid.
    let has_context_mode = if let Some(arr) = enabled.as_array() {
        arr.iter().any(|p| {
            p.as_str()
                .map_or(false, |s| s == "context-mode@context-mode")
        })
    } else if let Some(map) = enabled.as_object() {
        map.iter()
            .any(|(k, v)| k == "context-mode@context-mode" && plugin_value_enabled(v))
    } else {
        return; // unexpected shape, skip check
    };

    if !has_context_mode {
        eprintln!("atrium fleet: warning: context-mode@context-mode not in enabledPlugins");
        eprintln!("  run: /plugin install context-mode@context-mode");
    }
}

/// A control-plane-enabled fleet with no spawn-capable agent is almost always a
/// misconfigured lead: the coordinator comes up unable to create the very
/// teammates it exists to manage, and the gap stays invisible until the first
/// mid-run `ctl spawn` is denied. Worth a launch-time warning, where the roster
/// can still be fixed. `can_spawn` stays explicit and default-false by design —
/// this only surfaces the likely-misconfigured case, it grants nothing.
pub(super) fn ctl_without_spawner(allow_ctl: bool, spawner_count: usize) -> bool {
    allow_ctl && spawner_count == 0
}

/// The preflight warning for a roster at or past the host's pane cap
/// ([`atrium::resources::effective_cap`]), or `None` when it fits. Never a
/// refusal: every agent in the roster starts. What the cap still governs is a
/// mid-run `ctl spawn`, so a full roster with a spawner is worth a warning too.
pub(super) fn pane_cap_warning(agents: usize, spawners: usize, cap: usize) -> Option<String> {
    if agents > cap {
        Some(format!(
            "{agents} agents is more than this machine's pane cap of {cap} — all of them will \
             start, but each agent uses real memory; raise the cap with {} if the machine can \
             carry it",
            atrium::resources::ENV_MAX_PANES
        ))
    } else if agents == cap && spawners > 0 {
        Some(format!(
            "this roster fills the pane cap of {cap} — its spawners can't add teammates mid-run \
             (raise the cap with {})",
            atrium::resources::ENV_MAX_PANES
        ))
    } else {
        None
    }
}

/// Names of the agents a `deny` rule is aimed at but can't bind: every
/// non-claude agent when the fleet has session-wide rules, and any non-claude
/// agent with rules of its own.
pub(super) fn deny_unbound(fleet: &atrium::fleet::Fleet) -> Vec<String> {
    fleet
        .agents
        .iter()
        .filter(|a| !fleet.deny.is_empty() || !a.deny.is_empty())
        .filter(|a| {
            a.cmd
                .first()
                .is_some_and(|c| !atrium::bind::is_claude_stem(&atrium::bind::command_stem(c)))
        })
        .map(|a| a.name.clone())
        .collect()
}

/// The session compile pool as the banner states it: a suffix for the posture
/// line, plus a warning line when builds will run unpooled. `size` is the pool
/// this session created; `inherited` means an enclosing session's pool is already
/// in the environment. It rides on the posture line because the banner must fit a
/// short terminal (see the banner-order test) and the budget is part of the
/// posture being approved.
pub(super) fn build_pool_line(size: Option<usize>, inherited: bool) -> (String, Option<String>) {
    match (size, inherited) {
        (Some(n), _) => (format!(", {n} compile jobs shared"), None),
        (None, true) => (
            ", compile jobs shared with the enclosing session".to_string(),
            None,
        ),
        (None, false) => (
            ", build pool OFF".to_string(),
            Some(format!(
                "warning: build pool OFF — each agent's builds size themselves to the whole \
                 machine (set build_jobs or {})",
                atrium::buildpool::ENV_BUILD_JOBS
            )),
        ),
    }
}

#[cfg(test)]
mod tests;
