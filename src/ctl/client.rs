//! The `atrium ctl` client: argv to a request, send it, print the reply.

use super::render::{render_board, render_bus};
use super::reply::{obj, s};
use super::{zero_sub_warning, TrustMode, ENV_ADDRESS, ENV_PANE, ENV_TOKEN, MAX_TTL_MS};
use json::{Number, Value};
use std::process::ExitCode;

// ---- client --------------------------------------------------------------

/// `atrium ctl <cmd> …`: build a request from argv, send it to `ATRIUM_CTL`, print
/// the reply. Exit code reflects the reply's `ok`.
pub fn ctl_cmd(args: &[String]) -> ExitCode {
    // Help needs no session: `atrium ctl --help` answers from anywhere, and a
    // bare `atrium ctl` shows the same text as a usage error.
    if args.iter().any(|a| crate::help::wants(a)) {
        print!("{}", crate::help::CTL);
        return ExitCode::SUCCESS;
    }
    if args.is_empty() {
        eprintln!("atrium ctl: needs a command");
        eprint!("{}", crate::help::CTL);
        return ExitCode::FAILURE;
    }
    let Some(address) = std::env::var(ENV_ADDRESS).ok().filter(|a| !a.is_empty()) else {
        eprintln!(
            "atrium ctl: not inside a ctl-enabled atrium session ({ENV_ADDRESS} unset).\n\
             Start atrium with `--allow-ctl` and run `atrium ctl` from one of its panes."
        );
        return ExitCode::FAILURE;
    };
    let caller = std::env::var(ENV_PANE)
        .ok()
        .and_then(|p| p.parse::<usize>().ok());

    // `--json` prints the raw reply (for scripting); `--no-color` forces plain
    // text even on a TTY. Otherwise board/bus views render with SGR colors.
    let raw_json = args.iter().any(|a| a == "--json");
    let no_color = args.iter().any(|a| a == "--no-color");
    let filtered: Vec<String> = args
        .iter()
        .filter(|a| a.as_str() != "--json" && a.as_str() != "--no-color")
        .cloned()
        .collect();
    let args = &filtered[..];
    let color = !no_color && should_color();

    // `wait` is a client-side loop over `answer`/`status`: the server stays
    // one request, one reply, and a waiter that dies takes nothing with it.
    if args.first().map(String::as_str) == Some("wait") {
        return match parse_wait(args) {
            Ok(spec) => wait_cmd(&address, caller, &spec),
            Err(msg) => {
                eprintln!("atrium ctl: {msg}");
                eprint!("{}", crate::help::CTL);
                ExitCode::FAILURE
            }
        };
    }

    let request = match build_request(args, caller) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("atrium ctl: {msg}");
            eprint!("{}", crate::help::CTL);
            return ExitCode::FAILURE;
        }
    };

    match crate::ipc::request(&address, &request) {
        Ok(reply) => {
            // A `board` result renders as a table (unless --json); everything else
            // prints its JSON reply verbatim.
            match (args.first().map(String::as_str), raw_json) {
                (Some("board"), false) => match render_board(&reply, color) {
                    Some(view) => println!("{view}"),
                    None => println!("{reply}"),
                },
                (Some("bus"), false) => match render_bus(&reply, color) {
                    Some(view) => println!("{view}"),
                    None => println!("{reply}"),
                },
                _ => println!("{reply}"),
            }
            // A non-fatal advisory to STDERR (never stdout, which carries the JSON
            // reply): a `bus pub` that reached zero subscribers. Fires in both the
            // human and `--json` paths — stdout stays clean for `json::parse`.
            if let Some(warning) = zero_sub_warning(&reply) {
                eprintln!("atrium ctl: {warning}");
            }
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
            eprintln!("atrium ctl: {e}");
            ExitCode::FAILURE
        }
    }
}

/// What `wait` waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitFor {
    /// A reported answer with a seq past `after`.
    Answer,
    /// The pane at its prompt, idle, ended or errored: nothing running.
    Idle,
    /// The pane gone (its `status` refused: no such pane).
    Exit,
}

/// A parsed `wait <target> [--for answer|idle|exit] [--timeout <secs>] [--after <seq>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitSpec {
    pub target: String,
    pub what: WaitFor,
    pub timeout_s: u64,
    /// For `answer`: the seq already read; the wait ends on a newer one.
    pub after: u64,
}

/// The default `--timeout`, in seconds.
pub const WAIT_DEFAULT_TIMEOUT_S: u64 = 300;
/// The longest `--timeout` accepted, in seconds: one hour.
pub const WAIT_MAX_TIMEOUT_S: u64 = 3600;
/// How often `wait` asks again.
const WAIT_POLL_MS: u64 = 500;

/// Parse a `wait` argv. Pure and unit-tested.
pub fn parse_wait(args: &[String]) -> Result<WaitSpec, String> {
    let target = value_at(args, 1, "wait needs a target (pane id or role)")?.clone();
    let mut spec = WaitSpec {
        target,
        what: WaitFor::Answer,
        timeout_s: WAIT_DEFAULT_TIMEOUT_S,
        after: 0,
    };
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--for" => {
                let w = value_at(args, i + 1, "--for needs answer, idle or exit")?;
                spec.what = match w.as_str() {
                    "answer" => WaitFor::Answer,
                    "idle" => WaitFor::Idle,
                    "exit" => WaitFor::Exit,
                    other => {
                        return Err(format!(
                            "--for: unknown {other:?} (use answer, idle or exit)"
                        ))
                    }
                };
                i += 2;
            }
            "--timeout" => {
                let t = value_at(args, i + 1, "--timeout needs a number of seconds")?;
                spec.timeout_s = parse_whole(t)
                    .map_err(|()| "--timeout must be a whole number of seconds".to_string())?
                    .filter(|n| *n >= 1 && *n <= WAIT_MAX_TIMEOUT_S)
                    .ok_or_else(|| {
                        format!("--timeout {t} must be 1..{WAIT_MAX_TIMEOUT_S} seconds")
                    })?;
                i += 2;
            }
            "--after" => {
                let a = value_at(args, i + 1, "--after needs a seq")?;
                spec.after = parse_whole(a)
                    .map_err(|()| "--after must be a whole number".to_string())?
                    .ok_or_else(|| format!("--after {a} is too large"))?;
                i += 2;
            }
            other => return Err(format!("wait: unexpected argument {other:?}")),
        }
    }
    Ok(spec)
}

/// Does `reply` (one `answer` or `status` reply, as JSON text) end the wait?
/// Pure: the loop is tested through this.
pub fn wait_done(spec: &WaitSpec, reply: &str) -> bool {
    let Ok(v) = json::parse(reply) else {
        return false;
    };
    let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
    match spec.what {
        WaitFor::Answer => {
            ok && v
                .get("seq")
                .and_then(Value::as_i64)
                .is_some_and(|s| s >= 0 && (s as u64) > spec.after)
        }
        WaitFor::Idle => {
            ok && matches!(
                v.get("status").and_then(Value::as_str),
                Some("waiting-prompt") | Some("idle") | Some("ended") | Some("errored")
            )
        }
        // A refused status for a pane that was there is the exit: the reply
        // names no such pane.
        WaitFor::Exit => !ok,
    }
}

/// The `wait` loop: ask every [`WAIT_POLL_MS`] until [`wait_done`] or the
/// timeout, printing the last reply (or a timeout error) as the result.
fn wait_cmd(address: &str, caller: Option<usize>, spec: &WaitSpec) -> ExitCode {
    let verb = match spec.what {
        WaitFor::Answer => "answer",
        WaitFor::Idle | WaitFor::Exit => "status",
    };
    let request = match build_request(&[verb.to_string(), spec.target.clone()], caller) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("atrium ctl: {e}");
            return ExitCode::FAILURE;
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(spec.timeout_s);
    loop {
        match crate::ipc::request(address, &request) {
            Ok(reply) => {
                if wait_done(spec, &reply) {
                    println!("{reply}");
                    return ExitCode::SUCCESS;
                }
            }
            Err(e) => {
                // The channel itself is gone: for `exit` that is the answer.
                if spec.what == WaitFor::Exit {
                    println!(
                        "{}",
                        super::reply_err(&format!("atrium is gone: {e}")).to_json()
                    );
                    return ExitCode::SUCCESS;
                }
                eprintln!("atrium ctl: {e}");
                return ExitCode::FAILURE;
            }
        }
        if std::time::Instant::now() >= deadline {
            println!(
                "{}",
                super::reply_err(&format!(
                    "timeout after {}s waiting for {} of {}",
                    spec.timeout_s,
                    verb_label(spec.what),
                    spec.target
                ))
                .to_json()
            );
            return ExitCode::FAILURE;
        }
        std::thread::sleep(std::time::Duration::from_millis(WAIT_POLL_MS));
    }
}

fn verb_label(w: WaitFor) -> &'static str {
    match w {
        WaitFor::Answer => "an answer",
        WaitFor::Idle => "idle",
        WaitFor::Exit => "exit",
    }
}

/// True when stdout is a terminal and `NO_COLOR` is not set — the standard
/// condition for emitting SGR color codes. Also gates OSC-8 hyperlinks.
fn should_color() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var("NO_COLOR").is_err()
}

/// Absorb one argv token into a `field=value` list, tolerating **unquoted spaced
/// values**. A token that contains `=` starts a new field (`name` = everything
/// after the first `=`); a token with no `=` is a *continuation* — appended,
/// space-joined, to the current field's value. So the shell-split argv
/// `["msg=merged", "the", "PR", "url=http://x"]` becomes `msg="merged the PR"`,
/// `url="http://x"` without the caller needing to quote. Errors if a continuation
/// arrives before any field has been named.
fn absorb_field_token(fields: &mut Vec<(String, String)>, tok: &str) -> Result<(), String> {
    if let Some((f, val)) = tok.split_once('=') {
        // The payload is an ordered list on the wire: a repeated name would
        // reach the server twice and one would win silently.
        if fields.iter().any(|(have, _)| have == f) {
            return Err(format!("field {f:?} given twice"));
        }
        fields.push((f.to_string(), val.to_string()));
    } else if let Some((_, last)) = fields.last_mut() {
        last.push(' ');
        last.push_str(tok);
    } else {
        return Err(format!("expected field=value, got {tok:?}"));
    }
    Ok(())
}

/// Convert accumulated `(field, value)` string pairs into a JSON object value.
fn fields_to_value(fields: Vec<(String, String)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(f, v)| (f, Value::String(v)))
            .collect(),
    )
}

/// Parse a non-negative whole number, telling "too large" (`Ok(None)`) apart
/// from "not a number" (`Err`), whatever the platform's word size.
fn parse_whole(tok: &str) -> Result<Option<u64>, ()> {
    match tok.parse::<u64>() {
        Ok(n) => Ok(Some(n)),
        Err(e) if *e.kind() == std::num::IntErrorKind::PosOverflow => Ok(None),
        Err(_) => Err(()),
    }
}

/// The token at `idx` — a positional, or a flag's value — unless it is missing
/// or is itself a flag, which is the error `missing`. A flag's value taken
/// blindly swallowed the next flag: `--role --here` set role="--here" and lost
/// the placement.
fn value_at<'a>(args: &'a [String], idx: usize, missing: &str) -> Result<&'a String, String> {
    args.get(idx)
        .filter(|t| !t.starts_with('-'))
        .ok_or_else(|| missing.to_string())
}

/// The JSON request under construction: `(key, value)` pairs in wire order.
type Pairs = Vec<(&'static str, Value)>;

/// Why `atrium ctl` argv could not become a request. Always the caller's usage,
/// never the session's state, which is why `ctl_cmd` answers it with the help
/// text. The message names what was wrong (`"send needs text after the target"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(String);

impl UsageError {
    /// The message, without the `atrium ctl:` prefix.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// Turn `atrium ctl` argv (after the `ctl` word) + caller id into a JSON request
/// line. Pure and testable. A router: `caller` and `token` first, then the
/// subcommand's own builder below, each of which documents its grammar.
pub fn build_request(args: &[String], caller: Option<usize>) -> Result<String, UsageError> {
    build_pairs(args, caller).map_err(UsageError)
}

/// [`build_request`]'s body. The subcommand builders word their errors as plain
/// strings; the public boundary types them once.
fn build_pairs(args: &[String], caller: Option<usize>) -> Result<String, String> {
    let mut pairs: Pairs = Vec::new();
    if let Some(c) = caller {
        let c = i64::try_from(c).map_err(|_| format!("caller id {c} is out of range"))?;
        pairs.push(("caller", Value::Number(Number::Int(c))));
    }
    // The capability token authenticates the caller. It is env-sourced (like the
    // endpoint address and the caller id in `ctl_cmd`); tests, which never set
    // `ATRIUM_TOKEN`, simply send no token and are treated as unauthenticated.
    if let Ok(tok) = std::env::var(ENV_TOKEN) {
        if !tok.is_empty() {
            pairs.push(("token", Value::String(tok)));
        }
    }
    match args.first().map(String::as_str) {
        Some("spawn") => spawn_req(args, &mut pairs)?,
        Some("list") => {
            pairs.push(("cmd", s("list")));
        }
        Some("send") => send_req(args, &mut pairs)?,
        Some("status") => status_req(args, &mut pairs)?,
        Some("kill") => kill_req(args, &mut pairs)?,
        Some("audit") => audit_req(args, &mut pairs)?,
        Some("board") => board_req(args, &mut pairs)?,
        Some("bus") => bus_req(args, &mut pairs)?,
        Some("respawn") => respawn_req(args, &mut pairs)?,
        Some("hello") => hello_req(args, &mut pairs)?,
        Some("report") => report_req(args, &mut pairs)?,
        Some("whoami") => {
            pairs.push(("cmd", s("whoami")));
        }
        Some("answer") => {
            pairs.push(("cmd", s("answer")));
            let target = value_at(args, 1, "answer needs a target (pane id or role)")?;
            pairs.push(("target", Value::String(target.clone())));
        }
        Some(other) => return Err(format!("unknown subcommand {other:?}")),
        None => {
            return Err(
                "needs a subcommand: spawn | list | send | status | kill | audit | board | bus | \
                 respawn | hello | report | whoami | answer | wait"
                    .to_string(),
            )
        }
    }
    Ok(obj(pairs).to_string())
}

/// `spawn [--role R] [--identity I] [--mode M] [--worktree W] [--here | --window] -- <cmd...>`
fn spawn_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("spawn")));
    let mut role: Option<String> = None;
    let mut identity: Option<String> = None;
    let mut new_window = true;
    let mut mode: Option<TrustMode> = None;
    let mut worktree: Option<String> = None;
    let mut argv: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--worktree" => {
                worktree = Some(value_at(args, i + 1, "--worktree needs a name")?.clone());
                i += 2;
            }
            "--mode" => {
                let k = value_at(
                    args,
                    i + 1,
                    "--mode needs a value (plan, accept, or automode)",
                )?;
                mode = Some(TrustMode::from_policy_keyword(k).ok_or_else(|| {
                    format!("--mode: unknown {k:?} (use plan, accept, or automode)")
                })?);
                i += 2;
            }
            "--role" => {
                role = Some(value_at(args, i + 1, "--role needs a value")?.clone());
                i += 2;
            }
            "--identity" => {
                identity = Some(value_at(args, i + 1, "--identity needs a value")?.clone());
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
    if let Some(x) = identity {
        pairs.push(("identity", Value::String(x)));
    }
    if let Some(m) = mode {
        pairs.push(("mode", Value::String(m.policy_label().to_string())));
    }
    if let Some(w) = worktree {
        pairs.push(("worktree", Value::String(w)));
    }
    pairs.push(("window", Value::Bool(new_window)));
    pairs.push((
        "argv",
        Value::Array(argv.into_iter().map(Value::String).collect()),
    ));
    Ok(())
}

/// `send <target> <text...>`
fn send_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("send")));
    let target = value_at(args, 1, "send needs a target (pane id or role)")?;
    let text = args[2..].join(" ");
    if text.trim().is_empty() {
        return Err("send needs text after the target".to_string());
    }
    pairs.push(("target", Value::String(target.clone())));
    pairs.push(("text", Value::String(text)));
    Ok(())
}

/// `status [target]`
fn status_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("status")));
    if let Some(target) = args.get(1).filter(|t| !t.starts_with('-')) {
        pairs.push(("target", Value::String(target.clone())));
    }
    Ok(())
}

/// `kill <target>`
fn kill_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("kill")));
    let target = value_at(args, 1, "kill needs a target (pane id or role)")?;
    pairs.push(("target", Value::String(target.clone())));
    Ok(())
}

/// `hello mod=<version> [engine=<version>] [caps=<a,b,...>]` — a pane's mod
/// announcing itself. Field tokens, like `board set`; `caps` is a comma list.
fn hello_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("hello")));
    let mut fields: Vec<(String, String)> = Vec::new();
    for a in &args[1.min(args.len())..] {
        absorb_field_token(&mut fields, a).map_err(|e| format!("hello: {e}"))?;
    }
    let mut saw_mod = false;
    for (k, val) in fields {
        match k.as_str() {
            "mod" => {
                saw_mod = true;
                pairs.push(("mod", Value::String(val)));
            }
            "engine" => pairs.push(("engine", Value::String(val))),
            "caps" => pairs.push((
                "caps",
                Value::Array(
                    val.split(',')
                        .map(str::trim)
                        .filter(|c| !c.is_empty())
                        .map(|c| Value::String(c.to_string()))
                        .collect(),
                ),
            )),
            other => {
                return Err(format!(
                    "hello: unknown field {other:?} (use mod, engine, caps)"
                ))
            }
        }
    }
    if !saw_mod {
        return Err("hello needs mod=<version>".to_string());
    }
    Ok(())
}

/// `report [status=<s>] [reason=<r>] [context=<pct>] [cost=<usd>] [turns=<n>] [answer=<text...>]`
/// — a pane's mod reporting about itself. Numbers are typed on the wire so the
/// server never parses text it did not ask for.
fn report_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("report")));
    let mut fields: Vec<(String, String)> = Vec::new();
    for a in &args[1.min(args.len())..] {
        absorb_field_token(&mut fields, a).map_err(|e| format!("report: {e}"))?;
    }
    if fields.is_empty() {
        return Err(
            "report needs at least one field=value (status, reason, context, cost, turns, answer)"
                .to_string(),
        );
    }
    for (k, val) in fields {
        match k.as_str() {
            "status" => pairs.push(("status", Value::String(val))),
            "reason" => pairs.push(("reason", Value::String(val))),
            "answer" => pairs.push(("answer", Value::String(val))),
            "context" => {
                let n = parse_whole(&val)
                    .map_err(|()| format!("report: context must be a whole number, got {val:?}"))?
                    .filter(|n| *n <= 100)
                    .ok_or_else(|| format!("report: context must be 0..100, got {val:?}"))?;
                pairs.push(("context", Value::Number(Number::Int(n as i64))));
            }
            "turns" => {
                let n = parse_whole(&val)
                    .map_err(|()| format!("report: turns must be a whole number, got {val:?}"))?
                    .and_then(|n| i64::try_from(n).ok())
                    .ok_or_else(|| format!("report: turns {val} is too large"))?;
                pairs.push(("turns", Value::Number(Number::Int(n))));
            }
            "cost" => {
                let c: f64 = val
                    .trim()
                    .parse()
                    .ok()
                    .filter(|c: &f64| c.is_finite() && *c >= 0.0)
                    .ok_or_else(|| format!("report: cost must be a non-negative number, got {val:?}"))?;
                pairs.push(("cost", Value::Number(Number::Float(c))));
            }
            other => {
                return Err(format!(
                    "report: unknown field {other:?} (use status, reason, context, cost, turns, answer)"
                ))
            }
        }
    }
    Ok(())
}

/// `audit [tail]`
fn audit_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("audit")));
    if let Some(tok) = args.get(1).filter(|t| !t.starts_with('-')) {
        let n = parse_whole(tok)
            .map_err(|()| format!("audit tail must be a number, got {tok:?}"))?
            .and_then(|n| i64::try_from(n).ok())
            .ok_or_else(|| format!("audit tail {tok} is too large"))?;
        pairs.push(("tail", Value::Number(Number::Int(n))));
    }
    Ok(())
}

/// `board set|get|list|del|claim|release …`
fn board_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("board")));
    let sub = args
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| "board needs set|get|list|del|claim|release".to_string())?;
    pairs.push(("op", s(sub)));
    match sub {
        "set" => {
            let key = value_at(args, 2, "board set needs a key")?;
            pairs.push(("key", Value::String(key.clone())));
            // Remaining args are `field=value` pairs (empty value clears);
            // unquoted spaced values are joined across tokens.
            let mut fields: Vec<(String, String)> = Vec::new();
            for a in &args[3.min(args.len())..] {
                absorb_field_token(&mut fields, a).map_err(|e| format!("board set: {e}"))?;
            }
            if fields.is_empty() {
                return Err("board set needs at least one field=value".to_string());
            }
            pairs.push(("fields", fields_to_value(fields)));
        }
        "get" | "del" | "release" | "claim" => {
            let key = value_at(args, 2, &format!("board {sub} needs a key"))?;
            pairs.push(("key", Value::String(key.clone())));
            // `claim` only: an optional `--ttl SECS` overrides the default lease.
            if let Some(pos) = args
                .iter()
                .position(|a| a == "--ttl")
                .filter(|_| sub == "claim")
            {
                let tok = value_at(args, pos + 1, "--ttl needs a value in seconds")?;
                // Capped at MAX_TTL_MS, not just i64: the server adds the
                // lease to now_ms.
                let ms = parse_whole(tok)
                    .map_err(|()| "--ttl must be a whole number of seconds".to_string())?
                    .and_then(|secs| secs.checked_mul(1000))
                    .filter(|ms| *ms <= MAX_TTL_MS)
                    .ok_or_else(|| format!("--ttl {tok} is too large (at most 365 days)"))?;
                pairs.push(("ttl_ms", Value::Number(Number::Int(ms as i64))));
            }
        }
        "list" => {}
        other => {
            return Err(format!(
                "board op must be set|get|list|del|claim|release (got {other:?})"
            ))
        }
    }
    Ok(())
}

/// `bus pub|sub|unsub|feed|resolve|topics …`
fn bus_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("bus")));
    let sub = args
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| "bus needs pub|sub|unsub|feed|resolve".to_string())?;
    pairs.push(("op", s(sub)));
    match sub {
        "pub" => bus_pub(args, pairs)?,
        "sub" | "unsub" => {
            let topics: Vec<Value> = args[2.min(args.len())..]
                .iter()
                .filter(|t| !t.starts_with('-'))
                .map(|t| Value::String(t.clone()))
                .collect();
            if sub == "sub" && topics.is_empty() {
                return Err("bus sub needs at least one topic (or `*` for all)".to_string());
            }
            pairs.push(("topics", Value::Array(topics)));
        }
        "feed" => bus_feed(args, pairs)?,
        "resolve" => {
            let tok = value_at(args, 2, "bus resolve needs a seq")?;
            let seq = tok
                .parse::<i64>()
                .map_err(|_| format!("bus resolve: seq must be a number, got {tok:?}"))?;
            pairs.push(("seq", Value::Number(Number::Int(seq.max(0)))));
        }
        // No arguments: the op token alone (already pushed) is the request.
        "topics" => {}
        other => {
            return Err(format!(
                "bus op must be pub|sub|unsub|feed|resolve|topics (got {other:?})"
            ))
        }
    }
    Ok(())
}

/// `bus pub <topic> [--decision | --kind K] [--new] [--to R] field=value…`
fn bus_pub(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    let topic = value_at(args, 2, "bus pub needs a topic")?;
    pairs.push(("topic", Value::String(topic.clone())));
    // Default FYI; `--decision` (or `--kind K`) escalates. Remaining
    // args are `field=value` pairs (unquoted spaced values are
    // joined across tokens) — the structured payload.
    let mut kind = crate::bus::Kind::Fyi;
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut create = false;
    let mut i = 3.min(args.len());
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--decision" => {
                kind = crate::bus::Kind::DecisionNeeded;
                i += 1;
            }
            "--new" => {
                // Deliberately create a not-yet-seen topic (soft-gate
                // only; a declared fleet rejects off-list regardless).
                create = true;
                i += 1;
            }
            "--to" => {
                // Address the event to a teammate role (e.g. the
                // lead). Sugar for a `to=<role>` field: an
                // agent-addressed decision routes to that agent
                // instead of firing the human's urgent bar.
                let role = value_at(args, i + 1, "--to needs a role (e.g. --to lead)")?;
                fields.push(("to".to_string(), role.clone()));
                i += 2;
            }
            "--kind" => {
                let k = value_at(args, i + 1, "--kind needs fyi or decision_needed")?;
                kind = crate::bus::Kind::from_keyword(k)
                    .ok_or_else(|| format!("--kind: unknown {k:?} (use fyi or decision_needed)"))?;
                i += 2;
            }
            _ => {
                absorb_field_token(&mut fields, a).map_err(|e| format!("bus pub: {e}"))?;
                i += 1;
            }
        }
    }
    if fields.is_empty() {
        return Err("bus pub needs at least one field=value (e.g. msg=merged the PR)".to_string());
    }
    pairs.push(("kind", s(kind.as_str())));
    pairs.push(("fields", fields_to_value(fields)));
    if create {
        pairs.push(("create", Value::Bool(true)));
    }
    Ok(())
}

/// `bus feed [--since N | --since=N]`
fn bus_feed(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    // `--since N` resumes after cursor N; default 0 = from the start. Given at
    // most once: a second one used to send the `since` key twice.
    let mut since: Option<i64> = None;
    let mut i = 2;
    while i < args.len() {
        let n = if args[i] == "--since" {
            i += 2;
            args.get(i - 1)
                .and_then(|t| t.parse::<i64>().ok())
                .ok_or_else(|| "--since needs a number".to_string())?
        } else if let Some(rest) = args[i].strip_prefix("--since=") {
            i += 1;
            rest.parse::<i64>()
                .map_err(|_| "--since needs a number".to_string())?
        } else {
            return Err(format!("unexpected argument {:?} to bus feed", args[i]));
        };
        if since.replace(n).is_some() {
            return Err("--since may be given only once".to_string());
        }
    }
    if let Some(n) = since {
        pairs.push(("since", Value::Number(Number::Int(n.max(0)))));
    }
    Ok(())
}

/// `respawn <target> [--worktree W]`
fn respawn_req(args: &[String], pairs: &mut Pairs) -> Result<(), String> {
    pairs.push(("cmd", s("respawn")));
    let target = value_at(args, 1, "respawn needs a target (pane id or role)")?;
    pairs.push(("target", Value::String(target.clone())));
    if let Some(pos) = args.iter().position(|a| a == "--worktree") {
        let name = value_at(args, pos + 1, "--worktree needs a name")?;
        pairs.push(("worktree", Value::String(name.clone())));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
