//! Parsing `atrium.fleet.json` and building each agent's argv.

use super::*;
use crate::fleet::testutil::s;

#[test]
fn parses_a_full_fleet_with_every_field() {
    let text = r#"{
          "fleets": {
            "review-crew": {
              "grid": "2x2",
              "identity": "work",
              "agents": [
                {
                  "name": "reviewer",
                  "cmd": ["claude", "--continue"],
                  "identity": "wif:prod",
                  "cwd": "./review",
                  "add_dirs": ["../shared", "./specs"],
                  "prompt": "You review PRs for safety.",
                  "model": "opus",
                  "effort": "high"
                }
              ]
            }
          }
        }"#;
    let fleets = parse(text).unwrap();
    assert_eq!(fleets.names(), vec!["review-crew"]);
    let f = fleets.get("review-crew").unwrap();
    assert_eq!(f.grid.as_deref(), Some("2x2"));
    assert_eq!(f.identity.as_deref(), Some("work"));
    assert_eq!(f.agents.len(), 1);
    let a = &f.agents[0];
    assert_eq!(a.name, "reviewer");
    assert_eq!(a.cmd, s(&["claude", "--continue"]));
    assert_eq!(a.identity.as_deref(), Some("wif:prod"));
    assert_eq!(a.cwd.as_deref(), Some("./review"));
    assert_eq!(a.add_dirs, s(&["../shared", "./specs"]));
    assert_eq!(a.prompt.as_deref(), Some("You review PRs for safety."));
    assert_eq!(a.model.as_deref(), Some("opus"));
    assert_eq!(a.effort.as_deref(), Some("high"));
}

#[test]
fn parses_a_minimal_agent_with_only_name_and_cmd() {
    let text = r#"{ "fleets": { "solo": { "agents": [
          { "name": "worker", "cmd": ["claude"] }
        ] } } }"#;
    let f = parse(text).unwrap();
    let fleet = f.get("solo").unwrap();
    assert_eq!(fleet.grid, None);
    assert_eq!(fleet.identity, None);
    let a = &fleet.agents[0];
    assert_eq!(a.cmd, s(&["claude"]));
    assert_eq!(a.identity, None);
    assert_eq!(a.cwd, None);
    assert!(a.add_dirs.is_empty());
    assert_eq!(a.prompt, None);
    assert_eq!(a.model, None);
    assert_eq!(a.effort, None);
}

#[test]
fn kickoff_parses_and_is_the_trailing_positional_prompt() {
    let text = r#"{ "fleets": { "f": { "agents": [
          { "name": "a", "cmd": ["claude"], "prompt": "P", "kickoff": "go now" }
        ] } } }"#;
    let f = parse(text).unwrap();
    let a = &f.get("f").unwrap().agents[0];
    assert_eq!(a.kickoff.as_deref(), Some("go now"));
    // The kickoff is LAST — after every flag — so claude reads it as the first
    // user message and starts working immediately.
    let argv = a.args(&["/ctx".to_string()]);
    assert_eq!(
        argv,
        s(&[
            "claude",
            "--add-dir",
            "/ctx",
            "--append-system-prompt",
            "P",
            "go now"
        ])
    );
}

#[test]
fn unknown_fields_are_ignored_for_forward_compat() {
    let text = r#"{ "future": 1, "fleets": {
          "f": { "when": "later", "agents": [
            { "name": "a", "cmd": ["claude"], "tools": ["x"] }
          ] }
        } }"#;
    let f = parse(text).unwrap();
    assert_eq!(f.get("f").unwrap().agents[0].cmd, s(&["claude"]));
}

#[test]
fn build_jobs_sizes_the_compile_pool() {
    let text = |v: &str| {
        format!(
            r#"{{ "fleets": {{ "f": {{ "build_jobs": {v}, "agents": [{{ "name": "a", "cmd": ["claude"] }}] }} }} }}"#
        )
    };
    assert_eq!(
        parse(&text("6")).unwrap().get("f").unwrap().build_jobs,
        Some(6)
    );
    // 0 is how a fleet turns the pool off.
    assert_eq!(
        parse(&text("0")).unwrap().get("f").unwrap().build_jobs,
        Some(0)
    );
    for bad in ["-1", "2.5", "\"4\"", "true"] {
        let err = parse(&text(bad)).unwrap_err();
        assert!(err.contains("build_jobs"), "{bad}: {err}");
    }
    let absent = r#"{ "fleets": { "f": { "agents": [{ "name": "a", "cmd": ["claude"] }] } } }"#;
    assert_eq!(parse(absent).unwrap().get("f").unwrap().build_jobs, None);
}

#[test]
fn deny_is_parsed_at_fleet_and_agent_level() {
    let text = r#"{ "fleets": { "f": {
            "deny": ["cargo test --workspace", "Bash(git push --force*)"],
            "agents": [
              { "name": "a", "cmd": ["claude"], "deny": ["npm publish"] },
              { "name": "b", "cmd": ["claude"] }
            ] } } }"#;
    let fleets = parse(text).unwrap();
    let f = fleets.get("f").unwrap();
    assert_eq!(
        f.deny,
        s(&["cargo test --workspace", "Bash(git push --force*)"])
    );
    assert_eq!(f.agents[0].deny, s(&["npm publish"]));
    assert!(f.agents[1].deny.is_empty());
    for bad in [r#""deny": "x""#, r#""deny": [1]"#] {
        let t = format!(
            r#"{{ "fleets": {{ "f": {{ {bad}, "agents": [{{ "name": "a", "cmd": ["claude"] }}] }} }} }}"#
        );
        let err = parse(&t).unwrap_err();
        assert!(err.contains("deny"), "{bad}: {err}");
    }
}

#[test]
fn memory_mb_sets_a_fixed_ceiling() {
    let text = |v: &str| {
        format!(
            r#"{{ "fleets": {{ "f": {{ "memory_mb": {v}, "agents": [{{ "name": "a", "cmd": ["claude"] }}] }} }} }}"#
        )
    };
    assert_eq!(
        parse(&text("32768")).unwrap().get("f").unwrap().memory_mb,
        Some(32768)
    );
    assert_eq!(
        parse(&text("0")).unwrap().get("f").unwrap().memory_mb,
        Some(0)
    );
    for bad in ["-5", "1.5", "\"8G\"", "null"] {
        let err = parse(&text(bad)).unwrap_err();
        assert!(err.contains("memory_mb"), "{bad}: {err}");
    }
}

#[test]
fn preserves_fleet_order() {
    let text = r#"{ "fleets": {
          "b": { "agents": [{ "name": "x", "cmd": ["sh"] }] },
          "a": { "agents": [{ "name": "y", "cmd": ["sh"] }] }
        } }"#;
    assert_eq!(parse(text).unwrap().names(), vec!["b", "a"]);
}

#[test]
fn malformed_json_is_a_clear_error() {
    let err = parse("{ not json").unwrap_err();
    assert!(err.contains("malformed JSON"), "{err}");
}

#[test]
fn missing_fleets_object_is_rejected() {
    let err = parse(r#"{ "other": {} }"#).unwrap_err();
    assert!(err.contains("no \"fleets\""), "{err}");
}

#[test]
fn non_object_top_level_is_rejected() {
    assert!(parse("[]").unwrap_err().contains("must be a JSON object"));
}

#[test]
fn empty_agents_is_rejected() {
    let err = parse(r#"{ "fleets": { "f": { "agents": [] } } }"#).unwrap_err();
    assert!(err.contains("zero agents"), "{err}");
}

#[test]
fn missing_agents_is_rejected() {
    let err = parse(r#"{ "fleets": { "f": {} } }"#).unwrap_err();
    assert!(err.contains("no \"agents\""), "{err}");
}

#[test]
fn agent_without_cmd_is_rejected() {
    let err = parse(r#"{ "fleets": { "f": { "agents": [ { "name": "a" } ] } } }"#).unwrap_err();
    assert!(err.contains("missing \"cmd\""), "{err}");
}

#[test]
fn agent_without_name_is_rejected() {
    let err = parse(r#"{ "fleets": { "f": { "agents": [ { "cmd": ["sh"] } ] } } }"#).unwrap_err();
    assert!(err.contains("missing string \"name\""), "{err}");
}

#[test]
fn empty_cmd_array_is_rejected() {
    let err = parse(r#"{ "fleets": { "f": { "agents": [ { "name": "a", "cmd": [] } ] } } }"#)
        .unwrap_err();
    assert!(err.contains("\"cmd\" is empty"), "{err}");
}

#[test]
fn unknown_fleet_name_is_none() {
    let f = parse(r#"{ "fleets": { "a": { "agents": [{"name":"x","cmd":["sh"]}] } } }"#).unwrap();
    assert!(f.get("nope").is_none());
}

// --- the pure arg-builder ------------------------------------------------

#[test]
fn args_of_a_bare_agent_is_just_its_cmd() {
    let a = Agent {
        name: "x".into(),
        cmd: s(&["claude"]),
        ..Agent::default()
    };
    assert_eq!(a.args(&[]), s(&["claude"]));
}

#[test]
fn args_appends_every_set_field_in_order() {
    let a = Agent {
        name: "x".into(),
        cmd: s(&["claude", "--continue"]),
        prompt: Some("be careful".into()),
        model: Some("opus".into()),
        effort: Some("high".into()),
        ..Agent::default()
    };
    let dirs = s(&["/abs/shared", "/abs/specs"]);
    assert_eq!(
        a.args(&dirs),
        s(&[
            "claude",
            "--continue",
            "--add-dir",
            "/abs/shared",
            "/abs/specs",
            "--append-system-prompt",
            "be careful",
            "--model",
            "opus",
            "--effort",
            "high",
        ])
    );
}

#[test]
fn args_omits_absent_fields() {
    let a = Agent {
        name: "x".into(),
        cmd: s(&["claude"]),
        model: Some("sonnet".into()),
        ..Agent::default()
    };
    // Only --model is present; no --add-dir / --append-system-prompt / --effort.
    assert_eq!(a.args(&[]), s(&["claude", "--model", "sonnet"]));
}

#[test]
fn args_uses_the_resolved_add_dirs_not_the_fields_own() {
    // The builder takes resolved dirs as an argument; a.add_dirs is not read
    // directly, so the caller's resolution against the fleet dir is honored.
    let a = Agent {
        name: "x".into(),
        cmd: s(&["claude"]),
        add_dirs: s(&["./relative"]),
        ..Agent::default()
    };
    assert_eq!(
        a.args(&s(&["/base/relative"])),
        s(&["claude", "--add-dir", "/base/relative"])
    );
}

// --- path resolution -----------------------------------------------------

#[test]
fn spawning_is_a_declared_capability_defaulting_to_off() {
    // Creating teammates used to be implied by having ctl access at all, so
    // every agent in a fleet could do it. In a seven-agent review fleet that
    // meant all seven, when only the lead should.
    let f = parse(
        r#"{ "fleets": { "a": { "agents": [
                 { "name": "lead", "cmd": ["claude"], "can_spawn": true },
                 { "name": "worker", "cmd": ["claude"] }
               ] } } }"#,
    )
    .unwrap();
    let a = f.get("a").unwrap();
    assert_eq!(a.agents[0].can_spawn, Some(true), "the lead declared it");
    assert_eq!(
        a.agents[1].can_spawn, None,
        "a worker that says nothing must not silently get it"
    );
}

#[test]
fn a_fleet_can_declare_the_control_plane() {
    // A coordinating fleet without ctl fails SILENTLY: panes come up, the
    // kickoffs tell them to publish to a bus that does not exist, and nothing
    // happens. Declaring it in the file removes a flag that has to be
    // remembered every launch.
    let f = parse(
        r#"{ "fleets": { "a": { "allow_ctl": true,
                 "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
    )
    .unwrap();
    assert_eq!(f.get("a").unwrap().allow_ctl, Some(true));
}

#[test]
fn a_fleet_can_declare_a_canonical_topic_vocabulary() {
    let f = parse(
        r#"{ "fleets": { "a": { "topics": ["build", "review"],
                 "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
    )
    .unwrap();
    assert_eq!(
        f.get("a").unwrap().topics,
        Some(vec!["build".to_string(), "review".to_string()]),
        "declared topics parse in order; the bus normalizes them"
    );
}

#[test]
fn a_non_array_topics_is_a_clear_error() {
    let err = parse(
        r#"{ "fleets": { "a": { "topics": "build",
                 "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
    )
    .unwrap_err();
    assert!(err.contains("topics"), "unhelpful error: {err}");
}

#[test]
fn absent_topics_leaves_the_fleet_soft_gated() {
    let f = parse(r#"{ "fleets": { "a": { "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#)
        .unwrap();
    assert_eq!(f.get("a").unwrap().topics, None);
}

// --- worktree config -----------------------------------------------------

#[test]
fn an_agent_can_name_its_worktree_group() {
    let f = parse(
        r#"{ "fleets": { "a": { "agents": [
                 { "name": "solo",  "cmd": ["claude"], "worktree": "solo" },
                 { "name": "lead",  "cmd": ["claude"] },
                 { "name": "help",  "cmd": ["claude"], "worktree": "squad" },
                 { "name": "help2", "cmd": ["claude"], "worktree": "squad" }
               ] } } }"#,
    )
    .unwrap();
    let ag = &f.get("a").unwrap().agents;
    assert_eq!(ag[0].worktree.as_deref(), Some("solo"));
    assert_eq!(ag[1].worktree, None, "absent key = main tree (legacy)");
    assert_eq!(ag[2].worktree.as_deref(), Some("squad"));
    assert_eq!(
        ag[3].worktree.as_deref(),
        Some("squad"),
        "shared group name"
    );
}

#[test]
fn a_fleet_can_set_the_worktrees_shorthand_base_and_seed() {
    let f = parse(
        r#"{ "fleets": { "a": {
                 "worktrees": true,
                 "worktree_base": "../wt-session-2",
                 "worktree_seed": [".env", "config.local.json"],
                 "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
    )
    .unwrap();
    let fleet = f.get("a").unwrap();
    assert_eq!(fleet.worktrees, Some(true));
    assert_eq!(fleet.worktree_base.as_deref(), Some("../wt-session-2"));
    assert_eq!(
        fleet.worktree_seed,
        Some(vec![".env".to_string(), "config.local.json".to_string()])
    );
}

#[test]
fn absent_worktree_config_leaves_the_fleet_on_the_main_tree() {
    let f = parse(r#"{ "fleets": { "a": { "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#)
        .unwrap();
    let fleet = f.get("a").unwrap();
    assert_eq!(fleet.worktrees, None);
    assert_eq!(fleet.worktree_base, None);
    assert_eq!(fleet.worktree_seed, None);
    assert_eq!(fleet.agents[0].worktree, None);
}

#[test]
fn a_non_bool_worktrees_is_a_clear_error() {
    let err = parse(
        r#"{ "fleets": { "a": { "worktrees": "yes",
                 "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
    )
    .unwrap_err();
    assert!(err.contains("worktrees"), "unhelpful error: {err}");
}

#[test]
fn a_non_array_worktree_seed_is_a_clear_error() {
    let err = parse(
        r#"{ "fleets": { "a": { "worktree_seed": ".env",
                 "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
    )
    .unwrap_err();
    assert!(err.contains("worktree_seed"), "unhelpful error: {err}");
}

#[test]
fn an_agent_can_override_its_trust_posture() {
    // The mixed-model case: automode session, but the haiku agent asks for
    // accept (which carries an allowlist and is not model-gated).
    let f = parse(
        r#"{ "fleets": { "a": { "agents": [
                 { "name": "sonnet", "cmd": ["claude", "--model", "sonnet"] },
                 { "name": "haiku",  "cmd": ["claude", "--model", "haiku"], "trust": "accept" }
               ] } } }"#,
    )
    .unwrap();
    let ag = &f.get("a").unwrap().agents;
    assert_eq!(
        ag[0].trust, None,
        "no override → inherit the session posture"
    );
    assert_eq!(ag[1].trust.as_deref(), Some("accept"));
}

#[test]
fn a_non_bool_allow_ctl_is_a_clear_error() {
    let err = parse(
        r#"{ "fleets": { "a": { "allow_ctl": "yes", "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
    )
    .unwrap_err();
    assert!(err.contains("allow_ctl"), "unhelpful error: {err}");
}

#[test]
fn a_fleet_can_declare_its_own_trust_posture() {
    // A fleet is launched to run hands-off, so it needs a posture — and a CLI
    // flag typed on every launch is one you eventually get wrong. Declared in
    // the file it lives with the roster and shows up in a diff.
    let f = parse(
        r#"{ "fleets": { "a": { "trust": "accept",
                 "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
    )
    .unwrap();
    assert_eq!(f.get("a").unwrap().trust.as_deref(), Some("accept"));
}

#[test]
fn a_fleet_without_a_trust_key_declares_nothing() {
    let f = parse(r#"{ "fleets": { "a": { "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#)
        .unwrap();
    assert_eq!(f.get("a").unwrap().trust, None);
}

#[test]
fn a_non_string_trust_is_a_clear_error() {
    let err = parse(
        r#"{ "fleets": { "a": { "trust": 3, "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
    )
    .unwrap_err();
    assert!(err.contains("trust"), "unhelpful error: {err}");
}

#[test]
fn context_block_absent_is_none() {
    let f = parse(r#"{ "fleets": { "f": { "agents": [{ "name": "a", "cmd": ["claude"] }] } } }"#)
        .unwrap();
    assert_eq!(f.get("f").unwrap().context, None);
}

#[test]
fn context_block_with_known_provider_and_share() {
    let f = parse(
        r#"{ "fleets": { "f": {
              "context": { "provider": "context-mode", "share": "full" },
              "agents": [{ "name": "a", "cmd": ["claude"] }]
            } } }"#,
    )
    .unwrap();
    let ctx = f.get("f").unwrap().context.as_ref().unwrap();
    assert_eq!(ctx.provider, crate::context::Provider::ContextMode);
    assert_eq!(ctx.share, crate::context::Share::Full);
}

#[test]
fn context_block_unknown_provider_degrades_not_errors() {
    // An unknown provider must warn to stderr and degrade — it must never
    // reject the fleet file, since the file is otherwise valid.
    let f = parse(
        r#"{ "fleets": { "f": {
              "context": { "provider": "not-a-real-provider" },
              "agents": [{ "name": "a", "cmd": ["claude"] }]
            } } }"#,
    )
    .unwrap();
    let ctx = f.get("f").unwrap().context.as_ref().unwrap();
    assert_eq!(ctx.provider, crate::context::Provider::None);
    assert_eq!(ctx.share, crate::context::Share::Knowledge); // default
}

#[test]
fn context_block_unknown_share_degrades_to_knowledge() {
    let f = parse(
        r#"{ "fleets": { "f": {
              "context": { "provider": "context-mode", "share": "not-a-share" },
              "agents": [{ "name": "a", "cmd": ["claude"] }]
            } } }"#,
    )
    .unwrap();
    let ctx = f.get("f").unwrap().context.as_ref().unwrap();
    assert_eq!(ctx.share, crate::context::Share::Knowledge);
}
