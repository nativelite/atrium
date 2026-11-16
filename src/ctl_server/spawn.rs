//! `ctl spawn`: a visible worker in a new window, or `--here` beside the caller.

use super::dispatch::CtlSession;
use super::panes::{ctl_candidates, effective_mode, is_claude_pane, pane_by_agent, role_holder};
use crate::*;
use atrium::ctl;

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

/// The worktree a ctl spawn runs in, as `(cwd, norms)`, or the error reply when
/// it cannot be created.
fn worker_worktree(
    sp: &atrium::ctl::SpawnReq,
) -> Result<(Option<String>, Option<String>), atrium::ctl::Reply> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    worktree_spawn_params(&cwd, sp.worktree.as_deref())
        .map_err(|e| atrium::ctl::reply_err(&format!("spawn failed: {e}")))
}

/// Mark a freshly spawned pane as `caller`'s worker and build the spawn reply.
/// The one place a ctl spawn stamps its pane, for both placements.
fn adopt_worker(
    pane: &mut Pane,
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    note: Option<&str>,
) -> atrium::ctl::Reply {
    pane.role = sp.role.clone();
    pane.parent = caller;
    pane.depth = new_depth;
    pane.worktree = sp.worktree.clone();
    // The one place the mark is set: a hold spawn's pane, owned by `caller`.
    pane.placeholder = sp.hold;
    pane.held_for = caller.filter(|_| sp.hold);
    atrium::ctl::reply_spawned(
        pane.agent_id,
        sp.role.as_deref(),
        pane.session_id.as_deref(),
        note,
    )
}

/// `ctl spawn` (default): a visible worker in a brand-new window.
pub(crate) fn spawn_worker_window(
    cx: &mut CtlSession<'_>,
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
) -> atrium::ctl::Reply {
    let (rows, cols) = (cx.rows, cx.cols);
    let windows = &mut *cx.windows;
    let (wt_cwd, wt_norms) = match worker_worktree(sp) {
        Ok(pair) => pair,
        Err(reply) => return reply,
    };
    let mut flash = None;
    match spawn_window(
        PaneSpec {
            command: &sp.argv,
            id: 0,
            identity: sp.identity.as_deref(),
            cwd: wt_cwd.as_deref(),
            mode,
            extra_env: &[],
            extra_norms: wt_norms.as_deref(),
            deny: &[],
            codex: None,
        },
        rows,
        cols,
        &mut flash,
    ) {
        Ok(mut w) => {
            let reply = adopt_worker(&mut w.panes[0], sp, caller, new_depth, note);
            windows.push(w);
            // Size the new window's pane to its true rect now, so the agent
            // paints full-height immediately instead of at the rough spawn size
            // (which otherwise needs a manual terminal resize to correct).
            let last = windows.len() - 1;
            resize_window(&mut windows[last], rows, cols);
            reply
        }
        Err(e) => atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// `ctl spawn --here`: tile the worker *beside* the caller, in the caller's own
/// window, so a lead and its ICs sit in one view. Falls back to an error if the
/// caller's pane can't be located (nothing to sit beside). `--beside <pane>`
/// anchors on that pane instead when it is in the caller's window (the tmux
/// shim's `split-window -t %N`); otherwise the caller, and the reply says so.
pub(crate) fn spawn_worker_here(
    cx: &mut CtlSession<'_>,
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    new_depth: usize,
    mode: atrium::ctl::TrustMode,
    note: Option<&str>,
) -> atrium::ctl::Reply {
    let (rows, cols) = (cx.rows, cx.cols);
    let windows = &mut *cx.windows;
    let Some(caller_id) = caller else {
        return atrium::ctl::reply_err(
            "`--here` needs a caller pane; run it from inside an atrium pane",
        );
    };
    // Locate the window holding the caller.
    let Some(wi) = windows
        .iter()
        .position(|w| w.panes.iter().any(|p| p.agent_id == caller_id))
    else {
        return atrium::ctl::reply_err("`--here`: caller pane not found (rerun without --here)");
    };

    let w = &mut windows[wi];
    // The pane the worker lands next to: `--beside <pane>` when that pane is in
    // the caller's window, else the caller itself. An anchor in another window
    // would not be a split of this one, so it falls back and the reply says so
    // rather than placing the worker somewhere the request did not name.
    let mut beside_note: Option<String> = None;
    let anchor_agent = match sp.beside {
        Some(b) if w.panes.iter().any(|p| p.agent_id == AgentId(b)) => AgentId(b),
        Some(b) => {
            beside_note = Some(format!(
                "--beside {b}: not a pane in the caller's window; placed beside the caller"
            ));
            caller_id
        }
        None => caller_id,
    };
    let note = match (note, beside_note.as_deref()) {
        (Some(n), Some(b)) => Some(format!("{n}; {b}")),
        (None, Some(b)) => Some(b.to_string()),
        (n, None) => n.map(str::to_string),
    };
    let note = note.as_deref();
    let new_id = w.next_id;
    // Rough half-cell inner size; the caller resizes the window right after.
    let (pr, pc) = (
        (rows.saturating_sub(1).max(1) / 2).saturating_sub(2),
        (cols / 2).saturating_sub(2),
    );
    let (wt_cwd, wt_norms) = match worker_worktree(sp) {
        Ok(pair) => pair,
        Err(reply) => return reply,
    };
    let mut flash = None;
    // A hold spawn's pane is a placeholder: atrium's own waiting program,
    // not the request's command (it has none). Everything after this, the
    // adoption, the anchor, the share, the focus, is the worker's path.
    let spawned = if sp.hold {
        spawn_hold_pane(new_id, pr.max(1), pc.max(1))
    } else {
        spawn_pane_full(
            PaneSpec {
                command: &sp.argv,
                id: new_id,
                identity: sp.identity.as_deref(),
                cwd: wt_cwd.as_deref(),
                mode,
                extra_env: &[],
                extra_norms: wt_norms.as_deref(),
                deny: &[],
                codex: None,
            },
            pr.max(1),
            pc.max(1),
            &mut flash,
        )
    };
    match spawned {
        Ok(mut pane) => {
            let reply = adopt_worker(&mut pane, sp, caller, new_depth, note);
            // Re-tile the window into a balanced near-square grid over all its
            // panes (keeping their stable ids) with the worker BESIDE the anchor
            // (the caller, or `--beside`'s pane), rather than splitting the
            // caller alone or appending bottom-right.
            // A plain `split_pane` each time stacks every `--here` worker into
            // one column, so N of them degrade to an unusable `1×N` strip;
            // re-gridding keeps 2→1×2, 4→2×2, 6→2×3, 12→3×4, … balanced, in the
            // window's own orientation (rows, for a window `--here` started).
            // With `--size` (the shim's `-l 70%`) the anchor alone is split and
            // the worker takes that share of it; the ratio stays on the tree,
            // and later plain spawns re-grid only the region around their
            // anchor (`Tree::grow`), so a lead sized to 30 % keeps it while its
            // teammates tile inside the other 70 %.
            // Focus lands on the fresh worker, unless the caller said
            // `--no-focus` (the tmux shim's `-d`: a Claude Code lead spawning a
            // teammate must not move its human into it).
            let anchor = w
                .panes
                .iter()
                .find(|p| p.agent_id == anchor_agent)
                .map(|p| p.id)
                .unwrap_or(w.tree.focus());
            // A cell count is against the anchor's current width: `--here`
            // splits side by side, so the span is its columns.
            let share = sp.size.map(|sz| {
                let span = w
                    .tree
                    .rects(tiled_outer(rows, cols))
                    .into_iter()
                    .find(|(id, _)| *id == anchor)
                    .map(|(_, r)| r.cols)
                    .unwrap_or(cols as usize);
                sz.permille(span)
            });
            w.panes.push(pane);
            w.next_id += 1;
            w.tree = w.tree.grow(
                &mut w.grid_dir,
                anchor,
                new_id,
                layout::Dir::Vertical,
                share,
            );
            if sp.focus {
                w.tree.focus_pane(new_id);
            }
            w.zoomed = false;
            // Resize the whole window so both the caller and the fresh worker get
            // their exact inner rects — without this the worker keeps its rough
            // half-size and paints short (blank below), fixed only by a manual
            // terminal resize. Mirrors what the interactive split handlers do.
            resize_window(&mut windows[wi], rows, cols);
            reply
        }
        Err(e) => atrium::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// The file a ctl spawn's bare command names on `PATH`, as an absolute path:
/// what a spawn runs in place of the name, on every platform.
///
/// A name left for the system to find is not found on `PATH` alone. On
/// Windows a name atrium's own lookup misses is handed over as it is, and
/// `CreateProcess` then searches atrium's working directory, and `cmd.exe`
/// (which runs a batch shim) the pane's. Elsewhere the child resolves the
/// name itself, after it has changed into the pane's directory, so a relative
/// `PATH` entry (`./node_modules/.bin`, `.`, an empty one) names a folder
/// inside the project or the worktree a spawn asked for: a pane held to
/// `accept` could plant `node_modules/.bin/codex` there and have a spawn run
/// it with no posture. So the lookup is done here, over the **absolute**
/// directories of `PATH` only, and a name it does not find is refused. On
/// Windows it tries the system's extensions (`PATHEXT`); elsewhere the name
/// as it is, which must be a regular file with an execute bit.
///
/// An absolute entry is searched wherever it points, a folder inside the
/// project included (`/work/app/node_modules/.bin`): `PATH` is trusted as the
/// human set it.
///
/// `Ok(None)` for a command that already is a path: the only one that reaches
/// here is the caller's own recorded command (a spawn with none given), which
/// a fleet file or the launch approved as written. Pure over its inputs and
/// the file system.
pub(crate) fn pin_to_path(
    command: &str,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&str>,
) -> Result<Option<String>, String> {
    if command.contains(['/', '\\']) {
        return Ok(None);
    }
    let dirs: Vec<std::path::PathBuf> = path
        .map(|p| {
            std::env::split_paths(p)
                .filter(|d| d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let found = if cfg!(windows) {
        let exts: Vec<String> = pathext
            .unwrap_or(".COM;.EXE;.BAT;.CMD")
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .collect();
        atrium::resolve::resolve(command, &dirs, &exts)
    } else {
        dirs.iter()
            .map(|d| d.join(command))
            .find(|f| is_executable_file(f))
    };
    match found {
        Some(found) => Ok(Some(found.to_string_lossy().into_owned())),
        None => Err(format!(
            "spawn refused: {command:?} was not found in any directory of PATH (relative and \
             empty entries, which mean the current directory, are not searched). A ctl spawn \
             runs the file its command names on PATH and nothing else; install it, or put \
             its directory on PATH, as an absolute path, before launching atrium"
        )),
    }
}

/// A regular file the current user could execute (any execute bit).
#[cfg(unix)]
fn is_executable_file(file: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Off unix the extension decides what runs; this branch is not taken there.
#[cfg(not(unix))]
fn is_executable_file(file: &std::path::Path) -> bool {
    file.is_file()
}

/// What the placeholder guard knows of the calling pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HoldCaller {
    /// Its recorded command is claude ([`is_claude_pane`]).
    pub(crate) claude: bool,
    pub(crate) can_spawn: bool,
    pub(crate) depth: usize,
}

/// The refusal for a hold spawn from anything but a claude agent's pane.
pub(crate) const HOLD_NEEDS_CLAUDE: &str =
    "hold refused: a teammate placeholder is made by a claude pane's own Claude Code, \
     through atrium's tmux shim (`split-window … -- cat`), and by nothing else: not by an \
     operator, a shell, another agent CLI or a placeholder";

/// May this caller have a teammate placeholder, and at what depth? The guard
/// for `ctl spawn` with `hold`. Pure.
///
/// A placeholder runs atrium's own waiting program, so there is no command
/// to vet; what is left of a spawn's guards is who asks and how many panes
/// there are:
///
/// * the caller is a **claude agent's pane** (the tmux shim's only user), in
///   a session that puts the shim into claude panes (`teams` not `off`);
/// * it **may create teammates** (`can_spawn`), as for any spawn;
/// * the **depth cap** and the **pane cap** hold, as for any spawn: the
///   teammate will stand exactly where the placeholder does.
pub(crate) fn hold_allowed(
    caller: Option<HoldCaller>,
    teams_on: bool,
    max_depth: usize,
    live: usize,
    cap: usize,
) -> Result<usize, String> {
    let Some(caller) = caller.filter(|c| c.claude) else {
        return Err(HOLD_NEEDS_CLAUDE.to_string());
    };
    if !teams_on {
        return Err(
            "hold refused: this session keeps Claude Code's teammates in-process (teams: off)"
                .to_string(),
        );
    }
    if !caller.can_spawn {
        return Err("this agent is not permitted to create teammates \
             (set \"can_spawn\": true for it in the fleet file)"
            .to_string());
    }
    let attempted = caller.depth + 1;
    if attempted > max_depth {
        return Err(ctl::SpawnDenied::DepthExceeded {
            attempted,
            max: max_depth,
        }
        .message());
    }
    if live >= cap {
        return Err(format!(
            "pane cap reached ({live}/{cap}) — reap an agent or raise it with ATRIUM_MAX_PANES"
        ));
    }
    Ok(attempted)
}

/// `ctl spawn` with `hold`: a teammate placeholder beside the caller, placed
/// exactly as a `--here` worker is (anchor, share, focus), running atrium's
/// own waiting program ([`spawn_hold_pane`]).
fn spawn_hold(
    sp: &atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    cx: &mut CtlSession<'_>,
) -> atrium::ctl::Reply {
    let facts = caller
        .and_then(|c| pane_by_agent(cx.windows, c))
        .map(|p| HoldCaller {
            claude: is_claude_pane(p),
            can_spawn: p.can_spawn,
            depth: p.depth,
        });
    let live = cx.windows.iter().map(|w| w.panes.len()).sum::<usize>();
    let new_depth = match hold_allowed(
        facts,
        atrium::tmux_shim::session_enabled(),
        cx.max_depth,
        live,
        atrium::resources::effective_cap(),
    ) {
        Ok(depth) => depth,
        Err(why) => return ctl::reply_err(&why),
    };
    // No posture: the pane is not an agent's (`Off` adds nothing to a
    // command that is not one).
    spawn_worker_here(cx, sp, caller, new_depth, ctl::TrustMode::Off, None)
}

/// `ctl spawn`: the guards, in order — may this caller create teammates, is the
/// role free, is the argv one a teammate may run, the effective trust mode, the
/// depth cap, the pane cap, and credential delegation — then a new window, or
/// `--here` beside the caller.
pub(crate) fn spawn(
    mut sp: atrium::ctl::SpawnReq,
    caller: Option<AgentId>,
    privileged: bool,
    cx: &mut CtlSession<'_>,
) -> atrium::ctl::Reply {
    // A placeholder is its own request with its own guard: it names no
    // command, so none of the command's checks below has anything to check.
    if sp.hold {
        return spawn_hold(&sp, caller, cx);
    }
    let windows = &mut *cx.windows;
    let max_depth = cx.max_depth;
    let (extra_allow, session_identity) = (cx.extra_allow, cx.session_identity);
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
    // No command: the caller's own kind. A codex lead gets codex teammates
    // and a claude lead claude ones unless the spawn names the other after
    // `--`; from a pane that is no agent (a shell, the operator), claude.
    //
    // The command a request names is a bare name, for every caller. The
    // allowlist below looks at the file stem alone and a command with a
    // separator is run as given, so `\\host\share\claude.exe` or
    // `./x/claude.sh` would pass as `claude` and start a program of the
    // caller's choosing, which honours none of the posture flags atrium
    // appends: a way past the session's trust ceiling for any pane holding a
    // token. Checked on what the request carries, before the caller's own kind
    // is filled in below: that one is the command atrium itself recorded for
    // the calling pane (a fleet file's, the launch's, or an earlier spawn that
    // passed this check), never a request's.
    if sp
        .argv
        .first()
        .is_some_and(|program| !ctl::is_ctl_command(program))
    {
        return ctl::reply_err(ctl::SPAWN_NEEDS_BARE_COMMAND);
    }
    // A positional argument of an agent's command is its first prompt, and
    // atrium cannot put its header in front of that one: an argument that
    // starts with a character the agent CLI reads specially is refused
    // ([`ctl::vet_spawn_arg`]), on every argument. (An operator's argv was
    // refused for this at any trust, by its vetting.)
    if let Err(why) = ctl::vet_spawn_args(&sp.argv, ctl::TextFrom::Pane, trust_mode()) {
        return ctl::reply_err(why);
    }
    let mut notes: Vec<String> = Vec::new();
    if sp.argv.is_empty() {
        let own = caller
            .and_then(|cid| pane_by_agent(windows, cid))
            .filter(|p| atrium::bind::is_agent_stem(&p.title))
            .and_then(|p| p.argv.first().cloned())
            .unwrap_or_else(|| "claude".to_string());
        notes.push(format!(
            "no command given: spawned {own}, the caller's own kind"
        ));
        sp.argv = vec![own];
    }
    let stripped = match ctl::vet_spawn_argv(&sp.argv) {
        ctl::ArgvVerdict::Refused(why) => return ctl::reply_err(&why),
        ctl::ArgvVerdict::Ok { argv, stripped } => {
            sp.argv = argv;
            stripped
        }
    };
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
    let new_depth = match ctl::evaluate_spawn(&sp.argv, caller_depth, max_depth, extra_allow) {
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
    // The command is pinned to the file it names on PATH, and that file is
    // what is spawned and recorded; see [`pin_to_path`].
    {
        let path = std::env::var_os("PATH");
        let pathext = std::env::var("PATHEXT").ok();
        match pin_to_path(&sp.argv[0], path.as_deref(), pathext.as_deref()) {
            Ok(Some(pinned)) => sp.argv[0] = pinned,
            Ok(None) => {}
            Err(why) => return ctl::reply_err(&why),
        }
    }
    if sp.new_window {
        spawn_worker_window(cx, &sp, caller, new_depth, effective, note)
    } else {
        spawn_worker_here(cx, &sp, caller, new_depth, effective, note)
    }
}

#[cfg(test)]
mod tests;
