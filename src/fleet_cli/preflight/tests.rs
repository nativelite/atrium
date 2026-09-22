//! The preflight checks and the banner lines they produce.

use super::{
    build_pool_line, ctl_without_spawner, deny_unbound, pane_cap_warning, plugin_value_enabled,
    preflight_context_mode,
};

#[test]
fn a_roster_past_the_pane_cap_is_warned_about_never_refused() {
    assert_eq!(pane_cap_warning(8, 1, 30), None, "fits");
    assert_eq!(pane_cap_warning(30, 0, 30), None, "full, but nobody spawns");
    let over = pane_cap_warning(40, 0, 30).expect("over the cap");
    assert!(
        over.contains("40 agents") && over.contains("pane cap of 30"),
        "{over}"
    );
    assert!(over.contains("all of them will start"), "{over}");
    assert!(over.contains("ATRIUM_MAX_PANES"), "{over}");
    let full = pane_cap_warning(30, 1, 30).expect("full with a spawner");
    assert!(full.contains("can't add teammates"), "{full}");
}

#[test]
fn preflight_silently_skips_missing_settings() {
    // The preflight check is best-effort: if settings.json doesn't exist,
    // no warning is printed. This test just verifies the function doesn't panic
    // when called — actual output verification would require mocking file I/O.
    preflight_context_mode();
}

// -- plugin_value_enabled pure seam --------------------------------------

#[test]
fn plugin_enabled_boolean_true_is_enabled() {
    assert!(plugin_value_enabled(&json::parse("true").unwrap()));
}

#[test]
fn plugin_enabled_boolean_false_is_disabled() {
    assert!(!plugin_value_enabled(&json::parse("false").unwrap()));
}

#[test]
fn plugin_enabled_null_is_disabled() {
    assert!(!plugin_value_enabled(&json::parse("null").unwrap()));
}

#[test]
fn plugin_enabled_number_is_disabled() {
    // JS truthiness would consider 1 true — strict JSON boolean must not.
    assert!(!plugin_value_enabled(&json::parse("1").unwrap()));
}

#[test]
fn plugin_enabled_string_is_disabled() {
    assert!(!plugin_value_enabled(&json::parse("\"yes\"").unwrap()));
}

#[test]
fn ctl_fleet_without_a_spawner_warns() {
    // A coordinating fleet (ctl on) with no spawn-capable agent is the
    // misconfigured-lead case that stranded a real run.
    assert!(ctl_without_spawner(true, 0));
}

#[test]
fn ctl_fleet_with_a_spawner_is_fine() {
    assert!(!ctl_without_spawner(true, 1));
}

#[test]
fn no_ctl_never_warns() {
    // Without the control plane there are no teammates to spawn — silence.
    assert!(!ctl_without_spawner(false, 0));
}

#[test]
fn deny_rules_aimed_at_a_non_claude_agent_are_called_out() {
    let fleet = |fleet_deny: &[&str], agents: &[(&str, &str, &[&str])]| atrium::fleet::Fleet {
        grid: None,
        identity: None,
        trust: None,
        allow_ctl: None,
        context: None,
        topics: None,
        worktrees: None,
        worktree_base: None,
        worktree_seed: None,
        build_jobs: None,
        memory_mb: None,
        deny: fleet_deny.iter().map(|s| s.to_string()).collect(),
        claude_aliases: Vec::new(),
        agents: agents
            .iter()
            .map(|(name, cmd, deny)| atrium::fleet::Agent {
                name: name.to_string(),
                cmd: vec![cmd.to_string()],
                deny: deny.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            })
            .collect(),
    };
    // No rules anywhere: nothing to warn about, whatever the agents are.
    assert!(deny_unbound(&fleet(&[], &[("c", "codex", &[])])).is_empty());
    // Fleet-wide rules reach claude but not codex.
    assert_eq!(
        deny_unbound(&fleet(
            &["npm publish"],
            &[("a", "claude", &[]), ("c", "codex", &[])]
        )),
        vec!["c".to_string()]
    );
    // A non-claude agent's own rules are unbound; a claude agent's are fine.
    assert_eq!(
        deny_unbound(&fleet(
            &[],
            &[("a", "claude", &["x"]), ("c", "codex", &["y"])]
        )),
        vec!["c".to_string()]
    );
}

#[test]
fn the_banner_names_the_compile_budget() {
    let (pooled, warn) = build_pool_line(Some(16), false);
    assert_eq!(pooled, ", 16 compile jobs shared");
    assert_eq!(warn, None);
    // Inherited from an enclosing session: shared, just not ours to size.
    let (nested, warn) = build_pool_line(None, true);
    assert!(nested.contains("enclosing"), "{nested}");
    assert_eq!(warn, None);
    // Off (or failed to create) must add a warning line, not pass silently.
    let (off, warn) = build_pool_line(None, false);
    assert!(off.contains("OFF"), "{off}");
    assert!(warn.is_some_and(|w| w.starts_with("warning:")));
}
