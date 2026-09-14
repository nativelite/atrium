//! Pane and window spawning: build an agent's launch (session-id inject, trust
//! flags, identity env, Windows shim resolution) and start it on a pty. Moved out
//! of `main.rs` verbatim (r10 audit B11).

use crate::*;

pub(crate) fn spawn_window(
    command: &[String],
    rows: u16,
    cols: u16,
    _idx: usize,
    identity: Option<&str>,
    mode: atrium::ctl::TrustMode,
    flash: &mut Option<(String, Instant)>,
    job: Option<&atrium::reap::SessionJob>,
    cwd: Option<&str>,
    extra_norms: Option<&str>,
) -> std::io::Result<Window> {
    let pane = spawn_pane_full(
        PaneSpec {
            command,
            id: 0,
            identity,
            cwd,
            mode,
            extra_env: &[],
            extra_norms,
        },
        rows.saturating_sub(1).max(1),
        cols,
        flash,
    )?;
    // Enroll a RUNTIME-spawned pane in the session Job synchronously, before it is
    // returned, so a pane spawned mid-loop is torn down with atrium even if atrium
    // is killed the same instant (#94). `job` is `None` for pre-run-loop/startup
    // panes, which the run loop's lazy pass enrolls as before (status quo).
    // Idempotent + a no-op on unix + graceful on failure (never blocks the pane).
    if let Some(j) = job {
        j.assign(pane.pty.pid());
    }
    Ok(Window {
        panes: vec![pane],
        tree: Tree::new(0),
        zoomed: false,
        next_id: 1,
    })
}

/// Open `command` as a new window mid-run — enrolled in the session job — and
/// switch to it. A spawn failure is returned for the caller to word.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_window(
    windows: &mut Vec<Window>,
    active: &mut usize,
    command: &[String],
    identity: Option<&str>,
    mode: atrium::ctl::TrustMode,
    rows: u16,
    cols: u16,
    out: &mut impl std::io::Write,
    flash: &mut Option<(String, Instant)>,
    job: &atrium::reap::SessionJob,
) -> std::io::Result<()> {
    let w = spawn_window(
        command,
        rows,
        cols,
        windows.len(),
        identity,
        mode,
        flash,
        Some(job),
        None,
        None,
    )?;
    windows.push(w);
    let last = windows.len() - 1;
    switch_window(windows, active, last, rows, cols, out);
    Ok(())
}

/// Mass-spawn a single window of N tiles laid out as a balanced `grid`. The
/// split tree is built with [`layout::Tree::grid`] (ids `0..N`, focus 0) so
/// focus/rects/close all keep working exactly as for a hand-split grid. Each
/// tile runs the SAME command, each its own session (a fresh `--session-id` per
/// pane via the existing `bind` path inside `spawn_pane`), all under the same
/// `identity` if one was given. Panes are spawned at a rough grid-cell size; the
/// caller resizes the window right after so each pty/emulator gets its exact
/// inner rect. If any pane fails to spawn, the whole window is abandoned and the
/// already-spawned panes are killed (nothing to host half a grid).
pub(crate) fn spawn_window_grid(
    command: &[String],
    rows: u16,
    cols: u16,
    grid: atrium::spawn::Grid,
    identity: Option<&str>,
    flash: &mut Option<(String, Instant)>,
    job: Option<&atrium::reap::SessionJob>,
) -> std::io::Result<Window> {
    let tree = Tree::grid(grid.rows, grid.cols);
    let n = grid.total();
    // Rough per-cell inner size (minus the one-cell border on each side); the
    // caller's resize_window fixes it exactly right after.
    let cell_rows = ((rows.saturating_sub(1).max(1) as usize / grid.rows.max(1)).saturating_sub(2))
        .max(1) as u16;
    let cell_cols = ((cols as usize / grid.cols.max(1)).saturating_sub(2)).max(1) as u16;
    let mut panes: Vec<Pane> = Vec::with_capacity(n);
    for id in 0..n {
        match spawn_pane(
            command,
            cell_rows,
            cell_cols,
            id,
            identity,
            trust_mode(),
            flash,
        ) {
            Ok(pane) => {
                // Runtime-grid panes enroll synchronously (see spawn_window); a
                // startup grid passes `None` and rides the lazy pass (status quo).
                if let Some(j) = job {
                    j.assign(pane.pty.pid());
                }
                panes.push(pane);
            }
            Err(e) => {
                // Tear down whatever we already started — a partial grid is not
                // a coherent window.
                for p in panes.iter_mut() {
                    let _ = p.pty.kill();
                }
                return Err(e);
            }
        }
    }
    Ok(Window {
        panes,
        tree,
        zoomed: false,
        next_id: n,
    })
}

pub(crate) fn spawn_pane(
    command: &[String],
    rows: u16,
    cols: u16,
    id: usize,
    identity: Option<&str>,
    mode: atrium::ctl::TrustMode,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Pane> {
    spawn_pane_full(
        PaneSpec {
            command,
            id,
            identity,
            cwd: None,
            mode,
            extra_env: &[],
            extra_norms: None,
        },
        rows,
        cols,
        flash,
    )
}

/// The non-secret environment every pane is born with.
///
/// Split out from [`spawn_pane_full`] so the one rule that matters here can be
/// asserted directly: **the session marker does not depend on ctl.** It used to,
/// and that was the first of the two holes that stranded 75 processes holding
/// 526 ptys against a machine-wide limit of 511. `ATRIUM_CTL`/`ATRIUM_PANE`/
/// `ATRIUM_TOKEN` were pushed inside `if let (Some(addr), Some(token)) = …`, so a
/// pane launched without `--allow-ctl` — the default — carried no atrium
/// environment whatsoever. Nothing on the process said it was ours, so once atrium
/// was gone nothing could find it, and they had to be cleared by hand.
///
/// `ATRIUM_SESSION` is deliberately NOT one of those credentials. `ATRIUM_TOKEN` is
/// a capability that must not leak; this is a label that is *meant* to be read —
/// by the watchdog, by `atrium reap`, by a human with `ps`. Holding it authorises
/// nothing, and forging it can only nominate the forger's own process group for
/// collection, and then only once the atrium it names is already dead.
pub(crate) fn pane_base_env(
    session: Option<&atrium::orphan::SessionKey>,
    ctl: Option<(&str, &str, &str)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(key) = session {
        env.push((atrium::orphan::ENV_SESSION.to_string(), key.encode()));
    }
    if let Some((addr, pane, token)) = ctl {
        env.push((atrium::ctl::ENV_ADDRESS.to_string(), addr.to_string()));
        env.push((atrium::ctl::ENV_PANE.to_string(), pane.to_string()));
        env.push((atrium::ctl::ENV_TOKEN.to_string(), token.to_string()));
    }
    env
}

/// Combine an optional worktree-norms block with an optional ctl directive into
/// a single `--append-system-prompt` payload.
///
/// Claude is last-wins on `--append-system-prompt`: two separate flags silently
/// drop the first. This pure helper merges both text blocks so callers can push
/// exactly one flag. Returns `None` when both inputs are absent (no flag needed).
pub(crate) fn combine_system_prompt(norms: Option<&str>, ctl: Option<&str>) -> Option<String> {
    match (norms, ctl) {
        (Some(n), Some(c)) => Some(format!("{n}\n\n{c}")),
        (Some(n), None) => Some(n.to_string()),
        (None, Some(c)) => Some(c.to_string()),
        (None, None) => None,
    }
}

/// What to launch in a new pane — everything [`spawn_pane_full`] needs besides
/// the terminal geometry and the flash slot, by name. It replaces an 11-argument
/// positional signature where three adjacent `Option<&str>`s (identity, cwd,
/// norms) could be swapped without a compiler complaint (r10 audit B11).
pub(crate) struct PaneSpec<'a> {
    /// The command and its args, with any per-agent args (the fleet loader's
    /// `--add-dir` / `--append-system-prompt` / `--model` / `--effort`) already
    /// appended; `--session-id` is added here when the pane is a bindable agent.
    pub(crate) command: &'a [String],
    /// The pane's split-tree id within its window.
    pub(crate) id: usize,
    /// The credential identity name to resolve and inject, if any.
    pub(crate) identity: Option<&'a str>,
    /// Working directory: `Some(dir)` for a fleet/worktree agent (so its
    /// `CLAUDE.md` auto-loads), `None` to inherit atrium's.
    pub(crate) cwd: Option<&'a str>,
    /// The effective trust posture for this pane.
    pub(crate) mode: atrium::ctl::TrustMode,
    /// Extra environment for this child only (e.g. context-sharing vars).
    pub(crate) extra_env: &'a [(String, String)],
    /// Worktree norms to fold into the pane's `--append-system-prompt`.
    pub(crate) extra_norms: Option<&'a str>,
}

/// The shared spawn core: build the agent's launch from `spec` (session-id
/// inject, trust flags, identity env, Windows shim resolution) and start it on a
/// `rows x cols` pty. Every pane atrium hosts — the initial one, a split, a grid
/// tile, and each fleet agent — is born here, so the identity / `--session-id` /
/// effective-command discipline is written once and shared. Non-fatal problems
/// (an unresolvable identity, stripped flags) are reported through `flash`.
pub(crate) fn spawn_pane_full(
    spec: PaneSpec,
    rows: u16,
    cols: u16,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Pane> {
    let PaneSpec {
        command,
        id,
        identity,
        cwd,
        mode,
        extra_env,
        extra_norms,
    } = spec;
    // Cross-platform stem: split on `/` and `\` on every OS so a Windows-authored
    // fleet command (e.g. `C:\tools\claude.cmd`) is recognized as an agent on
    // macOS/Linux too — `Path::file_stem` would keep the backslashes there. This
    // title feeds is_claude/is_codex/vendor_tag, so the fix must live here.
    let title = atrium::bind::command_stem(&command[0]);
    // Permission posture for an agent pane (never a shell pane):
    //   Edits (`--trust`)          → `--permission-mode acceptEdits` + a safe
    //                                dev-command allowlist; dangerous commands
    //                                still prompt, visibly, in the pane.
    //   Auto (`automode`)          → `--permission-mode auto` (claude's auto mode:
    //                                hands-off edits + commands, its own guardrails).
    //   Skip (`skip`)              → `--dangerously-skip-permissions` (full bypass;
    //                                the human confirmed it at launch).
    //   Plan (`plan`)              → `--permission-mode plan` (read-only).
    // `mode` is the *effective* mode for this pane: the session policy for the
    // panes atrium opens itself, or — for a ctl spawn — the per-spawn `--mode` after
    // the operator-elevate / worker-cap governance in `apply_ctl`.
    // `--session-id`, `--append-system-prompt`, and the ~/.claude.json folder-trust
    // gate below are all Claude Code CLI specifics — injecting them into another
    // vendor would break its launch — so the narrow `is_claude` test gates them.
    // The **trust posture** now maps per vendor: claude gets its `--permission-mode`
    // flags, codex gets its own `--ask-for-approval`/`--sandbox` flags (§`--trust`
    // is vendor-aware). Any other agent still launches with its command untouched.
    // The broad `is_agent_stem` continues to govern vendor-neutral treatment like
    // the identity-env decision in `wants_env` below.
    let is_claude = atrium::bind::is_claude_stem(&title);
    let is_codex = atrium::vendors::vendor_for_stem(&title) == Some(agsess::Vendor::Codex);
    let trusted_launch = (is_claude || is_codex) && mode != atrium::ctl::TrustMode::Off;
    let mut base: Vec<String> = if trusted_launch {
        let mut v = command.to_vec();
        if is_claude {
            match mode {
                atrium::ctl::TrustMode::Edits => {
                    v.extend(atrium::trust::accept_edits_args(
                        &atrium::trust::extra_allow_from_env(),
                    ));
                }
                atrium::ctl::TrustMode::Auto => {
                    v.push("--permission-mode".to_string());
                    v.push("auto".to_string());
                }
                atrium::ctl::TrustMode::Skip => {
                    v.push(atrium::ctl::SKIP_PERMISSIONS_FLAG.to_string());
                }
                atrium::ctl::TrustMode::Plan => {
                    v.push("--permission-mode".to_string());
                    v.push("plan".to_string());
                }
                atrium::ctl::TrustMode::Off => unreachable!("trusted_launch implies not Off"),
            }
        } else if is_codex {
            // codex's approval/sandbox flags — its analog of the above. Guarded by
            // `is_codex` (not a bare `else`) so that if the claude/codex stem sets
            // ever overlap or a third vendor is added, codex flags reach ONLY a
            // codex pane — never silently some other agent's command.
            v.extend(atrium::trust::codex_trust_args(mode));
        }
        v
    } else {
        command.to_vec()
    };
    // When the ctl channel is live, teach every agent pane — the initial one and
    // ctl-spawned workers alike — to delegate through `atrium ctl` (visible panes)
    // instead of its own invisible Task/background-agents tool. This is the
    // dependable layer: it is always in the agent's context (via
    // `--append-system-prompt`), so reliable delegation no longer hinges on a
    // skill happening to surface. Appended after any trust flags so it terminates
    // the `--allowedTools` list cleanly rather than being read as one of its values.
    // Combine any worktree-specific norms with the ctl directive into a SINGLE
    // --append-system-prompt block via combine_system_prompt. Claude is last-wins
    // on this flag — two separate pushes silently drop the first.
    if is_claude {
        let ctl = if CTL_ADDRESS.get().is_some() {
            Some(atrium::ctl::AGENT_CTL_DIRECTIVE)
        } else {
            None
        };
        if let Some(block) = combine_system_prompt(extra_norms, ctl) {
            base.push("--append-system-prompt".to_string());
            base.push(block);
        }
    }
    // …and pre-accept claude's *folder-trust* dialog for this pane's working
    // directory — a separate gate the permission mode does NOT cover (it's stored
    // per-dir in ~/.claude.json). Without this a trusted launch in an untrusted
    // folder still blocks on "trust this folder?". claude-only: codex has its own
    // per-folder trust in ~/.codex/config.toml (TOML — a follow-up; see
    // `trust::codex_trust_args`). Only under --trust/--skip, only the trust bit,
    // only this pane's cwd; a parse/IO problem is flashed and the pane spawns anyway.
    if trusted_launch && is_claude {
        let dir = cwd
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        if let Err(e) = atrium::trust::ensure_trusted(&dir) {
            *flash = Some((format!("folder-trust: {e}"), Instant::now()));
        }
    }
    // Agent-aware bind (§3.3): if this is an agent pane atrium is launching and the
    // user did not already pick a session, mint a uuid and append
    // `--session-id <uuid>` to the *agent's* args (before any `cmd /C` shim
    // wrapping, so the flag reaches claude, not the shim host). Remember the id.
    let session_id = atrium::bind::session_id_for(&base);
    let mut user_cmd: Vec<String> = base;
    if let Some(uuid) = &session_id {
        user_cmd.push("--session-id".to_string());
        user_cmd.push(uuid.clone());
    }
    let effective = effective_command(&user_cmd);
    // Opt-in spawn diagnostic: `ATRIUM_SPAWN_LOG=<file>` appends the exact command
    // (post trust-flags, post shim) atrium launches for each pane, so a "why isn't
    // this pane in the mode I expected" question is answered by data, not guesses.
    if let Ok(path) = std::env::var("ATRIUM_SPAWN_LOG") {
        if !path.is_empty() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(f, "[{title}] {}", effective.join(" "));
            }
        }
    }
    let argrefs: Vec<&str> = effective[1..].iter().map(String::as_str).collect();
    let r = rows.max(1);
    let c = cols.max(1);

    // Stamp the process-global agent id now — it is both the pane's spawn-tree
    // key and the `ATRIUM_PANE` value injected below, so an agent inside can
    // attribute its own `ctl spawn` calls back to this pane.
    let agent_id = next_agent_id();

    // ctl env (non-secret): when the control channel is on, every pane learns
    // the endpoint (`ATRIUM_CTL`) and its own id (`ATRIUM_PANE`). This is the base
    // env; identity secrets (if any) are merged on top for this one spawn.
    // A per-pane capability token: 32 bytes of OS CSPRNG entropy (hex), so a
    // sibling pane cannot guess or brute-force it. Injected as ATRIUM_TOKEN and
    // stored on the pane; the ctl server authenticates a request by matching it.
    // Peer-credential binding at the ipc layer (same-user endpoint) is the second
    // gate underneath this token — see `ipc.rs`.
    // No token means OS entropy failed. Fail CLOSED: this pane gets no ctl
    // environment at all, so it simply has no control-plane access. The old
    // behaviour minted a guessable token from non-crypto ids and injected it
    // anyway, which is strictly worse — a sibling pane could then forge it.
    // `\r\n` because atrium is in raw mode here.
    let token = atrium::uid::token();
    if token.is_none() {
        eprint!(
            "atrium: warning: OS entropy unavailable; this pane is starting WITHOUT \
             ctl access rather than with a guessable capability token\r\n"
        );
    }
    let pane_id = agent_id.to_string();
    let mut base_env = pane_base_env(
        atrium::orphan::session_key().as_ref(),
        match (CTL_ADDRESS.get(), token.as_ref()) {
            (Some(addr), Some(tok)) => Some((addr.as_str(), pane_id.as_str(), tok.as_str())),
            _ => None,
        },
    );
    base_env.extend_from_slice(extra_env);

    // Identity injection (path B): only for an agent pane with an identity set.
    // Decide ONCE so the spawn path and the pane's stored tag can never diverge
    // — a pane tagged with an identity is exactly a pane spawned with its env.
    let inject = atrium::identity::wants_env(command, identity);

    // We RE-RESOLVE on every spawn — the resolved env (which contains secret
    // material) lives only in this local, is handed straight to the pty, and is
    // dropped at the end of this function. It is never cached on the pane,
    // never logged, never printed. The pane stores only the identity *name*.
    //
    // Resolve failure (no such target, vault locked) is surfaced in the bar and
    // the pane spawns with plain env (ambient creds) but still in `cwd` — visible,
    // not silent, and never unauthenticated-without-saying-so (§7).
    let pty = if inject {
        let name = identity.expect("wants_env implies Some");
        // An identity may be a comma-separated list (`work,hf`) so one agent gets
        // several keys at once — each resolves to its own env var(s) and they are
        // merged. A later entry that maps to the same var wins. The resolved env
        // holds secret values: merged with the (non-secret) ctl base env for this
        // one spawn, then dropped — never formatted, logged, or stored.
        let mut merged = base_env.clone();
        let mut failed: Vec<String> = Vec::new();
        for part in name.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match akey::resolve(part) {
                Ok(env) => merged.extend(env),
                // Names only in the message (akey's error carries the typed name,
                // never a secret); one bad name doesn't sink the others.
                Err(_) => failed.push(part.to_string()),
            }
        }
        if !failed.is_empty() {
            *flash = Some((
                format!(
                    "identity {:?} unresolved — running without {}",
                    failed.join(","),
                    if merged.len() == base_env.len() {
                        "credentials"
                    } else {
                        "those"
                    }
                ),
                Instant::now(),
            ));
        }
        pty::Pty::spawn_full(&effective[0], &argrefs, r, c, &merged, cwd)?
    } else {
        pty::Pty::spawn_full(&effective[0], &argrefs, r, c, &base_env, cwd)?
    };
    // The second marker, on disk. Measured on macOS 26: `ps -E` prints the
    // environment of ordinary binaries but NOTHING for a SIP/platform binary,
    // and `/bin/sh` — what a default pane runs, and the exact population that
    // stranded — is one of those. So the env marker alone is blind to shells
    // here; the stamp covers every pane. It records the pane's own start token,
    // so a stale file cannot become a kill order against a recycled pid.
    atrium::orphan::stamp(pty.pid());
    Ok(Pane {
        id,
        pty,
        term: vterm::Term::new(r as usize, c as usize),
        filter: atrium::filter::Passthrough::new(),
        title,
        activity: false,
        // A fresh pane is "active" (idle 0) until proven idle; stamped forward on
        // every output byte in the run loop.
        last_activity: std::time::Instant::now(),
        draft: atrium::deliver::Draft::default(),
        exited: false,
        session_id,
        launch_ms: agsess::sessions::now_ms(),
        cwd: cwd.map(str::to_string),
        // Store the identity NAME only, and only for panes that would actually
        // run under it (agent panes) — a shell keeps `None`, so no misleading
        // tag on a pane that was never credentialed. `inject` is the same
        // decision the spawn used, so tag and env can never disagree.
        identity: identity.filter(|_| inject).map(str::to_string),
        agent_id,
        token,
        // Spawn-tree fields default to "root pane the human opened"; the ctl
        // spawn handler overrides role/parent/depth for a ctl-created worker.
        role: None,
        parent: None,
        depth: 0,
        can_spawn: true,
        painted: false,
        mouse_wanted: false,
        argv: command.to_vec(),
        worktree: None,
    })
}

/// Strip embedded newlines from a shell-arg before it is forwarded through a
/// `cmd /C` shim.  `cmd.exe` builds the command line by concatenating all argv
/// tokens and then parses that string line-by-line, so a bare `\n` acts as a
/// line delimiter and silently drops every token that follows it — that is how
/// `--permission-mode auto` and `--session-id` vanished from fleet panes that
/// received a multi-line kickoff prompt in r8.
///
/// `CreateProcessW` and POSIX `exec` pass argv literally, so they must NOT use
/// this sanitization (a multi-line prompt is valid and meaningful there).
pub(crate) fn sanitize_shim_arg(s: &str) -> String {
    // Collapse CRLF first so it counts as one space, then lone CR and LF.
    s.replace("\r\n", " ").replace('\r', " ").replace('\n', " ")
}

/// The argv atrium spawns for `command` given where it `resolved` on PATH.
///
/// A resolved `.cmd`/`.bat` shim (npm-installed CLIs, Claude Code included) is
/// spawned directly: `pty` runs batch targets under `cmd.exe /c` with cmd-safe
/// argument encoding (`pty::cmdline`), so `&`, `|`, `%` and quotes in a prompt,
/// branch name or fleet arg arrive intact. Newlines are the one thing cmd.exe
/// cannot carry, so shim args still collapse them ([`sanitize_shim_arg`]).
/// Other resolved programs get their full path; an unresolved command is left
/// for the spawn to report.
pub(crate) fn assemble_command(
    command: &[String],
    resolved: Option<std::path::PathBuf>,
) -> Vec<String> {
    match resolved {
        Some(path) => {
            let shim = atrium::resolve::needs_shell(&path);
            let mut v = vec![path.to_string_lossy().into_owned()];
            v.extend(command[1..].iter().map(|a| {
                if shim {
                    sanitize_shim_arg(a)
                } else {
                    a.clone()
                }
            }));
            v
        }
        None => command.to_vec(),
    }
}

/// On Windows, resolve the command the way the shell would (PATH x PATHEXT);
/// see [`assemble_command`] for what is spawned.
#[cfg(windows)]
pub(crate) fn effective_command(command: &[String]) -> Vec<String> {
    use atrium::resolve;
    let dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect();
    assemble_command(command, resolve::resolve(&command[0], &dirs, &exts))
}

/// Off Windows there is no PATH/PATHEXT resolution or batch shim: the command is
/// spawned as given (the same unresolved branch of [`assemble_command`]).
#[cfg(not(windows))]
pub(crate) fn effective_command(command: &[String]) -> Vec<String> {
    assemble_command(command, None)
}
