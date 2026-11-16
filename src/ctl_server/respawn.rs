//! `ctl respawn`: restart a pane's process in place, as a new session.

use super::dispatch::{scope_denied, CtlSession};
use super::panes::{
    ctl_candidates, effective_mode, is_claude_pane, pane_by_agent, pane_by_agent_mut, role_holder,
};
use super::sends::{repoint_parents, retarget_sends};
use super::spawn::{pin_to_path, worktree_spawn_params};
use crate::*;
use atrium::ctl;

/// `ctl respawn`: replace a target's process with a fresh one in the same tile —
/// same command (as a new session), posture, deny rules, context and place, or a
/// named worktree — keeping its role, parent and depth. Its children and queued
/// sends follow it to its new agent id.
pub(crate) fn respawn(
    rr: ctl::RespawnReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    // `--teammate`: a different request under the same verb (the tmux shim's
    // `respawn-pane`), with its own guard. It never falls through to the
    // plain respawn below, and a plain respawn never takes a placeholder.
    if let Some(line) = rr.teammate.as_deref() {
        let teams_env = atrium::tmux_shim::session_teams_env();
        return teammate(&rr.target, line, caller, trust_mode(), &teams_env, cx);
    }
    let (windows, pending) = (&mut *cx.windows, &mut *cx.pending);
    let (rows, cols) = (cx.rows, cx.cols);
    let candidates = ctl_candidates(windows);
    let id = match ctl::resolve_target(&rr.target, &candidates) {
        Ok(id) => id,
        Err(e) => return ctl::reply_err(&e),
    };
    if let Some(deny) = scope_denied(windows, caller, privileged, id) {
        return deny;
    }
    // A placeholder has no command to replay but atrium's own waiting
    // program, and a respawn would hand that a token and a ctl environment.
    // It is started once, by its lead's `respawn-pane`, or killed.
    if pane_by_agent(windows, id).is_some_and(|p| p.placeholder) {
        return ctl::reply_err(&format!(
            "pane {id} is a teammate placeholder: it is started by its lead's Claude Code \
             (`respawn-pane`, through atrium's tmux shim) or killed, not respawned"
        ));
    }
    // The new process's mod will say hello again; nothing the old one
    // reported (its status, its answer) belongs to the relaunch.
    cx.mods.forget(id.0);
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
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
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
            codex: None,
        },
        pane_rows,
        pane_cols,
        &mut flash,
    ) {
        Ok(p) => p,
        Err(e) => return ctl::reply_err(&format!("respawn failed: {e}")),
    };
    // In-place replacement: kill the old child then swap in the new pty,
    // term, and session fields. Role/parent/depth/can_spawn are preserved.
    let p = pane_by_agent_mut(windows, id).expect("found above, and nothing has removed it since");
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
    p.paste_wanted = false;
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

// ---- a teammate into its placeholder -----------------------------------------

/// What the teammate guard knows of the pane a `respawn-pane` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) placeholder: bool,
    pub(crate) parent: Option<AgentId>,
    pub(crate) exited: bool,
}

/// Is `slot` a placeholder `lead` may start a teammate in: a live pane that
/// still carries the placeholder mark and whose parent is `lead` itself? The
/// whole rule for which pane a `respawn-pane` may replace. Pure.
///
/// `false` for a pane that does not exist, for any pane that runs a command
/// (an agent, a shell, a teammate already started: the mark is cleared when
/// one starts, so a second respawn finds none), for a placeholder another
/// pane made (a sibling's, a child's own, the lead's lead's), and for one
/// that is being reaped. "No ctl request changes a pane's recorded command"
/// holds for every pane this answers `false` for.
pub(crate) fn is_own_placeholder(lead: AgentId, slot: Option<Slot>) -> bool {
    slot.is_some_and(|s| s.placeholder && !s.exited && s.parent == Some(lead))
}

/// The posture a teammate starts at: the **lower** of what its command asked
/// for and its lead pane's own mode, then capped at the session's launch
/// trust by [`effective_mode`] like every spawn. Returns the mode and what
/// was held back, for the reply. Pure.
///
/// A command that asked for nothing (no permission flag, or `default` /
/// `dontAsk`) takes the lead pane's mode. It can never come out above the
/// lead pane's mode or above the session's.
pub(crate) fn teammate_mode(
    asked: atrium::tmux_shim::Asked,
    lead: ctl::TrustMode,
    policy: ctl::TrustMode,
) -> (ctl::TrustMode, Option<String>) {
    use atrium::tmux_shim::Asked;
    let (wanted, held) = match asked {
        Asked::LeadsOwn => (lead, None),
        Asked::Mode(m) if m.rank() <= lead.rank() => (m, None),
        Asked::Mode(m) => (
            lead,
            Some(format!(
                "asked for {}; held to its lead pane's {}",
                m.policy_label(),
                lead.policy_label()
            )),
        ),
    };
    let (mode, capped) = effective_mode(Some(wanted), policy);
    let note = match (held, capped) {
        (Some(h), Some(c)) => Some(format!("{h}; {c}")),
        (h, c) => h.or(c),
    };
    (mode, note)
}

/// The identity a teammate runs under: its **lead pane's own**, by name, or
/// none when the lead has none. Never a field of the request: a teammate
/// command names no identity, so this cannot be used to obtain one the lead
/// does not hold. Checked against the delegation rule all the same (a pane
/// may pass down its own identity, [`ctl::delegation_allowed`]), so the rule
/// is the one every spawn is held to. Pure.
pub(crate) fn teammate_identity(
    lead: Option<&str>,
    session: Option<&str>,
) -> Result<Option<String>, String> {
    ctl::delegation_allowed(lead, lead, session, false)?;
    Ok(lead.map(str::to_string))
}

/// The refusal for a `respawn --teammate` that does not come from a claude
/// agent's own pane.
pub(crate) const TEAMMATE_NEEDS_LEAD: &str =
    "teammate refused: a teammate is started by the claude pane that made its placeholder, \
     through atrium's tmux shim (`respawn-pane`), and by nothing else: not by an operator, \
     a shell, another agent CLI or a placeholder";

/// The refusal for a target that is not the caller's own placeholder. One
/// text for every such pane, so the answer says nothing about panes outside
/// the caller's reach.
fn not_own_placeholder(target: &str) -> String {
    format!(
        "teammate refused: pane {} is not a placeholder this pane made. `respawn-pane` starts \
         a teammate only in a placeholder the calling pane itself made with `split-window … \
         -- cat`, and only once; it never replaces what a pane already runs",
        atrium::audit::field(target)
    )
}

/// `ctl respawn <pane> --teammate <command line>`: turn the caller's own
/// placeholder into a Claude Code teammate, once. What the tmux shim sends
/// for Claude Code 2.1.325's `respawn-pane -k -t %N -- <command line>`.
///
/// The line is the caller's text and is **never run**: it is parsed
/// ([`atrium::tmux_shim::parse_teammate_command`]), vetted against a closed
/// table ([`atrium::tmux_shim::vet_teammate`]), and the teammate's command is
/// built here:
///
/// * **program**: the calling pane's own recorded command (pinned to `PATH`
///   like a spawn's "caller's own kind"), not the path in the line;
/// * **flags**: the ones the table passes on; the line's permission flags
///   are a request, resolved by [`teammate_mode`];
/// * **environment**: what atrium gave the lead pane, from atrium's own
///   record of it (its identity, re-resolved by name, and its fleet
///   context variables), then the three team variables, then the line's
///   assignments whose names are on the session's `teams_env`. Every other
///   assignment is dropped, and the ones whose value differs from what the
///   teammate has anyway are named ([`atrium::tmux_shim::env_not_passed`]);
/// * **directory**: the line's, when it is the lead's own or inside the
///   project ([`atrium::tmux_shim::teammate_cwd`]).
///
/// The pane keeps its place in the layout and its **agent id** (Claude Code
/// goes on naming it by the `%N` its split printed), becomes an agent's pane
/// with the role `--agent-name` gave, and is a ctl-spawned claude pane in
/// every other respect: a token, the ctl environment, the shim's `TMUX`,
/// `--session-id`, atrium's status-hooks `--settings`, the deny list. None of
/// those is in a teammate command (its `--settings` is dropped), so they do
/// not conflict with it.
pub(crate) fn teammate(
    target: &str,
    line: &str,
    caller: Option<AgentId>,
    policy: ctl::TrustMode,
    teams_env: &[String],
    cx: &mut CtlSession<'_>,
) -> ctl::Reply {
    use atrium::tmux_shim as shim;
    let windows = &mut *cx.windows;
    let (rows, cols) = (cx.rows, cx.cols);
    // Who asks: a claude agent's own pane, which may create teammates.
    let Some(lead_id) = caller else {
        return ctl::reply_err(TEAMMATE_NEEDS_LEAD);
    };
    let Some(lead) = pane_by_agent(windows, lead_id).filter(|p| is_claude_pane(p)) else {
        return ctl::reply_err(TEAMMATE_NEEDS_LEAD);
    };
    if !shim::session_enabled() {
        return ctl::reply_err(
            "teammate refused: this session keeps Claude Code's teammates in-process \
             (teams: off)",
        );
    }
    if !lead.can_spawn {
        return ctl::reply_err(
            "this agent is not permitted to create teammates \
             (set \"can_spawn\": true for it in the fleet file)",
        );
    }
    let lead_depth = lead.depth;
    let lead_mode = lead.mode;
    let lead_place = (lead.cwd.clone(), lead.norms.clone(), lead.worktree.clone());
    let lead_deny = lead.deny.clone();
    // What atrium gave the lead, from atrium's record of the lead pane:
    // the NAME of its identity (the credentials are resolved again at
    // the spawn, never kept) and its fleet context variables.
    let lead_env = lead.context_env.clone();
    let identity = match teammate_identity(lead.identity.as_deref(), cx.session_identity) {
        Ok(identity) => identity,
        Err(why) => return ctl::reply_err(&why),
    };
    let Some(own) = lead.argv.first().cloned() else {
        return ctl::reply_err(TEAMMATE_NEEDS_LEAD);
    };
    // Which pane: the caller's own live placeholder, by its id. Anything
    // else is refused before the line is even read.
    let id = target.parse::<usize>().ok().map(AgentId);
    let slot = id.and_then(|id| pane_by_agent(windows, id)).map(|p| Slot {
        placeholder: p.placeholder,
        parent: p.parent,
        exited: p.exited,
    });
    let Some(id) = id.filter(|_| is_own_placeholder(lead_id, slot)) else {
        return ctl::reply_err(&not_own_placeholder(target));
    };
    // The line: one grammar, then the closed table. Never a shell.
    let parsed = match shim::parse_teammate_command(line) {
        Ok(parsed) => parsed,
        Err(why) => {
            return ctl::reply_err(&format!(
                "teammate refused: the command line is not the one form atrium reads ({why}). \
                 atrium takes it apart itself and never hands it to a shell. Nothing was started"
            ))
        }
    };
    let mate = match shim::vet_teammate(&parsed, teams_env) {
        Ok(mate) => mate,
        Err(why) => return ctl::reply_err(&why),
    };
    // The role is the teammate's name: reserved names and a name a live pane
    // already wears are refused, as for any spawn.
    if atrium::operator::role_reserved(&mate.role) {
        return ctl::reply_err(
            "teammate refused: its name is reserved as a role: \"operator\", \
             \"operator:<name>\" and \"atrium\" (and anything that reads like them) name \
             callers that are not panes. Nothing was started",
        );
    }
    if let Some(holder) = role_holder(&mate.role, &ctl_candidates(windows)) {
        return ctl::reply_err(&format!(
            "teammate refused: its name {:?} already names pane {holder} as a role; roles are \
             unique while a pane lives. Nothing was started",
            mate.role
        ));
    }
    // The program: the lead's own recorded command, allowlisted and
    // depth-checked like any spawn's, and pinned to the file it names on
    // PATH. (A recorded path, a fleet file's or the launch's, is left as the
    // human wrote it.)
    if let Err(denied) = ctl::evaluate_spawn(
        std::slice::from_ref(&own),
        lead_depth,
        cx.max_depth,
        cx.extra_allow,
    ) {
        return ctl::reply_err(&denied.message());
    }
    let program = {
        let path = std::env::var_os("PATH");
        let pathext = std::env::var("PATHEXT").ok();
        match pin_to_path(&own, path.as_deref(), pathext.as_deref()) {
            Ok(Some(pinned)) => pinned,
            Ok(None) => own,
            Err(why) => return ctl::reply_err(&why),
        }
    };
    // Where: the line's directory, if it is the lead's own or in the project.
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let lead_dir = lead_place
        .0
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.clone());
    let real = match shim::teammate_cwd(&parsed.cwd, &lead_dir, &root) {
        Ok(real) => real,
        Err(why) => return ctl::reply_err(&why),
    };
    // In the lead's own directory the teammate is where the lead is, under
    // the same worktree instructions; anywhere else in the project it is a
    // plain pane there.
    let (cwd, norms, worktree) = if std::fs::canonicalize(&lead_dir).is_ok_and(|l| l == real) {
        lead_place
    } else {
        (Some(shim::plain_path(&real)), None, None)
    };
    let (mode, mode_note) = teammate_mode(mate.asked, lead_mode, policy);
    let mut command = vec![program];
    command.extend(mate.args.iter().cloned());
    let mut deny = lead_deny;
    for rule in &mate.deny {
        if !deny.contains(rule) {
            deny.push(rule.clone());
        }
    }
    let Some(wi) = windows
        .iter()
        .position(|w| w.panes.iter().any(|p| p.agent_id == id))
    else {
        return ctl::reply_err(&not_own_placeholder(target));
    };
    let slot_id = pane_by_agent(windows, id)
        .map(|p| p.id)
        .expect("the placeholder was found in this window");
    let (pane_rows, pane_cols) =
        pane_inner_size(windows[wi].tiled(), &windows[wi].tree, slot_id, rows, cols);
    // The teammate's own environment: the lead's context variables, then
    // what the vetting applied (the team variables and the allowlisted).
    let mut extra_env = lead_env.clone();
    extra_env.extend(mate.env.iter().cloned());
    // Which of the line's other assignments would have changed anything:
    // each is compared with the value the teammate will have. The
    // identity's values are secrets; they live in this block and are
    // only compared.
    let (not_passed, differing) = {
        let given = identity.as_deref().map(identity_values).unwrap_or_default();
        shim::env_not_passed(&mate.unapplied, |name| {
            let same = |k: &String| {
                if cfg!(windows) {
                    k.eq_ignore_ascii_case(name)
                } else {
                    k == name
                }
            };
            given
                .iter()
                .rev()
                .chain(extra_env.iter().rev())
                .find(|(k, _)| same(k))
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var(name).ok())
        })
    };
    let mut flash = None;
    let new_pane = match spawn_pane_in_slot(
        PaneSpec {
            command: &command,
            id: slot_id,
            // The lead pane's own identity, by name.
            identity: identity.as_deref(),
            cwd: cwd.as_deref(),
            mode,
            extra_env: &extra_env,
            extra_norms: norms.as_deref(),
            deny: &deny,
            codex: None,
        },
        id,
        pane_rows,
        pane_cols,
        &mut flash,
    ) {
        Ok(p) => p,
        Err(e) => {
            return ctl::reply_err(&format!("teammate refused: it could not be started: {e}"))
        }
    };
    // Nothing a mod said of this id belongs to the teammate (a placeholder
    // has no token, so nothing should be there; cleared all the same).
    cx.mods.forget(id.0);
    let session = new_pane.session_id.clone();
    let p = pane_by_agent_mut(windows, id).expect("found above, and nothing has removed it since");
    let _ = p.pty.kill();
    // The old reader goes with the old child, before its `Pty` drops.
    p.inbox = None;
    p.pty = new_pane.pty;
    p.term = new_pane.term;
    p.filter = new_pane.filter;
    p.title = new_pane.title;
    p.exited = false;
    p.session_id = new_pane.session_id;
    p.launch_ms = new_pane.launch_ms;
    p.cwd = new_pane.cwd;
    p.identity = new_pane.identity;
    p.token = new_pane.token;
    p.argv = new_pane.argv;
    p.deny = new_pane.deny;
    p.mode = new_pane.mode;
    p.norms = new_pane.norms;
    p.context_env = new_pane.context_env;
    p.worktree = worktree;
    p.kickoff = false;
    p.can_spawn = new_pane.can_spawn;
    p.role = Some(mate.role.clone());
    p.parent = Some(lead_id);
    p.depth = lead_depth + 1;
    p.activity = false;
    p.last_activity = std::time::Instant::now();
    p.draft = atrium::deliver::Draft::default();
    p.painted = false;
    p.mouse_wanted = false;
    p.paste_wanted = false;
    // The mark goes last and for good: the pane now runs a command, and no
    // request changes that again.
    p.placeholder = false;
    debug_assert_eq!(
        p.agent_id, new_pane.agent_id,
        "the teammate keeps the slot's id"
    );
    resize_window(&mut windows[wi], rows, cols);
    // What atrium decided, for the record: counts and flag names, no value.
    let mut notes: Vec<String> = Vec::new();
    // One line for the variables that were dropped and would have
    // differed: on the bar, in the reply and in the audit row. Names only.
    let not_passed = shim::not_passed_note(&mate.role, &not_passed, differing);
    if let Some(line) = &not_passed {
        notes.push(line.clone());
        *cx.flash = Some((line.clone(), Instant::now()));
    } else if flash.is_some() {
        // What the spawn itself had to say (an identity that did not resolve).
        *cx.flash = flash;
    }
    if !mate.dropped.is_empty() {
        notes.push(format!("not passed on: {}", mate.dropped.join(", ")));
    }
    notes.extend(mode_note);
    cx.audit.record(
        Some(lead_id),
        "teammate",
        &format!(
            "pane={id} role={} mode={} env_dropped={} flags_dropped={}",
            atrium::audit::field(&mate.role),
            mode.policy_label(),
            mate.env_dropped,
            if mate.dropped.is_empty() {
                "-".to_string()
            } else {
                mate.dropped.join(",")
            }
        ),
        true,
        not_passed.as_deref().unwrap_or(""),
    );
    let note = (!notes.is_empty()).then(|| notes.join("; "));
    ctl::reply_spawned(id, Some(&mate.role), session.as_deref(), note.as_deref())
}

#[cfg(test)]
mod tests;
