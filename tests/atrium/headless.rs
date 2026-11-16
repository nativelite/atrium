//! `atrium fleet up <name> --headless`: a session with no terminal on stdin or
//! stdout, read through its ctl endpoint, ending on its last pane's exit or a
//! termination signal through the same teardown an attached session runs.
//!
//! Unlike the other areas, atrium is NOT spawned in a pty here: that is the
//! point. stdin is the null device, stdout and stderr are files the test reads
//! afterwards, and on unix atrium is put in a session of its own first
//! (`setsid`), so it has no controlling terminal either, as under a job runner.

use crate::support::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A two-agent fleet in a temp directory. Each agent is a script that says it
/// is up (a file `up-<name>`), then waits until the test creates
/// `stop-<name>` and exits: so the test decides when each pane ends.
struct Lab {
    dir: PathBuf,
    child: Option<Child>,
}

impl Lab {
    /// `fleet_keys` is a JSON fragment of top-level fleet keys (with its
    /// trailing comma), such as `"allow_ctl": true,`.
    fn new(tag: &str, fleet_keys: &str) -> Lab {
        let dir =
            std::env::temp_dir().join(format!("atrium-headless-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let agent = |name: &str| -> String {
            let argv: Vec<String> = if cfg!(windows) {
                let script = dir.join("wait.cmd");
                vec![
                    "cmd".into(),
                    "/Q".into(),
                    "/D".into(),
                    "/C".into(),
                    script.to_string_lossy().into_owned(),
                    name.into(),
                ]
            } else {
                let script = dir.join("wait.sh");
                vec![
                    "sh".into(),
                    script.to_string_lossy().into_owned(),
                    name.into(),
                ]
            };
            // `{:?}` of an ASCII path is a JSON string: quotes, `\\` for `\`.
            let cmd: Vec<String> = argv.iter().map(|a| format!("{a:?}")).collect();
            format!(r#"{{ "name": "{name}", "cmd": [{}] }}"#, cmd.join(", "))
        };
        if cfg!(windows) {
            std::fs::write(
                dir.join("wait.cmd"),
                "@echo off\r\n\
                 echo up> \"%~dp0up-%1\"\r\n\
                 :loop\r\n\
                 if exist \"%~dp0stop-%1\" exit /b 0\r\n\
                 ping -n 2 127.0.0.1 >nul\r\n\
                 goto loop\r\n",
            )
            .unwrap();
        } else {
            std::fs::write(
                dir.join("wait.sh"),
                "d=$(dirname \"$0\")\n\
                 echo $$ > \"$d/up-$1\"\n\
                 while [ ! -e \"$d/stop-$1\" ]; do sleep 0.2; done\n",
            )
            .unwrap();
        }
        let fleet = format!(
            r#"{{ "fleets": {{ "test": {{ {fleet_keys} "grid": "1x2", "agents": [
              {}, {}
            ] }} }} }}"#,
            agent("one"),
            agent("two"),
        );
        std::fs::write(dir.join("atrium.fleet.json"), fleet).unwrap();
        Lab { dir, child: None }
    }

    /// The fleet launch, not yet started: no terminal on any of stdin, stdout
    /// or stderr, the developer's own config and session store out of reach,
    /// and nothing inherited that would make this look like a pane of an
    /// atrium the test run happens to be inside.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_atrium"));
        cmd.args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(self.dir.join("stdout.log")).unwrap())
            .stderr(std::fs::File::create(self.dir.join("stderr.log")).unwrap())
            .env("ATRIUM_STATE_DIR", self.dir.join("state"))
            .env_remove("ATRIUM_CTL")
            .env_remove("ATRIUM_PANE")
            .env_remove("ATRIUM_TOKEN");
        for (k, v) in hermetic_env(&self.dir) {
            cmd.env(k, v);
        }
        cmd
    }

    /// Start `atrium fleet up test --headless …` and keep it for the drop.
    fn start(&mut self, cmd: Command) -> u32 {
        self.start_in(cmd, false)
    }

    /// [`start`](Self::start); `own_console` (Windows) gives atrium a console
    /// of its own with no window, for a test run that has none to share.
    fn start_in(&mut self, mut cmd: Command, own_console: bool) -> u32 {
        own_session(&mut cmd, own_console);
        let child = cmd.spawn().unwrap();
        let pid = child.id();
        self.child = Some(child);
        pid
    }

    fn stdout(&self) -> Vec<u8> {
        std::fs::read(self.dir.join("stdout.log")).unwrap_or_default()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.dir.join("stderr.log")).unwrap_or_default())
            .into_owned()
    }

    fn touch(&self, name: &str) {
        std::fs::write(self.dir.join(name), b"").unwrap();
    }

    /// Wait for atrium to exit by itself and return its code; a session that
    /// outstays `secs` fails the test (and is killed by the drop).
    fn wait_exit(&mut self, secs: u64) -> i32 {
        let end = Instant::now() + Duration::from_secs(secs);
        let child = self.child.as_mut().unwrap();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            assert!(
                Instant::now() < end,
                "the headless session did not end in {secs} s; stderr: {}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn running(&mut self) -> bool {
        matches!(self.child.as_mut().unwrap().try_wait(), Ok(None))
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// On unix, give atrium a session of its own, and so no controlling terminal:
/// what a job runner or a service manager starts a process with. An open of
/// `/dev/tty` then fails, so a session that comes up here opened none.
#[cfg(unix)]
fn own_session(cmd: &mut Command, _own_console: bool) {
    use std::os::unix::process::CommandExt;
    extern "C" {
        fn setsid() -> i32;
    }
    // SAFETY: `setsid` is async-signal-safe and touches nothing the parent
    // shares; it runs in the forked child before exec.
    unsafe {
        cmd.pre_exec(|| {
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// On Windows, a process group of its own, so a Ctrl+Break can be sent to
/// atrium alone (`GenerateConsoleCtrlEvent` addresses a group).
#[cfg(windows)]
fn own_session(cmd: &mut Command, own_console: bool) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(if own_console {
        CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    } else {
        CREATE_NEW_PROCESS_GROUP
    });
}

/// `atrium ctl <args> --json` against the session of atrium `pid`, from
/// outside any pane: no token, so reads only. `(succeeded, stdout)`.
fn ctl(pid: u32, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_atrium"))
        .arg("ctl")
        .args(args)
        .arg("--json")
        .env("ATRIUM_CTL", atrium::ipc::address_for(pid))
        .env_remove("ATRIUM_PANE")
        .env_remove("ATRIUM_TOKEN")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Poll `ctl list` until `want` holds of its reply, or fail with the last one.
fn list_until(lab: &mut Lab, pid: u32, what: &str, want: impl Fn(&str) -> bool) -> String {
    let end = Instant::now() + Duration::from_secs(30);
    let mut last = String::new();
    while Instant::now() < end {
        let (ok, reply) = ctl(pid, &["list"]);
        if ok && want(&reply) {
            return reply;
        }
        last = reply;
        assert!(
            lab.running(),
            "atrium exited while waiting for {what}; stderr: {}",
            lab.stderr()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "ctl list never showed {what}; last reply: {last:?}; stderr: {}",
        lab.stderr()
    );
}

/// Whether the session of atrium `pid` no longer has an endpoint: one probe
/// that connects nothing (a client's request would wait out its whole
/// deadline for a name that is gone). On unix this is the socket file itself.
fn endpoint_gone(pid: u32) -> bool {
    !nlipc::endpoint_exists(&atrium::ipc::address_for(pid))
}

fn wait_for_file(path: &Path, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn has_role(reply: &str, role: &str) -> bool {
    reply.contains(&format!("\"role\":\"{role}\""))
}

/// The done-signal: a fleet comes up with no terminal anywhere, both panes
/// run, `ctl list` and `ctl status` answer through the endpoint, the session
/// outlives its first pane, and it ends by itself, cleanly, when the last one
/// exits. Nothing was ever written to stdout, and the endpoint is gone.
#[test]
fn a_headless_fleet_runs_with_no_terminal_and_ends_on_its_last_panes_exit() {
    let mut lab = Lab::new("run", r#""allow_ctl": true,"#);
    let cmd = lab.command(&["fleet", "up", "test", "--headless", "--size", "40x120"]);
    let pid = lab.start(cmd);

    // Both agents are in the tree, and both programs really ran.
    let reply = list_until(&mut lab, pid, "both panes", |r| {
        has_role(r, "one") && has_role(r, "two")
    });
    assert!(reply.contains("\"ok\":true"), "{reply}");
    for name in ["up-one", "up-two"] {
        assert!(
            wait_for_file(&lab.dir.join(name), 20),
            "the pane behind {name} never ran; stderr: {}",
            lab.stderr()
        );
    }
    let (ok, status) = ctl(pid, &["status"]);
    assert!(ok && status.contains("\"ok\":true"), "ctl status: {status}");

    // One pane exits: the session goes on with the other.
    lab.touch("stop-one");
    list_until(&mut lab, pid, "pane one gone", |r| {
        !has_role(r, "one") && has_role(r, "two")
    });
    assert!(lab.running(), "the session must outlive its first pane");

    // The last pane exits: the session ends by itself, as a success.
    lab.touch("stop-two");
    assert_eq!(lab.wait_exit(30), 0, "stderr: {}", lab.stderr());

    // No frame, title or mode switch ever reached stdout, a file here.
    assert!(
        lab.stdout().is_empty(),
        "a headless session wrote to stdout: {:?}",
        String::from_utf8_lossy(&lab.stdout())
    );
    // stderr is the log: the banner is there, as plain text.
    let err = lab.stderr();
    assert!(err.contains("test"), "no banner on stderr: {err:?}");
    assert!(
        !err.contains('\x1b'),
        "escape sequences reached the stderr log: {err:?}"
    );
    // The endpoint went with the session (on unix, its socket file).
    assert!(endpoint_gone(pid), "the ctl endpoint outlived its session");
}

/// With nobody to reach it, a headless session is refused before anything
/// starts: the reason on stderr, a failure exit, and no pane ever ran.
#[test]
fn a_headless_fleet_without_the_control_channel_is_refused() {
    let mut lab = Lab::new("noctl", "");
    let cmd = lab.command(&["fleet", "up", "test", "--headless"]);
    lab.start(cmd);
    assert_ne!(lab.wait_exit(20), 0);
    let err = lab.stderr();
    assert!(
        err.contains("--headless needs the control channel"),
        "{err:?}"
    );
    assert!(!lab.dir.join("up-one").exists(), "a pane ran anyway");
}

/// And with nobody to press Enter, the launch must have been approved in
/// advance (`ATRIUM_YES`): its absence is a refusal with the reason, not a
/// read of stdin and not a silent yes.
#[test]
fn a_headless_fleet_nobody_approved_is_refused() {
    let mut lab = Lab::new("noyes", "");
    let mut cmd = lab.command(&["fleet", "up", "test", "--headless", "--allow-ctl"]);
    cmd.env_remove("ATRIUM_YES");
    lab.start(cmd);
    assert_ne!(lab.wait_exit(20), 0);
    let err = lab.stderr();
    assert!(err.contains("ATRIUM_YES"), "{err:?}");
    assert!(!lab.dir.join("up-one").exists(), "a pane ran anyway");
}

/// A size that cannot be is a usage error, before the fleet file is read.
#[test]
fn a_headless_size_that_cannot_be_is_a_usage_error() {
    let mut lab = Lab::new("size", r#""allow_ctl": true,"#);
    let cmd = lab.command(&["fleet", "up", "test", "--headless", "--size", "0x0"]);
    lab.start(cmd);
    assert_ne!(lab.wait_exit(20), 0);
    assert!(lab.stderr().contains("<rows>x<cols>"), "{:?}", lab.stderr());
}

/// SIGTERM ends a headless session through the teardown: a success exit, the
/// panes' processes gone, the socket file removed. (unix; not run on the
/// Windows host this was written on.)
#[cfg(unix)]
#[test]
fn sigterm_ends_a_headless_fleet_through_the_teardown() {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGTERM: i32 = 15;
    let mut lab = Lab::new("term", r#""allow_ctl": true,"#);
    let cmd = lab.command(&["fleet", "up", "test", "--headless"]);
    let pid = lab.start(cmd);
    list_until(&mut lab, pid, "both panes", |r| {
        has_role(r, "one") && has_role(r, "two")
    });
    let pane_pids: Vec<i32> = ["up-one", "up-two"]
        .iter()
        .map(|name| {
            let path = lab.dir.join(name);
            assert!(wait_for_file(&path, 20), "{name} never ran");
            // The file exists before its line is complete: read until it parses.
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                if let Ok(pid) = text.trim().parse::<i32>() {
                    break pid;
                }
                assert!(Instant::now() < end, "{name} holds no pid: {text:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        })
        .collect();

    // SAFETY: a signal to a process this test started.
    assert_eq!(unsafe { kill(pid as i32, SIGTERM) }, 0);
    assert_eq!(lab.wait_exit(30), 0, "stderr: {}", lab.stderr());

    for pane in pane_pids {
        let end = Instant::now() + Duration::from_secs(10);
        // SAFETY: signal 0 only asks whether the process exists.
        while unsafe { kill(pane, 0) } == 0 {
            assert!(
                Instant::now() < end,
                "pane process {pane} outlived the session"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    assert!(endpoint_gone(pid), "the ctl socket file was left behind");
    assert!(lab.stdout().is_empty());
}

/// Ctrl+Break in a headless session's console ends it through the teardown
/// rather than where it stands: a success exit (the default handler's is
/// `STATUS_CONTROL_C_EXIT`), both panes' programs gone, the endpoint gone.
///
/// The event needs a console atrium and the sender share. A test run started
/// from a terminal has one, and atrium inherits it. A run with none (an agent
/// harness) gives atrium a windowless console of its own and attaches to it
/// for the moment of the send.
#[cfg(windows)]
#[test]
fn ctrl_break_ends_a_headless_fleet_through_the_teardown() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
        fn GetConsoleCP() -> u32;
        fn AttachConsole(pid: u32) -> i32;
        fn FreeConsole() -> i32;
    }
    const CTRL_BREAK_EVENT: u32 = 1;
    // SAFETY: no arguments; the console's code page, or 0 with no console
    // (a console need not have a window, so its window is not the question).
    let shared = unsafe { GetConsoleCP() } != 0;
    let mut lab = Lab::new("break", r#""allow_ctl": true,"#);
    let cmd = lab.command(&["fleet", "up", "test", "--headless"]);
    let pid = lab.start_in(cmd, !shared);
    list_until(&mut lab, pid, "both panes", |r| {
        has_role(r, "one") && has_role(r, "two")
    });
    for name in ["up-one", "up-two"] {
        assert!(wait_for_file(&lab.dir.join(name), 20), "{name} never ran");
    }

    if !shared {
        // SAFETY: joins the console of the process this test started.
        let attached = unsafe { AttachConsole(pid) };
        assert_ne!(
            attached,
            0,
            "could not attach to atrium's console: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: an event for the process group this test created (`own_session`),
    // which this process is not in.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    let send_error = std::io::Error::last_os_error();
    if !shared {
        // SAFETY: leaves the console attached just above.
        unsafe { FreeConsole() };
    }
    assert_ne!(sent, 0, "could not send Ctrl+Break: {send_error}");

    assert_eq!(lab.wait_exit(30), 0, "stderr: {}", lab.stderr());
    assert!(endpoint_gone(pid), "the ctl endpoint outlived its session");
    assert!(lab.stdout().is_empty());
    // The panes went with it: a waiting script would answer a stop file by
    // exiting, a dead one cannot take the marker away.
    lab.touch("stop-one");
    lab.touch("stop-two");
}
