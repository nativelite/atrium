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
/// Environment variable carrying the pane's **capability token** — an unguessable
/// per-pane secret minted at spawn. The server authenticates a request by looking
/// up the pane whose token matches (identity comes from the token, not the
/// self-reported `AMUX_PANE`), so a pane can neither claim another pane's id nor
/// claim operator. A request with no valid token is *unauthenticated* and may
/// only run read-only commands. Injected per pane alongside `AMUX_PANE`.
pub const ENV_TOKEN: &str = "AMUX_TOKEN";

/// The default spawn-depth ceiling: the recursion circuit-breaker (design §5).
/// Generous — a real hierarchy is CEO→lead→IC (depth 2–3); this only stops a
/// runaway self-spawning agent. Overridable/removable via `--max-depth`.
pub const DEFAULT_MAX_DEPTH: usize = 6;

/// A parsed control request.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The agent id of the pane that issued this (from `AMUX_PANE`), if any. This
    /// is *self-reported* and used only as a hint; the server authenticates the
    /// caller by [`Request::token`], never by trusting this field.
    pub caller: Option<usize>,
    /// The pane's capability token (from `AMUX_TOKEN`). The server resolves the
    /// real caller identity by matching this against a pane's minted token; absent
    /// or non-matching ⇒ the request is unauthenticated.
    pub token: Option<String>,
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
    /// Read or write the shared board — the team's source-of-truth tracker.
    Board(BoardOp),
    /// Publish to / pull from the shared pub/sub bus — the team's event stream.
    Bus(BusOp),
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

/// A `board` operation — the shared source-of-truth tracker.
#[derive(Debug, Clone, PartialEq)]
pub enum BoardOp {
    /// Merge `fields` into entry `key` (create if absent); an empty value clears
    /// that field.
    Set {
        key: String,
        fields: Vec<(String, String)>,
    },
    /// Read one entry's fields.
    Get { key: String },
    /// Roll up every entry.
    List,
    /// Remove an entry.
    Del { key: String },
    /// Atomically claim `key` for the caller with an optional lease `ttl_ms`
    /// (`None` ⇒ the board's [`crate::board::DEFAULT_LEASE_MS`]). The owner is
    /// derived server-side from the caller, never carried in the request, so a
    /// worker can't claim as someone else.
    Claim { key: String, ttl_ms: Option<u64> },
    /// Release the caller-visible claim on `key`, freeing it for the next taker.
    Release { key: String },
}

/// A `bus` operation — the shared pub/sub event stream ([`crate::bus`]). The
/// server derives *who* (the caller's role or pane) itself, so these payloads
/// never carry the subscriber name — a worker can't publish or subscribe *as*
/// someone else.
#[derive(Debug, Clone, PartialEq)]
pub enum BusOp {
    /// Publish a structured event to `topic` with an urgency `kind`.
    Pub {
        topic: String,
        kind: crate::bus::Kind,
        fields: Vec<(String, String)>,
    },
    /// Subscribe the caller to `topics` (merged); `*` is the firehose.
    Sub { topics: Vec<String> },
    /// Unsubscribe the caller from `topics`; empty ⇒ drop all its subscriptions.
    Unsub { topics: Vec<String> },
    /// Pull the caller's subscribed events with `seq > since` (no echo).
    Feed { since: u64 },
    /// Mark a `decision_needed` event answered.
    Resolve { seq: u64 },
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
    /// The permission mode this spawn requested (`--mode plan|accept|automode`),
    /// if any. `None` ⇒ inherit the session policy (the launch `--trust`). When
    /// set, amux honors it for the operator (root pane) and, for a non-operator
    /// worker, caps it at the session policy — a worker may match or de-escalate
    /// but never elevate ([`TrustMode::rank`]).
    pub mode: Option<TrustMode>,
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
    /// touch the folder-trust dialog or the permission mode. Policy keyword
    /// `default`.
    Off,
    /// `plan`: claude's **plan mode** (`--permission-mode plan`) — read-only
    /// analysis, no edits or commands. Also pre-accepts the folder-trust dialog so
    /// the agent can read the tree hands-off. The safest hands-off posture.
    Plan,
    /// `accept` (bare `--trust`): **auto-accept edits + a safe dev-command
    /// allowlist.** Agents edit and run the build/test loop hands-off, but a
    /// command outside the allowlist (e.g. `curl`, `git push`, `rm` outside the
    /// workdir) still surfaces as a **visible approval prompt** in its pane. Also
    /// pre-accepts the folder-trust dialog. The safe hands-off default.
    Edits,
    /// `automode`: claude's **auto mode** (`--permission-mode auto`) — the
    /// hands-off tier above accept-edits, auto-running edits *and* commands with
    /// claude's own guardrails (distinct from `skip`, which removes the guardrails
    /// entirely). Pre-accepts the folder-trust dialog. Note: claude only enters
    /// auto mode when the plan/model/org allow it, else it falls back to default.
    Auto,
    /// `skip` (`--skip-permissions`): **full bypass**
    /// (`--dangerously-skip-permissions`, i.e. `--permission-mode bypassPermissions`).
    /// Every command runs with no gate at all. Genuinely dangerous — amux requires
    /// an explicit launch confirmation before using it.
    Skip,
}

impl TrustMode {
    /// Autonomous-power rank, low→high: `Off` < `Plan` < `Edits` < `Skip`. The
    /// **session policy** (the launch `--trust <policy>`) is the ceiling: a
    /// non-operator worker may request a mode of equal or lower rank (match or
    /// de-escalate) but never a higher one (no self-elevation). The operator (the
    /// human's root pane) is exempt and may request any mode.
    pub fn rank(self) -> u8 {
        match self {
            TrustMode::Off => 0,
            TrustMode::Plan => 1,
            TrustMode::Edits => 2,
            TrustMode::Auto => 3,
            TrustMode::Skip => 4,
        }
    }

    /// Parse a policy keyword (`plan` / `accept` / `automode` / `skip` /
    /// `default`, with a couple of intuitive aliases) into a mode. `None` for
    /// anything else — the caller reports a clear error or treats the token as the
    /// hosted command.
    pub fn from_policy_keyword(k: &str) -> Option<TrustMode> {
        match k {
            "plan" => Some(TrustMode::Plan),
            "accept" | "acceptedits" | "edits" => Some(TrustMode::Edits),
            "automode" | "auto" => Some(TrustMode::Auto),
            "skip" | "bypass" | "dangerous" => Some(TrustMode::Skip),
            "default" | "off" | "ask" | "manual" => Some(TrustMode::Off),
            _ => None,
        }
    }

    /// The canonical policy keyword for this mode (round-trips with
    /// [`from_policy_keyword`](TrustMode::from_policy_keyword)); used for the ctl
    /// wire form and human-facing notes.
    pub fn policy_label(self) -> &'static str {
        match self {
            TrustMode::Off => "default",
            TrustMode::Plan => "plan",
            TrustMode::Edits => "accept",
            TrustMode::Auto => "automode",
            TrustMode::Skip => "skip",
        }
    }
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
    // `None` until a trust flag is seen; a second, different one is a conflict.
    let mut trust: Option<TrustMode> = None;
    let dup = || "set the session trust policy once (--trust <policy> or --skip-permissions)".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--allow-ctl" => {
                allow = true;
                i += 1;
            }
            "--trust" => {
                if trust.is_some() {
                    return Err(dup());
                }
                // Optional policy keyword: `--trust automode|accept|plan`. If the
                // next token is a known policy it is consumed; otherwise `--trust`
                // is bare (= accept) and the token is the hosted command.
                match args.get(i + 1).and_then(|n| TrustMode::from_policy_keyword(n)) {
                    Some(m) => {
                        trust = Some(m);
                        i += 2;
                    }
                    None => {
                        trust = Some(TrustMode::Edits);
                        i += 1;
                    }
                }
            }
            s if s.starts_with("--trust=") => {
                if trust.is_some() {
                    return Err(dup());
                }
                let k = &s["--trust=".len()..];
                trust = Some(TrustMode::from_policy_keyword(k).ok_or_else(|| {
                    format!("--trust: unknown policy {k:?} (use plan, accept, or automode)")
                })?);
                i += 1;
            }
            "--skip-permissions" => {
                if trust.is_some() {
                    return Err(dup());
                }
                trust = Some(TrustMode::Skip);
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
            _ => {
                return Ok((
                    allow,
                    effective_depth(max_depth),
                    trust.unwrap_or(TrustMode::Off),
                    args[i..].to_vec(),
                ))
            }
        }
    }
    Ok((
        allow,
        effective_depth(max_depth),
        trust.unwrap_or(TrustMode::Off),
        Vec::new(),
    ))
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
layout. Spawn each teammate as plain claude with NO raw permission flags: amux \
applies the session trust policy to every pane it launches, so never add \
--dangerously-skip-permissions or --permission-mode yourself. To request a \
specific mode for a teammate, add --mode plan or --mode accept or --mode \
automode or --mode skip to the spawn (plan is read-only, accept auto-accepts \
edits, automode is claude auto mode, skip is full bypass); amux honors it when \
the human operator asks and otherwise caps it at the session policy (a worker \
cannot elevate itself). \
Each teammate is a VISIBLE amux pane the human can watch, steer, and \
take over. \
Do NOT use your Task tool or background agents to delegate, since those run \
invisibly and defeat the purpose of amux. \
Track shared team state on the board instead of re-reading transcripts: \
amux ctl board set KEY field=value records current truth (status, owner, \
blocker, url), amux ctl board get KEY reads one entry, and amux ctl board list \
shows the whole board. Update the board when your status changes so the lead and \
your teammates see it. \
Before you start working a task, claim it: amux ctl board claim KEY takes it for \
you and holds a short lease. If the claim is denied it names who already holds it, \
so pick a different unclaimed task rather than assuming a teammate owns \
everything. Your board set updates renew your lease automatically while you work, \
so a task you hold never slips away. When you finish, amux ctl board release KEY \
frees it for others. Claim before you build and the team divides work with no \
collisions. \
Share fast-moving events on the bus, not just durable state on the board: \
amux ctl bus pub TOPIC field=value posts an update to a topic, add --decision \
when something needs a decision, and amux ctl bus sub TOPIC then amux ctl \
bus feed pulls what teammates published on the topics you follow. Post fyi \
updates freely. Route a decision to the teammate who should answer it with \
--to ROLE: a design question for the lead is amux ctl bus pub TOPIC --decision \
--to lead q=your question, which reaches the lead first instead of the human. \
Reserve a plain --decision with no --to for things that truly need the human. \
If you are the lead, watch your feed for decisions addressed to you and resolve \
them with amux ctl bus resolve SEQ once answered. \
Run amux ctl with no arguments for the full command surface, or use the \
amux-coordinate or amux-delegate skills for the full workflow.";

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
    let token = v.get("token").and_then(Value::as_str).map(str::to_string);
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
            let mode = v
                .get("mode")
                .and_then(Value::as_str)
                .and_then(TrustMode::from_policy_keyword);
            Cmd::Spawn(SpawnReq {
                role,
                argv,
                new_window,
                identity,
                mode,
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
        Some("board") => {
            let key = || -> Result<String, String> {
                v.get("key")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| "board needs a key".to_string())
            };
            let op = match v.get("op").and_then(Value::as_str) {
                Some("set") => {
                    let fields = v
                        .get("fields")
                        .and_then(Value::as_object)
                        .map(|o| {
                            o.iter()
                                .filter_map(|(k, val)| {
                                    val.as_str().map(|s| (k.clone(), s.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    BoardOp::Set { key: key()?, fields }
                }
                Some("get") => BoardOp::Get { key: key()? },
                Some("list") => BoardOp::List,
                Some("del") => BoardOp::Del { key: key()? },
                Some("claim") => {
                    let ttl_ms = v
                        .get("ttl_ms")
                        .and_then(Value::as_i64)
                        .filter(|n| *n > 0)
                        .map(|n| n as u64);
                    BoardOp::Claim { key: key()?, ttl_ms }
                }
                Some("release") => BoardOp::Release { key: key()? },
                other => {
                    return Err(format!(
                        "board op must be set|get|list|del|claim|release (got {other:?})"
                    ))
                }
            };
            Cmd::Board(op)
        }
        Some("bus") => {
            let op = match v.get("op").and_then(Value::as_str) {
                Some("pub") => {
                    let topic = v
                        .get("topic")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "bus pub needs a topic".to_string())?
                        .to_string();
                    let kind = v
                        .get("kind")
                        .and_then(Value::as_str)
                        .and_then(crate::bus::Kind::from_keyword)
                        .unwrap_or(crate::bus::Kind::Fyi);
                    let fields = v
                        .get("fields")
                        .and_then(Value::as_object)
                        .map(|o| {
                            o.iter()
                                .filter_map(|(k, val)| {
                                    val.as_str().map(|s| (k.clone(), s.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    BusOp::Pub { topic, kind, fields }
                }
                Some("sub") | Some("unsub") => {
                    let topics = v
                        .get("topics")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    if v.get("op").and_then(Value::as_str) == Some("sub") {
                        BusOp::Sub { topics }
                    } else {
                        BusOp::Unsub { topics }
                    }
                }
                Some("feed") => {
                    let since = v.get("since").and_then(Value::as_i64).unwrap_or(0).max(0) as u64;
                    BusOp::Feed { since }
                }
                Some("resolve") => {
                    let seq = v
                        .get("seq")
                        .and_then(Value::as_i64)
                        .ok_or_else(|| "bus resolve needs a seq".to_string())?
                        .max(0) as u64;
                    BusOp::Resolve { seq }
                }
                other => {
                    return Err(format!(
                        "bus op must be pub|sub|unsub|feed|resolve (got {other:?})"
                    ))
                }
            };
            Cmd::Bus(op)
        }
        Some(other) => return Err(format!("unknown command {other:?}")),
        None => return Err("request has no \"cmd\"".to_string()),
    };
    Ok(Request { caller, token, cmd })
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

/// `{"ok":true,"key":<key>,"entry":<entry|null>}` — a board `get`/`set` result.
/// `entry` (built by [`crate::board::entry_to_value`]) is `null` when the key is
/// absent. The caller passes the rendered value so this layer stays independent of
/// the board's internals.
pub fn reply_board_entry(key: &str, entry: Option<Value>) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("key", s(key)),
        ("entry", entry.unwrap_or(Value::Null)),
    ])
    .to_string()
}

/// `{"ok":true,"board":[{key,by,ms,fields}, …]}` — the whole board (a `list`).
pub fn reply_board_list(board: Value) -> String {
    obj(vec![("ok", Value::Bool(true)), ("board", board)]).to_string()
}

/// `{"ok":true,"key":<key>,"deleted":<bool>}` — a board `del`.
pub fn reply_board_del(key: &str, deleted: bool) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("key", s(key)),
        ("deleted", Value::Bool(deleted)),
    ])
    .to_string()
}

/// `{"ok":true,"key":<key>,"granted":<bool>,"holder":<who|null>,"lease_ms":<n>,
/// "entry":<entry|null>}` — a board `claim`. On grant, `entry` carries the fresh
/// claim; on denial, `holder`/`lease_ms` name who holds it and until when.
pub fn reply_board_claim(key: &str, claim: &crate::board::Claim) -> String {
    use crate::board::{entry_to_value, Claim};
    let (granted, holder, lease_ms, entry) = match claim {
        Claim::Granted(e) => (
            true,
            e.claimed_by.clone().map(Value::String).unwrap_or(Value::Null),
            e.lease_ms,
            entry_to_value(e),
        ),
        Claim::Denied { holder, lease_ms } => {
            (false, Value::String(holder.clone()), *lease_ms, Value::Null)
        }
    };
    obj(vec![
        ("ok", Value::Bool(true)),
        ("key", s(key)),
        ("granted", Value::Bool(granted)),
        ("holder", holder),
        ("lease_ms", Value::Number(Number::Int(lease_ms as i64))),
        ("entry", entry),
    ])
    .to_string()
}

/// `{"ok":true,"key":<key>,"released":<bool>}` — a board `release`.
pub fn reply_board_release(key: &str, released: bool) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("key", s(key)),
        ("released", Value::Bool(released)),
    ])
    .to_string()
}

/// `{"ok":true,"event":<event>}` — a bus `pub` result (the stored event, built
/// by [`crate::bus::event_to_value`]).
pub fn reply_bus_published(event: Value) -> String {
    obj(vec![("ok", Value::Bool(true)), ("event", event)]).to_string()
}

/// `{"ok":true,"subscribed":[<topic>,…]}` — the caller's full topic set after a
/// `sub`/`unsub`.
pub fn reply_bus_subscribed(topics: Vec<String>) -> String {
    let arr = topics.into_iter().map(Value::String).collect();
    obj(vec![
        ("ok", Value::Bool(true)),
        ("subscribed", Value::Array(arr)),
    ])
    .to_string()
}

/// `{"ok":true,"feed":[<event>,…],"cursor":<seq>}` — the pulled events (already
/// serialized + scoped by the caller) and the new cursor to pass as the next
/// `--since`. `cursor` is the max seq returned, or the request's `since` when the
/// pull was empty (so the cursor never goes backwards).
pub fn reply_bus_feed(feed: Value, cursor: u64) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("feed", feed),
        ("cursor", Value::Number(Number::Int(cursor as i64))),
    ])
    .to_string()
}

/// `{"ok":true,"seq":<seq>,"resolved":<bool>}` — a bus `resolve`.
pub fn reply_bus_resolved(seq: u64, resolved: bool) -> String {
    obj(vec![
        ("ok", Value::Bool(true)),
        ("seq", Value::Number(Number::Int(seq as i64))),
        ("resolved", Value::Bool(resolved)),
    ])
    .to_string()
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

    // `--json` prints the raw reply (for scripting); otherwise a `board` view
    // renders as a colored table with clickable links.
    let raw_json = args.iter().any(|a| a == "--json");
    let filtered: Vec<String> = args.iter().filter(|a| a.as_str() != "--json").cloned().collect();
    let args = &filtered[..];

    let request = match build_request(args, caller) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("amux ctl: {msg}");
            eprintln!(
                "usage: amux ctl spawn [--role R] [--identity X] [--here] [--mode plan|accept|automode|skip] -- <cmd...>\n\
                 \x20      | list | send <target> <text> | status [target] | kill <target> | audit [N]\n\
                 \x20      | board set <key> <field=value...> | board get <key> | board list | board del <key>\n\
                 \x20      | board claim <key> [--ttl secs] | board release <key>\n\
                 \x20      | bus pub <topic> [--decision] [--to <role>] <field=value...> | bus sub <topic...> | bus feed [--since N] | bus resolve <seq>"
            );
            return ExitCode::FAILURE;
        }
    };

    match crate::ipc::request(&address, &request) {
        Ok(reply) => {
            // A `board` result renders as a table (unless --json); everything else
            // prints its JSON reply verbatim.
            match (args.first().map(String::as_str), raw_json) {
                (Some("board"), false) => match render_board(&reply) {
                    Some(view) => println!("{view}"),
                    None => println!("{reply}"),
                },
                (Some("bus"), false) => match render_bus(&reply) {
                    Some(view) => println!("{view}"),
                    None => println!("{reply}"),
                },
                _ => println!("{reply}"),
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
            eprintln!("amux ctl: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Render a `board` reply as a human view: one line per entry, the `status`
/// field colored, `http(s)` values as OSC-8 clickable links, and the writer dim
/// in parens. `None` if the reply isn't a board result (caller falls back to the
/// raw JSON). Escapes are cosmetic — piping `--json` gives the machine form.
fn render_board(reply: &str) -> Option<String> {
    let v = json::parse(reply).ok()?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    // list → a table of entries (each carrying its own inline "key").
    if let Some(arr) = v.get("board").and_then(Value::as_array) {
        if arr.is_empty() {
            return Some("  (board is empty)".to_string());
        }
        let body = arr
            .iter()
            .map(|e| {
                let key = e.get("key").and_then(Value::as_str).unwrap_or("?");
                render_entry_line(key, e)
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Some(body);
    }
    // del → a one-line confirmation.
    if let Some(deleted) = v.get("deleted").and_then(Value::as_bool) {
        let key = v.get("key").and_then(Value::as_str).unwrap_or("?");
        return Some(format!(
            "  {key}: {}",
            if deleted {
                "removed"
            } else {
                "was not on the board"
            }
        ));
    }
    // claim → granted shows the now-held entry; denied names the current holder.
    if let Some(granted) = v.get("granted").and_then(Value::as_bool) {
        let key = v.get("key").and_then(Value::as_str).unwrap_or("?");
        if granted {
            let line = match v.get("entry") {
                Some(entry) if entry != &Value::Null => render_entry_line(key, entry),
                _ => format!("  {key}"),
            };
            return Some(format!("{line}\n  \x1b[38;5;10mclaimed {key}\x1b[0m"));
        }
        let holder = v.get("holder").and_then(Value::as_str).unwrap_or("someone");
        return Some(format!(
            "  \x1b[38;5;11m{key} is held by {holder}\x1b[0m — pick another task"
        ));
    }
    // release → a one-line confirmation.
    if let Some(released) = v.get("released").and_then(Value::as_bool) {
        let key = v.get("key").and_then(Value::as_str).unwrap_or("?");
        return Some(format!(
            "  {key}: {}",
            if released { "released" } else { "was not on the board" }
        ));
    }
    // get/set → one entry (or "not on the board").
    if let Some(key) = v.get("key").and_then(Value::as_str) {
        return Some(match v.get("entry") {
            Some(Value::Null) | None => format!("  {key}: (not on the board)"),
            Some(entry) => render_entry_line(key, entry),
        });
    }
    None
}

/// Render a `bus` reply as a human view: a `feed`/`pub` shows one line per event
/// (seq, an urgency glyph, the topic, its fields, and who sent it); `sub`/`unsub`
/// shows the resulting subscription set; `resolve` a one-line confirmation. `None`
/// if the reply isn't a bus result (caller falls back to raw JSON).
fn render_bus(reply: &str) -> Option<String> {
    let v = json::parse(reply).ok()?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    // feed → a list of events (plus the cursor to resume from).
    if let Some(arr) = v.get("feed").and_then(Value::as_array) {
        if arr.is_empty() {
            return Some("  (no new events)".to_string());
        }
        let cursor = v.get("cursor").and_then(Value::as_i64).unwrap_or(0);
        let mut body = arr.iter().map(render_event_line).collect::<Vec<_>>().join("\n");
        body.push_str(&format!("\n\x1b[2m  — cursor {cursor} (next: bus feed --since {cursor})\x1b[0m"));
        return Some(body);
    }
    // pub → the single stored event.
    if let Some(event) = v.get("event") {
        return Some(render_event_line(event));
    }
    // sub/unsub → the resulting subscription set.
    if let Some(subs) = v.get("subscribed").and_then(Value::as_array) {
        if subs.is_empty() {
            return Some("  (subscribed to nothing)".to_string());
        }
        let list = subs
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!("  subscribed: {list}"));
    }
    // resolve → a one-line confirmation.
    if let Some(resolved) = v.get("resolved").and_then(Value::as_bool) {
        let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(0);
        return Some(format!(
            "  decision #{seq}: {}",
            if resolved { "resolved" } else { "was not an open decision" }
        ));
    }
    None
}

/// Format one bus event: `[#7] ! deploy  msg=ship it?  (from dev_1)`. A
/// `decision_needed` event gets an amber `!` and bold topic so escalations stand
/// out from FYI chatter (a dim `·`).
fn render_event_line(event: &Value) -> String {
    let seq = event.get("seq").and_then(Value::as_i64).unwrap_or(0);
    let topic = event.get("topic").and_then(Value::as_str).unwrap_or("?");
    let kind = event.get("kind").and_then(Value::as_str).unwrap_or("fyi");
    let decision = kind == "decision_needed";
    let (glyph, topic_sgr) = if decision {
        ("\x1b[38;5;11m!\x1b[0m", "\x1b[1;38;5;11m") // amber bang, bold amber topic
    } else {
        ("\x1b[2m·\x1b[0m", "\x1b[1m") // dim dot, bold topic
    };
    let empty: &[(String, Value)] = &[];
    let fields = event.get("fields").and_then(Value::as_object).unwrap_or(empty);
    let field_str = fields
        .iter()
        .map(|(k, v)| {
            let vs = v.as_str().unwrap_or("");
            if is_url(vs) {
                format!("{k}={}", hyperlink(vs, vs))
            } else {
                format!("{k}={vs}")
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    let from = event
        .get("from")
        .and_then(Value::as_str)
        .map(|f| format!("  \x1b[2m(from {f})\x1b[0m"))
        .unwrap_or_default();
    format!("\x1b[2m[#{seq}]\x1b[0m {glyph} {topic_sgr}{topic}\x1b[0m  {field_str}{from}")
}

/// Format one board entry: `● key   status=DONE  owner=Max  url=<link>  (by dev_1)`.
/// `entry` is `{by, ms, fields:{…}}`; the `key` is passed in (list entries carry it
/// inline, get/set entries don't).
fn render_entry_line(key: &str, entry: &Value) -> String {
    let empty: &[(String, Value)] = &[];
    let fields = entry.get("fields").and_then(Value::as_object).unwrap_or(empty);
    let status = fields
        .iter()
        .find(|(k, _)| k == "status")
        .and_then(|(_, v)| v.as_str());
    let glyph = match status {
        Some(st) => format!("{}\u{25CF}\x1b[0m ", status_sgr(st)),
        None => "  ".to_string(),
    };
    let field_str = fields
        .iter()
        .map(|(k, v)| {
            let vs = v.as_str().unwrap_or("");
            let rendered = if k == "status" {
                format!("{}{vs}\x1b[0m", status_sgr(vs))
            } else if is_url(vs) {
                hyperlink(vs, vs)
            } else {
                vs.to_string()
            };
            format!("{k}={rendered}")
        })
        .collect::<Vec<_>>()
        .join("  ");
    // A live claim shows a magenta `⊙holder` tag so a glance at the board tells
    // you who is actively working each task (empty when unclaimed).
    let held = entry
        .get("claimed_by")
        .and_then(Value::as_str)
        .map(|h| format!("  \x1b[38;5;13m\u{2299}{h}\x1b[0m"))
        .unwrap_or_default();
    let by = entry
        .get("by")
        .and_then(Value::as_str)
        .map(|b| format!("  \x1b[2m(by {b})\x1b[0m"))
        .unwrap_or_default();
    format!("{glyph}\x1b[1m{key:<12}\x1b[0m {field_str}{held}{by}")
}

/// A status string → an SGR color: green done/shipped, red blocked/failed, cyan
/// in-progress, amber waiting/todo, default otherwise. Case-insensitive substring.
/// Shared by the CLI board view and the in-chrome board panel.
pub fn status_sgr(status: &str) -> &'static str {
    let s = status.to_ascii_lowercase();
    if s.contains("done") || s.contains("ship") || s.contains("complete") || s == "ok" {
        "\x1b[38;5;10m" // green
    } else if s.contains("block") || s.contains("fail") || s.contains("stuck") {
        "\x1b[38;5;9m" // red
    } else if s.contains("wip") || s.contains("progress") || s.contains("working") {
        "\x1b[38;5;14m" // cyan
    } else if s.contains("wait") || s.contains("todo") || s.contains("pending") {
        "\x1b[38;5;11m" // amber
    } else {
        "\x1b[0m"
    }
}

/// A value that looks like a web link.
pub fn is_url(v: &str) -> bool {
    v.starts_with("http://") || v.starts_with("https://")
}

/// Wrap `url` as an OSC-8 hyperlink (clickable in modern terminals). `label` is
/// the visible text (pass the url itself to show it verbatim).
pub fn hyperlink(url: &str, label: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\")
}

/// A one-glyph status marker for a status string: `●` done, `○` blocked, `◐`
/// in-progress/waiting, `·` otherwise. Pair with [`status_sgr`] for color.
pub fn status_glyph(status: &str) -> &'static str {
    let s = status.to_ascii_lowercase();
    if s.contains("done") || s.contains("ship") || s.contains("complete") || s == "ok" {
        "\u{25CF}" // ●
    } else if s.contains("block") || s.contains("fail") || s.contains("stuck") {
        "\u{25CB}" // ○
    } else if s.contains("wip")
        || s.contains("progress")
        || s.contains("working")
        || s.contains("wait")
        || s.contains("todo")
        || s.contains("pending")
    {
        "\u{25D0}" // ◐
    } else {
        "\u{00B7}" // ·
    }
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
    Value::Object(fields.into_iter().map(|(f, v)| (f, Value::String(v))).collect())
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
    // The capability token authenticates the caller. It is env-sourced (like the
    // endpoint address and the caller id in `ctl_cmd`); tests, which never set
    // `AMUX_TOKEN`, simply send no token and are treated as unauthenticated.
    if let Ok(tok) = std::env::var(ENV_TOKEN) {
        if !tok.is_empty() {
            pairs.push(("token", Value::String(tok)));
        }
    }
    match args.first().map(String::as_str) {
        Some("spawn") => {
            pairs.push(("cmd", s("spawn")));
            let mut role: Option<String> = None;
            let mut identity: Option<String> = None;
            let mut new_window = true;
            let mut mode: Option<TrustMode> = None;
            let mut argv: Vec<String> = Vec::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--mode" => {
                        let k = args
                            .get(i + 1)
                            .ok_or_else(|| "--mode needs a value (plan, accept, or automode)".to_string())?;
                        mode = Some(TrustMode::from_policy_keyword(k).ok_or_else(|| {
                            format!("--mode: unknown {k:?} (use plan, accept, or automode)")
                        })?);
                        i += 2;
                    }
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
            if let Some(m) = mode {
                pairs.push(("mode", Value::String(m.policy_label().to_string())));
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
        Some("board") => {
            pairs.push(("cmd", s("board")));
            let sub = args
                .get(1)
                .map(String::as_str)
                .ok_or_else(|| "board needs set|get|list|del|claim|release".to_string())?;
            pairs.push(("op", s(sub)));
            match sub {
                "set" => {
                    let key = args
                        .get(2)
                        .filter(|t| !t.starts_with('-'))
                        .ok_or_else(|| "board set needs a key".to_string())?;
                    pairs.push(("key", Value::String(key.clone())));
                    // Remaining args are `field=value` pairs (empty value clears);
                    // unquoted spaced values are joined across tokens.
                    let mut fields: Vec<(String, String)> = Vec::new();
                    for a in &args[3.min(args.len())..] {
                        absorb_field_token(&mut fields, a)
                            .map_err(|e| format!("board set: {e}"))?;
                    }
                    if fields.is_empty() {
                        return Err("board set needs at least one field=value".to_string());
                    }
                    pairs.push(("fields", fields_to_value(fields)));
                }
                "get" | "del" | "release" => {
                    let key = args
                        .get(2)
                        .filter(|t| !t.starts_with('-'))
                        .ok_or_else(|| format!("board {sub} needs a key"))?;
                    pairs.push(("key", Value::String(key.clone())));
                }
                "claim" => {
                    let key = args
                        .get(2)
                        .filter(|t| !t.starts_with('-'))
                        .ok_or_else(|| "board claim needs a key".to_string())?;
                    pairs.push(("key", Value::String(key.clone())));
                    // Optional `--ttl SECS` overrides the default lease length.
                    if let Some(pos) = args.iter().position(|a| a == "--ttl") {
                        let secs = args
                            .get(pos + 1)
                            .ok_or_else(|| "--ttl needs a value in seconds".to_string())?
                            .parse::<u64>()
                            .map_err(|_| "--ttl must be a whole number of seconds".to_string())?;
                        pairs.push(("ttl_ms", Value::Number(Number::Int((secs * 1000) as i64))));
                    }
                }
                "list" => {}
                other => {
                    return Err(format!(
                        "board op must be set|get|list|del|claim|release (got {other:?})"
                    ))
                }
            }
        }
        Some("bus") => {
            pairs.push(("cmd", s("bus")));
            let sub = args
                .get(1)
                .map(String::as_str)
                .ok_or_else(|| "bus needs pub|sub|unsub|feed|resolve".to_string())?;
            pairs.push(("op", s(sub)));
            match sub {
                "pub" => {
                    let topic = args
                        .get(2)
                        .filter(|t| !t.starts_with('-'))
                        .ok_or_else(|| "bus pub needs a topic".to_string())?;
                    pairs.push(("topic", Value::String(topic.clone())));
                    // Default FYI; `--decision` (or `--kind K`) escalates. Remaining
                    // args are `field=value` pairs (unquoted spaced values are
                    // joined across tokens) — the structured payload.
                    let mut kind = crate::bus::Kind::Fyi;
                    let mut fields: Vec<(String, String)> = Vec::new();
                    let mut i = 3.min(args.len());
                    while i < args.len() {
                        let a = &args[i];
                        match a.as_str() {
                            "--decision" => {
                                kind = crate::bus::Kind::DecisionNeeded;
                                i += 1;
                            }
                            "--to" => {
                                // Address the event to a teammate role (e.g. the
                                // lead). Sugar for a `to=<role>` field: an
                                // agent-addressed decision routes to that agent
                                // instead of firing the human's urgent bar.
                                let role = args
                                    .get(i + 1)
                                    .ok_or_else(|| "--to needs a role (e.g. --to lead)".to_string())?;
                                fields.push(("to".to_string(), role.clone()));
                                i += 2;
                            }
                            "--kind" => {
                                let k = args
                                    .get(i + 1)
                                    .ok_or_else(|| "--kind needs fyi or decision_needed".to_string())?;
                                kind = crate::bus::Kind::from_keyword(k).ok_or_else(|| {
                                    format!("--kind: unknown {k:?} (use fyi or decision_needed)")
                                })?;
                                i += 2;
                            }
                            _ => {
                                absorb_field_token(&mut fields, a)
                                    .map_err(|e| format!("bus pub: {e}"))?;
                                i += 1;
                            }
                        }
                    }
                    if fields.is_empty() {
                        return Err(
                            "bus pub needs at least one field=value (e.g. msg=merged the PR)".to_string(),
                        );
                    }
                    pairs.push(("kind", s(kind.as_str())));
                    pairs.push(("fields", fields_to_value(fields)));
                }
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
                "feed" => {
                    // `--since N` resumes after cursor N; default 0 = from the start.
                    let mut i = 2;
                    while i < args.len() {
                        if args[i] == "--since" {
                            let n = args
                                .get(i + 1)
                                .and_then(|t| t.parse::<i64>().ok())
                                .ok_or_else(|| "--since needs a number".to_string())?;
                            pairs.push(("since", Value::Number(Number::Int(n.max(0)))));
                            i += 2;
                        } else if let Some(rest) = args[i].strip_prefix("--since=") {
                            let n = rest
                                .parse::<i64>()
                                .map_err(|_| "--since needs a number".to_string())?;
                            pairs.push(("since", Value::Number(Number::Int(n.max(0)))));
                            i += 1;
                        } else {
                            return Err(format!("unexpected argument {:?} to bus feed", args[i]));
                        }
                    }
                }
                "resolve" => {
                    let tok = args
                        .get(2)
                        .filter(|t| !t.starts_with('-'))
                        .ok_or_else(|| "bus resolve needs a seq".to_string())?;
                    let seq = tok
                        .parse::<i64>()
                        .map_err(|_| format!("bus resolve: seq must be a number, got {tok:?}"))?;
                    pairs.push(("seq", Value::Number(Number::Int(seq.max(0)))));
                }
                other => {
                    return Err(format!(
                        "bus op must be pub|sub|unsub|feed|resolve (got {other:?})"
                    ))
                }
            }
        }
        Some(other) => return Err(format!("unknown subcommand {other:?}")),
        None => {
            return Err(
                "needs a subcommand: spawn | list | send | status | kill | audit | board | bus"
                    .to_string(),
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
    fn build_bus_pub_roundtrips_through_parse() {
        let line = build_request(
            &v(&["bus", "pub", "deploy", "msg=shipping v2", "url=https://x/pr/9"]),
            Some(0),
        )
        .unwrap();
        let req = parse_request(&line).unwrap();
        match req.cmd {
            Cmd::Bus(BusOp::Pub { topic, kind, fields }) => {
                assert_eq!(topic, "deploy");
                assert_eq!(kind, crate::bus::Kind::Fyi, "defaults to fyi");
                assert!(fields.contains(&("msg".to_string(), "shipping v2".to_string())));
                assert!(fields.contains(&("url".to_string(), "https://x/pr/9".to_string())));
            }
            other => panic!("expected bus pub, got {other:?}"),
        }
    }

    #[test]
    fn build_bus_pub_decision_flag_sets_kind() {
        let line =
            build_request(&v(&["bus", "pub", "release", "--decision", "q=ship now?"]), None).unwrap();
        let req = parse_request(&line).unwrap();
        match req.cmd {
            Cmd::Bus(BusOp::Pub { kind, .. }) => {
                assert_eq!(kind, crate::bus::Kind::DecisionNeeded)
            }
            other => panic!("expected bus pub, got {other:?}"),
        }
    }

    #[test]
    fn bus_pub_without_fields_is_an_error() {
        let err = build_request(&v(&["bus", "pub", "deploy"]), None).unwrap_err();
        assert!(err.contains("field=value"), "{err}");
    }

    #[test]
    fn build_bus_sub_and_feed_roundtrip() {
        let sub = parse_request(&build_request(&v(&["bus", "sub", "deploy", "*"]), None).unwrap()).unwrap();
        assert_eq!(
            sub.cmd,
            Cmd::Bus(BusOp::Sub {
                topics: v(&["deploy", "*"])
            })
        );
        let feed =
            parse_request(&build_request(&v(&["bus", "feed", "--since", "5"]), None).unwrap()).unwrap();
        assert_eq!(feed.cmd, Cmd::Bus(BusOp::Feed { since: 5 }));
    }

    #[test]
    fn build_bus_resolve_roundtrip() {
        let r = parse_request(&build_request(&v(&["bus", "resolve", "7"]), None).unwrap()).unwrap();
        assert_eq!(r.cmd, Cmd::Bus(BusOp::Resolve { seq: 7 }));
    }

    #[test]
    fn bus_pub_joins_unquoted_spaced_values() {
        // The shell splits `msg=merged the PR` into three tokens; the parser must
        // rejoin the continuation words into one value (no quoting needed).
        let line = build_request(
            &v(&["bus", "pub", "deploy", "msg=merged", "the", "PR", "url=https://x/pr/42"]),
            None,
        )
        .unwrap();
        match parse_request(&line).unwrap().cmd {
            Cmd::Bus(BusOp::Pub { fields, .. }) => {
                assert!(fields.contains(&("msg".to_string(), "merged the PR".to_string())));
                assert!(fields.contains(&("url".to_string(), "https://x/pr/42".to_string())));
            }
            other => panic!("expected bus pub, got {other:?}"),
        }
    }

    #[test]
    fn bus_pub_decision_keeps_spaced_value() {
        let line =
            build_request(&v(&["bus", "pub", "release", "--decision", "q=ship", "v2", "now?"]), None)
                .unwrap();
        match parse_request(&line).unwrap().cmd {
            Cmd::Bus(BusOp::Pub { kind, fields, .. }) => {
                assert_eq!(kind, crate::bus::Kind::DecisionNeeded);
                assert!(fields.contains(&("q".to_string(), "ship v2 now?".to_string())));
            }
            other => panic!("expected bus pub, got {other:?}"),
        }
    }

    #[test]
    fn board_set_joins_unquoted_spaced_values() {
        let line = build_request(
            &v(&["board", "set", "task", "status=in", "progress", "owner=Max"]),
            None,
        )
        .unwrap();
        match parse_request(&line).unwrap().cmd {
            Cmd::Board(BoardOp::Set { fields, .. }) => {
                assert!(fields.contains(&("status".to_string(), "in progress".to_string())));
                assert!(fields.contains(&("owner".to_string(), "Max".to_string())));
            }
            other => panic!("expected board set, got {other:?}"),
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
    fn flags_trust_takes_a_policy_keyword() {
        // `--trust automode|plan|accept` sets the session policy; the keyword is
        // consumed and the command (`claude`) is left in `rest`.
        for (policy, want) in [
            ("automode", TrustMode::Auto),
            ("skip", TrustMode::Skip),
            ("plan", TrustMode::Plan),
            ("accept", TrustMode::Edits),
        ] {
            let (_a, _d, trust, rest) =
                parse_flags(&v(&["--allow-ctl", "--trust", policy, "claude"])).unwrap();
            assert_eq!(trust, want, "--trust {policy}");
            assert_eq!(rest, v(&["claude"]), "--trust {policy} leaves the command");
        }
        // `--trust=plan` spelling.
        let (_a, _d, trust, rest) = parse_flags(&v(&["--trust=plan", "claude"])).unwrap();
        assert_eq!(trust, TrustMode::Plan);
        assert_eq!(rest, v(&["claude"]));
    }

    #[test]
    fn flags_bare_trust_is_accept_and_keeps_the_command() {
        // A bare `--trust` followed by a non-policy token = accept, and the token
        // is the hosted command (not eaten as a policy).
        let (_a, _d, trust, rest) = parse_flags(&v(&["--trust", "claude"])).unwrap();
        assert_eq!(trust, TrustMode::Edits);
        assert_eq!(rest, v(&["claude"]));
    }

    #[test]
    fn flags_trust_and_skip_permissions_conflict() {
        assert!(parse_flags(&v(&["--trust", "--skip-permissions", "claude"]))
            .unwrap_err()
            .contains("once"));
        assert!(parse_flags(&v(&["--skip-permissions", "--trust", "claude"]))
            .unwrap_err()
            .contains("once"));
        // Two policy spellings at once is also a conflict.
        assert!(parse_flags(&v(&["--trust", "plan", "--trust", "automode", "claude"]))
            .unwrap_err()
            .contains("once"));
    }

    #[test]
    fn trust_mode_rank_orders_by_autonomous_power() {
        // Off < plan < accept < automode < skip.
        assert!(TrustMode::Off.rank() < TrustMode::Plan.rank());
        assert!(TrustMode::Plan.rank() < TrustMode::Edits.rank());
        assert!(TrustMode::Edits.rank() < TrustMode::Auto.rank());
        assert!(TrustMode::Auto.rank() < TrustMode::Skip.rank());
        // Keyword ↔ label round-trip for every mode.
        for m in [
            TrustMode::Off,
            TrustMode::Plan,
            TrustMode::Edits,
            TrustMode::Auto,
            TrustMode::Skip,
        ] {
            assert_eq!(TrustMode::from_policy_keyword(m.policy_label()), Some(m));
        }
        // `automode` and `skip` are DISTINCT (the 0.18.0 conflation bug).
        assert_ne!(
            TrustMode::from_policy_keyword("automode"),
            TrustMode::from_policy_keyword("skip")
        );
    }

    #[test]
    fn build_spawn_mode_roundtrips_through_parse() {
        // `ctl spawn --mode automode` reaches the server as SpawnReq.mode = Auto
        // (auto mode) — NOT Skip (full bypass); they are separate.
        let line =
            build_request(&v(&["spawn", "--mode", "automode", "--", "claude"]), Some(0)).unwrap();
        match parse_request(&line).unwrap().cmd {
            Cmd::Spawn(sp) => assert_eq!(sp.mode, Some(TrustMode::Auto)),
            _ => panic!("expected spawn"),
        }
        // `--mode skip` is the separate full-bypass request.
        let sk = build_request(&v(&["spawn", "--mode", "skip", "--", "claude"]), Some(0)).unwrap();
        match parse_request(&sk).unwrap().cmd {
            Cmd::Spawn(sp) => assert_eq!(sp.mode, Some(TrustMode::Skip)),
            _ => panic!("expected spawn"),
        }
        // No --mode ⇒ None (inherit the session policy).
        let plain = build_request(&v(&["spawn", "--", "claude"]), None).unwrap();
        match parse_request(&plain).unwrap().cmd {
            Cmd::Spawn(sp) => assert_eq!(sp.mode, None),
            _ => panic!("expected spawn"),
        }
        // An unknown --mode is a clear client error.
        assert!(build_request(&v(&["spawn", "--mode", "yolo", "--", "claude"]), None)
            .unwrap_err()
            .contains("--mode"));
    }

    #[test]
    fn build_board_set_roundtrips_through_parse() {
        // `board set auth status=DONE owner=Max` → BoardOp::Set with both fields.
        let line = build_request(
            &v(&["board", "set", "auth", "status=DONE", "owner=Max"]),
            Some(0),
        )
        .unwrap();
        match parse_request(&line).unwrap().cmd {
            Cmd::Board(BoardOp::Set { key, fields }) => {
                assert_eq!(key, "auth");
                assert!(fields.contains(&("status".to_string(), "DONE".to_string())));
                assert!(fields.contains(&("owner".to_string(), "Max".to_string())));
            }
            _ => panic!("expected board set"),
        }
    }

    #[test]
    fn build_board_get_list_del_roundtrip() {
        let get = build_request(&v(&["board", "get", "auth"]), None).unwrap();
        assert!(matches!(
            parse_request(&get).unwrap().cmd,
            Cmd::Board(BoardOp::Get { key }) if key == "auth"
        ));
        let list = build_request(&v(&["board", "list"]), None).unwrap();
        assert!(matches!(
            parse_request(&list).unwrap().cmd,
            Cmd::Board(BoardOp::List)
        ));
        let del = build_request(&v(&["board", "del", "auth"]), None).unwrap();
        assert!(matches!(
            parse_request(&del).unwrap().cmd,
            Cmd::Board(BoardOp::Del { key }) if key == "auth"
        ));
    }

    #[test]
    fn bus_pub_to_addresses_a_decision_to_a_role() {
        // `--to lead` becomes a `to=lead` field, so the server/surfacing can route
        // the decision to that teammate instead of the human.
        let line = build_request(
            &v(&["bus", "pub", "build", "--decision", "--to", "lead", "q=wire order?"]),
            Some(0),
        )
        .unwrap();
        match parse_request(&line).unwrap().cmd {
            Cmd::Bus(BusOp::Pub { kind, fields, .. }) => {
                assert_eq!(kind, crate::bus::Kind::DecisionNeeded);
                assert!(fields.contains(&("to".to_string(), "lead".to_string())));
                assert!(fields.iter().any(|(k, _)| k == "q"));
            }
            other => panic!("expected bus pub, got {other:?}"),
        }
        // `--to` without a role is a clear client error.
        assert!(build_request(&v(&["bus", "pub", "t", "--decision", "--to"]), None)
            .unwrap_err()
            .contains("--to"));
    }

    #[test]
    fn parse_request_carries_the_capability_token() {
        // The server reads the token off the wire to authenticate the caller.
        let req = parse_request(r#"{"caller":2,"token":"deadbeef","cmd":"list"}"#).unwrap();
        assert_eq!(req.caller, Some(2));
        assert_eq!(req.token.as_deref(), Some("deadbeef"));
        // Absent token parses as None (an unauthenticated request).
        let req = parse_request(r#"{"cmd":"list"}"#).unwrap();
        assert_eq!(req.token, None);
    }

    #[test]
    fn build_board_claim_and_release_roundtrip() {
        // Bare claim → default lease (ttl_ms None).
        let claim = build_request(&v(&["board", "claim", "cli"]), Some(0)).unwrap();
        assert!(matches!(
            parse_request(&claim).unwrap().cmd,
            Cmd::Board(BoardOp::Claim { key, ttl_ms: None }) if key == "cli"
        ));
        // `--ttl 90` → 90_000 ms carried through to the server.
        let ttl = build_request(&v(&["board", "claim", "cli", "--ttl", "90"]), Some(0)).unwrap();
        assert!(matches!(
            parse_request(&ttl).unwrap().cmd,
            Cmd::Board(BoardOp::Claim { key, ttl_ms: Some(90_000) }) if key == "cli"
        ));
        // release → BoardOp::Release.
        let rel = build_request(&v(&["board", "release", "cli"]), Some(0)).unwrap();
        assert!(matches!(
            parse_request(&rel).unwrap().cmd,
            Cmd::Board(BoardOp::Release { key }) if key == "cli"
        ));
        // A malformed --ttl is a clear client error, not a silent default.
        assert!(build_request(&v(&["board", "claim", "cli", "--ttl", "soon"]), Some(0))
            .unwrap_err()
            .contains("--ttl"));
    }

    #[test]
    fn agent_directive_has_no_shim_breaking_chars() {
        // The directive is injected via `--append-system-prompt` through a Windows
        // `cmd /C` shim; a shell-metacharacter would break the quoting and kill the
        // agent pane on launch. Guard the load-bearing set so a future edit (like
        // the claim/lease lines) can't silently reintroduce one.
        for c in ['"', '`', '&', '|', '<', '>', '^', '%'] {
            assert!(
                !AGENT_CTL_DIRECTIVE.contains(c),
                "directive contains shim-breaking {c:?}"
            );
        }
    }

    #[test]
    fn agent_directive_teaches_claim_before_work() {
        // The behavioral half of the claim protocol: agents are told to claim
        // before building and release when done, else the primitive goes unused.
        assert!(AGENT_CTL_DIRECTIVE.contains("board claim"));
        assert!(AGENT_CTL_DIRECTIVE.contains("board release"));
    }

    #[test]
    fn board_claim_denied_view_names_the_holder() {
        // A denied claim renders the current holder so a loser knows to move on.
        let reply = reply_board_claim(
            "cli",
            &crate::board::Claim::Denied { holder: "scout".to_string(), lease_ms: 1100 },
        );
        let view = render_board(&reply).expect("claim reply renders");
        assert!(view.contains("held by scout"), "view was: {view}");
        // A granted claim shows the confirmation.
        let mut e = crate::board::Entry::default();
        e.claimed_by = Some("cli".to_string());
        e.lease_ms = 1100;
        let granted = reply_board_claim("cli", &crate::board::Claim::Granted(e));
        let gview = render_board(&granted).expect("granted renders");
        assert!(gview.contains("claimed cli"), "view was: {gview}");
    }

    #[test]
    fn board_view_renders_status_color_and_clickable_url() {
        // A list reply renders each entry with a colored status and an OSC-8 link.
        let reply = r#"{"ok":true,"board":[{"key":"auth","by":"dev_1","ms":1,"fields":{"status":"DONE","owner":"Max","url":"https://x.io"}}]}"#;
        let view = render_board(reply).unwrap();
        assert!(view.contains("auth"), "key present");
        assert!(view.contains("owner=Max"));
        assert!(view.contains("\x1b[38;5;10m"), "DONE is green");
        // The url is wrapped as an OSC-8 hyperlink, not shown as bare text only.
        assert!(view.contains("\x1b]8;;https://x.io\x1b\\"), "url is clickable");
        assert!(view.contains("(by dev_1)"));
    }

    #[test]
    fn board_view_handles_empty_and_missing() {
        assert_eq!(
            render_board(r#"{"ok":true,"board":[]}"#).unwrap(),
            "  (board is empty)"
        );
        assert!(render_board(r#"{"ok":true,"key":"x","entry":null}"#)
            .unwrap()
            .contains("not on the board"));
        // A non-board (or failed) reply → None, so the caller prints raw JSON.
        assert!(render_board(r#"{"ok":true,"pane":3}"#).is_none());
        assert!(render_board(r#"{"ok":false,"err":"nope"}"#).is_none());
    }

    #[test]
    fn status_color_map() {
        assert_eq!(status_sgr("DONE"), "\x1b[38;5;10m");
        assert_eq!(status_sgr("Blocked on api"), "\x1b[38;5;9m");
        assert_eq!(status_sgr("wip"), "\x1b[38;5;14m");
        assert_eq!(status_sgr("waiting"), "\x1b[38;5;11m");
        assert_eq!(status_sgr("anything else"), "\x1b[0m");
        assert!(is_url("https://x") && !is_url("Max"));
    }

    #[test]
    fn board_set_needs_a_field() {
        // A key with no field=value is a clear client error, and a bad pair too.
        assert!(build_request(&v(&["board", "set", "auth"]), None)
            .unwrap_err()
            .contains("field=value"));
        assert!(build_request(&v(&["board", "set", "auth", "nofieldeq"]), None)
            .unwrap_err()
            .contains("field=value"));
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
