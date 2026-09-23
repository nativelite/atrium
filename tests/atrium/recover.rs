//! The session store: what is saved, offered and closed.

use crate::support::*;
use std::time::{Duration, Instant};

// --- session store: what a crash leaves behind ------------------------------

/// Spawn an atrium shell in its own project and its own state directory, so the
/// test owns every session file it looks at (no other e2e test prunes them).
fn spawn_atrium_in(project: &std::path::Path, state: &std::path::Path) -> pty::Pty {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let env = vec![(
        "ATRIUM_STATE_DIR".to_string(),
        state.to_string_lossy().into_owned(),
    )];
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &argv,
        24,
        80,
        &env,
        Some(&project.to_string_lossy()),
    )
    .unwrap();
    let bar: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    read_until(&mut p, bar, Duration::from_secs(15));
    p
}

/// This atrium's snapshot, once it has written one.
fn session_of(
    project: &std::path::Path,
    state: &std::path::Path,
    pid: u32,
    within: Duration,
) -> atrium::session::Snapshot {
    let dir =
        atrium::session_store::project_dir(state, &atrium::session_store::project_id(project));
    let end = Instant::now() + within;
    loop {
        let found = std::fs::read_dir(&dir).ok().and_then(|entries| {
            entries.flatten().map(|e| e.path()).find(|path| {
                path.extension().and_then(|x| x.to_str()) == Some("json")
                    && atrium::session_store::pid_from_name(path) == Some(pid)
            })
        });
        if let Some(snap) = found.and_then(|path| atrium::session::load(&path).ok()) {
            return snap;
        }
        assert!(
            Instant::now() < end,
            "atrium {pid} wrote no session snapshot"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn scratch(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = std::env::temp_dir().join(format!("atrium-e2e-store-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let project = base.join("project");
    let state = base.join("state");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    (project, state)
}

/// A quit is a decision: the session is marked closed and the next launch does
/// not offer it. A killed atrium never reaches its teardown, so its session stays
/// open — which is what makes a crash offerable.
#[test]
fn a_quit_closes_the_session_and_a_kill_leaves_it_offerable() {
    let (project, state) = scratch("quit-kill");

    let mut quit = spawn_atrium_in(&project, &state);
    let pid = quit.pid();
    assert!(
        !session_of(&project, &state, pid, Duration::from_secs(15))
            .meta
            .clean
    );
    quit.write(b"\x01q").unwrap();
    wait_exit(&mut quit, 15);
    assert!(
        session_of(&project, &state, pid, Duration::from_secs(5))
            .meta
            .clean,
        "a deliberate quit must mark the session closed"
    );

    let mut killed = spawn_atrium_in(&project, &state);
    let kpid = killed.pid();
    let _ = session_of(&project, &state, kpid, Duration::from_secs(15));
    killed.kill().unwrap();
    let _ = wait_exit(&mut killed, 15);
    assert!(
        !session_of(&project, &state, kpid, Duration::from_secs(5))
            .meta
            .clean,
        "a killed session must stay offerable"
    );
    let _ = std::fs::remove_dir_all(project.parent().unwrap());
}

/// On unix a closed terminal window (SIGHUP) or a shutdown (SIGTERM) exits
/// through atrium's normal teardown — the one a quit uses. It must still not be
/// recorded as a decision to end the session, or every reboot would be forgotten.
#[cfg(unix)]
#[test]
fn a_termination_signal_leaves_the_session_offerable() {
    let (project, state) = scratch("sigterm");
    let mut p = spawn_atrium_in(&project, &state);
    let pid = p.pid();
    let _ = session_of(&project, &state, pid, Duration::from_secs(15));
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let _ = wait_exit(&mut p, 15);
    assert!(
        !session_of(&project, &state, pid, Duration::from_secs(5))
            .meta
            .clean,
        "a signal is not the operator quitting"
    );
    let _ = std::fs::remove_dir_all(project.parent().unwrap());
}

/// The launch-time offer, end to end in a real pty: a crashed session in this
/// project is shown with what it would run, and declining it starts a fresh
/// session and marks the old one closed so the next launch does not ask again.
#[test]
fn a_crashed_session_is_offered_on_launch_and_declining_closes_it() {
    let (project, state) = scratch("offer");
    let project_id = atrium::session_store::project_id(&project);
    // A pid no process can hold, and a heartbeat long stale: a crash.
    let dead_pid = 3_999_999_999u32;
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let mut snap = atrium::session::capture(
        vec![0],
        0,
        vec![atrium::session::PaneCapture {
            id: 0,
            role: Some("crashed-lead".to_string()),
            argv: vec![shell.to_string()],
            cwd: None,
            identity: None,
            session_id: None,
            worktree: None,
            deny: vec!["git push".to_string()],
            can_spawn: false,
            depth: 0,
            parent_pane: None,
            mode: None,
            kickoff: false,
            norms: None,
            context_env: Vec::new(),
        }],
        None,
    );
    snap.meta = atrium::session::SessionMeta {
        project: Some(project_id.clone()),
        pid: Some(dead_pid),
        saved_at_ms: Some(1_000),
        clean: false,
    };
    let planted = atrium::session_store::session_file(&state, &project_id, dead_pid, 1);
    atrium::session_store::ensure_private_dir(planted.parent().unwrap()).unwrap();
    atrium::session::save(&planted, &snap).unwrap();

    let (sh, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![sh];
    argv.extend(args);
    // The suite turns the offer off (.cargo/config.toml); this test is the one
    // place it is turned back on.
    let env = vec![
        (
            "ATRIUM_STATE_DIR".to_string(),
            state.to_string_lossy().into_owned(),
        ),
        ("ATRIUM_RESUME_OFFER".to_string(), "1".to_string()),
    ];
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &argv,
        24,
        120,
        &env,
        Some(&project.to_string_lossy()),
    )
    .unwrap();
    let asked = read_until(&mut p, b"Resume it?", Duration::from_secs(15));
    let shown = String::from_utf8_lossy(&asked).into_owned();
    assert!(shown.contains("Resume it?"), "no resume offer: {shown:?}");
    assert!(
        shown.contains("[crashed-lead]") && shown.contains("cannot spawn"),
        "the offer must show what each pane would run and may do: {shown:?}"
    );
    p.write(b"n\r").unwrap();
    let bar: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    read_until(&mut p, bar, Duration::from_secs(15));
    p.write(b"\x01q").unwrap();
    wait_exit(&mut p, 15);
    assert!(
        atrium::session::load(&planted).unwrap().meta.clean,
        "declining must mark the crashed session closed"
    );
    let _ = std::fs::remove_dir_all(project.parent().unwrap());
}

/// The team's bus lives beside the session's snapshot, so what a resume brings
/// back includes subscriptions and unread events — before, they lived only in the
/// atrium process and died with it.
#[test]
fn a_sessions_bus_is_saved_beside_its_snapshot() {
    let (project, state) = scratch("bus");
    let (shell, flag): (&str, &str) = if cfg!(windows) {
        ("cmd", "/Q")
    } else {
        ("sh", "-i")
    };
    let env = vec![(
        "ATRIUM_STATE_DIR".to_string(),
        state.to_string_lossy().into_owned(),
    )];
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &["--allow-ctl", shell, flag],
        24,
        120,
        &env,
        Some(&project.to_string_lossy()),
    )
    .unwrap();
    let bar: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    read_until(&mut p, bar, Duration::from_secs(15));
    let pid = p.pid();
    let atrium = env!("CARGO_BIN_EXE_atrium");
    p.write(format!("\"{atrium}\" ctl bus sub resume-probe-topic\r\n").as_bytes())
        .unwrap();
    let out = read_until(
        &mut p,
        b"subscribed: resume-probe-topic",
        Duration::from_secs(20),
    );
    assert!(
        contains(&out, b"subscribed: resume-probe-topic"),
        "no bus subscribe reply: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    wait_exit(&mut p, 15);
    let dir =
        atrium::session_store::project_dir(&state, &atrium::session_store::project_id(&project));
    let snapshot = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|path| {
            !atrium::session_store::is_sidecar(path)
                && atrium::session_store::pid_from_name(path) == Some(pid)
        })
        .expect("the session wrote a snapshot");
    let bus = std::fs::read_to_string(atrium::session_store::sidecar(&snapshot, "bus"))
        .expect("the bus is saved beside the snapshot");
    assert!(
        bus.contains("resume-probe-topic"),
        "the subscription must be in the saved bus: {bus}"
    );
    let _ = std::fs::remove_dir_all(project.parent().unwrap());
}
