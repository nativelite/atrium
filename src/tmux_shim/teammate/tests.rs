//! The teammate command's policy: what is passed on, consumed, dropped and
//! refused.

use super::*;
use tmuxproto::parse_teammate_command;

// ---- fixtures ---------------------------------------------------------------
//
// The command strings of Claude Code 2.1.325, written out from the code
// embedded in the installed binary (read 2026-11-15; the same strings are
// the parser's fixtures in nativelite-tmuxproto). They were not captured
// from a live lead. The template is
//
//   cd <Q cwd> && env <ENV> <Q program> <identity flags><inherited flags>
//
// where Q leaves a word bare when it is all `A-Z a-z 0-9 _ . / : = @ + , -`
// and otherwise wraps it in single quotes, and a flag is `--flag value`, or
// `--flag=value` when the value starts with `-` or reads as a URL.

/// No task list, no verify key, no inherited flags.
const FIXTURE_MIN: &str = "cd /home/dev/demo && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 ANTHROPIC_PROFILE='' ANTHROPIC_CONFIG_DIR='' CLAUDE_CODE_ATTRIBUTION_PROJECT='' CLAUDE_CODE_USER_DELEGATED_SESSION_DIRECT_API='' CLAUDE_CODE_USER_DELEGATED_SESSION='' CLAUDE_CODE_USER_DELEGATED_SESSION_ANY_CHECK_MODEL='' CLAUDE_CODE_USER_DELEGATED_SESSION_ALLOWED_CHECK_MODELS='' CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS= /home/dev/.local/bin/claude --agent-id researcher@demo --agent-name researcher --team-name demo --agent-color blue --parent-session-id 0b9f6c1e-2a67-4c58-9d0e-6f4f8a1d2c33";

/// Every identity flag, and the inherited flags of a plan-required teammate
/// with deny rules, a tool list, a settings file, a plugin directory and an
/// MCP config; forwarded provider, proxy and config variables.
const FIXTURE_FULL: &str = "cd '/home/dev/my project' && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 CLAUDE_CODE_TASK_LIST_ID=demo CLAUDE_CODE_USE_BEDROCK=1 AWS_REGION=us-east-1 AWS_PROFILE='team profile' ANTHROPIC_BASE_URL=https://proxy.example:8443/v1 CLAUDE_CONFIG_DIR=/home/dev/.claude-work HTTPS_PROXY=http://127.0.0.1:3128 NO_PROXY='localhost,*.internal' DISABLE_TELEMETRY='' ANTHROPIC_PROFILE='it'\"'\"'s' ANTHROPIC_CONFIG_DIR='' CLAUDE_CODE_ATTRIBUTION_PROJECT='' CLAUDE_CODE_USER_DELEGATED_SESSION_DIRECT_API='' CLAUDE_CODE_USER_DELEGATED_SESSION='' CLAUDE_CODE_USER_DELEGATED_SESSION_ANY_CHECK_MODEL='' CLAUDE_CODE_USER_DELEGATED_SESSION_ALLOWED_CHECK_MODELS='' CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS= /home/dev/.local/bin/claude --agent-id reviewer@demo --agent-name reviewer --team-name demo --agent-color colour208 --parent-session-id 0b9f6c1e-2a67-4c58-9d0e-6f4f8a1d2c33 --lead-verify-key 1763200000000.MCowBQYDK2VwAyEAq3x1Zb0m2m1Yw6i8cQ9fYt5n0u7JH2k4L-_dV3sT1aE --plan-mode-required --agent-type code-reviewer --permission-mode plan --proactivity ask --disallowed-tools 'Bash(rm -rf /*)' --disallowed-tools 'Read(./.env)' --tools Read --tools Grep --effort high --settings /home/dev/.local/state/atrium/hooks/settings.json --setting-sources=user,project --plugin-dir /home/dev/.local/share/atrium/mod --mcp-config '{\"mcpServers\":{}}' --strict-mcp-config --no-chrome --model claude-opus-4-5";

/// Linux with the tool-cgroup feature, from a lead that skips permissions.
const FIXTURE_CGROUP: &str = "sh -c '{ echo $PPID > \"$1\"/cgroup.procs; } 2>/dev/null' sh /sys/fs/cgroup/user.slice/claude-agent; cd /home/dev/demo && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS= /home/dev/.local/bin/claude --agent-id researcher@demo --agent-name researcher --team-name demo --agent-color blue --parent-session-id 0b9f6c1e-2a67-4c58-9d0e-6f4f8a1d2c33 --dangerously-skip-permissions";

/// Windows: both paths in single quotes.
const FIXTURE_WINDOWS: &str = "cd 'D:\\projects\\demo' && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 CLAUDE_CODE_TASK_LIST_ID=demo ANTHROPIC_PROFILE='' ANTHROPIC_CONFIG_DIR='' CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS= 'C:\\Users\\dev\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe' --agent-id researcher@demo --agent-name researcher --team-name demo --agent-color blue --parent-session-id 0b9f6c1e-2a67-4c58-9d0e-6f4f8a1d2c33 --lead-verify-key 1763200000000.MCowBQYDK2VwAyEAq3x1Zb0m2m1Yw6i8cQ9fYt5n0u7JH2k4L-_dV3sT1aE --permission-mode acceptEdits --model claude-opus-4-5";

const KEY: &str = "1763200000000.MCowBQYDK2VwAyEAq3x1Zb0m2m1Yw6i8cQ9fYt5n0u7JH2k4L-_dV3sT1aE";

fn vet(line: &str) -> Result<Teammate, String> {
    vet_teammate(&parse_teammate_command(line).expect("the line parses"), &[])
}

fn v(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// A command with the three flags every teammate carries, then `rest`.
fn with(rest: &str) -> String {
    format!("cd /d && env CLAUDECODE=1 /usr/bin/claude --agent-id a@t --agent-name a --team-name t {rest}")
}

#[test]
fn the_minimal_fixture_is_a_teammate_with_three_variables() {
    let t = vet(FIXTURE_MIN).unwrap();
    assert_eq!(t.role, "researcher");
    assert_eq!(
        t.args,
        v(&[
            "--agent-id",
            "researcher@demo",
            "--agent-name",
            "researcher",
            "--team-name",
            "demo",
            "--agent-color",
            "blue",
            "--parent-session-id",
            "0b9f6c1e-2a67-4c58-9d0e-6f4f8a1d2c33",
        ])
    );
    assert_eq!(t.asked, Asked::LeadsOwn, "no permission flag asks nothing");
    assert!(t.deny.is_empty() && t.dropped.is_empty());
    assert_eq!(
        t.env,
        [
            ("CLAUDECODE".to_string(), "1".to_string()),
            (
                "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                "1".to_string()
            ),
        ]
    );
    assert_eq!(t.env_dropped, 8);
}

#[test]
fn the_full_fixture_is_passed_consumed_and_dropped_flag_by_flag() {
    let t = vet(FIXTURE_FULL).unwrap();
    assert_eq!(t.role, "reviewer");
    assert_eq!(
        t.args,
        v(&[
            "--agent-id",
            "reviewer@demo",
            "--agent-name",
            "reviewer",
            "--team-name",
            "demo",
            "--agent-color",
            "colour208",
            "--parent-session-id",
            "0b9f6c1e-2a67-4c58-9d0e-6f4f8a1d2c33",
            "--lead-verify-key",
            KEY,
            "--plan-mode-required",
            "--agent-type",
            "code-reviewer",
            "--tools",
            "Read",
            "--tools",
            "Grep",
            "--effort",
            "high",
            "--strict-mcp-config",
            "--no-chrome",
            "--model",
            "claude-opus-4-5",
        ])
    );
    // `--plan-mode-required` is plan, whatever `--permission-mode` said.
    assert_eq!(t.asked, Asked::Mode(TrustMode::Plan));
    assert_eq!(t.deny, v(&["Bash(rm -rf /*)", "Read(./.env)"]));
    assert_eq!(
        t.dropped,
        [
            "--proactivity",
            "--settings",
            "--setting-sources",
            "--plugin-dir",
            "--mcp-config"
        ]
    );
    assert_eq!(
        t.env,
        [
            ("CLAUDECODE".to_string(), "1".to_string()),
            (
                "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                "1".to_string()
            ),
            ("CLAUDE_CODE_TASK_LIST_ID".to_string(), "demo".to_string()),
        ]
    );
    // Sixteen others: the provider, the proxy, CLAUDE_CONFIG_DIR, the
    // skip-permissions variable. None is applied.
    assert_eq!(t.env_dropped, 16);
    for (name, _) in &t.env {
        assert!(
            [ENV_CLAUDECODE, ENV_AGENT_TEAMS, ENV_TASK_LIST].contains(&name.as_str()),
            "{name}"
        );
    }
    // Nothing atrium passes on is a path, a URL or JSON from the string.
    for a in &t.args {
        assert!(!a.contains("/home") && !a.contains("mcpServers"), "{a}");
    }
}

#[test]
fn the_cgroup_and_windows_fixtures_vet_like_the_others() {
    let t = vet(FIXTURE_CGROUP).unwrap();
    assert_eq!(t.asked, Asked::Mode(TrustMode::Skip));
    assert!(
        !t.args.iter().any(|a| a.contains("permission")),
        "the skip switch is consumed: {:?}",
        t.args
    );
    assert_eq!(t.env_dropped, 1);
    let t = vet(FIXTURE_WINDOWS).unwrap();
    assert_eq!(t.role, "researcher");
    assert_eq!(t.asked, Asked::Mode(TrustMode::Edits));
    assert_eq!(t.args.last().map(String::as_str), Some("claude-opus-4-5"));
    assert_eq!(t.env.len(), 3);
    assert_eq!(t.env_dropped, 3);
}

/// A sample value that fits each shape.
fn sample(shape: Shape) -> &'static str {
    match shape {
        Shape::Name => "code reviewer-2",
        Shape::AgentId => "code reviewer-2@my team",
        Shape::Token => "0b9f6c1e-2a67-4c58-9d0e-6f4f8a1d2c33",
        Shape::Color => "colour205",
        Shape::Key => KEY,
        Shape::Model => "us.anthropic.claude-opus-4-5-v1:0[1m]",
        Shape::Word => "xhigh",
        Shape::Tool => "mcp__server__tool",
    }
}

/// Every row of the table, in both spellings: accepted, and treated as the
/// row says.
#[test]
fn every_flag_in_the_table_is_accepted_as_its_row_says() {
    let quote = |s: &str| format!("'{s}'");
    for flag in FLAGS {
        // The three every command carries are in the base; test them there.
        let base_has = ["--agent-id", "--agent-name", "--team-name"].contains(&flag.name);
        let (value, shape): (Option<&str>, Option<Shape>) = match flag.take {
            Take::Pass(s) | Take::PassEach(s) => (Some(sample(s)), Some(s)),
            Take::Role => (Some("dev one"), Some(Shape::Name)),
            Take::Mode => (Some("acceptEdits"), None),
            Take::Deny => (Some("Bash(curl:*)"), None),
            Take::DropValue => (Some("/any/path or {json}"), None),
            Take::Switch | Take::PlanRequired | Take::Skip | Take::DropSwitch => (None, None),
        };
        for inline in [false, true] {
            if value.is_none() && inline {
                continue;
            }
            let written = match (value, inline) {
                (Some(val), false) => format!("{} {}", flag.name, quote(val)),
                (Some(val), true) => quote(&format!("{}={val}", flag.name)),
                (None, _) => flag.name.to_string(),
            };
            let line = if base_has {
                let mut parts = vec![
                    ("--agent-id", "a@t".to_string()),
                    ("--agent-name", "a".to_string()),
                    ("--team-name", "t".to_string()),
                ];
                parts.retain(|(n, _)| *n != flag.name);
                let rest: Vec<String> = parts.iter().map(|(n, val)| format!("{n} {val}")).collect();
                format!("cd /d && env /usr/bin/claude {} {written}", rest.join(" "))
            } else {
                with(&written)
            };
            let t = vet(&line).unwrap_or_else(|e| panic!("{}: {e}", flag.name));
            let passed = t
                .args
                .iter()
                .any(|a| a == flag.name || a.starts_with(&format!("{}=", flag.name)));
            match flag.take {
                Take::Pass(_) | Take::PassEach(_) | Take::Role => {
                    assert!(passed, "{} is passed on", flag.name);
                    let val = value.unwrap();
                    assert!(shape.unwrap().fits(val));
                    if inline {
                        assert!(t.args.contains(&format!("{}={val}", flag.name)));
                    } else {
                        assert!(t.args.windows(2).any(|w| w[0] == flag.name && w[1] == val));
                    }
                }
                Take::Switch | Take::PlanRequired => assert!(passed, "{}", flag.name),
                Take::Mode | Take::Skip | Take::Deny => {
                    assert!(!passed, "{} is consumed", flag.name);
                    assert!(!t.dropped.contains(&flag.name));
                }
                Take::DropValue | Take::DropSwitch => {
                    assert!(!passed, "{} is dropped", flag.name);
                    assert_eq!(t.dropped, [flag.name]);
                    // Its value went with it.
                    assert!(!t.args.iter().any(|a| a.contains("json")), "{:?}", t.args);
                }
            }
            if flag.take == Take::Role {
                assert_eq!(t.role, "dev one");
            }
            if flag.take == Take::Deny {
                assert_eq!(t.deny, ["Bash(curl:*)"]);
            }
        }
    }
}

/// A second copy of a flag atrium writes itself would compete with atrium's:
/// no row passes one on.
#[test]
fn nothing_passed_on_is_a_flag_atrium_sets_itself() {
    for flag in FLAGS {
        let passes = matches!(
            flag.take,
            Take::Pass(_) | Take::PassEach(_) | Take::Switch | Take::Role | Take::PlanRequired
        );
        if passes {
            assert!(
                !ATRIUM_OWN_FLAGS.contains(&flag.name),
                "{} is atrium's own",
                flag.name
            );
        }
    }
    // And every permission-shaping flag of the table is consumed.
    for name in [
        "--permission-mode",
        "--dangerously-skip-permissions",
        "--proactivity",
        "--disallowed-tools",
        "--disallowedTools",
        "--settings",
        "--setting-sources",
        "--managed-settings",
        "--plugin-dir",
        "--plugin-dir-no-mcp",
        "--plugin-url",
        "--mcp-config",
        "--project-config-root",
        "--chrome",
    ] {
        let flag = FLAGS.iter().find(|fl| fl.name == name).expect(name);
        assert!(
            matches!(
                flag.take,
                Take::Mode | Take::Skip | Take::Deny | Take::DropValue | Take::DropSwitch
            ),
            "{name}"
        );
    }
}

#[test]
fn a_flag_outside_the_table_refuses_the_launch_and_is_named() {
    for (rest, named) in [
        ("--allowedTools 'Bash(*)'", "`--allowedTools`"),
        ("--allowed-tools=Bash", "`--allowed-tools`"),
        (
            "--allow-dangerously-skip-permissions",
            "`--allow-dangerously-skip-permissions`",
        ),
        ("--append-system-prompt 'do it'", "`--append-system-prompt`"),
        ("--session-id 0b9f6c1e", "`--session-id`"),
        ("--resume", "`--resume`"),
        ("--add-dir /", "`--add-dir`"),
        ("--teammate-mode tmux", "`--teammate-mode`"),
        ("--some-flag-of-2-2-0 x", "`--some-flag-of-2-2-0`"),
        ("-p", "`-p`"),
        ("--", "a flag whose name is not plain letters"),
        (
            "'--evil\u{202e}flag'",
            "a flag whose name is not plain letters",
        ),
        ("'--has space'", "a flag whose name is not plain letters"),
    ] {
        let e = vet(&with(rest)).unwrap_err();
        assert!(e.starts_with("teammate refused:"), "{e}");
        assert!(e.contains(named), "{rest}: {e}");
        assert!(
            e.contains("newer"),
            "the refusal says why it may happen: {e}"
        );
        assert!(e.contains("2.1.325"));
        assert!(!e.contains('\u{202e}'));
    }
}

#[test]
fn a_positional_or_a_prefix_argument_refuses_the_launch() {
    for line in [
        with("'do this task'"),
        with("--model opus 'then a prompt'"),
        // A launcher or `node cli.js`: an argument before the flags.
        "cd /d && env /usr/bin/claude /opt/cli.js --agent-id a@t --agent-name a --team-name t"
            .to_string(),
        // A switch followed by a word.
        with("--strict-mcp-config yes"),
    ] {
        let e = vet(&line).unwrap_err();
        assert!(e.contains("not a flag"), "{line}: {e}");
    }
}

#[test]
fn a_program_that_is_not_claude_refuses_the_launch() {
    for program in [
        "node",
        "/usr/bin/node",
        "sh",
        "/bin/sh",
        "cat",
        "codex",
        "'C:\\Windows\\System32\\cmd.exe'",
        "/tmp/claude-wrapper",
    ] {
        let line = format!("cd /d && env {program} --agent-id a@t --agent-name a --team-name t");
        let e = vet(&line).unwrap_err();
        assert!(e.contains("not claude"), "{program}: {e}");
        assert!(e.contains("CLAUDE_CODE_TEAMMATE_COMMAND"));
    }
    // By its name alone: any path to a claude passes here, and none is run
    // (the server starts the lead's own).
    for program in [
        "claude",
        "/nonexistent/claude",
        "'C:\\x\\claude.exe'",
        "'\\\\host\\share\\claude.cmd'",
    ] {
        let line = format!("cd /d && env {program} --agent-id a@t --agent-name a --team-name t");
        assert!(vet(&line).is_ok(), "{program}");
    }
}

#[test]
fn a_value_outside_its_shape_refuses_and_is_never_echoed() {
    let long = "a".repeat(300);
    for (flag, value) in [
        ("--agent-color", "'red; id'"),
        (
            "--agent-color",
            "'\u{43a}\u{440}\u{430}\u{441}\u{43d}\u{44b}\u{439}'",
        ),
        ("--agent-type", "'../../x'"),
        ("--agent-type", "'two  spaces'"),
        ("--agent-type", "'trailing '"),
        ("--agent-type", long.as_str()),
        ("--parent-session-id", "'id with space'"),
        ("--lead-verify-key", "'1.k/../x'"),
        ("--lead-verify-key", "'1.k k'"),
        ("--model", "'opus --dangerously-skip-permissions'"),
        ("--model", "'$(id)'"),
        ("--effort", "'high,low'"),
        ("--tools", "'Bash(*)'"),
        ("--permission-mode", "bubble"),
        ("--permission-mode", "'acceptEdits '"),
        ("--permission-mode", "''"),
        ("--disallowed-tools", "''"),
        // A rule atrium's deny list would rewrite is not carried intact.
        ("--disallowed-tools", "mcp__server__tool"),
        ("--disallowed-tools", "'git push'"),
        ("--disallowed-tools", "' Bash(x)'"),
        ("--disallowed-tools", "'Bash(\u{e9})'"),
    ] {
        let line = with(&format!("{flag} {value}"));
        let e = vet(&line).unwrap_err();
        assert!(
            e.contains(&format!("the value of flag `{flag}`")),
            "{line}: {e}"
        );
        assert!(!e.contains("id)") && !e.contains("../"), "{e}");
    }
    // A value that starts with `-` is refused however it is attached.
    for rest in [
        "--agent-color=-red",
        "--model=--dangerously-skip-permissions",
        "--model --dangerously-skip-permissions",
        "--agent-type '-x'",
        "--parent-session-id=-1",
        "--lead-verify-key=-k",
        "--effort=-high",
        "--tools=-Bash",
        "--disallowed-tools=--allowedTools",
    ] {
        let e = vet(&with(rest)).unwrap_err();
        assert!(e.contains("the value of flag"), "{rest}: {e}");
    }
    // The three every teammate carries are held to their shapes too.
    for line in [
        "cd /d && env claude --agent-id '-a@t' --agent-name a --team-name t",
        "cd /d && env claude --agent-id a@t --agent-name '-a' --team-name t",
        "cd /d && env claude --agent-id a@t --agent-name a --team-name 't;x'",
        "cd /d && env claude --agent-id a@t --agent-name 'a@b' --team-name t",
    ] {
        assert!(
            vet(line).unwrap_err().contains("the value of flag"),
            "{line}"
        );
    }
}

#[test]
fn a_malformed_use_of_a_known_flag_refuses() {
    for (rest, why) in [
        ("--model", "has no value"),
        ("--plan-mode-required=yes", "takes no value"),
        ("--dangerously-skip-permissions=1", "takes no value"),
        ("--model opus --model sonnet", "given twice"),
        ("--agent-id b@t", "given twice"),
        ("--plan-mode-required --plan-mode-required", "given twice"),
        (
            "--permission-mode plan --permission-mode acceptEdits",
            "given twice",
        ),
        (
            "--permission-mode plan --dangerously-skip-permissions",
            "two permission flags",
        ),
        (
            "--dangerously-skip-permissions --permission-mode plan",
            "two permission flags",
        ),
    ] {
        let e = vet(&with(rest)).unwrap_err();
        assert!(e.contains(why), "{rest}: {e}");
    }
    for (line, missing) in [
        (
            "cd /d && env claude --agent-name a --team-name t",
            "--agent-id",
        ),
        (
            "cd /d && env claude --agent-id a@t --team-name t",
            "--agent-name",
        ),
        (
            "cd /d && env claude --agent-id a@t --agent-name a",
            "--team-name",
        ),
        ("cd /d && env claude", "--agent-id"),
    ] {
        let e = vet(line).unwrap_err();
        assert!(e.contains(&format!("no `{missing}`")), "{line}: {e}");
    }
    // The repeatable ones repeat.
    let t = vet(&with(
        "--tools Read --tools '' --disallowed-tools A --disallowed-tools A --disallowedTools B --plugin-dir /a --plugin-dir /b",
    ))
    .unwrap();
    assert_eq!(&t.args[6..], v(&["--tools", "Read", "--tools", ""]));
    assert_eq!(t.deny, ["A", "B"], "deny rules are kept once each");
    assert_eq!(t.dropped, ["--plugin-dir"]);
}

#[test]
fn the_permission_flags_become_a_request_and_are_never_passed_on() {
    for (rest, asked) in [
        ("", Asked::LeadsOwn),
        ("--permission-mode default", Asked::LeadsOwn),
        ("--permission-mode dontAsk", Asked::LeadsOwn),
        ("--permission-mode plan", Asked::Mode(TrustMode::Plan)),
        (
            "--permission-mode acceptEdits",
            Asked::Mode(TrustMode::Edits),
        ),
        ("--permission-mode=auto", Asked::Mode(TrustMode::Auto)),
        (
            "--permission-mode bypassPermissions",
            Asked::Mode(TrustMode::Skip),
        ),
        (
            "--dangerously-skip-permissions",
            Asked::Mode(TrustMode::Skip),
        ),
        ("--plan-mode-required", Asked::Mode(TrustMode::Plan)),
        (
            "--plan-mode-required --dangerously-skip-permissions",
            Asked::Mode(TrustMode::Plan),
        ),
        (
            "--permission-mode bypassPermissions --plan-mode-required",
            Asked::Mode(TrustMode::Plan),
        ),
    ] {
        let t = vet(&with(rest)).unwrap();
        assert_eq!(t.asked, asked, "{rest}");
        assert!(
            !t.args
                .iter()
                .any(|a| a.contains("permission") || a.contains("bypass") || a == "acceptEdits"),
            "{rest}: {:?}",
            t.args
        );
    }
}

#[test]
fn only_three_variables_are_applied_and_the_rest_are_counted() {
    let line = "cd /d && env CLAUDECODE=0 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS='' CLAUDE_CODE_TASK_LIST_ID='my team' CLAUDE_CONFIG_DIR=/tmp/evil HTTPS_PROXY=http://evil:1 ANTHROPIC_BASE_URL=http://evil ANTHROPIC_API_KEY=sk-x CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1 PATH=/tmp/evil ATRIUM_TOKEN=x TMUX=x LD_PRELOAD=/tmp/x.so NODE_OPTIONS=--require=/tmp/x.js claude --agent-id a@t --agent-name a --team-name t";
    let t = vet(line).unwrap();
    assert_eq!(
        t.env,
        [
            ("CLAUDECODE".to_string(), "1".to_string()),
            (
                "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                "1".to_string()
            ),
            (
                "CLAUDE_CODE_TASK_LIST_ID".to_string(),
                "my team".to_string()
            ),
        ],
        "the two switches are 1 whatever the string says"
    );
    assert_eq!(t.env_dropped, 10);
    // A task-list id outside its shape is refused, not dropped: it is one of
    // the three atrium would apply.
    for bad in ["'../x'", "'-x'", "''", "'a;b'", "'a\u{e9}'"] {
        let line = format!(
            "cd /d && env CLAUDE_CODE_TASK_LIST_ID={bad} claude --agent-id a@t --agent-name a --team-name t"
        );
        let e = vet(&line).unwrap_err();
        assert!(e.contains("CLAUDE_CODE_TASK_LIST_ID"), "{bad}: {e}");
    }
    // The last one written is the one applied, as `env` would.
    let t = vet("cd /d && env CLAUDE_CODE_TASK_LIST_ID=one CLAUDE_CODE_TASK_LIST_ID=two claude --agent-id a@t --agent-name a --team-name t").unwrap();
    assert_eq!(t.env[2].1, "two");
}

#[test]
fn the_audit_detail_names_flags_and_counts_and_holds_no_value() {
    let d = teammate_audit(FIXTURE_FULL);
    assert_eq!(
        d,
        "teammate flags=--agent-id,--agent-name,--team-name,--agent-color,--parent-session-id,\
         --lead-verify-key,--plan-mode-required,--agent-type,--permission-mode,--proactivity,\
         --disallowed-tools,--tools,--effort,--settings,--setting-sources,--plugin-dir,\
         --mcp-config,--strict-mcp-config,--no-chrome,--model unknown=0 env=19"
    );
    for secret in [
        KEY,
        "MCowBQYDK2VwAyEA",
        "1763200000000",
        "reviewer",
        "/home/dev",
        "proxy.example",
        "team profile",
        "claude-opus",
        "0b9f6c1e",
        "rm -rf",
    ] {
        assert!(!d.contains(secret), "{secret} is in {d}");
    }
    // Unknown flags are counted, never named: their names are the caller's.
    let d = teammate_audit(&with("--evil-flag '--another\u{202e}'"));
    assert!(d.ends_with("unknown=2 env=1"), "{d}");
    assert!(!d.contains("evil") && !d.contains("another"));
    // A string that does not parse is its length.
    assert_eq!(
        teammate_audit("claude; rm -rf /"),
        "teammate=<unparseable, 16 chars>"
    );
    // The key's value after its flag is skipped, not read as a flag.
    let d = teammate_audit(&with("--lead-verify-key=--model"));
    assert!(d.contains("--lead-verify-key unknown=0"), "{d}");
}

// ---- the working directory --------------------------------------------------

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("atrium-teammate-cwd-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn dir(&self, rel: &str) -> PathBuf {
        let d = self.0.join(rel);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn a_teammate_starts_in_its_leads_directory_or_inside_the_project() {
    let sc = Scratch::new("ok");
    let root = sc.dir("project");
    let sub = sc.dir("project/crates/a");
    let lead = sc.dir("worktrees/lead");
    let real = |p: &Path| std::fs::canonicalize(p).unwrap();
    // The project itself, a directory inside it, and the lead's own
    // directory (a worktree outside the project).
    assert_eq!(teammate_cwd(s(&root), &lead, &root).unwrap(), real(&root));
    assert_eq!(teammate_cwd(s(&sub), &lead, &root).unwrap(), real(&sub));
    assert_eq!(teammate_cwd(s(&lead), &lead, &root).unwrap(), real(&lead));
    // Spelled with `..`, it is still judged by where it really is.
    let dotted = sub.join("..").join("a");
    assert_eq!(teammate_cwd(s(&dotted), &lead, &root).unwrap(), real(&sub));
}

#[test]
fn a_directory_outside_both_is_refused_without_repeating_it() {
    let sc = Scratch::new("out");
    let root = sc.dir("project");
    let lead = sc.dir("worktrees/lead");
    let outside = sc.dir("elsewhere/secret-name");
    let sibling = sc.dir("worktrees/other");
    let prefix_twin = sc.dir("project-evil");
    let file = root.join("file.txt");
    std::fs::write(&file, b"x").unwrap();
    let escape = root.join("..").join("elsewhere").join("secret-name");
    for (asked, why) in [
        (s(&outside).to_string(), "neither the lead's own nor inside"),
        (s(&sibling).to_string(), "neither"),
        // `project-evil` shares the project's name as a prefix, not its path.
        (s(&prefix_twin).to_string(), "neither"),
        (s(&escape).to_string(), "neither"),
        (s(&sc.0).to_string(), "neither"),
        (s(&root.join("missing")).to_string(), "does not exist"),
        (s(&file).to_string(), "not a directory"),
        ("relative/dir".to_string(), "not an absolute path"),
        (".".to_string(), "not an absolute path"),
        ("".to_string(), "not an absolute path"),
    ] {
        let e = teammate_cwd(&asked, &lead, &root).unwrap_err();
        assert!(e.starts_with("teammate refused:"), "{e}");
        assert!(e.contains(why), "{asked}: {e}");
        assert!(!e.contains("secret-name"), "the path is not repeated: {e}");
    }
}

/// A link inside the project that leads outside it: the real path decides.
#[cfg(unix)]
#[test]
fn a_symlinked_directory_cannot_step_outside_the_project() {
    let sc = Scratch::new("link");
    let root = sc.dir("project");
    let lead = sc.dir("project/lead");
    let outside = sc.dir("elsewhere");
    let inner = sc.dir("project/real");
    let link_out = root.join("looks-inside");
    std::os::unix::fs::symlink(&outside, &link_out).unwrap();
    let e = teammate_cwd(s(&link_out), &lead, &root).unwrap_err();
    assert!(e.contains("neither"), "{e}");
    // Through the link, deeper.
    std::fs::create_dir_all(outside.join("deep")).unwrap();
    let e = teammate_cwd(s(&link_out.join("deep")), &lead, &root).unwrap_err();
    assert!(e.contains("neither"), "{e}");
    // A link that stays inside is where it leads.
    let link_in = root.join("alias");
    std::os::unix::fs::symlink(&inner, &link_in).unwrap();
    assert_eq!(
        teammate_cwd(s(&link_in), &lead, &root).unwrap(),
        std::fs::canonicalize(&inner).unwrap()
    );
    // A project reached through a link is compared by its real path too.
    let root_link = sc.0.join("project-link");
    std::os::unix::fs::symlink(&root, &root_link).unwrap();
    assert!(teammate_cwd(s(&inner), &lead, &root_link).is_ok());
}

#[test]
fn a_canonical_path_is_written_plainly_for_the_process() {
    if cfg!(windows) {
        assert_eq!(plain_path(Path::new(r"\\?\D:\work\app")), r"D:\work\app");
        assert_eq!(
            plain_path(Path::new(r"\\?\UNC\host\share\app")),
            r"\\host\share\app"
        );
        // Anything else verbatim is left as it is.
        assert_eq!(
            plain_path(Path::new(r"\\?\Volume{1234}\x")),
            r"\\?\Volume{1234}\x"
        );
        assert_eq!(plain_path(Path::new(r"C:\plain")), r"C:\plain");
    } else {
        assert_eq!(plain_path(Path::new("/work/app")), "/work/app");
    }
    // What `teammate_cwd` answers is a directory a process can start in.
    let sc = Scratch::new("plain");
    let root = sc.dir("project");
    let real = teammate_cwd(s(&root), &root, &root).unwrap();
    let plain = plain_path(&real);
    assert!(!plain.starts_with(r"\\?\"), "{plain}");
    assert!(Path::new(&plain).is_dir());
}

// ---- `teams_env`: what a lead may pass on, and what is said of the rest -------

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|n| n.to_string()).collect()
}

#[test]
fn an_allowlisted_variable_is_applied_and_every_other_is_left_unapplied() {
    let line = "cd /d && env CLAUDECODE=1 ANTHROPIC_BASE_URL=https://one.example ANTHROPIC_BASE_URL=https://gw.example/v1 HTTPS_PROXY=http://proxy:3128 AWS_PROFILE='team a' CLAUDE_CONFIG_DIR=/other claude --agent-id a@t --agent-name a --team-name t";
    let cmd = parse_teammate_command(line).unwrap();
    let t = vet_teammate(
        &cmd,
        &names(&["ANTHROPIC_BASE_URL", "AWS_PROFILE", "NOT_IN_THE_LINE"]),
    )
    .unwrap();
    assert_eq!(
        t.env,
        [
            ("CLAUDECODE".to_string(), "1".to_string()),
            (
                "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                "1".to_string()
            ),
            // The last assignment of a name counts, as `env` has it.
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "https://gw.example/v1".to_string()
            ),
            ("AWS_PROFILE".to_string(), "team a".to_string()),
        ]
    );
    assert_eq!(
        t.unapplied,
        [
            ("HTTPS_PROXY".to_string(), "http://proxy:3128".to_string()),
            ("CLAUDE_CONFIG_DIR".to_string(), "/other".to_string()),
        ]
    );
    assert_eq!(t.env_dropped, 2);
    // With no list (the default) none of them is applied.
    let t = vet_teammate(&cmd, &[]).unwrap();
    assert_eq!(t.env.len(), 2);
    assert_eq!(t.unapplied.len(), 4);
}

/// A name that may never be listed is not applied even if a list holds it
/// (the load refuses such a list; this is the second lock).
#[test]
fn a_forbidden_name_is_never_applied_whatever_the_list_says() {
    let line = "cd /d && env PATH=/evil ATRIUM_TOKEN=x NODE_OPTIONS=--require=/x.js CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1 LD_PRELOAD=/x.so claude --agent-id a@t --agent-name a --team-name t";
    let cmd = parse_teammate_command(line).unwrap();
    let list = names(&[
        "PATH",
        "ATRIUM_TOKEN",
        "NODE_OPTIONS",
        "CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS",
        "LD_PRELOAD",
    ]);
    let t = vet_teammate(&cmd, &list).unwrap();
    assert_eq!(t.env.len(), 2, "{:?}", t.env);
    assert_eq!(t.unapplied.len(), 5);
    for (name, why) in [
        ("PATH", "which program"),
        ("Path", "which program"),
        ("PATHEXT", "which program"),
        ("ATRIUM_TOKEN", "atrium's own"),
        ("atrium_pane", "atrium's own"),
        ("TMUX", "which pane"),
        ("TMUX_PANE", "which pane"),
        ("NODE_OPTIONS", "loads code"),
        ("LD_PRELOAD", "loads code"),
        ("LD_LIBRARY_PATH", "loads code"),
        ("DYLD_INSERT_LIBRARIES", "loads code"),
        ("DYLD_LIBRARY_PATH", "loads code"),
        ("CLAUDE_CODE_TEAMMATE_COMMAND", "never a command"),
        (
            "CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS",
            "permission system",
        ),
        ("X_SKIP_PERMISSIONS", "permission system"),
    ] {
        let got = teams_env_refusal(name).unwrap_or_else(|| panic!("{name} may be listed"));
        assert!(got.contains(why), "{name}: {got}");
    }
    for ok in [
        "ANTHROPIC_BASE_URL",
        "HTTPS_PROXY",
        "AWS_PROFILE",
        "CLAUDE_CONFIG_DIR",
        "_X",
    ] {
        assert_eq!(teams_env_refusal(ok), None, "{ok}");
        assert!(is_env_name(ok));
    }
    let long = "A".repeat(65);
    for bad in ["", "1A", "A B", "A=B", "A-B", "\u{e9}", long.as_str()] {
        assert!(!is_env_name(bad), "{bad:?}");
    }
    assert!(is_env_name(&"A".repeat(64)));
}

#[test]
fn an_allowlisted_value_is_plain_text_or_the_launch_is_refused() {
    let list = names(&["ANTHROPIC_BASE_URL"]);
    for bad in ["'https://x\u{202e}moc.live'", "'a\u{200b}b'", "'a\u{ad}b'"] {
        let line = format!(
            "cd /d && env ANTHROPIC_BASE_URL={bad} claude --agent-id a@t --agent-name a --team-name t"
        );
        let e = vet_teammate(&parse_teammate_command(&line).unwrap(), &list).unwrap_err();
        assert!(e.contains("the value of ANTHROPIC_BASE_URL"), "{e}");
        assert!(!e.contains("live") && !e.contains('\u{202e}'), "{e}");
    }
    assert!(teams_env_value_ok(""));
    assert!(teams_env_value_ok(&"a".repeat(TEAMS_ENV_VALUE_MAX)));
    assert!(!teams_env_value_ok(&"a".repeat(TEAMS_ENV_VALUE_MAX + 1)));
    assert!(!teams_env_value_ok("a\nb") && !teams_env_value_ok("a\u{1b}[2J"));
    // Not on the list, the same value is simply not applied.
    let line = "cd /d && env ANTHROPIC_BASE_URL='a\u{200b}b' claude --agent-id a@t --agent-name a --team-name t";
    assert!(vet_teammate(&parse_teammate_command(line).unwrap(), &[]).is_ok());
}

#[test]
fn a_teams_env_list_is_names_only_and_null_is_not_an_empty_list() {
    let p = |text: &str| parse_teams_env(json::parse(text).ok().as_ref(), "config.json");
    assert_eq!(parse_teams_env(None, "config.json"), Ok(None));
    assert_eq!(p("null"), Ok(None));
    assert_eq!(p("[]"), Ok(Some(Vec::new())));
    assert_eq!(
        p(r#"["HTTPS_PROXY","AWS_PROFILE","HTTPS_PROXY"]"#),
        Ok(Some(names(&["HTTPS_PROXY", "AWS_PROFILE"])))
    );
    for bad in [
        r#""HTTPS_PROXY""#,
        r#"[1]"#,
        r#"["A B"]"#,
        r#"["A=1"]"#,
        r#"{}"#,
    ] {
        let e = p(bad).unwrap_err();
        assert!(
            e.starts_with("config.json: ") && e.contains("teams_env"),
            "{e}"
        );
    }
    let e = p(r#"["HTTPS_PROXY","NODE_OPTIONS"]"#).unwrap_err();
    assert_eq!(
        e,
        "config.json: \"teams_env\" may not list NODE_OPTIONS: it loads code into the \
         teammate's process"
    );
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

/// Only a variable whose value differs from what the teammate has anyway is
/// named; the note holds names and a count, never a value.
#[test]
fn the_not_passed_note_names_what_differs_and_holds_no_value() {
    let has = |name: &str| match name {
        "SAME" => Some("one".to_string()),
        "DIFFERENT" => Some("atriums-value".to_string()),
        "EMPTY_HERE" => Some(String::new()),
        "SAME_BUT_EMPTIED" => Some("set".to_string()),
        _ => None,
    };
    let unapplied = pairs(&[
        ("SAME", "one"),
        ("DIFFERENT", "leads-secret-value"),
        ("ONLY_IN_LEAD", "leads-other-value"),
        // An empty forwarded value is an unset variable.
        ("UNSET_AND_EMPTY", ""),
        ("EMPTY_HERE", ""),
        // Emptied by the lead, set here: that differs.
        ("SAME_BUT_EMPTIED", ""),
    ]);
    let (named, total) = env_not_passed(&unapplied, has);
    assert_eq!(named, ["DIFFERENT", "ONLY_IN_LEAD", "SAME_BUT_EMPTIED"]);
    assert_eq!(total, 3);
    let note = not_passed_note("researcher", &named, total).unwrap();
    assert_eq!(
        note,
        "teammate researcher: 3 variable(s) from the lead were not passed on: DIFFERENT, \
         ONLY_IN_LEAD, SAME_BUT_EMPTIED; add them to teams_env to pass them"
    );
    for value in [
        "leads-secret-value",
        "leads-other-value",
        "atriums-value",
        "one",
    ] {
        assert!(!note.contains(value), "{value} is in {note}");
    }
    // Nothing differs: nothing is said.
    let (named, total) = env_not_passed(&pairs(&[("SAME", "one"), ("X", "")]), has);
    assert!(named.is_empty() && total == 0);
    assert_eq!(not_passed_note("researcher", &named, total), None);
}

#[test]
fn the_not_passed_note_is_bounded_and_shows_only_names_that_are_names() {
    let many: Vec<(String, String)> = (0..20)
        .map(|i| (format!("VAR_{i}"), "x".to_string()))
        .collect();
    let (named, total) = env_not_passed(&many, |_| None);
    assert_eq!((named.len(), total), (20, 20));
    let note = not_passed_note("dev", &named, total).unwrap();
    assert!(note.contains("20 variable(s)"), "{note}");
    assert!(note.contains("VAR_7, (+12 more);"), "{note}");
    assert!(!note.contains("VAR_8"), "{note}");
    // A name that is not a variable name (too long, or with a character a
    // name does not have) is counted and not shown.
    let long = "N".repeat(65);
    let odd = pairs(&[
        (long.as_str(), "x"),
        ("has space", "x"),
        ("esc\u{1b}[2J", "x"),
        ("FINE", "x"),
    ]);
    let (named, total) = env_not_passed(&odd, |_| None);
    assert_eq!((named.clone(), total), (names(&["FINE"]), 4));
    let note = not_passed_note("dev", &named, total).unwrap();
    assert!(
        note.contains("4 variable(s)") && note.contains("FINE, (+3 more)"),
        "{note}"
    );
    assert!(!note.contains('\u{1b}') && !note.contains("NNNN"));
    // The role is the caller's too: scrubbed and bounded.
    let role = format!("dev\u{1b}[2J{}", "x".repeat(100));
    let note = not_passed_note(&role, &names(&["A"]), 1).unwrap();
    assert!(!note.contains('\u{1b}') && note.len() < 200, "{note}");
}
