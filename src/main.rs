//! The amux binary: the passthrough loop.
//!
//!     amux                      # panes run your shell (COMSPEC / $SHELL)
//!     amux claude               # panes run `claude`
//!     amux claude --continue    # any command + args
//!
//! Ctrl+A then: c new pane, n/p cycle, 1-9 switch, x kill, q quit.

use amux::bar::{bar_paint, PaneInfo};
use amux::input::{Action, PrefixScanner};
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

struct Pane {
    pty: pty::Pty,
    title: String,
    activity: bool,
    exited: bool,
    filter: amux::filter::Passthrough,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--help") {
        eprintln!("usage: amux [command [args...]]   (Ctrl+A ? in the bar shows keys)");
        return ExitCode::SUCCESS;
    }
    if args.first().map(String::as_str) == Some("--stdin-probe") {
        return stdin_probe();
    }
    let command: Vec<String> = if args.is_empty() {
        vec![default_shell()]
    } else {
        args
    };
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = run(&mut term, &command);
    // rawterm's Drop restores modes after our own screen cleanup ran.
    code
}

fn run(term: &mut rawterm::Terminal, command: &[String]) -> ExitCode {
    let mut out = std::io::stdout();
    let (mut rows, mut cols) = term.size().unwrap_or((24, 80));
    // Alt screen; scroll region above the bar so bottom-line newlines from
    // the passthrough stream can never push the bar away.
    let _ = write!(out, "\x1b[?1049h\x1b[2J\x1b[H\x1b[1;{}r", rows - 1);
    let _ = out.flush();

    let mut panes: Vec<Pane> = Vec::new();
    match spawn_pane(command, rows, cols) {
        Ok(p) => panes.push(p),
        Err(e) => {
            cleanup_screen(&mut out);
            eprintln!("amux: cannot start {:?}: {e}", command[0]);
            return ExitCode::FAILURE;
        }
    }
    let mut active = 0usize;
    let mut scanner = PrefixScanner::new();
    let mut buf = [0u8; 8192];
    let mut force_bar = true;
    let mut last_bar_paint = Instant::now();
    let mut last_size_check = Instant::now();
    let mut last_bar = String::new();
    let mut flash: Option<(String, Instant)> = None;

    'outer: loop {
        // 1. keystrokes -> scanner -> pane / commands
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
                    let _ = panes[active].pty.write(&b);
                }
                Action::NextPane => {
                    let next = (active + 1) % panes.len();
                    switch(
                        &mut panes,
                        &mut active,
                        next,
                        rows,
                        cols,
                        &mut out,
                        &mut force_bar,
                    );
                }
                Action::PrevPane => {
                    let prev = (active + panes.len() - 1) % panes.len();
                    switch(
                        &mut panes,
                        &mut active,
                        prev,
                        rows,
                        cols,
                        &mut out,
                        &mut force_bar,
                    );
                }
                Action::SwitchTo(i) => {
                    if i < panes.len() {
                        switch(
                            &mut panes,
                            &mut active,
                            i,
                            rows,
                            cols,
                            &mut out,
                            &mut force_bar,
                        );
                    }
                }
                Action::NewPane => match spawn_pane(command, rows, cols) {
                    Ok(p) => {
                        panes.push(p);
                        let last = panes.len() - 1;
                        switch(
                            &mut panes,
                            &mut active,
                            last,
                            rows,
                            cols,
                            &mut out,
                            &mut force_bar,
                        );
                    }
                    Err(e) => {
                        // Never fail silently: put the reason in the bar.
                        flash = Some((
                            format!("cannot start {:?}: {e}", command[0]),
                            Instant::now(),
                        ));
                        force_bar = true;
                    }
                },
                Action::KillPane => {
                    let _ = panes[active].pty.kill();
                }
                Action::Quit => {
                    if std::env::var_os("AMUX_DEBUG").is_some() {
                        eprint!("[amux-dbg quit-received]\r\n");
                    }
                    break 'outer;
                }
            }
        }

        // 2. active pane output: passthrough, draining bursts
        for i in 0..8 {
            let wait = if i == 0 {
                Duration::from_millis(5)
            } else {
                Duration::ZERO
            };
            match panes[active].pty.read_timeout(&mut buf, wait) {
                Ok(Some(n)) if n > 0 => {
                    if std::env::var_os("AMUX_DEBUG").is_some() {
                        eprint!("[amux-dbg pane-out {n}]\r\n");
                    }
                    let cleaned = panes[active].filter.feed(&buf[..n]);
                    let _ = out.write_all(&cleaned);
                }
                Ok(Some(_)) => {
                    panes[active].exited = true;
                    break;
                }
                _ => break,
            }
        }
        let _ = out.flush();

        // 3. background panes: drain (discarded — ConPTY repaints on
        // switch) and flag activity
        for (i, pane) in panes.iter_mut().enumerate() {
            if i == active {
                continue;
            }
            while let Ok(Some(n)) = pane.pty.read_timeout(&mut buf, Duration::ZERO) {
                if n == 0 {
                    pane.exited = true;
                    break;
                }
                pane.activity = true;
            }
        }

        // 4. reap exits; drop dead panes
        for pane in panes.iter_mut() {
            if let Ok(Some(_)) = pane.pty.try_wait() {
                pane.exited = true;
            }
        }
        if panes.iter().any(|p| p.exited) {
            let was_active_title = panes[active].title.clone();
            let old_active_alive = !panes[active].exited;
            panes.retain(|p| !p.exited);
            if panes.is_empty() {
                break;
            }
            if old_active_alive {
                active = panes
                    .iter()
                    .position(|p| p.title == was_active_title)
                    .unwrap_or(0)
                    .min(panes.len() - 1);
            } else {
                let target = active.min(panes.len() - 1);
                active = target;
                repaint_pane(&mut panes[target], rows, cols, &mut out);
            }
            force_bar = true;
        }

        // 5. resize propagation
        if last_size_check.elapsed() >= Duration::from_millis(400) {
            last_size_check = Instant::now();
            if let Ok((r, c)) = term.size() {
                if (r, c) != (rows, cols) && r >= 3 {
                    rows = r;
                    cols = c;
                    let _ = write!(out, "\x1b[1;{}r", rows - 1);
                    for pane in panes.iter_mut() {
                        let _ = pane.pty.resize(rows - 1, cols);
                    }
                    force_bar = true;
                }
            }
        }

        // 6. the bar (periodic repaint survives a pane's own clear-screen)
        let infos: Vec<PaneInfo> = panes
            .iter()
            .enumerate()
            .map(|(i, p)| PaneInfo {
                title: p.title.clone(),
                active: i == active,
                activity: p.activity,
                exited: p.exited,
            })
            .collect();
        if flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5))
        {
            flash = None;
            force_bar = true;
        }
        let note = flash.as_ref().map(|(m, _)| m.as_str()).unwrap_or("");
        let painted = bar_paint(&infos, rows, cols as usize, note);
        if force_bar
            || painted != last_bar
            || last_bar_paint.elapsed() >= Duration::from_millis(500)
        {
            let _ = out.write_all(painted.as_bytes());
            let _ = out.flush();
            last_bar = painted;
            last_bar_paint = Instant::now();
            force_bar = false;
        }
    }

    let dbg = std::env::var_os("AMUX_DEBUG").is_some();
    for pane in panes.iter_mut() {
        let _ = pane.pty.kill();
    }
    if dbg {
        eprint!("[amux-dbg killed]\r\n");
    }
    cleanup_screen(&mut out);
    if dbg {
        eprint!("[amux-dbg cleaned]\r\n");
    }
    drop(panes);
    if dbg {
        eprint!("[amux-dbg panes-dropped]\r\n");
    }
    ExitCode::SUCCESS
}

fn spawn_pane(command: &[String], rows: u16, cols: u16) -> std::io::Result<Pane> {
    let title = std::path::Path::new(&command[0])
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| command[0].clone());
    let effective = effective_command(command);
    let argrefs: Vec<&str> = effective[1..].iter().map(String::as_str).collect();
    let pty = pty::Pty::spawn(&effective[0], &argrefs, rows.saturating_sub(1).max(1), cols)?;
    Ok(Pane {
        pty,
        title,
        activity: false,
        exited: false,
        filter: amux::filter::Passthrough::new(),
    })
}

fn switch(
    panes: &mut [Pane],
    active: &mut usize,
    to: usize,
    rows: u16,
    cols: u16,
    out: &mut impl Write,
    force_bar: &mut bool,
) {
    if to == *active {
        return;
    }
    *active = to;
    panes[to].activity = false;
    repaint_pane(&mut panes[to], rows, cols, out);
    *force_bar = true;
}

/// Bring a pane's screen back: clear our display, then nudge the pane's
/// size so its terminal repaints in full (ConPTY always does; Unix
/// full-screen apps redraw on SIGWINCH).
fn repaint_pane(pane: &mut Pane, rows: u16, cols: u16, out: &mut impl Write) {
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = out.flush();
    let _ = pane.pty.resize(rows.saturating_sub(2).max(1), cols);
    let _ = pane.pty.resize(rows.saturating_sub(1).max(1), cols);
}

fn cleanup_screen(out: &mut impl Write) {
    // Reset the scroll region, leave the alt screen.
    let _ = write!(out, "\x1b[r\x1b[?1049l");
    let _ = out.flush();
}

/// Diagnostic mode: print the hex of every raw byte stdin delivers for a
/// few seconds. Answers "what does my terminal actually send?" — including
/// through nested consoles — without guessing.
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

/// On Windows, resolve the command the way the shell would (PATH x
/// PATHEXT) and host `.cmd`/`.bat` shims under `cmd /C` — npm-installed
/// CLIs (Claude Code included) are such shims, and `CreateProcessW`
/// cannot launch them directly.
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
