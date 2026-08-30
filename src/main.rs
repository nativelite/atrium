//! The amux binary: a dual-mode run loop — passthrough (0.1) and tiled (0.2).
//!
//!     amux                      # panes run your shell (COMSPEC / $SHELL)
//!     amux claude               # panes run `claude`
//!     amux claude --continue    # any command + args
//!
//! Ctrl+A then: c new window, n/p cycle, 1-9 switch windows, x kill focused
//! pane, q quit; `"`/`%` split the focused pane, h/j/k/l or arrows move focus,
//! z zoom (full-screen passthrough) the focused pane.
//!
//! ## The two rendering modes
//!
//! A window whose split tree is a single pane — or any window with its focused
//! pane *zoomed* — renders in **passthrough**: the pane's raw VT bytes go
//! straight to the real terminal, zero emulation, perfect fidelity. This is the
//! 0.1 path, untouched.
//!
//! The moment a window holds two or more visible panes it renders **tiled**:
//! each pane drives a `vterm::Term` sized to its rect, every pane's screen is
//! composited into one master `ansi::Screen`, and `Screen::diff` writes only the
//! changed bytes. Zoom is the escape hatch back to perfect fidelity for a TUI
//! that the emulator can't render pixel-exact (wide glyphs, sixel, mouse).

use amux::bar::{bar_paint, PaneInfo};
use amux::input::{Action, Dir, PrefixScanner};
use amux::layout::{self, Rect, Tree};
use amux::tile::{compose, AgentMark, PaneState, PaneView};
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// One hosted terminal: a pty, its emulator (for tiled compositing), its
/// passthrough filter (for the passthrough / zoom path), and bar metadata. Each
/// pane has a stable `id` the window's split tree refers to.
struct Pane {
    id: usize,
    pty: pty::Pty,
    term: vterm::Term,
    filter: amux::filter::Passthrough,
    title: String,
    activity: bool,
    exited: bool,
    /// The session id amux injected via `--session-id` when this pane is an
    /// agent it launched (§3.3). `None` for shells and agents amux did not
    /// bind. The binder maps this to the pane's live `agsess::Status`.
    session_id: Option<String>,
    /// The credential identity **name** this pane's agent runs under, if any
    /// (path B). Only the name is stored — never the resolved secret. On every
    /// spawn amux re-resolves the env via `akey` and injects it for that child
    /// alone; the resolved values live only for the spawn call and are dropped
    /// immediately. Surfaced in the chrome as a `·<name>` tag.
    identity: Option<String>,
}

/// One window: a split tree over a set of panes, plus a zoom flag. Windows are
/// the 0.1 "switchable full-screen" concept; each can now itself be a tiled
/// split tree (design doc §3: "windows and splits coexist").
struct Window {
    panes: Vec<Pane>,
    tree: Tree,
    zoomed: bool,
    next_id: usize,
}

impl Window {
    fn pane(&self, id: usize) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    fn pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    fn focused_mut(&mut self) -> Option<&mut Pane> {
        let f = self.tree.focus();
        self.pane_mut(f)
    }

    /// Tiled iff more than one pane and not zoomed. A single pane, or a zoomed
    /// pane, is passthrough.
    fn tiled(&self) -> bool {
        self.panes.len() > 1 && !self.zoomed
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // amux's own `--identity <name>` / `-I <name>` is stripped off the front,
    // before the hosted command begins; it tags the initial agent pane and is
    // inherited by every split/new pane (stored, re-resolved per spawn). Only
    // the NAME is threaded through — never the resolved secret. We parse it
    // *first* so amux's own meta-flags (`--help`, `--stdin-probe`) are still
    // recognized when they follow an identity (`amux --identity work --help`).
    let (identity, rest) = amux::identity::parse(&args);
    if rest.first().map(String::as_str) == Some("--help") {
        eprintln!(
            "usage: amux [--identity <name>] [command [args...]]   (Ctrl+A ? in the bar shows keys)"
        );
        return ExitCode::SUCCESS;
    }
    if rest.first().map(String::as_str) == Some("--stdin-probe") {
        return stdin_probe();
    }
    let command: Vec<String> = if rest.is_empty() {
        vec![default_shell()]
    } else {
        rest
    };
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    run(&mut term, &command, identity.as_deref())
}

fn run(term: &mut rawterm::Terminal, command: &[String], identity: Option<&str>) -> ExitCode {
    let mut out = std::io::stdout();
    let (mut rows, mut cols) = term.size().unwrap_or((24, 80));
    // Alt screen; scroll region above the bar so bottom-line newlines from the
    // passthrough stream can never push the bar away.
    let _ = write!(out, "\x1b[?1049h\x1b[2J\x1b[H\x1b[1;{}r", rows - 1);
    let _ = out.flush();

    let mut windows: Vec<Window> = Vec::new();
    // The initial pane can fail to spawn (command missing) *or* fail to resolve
    // its identity (no such target, vault locked). A spawn failure aborts amux
    // (there is nothing to host); an identity-resolve failure is surfaced in the
    // bar and the pane spawns *without* the credential — never silently, and
    // never unauthenticated-without-saying-so (§7).
    let mut flash: Option<(String, Instant)> = None;
    match spawn_window(command, rows, cols, 0, identity, &mut flash) {
        Ok(w) => windows.push(w),
        Err(e) => {
            cleanup_screen(&mut out);
            eprintln!("amux: cannot start {:?}: {e}", command[0]);
            return ExitCode::FAILURE;
        }
    }
    let mut active = 0usize; // active window index
    let mut scanner = PrefixScanner::new();
    let mut buf = [0u8; 8192];
    let mut force_repaint = true;
    let mut last_bar_paint = Instant::now();
    let mut last_size_check = Instant::now();
    let mut last_bar = String::new();

    // Agent session state (§5): one read-only `agsess::World` over the Claude
    // projects root, polled on the loop's existing `Instant`-throttle pattern —
    // no threads. The *first* refresh uses `refresh_since(process_start_ms)` so
    // the cold history scan (agtop measures ~1.4 s for ~60 MB) never freezes
    // keystrokes; amux can never care about a session that stopped writing
    // before it started. Thereafter: bound panes tail ~1 s, discovery ~5 s
    // (accelerated to ~1 s while any agent pane is still unbound).
    let mut world = agsess::World::new(agsess::default_root());
    let process_start_ms = agsess::sessions::now_ms();
    world.refresh_since(process_start_ms);
    let mut last_agent_poll = Instant::now();
    let mut last_agent_discover = Instant::now();
    // The previous composited master, kept per-frame so tiled mode diffs. Reset
    // to None (full repaint) on mode/layout/window changes.
    let mut prev_master: Option<ansi::Screen> = None;

    // The read at the top can fail (terminal gone) *and* commands deep inside
    // `break 'outer`; a labeled `loop` expresses both. clippy's while-let
    // rewrite can't host the labeled break, so allow it here.
    #[allow(clippy::while_let_loop)]
    'outer: loop {
        // 1. keystrokes -> scanner -> focused pane / commands
        let bytes = match term.read_bytes(Duration::from_millis(15)) {
            Ok(b) => b,
            Err(_) => break,
        };
        if !bytes.is_empty() && std::env::var_os("AMUX_DEBUG").is_some() {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            eprint!("[amux-dbg stdin {}]\r\n", hex.join(" "));
        }
        for action in scanner.feed(&bytes) {
            match action {
                Action::Forward(b) => {
                    if let Some(p) = windows[active].focused_mut() {
                        let _ = p.pty.write(&b);
                    }
                }
                Action::NextPane => {
                    let next = (active + 1) % windows.len();
                    switch_window(&mut windows, &mut active, next, rows, cols, &mut out);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::PrevPane => {
                    let prev = (active + windows.len() - 1) % windows.len();
                    switch_window(&mut windows, &mut active, prev, rows, cols, &mut out);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::SwitchTo(i) => {
                    if i < windows.len() {
                        switch_window(&mut windows, &mut active, i, rows, cols, &mut out);
                        prev_master = None;
                        force_repaint = true;
                    }
                }
                Action::NewPane => {
                    match spawn_window(command, rows, cols, windows.len(), identity, &mut flash) {
                        Ok(w) => {
                            windows.push(w);
                            let last = windows.len() - 1;
                            switch_window(&mut windows, &mut active, last, rows, cols, &mut out);
                            prev_master = None;
                            force_repaint = true;
                        }
                        Err(e) => {
                            flash = Some((
                                format!("cannot start {:?}: {e}", command[0]),
                                Instant::now(),
                            ));
                            force_repaint = true;
                        }
                    }
                }
                Action::SplitH => {
                    split_focused(
                        &mut windows[active],
                        layout::Dir::Horizontal,
                        command,
                        rows,
                        cols,
                        identity,
                        &mut flash,
                    );
                    resize_window(&mut windows[active], rows, cols);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::SplitV => {
                    split_focused(
                        &mut windows[active],
                        layout::Dir::Vertical,
                        command,
                        rows,
                        cols,
                        identity,
                        &mut flash,
                    );
                    resize_window(&mut windows[active], rows, cols);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::MoveFocus(d) => {
                    let outer = tiled_outer(rows, cols);
                    windows[active].tree.move_focus(to_move(d), outer);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::Zoom => {
                    let w = &mut windows[active];
                    // Zoom only means something with more than one pane.
                    if w.panes.len() > 1 {
                        w.zoomed = !w.zoomed;
                        resize_window(w, rows, cols);
                        prev_master = None;
                        force_repaint = true;
                    }
                }
                Action::KillPane => {
                    let w = &mut windows[active];
                    let victim = w.tree.focus();
                    if w.panes.len() > 1 {
                        // Multi-pane window: close synchronously so focus leaves
                        // the dying pane at once (the next keystrokes must route
                        // to the survivor, not the corpse) and the frame re-tiles
                        // without waiting for the async reap.
                        if let Some(p) = w.pane_mut(victim) {
                            let _ = p.pty.kill();
                        }
                        w.tree.close(victim);
                        w.panes.retain(|p| p.id != victim);
                        w.zoomed = w.zoomed && w.panes.len() > 1;
                        resize_window(w, rows, cols);
                        prev_master = None;
                        force_repaint = true;
                    } else if let Some(p) = w.pane_mut(victim) {
                        // Sole pane: async kill; the reap step drops the window
                        // and the app exits when the last window is gone (0.1).
                        let _ = p.pty.kill();
                    }
                }
                Action::Quit => {
                    if std::env::var_os("AMUX_DEBUG").is_some() {
                        eprint!("[amux-dbg quit-received]\r\n");
                    }
                    break 'outer;
                }
            }
        }

        // 2. drain every pane in the active window (all panes are live in
        //    tiled mode); feed the emulator, and in passthrough also write the
        //    focused pane's cleaned bytes straight through.
        let tiled = windows[active].tiled();
        let focus = windows[active].tree.focus();
        for pane in windows[active].panes.iter_mut() {
            for i in 0..8 {
                let wait = if i == 0 {
                    Duration::from_millis(5)
                } else {
                    Duration::ZERO
                };
                match pane.pty.read_timeout(&mut buf, wait) {
                    Ok(Some(n)) if n > 0 => {
                        // Always feed the emulator so a later switch/split/zoom
                        // renders the current screen without a repaint nudge.
                        pane.term.feed(&buf[..n]);
                        if !tiled && pane.id == focus {
                            let cleaned = pane.filter.feed(&buf[..n]);
                            let _ = out.write_all(&cleaned);
                        } else if pane.id != focus {
                            pane.activity = true;
                        }
                    }
                    Ok(Some(_)) => {
                        pane.exited = true;
                        break;
                    }
                    _ => break,
                }
            }
        }
        if !tiled {
            let _ = out.flush();
        }

        // 3. background windows: drain (discarded — emulator/ConPTY keep the
        //    screen) and flag activity so the bar shows it.
        for (i, w) in windows.iter_mut().enumerate() {
            if i == active {
                continue;
            }
            for pane in w.panes.iter_mut() {
                while let Ok(Some(n)) = pane.pty.read_timeout(&mut buf, Duration::ZERO) {
                    if n == 0 {
                        pane.exited = true;
                        break;
                    }
                    pane.term.feed(&buf[..n]);
                    pane.activity = true;
                }
            }
        }

        // 4. reap exits; drop dead panes and empty windows, re-tiling.
        let mut layout_changed = false;
        for w in windows.iter_mut() {
            for pane in w.panes.iter_mut() {
                if let Ok(Some(_)) = pane.pty.try_wait() {
                    pane.exited = true;
                }
            }
            if w.panes.iter().any(|p| p.exited) {
                let dead: Vec<usize> = w.panes.iter().filter(|p| p.exited).map(|p| p.id).collect();
                for id in dead {
                    // Collapse the tree first (keeps focus valid), then drop the
                    // pane. A last-pane close leaves the tree single; the window
                    // itself is removed below when its panes go empty.
                    let _ = w.tree.close(id);
                }
                w.panes.retain(|p| !p.exited);
                w.zoomed = w.zoomed && w.panes.len() > 1;
                layout_changed = true;
            }
        }
        if layout_changed {
            let empty_before_active = windows[..active]
                .iter()
                .filter(|w| w.panes.is_empty())
                .count();
            windows.retain(|w| !w.panes.is_empty());
            if windows.is_empty() {
                break;
            }
            active = active
                .saturating_sub(empty_before_active)
                .min(windows.len() - 1);
            resize_window(&mut windows[active], rows, cols);
            prev_master = None;
            force_repaint = true;
        }

        // 5. resize propagation
        if last_size_check.elapsed() >= Duration::from_millis(400) {
            last_size_check = Instant::now();
            if let Ok((r, c)) = term.size() {
                if (r, c) != (rows, cols) && r >= 3 {
                    rows = r;
                    cols = c;
                    let _ = write!(out, "\x1b[1;{}r", rows - 1);
                    for w in windows.iter_mut() {
                        resize_window(w, rows, cols);
                    }
                    prev_master = None;
                    force_repaint = true;
                }
            }
        }

        // 5b. agent state (§5). `refresh` tails only files that grew, so the
        //     bound-pane tick is cheap; discovery is the full `read_dir`, run
        //     rarely — but accelerated while any agent pane still lacks its
        //     transcript so a fresh agent binds promptly.
        let any_unbound = windows.iter().any(|w| {
            w.panes.iter().any(|p| {
                p.session_id
                    .as_deref()
                    .is_some_and(|id| !world.sessions.iter().any(|s| s.id == id))
            })
        });
        let discover_every = if any_unbound {
            Duration::from_millis(1000)
        } else {
            Duration::from_millis(5000)
        };
        if last_agent_discover.elapsed() >= discover_every {
            // Discovery + tail: a full refresh picks up new transcripts and
            // tails grown ones in one pass.
            world.refresh();
            last_agent_discover = Instant::now();
            last_agent_poll = Instant::now();
        } else if last_agent_poll.elapsed() >= Duration::from_millis(1000) {
            // Bound-pane tick: refresh tails only files whose length grew, so
            // this is ~a handful of stats for a small grid.
            world.refresh();
            last_agent_poll = Instant::now();
        }

        // 6. render the active window (tiled compose+diff, else passthrough is
        //    already written above) then paint the bar.
        if windows[active].tiled() {
            let master = render_tiled(&windows[active], rows, cols, &world.sessions);
            let bytes = match &prev_master {
                Some(prev) => prev.diff(&master),
                None => master.render_full(),
            };
            if !bytes.is_empty() {
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
            prev_master = Some(master);
        } else if force_repaint {
            // Passthrough (single pane or zoomed): nudge the focused pane's pty
            // so it repaints in full, the same trick 0.1 uses on window switch.
            repaint_focused(&mut windows[active], rows, cols, &mut out);
        }

        // 7. the bar (windows, with the active one starred)
        let infos: Vec<PaneInfo> = windows
            .iter()
            .enumerate()
            .map(|(i, w)| PaneInfo {
                title: w.panes.first().map(|p| p.title.clone()).unwrap_or_default(),
                active: i == active,
                activity: w.panes.iter().any(|p| p.activity),
                exited: w.panes.iter().all(|p| p.exited),
                // A window is "waiting" when a bound agent pane in it is blocked
                // on the human (§4.2 rung 3). Status only.
                waiting: w.panes.iter().any(|p| {
                    matches!(
                        amux::bind::status_for(p.session_id.as_deref(), &world.sessions),
                        Some(agsess::Status::WaitingApproval)
                    )
                }),
                // The window's identity tag: the first pane's identity name (all
                // panes in a window inherit the same identity in v1). Name only.
                identity: w.panes.first().and_then(|p| p.identity.clone()),
            })
            .collect();
        if flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5))
        {
            flash = None;
            force_repaint = true;
        }
        let note = flash.as_ref().map(|(m, _)| m.as_str()).unwrap_or("");
        let painted = bar_paint(&infos, rows, cols as usize, note);
        if force_repaint
            || painted != last_bar
            || last_bar_paint.elapsed() >= Duration::from_millis(500)
        {
            let _ = out.write_all(painted.as_bytes());
            let _ = out.flush();
            last_bar = painted;
            last_bar_paint = Instant::now();
        }
        force_repaint = false;
    }

    let dbg = std::env::var_os("AMUX_DEBUG").is_some();
    for w in windows.iter_mut() {
        for pane in w.panes.iter_mut() {
            let _ = pane.pty.kill();
        }
    }
    if dbg {
        eprint!("[amux-dbg killed]\r\n");
    }
    cleanup_screen(&mut out);
    if dbg {
        eprint!("[amux-dbg cleaned]\r\n");
    }
    drop(windows);
    if dbg {
        eprint!("[amux-dbg panes-dropped]\r\n");
    }
    ExitCode::SUCCESS
}

/// The tiled drawing area: the whole terminal minus the bar row (last line).
fn tiled_outer(rows: u16, cols: u16) -> Rect {
    Rect {
        row: 0,
        col: 0,
        rows: (rows as usize).saturating_sub(1).max(1),
        cols: cols as usize,
    }
}

fn to_move(d: Dir) -> layout::Move {
    match d {
        Dir::Left => layout::Move::Left,
        Dir::Right => layout::Move::Right,
        Dir::Up => layout::Move::Up,
        Dir::Down => layout::Move::Down,
    }
}

/// Compose the active window's panes into a master screen (boxed borders +
/// titles + liveness color + content); the caller diffs it to the terminal.
fn render_tiled(
    w: &Window,
    rows: u16,
    cols: u16,
    sessions: &[agsess::AgentSession],
) -> ansi::Screen {
    let outer = tiled_outer(rows, cols);
    let rects = w.tree.rects(outer);
    let focus = w.tree.focus();
    let views: Vec<PaneView> = rects
        .iter()
        .filter_map(|(id, rect)| {
            w.pane(*id).map(|p| {
                // Liveness in priority order: focus, then a dead child, then
                // recent background activity, then plain idle. `index` is the
                // pane's 1-based position in the split's leaf order.
                let state = if *id == focus {
                    PaneState::Focused
                } else if p.exited {
                    PaneState::Exited
                } else if p.activity {
                    PaneState::Active
                } else {
                    PaneState::Idle
                };
                // Agent mark only for *unfocused* bound agent panes (§4.1): a
                // blocked agent in the focused pane needs no escalation. The
                // binder maps the pane's injected id to its live status; carries
                // status only — never any transcript text.
                let agent = if *id == focus {
                    None
                } else {
                    amux::bind::status_for(p.session_id.as_deref(), sessions)
                        .map(|status| AgentMark { status })
                };
                let index = w.panes.iter().position(|q| q.id == *id).unwrap_or(0) + 1;
                PaneView {
                    screen: p.term.screen(),
                    rect: *rect,
                    index,
                    title: &p.title,
                    state,
                    agent,
                    identity: p.identity.as_deref(),
                }
            })
        })
        .collect();
    // Compose over the full terminal (rows), leaving the bar row untouched; the
    // bar is painted separately after the diff, exactly as in passthrough.
    compose(rows as usize, cols as usize, &views)
}

/// Split the focused pane, spawning a new pane sized to what its half will be.
/// On spawn failure the split is abandoned and the reason lands in the bar.
fn split_focused(
    w: &mut Window,
    dir: layout::Dir,
    command: &[String],
    rows: u16,
    cols: u16,
    identity: Option<&str>,
    flash: &mut Option<(String, Instant)>,
) {
    let new_id = w.next_id;
    // Size the new pane roughly to a half's *inner* area (minus the border);
    // resize_window fixes it exactly right after the split.
    let (pr, pc) = (
        (rows.saturating_sub(1).max(1) / 2).saturating_sub(2),
        (cols / 2).saturating_sub(2),
    );
    // Splits inherit the active identity (§4). resolve failure lands in `flash`.
    match spawn_pane(command, pr.max(1), pc.max(1), new_id, identity, flash) {
        Ok(pane) => {
            w.panes.push(pane);
            w.next_id += 1;
            w.tree.split(dir, new_id);
            w.zoomed = false; // a fresh split is always tiled
        }
        Err(e) => {
            *flash = Some((
                format!("cannot start {:?}: {e}", command[0]),
                Instant::now(),
            ));
            let _ = rows;
            let _ = cols;
        }
    }
}

/// Recompute every pane's rect and push the size to its pty and emulator. In
/// passthrough (single/zoomed) the focused pane owns the whole area; in tiled
/// mode each pane gets its rect.
fn resize_window(w: &mut Window, rows: u16, cols: u16) {
    if w.tiled() {
        let outer = tiled_outer(rows, cols);
        let rects = w.tree.rects(outer);
        for (id, rect) in rects {
            // Each pane wears a one-cell box border on every side, so its child
            // and emulator see the *inner* content area, not the bordered rect.
            let inner_rows = rect.rows.saturating_sub(2).max(1);
            let inner_cols = rect.cols.saturating_sub(2).max(1);
            if let Some(p) = w.pane_mut(id) {
                let _ = p.pty.resize(inner_rows as u16, inner_cols as u16);
                p.term.resize(inner_rows, inner_cols);
            }
        }
    } else {
        // Passthrough: the focused (or sole) pane fills the area above the bar.
        let ar = rows.saturating_sub(1).max(1);
        let focus = w.tree.focus();
        let sole = w.panes.len() == 1;
        for p in w.panes.iter_mut() {
            if p.id == focus || sole {
                let _ = p.pty.resize(ar, cols);
                p.term.resize(ar as usize, cols as usize);
            }
        }
    }
}

fn switch_window(
    windows: &mut [Window],
    active: &mut usize,
    to: usize,
    rows: u16,
    cols: u16,
    out: &mut impl Write,
) {
    if to == *active {
        return;
    }
    *active = to;
    for p in windows[to].panes.iter_mut() {
        p.activity = false;
    }
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = out.flush();
    resize_window(&mut windows[to], rows, cols);
}

/// Passthrough repaint: nudge the focused pane's pty size so its terminal
/// repaints in full (ConPTY always does; Unix full-screen apps redraw on
/// SIGWINCH). Used on window switch and mode changes into passthrough.
fn repaint_focused(w: &mut Window, rows: u16, cols: u16, out: &mut impl Write) {
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = out.flush();
    let ar = rows.saturating_sub(1).max(1);
    let focus = w.tree.focus();
    if let Some(p) = w.pane_mut(focus) {
        let _ = p.pty.resize(ar.saturating_sub(1).max(1), cols);
        let _ = p.pty.resize(ar, cols);
    }
}

fn spawn_window(
    command: &[String],
    rows: u16,
    cols: u16,
    _idx: usize,
    identity: Option<&str>,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Window> {
    let pane = spawn_pane(
        command,
        rows.saturating_sub(1).max(1),
        cols,
        0,
        identity,
        flash,
    )?;
    Ok(Window {
        panes: vec![pane],
        tree: Tree::new(0),
        zoomed: false,
        next_id: 1,
    })
}

fn spawn_pane(
    command: &[String],
    rows: u16,
    cols: u16,
    id: usize,
    identity: Option<&str>,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Pane> {
    let title = std::path::Path::new(&command[0])
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| command[0].clone());
    // Agent-aware bind (§3.3): if this is an agent pane amux is launching and the
    // user did not already pick a session, mint a uuid and append
    // `--session-id <uuid>` to the *agent's* args (before any `cmd /C` shim
    // wrapping, so the flag reaches claude, not the shim host). Remember the id.
    let session_id = amux::bind::session_id_for(command);
    let user_cmd: Vec<String> = match &session_id {
        Some(uuid) => {
            let mut v = command.to_vec();
            v.push("--session-id".to_string());
            v.push(uuid.clone());
            v
        }
        None => command.to_vec(),
    };
    let effective = effective_command(&user_cmd);
    let argrefs: Vec<&str> = effective[1..].iter().map(String::as_str).collect();
    let r = rows.max(1);
    let c = cols.max(1);

    // Identity injection (path B): only for an agent pane with an identity set.
    // Decide ONCE so the spawn path and the pane's stored tag can never diverge
    // — a pane tagged with an identity is exactly a pane spawned with its env.
    let inject = amux::identity::wants_env(command, identity);

    // We RE-RESOLVE on every spawn — the resolved env (which contains secret
    // material) lives only in this local, is handed straight to the pty, and is
    // dropped at the end of this function. It is never cached on the pane,
    // never logged, never printed. The pane stores only the identity *name*.
    //
    // Resolve failure (no such target, vault locked) is surfaced in the bar and
    // the pane spawns with plain `spawn` (ambient creds) — visible, not silent,
    // and never unauthenticated-without-saying-so (§7).
    let pty = if inject {
        let name = identity.expect("wants_env implies Some");
        match akey::resolve(name) {
            Ok(env) => {
                // `env` holds secret values; used for this one spawn only, then
                // dropped. Deliberately never formatted, logged, or stored.
                pty::Pty::spawn_with_env(&effective[0], &argrefs, r, c, &env)?
            }
            Err(e) => {
                // Name only in the message — `e` is akey's own error text
                // ("no key or WIF profile named …"), which carries the name the
                // user typed, never any secret value.
                *flash = Some((
                    format!("identity {name:?} unresolved: {e} — running without it"),
                    Instant::now(),
                ));
                pty::Pty::spawn(&effective[0], &argrefs, r, c)?
            }
        }
    } else {
        pty::Pty::spawn(&effective[0], &argrefs, r, c)?
    };
    Ok(Pane {
        id,
        pty,
        term: vterm::Term::new(r as usize, c as usize),
        filter: amux::filter::Passthrough::new(),
        title,
        activity: false,
        exited: false,
        session_id,
        // Store the identity NAME only, and only for panes that would actually
        // run under it (agent panes) — a shell keeps `None`, so no misleading
        // tag on a pane that was never credentialed. `inject` is the same
        // decision the spawn used, so tag and env can never disagree.
        identity: identity.filter(|_| inject).map(str::to_string),
    })
}

fn cleanup_screen(out: &mut impl Write) {
    // Reset the scroll region, leave the alt screen.
    let _ = write!(out, "\x1b[r\x1b[?1049l");
    let _ = out.flush();
}

/// Diagnostic mode: print the hex of every raw byte stdin delivers for a few
/// seconds. Answers "what does my terminal actually send?" — including through
/// nested consoles — without guessing.
fn stdin_probe() -> ExitCode {
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux: not a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = std::io::stdout();
    let _ = writeln!(out, "probe: type for 3s\r");
    let _ = out.flush();
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        if let Ok(bytes) = term.read_bytes(Duration::from_millis(100)) {
            if !bytes.is_empty() {
                let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
                let _ = writeln!(out, "got[{}]\r", hex.join(" "));
                let _ = out.flush();
            }
        }
    }
    let _ = writeln!(out, "probe-done\r");
    let _ = out.flush();
    ExitCode::SUCCESS
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".into())
    }
}

/// On Windows, resolve the command the way the shell would (PATH x PATHEXT) and
/// host `.cmd`/`.bat` shims under `cmd /C` — npm-installed CLIs (Claude Code
/// included) are such shims, and `CreateProcessW` cannot launch them directly.
#[cfg(windows)]
fn effective_command(command: &[String]) -> Vec<String> {
    use amux::resolve;
    let dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect();
    match resolve::resolve(&command[0], &dirs, &exts) {
        Some(path) if resolve::needs_shell(&path) => {
            let mut v = vec!["cmd".to_string(), "/C".to_string()];
            v.push(path.to_string_lossy().into_owned());
            v.extend(command[1..].iter().cloned());
            v
        }
        Some(path) => {
            let mut v = vec![path.to_string_lossy().into_owned()];
            v.extend(command[1..].iter().cloned());
            v
        }
        None => command.to_vec(),
    }
}

#[cfg(not(windows))]
fn effective_command(command: &[String]) -> Vec<String> {
    command.to_vec()
}
