//! Applying one ctl request against the live windows.

use super::panes::{
    caller_privileged, ctl_candidates, ctl_parents, pane_by_agent, pane_label, status_label,
};
use super::reply::{audit_outcome, audit_reply, current_subs, reply_tree};
use super::respawn::respawn;
use super::sends::take_for;
use super::sends::{queue_send, PendingSend, SendOrigin, SentBy, MAX_PENDING_PER_TARGET};
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
    /// How many `inbox --wait` connections the run loop holds open right
    /// now: a number `metrics` reports.
    pub(crate) held_inboxes: usize,
    pub(crate) world: &'a atrium::vendors::AgentState,
    /// The identity the session was launched under, if any.
    pub(crate) session_identity: Option<&'a str>,
    pub(crate) audit: &'a mut atrium::audit::Audit,
    pub(crate) board: &'a mut atrium::board::Board,
    pub(crate) bus: &'a mut atrium::bus::Bus,
    /// What each pane's mod has said (`hello`, `report`); see
    /// [`atrium::modstate`].
    pub(crate) mods: &'a mut atrium::modstate::ModState,
    /// Set by an `inbox --wait` that found nothing: the pane to answer when a
    /// delivery lands. The run loop parks the connection instead of sending
    /// the (empty) reply, and answers it from [`inbox_items`] later.
    pub(crate) hold: &'a mut Option<AgentId>,
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
pub(crate) fn apply_ctl(line: &str, cx: &mut CtlSession<'_>) -> (ctl::Reply, CtlEffect) {
    apply_ctl_as(line, cx, atrium::operator::session())
}

/// [`apply_ctl`] against a given set of operators: the session's
/// ([`atrium::operator::session`], loaded once at startup), or a test's.
pub(crate) fn apply_ctl_as(
    line: &str,
    cx: &mut CtlSession<'_>,
    operators: &atrium::operator::Operators,
) -> (ctl::Reply, CtlEffect) {
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
            // nothing that was in the payload: the error's own words, without
            // the request values it quotes (`bad_request_note`).
            let detail = format!("<unparseable, {} bytes>", line.len());
            let note = bad_request_note(&e);
            // A request that could not be parsed may still say whose it is.
            // An operator's is recorded under its name (or as a failed
            // attempt), like every other request an operator token comes with.
            match operator_of_unparsed(line, operators) {
                Some(by) => audit.record_as(&by, "bad-request", &detail, false, &note),
                None => audit.record(None, "bad-request", &detail, false, &note),
            };
            return (reply, CtlEffect::Nothing);
        }
    };
    // A request that carries an operator token is an operator's, start to
    // finish: it is authenticated, authorized, served and audited by
    // `apply_operator` and never reaches the pane path below, whatever else it
    // carries. A token that matches no operator is refused there; it does not
    // fall back to a pane's identity or to an anonymous read.
    if req.operator_token.is_some() {
        return apply_operator(req, operators, cx);
    }
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
        return (reply, CtlEffect::Nothing);
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
        return (
            audit_reply(audit, windows, caller, privileged, ar.tail),
            CtlEffect::Nothing,
        );
    }

    // `ping` and `metrics` are probes: a liveness check every few seconds and
    // a scrape every fifteen, for as long as the session lives. Recorded, they
    // would push every real action out of the audit ring and grow its file
    // without end, and they hand out a version, an uptime and counts, nothing
    // of any pane's. Answered, not recorded.
    if matches!(req.cmd, Cmd::Ping | Cmd::Metrics) {
        return (
            dispatch_ctl(req, caller, privileged, cx),
            CtlEffect::Nothing,
        );
    }

    let (action, detail) = req.cmd.audit_label();
    let is_poll = matches!(req.cmd, Cmd::Answer(_) | Cmd::Inbox(_) | Cmd::Asked(_));
    let effect = CtlEffect::of(&req.cmd);
    let reply = dispatch_ctl(req, caller, privileged, cx);
    // `wait` polls `answer` twice a second until one exists, and a mod polls
    // `inbox` every second or two: a poll that found nothing is not an action
    // and would flood the ring. The read that found something is recorded.
    let found_nothing = match &reply {
        ctl::Reply::Answer { seq: None, .. } => true,
        ctl::Reply::Inbox(items) => items.is_empty(),
        ctl::Reply::Asked { reply: None, .. } => true,
        _ => false,
    };
    if is_poll && found_nothing {
        // Nothing was taken, so nothing on screen can have changed either.
        return (reply, CtlEffect::Nothing);
    }
    let (ok, note) = audit_outcome(&reply);
    cx.audit.record(caller, action, &detail, ok, &note);
    (reply, effect)
}

/// What the audit log keeps of a parse error: its own words, and none of the
/// request's. `parse_request` quotes the value it could not take (`unknown
/// command "…"`, `(got Some("…"))`), and that value is whatever the caller
/// sent, of any length, with any character in it. The note is the text before
/// the first quote or parenthesis, scrubbed and cut to 80 characters; the
/// caller still gets the full error in its reply.
fn bad_request_note(err: &str) -> String {
    let head = err.split(['"', '(', '\'']).next().unwrap_or("");
    let head: String = atrium::modstate::text_safe(head).chars().take(80).collect();
    let head = head.trim().trim_end_matches(':').trim_end();
    if head.is_empty() {
        "malformed request".to_string()
    } else {
        head.to_string()
    }
}

/// The audit label for a request that failed to parse, when it carries an
/// `operator_token` field: `operator:<name>` if the token matches (compared
/// in constant time, as everywhere), `operator:?` if it does not or is not a
/// string. `None` when there is no such field, or the line is not JSON at all
/// and so says nothing about whose it is.
fn operator_of_unparsed(line: &str, operators: &atrium::operator::Operators) -> Option<String> {
    let v = json::parse(line).ok()?;
    let field = v.get("operator_token")?;
    Some(
        field
            .as_str()
            .and_then(|presented| operators.authenticate(presented))
            .map(atrium::operator::Operator::label)
            .unwrap_or_else(|| atrium::operator::UNKNOWN_LABEL.to_string()),
    )
}

/// The reply to a request whose operator token matches no operator. One text
/// whether the session has no token file at all or the token is simply wrong,
/// so the reply tells a prober nothing about which.
const UNKNOWN_OPERATOR: &str =
    "unauthenticated: the operator token matches no operator of this session";

/// Serve a request that carries an operator token ([`atrium::operator`]).
///
/// In order, and audited at whichever step ends it — as `operator:<name>` once
/// the token has matched, as `operator:?` when it has not:
///
/// 1. **Authenticate.** The token is matched against the session's operators
///    only, in constant time. A pane's `ATRIUM_TOKEN` in this field matches
///    nothing. No match is a refusal, for reads too: a wrong credential is not
///    an absent one.
/// 2. **One credential.** A request that also carries a pane token is refused.
///    Which identity it meant is not ours to guess.
/// 3. **Vet.** Every field is bounded and checked ([`atrium::operator::vet`]):
///    an operator may be a network peer.
/// 4. **Authorize.** The verb must be one the operator's entry names, or one
///    of the reads the endpoint serves to any caller; a pane's own verbs
///    (`hello`, `report`, `whoami`, `inbox`, `bus sub`, `bus unsub`) are
///    refused whatever the entry says.
/// 5. **Serve.** A read is served exactly as it is to a caller with no token
///    (`audit` aside: named in the entry, it is the whole log; not named, the
///    empty view an anonymous caller gets). A write goes through
///    [`dispatch_operator`].
///
/// Every request is recorded, an empty poll included: the exemption that
/// keeps a mod's twice-a-second `inbox` out of the log is a pane's.
fn apply_operator(
    req: ctl::Request,
    operators: &atrium::operator::Operators,
    cx: &mut CtlSession<'_>,
) -> (ctl::Reply, CtlEffect) {
    use atrium::operator::Verdict;
    let vetted = atrium::operator::vet(&req.cmd);
    // The detail is built from the request's own fields. Fields that failed
    // the vetting are not copied into the log.
    let (action, detail) = match &vetted {
        Ok(()) => req.cmd.audit_label(),
        Err(_) => (
            req.cmd.audit_label().0,
            "<fields withheld: refused>".to_string(),
        ),
    };
    let presented = req.operator_token.as_deref().unwrap_or("");
    let Some(op) = operators.authenticate(presented) else {
        cx.audit.record_as(
            atrium::operator::UNKNOWN_LABEL,
            action,
            &detail,
            false,
            "unknown operator token",
        );
        return (ctl::reply_err(UNKNOWN_OPERATOR), CtlEffect::Nothing);
    };
    let label = op.label();
    let refusal = if req.token.as_deref().is_some_and(|t| !t.is_empty()) {
        Some(
            "a request carries one credential: this one has an operator token and a pane token"
                .to_string(),
        )
    } else if let Err(why) = vetted {
        Some(format!("refused: {why}"))
    } else {
        None
    };
    let verdict = match refusal {
        Some(why) => Verdict::Refused(why),
        None => op.verdict(&req.cmd),
    };
    let listed = match verdict {
        Verdict::Refused(why) => {
            cx.audit.record_as(&label, action, &detail, false, &why);
            return (ctl::reply_err(&why), CtlEffect::Nothing);
        }
        Verdict::Listed => true,
        Verdict::OpenRead => false,
    };
    // The two probes are answered and not recorded, for an operator as for
    // anyone: a liveness check every few seconds would push every real action
    // out of the ring, and they hand out nothing of any pane's. The token was
    // still checked above, and a wrong one was recorded.
    if matches!(req.cmd, Cmd::Ping | Cmd::Metrics) {
        return (dispatch_ctl(req, None, false, cx), CtlEffect::Nothing);
    }
    if let Cmd::Audit(ar) = &req.cmd {
        // Recorded before the reply is built, so the reader sees its own read.
        cx.audit.record_as(&label, "audit", "read", true, "");
        return (
            audit_reply(&*cx.audit, &*cx.windows, None, listed, ar.tail),
            CtlEffect::Nothing,
        );
    }
    let effect = CtlEffect::of(&req.cmd);
    let reply = if req.cmd.is_read_only() {
        // What a caller with no token is served, by the same code.
        dispatch_ctl(req, None, false, cx)
    } else {
        dispatch_operator(req.cmd, op, cx)
    };
    let (ok, note) = audit_outcome(&reply);
    cx.audit.record_as(&label, action, &detail, ok, &note);
    (reply, effect)
}

/// An operator's write, on the handlers written for panes. This table is the
/// whole of how an operator maps onto them, and a verb is in it only once its
/// handler has been read for a caller with no pane:
///
/// * **Reach.** An operator acts across the whole session, so the subtree
///   guard is given `privileged`: `kill`, `respawn`, `send` and `ask` (those
///   two for agent panes only, see [`not_an_agent`]), and the
///   model-output reads reach any pane. `caller` is `None` throughout — an
///   operator is never passed off as some pane.
/// * **`spawn` is the exception.** It is run as a top-level caller *without*
///   privilege. The worker is a new window at depth 1 with no parent, so the
///   depth cap, the pane cap, the agent allowlist and the argv vetting all
///   apply as they do to a root pane's spawn; the trust mode is capped at the
///   session's launch trust like everyone's ([`super::panes::effective_mode`]);
///   and with no privilege the only identity it may run under is the session's
///   own default — an operator cannot hand a worker any credential in the
///   vault, which a root pane can. `--here` has no pane to sit beside and is
///   refused by the vetting.
/// * **Attribution.** Board entries and bus events are signed with the
///   operator's label, never a pane's.
/// * **Everything else is refused**, by the last arm: a verb added later does
///   not reach an operator until it is added here.
fn dispatch_operator(
    cmd: Cmd,
    op: &atrium::operator::Operator,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    let label = &op.label();
    let wake = OperatorWake {
        may_send: op.verbs().iter().any(|v| v == "send"),
    };
    match cmd {
        Cmd::Spawn(sp) => spawn(sp, None, false, cx),
        Cmd::Send(sr) => match not_an_agent(cx.windows, &sr.target) {
            Some(refusal) => refusal,
            None => send_under(sr, None, true, Some(label), trust_mode(), cx),
        },
        // The allowlist is the grant, for an operator as for a pane; the
        // entry must name the verb, and the target is an agent's pane.
        Cmd::Command(c) => command(c, None, true, Some(label), &ctl::session_commands(), cx),
        Cmd::Kill(kr) => kill(kr, None, true, cx),
        Cmd::Respawn(rr) => respawn(rr, None, true, cx),
        Cmd::Ask(a) => match not_an_agent(cx.windows, &a.target) {
            Some(refusal) => refusal,
            None => ask(a, None, true, cx),
        },
        Cmd::Answer(a) => answer(a, None, true, cx),
        Cmd::Asked(a) => asked(a, None, true, cx),
        Cmd::Who(w) => who(w, None, true, cx),
        Cmd::Board(
            op @ (ctl::BoardOp::Set { .. }
            | ctl::BoardOp::Del { .. }
            | ctl::BoardOp::Claim { .. }
            | ctl::BoardOp::Release { .. }),
        ) => board(op, None, Some(label), cx),
        Cmd::Bus(op @ (ctl::BusOp::Pub { .. } | ctl::BusOp::Resolve { .. })) => {
            bus(op, None, true, Some((label, wake)), cx)
        }
        other => ctl::reply_err(&format!(
            "{:?} is not open to operators",
            atrium::operator::verb(&other)
        )),
    }
}

/// Is this pane an agent's: is the command it was spawned with an agent CLI
/// (`claude`, a claude alias, `codex`, …)? The test is the command, not what
/// the process is doing now. A pane's recorded command is set when atrium
/// starts it and by nothing else: no ctl request rewrites it (`respawn` replays
/// it), so the answer holds for the pane's life.
pub(super) fn is_agent_pane(p: &Pane) -> bool {
    let program = p.argv.first().map(String::as_str).unwrap_or(&p.title);
    atrium::bind::is_agent_stem(&atrium::bind::command_stem(program))
}

/// The refusal for an operator's `send` or `ask` whose target is not an
/// agent's pane. Text sent to a pane is typed into it and submitted. Into an
/// agent that is a prompt, and what the agent then does is bounded by its
/// trust mode. Into a shell it is a command line, run with no gate at all:
/// `send 0 "curl … | sh"` would be command execution under any trust. So an
/// operator's text reaches agent panes only. (A target that does not resolve
/// is left to the handler, which says so.)
fn not_an_agent(windows: &[Window], target: &str) -> Option<ctl::Reply> {
    let id = ctl::resolve_target(target, &ctl_candidates(windows)).ok()?;
    let pane = pane_by_agent(windows, id)?;
    // The shared rule ([`ctl::text_may_land`]), at its strictest: for an
    // operator neither a mod's inbox nor a `skip` session opens another pane.
    let lands = ctl::text_may_land(
        is_agent_pane(pane),
        false,
        ctl::TextFrom::Operator,
        trust_mode(),
    );
    (!lands).then(|| {
        ctl::reply_err(&format!(
            "pane {id} is not an agent's pane: an operator's text is typed only into a pane \
             whose command is an agent (it would be a command line to a shell)"
        ))
    })
}

/// How an operator's `bus pub` may wake panes. A wake is a framed line typed
/// into a pane, so it follows the rule for an operator's text, and one more:
///
/// * it is typed into **agent panes only** ([`is_agent_pane`]); a shell that
///   is addressed, or that subscribed, is not woken;
/// * a pane the event merely **addresses** (`--to`) is woken only when the
///   operator's entry also names `send`. Choosing which pane gets text typed
///   into it is what `send` grants; `bus pub` alone reaches the panes that
///   subscribed to the topic, which asked for it.
///
/// The event is on the bus either way, for `bus feed`.
#[derive(Clone, Copy)]
struct OperatorWake {
    may_send: bool,
}

/// May a bus wake be queued for pane `id`? A wake is a line typed into a pane
/// and submitted, so it follows the rule for all such text
/// ([`ctl::text_may_land`]): an agent's pane takes it; any other pane takes
/// none from an operator, and from a pane only when its own mod collects its
/// deliveries (`inbox`, nothing is typed) or the session's trust is `skip`.
///
/// The frame does not make a wake safe for a shell. Its body is the
/// publisher's (`msg`, the fields) or, for the decisions atrium posts itself,
/// built from what panes reported (a file path, a role), and only control
/// characters are taken out of it: `&`, `|`, `;`, `` ` `` and `$(…)` stay, and
/// a shell that is handed `[atrium bus #7 …] x & calc` runs `calc`.
///
/// A wake that may not land is skipped, not refused: the event is on the bus
/// either way, for `bus feed` and the decisions panel.
fn wake_may_land(
    windows: &[Window],
    mods: &atrium::modstate::ModState,
    id: AgentId,
    from: ctl::TextFrom,
    policy: ctl::TrustMode,
) -> bool {
    pane_by_agent(windows, id).is_some_and(|p| {
        ctl::text_may_land(is_agent_pane(p), mods.has_cap(id.0, "inbox"), from, policy)
    })
}

/// A `ctl spawn` whose role would read as an operator's label on the board
/// and the bus (`operator`, `operator:<name>`, or a look-alike of either; see
/// [`atrium::operator::role_reserved`]): refused for every caller.
fn reserved_role(sp: &ctl::SpawnReq) -> Option<ctl::Reply> {
    let role = sp.role.as_deref()?;
    atrium::operator::role_reserved(role).then(|| {
        ctl::reply_err(
            "this role is reserved: \"operator\", \"operator:<name>\" and \"atrium\" (and \
             anything that reads like them) name callers that are not panes",
        )
    })
}

/// What a served request may have done to the screen, so the loop repaints
/// only as much as it must. Until 0.42.0 every request reset the renderer
/// and forced a full repaint, a `2J` clear: with a mod polling `inbox` every
/// 1.5 s per pane, an eight-pane fleet cleared the screen about five times a
/// second, and on Windows every write to the console host opens a ~16 ms frame
/// that the next keystroke waits behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CtlEffect {
    /// A read, or a poll that found nothing: the screen is as it was.
    Nothing,
    /// Broker state a panel or the bar shows (board, bus, a report, a send):
    /// repaint the chrome, keep the renderer's diff baseline.
    Chrome,
    /// Panes came or went: the layout changed, so the baseline is void and the
    /// next frame is drawn from scratch.
    Layout,
}

impl CtlEffect {
    /// Classify by the verb. A failed `spawn` or `kill` costs one needless
    /// full repaint; those are rare, and the alternative is a reply-shape
    /// match that drifts the moment a reply variant is added.
    pub(crate) fn of(cmd: &Cmd) -> CtlEffect {
        match cmd {
            Cmd::Spawn(_) | Cmd::Kill(_) | Cmd::Respawn(_) => CtlEffect::Layout,
            _ if cmd.is_read_only() => CtlEffect::Nothing,
            _ => CtlEffect::Chrome,
        }
    }
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
        Cmd::List => reply_tree(cx.windows, cx.world, cx.mods, None),
        Cmd::Status(sr) => status(sr, caller, privileged, cx),
        Cmd::Send(sr) => send(sr, caller, privileged, cx),
        Cmd::Command(c) => command(c, caller, privileged, None, &ctl::session_commands(), cx),
        Cmd::Spawn(sp) => match reserved_role(&sp) {
            Some(refusal) => refusal,
            None => spawn(sp, caller, privileged, cx),
        },
        Cmd::Kill(kr) => kill(kr, caller, privileged, cx),
        Cmd::Respawn(rr) => respawn(rr, caller, privileged, cx),
        // `audit` is handled in `apply_ctl` (it needs the log); never reaches here.
        Cmd::Audit(_) => ctl::reply_err("internal: audit dispatched to the wrong handler"),
        Cmd::Board(op) => board(op, caller, None, cx),
        Cmd::Bus(op) => bus(op, caller, privileged, None, cx),
        Cmd::Hello(h) => hello(h, caller, cx),
        Cmd::Report(r) => report(r, caller, cx),
        Cmd::Whoami => whoami(caller, cx),
        Cmd::Answer(a) => answer(a, caller, privileged, cx),
        Cmd::Inbox(i) => inbox(i, caller, cx),
        Cmd::Ask(a) => ask(a, caller, privileged, cx),
        Cmd::Asked(a) => asked(a, caller, privileged, cx),
        Cmd::Who(w) => who(w, caller, privileged, cx),
        Cmd::Ping => ctl::reply_pong(uptime_ms()),
        Cmd::Metrics => metrics(cx),
    }
}

/// When the session's run loop started; `None` until [`mark_session_start`].
static SESSION_START: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Note that a session's run loop starts now: what `ping` and `metrics`
/// measure uptime from, and where the session's delivered-wake count begins.
/// Called once per [`crate::run_loop`] run, before its first tick.
pub(crate) fn mark_session_start() {
    *SESSION_START.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    super::sends::reset_wakes_delivered();
}

/// Milliseconds since [`mark_session_start`]; `0` if no loop has started.
fn uptime_ms() -> u64 {
    let started = *SESSION_START.lock().unwrap_or_else(|e| e.into_inner());
    started.map_or(0, |t| {
        u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
    })
}

/// `ctl metrics`: the session's counters, each read from state the broker
/// already holds, at reply time: one pass over the panes, three reads of the
/// bus ring (512 events at most), two stored numbers. Counts only: no pane's
/// role, title or text is in the reply, so it is served without a token.
fn metrics(cx: &CtlSession<'_>) -> ctl::Reply {
    let mut m = ctl::Metrics {
        uptime_ms: uptime_ms(),
        wakes_delivered: super::sends::wakes_delivered(),
        held_inboxes: cx.held_inboxes,
        bus_depth: cx.bus.tail(usize::MAX).len(),
        bus_last_seq: cx.bus.tail(1).last().map_or(0, |e| e.seq),
        open_decisions: cx.bus.pending_decisions().len(),
        ..ctl::Metrics::default()
    };
    // The same status `list` shows: the mod's own report, else the inference.
    let now_ms = atrium::session_store::now_ms();
    for p in cx.windows.iter().flat_map(|w| &w.panes) {
        m.count_pane(
            cx.mods
                .status_for(p.agent_id.0, now_ms)
                .map(|s| s.label())
                .or_else(|| {
                    cx.world
                        .status_for(p.session_id.as_deref())
                        .map(status_label)
                }),
        );
    }
    ctl::reply_metrics(m)
}

/// `ctl ask`: queue a question for a pane's mod, which answers it from a fork
/// of the pane's own context, without interrupting its turn. Subtree-scoped
/// like `send`; the target's mod must have declared `ask`. The reply is read
/// with `asked` (the `ask` client loop does that).
fn ask(
    a: ctl::AskReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    let windows = &*cx.windows;
    let candidates = ctl_candidates(windows);
    let id = match ctl::resolve_target(&a.target, &candidates) {
        Ok(id) => id,
        Err(e) => return ctl::reply_err(&e),
    };
    if let Some(deny) = scope_denied(windows, caller, privileged, id) {
        return deny;
    }
    if !cx.mods.has_cap(id.0, "ask") || !cx.mods.has_cap(id.0, "inbox") {
        return ctl::reply_err(&format!(
            "pane {id} cannot be asked: its mod declared no ask capability (no mod, or an older one)"
        ));
    }
    let now = atrium::session_store::now_ms();
    match cx.mods.ask(id.0, caller.map(|c| c.0), &a.text, now) {
        Ok(ask_id) => ctl::reply_asked(id, ask_id, None),
        Err(e) => ctl::reply_err(&e),
    }
}

/// `ctl asked`: the reply to an ask, once the pane's mod reported it. The
/// same scope as `ask`; a pane that answered and left stays readable by its
/// parent, as `answer` does.
fn asked(
    a: ctl::AskedReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &CtlSession<'_>,
) -> ctl::Reply {
    let windows = &*cx.windows;
    let candidates = ctl_candidates(windows);
    let id = match ctl::resolve_target(&a.target, &candidates) {
        Ok(id) => {
            if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                return deny;
            }
            id
        }
        Err(e) => {
            let gone = a
                .target
                .parse::<usize>()
                .ok()
                .filter(|n| cx.mods.info(*n).is_some() || cx.mods.reply_for(a.id).is_some());
            let Some(n) = gone else {
                return ctl::reply_err(&e);
            };
            let may =
                privileged || (caller.is_some() && cx.mods.parent_of(n) == caller.map(|c| c.0));
            if !may {
                return ctl::reply_err(&format!(
                    "pane {n} has exited; only the pane that spawned it may read its replies"
                ));
            }
            AgentId(n)
        }
    };
    match cx.mods.reply_for(a.id) {
        Some(r) if r.pane == id.0 => ctl::reply_asked(id, a.id, Some(&r.text)),
        Some(_) => ctl::reply_err(&format!("ask #{} was not put to pane {id}", a.id)),
        None if cx.mods.ask_pending(a.id) => ctl::reply_asked(id, a.id, None),
        None => ctl::reply_err(&format!(
            "no such ask #{} for pane {id} (never asked, or its reply expired)",
            a.id
        )),
    }
}

/// `ctl who`: every pane whose mod reported an edit of `path`, with when, and
/// whether the pane is still here.
/// `ctl who`: which panes edited a file. Subtree-scoped like `status`: a
/// caller sees the panes in its own tree (a pane that has left the fleet is
/// nobody's), the operator every pane that ever reported.
fn who(
    w: ctl::WhoReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &CtlSession<'_>,
) -> ctl::Reply {
    let windows = &*cx.windows;
    let parents = ctl_parents(windows);
    let visible = |pane: usize| {
        privileged || caller.is_some_and(|c| atrium::ctl::in_subtree(AgentId(pane), c, &parents))
    };
    let panes = cx
        .mods
        .who(&w.path)
        .into_iter()
        .filter(|(pane, _, _)| visible(*pane))
        .map(|(pane, key, t)| {
            let live = pane_by_agent(windows, AgentId(pane));
            ctl::WhoEntry {
                pane: AgentId(pane),
                role: live.and_then(|p| p.role.clone()),
                key: key.to_string(),
                path: t.path.clone(),
                first_ms: t.first_ms,
                last_ms: t.last_ms,
                live: live.is_some(),
            }
        })
        .collect();
    ctl::reply_who(&w.path, panes)
}

/// The refusal for an `inbox` from a pane whose mod did not declare the cap.
pub(crate) const INBOX_NO_CAP: &str =
    "this pane's mod did not declare inbox in its hello; deliveries are typed into it";

/// `ctl inbox`: hand the caller's mod its queued deliveries. Only a pane whose
/// mod declared `inbox` is served (its queue is otherwise the pty path's, and
/// a send must land exactly once); the items leave the queue as they go.
/// `--wait` with nothing queued asks the run loop to hold the connection
/// (`cx.hold`) and answer it when something lands.
fn inbox(req: ctl::InboxReq, caller: Option<AgentId>, cx: &mut CtlSession<'_>) -> ctl::Reply {
    let Some(me) = caller else {
        return ctl::reply_err("inbox needs an authenticated pane (ATRIUM_TOKEN)");
    };
    if !cx.mods.has_cap(me.0, "inbox") {
        return ctl::reply_err(INBOX_NO_CAP);
    }
    let items = inbox_items(me, cx.pending, cx.mods);
    if req.wait && items.is_empty() {
        *cx.hold = Some(me);
    }
    ctl::reply_inbox(items)
}

/// Take `me`'s deliveries — queued sends and wakes the pty path has not begun
/// to type, then its queued questions — as the items of an `inbox` reply.
/// Nothing is taken when nothing is queued, so an empty answer is also a
/// non-destructive "is anything there?".
pub(crate) fn inbox_items(
    me: AgentId,
    pending: &mut Vec<PendingSend>,
    mods: &mut atrium::modstate::ModState,
) -> Vec<ctl::InboxItem> {
    let mut items: Vec<ctl::InboxItem> = take_for(pending, me)
        .into_iter()
        .map(|(origin, text)| {
            let kind = match origin {
                // A command is handed over as the prompt it is.
                SendOrigin::Ctl | SendOrigin::Command => "send",
                SendOrigin::BusWake => "wake",
            };
            ctl::InboxItem {
                kind: kind.to_string(),
                text,
                id: None,
            }
        })
        .collect();
    // Questions ride the same poll; the mod answers them from a fork, never
    // as a prompt.
    if mods.has_cap(me.0, "ask") {
        items.extend(mods.take_asks(me.0).into_iter().map(|a| ctl::InboxItem {
            kind: "ask".to_string(),
            text: a.text,
            id: Some(a.id),
        }));
    }
    items
}

/// `ctl answer`: a pane's last reported answer and status, for the ancestor
/// that spawned it. Subtree-scoped like `send`: the model's output of a pane
/// goes up the tree it was spawned into and nowhere else.
fn answer(
    a: ctl::AnswerReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &CtlSession<'_>,
) -> ctl::Reply {
    let windows = &*cx.windows;
    let candidates = ctl_candidates(windows);
    let id = match ctl::resolve_target(&a.target, &candidates) {
        Ok(id) => id,
        Err(e) => {
            // A pane that answered and left (a `claude -p` subagent) is gone
            // from the tree but its report is kept: its parent, as recorded at
            // the pane's own hello, may still read the answer. Nobody else.
            let gone = a
                .target
                .parse::<usize>()
                .ok()
                .filter(|n| cx.mods.info(*n).is_some());
            let Some(n) = gone else {
                return ctl::reply_err(&e);
            };
            let may = privileged
                || (caller.map(|c| c.0).is_some() && cx.mods.parent_of(n) == caller.map(|c| c.0));
            if !may {
                return ctl::reply_err(&format!(
                    "pane {n} has exited; only the pane that spawned it may read its answer"
                ));
            }
            let report = cx.mods.report_for(n);
            return ctl::reply_answer(
                AgentId(n),
                report.and_then(|r| r.status.map(|(s, _)| s.label())),
                report.and_then(|r| r.answer.as_ref().map(|(t, s)| (t.as_str(), *s))),
            );
        }
    };
    if let Some(deny) = scope_denied(windows, caller, privileged, id) {
        return deny;
    }
    let status = cx
        .mods
        .status_for(id.0, atrium::session_store::now_ms())
        .map(|s| s.label());
    let report = cx.mods.report_for(id.0);
    ctl::reply_answer(
        id,
        status,
        report.and_then(|r| r.answer.as_ref().map(|(t, s)| (t.as_str(), *s))),
    )
}

/// `ctl hello`: the caller's mod announces itself. The pane is the token's,
/// never a named one, so a mod can only ever speak for the pane it runs in.
fn hello(h: ctl::HelloReq, caller: Option<AgentId>, cx: &mut CtlSession<'_>) -> ctl::Reply {
    let Some(me) = caller else {
        return ctl::reply_err("hello needs an authenticated pane (ATRIUM_TOKEN)");
    };
    let parent = pane_by_agent(cx.windows, me)
        .and_then(|p| p.parent)
        .map(|p| p.0);
    let accepted = cx.mods.hello(
        me.0,
        parent,
        &h.version,
        h.engine.as_deref(),
        &h.caps,
        atrium::session_store::now_ms(),
    );
    ctl::reply_hello(me, accepted)
}

/// `ctl report`: the caller's mod reports its own status, usage or answer.
fn report(r: ctl::ReportReq, caller: Option<AgentId>, cx: &mut CtlSession<'_>) -> ctl::Reply {
    let Some(me) = caller else {
        return ctl::reply_err("report needs an authenticated pane (ATRIUM_TOKEN)");
    };
    let now = atrium::session_store::now_ms();
    let seq = cx.mods.report(me.0, &r, now);
    // A reply to a queued question: accepted only from the pane it was put to.
    // A refusal is the reply, but the rest of the report — a status, a
    // caption, the files edited — is applied first, as it would be alone.
    let refused_ask = match &r.ask {
        Some((id, text)) if !cx.mods.reply(me.0, *id, text, now) => Some(format!(
            "ask #{id} was not put to pane {me}, or is already answered"
        )),
        _ => None,
    };
    // Files edited, keyed by the pane's worktree, so two worktrees' copies of
    // one file collide; a pane with no worktree keys by the absolute path (not
    // its cwd: two unrelated repos' `README.md` are two files). The first time
    // two live panes share a key, one decision goes to the nearest pane above.
    if !r.touched.is_empty() {
        let root = pane_by_agent(cx.windows, me).and_then(|p| p.worktree.clone());
        cx.mods.touch(me.0, &r.touched, root.as_deref(), now);
        let live: Vec<usize> = ctl_candidates(cx.windows)
            .iter()
            .map(|(id, _)| id.0)
            .collect();
        for c in cx.mods.collisions_new(&live) {
            post_collision_decision(&c, now, cx);
        }
    }
    // The fleet's `respawn_at`: a pane whose context fill crossed it while idle
    // gets one decision posted to its parent, never a respawn by itself.
    if let Some(threshold) = atrium::modstate::session_respawn_at() {
        if cx.mods.respawn_crossing(me.0, threshold, now) {
            post_respawn_decision(me, threshold, now, cx);
        }
    }
    match refused_ask {
        Some(err) => ctl::reply_err(&err),
        None => ctl::reply_reported(me, seq),
    }
}

/// The nearest pane above every one of `panes` (a lead over its builders),
/// or `None` when they share no ancestor (the human decides).
fn common_ancestor(windows: &[Window], panes: &[usize]) -> Option<AgentId> {
    let parents = ctl_parents(windows);
    let parent_of = |id: AgentId| parents.iter().find(|(i, _)| *i == id).and_then(|(_, p)| *p);
    let first = AgentId(*panes.first()?);
    let mut cur = parent_of(first);
    for _ in 0..4096 {
        let candidate = cur?;
        if panes
            .iter()
            .all(|p| atrium::ctl::in_subtree(AgentId(*p), candidate, &parents))
        {
            return Some(candidate);
        }
        cur = parent_of(candidate);
    }
    None
}

/// One `decision_needed` on the `atrium` topic for a collision: the file and
/// the panes named, addressed to the nearest pane above them all, which is
/// woken as any addressed pane is. Posted once per collision
/// ([`atrium::modstate::ModState::collisions_new`]).
fn post_collision_decision(c: &atrium::modstate::Collision, now: u64, cx: &mut CtlSession<'_>) {
    let (windows, bus, pending) = (&*cx.windows, &mut *cx.bus, &mut *cx.pending);
    let names: Vec<String> = c
        .panes
        .iter()
        .map(|p| {
            pane_label(
                AgentId(*p),
                pane_by_agent(windows, AgentId(*p)).and_then(|q| q.role.as_deref()),
            )
        })
        .collect();
    let mut fields = vec![
        ("kind".to_string(), "collision".to_string()),
        ("file".to_string(), c.key.clone()),
        (
            "panes".to_string(),
            c.panes
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
        (
            "msg".to_string(),
            format!(
                "{} are both editing {}: the merge will conflict; who owns it?",
                names.join(" and "),
                c.key
            ),
        ),
    ];
    if let Some(lead) = common_ancestor(windows, &c.panes).and_then(|id| pane_by_agent(windows, id))
    {
        fields.push((
            "to".to_string(),
            pane_label(lead.agent_id, lead.role.as_deref()),
        ));
    }
    let detail = format!("file={} panes={}", c.key, names.join(","));
    match bus.publish(
        "atrium",
        atrium::bus::Kind::DecisionNeeded,
        None,
        &fields,
        now,
    ) {
        Ok(e) => {
            let candidates = ctl_candidates(windows);
            let parents = ctl_parents(windows);
            let wakes = bus_wakes(&e, "atrium", None, true, &candidates, &parents, |label| {
                bus.subscriptions(label)
                    .is_some_and(|t| t.contains(&e.topic) || t.contains("*"))
            });
            // The message names a file and roles, which panes chose.
            let policy = trust_mode();
            for (tid, text) in wakes {
                if wake_may_land(windows, cx.mods, tid, ctl::TextFrom::Pane, policy) {
                    queue_send(pending, tid, text, SendOrigin::BusWake, SentBy::default());
                }
            }
            cx.audit.record(None, "collision", &detail, true, "");
        }
        Err(msg) => {
            cx.audit.record(None, "collision", &detail, false, &msg);
        }
    }
}

/// One `decision_needed` on the `atrium` topic, addressed to the pane's parent
/// (the human when it has none), with the pane and its fill named, and the
/// parent woken as any addressed pane is. A publish the bus refuses (a full
/// ring, the rate cap) is audited and dropped: the next crossing asks again.
fn post_respawn_decision(pane: AgentId, threshold: u8, now: u64, cx: &mut CtlSession<'_>) {
    let (windows, bus, pending) = (&*cx.windows, &mut *cx.bus, &mut *cx.pending);
    let Some(p) = pane_by_agent(windows, pane) else {
        return;
    };
    let me = pane_label(pane, p.role.as_deref());
    let pct = cx
        .mods
        .report_for(pane.0)
        .and_then(|r| r.context_pct)
        .unwrap_or(threshold);
    let mut fields = vec![
        ("pane".to_string(), pane.0.to_string()),
        (
            "msg".to_string(),
            format!(
                "{me} is at {pct}% context (respawn_at {threshold}%): checkpoint and respawn it?"
            ),
        ),
    ];
    if let Some(parent) = p.parent.and_then(|id| pane_by_agent(windows, id)) {
        fields.push((
            "to".to_string(),
            pane_label(parent.agent_id, parent.role.as_deref()),
        ));
    }
    let detail = format!("pane={pane} pct={pct}");
    match bus.publish(
        "atrium",
        atrium::bus::Kind::DecisionNeeded,
        None,
        &fields,
        now,
    ) {
        Ok(e) => {
            let candidates = ctl_candidates(windows);
            let parents = ctl_parents(windows);
            let wakes = bus_wakes(&e, "atrium", None, true, &candidates, &parents, |label| {
                bus.subscriptions(label)
                    .is_some_and(|t| t.contains(&e.topic) || t.contains("*"))
            });
            // The message names the pane by its role, which its spawner chose.
            let policy = trust_mode();
            for (tid, text) in wakes {
                if wake_may_land(windows, cx.mods, tid, ctl::TextFrom::Pane, policy) {
                    queue_send(pending, tid, text, SendOrigin::BusWake, SentBy::default());
                }
            }
            cx.audit.record(None, "respawn-decision", &detail, true, "");
        }
        Err(msg) => {
            cx.audit
                .record(None, "respawn-decision", &detail, false, &msg);
        }
    }
}

/// `ctl whoami`: what the caller is, from the broker's own records: its pane,
/// role, parent, depth, trust mode, worktree, cwd, deny rules, whether it may
/// spawn, what its mod said, and the board item briefed to its role (an entry
/// whose `owner` or `builder` field names the role) with that item's `files`.
fn whoami(caller: Option<AgentId>, cx: &CtlSession<'_>) -> ctl::Reply {
    use json::{Number, Value};
    let Some(me) = caller else {
        return ctl::reply_err("whoami needs an authenticated pane (ATRIUM_TOKEN)");
    };
    let Some(p) = pane_by_agent(cx.windows, me) else {
        return ctl::reply_err("the calling pane is gone");
    };
    let s = |v: &str| Value::String(v.to_string());
    let opt = |v: Option<&str>| v.map(s).unwrap_or(Value::Null);
    let int = |n: usize| Value::Number(Number::Int(n as i64));
    let mut fields: Vec<(String, Value)> = vec![
        ("pane".to_string(), int(me.0)),
        ("role".to_string(), opt(p.role.as_deref())),
        (
            "parent".to_string(),
            p.parent.map(|a| int(a.0)).unwrap_or(Value::Null),
        ),
        ("depth".to_string(), int(p.depth)),
        ("mode".to_string(), s(p.mode.policy_label())),
        ("worktree".to_string(), opt(p.worktree.as_deref())),
        ("cwd".to_string(), opt(p.cwd.as_deref())),
        ("can_spawn".to_string(), Value::Bool(p.can_spawn)),
        (
            "deny".to_string(),
            Value::Array(p.deny.iter().map(|d| s(d)).collect()),
        ),
    ];
    let modv = match cx.mods.info(me.0) {
        Some(i) => Value::Object(
            vec![
                ("version".to_string(), s(&i.version)),
                ("engine".to_string(), opt(i.engine.as_deref())),
                (
                    "caps".to_string(),
                    Value::Array(i.caps.iter().map(|c| s(c)).collect()),
                ),
            ]
            .into_iter()
            .collect(),
        ),
        None => Value::Null,
    };
    fields.push(("mod".to_string(), modv));
    // How this session runs subagents, for the mod's agent.spawn hook.
    let (subagents, keep) = atrium::modstate::session_subagents();
    fields.push(("subagents".to_string(), s(subagents.as_str())));
    fields.push(("subagents_keep".to_string(), Value::Bool(keep)));
    // Whether the mod may spend a small model call on captions.
    fields.push((
        "captions".to_string(),
        Value::Bool(atrium::modstate::session_captions()),
    ));
    // The item briefed to this role, if the lead recorded one on the board.
    let item = p.role.as_deref().and_then(|role| {
        cx.board.list().into_iter().find(|(_, e)| {
            e.fields.get("owner").map(String::as_str) == Some(role)
                || e.fields.get("builder").map(String::as_str) == Some(role)
        })
    });
    match item {
        Some((key, entry)) => {
            fields.push(("item".to_string(), s(key)));
            let files: Vec<Value> = entry
                .fields
                .get("files")
                .map(|f| {
                    f.split(|c: char| c == ',' || c.is_whitespace())
                        .filter(|x| !x.is_empty())
                        .map(s)
                        .collect()
                })
                .unwrap_or_default();
            fields.push(("files".to_string(), Value::Array(files)));
        }
        None => {
            fields.push(("item".to_string(), Value::Null));
            fields.push(("files".to_string(), Value::Array(Vec::new())));
        }
    }
    ctl::reply_whoami(fields)
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
            reply_tree(windows, world, cx.mods, root)
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
            // A status the pane's own mod reported wins over the inference and
            // keeps its richer label (`errored`, `ended`).
            let status = cx
                .mods
                .status_for(id.0, atrium::session_store::now_ms())
                .map(|s| s.label())
                .or_else(|| {
                    pane.and_then(|p| world.status_for(p.session_id.as_deref()).map(status_label))
                });
            let idle = pane
                .map(|p| atrium::ipc::idle_ms(p.last_activity, std::time::Instant::now()))
                .unwrap_or(0);
            // The caption rides along while the status is fresh: what the
            // pane is doing, for a lead's `atrium_status` as for the overview.
            let doing = cx
                .mods
                .status_for(id.0, atrium::session_store::now_ms())
                .and_then(|_| cx.mods.report_for(id.0))
                .and_then(|r| r.doing.as_ref().map(|(d, _)| d.as_str()));
            ctl::reply_status_one(id, status, idle, doing)
        }
    }
}

/// `ctl send`: queue text for a target in the caller's subtree, under the
/// session's launch trust.
fn send(
    sr: ctl::SendReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    send_under(sr, caller, privileged, None, trust_mode(), cx)
}

/// [`send`] with the session's trust policy given, so the rule below is
/// testable at every policy.
///
/// After the subtree guard, the target must be a pane text may land in
/// ([`ctl::text_may_land`]). A root pane reaches every pane, and a fleet may
/// declare shell panes (a plain `atrium --allow-ctl` hosts the default shell
/// as pane 0), so an agent held to `plan` could otherwise type a command line
/// into a sibling shell and have it run with no gate at all. An operator's
/// stricter form of the rule is applied before this is called
/// ([`not_an_agent`]); what reaches here from one is an agent's pane, which
/// passes for everyone. `operator` is that caller's label (`operator:<name>`).
///
/// Then the text. It may hold no control character below `skip`
/// ([`ctl::vet_send_text`]), and what is queued for an agent is the text
/// behind atrium's attribution header ([`ctl::with_send_header`]): the first
/// character of the prompt is atrium's, so nothing the caller wrote can be
/// read by the agent CLI as a shell line (`!`), a command (`/`), a memory
/// note (`#`) or a mention (`@`). The header is put on here, once, so the
/// text reads the same typed into the pty or handed to the pane's mod.
fn send_under(
    mut sr: ctl::SendReq,
    caller: Option<AgentId>,
    privileged: bool,
    operator: Option<&str>,
    policy: ctl::TrustMode,
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
    if let Some(pane) = pane_by_agent(windows, id) {
        let lands = ctl::text_may_land(
            is_agent_pane(pane),
            cx.mods.has_cap(id.0, "inbox"),
            ctl::TextFrom::Pane,
            policy,
        );
        if !lands {
            return ctl::reply_err(&ctl::send_into_non_agent_refusal(id));
        }
    }
    let from = match operator {
        Some(_) => ctl::TextFrom::Operator,
        None => ctl::TextFrom::Pane,
    };
    sr.text = match ctl::vet_send_text(&sr.text, from, policy) {
        Ok(text) => text,
        Err(why) => return ctl::reply_err(why),
    };
    // Headed wherever it becomes a prompt: an agent's pane, or a pane whose
    // mod submits its deliveries. Bare only where it is typed into a pane
    // that is not an agent's, which the rule above allows at `skip` alone: a
    // shell has no prompt to protect, and a header would be a broken command.
    let headed = pane_by_agent(windows, id)
        .map_or(true, |p| is_agent_pane(p) || cx.mods.has_cap(id.0, "inbox"));
    if headed {
        // The sender as the broker knows it: an operator's name, or the
        // calling pane's ctl id (what the recipient can answer) and role.
        let sender_pane = caller.and_then(|c| pane_by_agent(windows, c));
        let sender = match (operator, sender_pane) {
            (Some(label), _) => {
                ctl::Sender::Operator(label.strip_prefix("operator:").unwrap_or(label))
            }
            (None, Some(p)) => ctl::Sender::Pane {
                id: p.agent_id.0,
                role: p.role.as_deref(),
            },
            (None, None) => ctl::Sender::Atrium,
        };
        sr.text = ctl::with_send_header(sender, &sr.text);
    }
    // Queued iff the target is mid-turn now; either way delivery is async
    // and happens when the target is idle.
    let busy = matches!(
        pane_by_agent(windows, id).and_then(|p| world.status_for(p.session_id.as_deref())),
        Some(agsess::Status::Working) | Some(agsess::Status::WaitingApproval)
    );
    let by = SentBy {
        pane: caller,
        operator: operator.map(str::to_string),
    };
    if !queue_send(pending, id, sr.text, SendOrigin::Ctl, by) {
        return ctl::reply_err(&format!(
            "agent {id} already has {MAX_PENDING_PER_TARGET} undelivered sends; \
                 it is not taking delivery (busy, a dialog open, or a draft). \
                 Check `ctl status` before sending more"
        ));
    }
    ctl::reply_sent(id, busy)
}

/// `ctl command`: deliver one slash command to an agent's pane as its own
/// prompt line: no header, typed as it is, then Enter.
///
/// A `send` can no longer be a command of the agent CLI (its first character
/// is atrium's header), so this is the one way a ctl caller runs one, and it
/// is closed unless the human opened it: the name must be on the session's
/// allowlist (`allowed`: the fleet's `commands`, else the user config's;
/// empty by default), which no request can add to. The name is one word of a
/// short alphabet with no arguments ([`ctl::vet_command`]), so `!` is not
/// expressible. The scope is `send`'s (a pane reaches its own subtree, an
/// operator the session), and the target must be an agent's pane at any
/// trust: a slash command typed into a shell is a path to run. Delivery is
/// queued like a send and waits for the prompt the same way.
fn command(
    c: ctl::CommandReq,
    caller: Option<AgentId>,
    privileged: bool,
    operator: Option<&str>,
    allowed: &[String],
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    let (windows, world, pending) = (&*cx.windows, cx.world, &mut *cx.pending);
    let candidates = ctl_candidates(windows);
    let id = match ctl::resolve_target(&c.target, &candidates) {
        Ok(id) => id,
        Err(e) => return ctl::reply_err(&e),
    };
    if let Some(deny) = scope_denied(windows, caller, privileged, id) {
        return deny;
    }
    let Some(pane) = pane_by_agent(windows, id) else {
        return ctl::reply_err("command: target pane not found");
    };
    if !is_agent_pane(pane) {
        return ctl::reply_err(&format!(
            "pane {id} is not an agent's pane: a slash command is delivered only to the agent \
             CLIs atrium knows ({}, or a claude alias), at any trust",
            atrium::bind::AGENT_STEMS.join(", ")
        ));
    }
    if let Err(why) = ctl::vet_command(&c.name, allowed) {
        return ctl::reply_err(&why);
    }
    let busy = matches!(
        world.status_for(pane.session_id.as_deref()),
        Some(agsess::Status::Working) | Some(agsess::Status::WaitingApproval)
    );
    let by = SentBy {
        pane: caller,
        operator: operator.map(str::to_string),
    };
    if !queue_send(pending, id, c.name, SendOrigin::Command, by) {
        return ctl::reply_err(&format!(
            "agent {id} already has {MAX_PENDING_PER_TARGET} undelivered sends; it is not \
             taking delivery. Check `ctl status` before sending more"
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
///
/// `operator` is the label of an operator-token caller (`operator:<name>`),
/// which has no pane to derive one from; `None` for every other caller.
fn board(
    op: ctl::BoardOp,
    caller: Option<AgentId>,
    operator: Option<&str>,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
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
        .or_else(|| operator.map(str::to_string))
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
///
/// `operator` is the label of an operator-token caller (`operator:<name>`),
/// as for [`board`].
fn bus(
    op: ctl::BusOp,
    caller: Option<AgentId>,
    privileged: bool,
    operator: Option<(&str, OperatorWake)>,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    bus_under(op, caller, privileged, operator, trust_mode(), cx)
}

/// [`bus`] with the session's trust policy given: it decides which panes a
/// publish may wake ([`wake_may_land`]), so it is testable at every policy.
fn bus_under(
    op: ctl::BusOp,
    caller: Option<AgentId>,
    privileged: bool,
    operator: Option<(&str, OperatorWake)>,
    policy: ctl::TrustMode,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    let (windows, bus, pending) = (&*cx.windows, &mut *cx.bus, &mut *cx.pending);
    let mods = &*cx.mods;
    let (operator, operator_wake) = (operator.map(|o| o.0), operator.map(|o| o.1));
    // The bus is SHARED like the board. The server derives `who` (the
    // caller's role, else its pane, else the operator) so a worker
    // publishes and subscribes *as itself* — it cannot forge another
    // sender. `who` is stable per pane, so the no-echo filter and a
    // subscriber's cursor stay consistent across calls.
    let who = caller
        .and_then(|cid| pane_by_agent(windows, cid))
        .map(|p| pane_label(p.agent_id, p.role.as_deref()))
        .or_else(|| operator.map(str::to_string))
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
                // Text is typed only where it may land: an agent's pane for
                // everyone; for a pane's publish also a pane whose mod takes
                // its own deliveries, or any pane in a `skip` session.
                let from = match operator_wake {
                    Some(_) => ctl::TextFrom::Operator,
                    None => ctl::TextFrom::Pane,
                };
                // Panes that were due a wake and got none because a line may
                // not be typed into them: named in the reply, and taken out
                // of the count the publisher reads as "who this reached".
                let mut not_woken: Vec<AgentId> = Vec::new();
                for (tid, text) in wakes {
                    if !wake_may_land(windows, mods, tid, from, policy) {
                        not_woken.push(tid);
                        continue;
                    }
                    if let Some(rule) = operator_wake {
                        let Some(pane) = pane_by_agent(windows, tid) else {
                            continue;
                        };
                        let subscribed = bus
                            .subscriptions(&pane_label(tid, pane.role.as_deref()))
                            .is_some_and(|t| t.contains(&e.topic) || t.contains("*"));
                        if !(subscribed || rule.may_send) {
                            continue;
                        }
                    }
                    let by = SentBy {
                        pane: caller,
                        operator: operator.map(str::to_string),
                    };
                    queue_send(pending, tid, text, SendOrigin::BusWake, by);
                }
                let skipped_subs = not_woken
                    .iter()
                    .filter(|tid| {
                        pane_by_agent(windows, **tid).is_some_and(|p| {
                            bus.subscriptions(&pane_label(**tid, p.role.as_deref()))
                                .is_some_and(|t| t.contains(&e.topic) || t.contains("*"))
                        })
                    })
                    .count();
                let note = (!not_woken.is_empty()).then(|| {
                    not_woken
                        .iter()
                        .map(|tid| format!("not woken: pane {tid} is not an agent's pane"))
                        .collect::<Vec<_>>()
                        .join("; ")
                        + match from {
                            ctl::TextFrom::Operator => {
                                " (the event is on the bus; an operator's line is typed only \
                                 into an agent's pane, at any trust)"
                            }
                            ctl::TextFrom::Pane => {
                                " (the event is on the bus; below --trust skip a line is typed \
                                 only into an agent's pane)"
                            }
                        }
                });
                ctl::reply_bus_published_noted(
                    atrium::bus::event_to_value(&e),
                    subs.saturating_sub(skipped_subs),
                    note,
                )
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
            // A caller-less read is a host's (a terminal, a dashboard): the
            // operator's view, every event past the cursor, as the ATRIUM_BUS
            // sidecar shows it. Served as "operator" it subscribed to nothing
            // and saw nothing, which broke the documented host read. A pane's
            // feed stays its own subscriptions, minus its own echoes.
            let events: Vec<&atrium::bus::Event> = if caller.is_none() {
                bus.tail(usize::MAX)
                    .into_iter()
                    .filter(|e| e.seq > since)
                    .collect()
            } else {
                bus.feed(&who, since)
            };
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

#[cfg(test)]
mod operator_tests;

#[cfg(test)]
mod ceiling_tests;

#[cfg(test)]
mod effect_tests {
    use super::*;

    fn cmd(line: &str) -> Cmd {
        ctl::parse_request(line).expect("parse").cmd
    }

    #[test]
    fn panes_coming_or_going_void_the_baseline() {
        for line in [
            r#"{"cmd":"spawn","argv":["claude"]}"#,
            r#"{"cmd":"kill","target":"3"}"#,
            r#"{"cmd":"respawn","target":"3"}"#,
        ] {
            assert_eq!(CtlEffect::of(&cmd(line)), CtlEffect::Layout, "{line}");
        }
    }

    #[test]
    fn reads_and_polls_leave_the_screen_alone() {
        for line in [
            r#"{"cmd":"list"}"#,
            r#"{"cmd":"status"}"#,
            r#"{"cmd":"board","op":"list"}"#,
            r#"{"cmd":"bus","op":"feed"}"#,
        ] {
            assert_eq!(CtlEffect::of(&cmd(line)), CtlEffect::Nothing, "{line}");
        }
    }

    #[test]
    fn broker_writes_repaint_the_chrome_without_a_clear() {
        for line in [
            r#"{"cmd":"board","op":"set","key":"m1","fields":{"status":"DONE"}}"#,
            r#"{"cmd":"bus","op":"pub","topic":"work","msg":"x"}"#,
            r#"{"cmd":"report","status":"working"}"#,
            r#"{"cmd":"inbox"}"#,
        ] {
            assert_eq!(CtlEffect::of(&cmd(line)), CtlEffect::Chrome, "{line}");
        }
    }

    /// A probe every few seconds and a scrape every fifteen must never cost a
    /// repaint, and both are answered without a token.
    #[test]
    fn ping_and_metrics_are_token_free_reads_that_draw_nothing() {
        for line in [r#"{"cmd":"ping"}"#, r#"{"cmd":"metrics"}"#] {
            let c = cmd(line);
            assert!(c.is_read_only(), "{line}");
            assert_eq!(CtlEffect::of(&c), CtlEffect::Nothing, "{line}");
        }
    }

    /// Both probes against a live session, with no token on the line: `ping`
    /// carries the uptime since the loop's start, `metrics` the numbers the
    /// broker holds, and neither repaints or leaves an audit entry.
    #[test]
    fn ping_and_metrics_answer_without_a_token_and_are_not_recorded() {
        // The loop started a quarter of a second ago. Only the start is set
        // here: `mark_session_start` also zeroes the wake count, which the
        // delivery tests running beside this one read.
        *SESSION_START.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(250));
        let mut windows = Vec::new();
        let mut pending = Vec::new();
        let world = atrium::vendors::AgentState::default();
        let mut audit = atrium::audit::Audit::new(16, None);
        let mut board = atrium::board::Board::new();
        let mut bus = atrium::bus::Bus::new();
        let mut mods = atrium::modstate::ModState::default();
        let mut hold = None;
        let field = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];
        bus.publish("work", atrium::bus::Kind::Fyi, None, &field("msg", "a"), 1)
            .unwrap();
        let ask = bus
            .publish(
                "work",
                atrium::bus::Kind::DecisionNeeded,
                None,
                &field("msg", "b"),
                2,
            )
            .unwrap();
        let mut cx = CtlSession {
            windows: &mut windows,
            rows: 24,
            cols: 80,
            max_depth: 3,
            extra_allow: &[],
            pending: &mut pending,
            held_inboxes: 2,
            world: &world,
            session_identity: None,
            audit: &mut audit,
            board: &mut board,
            bus: &mut bus,
            mods: &mut mods,
            hold: &mut hold,
        };

        let (reply, effect) = apply_ctl(r#"{"cmd":"ping"}"#, &mut cx);
        assert_eq!(effect, CtlEffect::Nothing);
        match reply {
            ctl::Reply::Pong { uptime_ms } => {
                assert!((250..600_000).contains(&uptime_ms), "{uptime_ms}")
            }
            other => panic!("{other:?}"),
        }

        let (reply, effect) = apply_ctl(r#"{"cmd":"metrics"}"#, &mut cx);
        assert_eq!(effect, CtlEffect::Nothing);
        match reply {
            ctl::Reply::Metrics(m) => {
                assert!(m.uptime_ms >= 250, "{m:?}");
                assert_eq!(m.panes, [0; ctl::PANE_STATES.len()]);
                assert_eq!(m.held_inboxes, 2);
                assert_eq!(m.bus_depth, 2);
                assert_eq!(m.bus_last_seq, ask.seq);
                assert_eq!(m.open_decisions, 1);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            cx.audit.latest_seq(),
            0,
            "a probe is not an action: nothing was recorded"
        );
    }
}
