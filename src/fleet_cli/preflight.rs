//! The checks and banner lines `fleet up` prints before asking the operator.

use super::fsan;
use super::up::FleetLaunch;
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
fn ctl_without_spawner(allow_ctl: bool, spawner_count: usize) -> bool {
    allow_ctl && spawner_count == 0
}

/// The preflight warning for a roster at or past the host's pane cap
/// ([`atrium::resources::effective_cap`]), or `None` when it fits. Never a
/// refusal: every agent in the roster starts. What the cap still governs is a
/// mid-run `ctl spawn`, so a full roster with a spawner is worth a warning too.
fn pane_cap_warning(agents: usize, spawners: usize, cap: usize) -> Option<String> {
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
fn deny_unbound(fleet: &atrium::fleet::Fleet) -> Vec<String> {
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
fn build_pool_line(size: Option<usize>, inherited: bool) -> (String, Option<String>) {
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

/// The machine state the banner reports alongside the roster: read from the
/// environment once, just before it prints, so [`banner_lines`] itself is a
/// pure function of what it is given.
pub(super) struct BannerFacts {
    /// The compile pool this launch would create (`None`: none).
    pub(super) pool_size: Option<usize>,
    /// An enclosing session's pool is already in the environment.
    pub(super) pool_inherited: bool,
    pub(super) memory_cap: atrium::memguard::Cap,
    pub(super) memory_enforcement: atrium::memguard::Enforcement,
    /// Deny rules from the environment (built-ins + `ATRIUM_DENY`).
    pub(super) env_deny: Vec<String>,
    /// How many panes this machine is allowed to carry.
    pub(super) pane_cap: usize,
    /// Colour the preflight block.
    pub(super) color: bool,
}

impl BannerFacts {
    pub(super) fn read(fleet: &atrium::fleet::Fleet) -> BannerFacts {
        BannerFacts {
            pool_size: atrium::buildpool::planned_size(fleet.build_jobs),
            pool_inherited: atrium::buildpool::inherited(),
            memory_cap: atrium::memguard::planned_cap(fleet.memory_mb),
            memory_enforcement: atrium::memguard::enforcement(),
            env_deny: atrium::trust::parse_deny_env(),
            pane_cap: atrium::resources::effective_cap(),
            color: std::io::IsTerminal::is_terminal(&std::io::stderr())
                && std::env::var_os("NO_COLOR").is_none(),
        }
    }
}

/// Everything `fleet up` shows the operator before asking, one line each, in
/// the order it prints.
///
/// The disclosure, then the posture, then the verdict, then the Enter.
///
/// Order is load-bearing and was measured: a six-agent roster's banner is
/// taller than a 24-row terminal, so whatever prints FIRST is what scrolls
/// away. The per-grant lines are the long, skimmable part; the posture, the
/// spawn capability and the verdict are the short, decisive part, so they go
/// last and are still on screen when the prompt appears.
pub(super) fn banner_lines(launch: &FleetLaunch, facts: &BannerFacts) -> Vec<String> {
    let fleet = &launch.fleet;
    let mut out: Vec<String> = launch
        .plan
        .banner_lines()
        .into_iter()
        .map(|line| format!("atrium fleet: {line}"))
        .collect();
    let mut warnings = launch.warnings.clone();
    // Always say the posture out loud. A fleet file can be authored by an agent
    // and skimmed by a human; a line naming what everything is about to run under
    // is the difference between reviewing it and assuming it.
    //
    // The compile budget is part of that posture: a fleet's builds are what
    // exhaust a machine, not its agents. Stated from the plan here; the pool
    // itself is created only once the operator approves.
    let (pool_suffix, pool_warning) = build_pool_line(facts.pool_size, facts.pool_inherited);
    let memory = atrium::memguard::describe(facts.memory_cap, facts.memory_enforcement);
    // Session-wide deny rules as every claude pane will carry them (built-ins
    // + ATRIUM_DENY + the fleet's); per-agent rules add to this.
    let mut session_deny = facts.env_deny.clone();
    session_deny.extend(fleet.deny.iter().cloned());
    let denied = atrium::trust::deny_args(&session_deny, &[]).len() - 1;
    out.push(format!(
        "atrium fleet: \"{}\" starting {} agent(s) at trust {}{}{pool_suffix}, {memory}, \
         {denied} deny rules",
        fsan(launch.name),
        fleet.agents.len(),
        launch.trust.policy_label(),
        if launch.allow_ctl {
            ", ctl on"
        } else {
            ", ctl OFF"
        }
    ));
    // Deny rules only reach claude (codex has no equivalent flag). Say so when a
    // rule is aimed at an agent it can't bind, rather than let it read as enforced.
    let unbound = deny_unbound(fleet);
    if !unbound.is_empty() {
        warnings.push(format!(
            "deny rules apply to claude agents only — not enforced for {}",
            unbound
                .iter()
                .map(|n| fsan(n))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(warning) = pool_warning {
        warnings.push(
            warning
                .strip_prefix("warning: ")
                .unwrap_or(&warning)
                .to_string(),
        );
    }
    // Name any agent that runs at a DIFFERENT posture than the session — part of
    // what the human approves (a mixed-model fleet often runs its haiku agents at
    // `accept` under an `automode` session).
    if !launch.trust_overrides.is_empty() {
        let shown = launch
            .trust_overrides
            .iter()
            .map(|(n, m)| format!("{n}={m}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!("atrium fleet: per-agent trust overrides — {shown}"));
    }

    // Who may create teammates is part of what the human approves, so say it.
    let spawners: Vec<String> = launch
        .plan
        .agents
        .iter()
        .zip(fleet.agents.iter())
        .filter(|(_, a)| a.can_spawn.unwrap_or(false))
        .map(|(ap, _)| ap.label.clone())
        .collect();
    out.push(format!(
        "atrium fleet: {} of {} may spawn teammates{}",
        spawners.len(),
        fleet.agents.len(),
        if spawners.is_empty() {
            String::new()
        } else {
            format!(" ({})", spawners.join(", "))
        }
    ));
    // A control-plane fleet where nobody may spawn is almost always a
    // misconfigured lead: the coordinator comes up unable to create the very
    // teammates it exists to manage, and the gap stays invisible until the
    // first mid-run spawn is denied. Surface it here, where the roster can
    // still be fixed.
    if ctl_without_spawner(launch.allow_ctl, spawners.len()) {
        warnings.push(
            "ctl is on but no agent may spawn teammates — if this fleet coordinates by \
             spawning, set \"can_spawn\": true on its lead"
                .to_string(),
        );
    }
    // The pane cap is a preflight warning, not a gate: the operator chose this
    // roster and the machine may well carry it. What the cap still does is refuse
    // a mid-run `ctl spawn` past it, and that is worth knowing before launch.
    if let Some(w) = pane_cap_warning(fleet.agents.len(), spawners.len(), facts.pane_cap) {
        warnings.push(w);
    }
    // Per-agent worktrees: say which agents leave the main tree, where, and on
    // what branch — isolation the operator is approving as much as the dirs above.
    if !launch.wt_plans.is_empty() {
        if launch.wt_active {
            out.push(format!(
                "atrium fleet: per-agent worktrees — {} off HEAD; auto-removed on exit only if \
                 clean + merged, else kept:",
                launch.wt_plans.len()
            ));
            for p in &launch.wt_plans {
                let who = p
                    .agents
                    .iter()
                    .map(|a| fsan(a))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push(format!(
                    "atrium fleet:   {} → {} (branch {}) [{who}]",
                    fsan(&p.name),
                    atrium::fleet::show_path(&p.dir),
                    fsan(&p.branch)
                ));
            }
        } else {
            warnings.push(format!(
                "this fleet asks for per-agent worktrees, but {} is not a git repo — every \
                 agent shares the main tree",
                atrium::fleet::show_path(&launch.cwd)
            ));
        }
    }
    // Every preflight warning, together, as the last thing before the verdict.
    out.extend(atrium::fleet::preflight_block(&warnings, facts.color));
    // The last line before the block, so it cannot scroll: it NAMES the
    // destinations rather than counting them. A surviving summary line that
    // omits the payload is the one thing an operator is guaranteed to read and
    // the one thing that tells them nothing.
    out.push(format!("atrium fleet: {}", launch.plan.verdict()));
    out
}

#[cfg(test)]
mod tests;
