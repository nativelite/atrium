//! Applying one ctl request against the live windows.

use super::panes::{
    caller_privileged, ctl_candidates, ctl_parents, pane_by_agent, pane_label, status_label,
};
use super::reply::{audit_outcome, audit_reply, current_subs, reply_tree};
use super::respawn::respawn;
use super::sends::{queue_send, PendingSend, SendOrigin, MAX_PENDING_PER_TARGET};
use super::spawn::spawn;
use super::wake::bus_wakes;
use crate::*;
use atrium::ctl::{self, Cmd};

/// The live session state a ctl request is applied against: everything the run
/// loop lends the control plane for one request. Built by the run loop each
/// time it answers a request; the handlers read and change it through here.
pub(crate) struct CtlSession<'a> {
    pub(crate) windows: &'a mut Vec<Window>,
    /// The terminal size, which new and respawned panes are laid out in.
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// `--max-depth`: how deep the spawn tree may grow.
    pub(crate) max_depth: usize,
    /// Commands a ctl spawn may run beyond the built-in agent allowlist.
    pub(crate) extra_allow: &'a [String],
    pub(crate) pending: &'a mut Vec<PendingSend>,
    pub(crate) world: &'a atrium::vendors::AgentState,
    /// The identity the session was launched under, if any.
    pub(crate) session_identity: Option<&'a str>,
    pub(crate) audit: &'a mut atrium::audit::Audit,
    pub(crate) board: &'a mut atrium::board::Board,
    pub(crate) bus: &'a mut atrium::bus::Bus,
    pub(crate) job: &'a atrium::reap::SessionJob,
}

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

/// Apply one ctl request against the live window set and return the JSON reply
/// line, **recording it to the audit log** (design §5). Thin wrapper: parse,
/// serve `audit` reads directly (they need the log and are not self-recorded),
/// else [`dispatch_ctl`] the request and record its outcome. `spawn`/`send`/
/// `status`/`kill` with a target are subtree-scoped; `spawn --identity` is
/// delegation-scoped.
pub(crate) fn apply_ctl(line: &str, cx: &mut CtlSession<'_>) -> ctl::Reply {
    let (windows, audit) = (&*cx.windows, &mut *cx.audit);
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
    let reply = dispatch_ctl(req, caller, privileged, cx);
    let (ok, note) = audit_outcome(&reply);
    cx.audit.record(caller, action, &detail, ok, &note);
    reply
}

/// The server side of the control channel: turn one parsed [`atrium::ctl::Request`]
/// into its JSON reply. `list`/`status` serialize the spawn tree (agsess status
/// folded in); `spawn` opens a visible worker after the pure allowlist/depth
/// guard ([`atrium::ctl::evaluate_spawn`]) and the credential-delegation guard
/// ([`atrium::ctl::delegation_allowed`]); `send` enqueues a queue-until-idle
/// delivery; `kill` tears down the target's subtree (the reap step reaps them).
/// `send`/`status`/`kill` with a target are subtree-scoped.
pub(crate) fn dispatch_ctl(
    req: ctl::Request,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    match req.cmd {
        Cmd::List => reply_tree(cx.windows, cx.world, None),
        Cmd::Status(sr) => status(sr, caller, privileged, cx),
        Cmd::Send(sr) => send(sr, caller, privileged, cx),
        Cmd::Spawn(sp) => spawn(sp, caller, privileged, cx),
        Cmd::Kill(kr) => kill(kr, caller, privileged, cx),
        Cmd::Respawn(rr) => respawn(rr, caller, privileged, cx),
        // `audit` is handled in `apply_ctl` (it needs the log); never reaches here.
        Cmd::Audit(_) => ctl::reply_err("internal: audit dispatched to the wrong handler"),
        Cmd::Board(op) => board(op, caller, cx),
        Cmd::Bus(op) => bus(op, caller, privileged, cx),
    }
}

/// `ctl status`: the caller's subtree, or one target's status and idle time.
fn status(
    sr: ctl::StatusReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &CtlSession<'_>,
) -> ctl::Reply {
    let (windows, world) = (&*cx.windows, cx.world);
    match sr.target {
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
    }
}

/// `ctl send`: queue text for a target in the caller's subtree.
fn send(
    sr: ctl::SendReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    let (windows, world, pending) = (&*cx.windows, cx.world, &mut *cx.pending);
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
    if !queue_send(pending, id, sr.text, SendOrigin::Ctl) {
        return ctl::reply_err(&format!(
            "agent {id} already has {MAX_PENDING_PER_TARGET} undelivered sends; \
                 it is not taking delivery (busy, a dialog open, or a draft). \
                 Check `ctl status` before sending more"
        ));
    }
    ctl::reply_sent(id, busy)
}

/// `ctl kill`: tear down a target in the caller's subtree and all its descendants.
fn kill(
    kr: ctl::KillReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    let windows = &mut *cx.windows;
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

/// `ctl board`: read and write the shared task board.
fn board(op: ctl::BoardOp, caller: Option<AgentId>, cx: &mut CtlSession<'_>) -> ctl::Reply {
    let (windows, board) = (&*cx.windows, &mut *cx.board);
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
        ctl::BoardOp::List => ctl::reply_board_list(atrium::board::entries_to_value(&board.list())),
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

/// `ctl bus`: publish, subscribe and read on the shared event bus.
fn bus(
    op: ctl::BusOp,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    let (windows, bus, pending) = (&*cx.windows, &mut *cx.bus, &mut *cx.pending);
    // The bus is SHARED like the board. The server derives `who` (the
    // caller's role, else its pane, else the operator) so a worker
    // publishes and subscribes *as itself* — it cannot forge another
    // sender. `who` is stable per pane, so the no-echo filter and a
    // subscriber's cursor stay consistent across calls.
    let who = caller
        .and_then(|cid| pane_by_agent(windows, cid))
        .map(|p| pane_label(p.agent_id, p.role.as_deref()))
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
                // An event is DELIVERED to the panes it addresses (`--to`)
                // and to the topic's subscribers, not just recorded: the bus
                // is pull for the record, and an idle target never sees a
                // record. Best-effort, framed and sanitized, capped and
                // coalesced per target; see `bus_wakes` for who qualifies.
                // Over the cap the wake is skipped; the message itself is
                // still on the bus.
                let candidates = ctl_candidates(windows);
                let parents = ctl_parents(windows);
                let wakes = bus_wakes(
                    &e,
                    &who,
                    caller,
                    privileged,
                    &candidates,
                    &parents,
                    |label| {
                        bus.subscriptions(label)
                            .is_some_and(|t| t.contains(&e.topic) || t.contains("*"))
                    },
                );
                for (tid, text) in wakes {
                    queue_send(pending, tid, text, SendOrigin::BusWake);
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

/// Subtree-scope guard: `None` if the caller may act on `target`, else a ready
/// JSON refusal. The operator (privileged) may act on anything.
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
