//! Applying one ctl request against the live windows.

use super::panes::{
    caller_privileged, ctl_candidates, ctl_parents, effective_mode, pane_by_agent,
    pane_by_agent_mut, pane_label, role_holder, status_label,
};
use super::reply::{audit_outcome, audit_reply, current_subs, reply_tree};
use super::sends::{
    queue_send, repoint_parents, retarget_sends, PendingSend, SendOrigin, MAX_PENDING_PER_TARGET,
};
use super::spawn::{spawn_worker_here, spawn_worker_window, worktree_spawn_params};
use super::wake::bus_wakes;
use crate::*;

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
            if !queue_send(pending, id, sr.text, SendOrigin::Ctl) {
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
            // A role names exactly one live pane. The bus keys subscriptions by
            // role, and a bus event wakes a pane because *its label* subscribed
            // — so a second pane wearing an existing role would subscribe on
            // the first one's behalf and route wakes into it, and its own posts
            // would read as the first one's. `--to` addressing would be
            // ambiguous too. Refused for everyone, the operator included: the
            // collision is what is unsafe, not who caused it.
            if let Some(role) = sp.role.as_deref() {
                if let Some(holder) = role_holder(role, &ctl_candidates(windows)) {
                    return ctl::reply_err(&format!(
                        "role {role:?} already names pane {holder}; roles are unique while a \
                         pane lives (kill it, wait for it to exit, or pick another name)"
                    ));
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
    }
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

#[cfg(test)]
mod tests;
