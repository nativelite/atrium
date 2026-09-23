//! Hosted process trees: nothing is orphaned, on quit, signal or hard kill.

use crate::support::*;
use std::time::{Duration, Instant};

// --- process lifecycle (F13): atrium must not orphan its agents ---------------

/// Read a pid the hosted shell prints for itself, so the test can watch that
/// exact process after atrium is gone. Unix-only: the whole property is about
/// POSIX signal disposition.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The pid of a process's parent. atrium is located this way — as the parent of
/// the shell it hosts — rather than by scanning for its own binary name: the
/// integration tests run in parallel, so several atrium processes share this test
/// harness as their parent and a name scan can match somebody else's. (It did:
/// an earlier version of this test SIGHUP'd another test's atrium and made that
/// test fail instead of this one.)
#[cfg(unix)]
fn parent_of(pid: u32) -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// **Closing the terminal window must not orphan the agents.** A terminal that
/// goes away sends SIGHUP; atrium's teardown (which kills every pane) only runs
/// when the event loop exits normally, so without a handler the default action
/// kills atrium outright and every hosted agent is reparented to init and runs on
/// — invisibly, and in a real fleet, billably. Observed live on macOS: ten agent
/// processes still resident 19 minutes after the operator closed the window.
#[cfg(unix)]
#[test]
fn sighup_does_not_orphan_hosted_panes() {
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &["sh", "-i"], 24, 80).unwrap();

    // Have the hosted shell tell us its own pid, so we can watch it directly.
    // The marker is split across two quoted strings so the shell's echo of the
    // typed line ("echo \"$m\"\"=$$\"") cannot contain `atriumpid=` — only the
    // command's actual output does. Without that, read_until matches the echo
    // and returns before the shell has run anything.
    p.write(b"m=atriumpid; echo \"$m\"\"=$$\"\r\n").unwrap();
    let out = read_until(&mut p, b"atriumpid=", Duration::from_secs(15));
    let text = String::from_utf8_lossy(&out);
    let shell_pid: u32 = text
        .split("atriumpid=")
        .nth(1)
        .and_then(|s| {
            let d: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            d.parse().ok()
        })
        .unwrap_or_else(|| panic!("no shell pid in: {text:?}"));
    assert!(pid_alive(shell_pid), "hosted shell never came up");

    let atrium = parent_of(shell_pid).expect("could not locate the atrium process");
    let _ = std::process::Command::new("kill")
        .args(["-HUP", &atrium.to_string()])
        .status();

    // The pane must go with it. Poll rather than sleep a fixed time.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !pid_alive(shell_pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &shell_pid.to_string()])
        .status();
    panic!("pane {shell_pid} survived SIGHUP to atrium {atrium} — orphaned agent");
}

/// **Closing a full-screen overlay must repaint the pane** (F10).
///
/// `repaint_focused` cleared the screen and then asked the hosted app to redraw
/// by resizing its pty `h-1` then straight back to `h` — ending at the size it
/// already had. An app that only repaints on a real dimension change (or that
/// is mid-turn and defers) correctly does nothing, and the operator is left
/// staring at a blank screen with just the status bar. Reported on macOS after
/// pressing `g` on a board decision; the fix paints from atrium's own emulator
/// instead of asking the child.
///
/// `sh` never redraws on SIGWINCH, which makes it the perfect probe: if the
/// prompt comes back, atrium painted it.
#[test]
fn closing_an_overlay_repaints_the_pane() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80).unwrap();

    // A marker on screen that only atrium can bring back. Written so `repaint=marker`
    // appears in the command's OUTPUT but not in the typed line itself, so the
    // match below can't be satisfied by the input echo: sh uses `$m` indirection;
    // cmd escapes the `=` with a caret (`repaint^=marker` on the line → `repaint=
    // marker` in the output). Both shells are covered — the shell is already
    // branched above, and the marker command must be too (this test shipped
    // bash-only and could never pass on Windows).
    let marker_cmd: &[u8] = if cfg!(windows) {
        b"echo repaint^=marker\r\n"
    } else {
        b"m=repaint; echo \"$m\"\"=marker\"\r\n"
    };
    p.write(marker_cmd).unwrap();
    let out = read_until(&mut p, b"repaint=marker", Duration::from_secs(15));
    assert!(
        contains(&out, b"repaint=marker"),
        "pane never painted: {:?}",
        String::from_utf8_lossy(&out)
    );

    // Open the board overlay, which clears the screen and covers the pane. Wait
    // for the panel's own heading: the status bar always reads `b:board`, so
    // waiting on "board" returned before the overlay opened, and both toggles
    // then landed in one tick — open and closed with nothing ever drawn.
    p.write(b"\x01b").unwrap();
    let opened = read_until(&mut p, b"board + bus", Duration::from_secs(10));
    assert!(
        contains(&opened, b"board + bus"),
        "the board never opened: {:?}",
        String::from_utf8_lossy(&opened)
    );

    // Close it. That is a real view transition, so the pane must come back —
    // painted by atrium, because `sh` will not repaint itself.
    p.write(b"\x01b").unwrap();
    let back = read_until(&mut p, b"repaint=marker", Duration::from_secs(10));
    assert!(
        contains(&back, b"repaint=marker"),
        "pane not repainted after the overlay closed — blank screen: {:?}",
        String::from_utf8_lossy(&back)
    );

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// The three overlays hand keys between each other and back to the pane, and
/// `Ctrl+A q` quits from inside one (r10 B11 carved these handlers out of the
/// event loop into `overlay_keys`; this drives every transition through the real
/// binary so the carve is held to the old behavior).
#[test]
fn overlays_switch_between_each_other_close_on_esc_and_quit_from_inside() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80).unwrap();
    let marker_cmd: &[u8] = if cfg!(windows) {
        b"echo overlay^=marker\r\n"
    } else {
        b"m=overlay; echo \"$m\"\"=marker\"\r\n"
    };
    p.write(marker_cmd).unwrap();
    let out = read_until(&mut p, b"overlay=marker", Duration::from_secs(15));
    assert!(contains(&out, b"overlay=marker"), "pane never painted");

    // Pane -> overview -> board -> log, each switch in one keystroke.
    for (keys, header) in [
        (&b"\x01o"[..], &b"overview"[..]),
        (b"\x01b", b"board + bus"),
        (b"\x01a", b"activity log"),
    ] {
        p.write(keys).unwrap();
        let seen = read_until(&mut p, header, Duration::from_secs(10));
        assert!(
            contains(&seen, header),
            "{:?} did not open {:?}: {:?}",
            String::from_utf8_lossy(keys),
            String::from_utf8_lossy(header),
            String::from_utf8_lossy(&seen)
        );
    }

    // Esc closes the log and the pane comes back.
    p.write(b"\x1b").unwrap();
    let back = read_until(&mut p, b"overlay=marker", Duration::from_secs(10));
    assert!(
        contains(&back, b"overlay=marker"),
        "Esc did not return to the pane: {:?}",
        String::from_utf8_lossy(&back)
    );

    // Quit from inside an open overlay.
    p.write(b"\x01o").unwrap();
    read_until(&mut p, b"overview", Duration::from_secs(10));
    p.write(b"\x01q").unwrap();
    assert_eq!(
        wait_exit(&mut p, 15),
        0,
        "Ctrl+A q inside the overview must quit"
    );
}

/// **A pane's whole process tree must die with it** (review finding #2).
///
/// `Pty::kill` is `SIGKILL` to the direct child only, so everything that child
/// spawned survives and is reparented to `init`. For an agent pane that means
/// its MCP servers, language servers and node helpers leak — measured at 23
/// processes holding 4.6 GB during a fleet review. This is not a signal-handling
/// edge case: it happens on a **clean** `Ctrl+A q`, and has since day one.
///
/// `sleep` stands in for whatever an agent spawns. It is found via `pgrep`
/// rather than by parsing pane output, so the test does not depend on rendering.
#[cfg(unix)]
#[test]
fn quitting_kills_the_panes_whole_process_tree() {
    // A NON-interactive shell, deliberately. An interactive `sh -i` runs job
    // control, which puts each background job in its own process group — the one
    // arrangement `killpg` cannot follow. A real agent does not background jobs
    // through a job-control shell; it forks helpers that inherit its group, which
    // is what this reproduces.
    //
    // The grandchild reports its own pid to a file keyed by this test process, so
    // the test never has to guess which atrium is its own. Scanning the process
    // table for that is racy under the parallel suite and picks somebody else's.
    let marker = std::env::temp_dir().join(format!("atrium-tree-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = format!("sleep 600 & echo $! > {} ; wait", marker.display());
    let mut p =
        pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &["sh", "-c", &script], 24, 80).unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let grandkid: u32 = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            if let Ok(v) = t.trim().parse::<u32>() {
                break v;
            }
        }
        assert!(
            Instant::now() < deadline,
            "grandchild never reported its pid"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(pid_alive(grandkid), "grandchild died before the test began");
    let grandkids = [grandkid];

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    std::thread::sleep(Duration::from_millis(1500));

    let survivors: Vec<u32> = grandkids
        .iter()
        .copied()
        .filter(|g| pid_alive(*g))
        .collect();
    for g in &survivors {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &g.to_string()])
            .status();
    }
    assert!(
        survivors.is_empty(),
        "pane grandchildren survived a clean quit: {survivors:?}"
    );
}

/// **A SIGKILLed atrium must not leave the tree behind** (lifecycle layer 4).
///
/// Signal handlers cannot run on `SIGKILL`, a panic, or an OOM kill, so no
/// teardown inside atrium can cover them — the whole tree simply leaks. The
/// watchdog is a re-exec of atrium that outlives the session: when atrium vanishes
/// it kills every process group the crash registry names. This is the only layer
/// that covers a death atrium cannot observe.
#[cfg(unix)]
#[test]
fn a_sigkilled_atrium_still_takes_its_tree_down() {
    let marker = std::env::temp_dir().join(format!("atrium-kill9-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = format!("sleep 600 & echo $! > {} ; wait", marker.display());
    let p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &["sh", "-c", &script], 24, 80).unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let grandkid: u32 = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            if let Ok(v) = t.trim().parse::<u32>() {
                break v;
            }
        }
        assert!(
            Instant::now() < deadline,
            "grandchild never reported its pid"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    // Give the loop a tick to register the pane and start the watchdog.
    std::thread::sleep(Duration::from_millis(600));

    // The one death atrium cannot handle.
    let atrium = p.pid();
    assert!(atrium != 0, "no atrium pid");
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &atrium.to_string()])
        .status();

    // Watchdog poll (250ms) + SIGTERM + grace (750ms), with room to spare.
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if !pid_alive(grandkid) {
            let _ = std::fs::remove_file(&marker);
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &grandkid.to_string()])
        .status();
    let _ = std::fs::remove_file(&marker);
    panic!("tree survived SIGKILL of atrium {atrium} — watchdog did not clean up");
}

/// The same SIGKILL, with the registry taken away first — which is what atrium
/// itself used to do.
///
/// Teardown computed which pane groups had survived and then deleted the
/// registry *unconditionally*, on the one path where it had just proved the
/// record was needed. The watchdog woke on pipe EOF, read a file that was no
/// longer there, got an empty list, killed nothing, and exited. Here the file is
/// removed by hand so the same hole is reproduced deterministically rather than
/// waiting for a teardown to lose the race.
///
/// What must collect the tree instead is the marker on the pane itself. Note
/// which marker: the pane is `/bin/sh`, and macOS will not disclose a SIP
/// binary's environment to `ps` at all, so this passes through the stamp file —
/// the same path every default shell pane takes on this platform.
///
/// Revert check: drop the `orphan::sweep` call at the end of
/// `reap::watchdog_main` and this fails with "tree survived", while
/// `a_sigkilled_atrium_still_takes_its_tree_down` above keeps passing, because
/// that one still has its registry.
#[cfg(unix)]
#[test]
fn a_sigkilled_atrium_takes_its_tree_down_even_with_no_registry() {
    let marker = std::env::temp_dir().join(format!("atrium-noreg-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = format!("sleep 600 & echo $! > {} ; wait", marker.display());
    let p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &["sh", "-c", &script], 24, 80).unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let grandkid: u32 = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            if let Ok(v) = t.trim().parse::<u32>() {
                break v;
            }
        }
        assert!(
            Instant::now() < deadline,
            "grandchild never reported its pid"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    // Let the loop register the pane, write the registry and start the watchdog.
    std::thread::sleep(Duration::from_millis(800));

    let atrium = p.pid();
    assert!(atrium != 0, "no atrium pid");
    let registry = atrium::reap::registry_path(atrium);
    assert!(
        registry.exists(),
        "atrium should have written {} before we take it away",
        registry.display()
    );
    std::fs::remove_file(&registry).expect("remove the registry");

    let _ = std::process::Command::new("kill")
        .args(["-KILL", &atrium.to_string()])
        .status();

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if !pid_alive(grandkid) {
            let _ = std::fs::remove_file(&marker);
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &grandkid.to_string()])
        .status();
    let _ = std::fs::remove_file(&marker);
    panic!("tree survived SIGKILL of atrium {atrium} with its registry deleted");
}

/// Every pane is born marked, whether or not the control channel is on.
///
/// The default launch has no `--allow-ctl`, and that is exactly the case that
/// used to produce a pane with no atrium environment at all. Read back from inside
/// the pane rather than from `ps`, because on macOS `ps -E` will not show a
/// shell's environment to anyone — which is a fact about `ps`, not about whether
/// the variable is there.
///
/// Revert check: move the `ATRIUM_SESSION` push in `pane_base_env` back inside the
/// `if let Some((addr, …)) = ctl` arm and this fails — the pane echoes an empty
/// line where the key should be.
#[cfg(unix)]
#[test]
fn a_pane_launched_without_ctl_still_carries_the_session_marker() {
    let out = std::env::temp_dir().join(format!("atrium-mark-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let script = format!(
        "printf '[%s]\\n' \"$ATRIUM_SESSION\" > {} ; sleep 600",
        out.display()
    );
    // No --allow-ctl: the default posture, and the one that used to be unmarked.
    let mut p =
        pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &["sh", "-c", &script], 24, 80).unwrap();
    let atrium = p.pid();

    let deadline = Instant::now() + Duration::from_secs(15);
    let seen = loop {
        if let Ok(t) = std::fs::read_to_string(&out) {
            if t.contains('\n') {
                break t.trim().to_string();
            }
        }
        assert!(Instant::now() < deadline, "pane never reported its marker");
        std::thread::sleep(Duration::from_millis(100));
    };
    // Quit the way a human does — Ctrl+A q — so the pane's tree is torn down
    // through the normal path and nothing is left holding a pty.
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
    let _ = std::fs::remove_file(&out);

    let key = seen
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or("");
    assert!(
        !key.is_empty(),
        "a pane spawned without --allow-ctl carried no ATRIUM_SESSION (got {seen:?})"
    );
    let parsed = atrium::orphan::SessionKey::parse(key)
        .unwrap_or_else(|| panic!("ATRIUM_SESSION is not a well-formed key: {key:?}"));
    assert_eq!(
        parsed.owner, atrium,
        "the marker must name the atrium that spawned the pane"
    );
}

/// A pane script that spawns a long-lived grandchild (`ping`), records its pid to
/// `marker`, and waits on it — the Windows analog of `sleep 600 & echo $! ; wait`.
#[cfg(windows)]
fn ping_grandchild_argv(marker: &std::path::Path) -> [String; 5] {
    let mpath = marker.display().to_string().replace('\\', "/");
    let script = format!(
        "$p = Start-Process -FilePath ping -ArgumentList @('-n','601','127.0.0.1') \
         -PassThru -WindowStyle Hidden; Set-Content -Path '{mpath}' -Value $p.Id; \
         Wait-Process -Id $p.Id"
    );
    [
        "powershell".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        script,
    ]
}

/// Block until `marker` holds a pid, or panic past `deadline`.
#[cfg(windows)]
fn read_marker_pid(marker: &std::path::Path, within: Duration) -> u32 {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(t) = std::fs::read_to_string(marker) {
            if let Ok(v) = t.trim().parse::<u32>() {
                return v;
            }
        }
        assert!(
            Instant::now() < deadline,
            "grandchild never reported its pid"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// True once `pid` is gone, polled until `within` elapses.
#[cfg(windows)]
fn wait_pid_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !atrium::reap::pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// Block until `pid` is a member of a Windows Job Object, or `within` elapses.
/// Returns true once membership is confirmed; false if the deadline passes first.
/// This replaces a fixed sleep when waiting for atrium's run loop to call
/// `session_job.assign()` — the poll terminates as soon as the assignment is
/// observable, so slow CI machines get as much time as they need.
#[cfg(windows)]
fn wait_in_any_job(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if atrium::reap::pid_in_any_job(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// **A clean `Ctrl+A q` kills the pane's whole tree on Windows** (acceptance #2).
/// atrium exits, its last handle to the session Job Object closes, and
/// kill-on-close terminates the pane and its grandchild together.
#[cfg(windows)]
#[test]
fn quitting_kills_the_pane_tree_windows() {
    let marker = std::env::temp_dir().join(format!("atrium-wtree-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let argv = ping_grandchild_argv(&marker);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv_ref, 24, 80).unwrap();

    let grandkid = read_marker_pid(&marker, Duration::from_secs(25));
    assert!(
        atrium::reap::pid_alive(grandkid),
        "grandchild died before the test began"
    );

    p.write(b"\x01q").unwrap(); // Ctrl+A q
    let _ = wait_exit(&mut p, 15);

    let gone = wait_pid_gone(grandkid, Duration::from_secs(10));
    if !gone {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &grandkid.to_string()])
            .status();
    }
    let _ = std::fs::remove_file(&marker);
    assert!(gone, "pane grandchild {grandkid} survived a clean quit");
}

/// **A hard-killed atrium still takes its tree down on Windows** (acceptance #3).
/// `taskkill /F` is `TerminateProcess` — uncatchable, no teardown runs — but the
/// kernel closes atrium's job handle on exit, so kill-on-close fires anyway. This
/// is the death mode no macOS mechanism can match.
#[cfg(windows)]
#[test]
fn hard_killed_atrium_still_takes_its_tree_down_windows() {
    let marker = std::env::temp_dir().join(format!("atrium-wkill-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let argv = ping_grandchild_argv(&marker);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv_ref, 24, 80).unwrap();

    let grandkid = read_marker_pid(&marker, Duration::from_secs(25));
    // Wait until atrium's run loop has assigned the pane to the Job Object.
    // The grandchild inherits Job membership from PowerShell once PowerShell is
    // assigned, so polling grandkid is the first observable evidence of assignment.
    // Ceiling of 15 s handles slow CI; on a fast machine this returns in <100 ms.
    let assigned = wait_in_any_job(grandkid, Duration::from_secs(15));
    assert!(
        assigned,
        "grandkid {grandkid} never joined a Job Object — run loop stalled"
    );

    let atrium = p.pid();
    assert!(atrium != 0, "no atrium pid");
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &atrium.to_string()])
        .status();

    let gone = wait_pid_gone(grandkid, Duration::from_secs(12));
    if !gone {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &grandkid.to_string()])
            .status();
    }
    let _ = std::fs::remove_file(&marker);
    assert!(
        gone,
        "tree survived taskkill /F of atrium {atrium} — kill-on-close did not fire"
    );
}
