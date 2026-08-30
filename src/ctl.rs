//! `amux ctl` — the verb layer over the control channel ([`crate::ipc`]).
//!
//! Two halves live here:
//! * The **client** ([`ctl_cmd`]): parse `amux ctl <cmd> …` argv into one JSON
//!   request, read `AMUX_CTL` (the endpoint) and `AMUX_PANE` (the caller's agent
//!   id) from the environment amux injected, send it, and print the JSON reply.
//! * The **protocol + policy** the server (the run loop) applies: [`parse_request`]
//!   turns a request line into a typed [`Request`], and [`evaluate_spawn`] is the
//!   *pure* guard — allowlist + depth cap — so the safety rules are unit-tested
//!   without a pty or a running amux.
//!
//! C1 surface: `spawn` (open a visible worker pane) and `list` (the org chart).
//! `send` / `status` / `kill` arrive in C2–C3.

use std::process::ExitCode;

use json::{Number, Value};

use crate::bind;

/// Environment variable naming the control endpoint, injected into every pane a
/// `--allow-ctl` amux spawns. Absent → `amux ctl` refuses (not in a ctl session).
pub const ENV_ADDRESS: &str = "AMUX_CTL";
/// Environment variable carrying the *caller* pane's agent id, so the server can
/// attribute a spawn to its parent (spawn tree + depth). Injected per pane.
pub const ENV_PANE: &str = "AMUX_PANE";

/// The default spawn-depth ceiling: the recursion circuit-breaker (design §5).
/// Generous — a real hierarchy is CEO→lead→IC (depth 2–3); this only stops a
/// runaway self-spawning agent. Overridable/removable via `--max-depth`.
pub const DEFAULT_MAX_DEPTH: usize = 6;

/// A parsed control request.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The agent id of the pane that issued this (from `AMUX_PANE`), if any.
    pub caller: Option<usize>,
    pub cmd: Cmd,
}

/// The C1 command set.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// Open a new worker pane running `argv`, tagged `role`.
    Spawn(SpawnReq),
    /// Report the spawn tree.
    List,
}

/// A `spawn` request's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnReq {
    pub role: Option<String>,
    /// The command to host, e.g. `["claude"]`. Must be non-empty and on the
    /// agent allowlist (checked by [`evaluate_spawn`]).
    pub argv: Vec<String>,
    /// Open in a new window (true, the C1 default) vs. split the caller (later).
    pub new_window: bool,
}

/// Why a spawn was refused. Each maps to a clear reply the caller can act on.
#[derive(Debug, Clone, PartialEq)]
pub enum SpawnDenied {
    EmptyCommand,
    NotAllowed(String),
    DepthExceeded { attempted: usize, max: usize },
}

impl SpawnDenied {
    pub fn message(&self) -> String {
        match self {
            SpawnDenied::EmptyCommand => "spawn needs a command (e.g. `-- claude`)".to_string(),
            SpawnDenied::NotAllowed(stem) => format!(
                "{stem:?} is not on the agent allowlist ({}); ctl spawns agents only",
                bind::AGENT_STEMS.join(", ")
            ),
            SpawnDenied::DepthExceeded { attempted, max } => {
                format!("spawn depth {attempted} exceeds --max-depth {max} (recursion guard)")
            }
        }
    }
}

/// The **pure** spawn guard: is this command allowed to spawn at this depth?
/// `caller_depth` is the depth of the requesting pane (a human/root pane is 0);
/// the new worker would be at `caller_depth + 1`. `extra_allow` extends the
/// built-in agent allowlist ([`bind::AGENT_STEMS`]) with operator-approved stems
/// (the `AMUX_CTL_ALLOW` knob). Returns the new worker's depth on success.
/// Unit-tested in isolation — this is the heart of the safety model.
pub fn evaluate_spawn(
    argv: &[String],
    caller_depth: usize,
    max_depth: usize,
    extra_allow: &[String],
) -> Result<usize, SpawnDenied> {
    let Some(first) = argv.first() else {
        return Err(SpawnDenied::EmptyCommand);
    };
    if first.is_empty() {
        return Err(SpawnDenied::EmptyCommand);
    }
    let stem = std::path::Path::new(first)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| first.clone());
    let allowed = bind::is_agent_stem(&stem) || extra_allow.iter().any(|s| s == &stem);
    if !allowed {
        return Err(SpawnDenied::NotAllowed(stem));
    }
    let attempted = caller_depth + 1;
    if attempted > max_depth {
        return Err(SpawnDenied::DepthExceeded {
            attempted,
            max: max_depth,
        });
    }
    Ok(attempted)
}

/// Environment knob (comma-separated) that extends the ctl agent allowlist
/// beyond [`bind::AGENT_STEMS`]. Opt-in on top of `--allow-ctl` and set by the
/// human who launches amux, so it never weakens the confused-agent guard for a
/// session that did not ask for it. Empty/unset ⇒ agents-only (`{claude}`).
pub const ENV_ALLOW: &str = "AMUX_CTL_ALLOW";

/// Read [`ENV_ALLOW`] into the extra-allow list (trimmed, empties dropped).
pub fn extra_allow_from_env() -> Vec<String> {
    std::env::var(ENV_ALLOW)
        .ok()
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Pull amux's own ctl meta-flags off the front of the (already identity-
/// stripped) argument vector, before the hosted command begins — exactly the way
/// [`crate::spawn::parse`] pulls `-n`/`--grid`. Returns `(allow_ctl, max_depth,
/// rest)`, where `rest` is the untouched remainder (grid flags + hosted command).
///
/// * `--allow-ctl` — opt in to the control channel (off by default).
/// * `--max-depth <N>` — the recursion guard ceiling (default
///   [`DEFAULT_MAX_DEPTH`]); `0` means unlimited (the guard is removed).
///
/// Parsing stops at the first non-flag token, so a `--max-depth` the hosted
/// program takes is never eaten. A bad `--max-depth` value is a clear error.
pub fn parse_flags(args: &[String]) -> Result<(bool, usize, Vec<String>), String> {
    let mut allow = false;
    let mut max_depth = DEFAULT_MAX_DEPTH;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--allow-ctl" => {
                allow = true;
                i += 1;
            }
            "--max-depth" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| "--max-depth needs a value (0 = unlimited)".to_string())?;
                max_depth = parse_depth(val)?;
                i += 2;
            }
            s if s.starts_with("--max-depth=") => {
                max_depth = parse_depth(&s["--max-depth=".len()..])?;
                i += 1;
            }
            _ => return Ok((allow, effective_depth(max_depth), args[i..].to_vec())),
        }
    }
    Ok((allow, effective_depth(max_depth), Vec::new()))
}

fn parse_depth(val: &str) -> Result<usize, String> {
    val.parse::<usize>()
        .map_err(|_| "--max-depth must be a non-negative integer (0 = unlimited)".to_string())
}

/// `0` from the user means "no limit"; represent it as the max so the guard in
/// [`evaluate_spawn`] can never trip.
fn effective_depth(d: usize) -> usize {
    if d == 0 {
        usize::MAX
    } else {
        d
    }
}

/// Parse a request line (one JSON object) into a [`Request`], or a clear error.
pub fn parse_request(line: &str) -> Result<Request, String> {
    let v = json::parse(line).map_err(|e| format!("bad request json: {e}"))?;
    let caller = v.get("caller").and_then(Value::as_i64).map(|n| n as usize);
    let cmd = match v.get("cmd").and_then(Value::as_str) {
        Some("spawn") => {
            let argv = match v.get("argv").and_then(Value::as_array) {
                Some(items) => items
                    .iter()
                    .map(|it| it.as_str().map(str::to_string))
                    .collect::<Option<Vec<String>>>()
                    .ok_or_else(|| "argv must be an array of strings".to_string())?,
                None => return Err("spawn needs an argv array".to_string()),
            };
            let role = v.get("role").and_then(Value::as_str).map(str::to_string);
            let new_window = v.get("window").and_then(Value::as_bool).unwrap_or(true);
            Cmd::Spawn(SpawnReq {
                role,
                argv,
                new_window,
            })
        }
        Some("list") => Cmd::List,
        Some(other) => return Err(format!("unknown command {other:?}")),
        None => return Err("request has no \"cmd\"".to_string()),
    };
    Ok(Request { caller, cmd })
}

// ---- reply builders (compact JSON via `Value`'s Display) ------------------

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn i(n: usize) -> Value {
    Value::Number(Number::Int(n as i64))
}

/// `{"ok":false,"err":"<msg>"}`
pub fn reply_err(msg: &str) -> String {
    obj(vec![("ok", Value::Bool(false)), ("err", s(msg))]).to_string()
}

/// `{"ok":true,"pane":<id>,"role":<role|null>,"session":<sid|null>}`
pub fn reply_spawned(pane: usize, role: Option<&str>, session: Option<&str>) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("pane", i(pane)),
        ("role", role.map(s).unwrap_or(Value::Null)),
        ("session", session.map(s).unwrap_or(Value::Null)),
    ])
    .to_string()
}

/// One node in the org chart, as the run loop knows it.
pub struct TreeNode<'a> {
    pub id: usize,
    pub parent: Option<usize>,
    pub role: Option<&'a str>,
    pub title: &'a str,
    pub depth: usize,
    pub status: Option<&'a str>,
}

/// `{"ok":true,"tree":[{id,parent,role,title,depth,status}, …]}`
pub fn reply_list(nodes: &[TreeNode]) -> String {
    let arr = nodes
        .iter()
        .map(|n| {
            obj(vec![
                ("id", i(n.id)),
                ("parent", n.parent.map(i).unwrap_or(Value::Null)),
                ("role", n.role.map(s).unwrap_or(Value::Null)),
                ("title", s(n.title)),
                ("depth", i(n.depth)),
                ("status", n.status.map(s).unwrap_or(Value::Null)),
            ])
        })
        .collect();
    obj(vec![("ok", Value::Bool(true)), ("tree", Value::Array(arr))]).to_string()
}

// ---- client --------------------------------------------------------------

/// `amux ctl <cmd> …`: build a request from argv, send it to `AMUX_CTL`, print
/// the reply. Exit code reflects the reply's `ok`.
pub fn ctl_cmd(args: &[String]) -> ExitCode {
    let Some(address) = std::env::var(ENV_ADDRESS).ok().filter(|a| !a.is_empty()) else {
        eprintln!(
            "amux ctl: not inside a ctl-enabled amux session ({ENV_ADDRESS} unset).\n\
             Start amux with `--allow-ctl` and run `amux ctl` from one of its panes."
        );
        return ExitCode::FAILURE;
    };
    let caller = std::env::var(ENV_PANE)
        .ok()
        .and_then(|p| p.parse::<usize>().ok());

    let request = match build_request(args, caller) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("amux ctl: {msg}");
            eprintln!("usage: amux ctl spawn [--role R] [-- <cmd...>] | amux ctl list");
            return ExitCode::FAILURE;
        }
    };

    match crate::ipc::request(&address, &request) {
        Ok(reply) => {
            println!("{reply}");
            let ok = json::parse(&reply)
                .ok()
                .and_then(|v| v.get("ok").and_then(json::Value::as_bool))
                .unwrap_or(false);
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("amux ctl: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Turn `amux ctl` argv (after the `ctl` word) + caller id into a JSON request
/// line. Pure and testable. Grammar:
///   `spawn [--role R] [--here | --window] [-- <cmd...>]`
///   `list`
pub fn build_request(args: &[String], caller: Option<usize>) -> Result<String, String> {
    let mut pairs: Vec<(&str, Value)> = Vec::new();
    if let Some(c) = caller {
        pairs.push(("caller", Value::Number(Number::Int(c as i64))));
    }
    match args.first().map(String::as_str) {
        Some("spawn") => {
            pairs.push(("cmd", s("spawn")));
            let mut role: Option<String> = None;
            let mut new_window = true;
            let mut argv: Vec<String> = Vec::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--role" => {
                        role = Some(
                            args.get(i + 1)
                                .cloned()
                                .ok_or_else(|| "--role needs a value".to_string())?,
                        );
                        i += 2;
                    }
                    "--here" => {
                        new_window = false;
                        i += 1;
                    }
                    "--window" => {
                        new_window = true;
                        i += 1;
                    }
                    "--" => {
                        argv = args[i + 1..].to_vec();
                        break;
                    }
                    other => return Err(format!("unexpected argument {other:?} (use `-- <cmd>`)")),
                }
            }
            if argv.is_empty() {
                return Err("spawn needs a command after `--` (e.g. `-- claude`)".to_string());
            }
            if let Some(r) = role {
                pairs.push(("role", Value::String(r)));
            }
            pairs.push(("window", Value::Bool(new_window)));
            pairs.push((
                "argv",
                Value::Array(argv.into_iter().map(Value::String).collect()),
            ));
        }
        Some("list") => {
            pairs.push(("cmd", s("list")));
        }
        Some(other) => return Err(format!("unknown subcommand {other:?}")),
        None => return Err("needs a subcommand: spawn | list".to_string()),
    }
    Ok(obj(pairs).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn allowlisted_agent_at_shallow_depth_is_allowed() {
        assert_eq!(evaluate_spawn(&v(&["claude"]), 0, 6, &[]), Ok(1));
        assert_eq!(
            evaluate_spawn(&v(&["claude", "--continue"]), 2, 6, &[]),
            Ok(3)
        );
    }

    #[test]
    fn non_allowlisted_command_is_refused() {
        assert_eq!(
            evaluate_spawn(&v(&["rm", "-rf", "/"]), 0, 6, &[]),
            Err(SpawnDenied::NotAllowed("rm".to_string()))
        );
    }

    #[test]
    fn extra_allow_admits_an_operator_approved_stem() {
        // `sh` is not a built-in agent, but AMUX_CTL_ALLOW=sh lets it spawn.
        assert_eq!(
            evaluate_spawn(&v(&["sh"]), 0, 6, &["sh".to_string()]),
            Ok(1)
        );
        // …and only the approved one; a different command is still refused.
        assert_eq!(
            evaluate_spawn(&v(&["bash"]), 0, 6, &["sh".to_string()]),
            Err(SpawnDenied::NotAllowed("bash".to_string()))
        );
    }

    #[test]
    fn path_qualified_agent_matches_by_stem() {
        // A full path to claude still resolves to the "claude" stem.
        assert_eq!(evaluate_spawn(&v(&["/usr/bin/claude"]), 0, 6, &[]), Ok(1));
    }

    #[test]
    fn depth_over_the_cap_is_refused() {
        assert_eq!(
            evaluate_spawn(&v(&["claude"]), 6, 6, &[]),
            Err(SpawnDenied::DepthExceeded {
                attempted: 7,
                max: 6
            })
        );
    }

    #[test]
    fn empty_command_is_refused() {
        assert_eq!(
            evaluate_spawn(&[], 0, 6, &[]),
            Err(SpawnDenied::EmptyCommand)
        );
    }

    #[test]
    fn build_spawn_request_roundtrips_through_parse() {
        let line =
            build_request(&v(&["spawn", "--role", "dev_1", "--", "claude"]), Some(0)).unwrap();
        let req = parse_request(&line).unwrap();
        assert_eq!(req.caller, Some(0));
        match req.cmd {
            Cmd::Spawn(sp) => {
                assert_eq!(sp.role.as_deref(), Some("dev_1"));
                assert_eq!(sp.argv, v(&["claude"]));
                assert!(sp.new_window);
            }
            _ => panic!("expected spawn"),
        }
    }

    #[test]
    fn build_here_sets_window_false() {
        let line = build_request(&v(&["spawn", "--here", "--", "claude"]), None).unwrap();
        let req = parse_request(&line).unwrap();
        match req.cmd {
            Cmd::Spawn(sp) => assert!(!sp.new_window),
            _ => panic!("expected spawn"),
        }
    }

    #[test]
    fn build_list_request() {
        let line = build_request(&v(&["list"]), Some(3)).unwrap();
        let req = parse_request(&line).unwrap();
        assert_eq!(req.caller, Some(3));
        assert_eq!(req.cmd, Cmd::List);
    }

    #[test]
    fn spawn_without_command_is_an_error() {
        let err = build_request(&v(&["spawn", "--role", "x"]), None).unwrap_err();
        assert!(err.contains("needs a command"), "{err}");
    }

    #[test]
    fn parse_rejects_unknown_command() {
        let err = parse_request(r#"{"cmd":"frobnicate"}"#).unwrap_err();
        assert!(err.contains("unknown command"), "{err}");
    }

    #[test]
    fn flags_default_off_and_pass_command_through() {
        let (allow, depth, rest) = parse_flags(&v(&["claude", "--continue"])).unwrap();
        assert!(!allow);
        assert_eq!(depth, DEFAULT_MAX_DEPTH);
        assert_eq!(rest, v(&["claude", "--continue"]));
    }

    #[test]
    fn flags_allow_ctl_and_max_depth() {
        let (allow, depth, rest) =
            parse_flags(&v(&["--allow-ctl", "--max-depth", "3", "claude"])).unwrap();
        assert!(allow);
        assert_eq!(depth, 3);
        assert_eq!(rest, v(&["claude"]));
    }

    #[test]
    fn flags_max_depth_zero_is_unlimited() {
        let (_, depth, _) =
            parse_flags(&v(&["--allow-ctl", "--max-depth", "0", "claude"])).unwrap();
        assert_eq!(depth, usize::MAX);
    }

    #[test]
    fn flags_stop_at_command_so_child_keeps_its_flags() {
        // A `--max-depth` after the command belongs to the child, untouched.
        let (allow, _, rest) =
            parse_flags(&v(&["--allow-ctl", "claude", "--max-depth", "9"])).unwrap();
        assert!(allow);
        assert_eq!(rest, v(&["claude", "--max-depth", "9"]));
    }

    #[test]
    fn flags_bad_max_depth_errors() {
        let err = parse_flags(&v(&["--max-depth", "lots"])).unwrap_err();
        assert!(err.contains("--max-depth"), "{err}");
    }

    #[test]
    fn reply_builders_are_valid_json() {
        let spawned = reply_spawned(3, Some("dev_1"), Some("abc-123"));
        let v = json::parse(&spawned).unwrap();
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
        assert_eq!(v.get("pane").and_then(Value::as_i64), Some(3));

        let nodes = [TreeNode {
            id: 0,
            parent: None,
            role: Some("ceo"),
            title: "claude",
            depth: 0,
            status: Some("working"),
        }];
        let listed = reply_list(&nodes);
        let v = json::parse(&listed).unwrap();
        assert_eq!(
            v.get("tree").and_then(Value::as_array).map(<[_]>::len),
            Some(1)
        );

        let err = reply_err("nope");
        let v = json::parse(&err).unwrap();
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(false));
    }
}
