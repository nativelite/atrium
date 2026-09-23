//! The build pool, the memory guard and the deny list, per platform.

use crate::support::*;
use std::time::{Duration, Instant};

// --- Windows tree teardown (Job Object, kill-on-close) ----------------------
//
// The Windows counterparts of the two tests above. There is no process group and
// no watchdog on Windows; the guarantee is a Job Object with
// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` that atrium holds for its whole life, so the
// kernel terminates every pane and everything a pane spawned the instant atrium's
// handle closes — however atrium exits.

/// **A pane is born pointing at a live compile pool** (the fleet OOM fix).
///
/// The pane reports its `CARGO_MAKEFLAGS`, and the test then opens the named
/// semaphore exactly as cargo's `jobserver` client does and reads its count. That
/// proves the whole chain: atrium created the pool, sized it from
/// `ATRIUM_BUILD_JOBS`, injected its name, and it is reachable from another
/// process. An inherited `CARGO_MAKEFLAGS` (this test running inside an atrium
/// pane) is blanked so the atrium under test must create its own.
#[cfg(windows)]
#[test]
fn a_pane_is_given_a_live_build_pool_windows() {
    use core::ffi::c_void;
    extern "system" {
        fn OpenSemaphoreW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
        fn CloseHandle(h: *mut c_void) -> i32;
    }
    #[link(name = "ntdll")]
    extern "system" {
        fn NtQuerySemaphore(
            sem: *mut c_void,
            class: i32,
            info: *mut c_void,
            len: u32,
            ret: *mut u32,
        ) -> i32;
    }
    const SEMAPHORE_QUERY_STATE: u32 = 0x0001;
    const SYNCHRONIZE: u32 = 0x0010_0000;

    let marker = std::env::temp_dir().join(format!("atrium-pool-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let mpath = marker.display().to_string().replace('\\', "/");
    let script = format!(
        "Set-Content -Path '{mpath}' -Value ('[' + $env:CARGO_MAKEFLAGS + ']'); \
         Start-Sleep -Seconds 600"
    );
    let argv = [
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &script,
    ];
    let env = [
        ("CARGO_MAKEFLAGS".to_string(), String::new()),
        ("ATRIUM_BUILD_JOBS".to_string(), "3".to_string()),
    ];
    let mut p =
        pty::Pty::spawn_with_env(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80, &env).unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let seen = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            if let (Some(a), Some(b)) = (t.find('['), t.rfind(']')) {
                break t[a + 1..b].to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "pane never reported CARGO_MAKEFLAGS"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let counts = atrium::buildpool::jobserver_auth(&seen).map(|auth| {
        let wide: Vec<u16> = auth.encode_utf16().chain(Some(0)).collect();
        // SAFETY: a null-terminated name; the handle is closed below.
        let sem = unsafe { OpenSemaphoreW(SEMAPHORE_QUERY_STATE | SYNCHRONIZE, 0, wide.as_ptr()) };
        let mut info = [0i32; 2];
        // SAFETY: SEMAPHORE_BASIC_INFORMATION is two LONGs; the length says so.
        let status = if sem.is_null() {
            -1
        } else {
            unsafe {
                NtQuerySemaphore(
                    sem,
                    0,
                    info.as_mut_ptr() as *mut c_void,
                    8,
                    core::ptr::null_mut(),
                )
            }
        };
        if !sem.is_null() {
            // SAFETY: closing the handle opened above.
            unsafe { CloseHandle(sem) };
        }
        (status, info)
    });
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
    let _ = std::fs::remove_file(&marker);

    let (status, [current, maximum]) =
        counts.unwrap_or_else(|| panic!("pane's CARGO_MAKEFLAGS names no jobserver: {seen:?}"));
    assert_eq!(
        status, 0,
        "the pool named in {seen:?} could not be opened by name"
    );
    assert_eq!(maximum, 3, "ATRIUM_BUILD_JOBS=3 must size the pool");
    assert_eq!(current, 3, "an idle pool holds all its tokens");
}

/// **The soft memory guard stops a pane's runaway build** (Linux).
///
/// atrium runs with `ATRIUM_MEMORY_MB=64`. The pane starts a "build" — python
/// run through a symlink named `rustc`, so its `comm` is a build image — that
/// fills 300 MB and sleeps. Within a few guard ticks it must be SIGKILLed (the
/// shell reports 137); a guard that doesn't act lets it sleep out its minute.
#[cfg(target_os = "linux")]
#[test]
fn the_soft_memory_guard_stops_a_runaway_build_linux() {
    let dir = std::env::temp_dir().join(format!("atrium-softguard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("rustc");
    std::os::unix::fs::symlink("/usr/bin/python3", &shim).unwrap();
    let marker = dir.join("exit.txt");
    let script = format!(
        "'{shim}' -c 'import time; b = b\"x\" * (300 * 1024 * 1024); time.sleep(60)'; \
         echo \"exit=$?\" > '{marker}'; sleep 600",
        shim = shim.display(),
        marker = marker.display()
    );
    let env = [("ATRIUM_MEMORY_MB".to_string(), "64".to_string())];
    let mut p = pty::Pty::spawn_with_env(
        env!("CARGO_BIN_EXE_atrium"),
        &["sh", "-c", &script],
        24,
        80,
        &env,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let seen = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            if t.contains("exit=") {
                break t.trim().to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "the runaway build was never stopped"
        );
        read_until(&mut p, b"\x00never-printed\x00", Duration::from_millis(200));
    };
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(seen, "exit=137", "the build must be SIGKILLed by the guard");
}

/// **Under a delegated scope the Linux cap is hard** (needs a systemd user
/// manager, so it only runs when asked: `cargo test --test atrium -- --ignored
/// a_delegated_scope`).
///
/// atrium is launched the documented way — `systemd-run --user --scope -p
/// Delegate=yes atrium …` — with a 64 MB cap. The pane records its own cgroup
/// and then runs a 300 MB "build". Proven, in order: the pane was moved into
/// atrium's `panes` leaf, that leaf's `memory.max` is the cap, and the build was
/// stopped.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs systemd-run --user with delegation"]
fn a_delegated_scope_holds_panes_under_a_hard_cap_linux() {
    let dir = std::env::temp_dir().join(format!("atrium-hardcap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("rustc");
    std::os::unix::fs::symlink("/usr/bin/python3", &shim).unwrap();
    let marker = dir.join("out.txt");
    let script = format!(
        "sleep 4; echo \"cg=$(cut -d: -f3 /proc/self/cgroup)\" > '{marker}'; \
         '{shim}' -c 'import time; b = b\"x\" * (300 * 1024 * 1024); time.sleep(60)'; \
         echo \"exit=$?\" >> '{marker}'; sleep 600",
        shim = shim.display(),
        marker = marker.display()
    );
    let env = [("ATRIUM_MEMORY_MB".to_string(), "64".to_string())];
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let mut p = pty::Pty::spawn_with_env(
        "systemd-run",
        &[
            "--user",
            "--scope",
            "-p",
            "Delegate=yes",
            "--quiet",
            atrium,
            "sh",
            "-c",
            &script,
        ],
        24,
        80,
        &env,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    let seen = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            if t.contains("exit=") {
                break t;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the pane never finished: {:?}",
            std::fs::read_to_string(&marker)
        );
        read_until(&mut p, b"\x00never-printed\x00", Duration::from_millis(200));
    };
    let cg = seen
        .lines()
        .find_map(|l| l.strip_prefix("cg="))
        .unwrap_or_default()
        .to_string();
    let max = std::fs::read_to_string(format!("/sys/fs/cgroup{cg}/memory.max")).unwrap_or_default();
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        cg.ends_with("/panes"),
        "the pane was not moved into the panes leaf: {cg:?}"
    );
    assert_eq!(
        max.trim(),
        (64 * 1024 * 1024).to_string(),
        "memory.max is not the cap"
    );
    assert!(
        seen.contains("exit=137"),
        "the build was not stopped: {seen}"
    );
}

/// **A claude pane is launched with the deny list** (Windows).
///
/// A fake `claude.cmd` records the argv atrium actually launched it with. It must
/// carry `--disallowedTools` with the built-in fail-safe first and the
/// session's `ATRIUM_DENY` entry normalised to a rule — proof the rules reach
/// claude, not just that the rule builder is right.
#[cfg(windows)]
#[test]
fn a_claude_pane_is_launched_with_the_deny_list_windows() {
    let dir = std::env::temp_dir().join(format!("atrium-deny-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let argv_file = dir.join("argv.txt");
    std::fs::write(
        dir.join("claude.cmd"),
        "@echo off\r\necho %* > \"%~dp0argv.txt\"\r\nping -n 600 127.0.0.1 > nul\r\n",
    )
    .unwrap();
    let claude = dir.join("claude.cmd").display().to_string();
    let env = [("ATRIUM_DENY".to_string(), "npm publish".to_string())];
    let mut p =
        pty::Pty::spawn_with_env(env!("CARGO_BIN_EXE_atrium"), &[&claude], 24, 80, &env).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let seen = loop {
        if let Ok(t) = std::fs::read_to_string(&argv_file) {
            if t.contains("disallowedTools") || t.trim().len() > 10 {
                break t;
            }
        }
        assert!(
            Instant::now() < deadline,
            "fake claude never recorded its argv"
        );
        read_until(&mut p, b"\x00never-printed\x00", Duration::from_millis(200));
    };
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
    let _ = std::fs::remove_dir_all(&dir);

    let flag = seen
        .find("--disallowedTools")
        .unwrap_or_else(|| panic!("no deny flag: {seen}"));
    let builtin = seen
        .find("Bash(*CARGO_MAKEFLAGS*)")
        .unwrap_or_else(|| panic!("{seen}"));
    let session = seen
        .find("Bash(npm publish*)")
        .unwrap_or_else(|| panic!("{seen}"));
    assert!(flag < builtin && builtin < session, "order: {seen}");
}

/// **The memory guard holds a real pane under its ceiling** (Windows).
///
/// atrium runs with `ATRIUM_MEMORY_MB=400`; the pane waits for the guard's first
/// tick, then tries to commit 800 MB and records whether it could. The same
/// allocation succeeds outside a limited job (`reap`'s unit test is the control),
/// so a `FAIL` here is the guard's limit reaching the pane, not PowerShell.
#[cfg(windows)]
#[test]
fn the_memory_guard_holds_a_pane_under_its_ceiling_windows() {
    let marker = std::env::temp_dir().join(format!("atrium-memguard-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let mpath = marker.display().to_string().replace('\\', "/");
    // 7 s covers lazy job assignment plus the guard's 3 s cadence.
    let script = format!(
        "Start-Sleep -Seconds 7; \
         try {{ $a = [byte[]]::new(800MB); $a[799MB] = 1; $r = 'OK' }} catch {{ $r = 'FAIL' }}; \
         Set-Content -Path '{mpath}' -Value $r; Start-Sleep -Seconds 600"
    );
    let argv = [
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &script,
    ];
    let env = [("ATRIUM_MEMORY_MB".to_string(), "400".to_string())];
    let mut p =
        pty::Pty::spawn_with_env(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80, &env).unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    let seen = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                break t;
            }
        }
        assert!(
            Instant::now() < deadline,
            "pane never reported its allocation"
        );
        // Keep reading atrium's screen while waiting. An undrained pty fills, the
        // next frame write blocks, and the whole run loop — guard included —
        // stalls until someone reads (measured: 6.9 s frozen in the resize phase).
        read_until(&mut p, b"\x00never-printed\x00", Duration::from_millis(200));
    };
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
    let _ = std::fs::remove_file(&marker);
    assert_eq!(seen, "FAIL", "an 800 MB allocation got past a 400 MB guard");
}

/// **The memory guard keeps working while atrium's screen is stuck** (Windows).
///
/// Nothing reads atrium's terminal here — a stopped terminal, or a console with
/// a QuickEdit selection held. Its frame writes block, and the run loop blocks
/// with them. The guard must not: the pane still has to run under its ceiling.
#[cfg(windows)]
#[test]
fn the_memory_guard_holds_while_the_screen_is_not_being_read_windows() {
    let marker = std::env::temp_dir().join(format!("atrium-stuck-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let mpath = marker.display().to_string().replace('\\', "/");
    // The pane prints nothing (a pane that prints would block on its own output
    // once atrium stops reading it); atrium's own frames are what back up. It
    // waits out the guard's first tick, then tries to commit 800 MB.
    let script = format!(
        "Start-Sleep -Seconds 7; \
         try {{ $a = [byte[]]::new(800MB); $a[799MB] = 1; $r = 'OK' }} catch {{ $r = 'FAIL' }}; \
         Set-Content -Path '{mpath}' -Value $r; Start-Sleep -Seconds 600"
    );
    let argv = [
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &script,
    ];
    let env = [("ATRIUM_MEMORY_MB".to_string(), "400".to_string())];
    let mut p =
        pty::Pty::spawn_with_env(env!("CARGO_BIN_EXE_atrium"), &argv, 24, 80, &env).unwrap();
    // Deliberately NOT reading atrium's output while the pane works.
    let deadline = Instant::now() + Duration::from_secs(40);
    let seen = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                break Some(t);
            }
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 30), 0);
    let _ = std::fs::remove_file(&marker);
    let seen = seen.expect("pane never reported its allocation");
    assert_eq!(
        seen, "FAIL",
        "an 800 MB allocation got past a 400 MB guard while the screen was stuck"
    );
}
