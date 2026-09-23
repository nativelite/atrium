//! The control plane, driven from inside live panes.

use crate::support::*;
use std::time::Duration;

// --- end to end: the ctl control channel (C1) -------------------------------

/// Spawn `atrium --allow-ctl <shell>` in a pty, with `ATRIUM_CTL_ALLOW` set so the
/// harmless shell counts as a spawnable worker, and return the pty once the bar
/// (pane 1) is up. The hosted shell inherits `ATRIUM_CTL`/`ATRIUM_PANE`, so a client
/// typed into it drives the real channel — exactly as an agent-in-a-pane would.
fn spawn_atrium_ctl_shell() -> (pty::Pty, &'static str, &'static str) {
    let (shell, flag): (&str, &str) = if cfg!(windows) {
        ("cmd", "/Q")
    } else {
        ("sh", "-i")
    };
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &["--allow-ctl", shell, flag],
        24,
        100,
        &[("ATRIUM_CTL_ALLOW".to_string(), shell.to_string())],
        None,
    )
    .unwrap();
    let bar: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    read_until(&mut p, bar, Duration::from_secs(15));
    (p, shell, flag)
}

/// `atrium ctl list`, run inside a live `--allow-ctl` atrium pane, connects over the
/// real channel and returns the org chart as JSON — proving the whole loop:
/// bind → inject env → client connect → non-blocking server drain → reply.
#[test]
fn ctl_list_roundtrips_through_a_live_atrium() {
    let (mut p, _shell, _flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(format!("\"{atrium}\" ctl list\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"tree\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"tree\""),
        "no ctl list reply in: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `atrium ctl spawn -- <shell>` opens a *visible* new worker window: after the
/// call the bar gains a second pane entry. This is the C1 "an in-pane
/// `ctl spawn` opens a visible worker" done-criterion, end to end.
#[test]
fn ctl_spawn_opens_a_visible_worker_window() {
    let (mut p, shell, _flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(format!("\"{atrium}\" ctl spawn --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    let out = read_until(&mut p, two, Duration::from_secs(20));
    assert!(
        contains(&out, two),
        "no second (worker) pane in the bar after ctl spawn: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// A spawn of a command that is *not* on the allowlist is refused cleanly over
/// the channel — the guard reaches the client as a JSON error, no worker opens.
#[test]
fn ctl_spawn_off_the_allowlist_is_refused() {
    let (mut p, _shell, _flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    // `whoami` is a real binary on both platforms but not an agent/allowlisted.
    p.write(format!("\"{atrium}\" ctl spawn -- whoami\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"allowlist", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":false") && contains(&out, b"allowlist"),
        "expected an allowlist refusal, got: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `atrium ctl status <id>` returns a target's status as JSON. Pane 0 is always
/// the initial pane (agent ids are process-global from 0), so this is stable.
#[test]
fn ctl_status_reports_a_pane() {
    let (mut p, _shell, _flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(format!("\"{atrium}\" ctl status 0\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"pane\":0", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"pane\":0"),
        "no status reply: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `atrium ctl send <role> <text>` delivers the text to the worker as input. We
/// spawn the worker in a *new* window, task it from the caller window, then
/// switch to the worker window to observe: the marker appears there only if the
/// send actually reached the worker's pty (the caller's command echo lives in a
/// different window, so it can't produce a false positive).
#[test]
fn ctl_send_delivers_a_task_to_a_worker() {
    let (mut p, shell, _flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    p.write(format!("\"{atrium}\" ctl spawn --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    read_until(&mut p, two, Duration::from_secs(20)); // worker window opened
    p.write(format!("\"{atrium}\" ctl send dev_1 echo WORKERMARK7\r\n").as_bytes())
        .unwrap();
    p.write(b"\x012").unwrap(); // Ctrl+A 2 -> watch the worker window
    let out = read_until(&mut p, b"WORKERMARK7", Duration::from_secs(20));
    assert!(
        contains(&out, b"WORKERMARK7"),
        "worker never received the send: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `atrium ctl spawn --here` succeeds over the channel (tiles the worker beside its
/// caller): the reply carries the worker's role, proving the split-placement path
/// ran end to end.
#[test]
fn ctl_spawn_here_succeeds() {
    let (mut p, shell, _flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(format!("\"{atrium}\" ctl spawn --here --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"role\":\"dev_1\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"role\":\"dev_1\""),
        "no --here spawn reply: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `atrium ctl respawn` restarts the agent it was pointed at, arguments and all.
/// It used to relaunch only the command's name, so `claude --model opus` came back
/// as a bare `claude` — the wrong agent for a clean restart. The spawn log records
/// the exact command each launch ran; the respawn's line must carry the worker's
/// own argument, which the parent shell's command does not.
#[test]
fn ctl_respawn_keeps_the_panes_arguments() {
    let log = std::env::temp_dir().join(format!(
        "atrium-respawn-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (shell, flag, marker): (&str, &str, &str) = if cfg!(windows) {
        ("cmd", "/Q", "/D")
    } else {
        ("sh", "-i", "-u")
    };
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &["--allow-ctl", shell, flag],
        24,
        100,
        &[
            ("ATRIUM_CTL_ALLOW".to_string(), shell.to_string()),
            (
                "ATRIUM_SPAWN_LOG".to_string(),
                log.to_string_lossy().into_owned(),
            ),
        ],
        None,
    )
    .unwrap();
    let bar: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    read_until(&mut p, bar, Duration::from_secs(15));
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(
        format!("\"{atrium}\" ctl spawn --here --role keeper -- {shell} {flag} {marker}\r\n")
            .as_bytes(),
    )
    .unwrap();
    let out = read_until(&mut p, b"\"role\":\"keeper\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true"),
        "spawn failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(format!("\"{atrium}\" ctl respawn keeper\r\n").as_bytes())
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let lines = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        if lines.len() >= 3 || std::time::Instant::now() > deadline {
            break lines;
        }
        let _ = read_until(&mut p, b"\x00never", Duration::from_millis(200));
    };
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    let _ = std::fs::remove_file(&log);
    assert!(
        lines.len() >= 3,
        "expected launch, spawn and respawn in the spawn log, got {lines:?}"
    );
    let spawned = lines[1].split_whitespace().any(|a| a == marker);
    let respawned = lines[2].split_whitespace().any(|a| a == marker);
    assert!(spawned, "the spawn itself lost {marker}: {lines:?}");
    assert!(
        respawned,
        "the respawn dropped the pane's arguments: {lines:?}"
    );
}

/// A respawned pane is the size of its tile. `ctl respawn` used to start the
/// replacement at the terminal's size, so its emulator was wider and taller than
/// the tile it is drawn into: text was cut off at the tile's right edge and the
/// bottom rows (claude's input box) were never shown, until a manual terminal
/// resize re-tiled the window. The probe is the symptom itself: the restarted
/// worker echoes a line wider than its tile. On a tile-sized emulator it wraps
/// and the tail is visible; on a terminal-sized one it stays on one row and the
/// tail lies beyond the tile's right edge, never drawn. The tail is spelled with
/// the shell's own quoting so the typed command never contains it literally.
#[test]
fn ctl_respawn_sizes_the_pane_to_its_tile() {
    let (mut p, shell, flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(
        format!("\"{atrium}\" ctl spawn --here --role keeper -- {shell} {flag}\r\n").as_bytes(),
    )
    .unwrap();
    let out = read_until(&mut p, b"\"role\":\"keeper\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true"),
        "spawn failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    // The replacement takes the next agent id after the worker's.
    let visible = String::from_utf8_lossy(&strip_csi_bytes(&out)).into_owned();
    let worker: u32 = visible
        .split("\"pane\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("no pane id in the spawn reply: {visible:?}"));
    let respawned = format!("\"pane\":{}", worker + 1);
    // `--here` focuses the fresh worker (on the right); the lead on the left
    // does the restarting, so it is the lead that must be typed into.
    p.write(b"\x01h").unwrap();
    p.write(format!("\"{atrium}\" ctl respawn keeper\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, respawned.as_bytes(), Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, respawned.as_bytes()),
        "respawn failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    // Back to the worker's slot, now holding the restarted shell. 60 filler
    // cells push the tail past a half-width tile's 48 columns but keep the
    // echoed line inside the terminal's 100, so only a wrap can show it.
    p.write(b"\x01l").unwrap();
    let filler = "A".repeat(60);
    let (typed, marker): (&str, &[u8]) = if cfg!(windows) {
        ("WRAP^^TAIL9", b"WRAP^TAIL9") // cmd: ^^ escapes to a single ^
    } else {
        // sh: the empty "" vanishes on output. (Not inside single quotes,
        // where it would print literally and the marker could never appear.)
        ("WRAP\"\"TAIL9", b"WRAPTAIL9")
    };
    p.write(format!("echo {filler}{typed}\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, marker, Duration::from_secs(20));
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    assert!(
        contains(&out, marker),
        "the respawned pane is wider than its tile (the echoed line did not wrap): {:?}",
        String::from_utf8_lossy(&out)
    );
}

/// A pane that draws everything while its window is in the background shows
/// its content when that window is switched to — not the "starting…" spinner.
/// Only the active window's drain used to mark a pane painted; a pane restarted
/// in a background tiled window came up at its prompt, went quiet, and was
/// drawn as still starting (spinner animating) until it wrote again. Layout:
/// window 1 is the lead; window 2 holds two shells (tiled). The lead restarts
/// one of them while window 2 is in the background, then switches over.
///
/// Fails before the fix on unix only: on Windows the switch's same-size pty
/// resize makes ConPTY repaint the pane, and that fresh output marked it
/// painted through the active drain. A shell on unix writes nothing for a
/// same-size resize, so there the spinner stayed.
#[test]
fn a_pane_painted_in_the_background_shows_its_content_when_switched_to() {
    let (mut p, shell, flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    p.write(format!("\"{atrium}\" ctl spawn --role alpha -- {shell} {flag}\r\n").as_bytes())
        .unwrap();
    read_until(&mut p, two, Duration::from_secs(20)); // window 2 opened
    p.write(b"\x012").unwrap(); // watch window 2, focus on alpha
    let _ = read_until(&mut p, b"\x00never", Duration::from_millis(1500));
    p.write(format!("\"{atrium}\" ctl spawn --here --role beta -- {shell} {flag}\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"role\":\"beta\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true"),
        "tiling window 2 failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    let visible = String::from_utf8_lossy(&strip_csi_bytes(&out)).into_owned();
    let beta: u32 = visible
        .rsplit("\"pane\":")
        .next()
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("no pane id in the spawn reply: {visible:?}"));
    // Let beta's shell paint its prompt, then leave window 2 in the background.
    let _ = read_until(&mut p, b"\x00never", Duration::from_secs(2));
    p.write(b"\x011").unwrap();
    let _ = read_until(&mut p, b"\x00never", Duration::from_millis(1500));
    // Restart beta from the lead: its new shell draws while window 2 is hidden.
    let respawned = format!("\"pane\":{}", beta + 1);
    p.write(format!("\"{atrium}\" ctl respawn beta\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, respawned.as_bytes(), Duration::from_secs(20));
    assert!(
        contains(&out, respawned.as_bytes()),
        "respawn failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    let _ = read_until(&mut p, b"\x00never", Duration::from_secs(3));
    // Switch over and watch: the spinner must not be drawn at any point.
    p.write(b"\x012").unwrap();
    let out = read_until(&mut p, b"starting", Duration::from_secs(4));
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    assert!(
        !contains(&out, b"starting"),
        "a pane painted in the background is drawn as still starting: {:?}",
        String::from_utf8_lossy(&out)
    );
}

/// A pane can restart itself: `ctl respawn` aimed at the caller's own pane id
/// kills the process that asked and launches a new one in its place. This is how
/// a lead clears its context mid-run, so the request must not wedge or take the
/// session down when its sender disappears before the reply.
#[test]
fn a_pane_can_respawn_itself() {
    let log = std::env::temp_dir().join(format!(
        "atrium-self-respawn-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (shell, flag, own_id): (&str, &str, &str) = if cfg!(windows) {
        ("cmd", "/Q", "%ATRIUM_PANE%")
    } else {
        ("sh", "-i", "$ATRIUM_PANE")
    };
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &["--allow-ctl", shell, flag],
        24,
        100,
        &[
            ("ATRIUM_CTL_ALLOW".to_string(), shell.to_string()),
            (
                "ATRIUM_SPAWN_LOG".to_string(),
                log.to_string_lossy().into_owned(),
            ),
        ],
        None,
    )
    .unwrap();
    let bar: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    read_until(&mut p, bar, Duration::from_secs(15));
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(format!("\"{atrium}\" ctl respawn {own_id}\r\n").as_bytes())
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let launches = loop {
        let n = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();
        if n >= 2 || std::time::Instant::now() > deadline {
            break n;
        }
        let _ = read_until(&mut p, b"\x00never", Duration::from_millis(200));
    };
    let _ = std::fs::remove_file(&log);
    assert!(launches >= 2, "the pane did not restart itself");
    // The new process is shown, not just started: what it prints reaches the
    // screen. The typed command splits the word, so only its output can match.
    let echo = if cfg!(windows) {
        "echo re^spawned-shows\r\n"
    } else {
        "echo re''spawned-shows\r\n"
    };
    p.write(echo.as_bytes()).unwrap();
    let out = read_until(&mut p, b"respawned-shows", Duration::from_secs(15));
    let shown = contains(&out, b"respawned-shows");
    p.write(b"\x01q").unwrap();
    // Panics if atrium does not quit after a self-respawn.
    let _ = wait_exit(&mut p, 15);
    assert!(
        shown,
        "the restarted pane's output never reached the screen: {:?}",
        String::from_utf8_lossy(&out)
    );
}

// --- end to end: the ctl control channel (C3) -------------------------------

/// `atrium ctl kill <role>` tears down the worker over the live channel: spawn a
/// worker in a new window, then kill it by role — the reply carries the
/// `killed` set (the torn-down subtree), proving `kill` ran end to end and the
/// reap path took the worker's window down.
#[test]
fn ctl_kill_tears_down_a_worker() {
    let (mut p, shell, _flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    p.write(format!("\"{atrium}\" ctl spawn --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    read_until(&mut p, two, Duration::from_secs(20)); // worker window opened
    p.write(format!("\"{atrium}\" ctl kill dev_1\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"killed\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"killed\""),
        "no ctl kill reply with a torn-down set: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `atrium ctl audit` returns the recorded control-request log. After a spawn the
/// log holds a `spawn` entry; the operator (the initial root pane) sees it —
/// proving requests are recorded and the log is readable live over the channel.
#[test]
fn ctl_audit_records_requests() {
    let (mut p, shell, _flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    p.write(format!("\"{atrium}\" ctl spawn --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    read_until(&mut p, two, Duration::from_secs(20)); // spawn recorded
    p.write(format!("\"{atrium}\" ctl audit\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"audit\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"action\":\"spawn\""),
        "audit log missing the spawn entry: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// A plain `bus pub` on a topic the lead subscribed to is typed into the lead's
/// pane — by a depth-1 worker, outside its own subtree. The bus used to be pull
/// only: a finished worker's announcement sat unseen until the lead happened to
/// run `bus feed`. The lead is a shell (unbound), so the wake lands after the
/// unbound grace and is echoed by the shell, where the marker is read. The
/// marker is spelled with the worker shell's quoting so the typed command's
/// echo never contains it; only the delivered wake does.
#[test]
fn a_publish_on_a_subscribed_topic_wakes_the_subscriber_outside_the_publishers_subtree() {
    let (mut p, shell, flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(format!("\"{atrium}\" ctl bus --json sub work\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"subscribed\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true"),
        "sub failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(format!("\"{atrium}\" ctl spawn --here --role w -- {shell} {flag}\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"role\":\"w\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true"),
        "spawn failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    // `--here` focuses the worker: it publishes, unrouted, from depth 1.
    let typed: &str = if cfg!(windows) {
        "WAKE^^MARK1"
    } else {
        "'WAKE\"\"MARK1'"
    };
    p.write(format!("\"{atrium}\" ctl bus --json pub work --new msg={typed}\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"seq\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true"),
        "pub failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    // Back to the lead: the wake is typed there, framed and attributed.
    p.write(b"\x01h").unwrap();
    let out = read_until(&mut p, b"input]", Duration::from_secs(20));
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    assert!(
        contains(&out, b"teammate") && contains(&out, b"input]"),
        "the subscriber was not woken: {:?}",
        String::from_utf8_lossy(&strip_csi_bytes(&out))
    );
}

/// A worker's `--to` hand-off up to the pane that spawned it wakes that pane.
/// The subtree rule used to drop it silently: the lead is not in the worker's
/// subtree, so the most common hand-off of all never landed.
#[test]
fn a_workers_to_wakes_the_pane_that_spawned_it() {
    let (mut p, shell, flag) = spawn_atrium_ctl_shell();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(format!("\"{atrium}\" ctl spawn --here --role w -- {shell} {flag}\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"role\":\"w\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true"),
        "spawn failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    // The lead is agent 0; the worker addresses it by id, nobody subscribed.
    let typed: &str = if cfg!(windows) {
        "WAKE^^MARK2"
    } else {
        "'WAKE\"\"MARK2'"
    };
    p.write(
        format!("\"{atrium}\" ctl bus --json pub work --new --to 0 msg={typed}\r\n").as_bytes(),
    )
    .unwrap();
    let out = read_until(&mut p, b"\"seq\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true"),
        "pub failed: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01h").unwrap();
    let out = read_until(&mut p, b"input]", Duration::from_secs(20));
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    assert!(
        contains(&out, b"teammate") && contains(&out, b"input]"),
        "the ancestor was not woken: {:?}",
        String::from_utf8_lossy(&strip_csi_bytes(&out))
    );
}
