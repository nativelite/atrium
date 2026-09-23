//! A live atrium session: start, passthrough, windows, the command prompt.

use crate::support::*;
use std::time::{Duration, Instant};

// --- end to end: atrium inside a pty ------------------------------------------

/// `atrium --version` answers on stdout with a zero exit, with no terminal.
///
/// It is the first thing typed into a bug report, and until it existed an
/// unrecognized flag fell through to "the command to host" — so `atrium --version`
/// died on "stdin/stdout must be a terminal" the moment anyone piped it. Both
/// spellings, and after an `--identity`, since identity is stripped first.
#[test]
fn version_prints_without_a_terminal() {
    let expected = format!("atrium {}", env!("CARGO_PKG_VERSION"));
    for args in [
        vec!["--version"],
        vec!["-V"],
        vec!["--identity", "work", "--version"],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_atrium"))
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "exit for {args:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            expected,
            "stdout for {args:?}"
        );
    }
}

/// atrium hosting a one-shot command: output passes through, and when the
/// only pane's child exits, atrium itself exits cleanly.
#[test]
fn passthrough_and_auto_exit() {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut p = pty::Pty::spawn(
        env!("CARGO_BIN_EXE_atrium"),
        &[shell, flag, "echo atrium-e2e-marker"],
        24,
        80,
    )
    .unwrap();
    let out = read_until(&mut p, b"atrium-e2e-marker", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-e2e-marker"),
        "output: {:?}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// A plain `atrium` shows its startup logo. The hosted shell prints its prompt
/// within milliseconds, and that used to wipe the splash before its first frame
/// was ever written; the logo now holds until it has been seen, then the shell
/// takes over and still answers.
#[test]
fn a_plain_start_shows_the_logo_before_the_shell() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80).unwrap();
    let out = read_until(&mut p, b"where your agents gather", Duration::from_secs(15));
    let logo_at = Instant::now();
    assert!(
        contains(&out, b"where your agents gather"),
        "no logo in: {:?}",
        String::from_utf8_lossy(&out)
    );
    // Seen means held: the shell's own first screen must not replace the logo
    // in the same breath. (A frame that is written and wiped a few ms later is
    // in the byte stream but never on the screen.)
    let prompt: &[u8] = if cfg!(windows) { b"Microsoft" } else { b"$ " };
    let out = read_until(&mut p, prompt, Duration::from_secs(15));
    assert!(
        contains(&out, prompt),
        "no shell screen: {:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        logo_at.elapsed() >= Duration::from_millis(800),
        "the logo was replaced after only {:?}",
        logo_at.elapsed()
    );
    p.write(b"echo atrium-logo-then-shell\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-logo-then-shell", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-logo-then-shell"),
        "the shell never took over: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// **A terminal that stops reading freezes nothing but its own view.**
///
/// The test stops reading atrium's screen while a pane floods output, so every
/// frame atrium writes backs up. atrium must keep draining the pane (the flood
/// finishes and the shell writes a file after it), and once the terminal reads
/// again the screen must come back and stay live.
///
/// Unix also types a command blind while nothing is read: keys are independent
/// of output there. Windows can't be asked that: the console host that stopped
/// delivering output stops delivering input too (measured: atrium's loop kept
/// ticking, no key arrived until the terminal read again).
#[test]
fn a_terminal_that_stops_reading_freezes_nothing_but_the_view() {
    let dir = std::env::temp_dir().join(format!("atrium-unread-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let flooded = dir.join("flooded.txt");
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80).unwrap();
    let prompt: &[u8] = if cfg!(windows) { b"Microsoft" } else { b"$ " };
    let out = read_until(&mut p, prompt, Duration::from_secs(15));
    assert!(
        contains(&out, prompt),
        "no shell: {:?}",
        String::from_utf8_lossy(&out)
    );
    std::thread::sleep(Duration::from_millis(300));

    // A flood that records when it is done, typed while the terminal still
    // reads; then the terminal stops reading entirely.
    let fpath = flooded.display().to_string();
    let (flood, last_line) = if cfg!(windows) {
        (
            format!(
                "(for /L %i in (1,1,10000) do @echo flood-line-%i) & echo done> \"{fpath}\"\r\n"
            ),
            "flood-line-10000",
        )
    } else {
        (
            format!(
                "i=1; while [ $i -le 200000 ]; do echo flood-line-$i; i=$((i+1)); done; \
                 echo done > '{fpath}'\r\n"
            ),
            "flood-line-200000",
        )
    };
    p.write(flood.as_bytes()).unwrap();
    let done_within = |path: &std::path::Path, secs: u64| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if std::fs::read_to_string(path)
                .is_ok_and(|t| t.contains("done") || t.contains("alive"))
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let drained = done_within(&flooded, 90);

    #[cfg(unix)]
    let keys = {
        let alive = dir.join("alive.txt");
        p.write(format!("echo alive > '{}'\r\n", alive.display()).as_bytes())
            .unwrap();
        done_within(&alive, 30)
    };

    // Read again: the screen has to come back, and stay live.
    let mut buf = [0u8; 8192];
    let mut resumed = Vec::new();
    let settle = Instant::now() + Duration::from_secs(5);
    while Instant::now() < settle {
        if let Ok(Some(n)) = p.read_timeout(&mut buf, Duration::from_millis(100)) {
            resumed.extend_from_slice(&buf[..n]);
        }
    }
    // Written so the typed line itself can't satisfy the match.
    let live: &[u8] = if cfg!(windows) {
        b"echo resync^-ok-7\r\n"
    } else {
        b"m=resync; echo \"$m-ok-7\"\r\n"
    };
    p.write(live).unwrap();
    let back = read_until(&mut p, b"resync-ok-7", Duration::from_secs(20));
    p.write(b"\x01q").unwrap();
    let code = wait_exit(&mut p, 30);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        drained,
        "the pane's flood never finished while the screen was unread: atrium stopped draining it"
    );
    #[cfg(unix)]
    assert!(
        keys,
        "a command typed while the screen was unread never ran: atrium stopped taking keys"
    );
    assert!(
        contains(&resumed, last_line.as_bytes()),
        "the screen did not come back with the pane's current content: {:?}",
        String::from_utf8_lossy(&strip_csi_bytes(
            &resumed[resumed.len().saturating_sub(3000)..]
        ))
    );
    assert!(
        contains(&back, b"resync-ok-7"),
        "the screen is not live after resuming: {:?}",
        String::from_utf8_lossy(&back)
    );
    assert_eq!(code, 0);
}

/// **A platform limitation is information, not a decision** (Windows).
///
/// The warden can't detect nested sessions on Windows. It says so once. That note
/// used to be raised as a decision, so every Windows session opened with
/// "1 decision needs you" on the bar — which read as an error the operator had to
/// act on, and trained them to ignore the one signal meant to stop them. It is
/// still recorded (the audit log keeps it), just not counted as a decision.
#[cfg(windows)]
#[test]
fn a_platform_limitation_is_not_counted_as_a_decision() {
    let audit = std::env::temp_dir().join(format!("atrium-info-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&audit);
    let env = [("ATRIUM_CTL_AUDIT".to_string(), audit.display().to_string())];
    let mut p = pty::Pty::spawn_with_env(
        env!("CARGO_BIN_EXE_atrium"),
        &["--allow-ctl", "cmd", "/Q"],
        24,
        80,
        &env,
    )
    .unwrap();
    // The warden runs on a 3 s cadence; give it two passes.
    let out = read_until(&mut p, b"decision", Duration::from_secs(8));
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    let recorded = std::fs::read_to_string(&audit).unwrap_or_default();
    let _ = std::fs::remove_file(&audit);
    assert!(
        !contains(&out, b"decision"),
        "the bar counted a platform limitation as a decision: {:?}
audit: {recorded}",
        String::from_utf8_lossy(&strip_csi_bytes(&out[out.len().saturating_sub(600)..]))
    );
    assert!(
        recorded.contains("warden-descent-unsupported"),
        "the limitation must still be recorded: {recorded}"
    );
}

/// Interactive session: keystrokes reach the hosted shell through atrium,
/// its response comes back, the bar is painted, and Ctrl+A q quits.
#[test]
fn interactive_roundtrip_bar_and_quit() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80).unwrap();
    // The bar names the pane after the command.
    let bar_needle: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    let out = read_until(&mut p, bar_needle, Duration::from_secs(15));
    assert!(
        contains(&out, bar_needle),
        "no bar in: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"echo atrium-rt-42\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-rt-42", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-rt-42"),
        "output: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap(); // Ctrl+A q
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Ctrl+A c opens a second pane (bar shows it) and 1/2 switch between
/// them with the round-trip still working afterwards.
#[test]
fn new_pane_opens_and_switches() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80).unwrap();
    let one: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    let out = read_until(&mut p, one, Duration::from_secs(15));
    assert!(contains(&out, one), "no first pane bar");
    p.write(b"\x01c").unwrap();
    let out = read_until(&mut p, two, Duration::from_secs(15));
    assert!(
        contains(&out, two),
        "no second pane in bar after Ctrl+A c: {:?}",
        String::from_utf8_lossy(&out)
    );
    // Switch back to 1 and prove the pane still talks.
    p.write(b"\x011").unwrap();
    p.write(b"echo atrium-np-9\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-np-9", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-np-9"),
        "pane 1 dead after switching: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Set a shell variable in the focused pane, and the `echo` whose OUTPUT (not
/// its echoed input, which still reads `%V%`/`${V}`) is `{tag}-{value}`.
fn set_var_cmd(value: &str) -> String {
    if cfg!(windows) {
        format!("set ATRIUM_W={value}\r\n")
    } else {
        format!("ATRIUM_W={value}\r\n")
    }
}

fn echo_var_cmd(tag: &str) -> String {
    if cfg!(windows) {
        format!("echo {tag}-%ATRIUM_W%\r\n")
    } else {
        format!("echo {tag}-${{ATRIUM_W}}\r\n")
    }
}

/// Ctrl+A n / Ctrl+A p cycle windows and wrap at both ends: each shell carries
/// its own variable, so the echo proves which window received the keys.
#[test]
fn next_and_prev_window_wrap_and_route_input() {
    let mut p = spawn_atrium_shell(24, 80);
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    p.write(set_var_cmd("one").as_bytes()).unwrap();
    p.write(b"\x01c").unwrap();
    let out = read_until(&mut p, two, Duration::from_secs(15));
    assert!(contains(&out, two), "no second window after Ctrl+A c");
    p.write(set_var_cmd("two").as_bytes()).unwrap();

    p.write(b"\x01n").unwrap(); // from window 2, next wraps to window 1
    p.write(echo_var_cmd("nx").as_bytes()).unwrap();
    let out = read_until(&mut p, b"nx-one", Duration::from_secs(15));
    assert!(
        contains(&out, b"nx-one"),
        "Ctrl+A n did not wrap to window 1: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01p").unwrap(); // from window 1, prev wraps to window 2
    p.write(echo_var_cmd("pv").as_bytes()).unwrap();
    let out = read_until(&mut p, b"pv-two", Duration::from_secs(15));
    assert!(
        contains(&out, b"pv-two"),
        "Ctrl+A p did not wrap to window 2: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Ctrl+A ! opens the platform shell in a new window, live.
#[test]
fn shell_window_opens_live() {
    let mut p = spawn_atrium_shell(24, 80);
    // The same resolution atrium uses (COMSPEC / SHELL), as the bar labels it.
    let var = if cfg!(windows) { "COMSPEC" } else { "SHELL" };
    let fallback = if cfg!(windows) { "cmd" } else { "sh" };
    let shell = std::env::var(var).unwrap_or_else(|_| fallback.into());
    let stem = std::path::Path::new(&shell)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| fallback.into());
    let label = format!("2:{stem}");
    p.write(b"\x01!").unwrap();
    let out = read_until(&mut p, label.as_bytes(), Duration::from_secs(15));
    assert!(
        contains(&out, label.as_bytes()),
        "no {label} window after Ctrl+A !: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"echo atrium-shellwin\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-shellwin", Duration::from_secs(15));
    assert!(contains(&out, b"atrium-shellwin"), "shell window not live");
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Let atrium drain and act on what was written so far, the way the gap between
/// a human's keystrokes does, so the next write lands in a separate read.
fn settle(p: &mut pty::Pty) {
    read_until(p, b"\x00never-printed\x00", Duration::from_millis(700));
}

/// Ctrl+A : captures keys into a command line: Esc closes it without running
/// anything (keys reach the pane again), Enter opens the typed command in a new
/// window. Keys are paced like typing (see `settle`).
#[test]
fn command_prompt_cancels_and_opens_a_window() {
    let mut p = spawn_atrium_shell(24, 80);
    // Cancelled: the typed text never runs, and the next keys reach the pane.
    p.write(b"\x01:").unwrap();
    settle(&mut p);
    p.write(b"echo atrium-never\x1b").unwrap();
    settle(&mut p);
    p.write(set_var_cmd("still-one").as_bytes()).unwrap();
    p.write(echo_var_cmd("esc").as_bytes()).unwrap();
    let out = read_until(&mut p, b"esc-still-one", Duration::from_secs(15));
    assert!(
        contains(&out, b"esc-still-one"),
        "keys did not reach the pane after Esc closed the prompt: {:?}",
        String::from_utf8_lossy(&out)
    );

    // Submitted: the command opens as window 2 and is live.
    let (cmd, two): (&[u8], &[u8]) = if cfg!(windows) {
        (b"cmd /Q\r", b"2:cmd")
    } else {
        (b"sh -i\r", b"2:sh")
    };
    p.write(b"\x01:").unwrap();
    settle(&mut p);
    p.write(cmd).unwrap();
    let out = read_until(&mut p, two, Duration::from_secs(15));
    assert!(
        contains(&out, two),
        "prompt did not open the command as window 2: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(echo_var_cmd("sub").as_bytes()).unwrap();
    // A fresh shell: the variable set in window 1 is not defined here.
    let fresh: &[u8] = if cfg!(windows) {
        b"sub-%ATRIUM_W%"
    } else {
        b"sub-\r"
    };
    let out = read_until(&mut p, fresh, Duration::from_secs(15));
    assert!(
        contains(&out, fresh),
        "keys did not reach the new window: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Keys that arrive in the same terminal read as Ctrl+A : (a paste, a fast
/// typist, a slow link) belong to the prompt it just opened — they used to skip
/// it and land in the pane, which then ran the command itself.
#[test]
fn command_prompt_keeps_keys_from_the_same_read() {
    let mut p = spawn_atrium_shell(24, 80);
    let (burst, two): (&[u8], &[u8]) = if cfg!(windows) {
        (b"\x01:cmd /Q\r", b"2:cmd")
    } else {
        (b"\x01:sh -i\r", b"2:sh")
    };
    p.write(burst).unwrap();
    let out = read_until(&mut p, two, Duration::from_secs(15));
    assert!(
        contains(&out, two),
        "the command typed with Ctrl+A : did not open window 2: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// A literal Ctrl+A goes through with the doubled prefix.
#[test]
fn double_prefix_reaches_the_child() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80).unwrap();
    read_until(&mut p, b"atrium", Duration::from_secs(15));
    // Ctrl+A Ctrl+A -> literal 0x01 -> most shells show nothing fatal;
    // then a normal command still round-trips (the stream is not desynced).
    p.write(b"\x01\x01").unwrap();
    p.write(b"echo atrium-lit-7\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-lit-7", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-lit-7"),
        "output: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}
