//! The preflight checks and the banner lines they produce.

use super::{
    banner_lines, build_pool_line, codex_lines, codex_unbound, ctl_without_spawner, deny_unbound,
    pane_cap_warning, plugin_value_enabled, preflight_context_mode, BannerFacts,
};
use crate::fleet_cli::up::FleetLaunch;

/// A launch of fleet `f` (JSON `fleet`), granting no directories, with the
/// planning warnings `warnings`.
fn launch_of(fleet: &str, allow_ctl: bool, warnings: &[&str]) -> FleetLaunch<'static> {
    let dir = std::env::temp_dir();
    let fleet = atrium::fleet::parse(&format!(r#"{{"fleets":{{"f":{fleet}}}}}"#))
        .expect("test fleet parses")
        .get("f")
        .expect("fleet f")
        .clone();
    let anchor = atrium::fleet::anchor_for(&dir, &dir, false, None);
    let plan = atrium::fleet::Plan::build(
        &fleet,
        &dir.join("atrium.fleet.json"),
        &dir,
        anchor,
        &atrium::fleet::Stores::default(),
    );
    FleetLaunch {
        name: "f",
        cwd: dir,
        fleet,
        allow_ctl,
        trust: atrium::ctl::TrustMode::Auto,
        trust_overrides: Vec::new(),
        warnings: warnings.iter().map(|w| w.to_string()).collect(),
        grid: atrium::spawn::Grid::balanced(2),
        plan,
        wt_plans: Vec::new(),
        wt_active: false,
    }
}

/// Fixed machine state, so a banner test depends on nothing but its launch.
fn facts() -> BannerFacts {
    BannerFacts {
        pool_size: Some(4),
        pool_inherited: false,
        memory_cap: atrium::memguard::Cap::Dynamic,
        memory_enforcement: atrium::memguard::Enforcement::Hard,
        env_deny: Vec::new(),
        pane_cap: 30,
        color: false,
        mod_line: "mod: installed 0.1.0 at /cfg/mod".to_string(),
        mod_missing: false,
        teams_on: true,
        mod_on: true,
        teams_var: false,
        codex_hooks_unreviewed: Vec::new(),
        codex_hooks_reviewed: Vec::new(),
    }
}

/// What a codex pane does about codex's hooks review is part of the posture
/// the operator approves: a line names the agents whose hooks run unreviewed,
/// a warning names those whose codex cannot skip the review screen.
#[test]
fn the_banner_says_whose_codex_hooks_run_unreviewed_and_whose_cannot() {
    let launch = launch_of(TWO_AGENTS, true, &[]);
    let quiet = banner_lines(&launch, &facts());
    assert!(
        !quiet.iter().any(|l| l.contains("codex hooks")),
        "{quiet:?}"
    );

    let mut unreviewed = facts();
    unreviewed.codex_hooks_unreviewed = vec!["cx".to_string()];
    let lines = banner_lines(&launch, &unreviewed);
    let line = lines
        .iter()
        .find(|l| l.contains("codex hooks run unreviewed for cx"))
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert!(line.contains(atrium::vendors::CODEX_BYPASS_HOOK_TRUST));
    assert!(!line.contains("warning"));

    let mut reviewed = facts();
    reviewed.codex_hooks_reviewed = vec!["cx".to_string()];
    let lines = banner_lines(&launch, &reviewed);
    assert!(
        lines.iter().any(|l| l.contains("codex has no")
            && l.contains(atrium::vendors::CODEX_BYPASS_HOOK_TRUST)
            && l.contains("for cx")),
        "{lines:?}"
    );
}

const TWO_AGENTS: &str = r#"{"deny":["Bash(git push*)"],"agents":[
    {"name":"lead","cmd":["claude"],"can_spawn":true},
    {"name":"cx","cmd":["codex"]}]}"#;

#[test]
fn the_verdict_is_the_last_line_and_the_posture_follows_every_grant_line() {
    let launch = launch_of(TWO_AGENTS, true, &[]);
    let lines = banner_lines(&launch, &facts());
    let grants = launch.plan.banner_lines().len();
    assert_eq!(
        lines.last(),
        Some(&format!("atrium fleet: {}", launch.plan.verdict())),
        "the verdict is what is still on screen at the prompt: {lines:#?}"
    );
    assert!(
        lines[grants].starts_with("atrium fleet: \"f\" starting 2 agent(s) at trust automode"),
        "the posture comes right after the {grants} grant line(s): {lines:#?}"
    );
    assert!(
        lines[grants].contains(", ctl on, 4 compile jobs shared"),
        "{}",
        lines[grants]
    );
}

#[test]
fn planning_warnings_lead_the_preflight_block() {
    let vetted = "agent \"lead\": ignoring --x — set the posture with \"trust\" instead";
    let lines = banner_lines(&launch_of(TWO_AGENTS, true, &[vetted]), &facts());
    let at = |needle: &str| {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle:?}: {lines:#?}"))
    };
    // The argv-vetting note found while planning prints before the warnings
    // the banner derives itself (here, a deny rule codex cannot bind).
    assert!(at(vetted) < at("not enforced for cx"), "{lines:#?}");
}

#[test]
fn the_banner_says_who_may_spawn_and_warns_a_ctl_fleet_with_no_spawner() {
    let lines = banner_lines(&launch_of(TWO_AGENTS, true, &[]), &facts());
    assert!(lines.contains(&"atrium fleet: 1 of 2 may spawn teammates (lead)".to_string()));
    assert!(
        !lines.iter().any(|l| l.contains("no agent may spawn")),
        "{lines:#?}"
    );

    let nobody = r#"{"agents":[{"name":"a","cmd":["claude"]}]}"#;
    let lines = banner_lines(&launch_of(nobody, true, &[]), &facts());
    assert!(lines.contains(&"atrium fleet: 0 of 1 may spawn teammates".to_string()));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("ctl is on but no agent may spawn")),
        "{lines:#?}"
    );
}

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
        mod_enabled: None,
        teams: None,
        commands: None,
        teams_env: None,
        title: None,
        notify: None,
        subagents: None,
        subagents_keep: None,
        captions: None,
        respawn_at: None,
        context: None,
        topics: None,
        worktrees: None,
        worktree_base: None,
        worktree_seed: None,
        build_jobs: None,
        memory_mb: None,
        deny: fleet_deny.iter().map(|s| s.to_string()).collect(),
        claude_aliases: Vec::new(),
        codex: None,
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

/// Each distinct effective `codex` block is said once with the codex agents
/// it reaches, an empty one not at all, and a block on an agent that is not
/// codex is named as ignored.
#[test]
fn codex_blocks_are_disclosed_per_distinct_block_and_flagged_off_codex() {
    use atrium::vendors::perms::CodexPerms;
    let block = |text: &str| CodexPerms::parse("t", &json::parse(text).unwrap()).unwrap();
    let agent = |name: &str, cmd: &str, codex: Option<CodexPerms>| atrium::fleet::Agent {
        name: name.to_string(),
        cmd: vec![cmd.to_string()],
        codex,
        ..Default::default()
    };
    let mut fleet = atrium::fleet::Fleet {
        grid: None,
        identity: None,
        trust: None,
        allow_ctl: None,
        mod_enabled: None,
        teams: None,
        commands: None,
        teams_env: None,
        title: None,
        notify: None,
        subagents: None,
        subagents_keep: None,
        captions: None,
        respawn_at: None,
        context: None,
        topics: None,
        worktrees: None,
        worktree_base: None,
        worktree_seed: None,
        build_jobs: None,
        memory_mb: None,
        deny: Vec::new(),
        claude_aliases: Vec::new(),
        codex: Some(block(r#"{ "network": true }"#)),
        agents: vec![
            agent("dev", "codex", None),
            agent("qa", "codex", None),
            agent("lead", "claude", None),
            agent(
                "ops",
                "codex",
                Some(block(r#"{ "git": "read", "approval": "on-request" }"#)),
            ),
        ],
    };
    assert_eq!(
        codex_lines(&fleet),
        vec![
            "codex dev, qa — network on (every host)".to_string(),
            "codex ops — approval on-request, git read-only, network on (every host)".to_string(),
        ]
    );
    assert!(
        codex_unbound(&fleet).is_empty(),
        "the fleet's block is scoped by definition"
    );
    fleet.agents[2].codex = Some(block(r#"{ "network": false }"#));
    assert_eq!(codex_unbound(&fleet), vec!["lead".to_string()]);
    // A block that changes nothing is not disclosed.
    fleet.codex = None;
    fleet.agents[3].codex = Some(block("{}"));
    assert!(codex_lines(&fleet).is_empty());
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

/// The mod's state is part of the posture: a line when it is installed, a
/// warning in the preflight block when claude panes would load none, and
/// nothing at all for a roster with no claude pane.
#[test]
fn the_banner_states_the_mod_and_warns_when_claude_panes_would_load_none() {
    let lines = banner_lines(&launch_of(TWO_AGENTS, true, &[]), &facts());
    assert!(
        lines.contains(&"atrium fleet: mod: installed 0.1.0 at /cfg/mod".to_string()),
        "{lines:#?}"
    );
    let mut missing = facts();
    missing.mod_line = "mod: not installed at /cfg/mod; run `atrium mod install`".to_string();
    missing.mod_missing = true;
    let lines = banner_lines(&launch_of(TWO_AGENTS, true, &[]), &missing);
    assert!(
        lines.iter().any(|l| l.contains("not installed")),
        "{lines:#?}"
    );
    assert!(
        !lines.contains(&format!("atrium fleet: {}", missing.mod_line)),
        "a warning goes in the preflight block, not the posture: {lines:#?}"
    );
    let codex_only = r#"{"agents":[{"name":"cx","cmd":["codex"]}]}"#;
    let lines = banner_lines(&launch_of(codex_only, true, &[]), &missing);
    assert!(
        !lines.iter().any(|l| l.contains("mod:")),
        "no claude pane, nothing to say: {lines:#?}"
    );
}

/// The banner says which path handles a named teammate in this session:
/// the mod, the tmux shim, or neither. It no longer announces teams whenever
/// the shim is on.
#[test]
fn the_banner_says_which_path_handles_a_named_teammate() {
    let teams = |fleet: &str, allow_ctl: bool, facts: &BannerFacts| -> String {
        let lines = banner_lines(&launch_of(fleet, allow_ctl, &[]), facts);
        let found: Vec<&String> = lines.iter().filter(|l| l.contains(" teams: ")).collect();
        assert_eq!(found.len(), 1, "one line, once: {lines:#?}");
        found[0].clone()
    };
    // (a) The mod is on: it takes the Agent tool over and makes the pane.
    assert_eq!(
        teams(TWO_AGENTS, true, &facts()),
        "atrium fleet: teams: named subagents and teammates become atrium panes through the \
         mod (subagents: panes)"
    );
    // (b) The mod is off and the shim is on: Claude Code's own teams, which
    // need a variable atrium cannot see unless it is in its environment.
    let mut shim = facts();
    shim.mod_on = false;
    assert_eq!(
        teams(TWO_AGENTS, true, &shim),
        "atrium fleet: teams: the mod is off; Claude Code's own teammates land as panes \
         through the tmux shim; teams also need CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 in the \
         environment or in Claude Code's settings (atrium cannot see it in its environment)"
    );
    shim.teams_var = true;
    assert_eq!(
        teams(TWO_AGENTS, true, &shim),
        "atrium fleet: teams: the mod is off; Claude Code's own teammates land as panes \
         through the tmux shim"
    );
    // (c) Neither.
    let mut neither = facts();
    neither.mod_on = false;
    neither.teams_on = false;
    assert_eq!(
        teams(TWO_AGENTS, true, &neither),
        "atrium fleet: teams: the mod is off and the tmux shim is off: subagents and \
         teammates run inside their agent and are not visible to atrium"
    );
    // Without a control plane neither path exists, and nothing is said.
    let lines = banner_lines(&launch_of(TWO_AGENTS, false, &[]), &facts());
    assert!(!lines.iter().any(|l| l.contains("teams:")), "{lines:#?}");
    // The fleet's `subagents` policy is the one named.
    let native = TWO_AGENTS.replacen('{', r#"{"subagents":"native","#, 1);
    assert!(teams(&native, true, &facts()).contains("(subagents: native)"));
    let deny = TWO_AGENTS.replacen('{', r#"{"subagents":"deny","#, 1);
    assert!(teams(&deny, true, &facts()).contains("(subagents: deny)"));
    // The old line is gone.
    let all = banner_lines(&launch_of(TWO_AGENTS, true, &[]), &facts()).join("\n");
    assert!(!all.contains("teammateMode"), "{all}");
    // No claude pane: nothing to say about teammates.
    let codex_only = r#"{"agents":[{"name":"cx","cmd":["codex"]}]}"#;
    let lines = banner_lines(&launch_of(codex_only, true, &[]), &facts());
    assert!(!lines.iter().any(|l| l.contains("teams:")), "{lines:#?}");
}

/// The slash commands a fleet lets `ctl command` deliver are in the banner,
/// each one a grant; a fleet that lists none says nothing.
#[test]
fn the_banner_shows_the_fleets_command_allowlist() {
    let fleet = |body: &str| {
        let roster =
            format!(r#"{{"fleets":{{"a":{{{body}"agents":[{{"name":"x","cmd":["claude"]}}]}}}}}}"#);
        atrium::fleet::parse(&roster)
            .unwrap()
            .get("a")
            .unwrap()
            .clone()
    };
    assert_eq!(super::commands_line(&fleet("")), None);
    assert_eq!(super::commands_line(&fleet(r#""commands": [],"#)), None);
    let line =
        super::commands_line(&fleet(r#""commands": ["/compact", "/clear"],"#)).expect("a line");
    assert!(line.starts_with("atrium fleet: "), "{line}");
    assert!(
        line.contains("/compact, /clear") && line.contains("grant"),
        "{line}"
    );
}

/// The banner line is one single-spaced line: a lost line continuation once
/// left a run of spaces in the middle of it.
#[test]
fn the_command_allowlist_line_has_no_run_of_spaces() {
    let roster =
        r#"{"fleets":{"a":{"commands":["/compact"],"agents":[{"name":"x","cmd":["claude"]}]}}}"#;
    let fleet = atrium::fleet::parse(roster)
        .unwrap()
        .get("a")
        .unwrap()
        .clone();
    let line = super::commands_line(&fleet).expect("a line");
    assert!(!line.contains("  "), "{line:?}");
    assert_eq!(
        line,
        "atrium fleet: slash commands a pane may run in another agent's pane (`ctl command`): \
         /compact; each is a grant to every pane that can reach one"
    );
}

/// The variables a fleet lets a Claude Code lead set on its teammates are in
/// the banner when there are any, and not otherwise.
#[test]
fn a_teams_env_list_is_disclosed_in_the_banner_when_it_is_not_empty() {
    let fleet = |body: &str| {
        let roster =
            format!(r#"{{"fleets":{{"a":{{{body}"agents":[{{"name":"x","cmd":["claude"]}}]}}}}}}"#);
        atrium::fleet::parse(&roster)
            .unwrap()
            .get("a")
            .unwrap()
            .clone()
    };
    assert_eq!(super::teams_env_line(&fleet("")), None);
    assert_eq!(super::teams_env_line(&fleet(r#""teams_env": [],"#)), None);
    let line = super::teams_env_line(&fleet(r#""teams_env": ["AWS_PROFILE", "AWS_REGION"],"#))
        .expect("a line");
    assert_eq!(
        line,
        "atrium fleet: variables a Claude Code lead may set on its teammates (`teams_env`): \
         AWS_PROFILE, AWS_REGION; each is taken from a pane's request"
    );
    // A name that moves where settings are read from, or where traffic
    // goes, is listed and warned of.
    let line = super::teams_env_line(&fleet(
        r#""teams_env": ["AWS_PROFILE", "ANTHROPIC_BASE_URL", "HTTPS_PROXY", "CLAUDE_CONFIG_DIR"],"#,
    ))
    .expect("a line");
    assert!(
        line.starts_with(
            "atrium fleet: variables a Claude Code lead may set on its teammates (`teams_env`): \
             AWS_PROFILE, ANTHROPIC_BASE_URL, HTTPS_PROXY, CLAUDE_CONFIG_DIR; each is taken"
        ),
        "{line}"
    );
    assert!(
        line.contains("WARNING: ANTHROPIC_BASE_URL, HTTPS_PROXY, CLAUDE_CONFIG_DIR can move"),
        "{line}"
    );
    assert!(!line.contains("  "), "{line:?}");
}
