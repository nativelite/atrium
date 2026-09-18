//! The control-plane server: turn a `ctl` request line into its typed
//! [`atrium::ctl::Reply`] against the live windows, and flush queued `ctl send`
//! deliveries. Moved out of `main.rs` verbatim (r10 audit B11) — the run loop
//! calls in here; nothing here owns the terminal.

use crate::*;

/// A `ctl send` awaiting delivery. The design queues a task until the target is
/// **idle** (agsess-gated) rather than injecting into a live turn (Decision 4).
/// Once the target is ready we write the text, then — after a short beat so the
/// agent's TUI registers the line before the submit — write the Enter. The beat
/// mirrors the C0 spike, which split the text and `\r` with a delay.
pub(crate) struct PendingSend {
    /// Target pane's global agent id (already resolved + scope-checked).
    target: AgentId,
    text: String,
    /// When the send was accepted — a fallback so a target that never yields a
    /// derivable status (a shell, a not-yet-bound agent) still gets it.
    queued_at: Instant,
    /// How many bytes of `text` the target's pty has actually accepted.
    ///
    /// `write(2)` returns a COUNT and may legitimately take less than offered —
    /// a pty's input buffer is finite, and in canonical mode a single line is
    /// capped near 1 KB. The old code discarded that count, marked the send
    /// delivered, and submitted Enter regardless, so a long task arrived as a
    /// fragment while `ctl` had already replied `ok:true`.
    written: usize,
    /// `Some(t)` once the text has been written IN FULL; `t` gates the follow-up
    /// Enter. Never set on a partial write — Enter must not submit a fragment.
    text_written_at: Option<Instant>,
}

/// How long after writing the task text we send the Enter that submits it.
pub(crate) const SEND_ENTER_DELAY: Duration = Duration::from_millis(400);

/// If a target never yields a derivable agsess status (non-agent / unbound),
/// deliver anyway once the send has waited this long, so a queue never wedges.
pub(crate) const SEND_UNBOUND_FALLBACK: Duration = Duration::from_secs(2);

/// Most undelivered sends one target may hold. A pane wedged on an open dialog
/// never takes delivery, and each send can be a max-size ctl request, so an
/// uncapped queue grows for as long as the pane stays wedged. 32 is far past any
/// real backlog and bounds one target at about 2 MiB.
pub(crate) const MAX_PENDING_PER_TARGET: usize = 32;

/// Queue `text` for `target` unless that target already holds
/// [`MAX_PENDING_PER_TARGET`] undelivered sends. Returns whether it was queued.
fn queue_send(pending: &mut Vec<PendingSend>, target: AgentId, text: String) -> bool {
    if pending.iter().filter(|ps| ps.target == target).count() >= MAX_PENDING_PER_TARGET {
        return false;
    }
    pending.push(PendingSend {
        target,
        text,
        queued_at: Instant::now(),
        written: 0,
        text_written_at: None,
    });
    true
}

/// Point every parent link that names `old` at `new`. A respawn mints its pane a
/// new agent id; its workers must follow, or they fall out of its subtree. Pure.
pub(crate) fn repoint_parents<'a>(
    links: impl Iterator<Item = &'a mut Option<AgentId>>,
    old: AgentId,
    new: AgentId,
) {
    for link in links.filter(|l| **l == Some(old)) {
        *link = Some(new);
    }
}

/// Hand the sends queued for a respawned pane's old id to its new one, each from
/// its first byte: whatever part reached the killed process died with it. The
/// wait restarts too, so a new process that has no status yet gets the same grace
/// before delivery as a freshly spawned one, not a send already past it.
fn retarget_sends(pending: &mut [PendingSend], old: AgentId, new: AgentId, now: Instant) {
    for ps in pending.iter_mut().filter(|ps| ps.target == old) {
        ps.target = new;
        ps.written = 0;
        ps.text_written_at = None;
        ps.queued_at = now;
    }
}

/// Find a hosted pane by its global agent id (immutable / mutable).
pub(crate) fn pane_by_agent(windows: &[Window], id: AgentId) -> Option<&Pane> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .find(|p| p.agent_id == id)
}

pub(crate) fn pane_by_agent_mut(windows: &mut [Window], id: AgentId) -> Option<&mut Pane> {
    windows
        .iter_mut()
        .flat_map(|w| w.panes.iter_mut())
        .find(|p| p.agent_id == id)
}

/// `(agent_id, role)` for every live pane — the candidate set for target
/// resolution.
pub(crate) fn ctl_candidates(windows: &[Window]) -> Vec<(AgentId, Option<String>)> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| (p.agent_id, p.role.clone()))
        .collect()
}

/// `(agent_id, parent)` for every live pane — the spawn-tree edges the subtree
/// guard walks.
pub(crate) fn ctl_parents(windows: &[Window]) -> Vec<(AgentId, Option<AgentId>)> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| (p.agent_id, p.parent))
        .collect()
}

/// The **human** controls everything (Decision 3). A caller is human-privileged
/// when it has no attributed pane, or when its pane is a *root* (one atrium opened,
/// `parent == None`, depth 0) — i.e. where the operator sits. A spawned worker
/// (depth > 0) is scoped to its own subtree.
pub(crate) fn caller_privileged(windows: &[Window], caller: Option<AgentId>) -> bool {
    // Look the caller up, then decide in a pure function so the DECISION can be
    // tested without constructing panes (each of which needs a live pty). This
    // gate had no tests at all: when its two `None` arms were flipped from
    // `true` to `false` — a security fix — nothing in the suite changed, which is
    // precisely the problem.
    let pane_root = caller
        .and_then(|id| pane_by_agent(windows, id))
        .map(|p| (p.parent, p.depth));
    privilege_for(caller, pane_root)
}

/// Resolve a spawn's permission mode against the session policy.
///
/// The session policy is a CEILING, and it now holds for everyone — including a
/// root pane. It used to be bypassed outright for a "privileged" caller
/// (`Some(req) if privileged => req`), on the reading that the root pane is the
/// human and the human may elevate. Two things break that:
///
/// 1. `-n N` and `--grid` give every pane no parent, so EVERY pane in a
///    mass-spawned session was classified as the operator. An agent could then
///    ask for `skip` — a full permission bypass — in a session the human had
///    deliberately set to `plan`.
/// 2. atrium's own documentation calls `--trust` "the mode spawned agents run in,
///    and the ceiling they are capped at". A ceiling that a pane can exceed is
///    not a ceiling, and the human cannot tell from the outside which panes could.
///
/// So the bypass is gone. `--mode` may still DE-ESCALATE freely (asking for less
/// than the policy is always allowed), which is the useful half; it can no longer
/// escalate. To run agents at a higher posture, the human sets it at launch,
/// where it is a visible, deliberate choice rather than something a pane can
/// request. Returns the effective mode and a note when a request was capped.
pub(crate) fn effective_mode(
    requested: Option<atrium::ctl::TrustMode>,
    policy: atrium::ctl::TrustMode,
) -> (atrium::ctl::TrustMode, Option<String>) {
    match requested {
        None => (policy, None),
        Some(req) if req.rank() <= policy.rank() => (req, None),
        Some(req) => (
            policy,
            Some(format!(
                "capped to {} (session policy); nothing may elevate itself to {} \
                 — set the policy at launch with --trust",
                policy.policy_label(),
                req.policy_label()
            )),
        ),
    }
}

/// The privilege decision, given only what it needs.
///
/// `pane_parent` says what the caller's capability token resolved to:
/// - `None` — no such live pane (a stale or forged token), or no caller at all
/// - `Some((None, 0))` — a live root pane the human opened
/// - `Some((Some(_), _))` — a live pane spawned by another: a worker
/// - `Some((None, d))` with `d > 0` — a worker that has *lost* its parent link
///
/// That last case is why the depth is part of the decision and not just the
/// parent. A worker's parent link can go missing: `atrium recover` resolves each
/// worker's parent through the pane that spawned it, and a parent which had
/// already exited (so it is no longer in the window) resolves to `None`. Keying
/// the gate on the parent alone then promoted that worker to **operator** on the
/// next recovery — handing it session-wide `send`/`kill`/`respawn` and the right
/// to delegate any identity in the vault. Depth is recorded independently, so
/// requiring both closes it. The doc above has described a root as "parent ==
/// None, depth 0" since this gate was written; this is the code catching up.
pub(crate) fn privilege_for(
    caller: Option<AgentId>,
    pane_root: Option<(Option<AgentId>, usize)>,
) -> bool {
    match caller {
        // No authenticated caller is NOT the operator. This arm used to return
        // `true`, which inverted the gate: holding no credential granted strictly
        // more than holding a worker's, so `env -u ATRIUM_TOKEN atrium ctl ...`
        // promoted you. `caller` is derived from the capability token, never
        // self-reported, so `None` means exactly "unauthenticated" and must be
        // the least trusted state, not the most.
        None => false,
        Some(_) => match pane_root {
            Some((parent, depth)) => parent.is_none() && depth == 0,
            // A token resolving to no live pane is stale or forged, not the
            // operator. Same inversion as above.
            None => false,
        },
    }
}

/// The agsess status label the ctl protocol reports (stable strings the calling
/// agent can match on).
pub(crate) fn status_label(s: agsess::Status) -> &'static str {
    match s {
        agsess::Status::Working => "working",
        agsess::Status::WaitingApproval => "waiting-approval",
        agsess::Status::WaitingPrompt => "waiting-prompt",
        agsess::Status::Idle => "idle",
    }
}

/// Apply one ctl request against the live window set and return the JSON reply
/// line, **recording it to the audit log** (design §5). Thin wrapper: parse,
/// serve `audit` reads directly (they need the log and are not self-recorded),
/// else [`dispatch_ctl`] the request and record its outcome. `spawn`/`send`/
/// `status`/`kill` with a target are subtree-scoped; `spawn --identity` is
/// delegation-scoped.
#[allow(clippy::too_many_arguments)]
/// Is this decision addressed to a live teammate (a `to=<role>` field naming a
/// pane that exists)? Such a decision routes to that agent — the human isn't
/// urgently pinged for it — implementing worker→lead escalation before lead→human.
/// A `to` naming no live pane, or no `to` at all, is human-facing.
pub(crate) fn decision_for_agent(e: &atrium::bus::Event, windows: &[Window]) -> bool {
    e.fields.get("to").is_some_and(|to| {
        windows
            .iter()
            .flat_map(|w| &w.panes)
            .any(|p| p.role.as_deref() == Some(to.as_str()))
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_ctl(
    line: &str,
    windows: &mut Vec<Window>,
    rows: u16,
    cols: u16,
    max_depth: usize,
    extra_allow: &[String],
    pending: &mut Vec<PendingSend>,
    world: &atrium::vendors::AgentState,
    session_identity: Option<&str>,
    audit: &mut atrium::audit::Audit,
    board: &mut atrium::board::Board,
    bus: &mut atrium::bus::Bus,
    job: &atrium::reap::SessionJob,
) -> atrium::ctl::Reply {
    use atrium::ctl::{self, Cmd};

    let req = match ctl::parse_request(line) {
        Ok(r) => r,
        Err(e) => {
            let reply = ctl::reply_err(&e);
            // NEVER log the raw request. `build_request` emits
            // `{"caller":N,"token":"` - a 21-character prefix - so an 80-char
            // truncation wrote 59 of a 64-character token into a log that
            // `ctl audit` hands to an unauthenticated reader. Record the shape
            // and the parse error, which is what debugging actually needs, and
            // nothing that was in the payload.
            audit.record(
                None,
                "bad-request",
                &format!("<unparseable, {} bytes>", line.len()),
                false,
                &e,
            );
            return reply;
        }
    };
    // Authenticate the caller by its capability token, not its self-reported id:
    // the caller *is* the pane whose minted token matches. This overwrites the
    // self-reported `caller`, so a pane cannot claim another pane's id or claim
    // operator (its own token forces its real, depth-scoped identity). A request
    // with no token or a non-matching one is unauthenticated. Every pane atrium
    // spawns in a ctl session is born with a token (the endpoint binds before any
    // spawn), so a legitimate caller is never locked out; only an external or
    // token-stripped request is. See §4.1 of the whitepaper for the threat model
    // and the peer-credential hardening that closes the remaining raw-socket vector.
    let authed = req
        .token
        .as_deref()
        .filter(|t| !t.is_empty())
        .and_then(|tok| {
            windows
                .iter()
                .flat_map(|w| &w.panes)
                // `as_deref()` so a pane with no token (entropy failure at
                // spawn) can never match — `None == None` must not authenticate.
                .find(|p| p.token.as_deref() == Some(tok))
                .map(|p| p.agent_id)
        });
    let authenticated = authed.is_some();
    // Reads (list/status/audit/board get/list/bus feed) are open; anything that
    // mutates the fleet or its shared state requires an authenticated caller.
    if !authenticated && !req.cmd.is_read_only() {
        let reply = ctl::reply_err(
            "unauthenticated: control request carries no valid pane token (ATRIUM_TOKEN)",
        );
        let (action, detail) = req.cmd.audit_label();
        audit.record(None, action, &detail, false, "unauthenticated");
        return reply;
    }
    // The AUTHENTICATED caller, typed apart from the request's self-reported
    // `caller: Option<usize>` hint so the two can never be confused (r10 B4).
    let caller: Option<AgentId> = authed;
    let privileged = caller_privileged(windows, caller);

    // `audit` reads the log, and IS recorded. It used to be exempt on the
    // reasoning that a query shouldn't pollute what it queries — but that made
    // the most sensitive read the only one leaving no trace, so exfiltrating the
    // log was invisible after the fact. The entry is written before the reply is
    // built, so a reader sees its own access: self-documenting, not hidden.
    // Subtree-scoped like any read: a worker sees only its own subtree.
    if let Cmd::Audit(ar) = &req.cmd {
        audit.record(caller, "audit", "read", true, "");
        return audit_reply(audit, windows, caller, privileged, ar.tail);
    }

    let (action, detail) = req.cmd.audit_label();
    let reply = dispatch_ctl(
        req,
        caller,
        windows,
        rows,
        cols,
        max_depth,
        extra_allow,
        pending,
        world,
        session_identity,
        privileged,
        board,
        bus,
        job,
    );
    let (ok, note) = audit_outcome(&reply);
    audit.record(caller, action, &detail, ok, &note);
    reply
}

/// A bus message carrying a `to` field is a directed hand-off. The bus is
/// pull-based, so an idle target never sees it until it runs `bus feed` — and if
/// it isn't subscribed to that topic, not even then. So a routed publish also
/// queues a best-effort wake that carries the content itself. Returns
/// `(target-role, wake-text)` when the message is routed, else `None`. Pure: the
/// role resolution and scope-gating stay at the call site, where a routed wake
/// gets the same scope check as `ctl send` and so grants no new reach.
pub(crate) fn routed_wake(
    fields: &std::collections::BTreeMap<String, String>,
    topic: &str,
    seq: u64,
    kind: atrium::bus::Kind,
    who: &str,
) -> Option<(String, String)> {
    let to = fields.get("to")?;
    // The human-readable payload lives in `msg` (fyi) or `q` (a decision); fall
    // back to a pointer if a routed message carried neither.
    let body = fields
        .get("msg")
        .or_else(|| fields.get("q"))
        .map(String::as_str)
        .unwrap_or("(see: atrium ctl bus feed)");
    // The terse-headline convention: long evidence rides a `detail=<pointer>`
    // (board key / path / URL), surfaced after the headline so the target knows
    // where to look without the bus line carrying the whole payload.
    let tail = fields
        .get("detail")
        .map(|d| format!(" (detail: {d})"))
        .unwrap_or_default();
    let text = format!(
        "[atrium bus #{seq} {} / {who} -> you on \"{topic}\"] {body}{tail}",
        kind.as_str()
    );
    Some((to.clone(), text))
}

/// The server side of the control channel: turn one parsed [`atrium::ctl::Request`]
/// into its JSON reply. `list`/`status` serialize the spawn tree (agsess status
/// folded in); `spawn` opens a visible worker after the pure allowlist/depth
/// guard ([`atrium::ctl::evaluate_spawn`]) and the credential-delegation guard
/// ([`atrium::ctl::delegation_allowed`]); `send` enqueues a queue-until-idle
/// delivery; `kill` tears down the target's subtree (the reap step reaps them).
/// `send`/`status`/`kill` with a target are subtree-scoped.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_ctl(
    req: atrium::ctl::Request,
    caller: Option<AgentId>,
    windows: &mut Vec<Window>,
    rows: u16,
    cols: u16,
    max_depth: usize,
    extra_allow: &[String],
    pending: &mut Vec<PendingSend>,
    world: &atrium::vendors::AgentState,
    session_identity: Option<&str>,
    privileged: bool,
    board: &mut atrium::board::Board,
    bus: &mut atrium::bus::Bus,
    job: &atrium::reap::SessionJob,
) -> atrium::ctl::Reply {
    use atrium::ctl::{self, Cmd};

    match req.cmd {
        Cmd::List => reply_tree(windows, world, None),
        Cmd::Status(sr) => match sr.target {
            None => {
                // No target: the caller's subtree (whole tree for the operator).
                let root = if privileged { None } else { caller };
                reply_tree(windows, world, root)
            }
            Some(t) => {
                let candidates = ctl_candidates(windows);
                let id = match ctl::resolve_target(&t, &candidates) {
                    Ok(id) => id,
                    Err(e) => return ctl::reply_err(&e),
                };
                if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                    return deny;
                }
                let pane = pane_by_agent(windows, id);
                let status =
                    pane.and_then(|p| world.status_for(p.session_id.as_deref()).map(status_label));
                let idle = pane
                    .map(|p| atrium::ipc::idle_ms(p.last_activity, std::time::Instant::now()))
                    .unwrap_or(0);
                ctl::reply_status_one(id, status, idle)
            }
        },
        Cmd::Send(sr) => {
            let candidates = ctl_candidates(windows);
            let id = match ctl::resolve_target(&sr.target, &candidates) {
                Ok(id) => id,
                Err(e) => return ctl::reply_err(&e),
            };
            if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                return deny;
            }
            // Queued iff the target is mid-turn now; either way delivery is async
            // and happens when the target is idle.
            let busy = matches!(
                pane_by_agent(windows, id).and_then(|p| world.status_for(p.session_id.as_deref())),
                Some(agsess::Status::Working) | Some(agsess::Status::WaitingApproval)
            );
            if !queue_send(pending, id, sr.text) {
                return ctl::reply_err(&format!(
                    "agent {id} already has {MAX_PENDING_PER_TARGET} undelivered sends; \
                     it is not taking delivery (busy, a dialog open, or a draft). \
                     Check `ctl status` before sending more"
                ));
            }
            ctl::reply_sent(id, busy)
        }
        Cmd::Spawn(mut sp) => {
            // Capability check, FIRST. Creating teammates used to be implied by
            // having ctl access at all - so every agent in a fleet could do it,
            // and in a seven-agent review fleet all seven could, when only the
            // lead should. A depth cap bounds how FAR fan-out goes; it never says
            // who may start it.
            //
            // Before the depth cap and the trust ceiling, not instead of them: a
            // pane that may spawn is still capped on both.
            if let Some(c) = caller {
                let allowed = pane_by_agent(windows, c)
                    .map(|p| p.can_spawn)
                    .unwrap_or(false);
                if !allowed {
                    return ctl::reply_err(
                        "this agent is not permitted to create teammates \
                         (set \"can_spawn\": true for it in the fleet file)",
                    );
                }
            }
            // atrium owns the permission posture. Two layers, both surfaced (never
            // silent) in the reply `note`:
            //
            //  1. Strip RAW claude permission flags the agent slipped into the argv
            //     (`--dangerously-skip-permissions`, `--permission-mode`, …). Agents
            //     request a mode through `--mode`, not raw flags, so atrium stays the
            //     single source of truth.
            //  2. Resolve the effective mode from the per-spawn `--mode` under the
            //     session policy: the **operator** (the human's root pane) may set
            //     ANY mode (elevation is the human directing); a non-operator
            //     **worker** is capped at the policy — it may match or de-escalate
            //     but never elevate itself. No `--mode` ⇒ inherit the policy.
            // Vet before anything else: an unknown vendor or a flag a teammate
            // may not choose is refused outright, not quietly cleaned. Stripping
            // only ever covered three names, and the surface it guards — MCP
            // servers, plugin dirs, settings files, all of which execute code at
            // startup — is far larger than that.
            let stripped = match ctl::vet_spawn_argv(&sp.argv) {
                ctl::ArgvVerdict::Refused(why) => return ctl::reply_err(&why),
                ctl::ArgvVerdict::Ok { argv, stripped } => {
                    sp.argv = argv;
                    stripped
                }
            };
            let mut notes: Vec<String> = Vec::new();
            if !stripped.is_empty() {
                notes.push(format!(
                    "ignored raw {} — request a mode with --mode instead",
                    stripped.join(", ")
                ));
            }
            let policy = trust_mode();
            let (effective, cap_note) = effective_mode(sp.mode, policy);
            if let Some(n) = cap_note {
                notes.push(n);
            }
            let note = if notes.is_empty() {
                None
            } else {
                Some(notes.join("; "))
            };
            let note = note.as_deref();
            let caller_depth = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .map(|p| p.depth)
                .unwrap_or(0);
            let new_depth =
                match ctl::evaluate_spawn(&sp.argv, caller_depth, max_depth, extra_allow) {
                    Ok(d) => d,
                    Err(denied) => return ctl::reply_err(&denied.message()),
                };
            // Pane cap (host-resource guard): the depth guard bounds recursion; this
            // bounds total *breadth* so a runaway fan-out can't exhaust the machine
            // (agent processes dominate RAM, not atrium). Host-derived, overridable
            // with ATRIUM_MAX_PANES.
            let live = windows.iter().map(|w| w.panes.len()).sum::<usize>();
            let cap = atrium::resources::effective_cap();
            if live >= cap {
                return ctl::reply_err(&format!(
                    "pane cap reached ({live}/{cap}) — reap an agent or raise it with ATRIUM_MAX_PANES"
                ));
            }
            // Credential-delegation guard (§5): a worker may only pass down an
            // identity it itself holds (its own or the session default); the
            // operator delegates anything.
            let caller_identity = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .and_then(|p| p.identity.clone());
            if let Err(msg) = ctl::delegation_allowed(
                sp.identity.as_deref(),
                caller_identity.as_deref(),
                session_identity,
                privileged,
            ) {
                return ctl::reply_err(&msg);
            }
            if sp.new_window {
                spawn_worker_window(
                    windows, &sp, caller, new_depth, rows, cols, effective, note, job,
                )
            } else {
                spawn_worker_here(
                    windows, &sp, caller, new_depth, rows, cols, effective, note, job,
                )
            }
        }
        Cmd::Kill(kr) => {
            let candidates = ctl_candidates(windows);
            let id = match ctl::resolve_target(&kr.target, &candidates) {
                Ok(id) => id,
                Err(e) => return ctl::reply_err(&e),
            };
            if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                return deny;
            }
            // Tear down the target and every descendant. We only mark the panes
            // dead (kill the pty, set `exited`); the run loop's reap step (§4)
            // then collapses the split trees, drops the panes, and removes any
            // window left empty — the same proven path an interactive `x` uses.
            let parents = ctl_parents(windows);
            let mut killed: Vec<AgentId> = Vec::new();
            for w in windows.iter_mut() {
                for p in w.panes.iter_mut() {
                    if atrium::ctl::in_subtree(p.agent_id, id, &parents) {
                        let _ = p.pty.kill();
                        p.exited = true;
                        killed.push(p.agent_id);
                    }
                }
            }
            killed.sort_unstable();
            ctl::reply_killed(&killed)
        }
        Cmd::Respawn(rr) => {
            let candidates = ctl_candidates(windows);
            let id = match ctl::resolve_target(&rr.target, &candidates) {
                Ok(id) => id,
                Err(e) => return ctl::reply_err(&e),
            };
            if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                return deny;
            }
            // Capture the info we need before mutating the pane.
            let (pane_slot_id, cmd, identity_name, deny, mode, context_env, place) = {
                let Some(p) = pane_by_agent(windows, id) else {
                    return ctl::reply_err("respawn: target pane not found");
                };
                (
                    p.id,
                    respawn_argv(&p.argv, &p.title),
                    p.identity.clone(),
                    p.deny.clone(),
                    p.mode,
                    p.context_env.clone(),
                    (p.cwd.clone(), p.norms.clone(), p.worktree.clone()),
                )
            };
            // Where the new process runs. A named worktree is created (idempotent)
            // and used; otherwise the pane restarts where it was, with its own
            // worktree instructions. It used to land in atrium's cwd with none, so
            // restarting a worktree agent moved it onto the main tree.
            let (new_cwd, norms, worktree) = match rr.worktree.as_deref() {
                Some(name) => {
                    let cwd =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    match worktree_spawn_params(&cwd, Some(name)) {
                        Ok((c, n)) => (c, n, Some(name.to_string())),
                        Err(e) => return ctl::reply_err(&format!("spawn failed: {e}")),
                    }
                }
                None => place,
            };
            // The replacement is born at its tile's inner size, not the
            // terminal's. Spawned at the terminal's size, its emulator was larger
            // than the tile it is blitted into: the text was clipped on the right
            // and the bottom rows — claude's input box — sat outside the copied
            // rectangle until a manual terminal resize re-tiled the window.
            let Some(wi) = windows
                .iter()
                .position(|w| w.panes.iter().any(|p| p.agent_id == id))
            else {
                return ctl::reply_err("respawn: target pane not found");
            };
            let (pane_rows, pane_cols) = pane_inner_size(
                windows[wi].tiled(),
                &windows[wi].tree,
                pane_slot_id,
                rows,
                cols,
            );
            let mut flash = None;
            let new_pane = match spawn_pane_full(
                PaneSpec {
                    command: &cmd,
                    id: pane_slot_id,
                    identity: identity_name.as_deref(),
                    cwd: new_cwd.as_deref(),
                    // The pane's own posture and deny rules survive a respawn. They
                    // used to be replaced by the session ceiling and no rules at all,
                    // so respawning a restricted fleet agent quietly lifted its limits.
                    mode: effective_mode(Some(mode), trust_mode()).0,
                    // The pane's context store, so a restarted agent keeps its
                    // knowledge base (a fresh session, not a fresh memory).
                    extra_env: &context_env,
                    extra_norms: norms.as_deref(),
                    deny: &deny,
                },
                pane_rows,
                pane_cols,
                &mut flash,
            ) {
                Ok(p) => p,
                Err(e) => return ctl::reply_err(&format!("respawn failed: {e}")),
            };
            job.assign(new_pane.pty.pid());
            // In-place replacement: kill the old child then swap in the new pty,
            // term, and session fields. Role/parent/depth/can_spawn are preserved.
            let p = pane_by_agent_mut(windows, id).unwrap();
            let _ = p.pty.kill();
            // The old reader goes with the old child, and before its `Pty` drops
            // (see `Pane::inbox`). Kept, it went on reading the dead pty, and since
            // the loop only starts a reader for a pane with none, nothing the new
            // process wrote was ever shown.
            p.inbox = None;
            p.pty = new_pane.pty;
            p.term = new_pane.term;
            p.filter = new_pane.filter;
            p.exited = false;
            p.session_id = new_pane.session_id.clone();
            p.launch_ms = new_pane.launch_ms;
            p.cwd = new_pane.cwd;
            // What the snapshot records must describe the process now running: its
            // command, where it runs, its worktree instructions and context
            // variables. The kickoff is kept: a respawn starts a new session, and a
            // fresh fleet agent needs its opening instructions (the same rule a
            // recovery follows when it cannot resume a transcript).
            p.argv = new_pane.argv;
            p.norms = new_pane.norms;
            p.context_env = new_pane.context_env;
            p.worktree = worktree;
            p.mode = new_pane.mode;
            p.agent_id = new_pane.agent_id;
            p.token = new_pane.token;
            p.activity = false;
            p.painted = false;
            let new_id = p.agent_id;
            let role = p.role.clone();
            // Re-tile the window so the new pane (and every sibling) holds its
            // exact inner rect — the backstop `ctl spawn --here` already relies on.
            resize_window(&mut windows[wi], rows, cols);
            repoint_parents(
                windows
                    .iter_mut()
                    .flat_map(|w| w.panes.iter_mut())
                    .map(|p| &mut p.parent),
                id,
                new_id,
            );
            retarget_sends(pending, id, new_id, Instant::now());
            ctl::reply_spawned(
                new_id,
                role.as_deref(),
                new_pane.session_id.as_deref(),
                None,
            )
        }
        // `audit` is handled in `apply_ctl` (it needs the log); never reaches here.
        Cmd::Audit(_) => ctl::reply_err("internal: audit dispatched to the wrong handler"),
        Cmd::Board(op) => {
            // The board is SHARED team state: any pane in the session reads and
            // writes the same source of truth (no subtree scoping — coordination
            // is the team's job, and `updated_by` + the audit log keep it
            // accountable). Single-writer daemon ⇒ no locking.
            let by = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .map(|p| {
                    p.role
                        .clone()
                        // 1-based to match the status bar's `1:`, `2:` numbering
                        // (agent_id is 0-based internally). Roles are the stable
                        // identity; this is the human-friendly fallback label.
                        .unwrap_or_else(|| format!("pane {}", p.agent_id.0 + 1))
                })
                .or_else(|| Some("operator".to_string()));
            match op {
                ctl::BoardOp::Set { key, fields } => {
                    let e = board.set(&key, &fields, by.as_deref(), agsess::sessions::now_ms());
                    ctl::reply_board_entry(&key, Some(atrium::board::entry_to_value(&e)))
                }
                ctl::BoardOp::Get { key } => {
                    let entry = board.get(&key).map(atrium::board::entry_to_value);
                    ctl::reply_board_entry(&key, entry)
                }
                ctl::BoardOp::List => {
                    ctl::reply_board_list(atrium::board::entries_to_value(&board.list()))
                }
                ctl::BoardOp::Del { key } => {
                    let deleted = board.del(&key);
                    ctl::reply_board_del(&key, deleted)
                }
                ctl::BoardOp::Claim { key, ttl_ms } => {
                    // The owner is the caller (`by`), derived server-side — a
                    // worker can't claim as someone else. Default the lease to the
                    // board's DEFAULT_LEASE_MS unless the caller set --ttl.
                    let owner = by.as_deref().unwrap_or("operator");
                    let ttl = ttl_ms.unwrap_or(atrium::board::DEFAULT_LEASE_MS);
                    let outcome = board.claim(&key, owner, ttl, agsess::sessions::now_ms());
                    ctl::reply_board_claim(&key, &outcome)
                }
                ctl::BoardOp::Release { key } => {
                    let released = board.release(&key, agsess::sessions::now_ms());
                    ctl::reply_board_release(&key, released)
                }
            }
        }
        Cmd::Bus(op) => {
            // The bus is SHARED like the board. The server derives `who` (the
            // caller's role, else its pane, else the operator) so a worker
            // publishes and subscribes *as itself* — it cannot forge another
            // sender. `who` is stable per pane, so the no-echo filter and a
            // subscriber's cursor stay consistent across calls.
            let who = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .map(|p| {
                    p.role
                        .clone()
                        // 1-based to match the status bar's `1:`, `2:` numbering
                        // (agent_id is 0-based internally). Roles are the stable
                        // identity; this is the human-friendly fallback label.
                        .unwrap_or_else(|| format!("pane {}", p.agent_id.0 + 1))
                })
                .unwrap_or_else(|| "operator".to_string());
            let now = agsess::sessions::now_ms();
            match op {
                ctl::BusOp::Pub {
                    topic,
                    kind,
                    fields,
                    create,
                } => match bus
                    .admit_topic(&topic, create)
                    .and_then(|()| bus.publish(&topic, kind, Some(&who), &fields, now))
                {
                    Ok(e) => {
                        // No-echo-accurate: how many *other* subscribers receive
                        // it (the publisher is excluded), so the client can warn on
                        // a publish that reached nobody.
                        let subs = bus.subscriber_count(&e.topic, Some(&who));
                        // A message routed to a role (`--to`) is DELIVERED, not just
                        // recorded: without this an idle target never acts on it (it
                        // sits unseen on the pull-based bus). Best-effort and scope-
                        // gated exactly like `ctl send`, so it grants no new reach;
                        // the message still lives on the bus as the durable record.
                        if let Some((to, text)) =
                            routed_wake(&e.fields, &e.topic, e.seq, e.kind, &who)
                        {
                            let candidates = ctl_candidates(windows);
                            if let Ok(tid) = ctl::resolve_target(&to, &candidates) {
                                if caller != Some(tid)
                                    && scope_denied(windows, caller, privileged, tid).is_none()
                                {
                                    // Over the cap the wake is skipped; the
                                    // message itself is still on the bus.
                                    queue_send(pending, tid, text);
                                }
                            }
                        }
                        ctl::reply_bus_published(atrium::bus::event_to_value(&e), subs)
                    }
                    Err(msg) => ctl::reply_err(&msg),
                },
                ctl::BusOp::Sub { topics } => {
                    // A declared (strict) fleet gates subscriptions too: you may
                    // only listen on a declared topic. The soft-gate admits any
                    // topic for *listening* (create=true), so subscribing to a
                    // not-yet-published topic is fine — only publishing into the
                    // void needs the deliberate --new.
                    if let Some(bad) = topics.iter().find_map(|t| bus.admit_topic(t, true).err()) {
                        ctl::reply_err(&bad)
                    } else {
                        bus.subscribe(&who, &topics);
                        ctl::reply_bus_subscribed(current_subs(bus, &who))
                    }
                }
                ctl::BusOp::Unsub { topics } => {
                    bus.unsubscribe(&who, &topics);
                    ctl::reply_bus_subscribed(current_subs(bus, &who))
                }
                ctl::BusOp::Feed { since } => {
                    let events = bus.feed(&who, since);
                    // The new cursor is the max seq pulled, or `since` when empty,
                    // so it never rewinds.
                    let cursor = events.iter().map(|e| e.seq).max().unwrap_or(since);
                    ctl::reply_bus_feed(atrium::bus::events_to_value(&events), cursor)
                }
                ctl::BusOp::Resolve { seq } => ctl::reply_bus_resolved(seq, bus.resolve(seq)),
                ctl::BusOp::Topics => ctl::reply_bus_topics(bus.topics_with_counts()),
            }
        }
    }
}

/// A subscriber's current topic set as a `Vec` (for the `sub`/`unsub` reply).
pub(crate) fn current_subs(bus: &atrium::bus::Bus, who: &str) -> Vec<String> {
    bus.subscriptions(who)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

/// Truncate a string to `n` chars (char-safe), appending `…` when cut. Keeps a
/// bad-request line short in the audit log.
pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// `(ok, note)` for the audit record: the error on failure, or a compact success
/// tag naming the salient id(s). Read from the typed [`atrium::ctl::Reply`] — it
/// used to re-parse the server's own JSON reply by string keys (r10 audit B12).
pub(crate) fn audit_outcome(reply: &atrium::ctl::Reply) -> (bool, String) {
    use atrium::ctl::Reply;
    let note = match reply {
        Reply::Err(msg) => return (false, msg.clone()),
        Reply::Spawned { pane, .. } | Reply::StatusOne { pane, .. } => format!("pane={pane}"),
        Reply::Killed(ids) => format!(
            "killed={}",
            ids.iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        Reply::Sent { target, .. } => format!("target={target}"),
        _ => String::new(),
    };
    (true, note)
}

/// Serve a `ctl audit` read: the in-memory log, subtree-scoped. The operator
/// sees everything; a worker sees only entries issued from within its own
/// subtree (operator/`None`-caller entries are hidden from it).
pub(crate) fn audit_reply(
    audit: &atrium::audit::Audit,
    windows: &[Window],
    caller: Option<AgentId>,
    privileged: bool,
    tail: Option<usize>,
) -> atrium::ctl::Reply {
    let entries = if privileged {
        audit.view(tail, |_| true)
    } else if let Some(root) = caller {
        let parents = ctl_parents(windows);
        audit.view(tail, |e| match e.caller {
            Some(c) => atrium::ctl::in_subtree(c, root, &parents),
            None => false, // operator actions are hidden from a worker
        })
    } else {
        // An UNAUTHENTICATED caller (no token, so `caller == None`) used to land
        // here, and this branch was byte-identical to the privileged one - so
        // holding no credential returned the entire fleet-wide audit trail,
        // including the operator actions the branch above deliberately hides
        // from a worker. That is the same "absent means most trusted" inversion
        // `privilege_for` was fixed to remove, left standing in one call site.
        Vec::new()
    };
    atrium::ctl::reply_audit(entries, audit.oldest_seq(), audit.latest_seq())
}

/// Serialize the spawn tree as a `list`/`status` reply. `root == Some(id)` limits
/// it to that pane's subtree (subtree-scoped status); `None` is the whole tree.
pub(crate) fn reply_tree(
    windows: &[Window],
    world: &atrium::vendors::AgentState,
    root: Option<AgentId>,
) -> atrium::ctl::Reply {
    let parents = ctl_parents(windows);
    let mut panes: Vec<&Pane> = windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .filter(|p| match root {
            None => true,
            Some(r) => atrium::ctl::in_subtree(p.agent_id, r, &parents),
        })
        .collect();
    panes.sort_by_key(|p| p.agent_id);
    // One monotonic reading for the whole snapshot so every node's idle is
    // measured against the same instant (no per-pane clock drift within a reply).
    let now = std::time::Instant::now();
    let nodes: Vec<atrium::ctl::TreeNode> = panes
        .iter()
        .map(|p| atrium::ctl::TreeNode {
            id: p.agent_id,
            parent: p.parent,
            role: p.role.as_deref(),
            title: &p.title,
            depth: p.depth,
            status: world.status_for(p.session_id.as_deref()).map(status_label),
            idle_ms: atrium::ipc::idle_ms(p.last_activity, now),
        })
        .collect();
    atrium::ctl::reply_list(&nodes)
}

/// Subtree-scope guard: `None` if the caller may act on `target`, else a ready
/// JSON refusal. The operator (privileged) may act on anything.
/// The command a `ctl respawn` relaunches: the pane's own recorded command, as a
/// new session. The governed permission flags are stripped (the pane's `mode`
/// sets its posture at spawn), and for claude every session-selecting argument
/// goes too, so the spawn mints a fresh `--session-id` rather than resuming the
/// conversation the restart exists to leave. Other programs keep their arguments
/// verbatim: `-c` is a command string to a shell, not a session. A pane with no
/// recorded command falls back to its title. Pure.
pub(crate) fn respawn_argv(argv: &[String], title: &str) -> Vec<String> {
    let (argv, _) = atrium::ctl::sanitize_spawn_argv(argv);
    match argv.first() {
        None => vec![title.to_string()],
        Some(c) if atrium::bind::is_claude_stem(&atrium::bind::command_stem(c)) => {
            strip_session_args(&argv)
        }
        Some(_) => argv,
    }
}

pub(crate) fn scope_denied(
    windows: &[Window],
    caller: Option<AgentId>,
    privileged: bool,
    target: AgentId,
) -> Option<atrium::ctl::Reply> {
    if privileged {
        return None;
    }
    let root = caller?; // non-privileged implies a known caller pane
    if atrium::ctl::in_subtree(target, root, &ctl_parents(windows)) {
        None
    } else {
        Some(atrium::ctl::reply_err(&format!(
            "pane {target} is outside your subtree; a worker may only steer what it spawned"
        )))
    }
}

/// Resolve an optional ad-hoc worktree name into `(cwd, norms)` for a ctl spawn.
///
/// When `wt_name` is `Some`, creates the worktree (idempotent) and returns its
/// directory as cwd plus the behavioral norms text. `None` → `Ok((None, None))`,
/// which leaves the spawned pane in atrium's own cwd with no extra norms.
/// Returns `Err` (with a human-readable message) when the worktree cannot be
/// created — the caller must surface this as a ctl error rather than spawning
/// a child into a directory that does not exist.
///
/// `junction_sibling_deps` is best-effort: failures are silently ignored so a
/// missing junction never prevents the worktree from being usable.
pub(crate) fn worktree_spawn_params(
    cwd: &std::path::Path,
    wt_name: Option<&str>,
) -> Result<(Option<String>, Option<String>), String> {
    let Some(name) = wt_name else {
        return Ok((None, None));
    };
    let plan = atrium::worktree::plan_for(cwd, name);
    atrium::worktree::ensure(cwd, &plan)
        .map_err(|e| format!("could not create worktree '{name}': {e}"))?;
    let _ = atrium::worktree::junction_sibling_deps(cwd, &plan);
    let dir = plan.dir.to_string_lossy().into_owned();
    let norms = atrium::worktree::worktree_norms(&plan.name, &plan.branch);
    Ok((Some(dir), Some(norms)))
}

/// `ctl spawn` (default): a visible worker in a brand-new window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_worker_window(
    windows: &mut Vec<Window>,
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    rows: u16,
    cols: u16,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
    job: &atrium::reap::SessionJob,
) -> atrium::ctl::Reply {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (wt_cwd, wt_norms) = match worktree_spawn_params(&cwd, sp.worktree.as_deref()) {
        Ok(pair) => pair,
        Err(e) => return atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    };
    let mut flash = None;
    match spawn_window(
        &sp.argv,
        rows,
        cols,
        windows.len(),
        sp.identity.as_deref(),
        mode,
        &mut flash,
        Some(job),
        wt_cwd.as_deref(),
        wt_norms.as_deref(),
    ) {
        Ok(mut w) => {
            let pane = &mut w.panes[0];
            pane.role = sp.role.clone();
            pane.parent = caller;
            pane.depth = new_depth;
            pane.worktree = sp.worktree.clone();
            let agent_id = pane.agent_id;
            let session = pane.session_id.clone();
            windows.push(w);
            // Size the new window's pane to its true rect now, so the agent
            // paints full-height immediately instead of at the rough spawn size
            // (which otherwise needs a manual terminal resize to correct).
            let last = windows.len() - 1;
            resize_window(&mut windows[last], rows, cols);
            atrium::ctl::reply_spawned(agent_id, sp.role.as_deref(), session.as_deref(), note)
        }
        Err(e) => atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// `ctl spawn --here`: tile the worker *beside* the caller, in the caller's own
/// window, so a lead and its ICs sit in one view. Falls back to an error if the
/// caller's pane can't be located (nothing to sit beside).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_worker_here(
    windows: &mut [Window],
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    rows: u16,
    cols: u16,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
    job: &atrium::reap::SessionJob,
) -> atrium::ctl::Reply {
    let Some(caller_id) = caller else {
        return atrium::ctl::reply_err(
            "`--here` needs a caller pane; run it from inside an atrium pane",
        );
    };
    // Locate the window holding the caller and that caller's per-window pane id.
    let Some((wi, caller_pane_id)) = windows.iter().enumerate().find_map(|(i, w)| {
        w.panes
            .iter()
            .find(|p| p.agent_id == caller_id)
            .map(|p| (i, p.id))
    }) else {
        return atrium::ctl::reply_err("`--here`: caller pane not found (rerun without --here)");
    };

    let w = &mut windows[wi];
    let new_id = w.next_id;
    // Rough half-cell inner size; the caller resizes the window right after.
    let (pr, pc) = (
        (rows.saturating_sub(1).max(1) / 2).saturating_sub(2),
        (cols / 2).saturating_sub(2),
    );
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (wt_cwd, wt_norms) = match worktree_spawn_params(&cwd, sp.worktree.as_deref()) {
        Ok(pair) => pair,
        Err(e) => return atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    };
    let mut flash = None;
    match spawn_pane_full(
        PaneSpec {
            command: &sp.argv,
            id: new_id,
            identity: sp.identity.as_deref(),
            cwd: wt_cwd.as_deref(),
            mode,
            extra_env: &[],
            extra_norms: wt_norms.as_deref(),
            deny: &[],
        },
        pr.max(1),
        pc.max(1),
        &mut flash,
    ) {
        Ok(mut pane) => {
            pane.role = sp.role.clone();
            pane.parent = caller;
            pane.depth = new_depth;
            pane.worktree = sp.worktree.clone();
            let agent_id = pane.agent_id;
            let session = pane.session_id.clone();
            // Enroll this ctl-spawned pane in the session Job synchronously (same
            // #94 guarantee as spawn_window; idempotent, unix no-op, graceful).
            job.assign(pane.pty.pid());
            w.panes.push(pane);
            w.next_id += 1;
            // Re-tile the WHOLE window into a balanced near-square grid over all
            // its panes (keeping their stable ids), rather than just splitting the
            // caller side-by-side. A plain `split_pane` each time stacks every
            // `--here` worker into one column, so N of them degrade to an
            // unusable `1×N` strip; re-gridding keeps 2→1×2, 4→2×2, 6→2×3,
            // 12→3×4, … balanced. Focus lands on the fresh worker.
            let _ = caller_pane_id; // (kept for the error message above)
            let ids: Vec<usize> = w.panes.iter().map(|p| p.id).collect();
            w.tree = Tree::grid_from_ids(&ids);
            w.tree.focus_pane(new_id);
            w.zoomed = false;
            // Resize the whole window so both the caller and the fresh worker get
            // their exact inner rects — without this the worker keeps its rough
            // half-size and paints short (blank below), fixed only by a manual
            // terminal resize. Mirrors what the interactive split handlers do.
            resize_window(&mut windows[wi], rows, cols);
            atrium::ctl::reply_spawned(agent_id, sp.role.as_deref(), session.as_deref(), note)
        }
        Err(e) => atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// Flush queued `ctl send`s (Decision 4: queue until the target is idle). For
/// each pending send: once the target reports a ready status (or the unbound
/// fallback elapses), write the text; a beat later write the Enter and drop it.
/// A vanished target is dropped. Returns whether anything was written (so the
/// caller can request a repaint).
pub(crate) fn flush_sends(
    pending: &mut Vec<PendingSend>,
    windows: &mut [Window],
    world: &atrium::vendors::AgentState,
) -> bool {
    if pending.is_empty() {
        return false;
    }
    let now = Instant::now();
    let mut wrote = false;
    pending.retain_mut(|ps| {
        match ps.text_written_at {
            None => {
                // Decide readiness from the target's live status, an open
                // dialog, and any unsent human draft (see atrium::deliver).
                let ready = match pane_by_agent(windows, ps.target) {
                    None => return false, // target gone — drop the send
                    Some(p) => {
                        let sid = p.session_id.as_deref();
                        atrium::deliver::ready(
                            world.status_for(sid),
                            world.awaiting_tool_for(sid),
                            p.draft.holds(now),
                            now.duration_since(ps.queued_at),
                            SEND_UNBOUND_FALLBACK,
                        )
                    }
                };
                if ready {
                    if let Some(p) = pane_by_agent_mut(windows, ps.target) {
                        // Write from where we left off and advance by what was
                        // actually accepted. A short write is normal, not an
                        // error: the pty's buffer is finite. Resuming across
                        // ticks — rather than looping here until the whole thing
                        // lands — is deliberate, because this runs on the single
                        // event loop and a blocking write into a full buffer
                        // would freeze every pane until the target drained it.
                        let bytes = ps.text.as_bytes();
                        match p.pty.write(&bytes[ps.written..]) {
                            Ok(0) => {} // took nothing this tick; try the next
                            Ok(n) => {
                                ps.written += n;
                                wrote = true;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(_) => return false, // target unwritable — drop it
                        }
                        // Only now is the send real. Enter waits for the last byte.
                        if ps.written >= bytes.len() {
                            ps.text_written_at = Some(now);
                        }
                    } else {
                        return false;
                    }
                }
                true
            }
            Some(t) => {
                if now.duration_since(t) >= SEND_ENTER_DELAY {
                    if let Some(p) = pane_by_agent_mut(windows, ps.target) {
                        let _ = p.pty.write(b"\r");
                        wrote = true;
                    }
                    false // delivered — drop
                } else {
                    true
                }
            }
        }
    });
    wrote
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// A respawn restarts the agent that was running — its model, its kickoff —
    /// as a new session with no permission flags of its own.
    #[test]
    fn a_respawned_claude_keeps_its_arguments_but_not_its_session() {
        let argv = args(&[
            "claude",
            "--model",
            "opus",
            "--resume",
            "abc",
            "--dangerously-skip-permissions",
            "--session-id=def",
            "-c",
            "You lead. Read PLAN.md.",
        ]);
        assert_eq!(
            respawn_argv(&argv, "claude"),
            args(&["claude", "--model", "opus", "You lead. Read PLAN.md."])
        );
    }

    #[test]
    fn a_respawned_shell_keeps_a_dash_c_command() {
        let argv = args(&["sh", "-c", "make watch"]);
        assert_eq!(respawn_argv(&argv, "sh"), argv);
    }

    /// A respawn mints the pane a new agent id. Its workers name their parent by
    /// id, so they must follow it, or a restarted lead below the root could no
    /// longer steer the team it built.
    #[test]
    fn a_respawned_panes_workers_follow_it_to_its_new_id() {
        let (old, new, other) = (AgentId(4), AgentId(9), AgentId(2));
        let mut parents = vec![Some(old), Some(other), None, Some(old)];
        repoint_parents(parents.iter_mut(), old, new);
        assert_eq!(parents, vec![Some(new), Some(other), None, Some(new)]);
    }

    /// Sends queued for the old process go to the new one, from the start: a
    /// fragment already written into the dead pty is not continued mid-text.
    #[test]
    fn sends_queued_for_a_respawned_pane_are_delivered_whole_to_its_new_process() {
        let (old, new, other) = (AgentId(4), AgentId(9), AgentId(2));
        let mut pending = Vec::new();
        assert!(queue_send(&mut pending, old, "resume from PLAN.md".into()));
        assert!(queue_send(&mut pending, other, "keep going".into()));
        pending[0].written = 6;
        pending[0].text_written_at = Some(Instant::now());
        let later = pending[0].queued_at + SEND_UNBOUND_FALLBACK;
        retarget_sends(&mut pending, old, new, later);
        assert_eq!(pending[0].target, new);
        assert_eq!(pending[0].queued_at, later);
        assert_eq!(pending[0].written, 0);
        assert!(pending[0].text_written_at.is_none());
        assert_eq!(pending[1].target, other);
    }

    #[test]
    fn a_pane_with_no_recorded_command_respawns_its_title() {
        assert_eq!(respawn_argv(&[], "claude"), args(&["claude"]));
    }

    #[test]
    fn a_target_that_never_takes_delivery_cannot_grow_the_queue_without_bound() {
        // A pane wedged on an open dialog never becomes ready, so its sends are
        // never drained; each one can carry a max-size ctl request. The queue per
        // target is capped, and one wedged target doesn't block the others.
        let mut pending = Vec::new();
        let wedged = AgentId(1);
        for _ in 0..MAX_PENDING_PER_TARGET {
            assert!(queue_send(&mut pending, wedged, "task".into()));
        }
        assert!(!queue_send(&mut pending, wedged, "one too many".into()));
        assert_eq!(pending.len(), MAX_PENDING_PER_TARGET);
        assert!(queue_send(&mut pending, AgentId(2), "other".into()));
    }
}
