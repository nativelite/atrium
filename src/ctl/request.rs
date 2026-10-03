//! The request types a ctl client sends, and the server's parser for them.

use super::TrustMode;
#[cfg(doc)]
use super::{delegation_allowed, evaluate_spawn, in_subtree};
use json::Value;

/// A pane's **agent id**: the process-global key of the ctl spawn tree — the
/// value `send`/`status`/`kill` targets resolve to and the subtree-scoping
/// authorization guard ([`in_subtree`]) compares.
///
/// A newtype, not a bare `usize`, because a pane also has a per-window split-tree
/// id that IS a bare `usize`, minted from a different counter. Both used to be
/// `usize`, so passing one where the other belonged compiled cleanly and misrouted
/// an authorization check (r10 audit B4); now it is a type error. Serialized as
/// its plain number on the wire.
///
/// ```compile_fail
/// use atrium::ctl::{in_subtree, AgentId};
/// // A split-tree pane id (a bare usize) is not an agent id: this must not compile.
/// let split_tree_id: usize = 1;
/// in_subtree(split_tree_id, AgentId(0), &[]);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(pub usize);

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A parsed control request.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The agent id of the pane that issued this (from `ATRIUM_PANE`), if any. This
    /// is *self-reported* and used only as a hint; the server authenticates the
    /// caller by [`Request::token`], never by trusting this field.
    pub caller: Option<usize>,
    /// The pane's capability token (from `ATRIUM_TOKEN`). The server resolves the
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
    /// Kill a target pane's child process and relaunch it in a new working
    /// directory — optionally an ad-hoc worktree created on demand.
    Respawn(RespawnReq),
    /// A pane's mod announces itself: its version and capabilities. About the
    /// caller only ([`crate::modstate`]).
    Hello(HelloReq),
    /// A pane's mod reports its own status, usage or last answer. About the
    /// caller only ([`crate::modstate`]).
    Report(ReportReq),
    /// What is the caller: its pane, role, parent, mode, deny rules, mod and
    /// the board item briefed to its role.
    Whoami,
}

impl Cmd {
    /// Is this a read-only command — served to any caller, authenticated or not?
    /// Everything else mutates the fleet or its shared state (spawn, send, kill,
    /// board writes/claims, bus publish/subscribe/resolve) and requires an
    /// authenticated, token-matched caller.
    ///
    /// An exhaustive `match`, deliberately: it used to be a `matches!` in `main.rs`,
    /// which a new variant silently fell out of (as "not read-only" — fail-secure,
    /// but mis-gated). Now adding a command is a compile error here until its
    /// posture is decided, next to the variant (r10 audit B5).
    pub fn is_read_only(&self) -> bool {
        match self {
            Cmd::List | Cmd::Status(_) | Cmd::Audit(_) => true,
            Cmd::Spawn(_) | Cmd::Send(_) | Cmd::Kill(_) | Cmd::Respawn(_) => false,
            // A mod's reports write broker state, and `whoami` answers about
            // the caller, which an unauthenticated request does not have.
            Cmd::Hello(_) | Cmd::Report(_) | Cmd::Whoami => false,
            Cmd::Board(op) => match op {
                BoardOp::Get { .. } | BoardOp::List => true,
                BoardOp::Set { .. }
                | BoardOp::Del { .. }
                | BoardOp::Claim { .. }
                | BoardOp::Release { .. } => false,
            },
            Cmd::Bus(op) => match op {
                BusOp::Feed { .. } | BusOp::Topics => true,
                BusOp::Pub { .. }
                | BusOp::Sub { .. }
                | BusOp::Unsub { .. }
                | BusOp::Resolve { .. } => false,
            },
        }
    }

    /// The audit `(action, detail)` for a request: a stable verb plus a compact,
    /// **secret-free** description (identity *names* only; `send` logs the text
    /// *length*, never the body).
    pub fn audit_label(&self) -> (&'static str, String) {
        match self {
            Cmd::List => ("list", String::new()),
            Cmd::Status(sr) => (
                "status",
                match &sr.target {
                    Some(t) => format!("target={t}"),
                    None => "scope=subtree".to_string(),
                },
            ),
            Cmd::Send(sr) => (
                "send",
                format!("target={} len={}", sr.target, sr.text.chars().count()),
            ),
            Cmd::Spawn(sp) => {
                let stem = sp
                    .argv
                    .first()
                    .map(|a| crate::bind::command_stem(a))
                    .unwrap_or_default();
                (
                    "spawn",
                    format!(
                        "role={} argv={} identity={}",
                        sp.role.as_deref().unwrap_or("-"),
                        stem,
                        sp.identity.as_deref().unwrap_or("-")
                    ),
                )
            }
            Cmd::Kill(kr) => ("kill", format!("target={}", kr.target)),
            Cmd::Audit(_) => ("audit", String::new()),
            Cmd::Board(op) => (
                "board",
                match op {
                    BoardOp::Set { key, fields } => {
                        format!("set {key} fields={}", fields.len())
                    }
                    BoardOp::Get { key } => format!("get {key}"),
                    BoardOp::List => "list".to_string(),
                    BoardOp::Del { key } => format!("del {key}"),
                    BoardOp::Claim { key, .. } => format!("claim {key}"),
                    BoardOp::Release { key } => format!("release {key}"),
                },
            ),
            Cmd::Bus(op) => (
                "bus",
                match op {
                    BusOp::Pub {
                        topic,
                        kind,
                        fields,
                        create,
                    } => {
                        let new = if *create { " --new" } else { "" };
                        format!("pub {topic}{new} {} fields={}", kind.as_str(), fields.len())
                    }
                    BusOp::Sub { topics } => format!("sub {}", topics.join(",")),
                    BusOp::Unsub { topics } => format!("unsub {}", topics.join(",")),
                    BusOp::Feed { since } => format!("feed since={since}"),
                    BusOp::Resolve { seq } => format!("resolve {seq}"),
                    BusOp::Topics => "topics".to_string(),
                },
            ),
            Cmd::Respawn(rr) => (
                "respawn",
                format!(
                    "target={} worktree={}",
                    rr.target,
                    rr.worktree.as_deref().unwrap_or("-")
                ),
            ),
            Cmd::Hello(h) => (
                "hello",
                format!("mod={} caps={}", h.version, h.caps.join(",")),
            ),
            // The answer's length, never its text: it is the model's output.
            Cmd::Report(r) => (
                "report",
                format!(
                    "status={} context={} answer_len={}",
                    r.status.map(|s| s.label()).unwrap_or("-"),
                    r.context_pct
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    r.answer.as_ref().map(|a| a.chars().count()).unwrap_or(0)
                ),
            ),
            Cmd::Whoami => ("whoami", String::new()),
        }
    }
}

/// A `hello` request's payload: what a pane's mod says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloReq {
    /// The mod's version string.
    pub version: String,
    /// The hosting Claude Code's version, when the mod could read it.
    pub engine: Option<String>,
    /// The capabilities it offers; the server keeps those in
    /// [`crate::modstate::CAPS`].
    pub caps: Vec<String>,
}

/// A `report` request's payload: the mod's own status, usage or answer.
pub type ReportReq = crate::modstate::Report;

/// `hello` and `report` are about the caller: a request that names a target
/// is a request to speak for another pane, and is refused outright.
fn refuse_target(v: &Value, cmd: &str) -> Result<(), String> {
    if v.get("target").is_some() {
        return Err(format!(
            "{cmd} is about the caller only: it takes no target"
        ));
    }
    Ok(())
}

/// A `kill` request's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct KillReq {
    /// Pane id (numeric) or role label at the root of the subtree to tear down.
    pub target: String,
}

/// A `respawn` request's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct RespawnReq {
    /// Pane id (numeric) or role label to restart in-place.
    pub target: String,
    /// Name of an ad-hoc worktree to create (via `worktree::plan_for` +
    /// `worktree::ensure`) and use as the relaunched pane's cwd.
    pub worktree: Option<String>,
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

/// The longest lease a board claim may ask for: 365 days. The board stores
/// `now_ms + ttl_ms`, so an unbounded ttl overflows the lease; a year is
/// already far past any task a lease exists to time out.
pub const MAX_TTL_MS: u64 = 365 * 24 * 60 * 60 * 1000;

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
    /// (`None` ⇒ the board's [`crate::board::DEFAULT_LEASE_MS`]), clamped to
    /// [`MAX_TTL_MS`]. The owner is
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
        /// The `--new` intent: the caller means to create a not-yet-seen topic.
        /// Only meaningful under the soft-gate (no declared vocabulary); a
        /// declared fleet ignores it (no `--new` escape).
        create: bool,
    },
    /// Subscribe the caller to `topics` (merged); `*` is the firehose.
    Sub { topics: Vec<String> },
    /// Unsubscribe the caller from `topics`; empty ⇒ drop all its subscriptions.
    Unsub { topics: Vec<String> },
    /// Pull the caller's subscribed events with `seq > since` (no echo).
    Feed { since: u64 },
    /// Mark a `decision_needed` event answered.
    Resolve { seq: u64 },
    /// List the active topics with their subscriber counts — discoverability, so
    /// a publisher can see which topics exist and who is actually listening before
    /// firing into the void.
    Topics,
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
    /// The permission mode this spawn requested
    /// (`--mode plan|accept|automode|skip`),
    /// if any. `None` ⇒ inherit the session policy (the launch `--trust`). When
    /// set, atrium honors it for the operator (root pane) and, for a non-operator
    /// worker, caps it at the session policy — a worker may match or de-escalate
    /// but never elevate ([`TrustMode::rank`]).
    pub mode: Option<TrustMode>,
    /// Name of an ad-hoc worktree to create and use as the pane's cwd
    /// (`--worktree <name>`). When set, atrium calls `worktree::plan_for` +
    /// `worktree::ensure` before spawning and passes `plan.dir` as cwd.
    pub worktree: Option<String>,
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
            let identity = v
                .get("identity")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mode = v
                .get("mode")
                .and_then(Value::as_str)
                .and_then(TrustMode::from_policy_keyword);
            // An unparseable mode used to collapse to `None`, which is
            // indistinguishable from "no mode given" — so it INHERITED THE
            // SESSION POLICY and failed open toward the more permissive setting.
            // The client rejects unknown modes, so only a hand-rolled or hostile
            // request reaches here, which is exactly the caller not to be lenient
            // with. Present-but-unparseable is now an error, not an absence.
            if mode.is_none() {
                if let Some(raw) = v.get("mode").and_then(Value::as_str) {
                    return Err(format!(
                        "unknown mode {raw:?} (use plan, accept, automode or skip)"
                    ));
                }
            }
            Cmd::Spawn(SpawnReq {
                role,
                argv,
                new_window,
                identity,
                mode,
                worktree: v
                    .get("worktree")
                    .and_then(Value::as_str)
                    .map(str::to_string),
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
        Some("respawn") => {
            let target = v
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| "respawn needs a target".to_string())?
                .to_string();
            let worktree = v
                .get("worktree")
                .and_then(Value::as_str)
                .map(str::to_string);
            Cmd::Respawn(RespawnReq { target, worktree })
        }
        Some("audit") => {
            let tail = v
                .get("tail")
                .and_then(Value::as_i64)
                .map(|n| n.max(0) as usize);
            Cmd::Audit(AuditReq { tail })
        }
        Some("hello") => {
            refuse_target(&v, "hello")?;
            let version = v
                .get("mod")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "hello needs mod=<version>".to_string())?
                .to_string();
            let engine = v.get("engine").and_then(Value::as_str).map(str::to_string);
            let caps = match v.get("caps") {
                None => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|it| it.as_str().map(str::to_string))
                    .collect::<Option<Vec<String>>>()
                    .ok_or_else(|| "caps must be an array of strings".to_string())?,
                Some(Value::String(list)) => list
                    .split(',')
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .collect(),
                Some(_) => return Err("caps must be an array of strings".to_string()),
            };
            Cmd::Hello(HelloReq {
                version,
                engine,
                caps,
            })
        }
        Some("report") => {
            refuse_target(&v, "report")?;
            let status = match v.get("status").and_then(Value::as_str) {
                Some(label) => Some(crate::modstate::ModStatus::parse(label).ok_or_else(|| {
                    format!(
                        "unknown status {label:?} (use working, waiting-approval, \
                         waiting-prompt, idle, errored or ended)"
                    )
                })?),
                None => None,
            };
            let reason = v.get("reason").and_then(Value::as_str).map(str::to_string);
            let context_pct = match v.get("context") {
                None => None,
                Some(n) => Some(
                    n.as_i64()
                        .filter(|p| (0..=100).contains(p))
                        .map(|p| p as u8)
                        .ok_or_else(|| "context must be a whole number 0..100".to_string())?,
                ),
            };
            let cost_usd = match v.get("cost") {
                None => None,
                Some(n) => Some(
                    n.as_f64()
                        .filter(|c| c.is_finite() && *c >= 0.0)
                        .ok_or_else(|| "cost must be a non-negative number".to_string())?,
                ),
            };
            let turns = match v.get("turns") {
                None => None,
                Some(n) => Some(
                    n.as_i64()
                        .filter(|t| *t >= 0)
                        .map(|t| t as u64)
                        .ok_or_else(|| "turns must be a whole number".to_string())?,
                ),
            };
            let answer = v.get("answer").and_then(Value::as_str).map(str::to_string);
            let r = ReportReq {
                status,
                reason,
                context_pct,
                cost_usd,
                turns,
                answer,
            };
            if r == ReportReq::default() {
                return Err(
                    "report needs at least one of status, reason, context, cost, turns or answer"
                        .to_string(),
                );
            }
            Cmd::Report(r)
        }
        Some("whoami") => Cmd::Whoami,
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
                    BoardOp::Set {
                        key: key()?,
                        fields,
                    }
                }
                Some("get") => BoardOp::Get { key: key()? },
                Some("list") => BoardOp::List,
                Some("del") => BoardOp::Del { key: key()? },
                Some("claim") => {
                    let ttl_ms = v
                        .get("ttl_ms")
                        .and_then(Value::as_i64)
                        .filter(|n| *n > 0)
                        .map(|n| (n as u64).min(MAX_TTL_MS));
                    BoardOp::Claim {
                        key: key()?,
                        ttl_ms,
                    }
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
                    let create = v.get("create").and_then(Value::as_bool).unwrap_or(false);
                    BusOp::Pub {
                        topic,
                        kind,
                        fields,
                        create,
                    }
                }
                Some("sub") | Some("unsub") => {
                    let topics = v
                        .get("topics")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|t| t.as_str().map(str::to_string))
                                .collect()
                        })
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
                Some("topics") => BusOp::Topics,
                other => {
                    return Err(format!(
                        "bus op must be pub|sub|unsub|feed|resolve|topics (got {other:?})"
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
