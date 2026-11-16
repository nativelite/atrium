//! What atrium honours of the command a Claude Code lead hands
//! `respawn-pane` to start a teammate (Claude Code 2.1.325).
//!
//! The command is a shell line a process inside a pane wrote:
//! `cd <cwd> && env <NAME=value>... <program> <flags>`. On a real tmux it is
//! run as written, so whoever holds the lead's pane decides the teammate's
//! program, its environment, its working directory and its permission flags.
//! Under atrium a pane's request is held to the session's trust ceiling, so
//! the line is never run. [`tmuxproto::parse_teammate_command`] takes it
//! apart, and this module decides what of it reaches the teammate:
//!
//! * the **program** only has to be claude by its name; what runs is the
//!   lead pane's own recorded command (the server substitutes it);
//! * each **flag** must be in [`FLAGS`], a closed table read from Claude Code
//!   2.1.325, and each value must fit its shape. The permission flags are
//!   consumed and become a *request* the server caps; the flags that load
//!   code or settings are consumed and dropped; an unknown flag, a positional
//!   argument or a value outside its shape refuses the launch;
//! * of the **environment**, three assignments are applied, the ones whose
//!   names the human listed in `teams_env` are applied, and the rest are
//!   dropped (the server names the ones that would have differed);
//! * the **working directory** must be the lead pane's own
//!   ([`teammate_cwd`]).
//!
//! Everything here is pure (the directory check reads the file system), so
//! the whole policy is tested without a pane.

use crate::bind;
use crate::ctl::TrustMode;
use std::path::Path;
use tmuxproto::TeammateCommand;

/// The value a flag may carry: printable ASCII from a small set, bounded, and
/// never starting with `-` (a value that reads as a flag is refused, however
/// it was attached).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A teammate or team name, an agent type: a letter or digit, then up to
    /// 63 of `A-Z a-z 0-9 _ . -` and single spaces, not ending in a space.
    Name,
    /// `<name>@<team>`: [`Shape::Name`]'s characters and `@`, up to 160.
    AgentId,
    /// A session id: 1 to 128 of `A-Z a-z 0-9 _ -`.
    Token,
    /// A colour word (`blue`, `colour208`): 1 to 24 letters and digits.
    Color,
    /// `<time>.<public key>`: a letter or digit, then up to 1100 of
    /// `A-Z a-z 0-9 . _ -`.
    Key,
    /// A model name or alias: a letter or digit, then up to 199 of
    /// `A-Z a-z 0-9 . _ : / @ [ ] -`.
    Model,
    /// An effort level: 1 to 24 letters and digits.
    Word,
    /// A tool name, or nothing (`--tools ''` is "no tools"): a letter, then up
    /// to 99 of `A-Z a-z 0-9 _ -`.
    Tool,
}

impl Shape {
    /// Does `v` fit? Pure.
    pub fn fits(self, v: &str) -> bool {
        let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
        let starts = v.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
        match self {
            Shape::Name => {
                starts
                    && v.len() <= 64
                    && !v.ends_with(' ')
                    && !v.contains("  ")
                    && v.chars().all(|c| word(c) || c == ' ')
            }
            Shape::AgentId => {
                starts
                    && v.len() <= 160
                    && !v.ends_with(' ')
                    && !v.contains("  ")
                    && v.chars().all(|c| word(c) || c == ' ' || c == '@')
            }
            Shape::Token => {
                (1..=128).contains(&v.len())
                    && !v.starts_with('-')
                    && v.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            }
            Shape::Color | Shape::Word => {
                (1..=24).contains(&v.len()) && v.chars().all(|c| c.is_ascii_alphanumeric())
            }
            Shape::Key => starts && v.len() <= 1101 && v.chars().all(word),
            Shape::Model => {
                starts
                    && v.len() <= 200
                    && v.chars()
                        .all(|c| word(c) || matches!(c, ':' | '/' | '@' | '[' | ']'))
            }
            Shape::Tool => {
                v.is_empty()
                    || (v.len() <= 100
                        && v.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                        && v.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')))
            }
        }
    }
}

/// What atrium does with one flag of the teammate command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// Passed on with its value, once; the value must fit.
    Pass(Shape),
    /// Passed on with its value, as often as it is written.
    PassEach(Shape),
    /// A switch, passed on.
    Switch,
    /// `--agent-name <name>`: passed on, and the pane's role.
    Role,
    /// `--plan-mode-required`: passed on, and a request for `plan`.
    PlanRequired,
    /// `--permission-mode <mode>`: consumed; a request the server caps.
    Mode,
    /// `--dangerously-skip-permissions`: consumed; a request for `skip`.
    Skip,
    /// A deny rule: consumed, and added to the pane's own deny list, which
    /// atrium writes as its one `--disallowedTools`.
    Deny,
    /// Consumed with its value and not passed on.
    DropValue,
    /// A switch, consumed and not passed on.
    DropSwitch,
}

/// One row of the closed table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag {
    pub name: &'static str,
    pub take: Take,
}

const fn f(name: &'static str, take: Take) -> Flag {
    Flag { name, take }
}

/// Every flag a Claude Code 2.1.325 lead can put in a teammate command, and
/// nothing else. Read from the installed binary's own code (2026-11-15).
///
/// The first eight are the teammate's identity, written by the spawn
/// function itself (`--agent-type` among them, which is dropped: see below).
/// The rest are the "inherited" flags, which one function
/// builds from the lead's own state: its permission mode, its command-line
/// deny rules and tool list, its model and effort, and every settings, plugin
/// and MCP source it was started with.
///
/// **Dropped** (consumed, never passed on): the flags that would let the
/// string choose what code or settings the teammate loads. `--settings` and
/// `--managed-settings` carry hooks and permission rules; `--plugin-dir`,
/// `--plugin-dir-no-mcp` and `--plugin-url` carry hooks; `--mcp-config` names
/// commands to start; `--project-config-root` moves where project settings
/// are read from; `--setting-sources` can leave out the settings file that
/// holds the human's own deny rules and hooks; `--proactivity` shapes the
/// permission mode; `--chrome` turns a browser integration on. And
/// `--agent-type`, for now: it names an agent definition, which a pane can
/// write under the project's `.claude/agents`, and whether that file's own
/// `permissionMode`, hooks or MCP servers outrank the `--permission-mode`
/// atrium passes has not been established by a live run. Dropped, the
/// teammate runs as a general teammate. A teammate
/// gets atrium's own `--settings`
/// (its status hooks) and atrium's mod like every claude pane, and reads the
/// user's and the project's settings as any claude started in that directory.
///
/// **Passed on** besides the identity: what can only narrow the teammate
/// (`--tools`, `--strict-mcp-config`, `--no-chrome`, `--restricted`) or
/// picks a model (`--model`, `--effort`, `--include-unsupported-models`).
pub const FLAGS: &[Flag] = &[
    // The teammate's identity (the spawn function).
    f("--agent-id", Take::Pass(Shape::AgentId)),
    f("--agent-name", Take::Role),
    f("--team-name", Take::Pass(Shape::Name)),
    f("--agent-color", Take::Pass(Shape::Color)),
    f("--parent-session-id", Take::Pass(Shape::Token)),
    f("--lead-verify-key", Take::Pass(Shape::Key)),
    f("--plan-mode-required", Take::PlanRequired),
    f("--agent-type", Take::DropValue),
    // The permission posture (the inherited-flags function, first).
    f("--dangerously-skip-permissions", Take::Skip),
    f("--permission-mode", Take::Mode),
    f("--proactivity", Take::DropValue),
    // Deny rules and the tool list.
    f("--disallowed-tools", Take::Deny),
    f("--disallowedTools", Take::Deny),
    f("--tools", Take::PassEach(Shape::Tool)),
    // The model.
    f("--model", Take::Pass(Shape::Model)),
    f("--effort", Take::Pass(Shape::Word)),
    f("--include-unsupported-models", Take::Switch),
    // Settings, plugins, MCP.
    f("--settings", Take::DropValue),
    f("--setting-sources", Take::DropValue),
    f("--managed-settings", Take::DropValue),
    f("--plugin-dir", Take::DropValue),
    f("--plugin-dir-no-mcp", Take::DropValue),
    f("--plugin-url", Take::DropValue),
    f("--project-config-root", Take::DropValue),
    f("--mcp-config", Take::DropValue),
    f("--strict-mcp-config", Take::Switch),
    f("--chrome", Take::DropSwitch),
    f("--no-chrome", Take::Switch),
    f("--restricted", Take::Switch),
];

/// The flags atrium writes into a claude pane's command itself. None of them
/// may be passed on from a teammate command: a second copy would compete
/// with atrium's (claude takes the last of a repeated option).
pub const ATRIUM_OWN_FLAGS: &[&str] = &[
    "--permission-mode",
    "--dangerously-skip-permissions",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--append-system-prompt",
    "--session-id",
    "--settings",
    "--teammate-mode",
];

/// The environment assignments atrium applies; the first two always with the
/// value `1`, whatever the string says.
pub const ENV_CLAUDECODE: &str = "CLAUDECODE";
pub const ENV_AGENT_TEAMS: &str = "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS";
pub const ENV_TASK_LIST: &str = "CLAUDE_CODE_TASK_LIST_ID";

/// Most deny rules and most arguments a teammate command may carry.
const MAX_DENY: usize = 512;
const MAX_DENY_LEN: usize = 512;

/// What the claude versions were, for a refusal a human will read.
const READ_FROM: &str = "this atrium knows the teammate command of Claude Code 2.1.289 and 2.1.325";

/// The posture a teammate command asks for, before the server caps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// No permission flag at all: the teammate takes its lead pane's mode.
    Nothing,
    /// `--permission-mode default`: ask before every edit and command. It
    /// sits under every rung atrium governs but `plan`, so it is passed on
    /// as written (a lead its human switched down to `default` does not get
    /// hands-off teammates).
    Default,
    /// `--permission-mode dontAsk`: refuse what is not already allowed.
    /// Passed on as written, like [`Asked::Default`].
    DontAsk,
    /// A mode atrium governs, by name, by `--plan-mode-required`, or by the
    /// skip switch.
    Mode(TrustMode),
}

/// A teammate command, vetted: only what atrium will act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Teammate {
    /// `--agent-name`: the pane's role.
    pub role: String,
    /// The flags passed on, in the order written, each as the string wrote it
    /// (`--flag`, `value`, or the one word `--flag=value`).
    pub args: Vec<String>,
    /// The posture asked for.
    pub asked: Asked,
    /// Deny rules to add to the pane's deny list.
    pub deny: Vec<String>,
    /// The environment applied.
    pub env: Vec<(String, String)>,
    /// How many assignments were in the string and not applied.
    pub env_dropped: usize,
    /// Those assignments, the last of each name: the server compares each
    /// with what the teammate will have anyway and names the ones that
    /// differ ([`env_not_passed`]). Values; never logged or shown.
    pub unapplied: Vec<(String, String)>,
    /// The flags consumed and not passed on, each once, in the order met.
    pub dropped: Vec<&'static str>,
}

/// A flag's name as a message may show it: itself when it is `-` or `--` and
/// up to 48 of `A-Z a-z 0-9 -`, otherwise a description. A refusal reaches the
/// caller, its terminal and the audit log, and the name is the caller's text.
fn flag_label(name: &str) -> String {
    let body = name.trim_start_matches('-');
    let dashes = name.len() - body.len();
    if (1..=2).contains(&dashes)
        && (1..=48).contains(&body.len())
        && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        format!("`{name}`")
    } else {
        "a flag whose name is not plain letters".to_string()
    }
}

fn refused(what: &str) -> String {
    format!(
        "teammate refused: {what}. Nothing was started. {READ_FROM}; if the installed Claude \
         Code is newer, update atrium, or keep its teammates in-process (`--teammate-mode \
         in-process`, or `\"teams\": \"off\"` in atrium's config)"
    )
}

/// The trust mode a `--permission-mode` value asks for, or `Err` for a value
/// this atrium does not know.
fn asked_mode(value: &str) -> Result<Asked, ()> {
    match value {
        "acceptEdits" => Ok(Asked::Mode(TrustMode::Edits)),
        "auto" => Ok(Asked::Mode(TrustMode::Auto)),
        "plan" => Ok(Asked::Mode(TrustMode::Plan)),
        "bypassPermissions" => Ok(Asked::Mode(TrustMode::Skip)),
        "default" => Ok(Asked::Default),
        "dontAsk" => Ok(Asked::DontAsk),
        _ => Err(()),
    }
}

/// Vet a parsed teammate command against the closed table. `Err` is the
/// refusal, which names the flag and never a value. Pure but for the
/// session's claude aliases ([`bind::is_claude_stem`]).
///
/// Refused: a program that is not claude by its name; an argument that is
/// not a flag (a positional, which would be the teammate's first prompt, and
/// any prefix argument a launcher or `CLAUDE_CODE_TEAMMATE_COMMAND` adds); a
/// flag outside [`FLAGS`]; a switch given a value; a valued flag with none; a
/// value outside its [`Shape`]; a one-time flag given twice; two permission
/// flags; a `--permission-mode` this atrium does not know; a missing
/// `--agent-id`, `--agent-name` or `--team-name`; a task-list id outside its
/// shape; an allowlisted variable whose value holds a control or hidden
/// character.
///
/// `teams_env` is the session's allowlist of variable names
/// ([`session_teams_env`]): an assignment of one of them is applied, after
/// the three team variables. Every other assignment is in `unapplied`.
pub fn vet_teammate(cmd: &TeammateCommand, teams_env: &[String]) -> Result<Teammate, String> {
    let program = cmd.argv.first().map(String::as_str).unwrap_or("");
    if !bind::is_claude_stem(&bind::command_stem(program)) {
        return Err(refused(
            "the command's program is not claude. atrium starts a teammate as the same \
             claude its lead pane was started as; CLAUDE_CODE_TEAMMATE_COMMAND and a \
             launcher setting are not honoured",
        ));
    }
    let mut args: Vec<String> = Vec::new();
    let mut seen: Vec<&'static str> = Vec::new();
    let mut dropped: Vec<&'static str> = Vec::new();
    let mut deny: Vec<String> = Vec::new();
    let mut role: Option<String> = None;
    let mut asked: Option<Asked> = None;
    let mut plan_required = false;
    let mut i = 1;
    while i < cmd.argv.len() {
        let arg = cmd.argv[i].as_str();
        i += 1;
        if !arg.starts_with('-') {
            return Err(refused(
                "the command has an argument that is not a flag (a positional argument \
                 would be the teammate's first prompt, and a prefix argument means a \
                 launcher or CLAUDE_CODE_TEAMMATE_COMMAND, which atrium does not honour)",
            ));
        }
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg, None),
        };
        let Some(flag) = FLAGS.iter().find(|fl| fl.name == name) else {
            return Err(refused(&format!(
                "flag {} is not one atrium knows in a teammate command",
                flag_label(name)
            )));
        };
        let name = flag.name;
        let valued = !matches!(
            flag.take,
            Take::Switch | Take::PlanRequired | Take::Skip | Take::DropSwitch
        );
        let value: Option<&str> = if !valued {
            if inline.is_some() {
                return Err(refused(&format!(
                    "flag `{name}` takes no value and was given one"
                )));
            }
            None
        } else if let Some(v) = inline {
            Some(v)
        } else if let Some(v) = cmd.argv.get(i) {
            i += 1;
            Some(v.as_str())
        } else {
            return Err(refused(&format!("flag `{name}` has no value")));
        };
        let once = !matches!(
            flag.take,
            Take::PassEach(_) | Take::Deny | Take::DropValue | Take::DropSwitch
        );
        if once && seen.contains(&name) {
            return Err(refused(&format!("flag `{name}` is given twice")));
        }
        if !seen.contains(&name) {
            seen.push(name);
        }
        let bad_value = || {
            refused(&format!(
                "the value of flag `{name}` is not of the form atrium accepts for it"
            ))
        };
        // As the string wrote it: two words, or the one word `--flag=value`.
        let pass = |args: &mut Vec<String>, value: Option<&str>| match (value, inline) {
            (Some(_), Some(_)) => args.push(arg.to_string()),
            (Some(v), None) => {
                args.push(name.to_string());
                args.push(v.to_string());
            }
            (None, _) => args.push(name.to_string()),
        };
        match flag.take {
            Take::Pass(shape) | Take::PassEach(shape) => {
                let v = value.unwrap_or("");
                if !shape.fits(v) {
                    return Err(bad_value());
                }
                pass(&mut args, Some(v));
            }
            Take::Role => {
                let v = value.unwrap_or("");
                if !Shape::Name.fits(v) {
                    return Err(bad_value());
                }
                role = Some(v.to_string());
                pass(&mut args, Some(v));
            }
            Take::Switch => pass(&mut args, None),
            Take::PlanRequired => {
                plan_required = true;
                pass(&mut args, None);
            }
            Take::Mode | Take::Skip => {
                if asked.is_some() {
                    return Err(refused(
                        "the command carries two permission flags, and atrium does not \
                         choose between them",
                    ));
                }
                asked = Some(match flag.take {
                    Take::Skip => Asked::Mode(TrustMode::Skip),
                    _ => asked_mode(value.unwrap_or("")).map_err(|()| bad_value())?,
                });
            }
            Take::Deny => {
                let v = value.unwrap_or("");
                // The rule joins the pane's deny list, which atrium writes as
                // claude rules ([`crate::trust::deny_rule`]): a tool name
                // (`WebFetch`) or a tool with a pattern (`Bash(git push:*)`)
                // is written as it is, and anything else would be rewritten
                // as a command prefix, which is another rule.
                let plain = !v.is_empty()
                    && v.len() <= MAX_DENY_LEN
                    && !v.starts_with('-')
                    && v.chars().all(|c| c == ' ' || c.is_ascii_graphic())
                    && crate::trust::deny_rule(v).as_deref() == Some(v);
                if !plain || deny.len() >= MAX_DENY {
                    // A deny rule atrium cannot carry intact is a rule the
                    // teammate would not enforce: refused, as Claude Code
                    // itself refuses to start a teammate without one.
                    return Err(bad_value());
                }
                if !deny.iter().any(|d| d == v) {
                    deny.push(v.to_string());
                }
            }
            Take::DropValue | Take::DropSwitch => {
                if !dropped.contains(&name) {
                    dropped.push(name);
                }
            }
        }
    }
    for needed in ["--agent-id", "--agent-name", "--team-name"] {
        if !seen.contains(&needed) {
            return Err(refused(&format!(
                "the command has no `{needed}` (a teammate carries an agent id, an agent \
                 name and a team name)"
            )));
        }
    }
    let role = role.expect("--agent-name was seen, and sets the role");
    // `--plan-mode-required` is `plan`, whatever mode came with it (Claude
    // Code writes `--permission-mode plan` beside it).
    let asked = if plan_required {
        Asked::Mode(TrustMode::Plan)
    } else {
        asked.unwrap_or(Asked::Nothing)
    };
    let mut env = vec![
        (ENV_CLAUDECODE.to_string(), "1".to_string()),
        (ENV_AGENT_TEAMS.to_string(), "1".to_string()),
    ];
    let mut env_dropped = 0;
    let mut task_list: Option<&str> = None;
    // The last assignment of a name is the one that counts, as `env` has it.
    let mut listed: Vec<(String, String)> = Vec::new();
    let mut unapplied: Vec<(String, String)> = Vec::new();
    // (On Windows a name is the same name in any case, as the merge into the
    // child's environment has it.)
    let fold = cfg!(windows);
    let put = |into: &mut Vec<(String, String)>, name: &str, value: &str| {
        into.retain(|(n, _)| !env_name_eq(n, name, fold));
        into.push((name.to_string(), value.to_string()));
    };
    for (name, value) in &cmd.env {
        match name.as_str() {
            ENV_CLAUDECODE | ENV_AGENT_TEAMS => {}
            ENV_TASK_LIST => {
                if !Shape::AgentId.fits(value) {
                    return Err(refused(&format!(
                        "the value of {ENV_TASK_LIST} is not of the form atrium accepts for it"
                    )));
                }
                task_list = Some(value);
            }
            // On the human's list (`teams_env`), and not a name that may
            // never be listed, whatever the list says.
            n if teams_env.iter().any(|a| env_name_eq(a, n, fold))
                && teams_env_refusal(n).is_none() =>
            {
                if !teams_env_value_ok(value) {
                    return Err(refused(&format!(
                        "the value of {n}, which `teams_env` lets a lead pass on, holds a \
                         control or hidden character or is over {TEAMS_ENV_VALUE_MAX} bytes"
                    )));
                }
                put(&mut listed, n, value);
            }
            n => {
                env_dropped += 1;
                put(&mut unapplied, n, value);
            }
        }
    }
    if let Some(id) = task_list {
        env.push((ENV_TASK_LIST.to_string(), id.to_string()));
    }
    env.extend(listed);
    Ok(Teammate {
        role,
        args,
        asked,
        deny,
        env,
        env_dropped,
        unapplied,
        dropped,
    })
}

// ---- `teams_env`: the variables a lead may pass on ---------------------------

/// Longest value an allowlisted variable may carry.
pub const TEAMS_ENV_VALUE_MAX: usize = 4096;

/// Is `name` an environment variable name a `teams_env` list may hold: a
/// letter or `_`, then up to 63 letters, digits and `_`?
pub fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Are `a` and `b` the same environment variable name? Exactly, or without
/// regard to ASCII case when `fold` is set, which is how Windows has it.
pub fn env_name_eq(a: &str, b: &str, fold: bool) -> bool {
    if fold {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Why `name` can never be on a `teams_env` list, or `None` when it may. Each
/// of these, set from a lead's command line, would do more than configure a
/// provider: skip the permission system, impersonate another pane to atrium,
/// change which programs a name finds, or load code into the teammate before
/// any flag is read. Case is folded: Windows does not tell `Path` from `PATH`.
pub fn teams_env_refusal(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_uppercase();
    if n.contains("SKIP_PERMISSIONS") {
        return Some("it switches the permission system off");
    }
    if n.starts_with("ATRIUM_") {
        return Some("ATRIUM_ variables are atrium's own, the pane's token among them");
    }
    if n.starts_with("DYLD_") || matches!(n.as_str(), "LD_PRELOAD" | "LD_LIBRARY_PATH") {
        return Some("it loads code into the teammate's process");
    }
    if n.starts_with("GIT_CONFIG_") {
        return Some("it rewrites git's configuration, which can name programs to run");
    }
    match n.as_str() {
        "TMUX" | "TMUX_PANE" => Some("it is how atrium tells a claude pane which pane it is"),
        "PATH" | "PATHEXT" => Some("it decides which program a name runs"),
        "NODE_OPTIONS" | "BUN_OPTIONS" | "CLAUDE_CODE_PLUGIN_DIRS" => {
            Some("it loads code into the teammate's process")
        }
        "BASH_ENV" | "ENV" => Some("it names a script every shell the teammate starts runs"),
        "COMSPEC" | "SHELL" | "CLAUDE_CODE_SHELL" => {
            Some("it decides which shell runs the teammate's commands")
        }
        "GIT_SSH_COMMAND" => Some("it names a program git runs"),
        "CARGO_MAKEFLAGS" => Some("it is the session's build pool, which atrium sets"),
        "CLAUDE_CODE_TEAMMATE_COMMAND" => {
            Some("a teammate is the lead pane's own claude, never a command a variable names")
        }
        _ => None,
    }
}

/// Is `name` one a `teams_env` list may hold but that moves **where settings
/// are read from or where traffic goes**: `CLAUDE_CONFIG_DIR`, `HOME`,
/// `USERPROFILE`, `APPDATA`, `ANTHROPIC_BASE_URL`, any name ending `_PROXY`,
/// `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE` and `SystemRoot`, in any case?
/// Listable on purpose (a proxy is the common case), and disclosed with a
/// warning wherever the list is shown ([`teams_env_disclosure`]).
pub fn teams_env_warned(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n.ends_with("_PROXY")
        || matches!(
            n.as_str(),
            "CLAUDE_CONFIG_DIR"
                | "HOME"
                | "USERPROFILE"
                | "APPDATA"
                | "ANTHROPIC_BASE_URL"
                | "NODE_EXTRA_CA_CERTS"
                | "SSL_CERT_FILE"
                | "SYSTEMROOT"
        )
}

/// What a human is shown of a `teams_env` list before a session starts (the
/// fleet banner, a plain launch's one stderr line, `atrium config path`):
/// the names, and a `WARNING` naming the ones [`teams_env_warned`] flags.
/// `None` for an empty list.
pub fn teams_env_disclosure(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let mut line = format!(
        "variables a Claude Code lead may set on its teammates (`teams_env`): {}; each \
         is taken from a pane's request",
        names.join(", ")
    );
    let warned: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| teams_env_warned(n))
        .collect();
    if !warned.is_empty() {
        line.push_str(&format!(
            ". WARNING: {} can move where a teammate reads its settings and \
             credentials from, or where its traffic goes",
            warned.join(", ")
        ));
    }
    Some(line)
}

/// May an allowlisted variable carry `value`: at most
/// [`TEAMS_ENV_VALUE_MAX`] bytes, no control character and none that hides or
/// reorders text ([`crate::ctl::is_hidden_char`])?
pub fn teams_env_value_ok(value: &str) -> bool {
    value.len() <= TEAMS_ENV_VALUE_MAX
        && !value
            .chars()
            .any(|c| c.is_control() || crate::ctl::is_hidden_char(c))
}

/// Parse a `teams_env` list (the user config's or a fleet's): an array of
/// environment variable **names** a lead's teammate command may set on its
/// teammate. `whose` names the file for the error. Absent or `null` is `None`
/// (not set), which is not `[]` (set, and empty: a fleet's `[]` replaces the
/// config's list). A name that is not a variable name, or one that may never
/// be listed ([`teams_env_refusal`]), is an error that says why.
pub fn parse_teams_env(
    v: Option<&json::Value>,
    whose: &str,
) -> Result<Option<Vec<String>>, String> {
    let items = match v {
        None | Some(json::Value::Null) => return Ok(None),
        Some(json::Value::Array(items)) => items,
        Some(_) => {
            return Err(format!(
                "{whose}: \"teams_env\" must be an array of environment variable names, such \
                 as [\"ANTHROPIC_BASE_URL\", \"HTTPS_PROXY\"]"
            ))
        }
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let name = item.as_str().filter(|n| is_env_name(n)).ok_or_else(|| {
            format!(
                "{whose}: every entry of \"teams_env\" is an environment variable name: a \
                 letter or _ then up to 63 of A-Z a-z 0-9 _"
            )
        })?;
        if let Some(why) = teams_env_refusal(name) {
            return Err(format!("{whose}: \"teams_env\" may not list {name}: {why}"));
        }
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    Ok(Some(out))
}

/// The session's `teams_env`, set once at launch from a fleet.
static SESSION_TEAMS_ENV: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Record the session's `teams_env` (a fleet's, already filled from the
/// config where the fleet set none). First call wins: it is made before any
/// pane exists, and nothing a ctl request says reaches it.
pub fn set_session_teams_env(names: Vec<String>) {
    let _ = SESSION_TEAMS_ENV.set(names);
}

/// The variables a lead's teammate command may set in this session: the
/// fleet's list when one was launched, else the user config's `teams_env`.
/// Empty by default.
pub fn session_teams_env() -> Vec<String> {
    SESSION_TEAMS_ENV
        .get()
        .cloned()
        .unwrap_or_else(|| crate::config::get().teams_env.clone())
}

/// Most names one notice lists before it counts the rest.
const NOT_PASSED_SHOWN: usize = 8;

/// Of the assignments a teammate command carried and atrium did not apply,
/// the **names** whose value differs from what the teammate will have anyway
/// (`will_have`: atrium's own ambient environment and the non-secret
/// variables it adds). An empty forwarded value is the same as an unset
/// variable. A variable the teammate gets the same value for is not a loss
/// and is not named.
///
/// `secret` is the names the lead's **identity** sets. Such a variable is
/// never compared and never named or counted: the teammate has atrium's
/// value for it by design, and answering "same" or "different" for a value
/// the caller chose would let it test guesses of a credential.
///
/// `fold` (Windows) makes a name the same name in any case, as the merge
/// into the child's environment has it: of several case variants the last
/// is the one assignment, compared once, so variants cannot carry several
/// guesses or count twice.
///
/// Names only, never a value. They are the caller's text: one that is not a
/// variable name of at most 64 characters is left out of the list and counted
/// in the second number. Returns `(names, how many differ in all)`.
pub fn env_not_passed(
    unapplied: &[(String, String)],
    secret: &[String],
    fold: bool,
    will_have: impl Fn(&str) -> Option<String>,
) -> (Vec<String>, usize) {
    // One assignment per name: the last, as the environment has it.
    let mut last: Vec<&(String, String)> = Vec::new();
    for pair in unapplied {
        last.retain(|(n, _)| !env_name_eq(n, &pair.0, fold));
        last.push(pair);
    }
    let mut names: Vec<String> = Vec::new();
    let mut total = 0;
    for (name, value) in last {
        if secret.iter().any(|s| env_name_eq(s, name, fold)) {
            continue;
        }
        if *value == will_have(name).unwrap_or_default() {
            continue;
        }
        total += 1;
        if is_env_name(name) && !names.contains(name) {
            names.push(name.clone());
        }
    }
    (names, total)
}

/// The one line that says what was not passed on, for the bar and the audit
/// log; `None` when nothing differed. At most eight names, then a count.
pub fn not_passed_note(role: &str, names: &[String], total: usize) -> Option<String> {
    if total == 0 {
        return None;
    }
    let shown: Vec<&str> = names
        .iter()
        .take(NOT_PASSED_SHOWN)
        .map(String::as_str)
        .collect();
    let rest = total - shown.len();
    let mut list = shown.join(", ");
    if rest > 0 {
        if !list.is_empty() {
            list.push_str(", ");
        }
        list.push_str(&format!("(+{rest} more)"));
    }
    Some(format!(
        "teammate {}: {total} variable(s) from the lead were not passed on: {list}; add them \
         to teams_env to pass them",
        crate::audit::scrub(role, 40)
    ))
}

/// The audit detail for a teammate command: the names of the flags it
/// carries that are in [`FLAGS`], how many it carries that are not, and how
/// many environment assignments. Never a value: not the verify key, not a
/// path, not a model. A string that does not parse is described by its length.
pub fn teammate_audit(command: &str) -> String {
    const SHOWN: usize = 32;
    let Ok(cmd) = tmuxproto::parse_teammate_command(command) else {
        return format!("teammate=<unparseable, {} chars>", command.chars().count());
    };
    let mut names: Vec<&'static str> = Vec::new();
    let mut unknown = 0usize;
    let mut i = 1;
    while i < cmd.argv.len() {
        let arg = cmd.argv[i].as_str();
        i += 1;
        let name = arg.split_once('=').map(|(n, _)| n).unwrap_or(arg);
        match FLAGS.iter().find(|fl| fl.name == name) {
            Some(flag) => {
                if !names.contains(&flag.name) {
                    names.push(flag.name);
                }
                let valued = !matches!(
                    flag.take,
                    Take::Switch | Take::PlanRequired | Take::Skip | Take::DropSwitch
                );
                if valued && !arg.contains('=') {
                    i += 1;
                }
            }
            None => unknown += 1,
        }
    }
    let more = names.len().saturating_sub(SHOWN);
    names.truncate(SHOWN);
    let mut out = format!("teammate flags={}", names.join(","));
    if more > 0 {
        out.push_str(&format!(",(+{more} more)"));
    }
    out.push_str(&format!(" unknown={unknown} env={}", cmd.env.len()));
    out
}

/// May a teammate start in `asked` (the word after `cd`)? Only when it is
/// the **lead pane's own working directory**: the two are compared by their
/// real paths (`std::fs::canonicalize`), so another spelling of the same
/// directory passes and a link to another does not. `Ok` means "start it
/// where the lead is"; the caller uses its own record of the lead's
/// directory, never this string. The refusal does not repeat the path.
///
/// A directory of the caller's choosing, even one inside the project, is
/// refused: a claude started there loads that directory's `.claude/settings`,
/// `.mcp.json` and `CLAUDE.md`, and atrium would pre-accept its folder trust,
/// none of which a plain `ctl spawn` lets a pane choose.
pub fn teammate_cwd(asked: &str, lead: &Path) -> Result<(), String> {
    let refuse = |why: &str| {
        format!(
            "teammate refused: its working directory {why}. A teammate starts where its \
             lead is: the only directory atrium accepts is the lead pane's own (a \
             directory of the request's choosing would bring its own settings, MCP servers \
             and instructions). Nothing was started"
        )
    };
    let path = Path::new(asked);
    if !path.is_absolute() {
        return Err(refuse("is not an absolute path"));
    }
    let real =
        std::fs::canonicalize(path).map_err(|_| refuse("does not exist or cannot be read"))?;
    if !std::fs::canonicalize(lead).is_ok_and(|l| l == real) {
        return Err(refuse("is not its lead pane's own"));
    }
    Ok(())
}

/// A lead's recorded program as the teammate is started with it. A bare name
/// is left for the `PATH` pin; an absolute path is left as it is. A
/// **relative path** (`./bin/claude`, a fleet `cmd` written that way) is made
/// absolute against `base`, atrium's own working directory, which is where it
/// was resolved from when the lead was started: on unix a child resolves a
/// relative program after it has changed into its own directory, so the same
/// words could name another file there.
pub fn absolute_program(own: &str, base: &Path) -> String {
    let is_path = own.contains(['/', '\\']);
    if is_path && !Path::new(own).is_absolute() {
        return base.join(own).to_string_lossy().into_owned();
    }
    own.to_string()
}

#[cfg(test)]
mod tests;
