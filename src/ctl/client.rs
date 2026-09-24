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

/// Turn `atrium ctl` argv (after the `ctl` word) + caller id into a JSON request
/// line. Pure and testable. A router: `caller` and `token` first, then the
/// subcommand's own builder below, each of which documents its grammar.
pub fn build_request(args: &[String], caller: Option<usize>) -> Result<String, String> {
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
        Some(other) => return Err(format!("unknown subcommand {other:?}")),
        None => {
            return Err(
                "needs a subcommand: spawn | list | send | status | kill | audit | board | bus | respawn"
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
