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
//! Surface: `spawn` (open a visible worker — new window or `--here` split, under
//! an optionally-delegated `--identity`), `list` (the org chart), `send`
//! (queue-until-idle task delivery), `status` (agsess-backed), `kill` (subtree
//! teardown), and `audit` (the ctl request log). `send`/`status`/`kill`/`audit`
//! with a target are subtree-scoped; credential delegation is scoped by
//! [`delegation_allowed`] (C3).

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

/// The command set (C1: spawn/list; C2: send/status; C3: kill/audit).
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// Open a new worker pane running `argv`, tagged `role`.
    Spawn(SpawnReq),
    /// Report the spawn tree.
    List,
    /// Feed `text` to a target pane's agent as a submitted prompt (queued until
    /// the target is idle).
    Send(SendReq),
    /// Report status: one target, or (no target) the caller's visible subtree.
    Status(StatusReq),
    /// Terminate a target pane **and its whole subtree** (design §5: a kill
    /// tears down descendants, so a lead's `kill` reaps its ICs too).
    Kill(KillReq),
    /// Report the ctl request log (subtree-scoped for a worker), most recent
    /// `tail` entries or all of the in-memory ring.
    Audit(AuditReq),
}

/// A `kill` request's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct KillReq {
    /// Pane id (numeric) or role label at the root of the subtree to tear down.
    pub target: String,
}

/// An `audit` request's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditReq {
    /// Show only the most recent `n` entries; `None` = the whole in-memory ring.
    pub tail: Option<usize>,
}

/// A `send` request's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct SendReq {
    /// Pane id (numeric) or role label to deliver to.
    pub target: String,
    /// The task/prompt text to submit.
    pub text: String,
}

/// A `status` request's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusReq {
    /// A specific pane id/role, or `None` for the caller's subtree roll-up.
    pub target: Option<String>,
}

/// A `spawn` request's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnReq {
    pub role: Option<String>,
    /// The command to host, e.g. `["claude"]`. Must be non-empty and on the
    /// agent allowlist (checked by [`evaluate_spawn`]).
    pub argv: Vec<String>,
    /// Open in a new window (true, the default) vs. `--here` split beside the
    /// caller (false).
    pub new_window: bool,
    /// The credential identity **name** to run the worker under (`--identity X`),
    /// if any. Subject to delegation scoping ([`delegation_allowed`]): a worker
    /// may only pass down an identity it itself holds. `None` = no identity
    /// (ambient env), the default.
    pub identity: Option<String>,
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

/// Permission-posture flags that **amux** owns, not the spawning agent. A hosted
/// agent running `amux ctl spawn -- claude …` must not be able to hand its
/// teammates a stronger posture than the human chose at launch — e.g. appending
/// `--dangerously-skip-permissions` (full bypass) or `--allowedTools "Bash(*)"`
/// (auto-allow everything) when the operator launched in safe `--trust` mode.
/// amux applies the launch posture itself (see `spawn_pane_full`), so these are
/// stripped from agent-supplied argv.
const GOVERNED_FLAGS: [&str; 3] = [
    "--dangerously-skip-permissions",
    "--permission-mode",
    "--allowedTools",
];

/// Strip amux-governed permission flags (and their values) from a ctl-spawn
/// argv, returning `(cleaned, stripped)` where `stripped` names the flags removed
/// (surfaced in the spawn reply's `note` — never silent, per the guardrails).
/// `--dangerously-skip-permissions` is a bare switch; `--permission-mode` and
/// `--allowedTools` each consume the following token as their value. Both
/// `--flag value` and `--flag=value` spellings are handled. Pure.
pub fn sanitize_spawn_argv(argv: &[String]) -> (Vec<String>, Vec<String>) {
    let takes_value = |f: &str| f == "--permission-mode" || f == "--allowedTools";
    let mut cleaned = Vec::with_capacity(argv.len());
    let mut stripped: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        // Match either `--flag` or the `--flag=value` form.
        let head = a.split('=').next().unwrap_or(a);
        if let Some(&flag) = GOVERNED_FLAGS.iter().find(|&&g| g == head) {
            if !stripped.iter().any(|s| s == flag) {
                stripped.push(flag.to_string());
            }
            // Drop a *separate* value token only for `--flag value` (not
            // `--flag=value`, whose value rides along in this same arg).
            if takes_value(flag) && !a.contains('=') && i + 1 < argv.len() {
                i += 1;
            }
            i += 1;
            continue;
        }
        cleaned.push(a.clone());
        i += 1;
    }
    (cleaned, stripped)
}

/// Resolve a `send`/`status` target string to a pane's agent id. A numeric
/// target matches by agent id; otherwise it matches by role label. `candidates`
/// is `(agent_id, role)` for every live pane. Returns a clear error when the
/// target is unknown or a role is ambiguous (matches more than one pane). Pure.
pub fn resolve_target(
    target: &str,
    candidates: &[(usize, Option<String>)],
) -> Result<usize, String> {
    if let Ok(id) = target.parse::<usize>() {
        if candidates.iter().any(|(cid, _)| *cid == id) {
            return Ok(id);
        }
        return Err(format!("no pane with id {id}"));
    }
    let hits: Vec<usize> = candidates
        .iter()
        .filter(|(_, role)| role.as_deref() == Some(target))
        .map(|(cid, _)| *cid)
        .collect();
    match hits.as_slice() {
        [] => Err(format!("no pane with id or role {target:?}")),
        [one] => Ok(*one),
        many => Err(format!(
            "role {target:?} is ambiguous ({} panes: {}); use a pane id",
            many.len(),
            many.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Is `target` inside the subtree rooted at `root` (i.e. `root` itself or a
/// descendant of it)? `parents` maps each `agent_id` to its parent. This is the
/// **subtree-scoping guard** (Decision 3): a non-privileged caller may only
/// `send`/`status` panes in its own subtree. Pure; cycle-guarded.
pub fn in_subtree(target: usize, root: usize, parents: &[(usize, Option<usize>)]) -> bool {
    if target == root {
        return true;
    }
    let parent_of = |id: usize| parents.iter().find(|(i, _)| *i == id).and_then(|(_, p)| *p);
    let mut cur = target;
    // The tree is shallow (depth-capped) but guard against a malformed cycle.
    for _ in 0..4096 {
        match parent_of(cur) {
            Some(p) if p == root => return true,
            Some(p) => cur = p,
            None => return false,
        }
    }
    false
}

/// The **credential-delegation guard** (design §5): which identity may a
/// `ctl spawn` run its worker under? A spawned worker (non-privileged caller)
/// may only delegate an identity it itself holds — its **own** identity, or the
/// **session/fleet default** amux launched with — so an IC can't mint itself
/// `wif:prod` that its lead was never granted. The **operator** (human root,
/// `privileged`) is the trust root and may delegate any identity in the vault.
/// Requesting `None` (no identity) is always allowed. Pure; unit-tested.
///
/// * `requested` — the `--identity` name the spawn asked for (or `None`).
/// * `caller_identity` — the requesting pane's own identity name.
/// * `session_default` — the identity amux itself was launched under.
pub fn delegation_allowed(
    requested: Option<&str>,
    caller_identity: Option<&str>,
    session_default: Option<&str>,
    privileged: bool,
) -> Result<(), String> {
    let Some(req) = requested else {
        return Ok(()); // no identity delegated — nothing to escalate
    };
    if privileged || Some(req) == caller_identity || Some(req) == session_default {
        return Ok(());
    }
    Err(format!(
        "identity {req:?} is not yours to delegate; a worker may only pass down \
         its own identity or the session default"
    ))
}

/// Environment knob naming a file to append the ctl audit log to as JSONL, one
/// object per request (opt-in, on top of `--allow-ctl`; design §5 "optionally
/// on-disk"). Unset ⇒ the log is kept in memory only, readable via `ctl audit`.
/// Only identity **names** are ever written — never resolved secret values.
pub const ENV_AUDIT: &str = "AMUX_CTL_AUDIT";

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

/// How much amux relaxes the permission posture of the agent panes it spawns.
/// Two opt-in levels, deliberately separate so the safe one is the easy one and
/// the dangerous one is a conscious choice:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustMode {
    /// Default: no relaxation. claude decides per its own settings; amux does not
    /// touch the folder-trust dialog or the permission mode.
    Off,
    /// `--trust`: **auto-accept edits + a safe dev-command allowlist.** Agents
    /// edit and run the build/test loop hands-off, but a command outside the
    /// allowlist (e.g. `curl`, `git push`, `rm` outside the workdir) still
    /// surfaces as a **visible approval prompt** in its pane. Also pre-accepts
    /// the folder-trust dialog. The safe hands-off default.
    Edits,
    /// `--skip-permissions`: **full bypass** (`--dangerously-skip-permissions`).
    /// Every command runs with no gate at all. Genuinely dangerous — amux
    /// requires an explicit launch confirmation before using it.
    Skip,
}

/// Pull amux's own launch meta-flags off the front of the (already identity-
/// stripped) argument vector, before the hosted command begins — exactly the way
/// [`crate::spawn::parse`] pulls `-n`/`--grid`. Returns `(allow_ctl, max_depth,
/// trust, rest)`, where `rest` is the untouched remainder (grid flags + hosted
/// command).
///
/// * `--allow-ctl` — opt in to the control channel (off by default).
/// * `--max-depth <N>` — the recursion guard ceiling (default
///   [`DEFAULT_MAX_DEPTH`]); `0` means unlimited (the guard is removed).
/// * `--trust` — [`TrustMode::Edits`]: auto-accept-edits + safe allowlist, the
///   safe hands-off default (dangerous commands still prompt, visibly).
/// * `--skip-permissions` — [`TrustMode::Skip`]: full `--dangerously-skip-
///   permissions` bypass. Mutually exclusive with `--trust`; amux confirms it at
///   launch (§`main`).
///
/// Parsing stops at the first non-flag token, so a flag the hosted program takes
/// is never eaten. A bad `--max-depth` value, or both trust flags at once, is a
/// clear error.
pub fn parse_flags(args: &[String]) -> Result<(bool, usize, TrustMode, Vec<String>), String> {
    let mut allow = false;
    let mut max_depth = DEFAULT_MAX_DEPTH;
    let mut trust = TrustMode::Off;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--allow-ctl" => {
                allow = true;
                i += 1;
            }
            "--trust" => {
                if trust == TrustMode::Skip {
                    return Err("use --trust or --skip-permissions, not both".to_string());
                }
                trust = TrustMode::Edits;
                i += 1;
            }
            "--skip-permissions" => {
                if trust == TrustMode::Edits {
                    return Err("use --trust or --skip-permissions, not both".to_string());
                }
                trust = TrustMode::Skip;
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
            _ => return Ok((allow, effective_depth(max_depth), trust, args[i..].to_vec())),
        }
    }
    Ok((allow, effective_depth(max_depth), trust, Vec::new()))
}

/// The claude flag `--skip-permissions` injects into every agent pane: skips the
/// workspace trust dialog and **all** permission prompts (full bypass).
pub const SKIP_PERMISSIONS_FLAG: &str = "--dangerously-skip-permissions";

/// System-prompt directive amux appends (via `--append-system-prompt`) to every
/// **agent** pane it launches **when the ctl channel is live** — so a hosted
/// agent reliably knows to delegate through `ctl` (visible panes) instead of its
/// own invisible Task/background-agents tool. This is the dependable layer: it is
/// always in the agent's context for a ctl session, so it doesn't rely on a skill
/// happening to surface. The agent applies it against whatever the human types —
/// delegate on "coordinate a team", stay solo otherwise.
// IMPORTANT: this string is passed to claude via `--append-system-prompt` on the
// command line, which on Windows goes through a `cmd /C claude.cmd ...` shim.
// It must contain NO shell-special characters — no quotes, backticks, angle
// brackets, parens, &, |, %, ^ — or the shim's quoting breaks and the agent pane
// dies on launch. Keep it plain prose (letters, spaces, commas, periods, colons,
// hyphens) so it survives the round-trip intact.
pub const AGENT_CTL_DIRECTIVE: &str = "You are running inside amux, a terminal \
multiplexer with a live control plane whose endpoint is in the AMUX_CTL \
environment variable. When a task calls for delegating to teammates or \
coordinating a team, delegate by running the shell command amux ctl spawn \
--role NAME -- claude, then amux ctl send NAME followed by a self-contained \
subtask, and track them with amux ctl status and reap them with amux ctl kill. \
Placement: add --here to a spawn to tile the teammate beside you in one shared \
view, or --window for a separate window, and follow the human preference on \
layout. Spawn each teammate as plain claude with NO permission flags: amux \
already applies the human chosen trust mode to every pane it launches, so never \
add --dangerously-skip-permissions or --permission-mode yourself. \
Each teammate is a VISIBLE amux pane the human can watch, steer, and \
take over. \
Do NOT use your Task tool or background agents to delegate, since those run \
invisibly and defeat the purpose of amux. Run amux ctl with no arguments for the \
full command surface, or use the amux-coordinate or amux-delegate skills for the \
full workflow.";

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
            let identity = v.get("identity").and_then(Value::as_str).map(str::to_string);
            Cmd::Spawn(SpawnReq {
                role,
                argv,
                new_window,
                identity,
            })
        }
        Some("list") => Cmd::List,
        Some("send") => {
            let target = v
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| "send needs a target".to_string())?
                .to_string();
            let text = v
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "send needs text".to_string())?
                .to_string();
            Cmd::Send(SendReq { target, text })
        }
        Some("status") => {
            let target = v.get("target").and_then(Value::as_str).map(str::to_string);
            Cmd::Status(StatusReq { target })
        }
        Some("kill") => {
            let target = v
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| "kill needs a target".to_string())?
                .to_string();
            Cmd::Kill(KillReq { target })
        }
        Some("audit") => {
            let tail = v
                .get("tail")
                .and_then(Value::as_i64)
                .map(|n| n.max(0) as usize);
            Cmd::Audit(AuditReq { tail })
        }
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

/// `{"ok":true,"pane":<id>,"role":<role|null>,"session":<sid|null>,"note":<note|null>}`.
/// `note` carries a non-fatal advisory (e.g. permission flags amux stripped from
/// the spawn argv) so the caller sees it instead of it happening silently.
pub fn reply_spawned(
    pane: usize,
    role: Option<&str>,
    session: Option<&str>,
    note: Option<&str>,
) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("pane", i(pane)),
        ("role", role.map(s).unwrap_or(Value::Null)),
        ("session", session.map(s).unwrap_or(Value::Null)),
        ("note", note.map(s).unwrap_or(Value::Null)),
    ])
    .to_string()
}

/// `{"ok":true,"target":<id>,"queued":<bool>}` — a `send` was accepted. `queued`
/// is true when the target was busy (delivery waits for it to go idle), false
/// when it will go out immediately. Delivery itself is asynchronous.
pub fn reply_sent(target: usize, queued: bool) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("target", i(target)),
        ("queued", Value::Bool(queued)),
    ])
    .to_string()
}

/// `{"ok":true,"killed":[<id>,…]}` — the set of panes torn down by a `kill`
/// (the target plus every descendant), sorted. Empty only if the target
/// vanished between resolve and teardown.
pub fn reply_killed(killed: &[usize]) -> String {
    let arr = killed.iter().map(|id| i(*id)).collect();
    obj(vec![("ok", Value::Bool(true)), ("killed", Value::Array(arr))]).to_string()
}

/// `{"ok":true,"audit":[<entry>,…]}` — the (already-serialized, already-scoped)
/// audit entries, oldest-first. The caller builds each entry `Value` from its
/// own log so this crate stays free of the log's storage type.
pub fn reply_audit(entries: Vec<Value>) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("audit", Value::Array(entries)),
    ])
    .to_string()
}

/// `{"ok":true,"pane":<id>,"status":"<label>"}` — a single target's status.
pub fn reply_status_one(pane: usize, status: Option<&str>) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("pane", i(pane)),
        ("status", status.map(s).unwrap_or(Value::Null)),
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
            eprintln!(
                "usage: amux ctl spawn [--role R] [--identity X] [--here] -- <cmd...>\n\
                 \x20      | list | send <target> <text> | status [target] | kill <target> | audit [N]"
            );
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
            let mut identity: Option<String> = None;
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
                    "--identity" => {
                        identity = Some(
                            args.get(i + 1)
                                .cloned()
                                .ok_or_else(|| "--identity needs a value".to_string())?,
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
            if let Some(x) = identity {
                pairs.push(("identity", Value::String(x)));
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
        Some("send") => {
            pairs.push(("cmd", s("send")));
            let target = args
                .get(1)
                .filter(|t| !t.starts_with('-'))
                .ok_or_else(|| "send needs a target (pane id or role)".to_string())?;
            let text = args[2..].join(" ");
            if text.trim().is_empty() {
                return Err("send needs text after the target".to_string());
            }
            pairs.push(("target", Value::String(target.clone())));
            pairs.push(("text", Value::String(text)));
        }
        Some("status") => {
            pairs.push(("cmd", s("status")));
            if let Some(target) = args.get(1).filter(|t| !t.starts_with('-')) {
                pairs.push(("target", Value::String(target.clone())));
            }
        }
        Some("kill") => {
            pairs.push(("cmd", s("kill")));
            let target = args
                .get(1)
                .filter(|t| !t.starts_with('-'))
                .ok_or_else(|| "kill needs a target (pane id or role)".to_string())?;
            pairs.push(("target", Value::String(target.clone())));
        }
        Some("audit") => {
            pairs.push(("cmd", s("audit")));
            if let Some(tok) = args.get(1).filter(|t| !t.starts_with('-')) {
                let n = tok
                    .parse::<usize>()
                    .map_err(|_| format!("audit tail must be a number, got {tok:?}"))?;
                pairs.push(("tail", Value::Number(Number::Int(n as i64))));
            }
        }
        Some(other) => return Err(format!("unknown subcommand {other:?}")),
        None => {
            return Err(
                "needs a subcommand: spawn | list | send | status | kill | audit".to_string(),
            )
        }
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
    fn sanitize_strips_dangerous_bypass_flag() {
        let (clean, stripped) = sanitize_spawn_argv(&v(&["claude", "--dangerously-skip-permissions"]));
        assert_eq!(clean, v(&["claude"]));
        assert_eq!(stripped, v(&["--dangerously-skip-permissions"]));
    }

    #[test]
    fn sanitize_strips_permission_mode_and_its_value() {
        // `--flag value` form: both the flag and its value token go.
        let (clean, stripped) =
            sanitize_spawn_argv(&v(&["claude", "--permission-mode", "bypassPermissions", "-r"]));
        assert_eq!(clean, v(&["claude", "-r"]));
        assert_eq!(stripped, v(&["--permission-mode"]));
    }

    #[test]
    fn sanitize_strips_allowedtools_including_eq_form() {
        // `--flag=value` form: the value rides in the same arg, no extra token.
        let (clean, stripped) = sanitize_spawn_argv(&v(&["claude", "--allowedTools=Bash(*)"]));
        assert_eq!(clean, v(&["claude"]));
        assert_eq!(stripped, v(&["--allowedTools"]));
    }

    #[test]
    fn sanitize_leaves_benign_argv_untouched() {
        let (clean, stripped) = sanitize_spawn_argv(&v(&["claude", "--continue", "--model", "opus"]));
        assert_eq!(clean, v(&["claude", "--continue", "--model", "opus"]));
        assert!(stripped.is_empty());
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
    fn build_and_parse_send_request() {
        let line = build_request(&v(&["send", "dev_1", "implement", "X", "TDD"]), Some(0)).unwrap();
        let req = parse_request(&line).unwrap();
        assert_eq!(req.caller, Some(0));
        match req.cmd {
            Cmd::Send(sr) => {
                assert_eq!(sr.target, "dev_1");
                assert_eq!(sr.text, "implement X TDD");
            }
            _ => panic!("expected send"),
        }
    }

    #[test]
    fn send_without_text_is_an_error() {
        let err = build_request(&v(&["send", "dev_1"]), None).unwrap_err();
        assert!(err.contains("text"), "{err}");
    }

    #[test]
    fn build_and_parse_status_request_with_and_without_target() {
        let one = parse_request(&build_request(&v(&["status", "3"]), None).unwrap()).unwrap();
        assert_eq!(
            one.cmd,
            Cmd::Status(StatusReq {
                target: Some("3".into())
            })
        );
        let all = parse_request(&build_request(&v(&["status"]), None).unwrap()).unwrap();
        assert_eq!(all.cmd, Cmd::Status(StatusReq { target: None }));
    }

    #[test]
    fn resolve_target_by_id_and_role() {
        let panes = [
            (0usize, Some("ceo".to_string())),
            (1, Some("dev_1".to_string())),
            (2, None),
        ];
        assert_eq!(resolve_target("1", &panes), Ok(1));
        assert_eq!(resolve_target("dev_1", &panes), Ok(1));
        assert!(resolve_target("9", &panes).unwrap_err().contains("no pane"));
        assert!(resolve_target("ghost", &panes)
            .unwrap_err()
            .contains("no pane"));
    }

    #[test]
    fn resolve_target_flags_ambiguous_roles() {
        let panes = [
            (1usize, Some("dev".to_string())),
            (2, Some("dev".to_string())),
        ];
        assert!(resolve_target("dev", &panes)
            .unwrap_err()
            .contains("ambiguous"));
    }

    #[test]
    fn in_subtree_walks_the_parent_chain() {
        // 0 (root) → 1 (lead) → 2, 3 (ICs); 4 is a sibling lead's IC.
        let parents = [
            (0usize, None),
            (1, Some(0)),
            (2, Some(1)),
            (3, Some(1)),
            (4, Some(5)),
            (5, Some(0)),
        ];
        assert!(in_subtree(2, 1, &parents)); // IC is in its lead's subtree
        assert!(in_subtree(1, 1, &parents)); // a pane is in its own subtree
        assert!(!in_subtree(4, 1, &parents)); // a cousin is not
        assert!(in_subtree(4, 0, &parents)); // everything is under the root
        assert!(!in_subtree(1, 2, &parents)); // a parent is not under its child
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
        let (allow, depth, trust, rest) = parse_flags(&v(&["claude", "--continue"])).unwrap();
        assert!(!allow);
        assert_eq!(trust, TrustMode::Off);
        assert_eq!(depth, DEFAULT_MAX_DEPTH);
        assert_eq!(rest, v(&["claude", "--continue"]));
    }

    #[test]
    fn flags_allow_ctl_and_max_depth() {
        let (allow, depth, _yolo, rest) =
            parse_flags(&v(&["--allow-ctl", "--max-depth", "3", "claude"])).unwrap();
        assert!(allow);
        assert_eq!(depth, 3);
        assert_eq!(rest, v(&["claude"]));
    }

    #[test]
    fn flags_trust_is_parsed() {
        let (allow, _depth, trust, rest) =
            parse_flags(&v(&["--allow-ctl", "--trust", "claude"])).unwrap();
        assert!(allow);
        assert_eq!(trust, TrustMode::Edits);
        assert_eq!(rest, v(&["claude"]));
    }

    #[test]
    fn flags_skip_permissions_is_parsed() {
        let (_allow, _depth, trust, rest) =
            parse_flags(&v(&["--skip-permissions", "claude"])).unwrap();
        assert_eq!(trust, TrustMode::Skip);
        assert_eq!(rest, v(&["claude"]));
    }

    #[test]
    fn flags_trust_and_skip_permissions_conflict() {
        assert!(parse_flags(&v(&["--trust", "--skip-permissions", "claude"]))
            .unwrap_err()
            .contains("not both"));
        assert!(parse_flags(&v(&["--skip-permissions", "--trust", "claude"]))
            .unwrap_err()
            .contains("not both"));
    }

    #[test]
    fn flags_max_depth_zero_is_unlimited() {
        let (_, depth, _, _) =
            parse_flags(&v(&["--allow-ctl", "--max-depth", "0", "claude"])).unwrap();
        assert_eq!(depth, usize::MAX);
    }

    #[test]
    fn flags_stop_at_command_so_child_keeps_its_flags() {
        // A `--max-depth` after the command belongs to the child, untouched.
        let (allow, _, _, rest) =
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
    fn build_spawn_with_identity_roundtrips() {
        let line = build_request(
            &v(&["spawn", "--role", "dev_1", "--identity", "work", "--", "claude"]),
            Some(0),
        )
        .unwrap();
        let req = parse_request(&line).unwrap();
        match req.cmd {
            Cmd::Spawn(sp) => {
                assert_eq!(sp.role.as_deref(), Some("dev_1"));
                assert_eq!(sp.identity.as_deref(), Some("work"));
                assert_eq!(sp.argv, v(&["claude"]));
            }
            _ => panic!("expected spawn"),
        }
    }

    #[test]
    fn spawn_without_identity_leaves_it_none() {
        let line = build_request(&v(&["spawn", "--", "claude"]), None).unwrap();
        match parse_request(&line).unwrap().cmd {
            Cmd::Spawn(sp) => assert_eq!(sp.identity, None),
            _ => panic!("expected spawn"),
        }
    }

    #[test]
    fn build_and_parse_kill_request() {
        let req = parse_request(&build_request(&v(&["kill", "dev_1"]), Some(1)).unwrap()).unwrap();
        assert_eq!(req.caller, Some(1));
        assert_eq!(
            req.cmd,
            Cmd::Kill(KillReq {
                target: "dev_1".into()
            })
        );
    }

    #[test]
    fn kill_without_target_is_an_error() {
        let err = build_request(&v(&["kill"]), None).unwrap_err();
        assert!(err.contains("target"), "{err}");
    }

    #[test]
    fn build_and_parse_audit_with_and_without_tail() {
        let tailed = parse_request(&build_request(&v(&["audit", "20"]), None).unwrap()).unwrap();
        assert_eq!(tailed.cmd, Cmd::Audit(AuditReq { tail: Some(20) }));
        let all = parse_request(&build_request(&v(&["audit"]), None).unwrap()).unwrap();
        assert_eq!(all.cmd, Cmd::Audit(AuditReq { tail: None }));
    }

    #[test]
    fn audit_non_numeric_tail_is_an_error() {
        let err = build_request(&v(&["audit", "lots"]), None).unwrap_err();
        assert!(err.contains("number"), "{err}");
    }

    #[test]
    fn delegation_none_is_always_allowed() {
        assert!(delegation_allowed(None, None, None, false).is_ok());
        assert!(delegation_allowed(None, Some("work"), Some("prod"), false).is_ok());
    }

    #[test]
    fn delegation_worker_may_pass_its_own_or_the_session_default() {
        // Caller holds "work"; session default is "team".
        assert!(delegation_allowed(Some("work"), Some("work"), Some("team"), false).is_ok());
        assert!(delegation_allowed(Some("team"), Some("work"), Some("team"), false).is_ok());
    }

    #[test]
    fn delegation_worker_cannot_mint_an_unheld_identity() {
        let err = delegation_allowed(Some("prod"), Some("work"), Some("team"), false).unwrap_err();
        assert!(err.contains("prod"), "{err}");
        assert!(err.contains("delegate"), "{err}");
    }

    #[test]
    fn delegation_operator_may_delegate_anything() {
        // privileged == the human root: any vault identity is theirs to grant.
        assert!(delegation_allowed(Some("prod"), None, Some("work"), true).is_ok());
    }

    #[test]
    fn reply_killed_lists_the_torn_down_panes() {
        let r = reply_killed(&[1, 2, 3]);
        let v = json::parse(&r).unwrap();
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
        assert_eq!(
            v.get("killed").and_then(Value::as_array).map(<[_]>::len),
            Some(3)
        );
    }

    #[test]
    fn reply_builders_are_valid_json() {
        let spawned = reply_spawned(3, Some("dev_1"), Some("abc-123"), None);
        let v = json::parse(&spawned).unwrap();
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
        assert_eq!(v.get("pane").and_then(Value::as_i64), Some(3));
        assert!(matches!(v.get("note"), Some(Value::Null)));

        // With a note, it rides along as a string field.
        let noted = reply_spawned(3, None, None, Some("stripped --dangerously-skip-permissions"));
        let nv = json::parse(&noted).unwrap();
        assert_eq!(
            nv.get("note").and_then(Value::as_str),
            Some("stripped --dangerously-skip-permissions")
        );

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
