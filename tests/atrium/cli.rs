//! Commands that answer without a session: `config`, and help everywhere.

/// `atrium config init --at <path>` writes the starter there and remembers it in
/// the platform place's pointer; `atrium config path` then reports that path as
/// named by the pointer; a second `init` refuses to overwrite. The platform
/// place is redirected to a temp dir, so nothing touches the developer's own.
#[test]
fn config_init_writes_the_starter_and_the_pointer_and_path_reports_it() {
    let base = std::env::temp_dir().join(format!("atrium-config-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let platform = base.join("platform");
    let target = base.join("dots").join("atrium.json");
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_atrium"))
            .args(args)
            .env("APPDATA", &platform)
            .env("XDG_CONFIG_HOME", &platform)
            .env("ATRIUM_YES", "1")
            .env_remove("ATRIUM_CONFIG")
            .output()
            .unwrap()
    };
    let out = run(&["config", "init", "--at", &target.to_string_lossy()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&target).unwrap();
    assert!(text.contains("\"claude_aliases\""), "a starter: {text}");
    let pointer = platform.join("atrium").join("config.path");
    assert_eq!(
        std::fs::read_to_string(&pointer).unwrap().trim(),
        target.display().to_string()
    );

    let out = run(&["config", "path"]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains(&target.display().to_string()) && stdout.contains("present"),
        "{stdout}"
    );
    assert!(stdout.contains(&pointer.display().to_string()), "{stdout}");

    let out = run(&["config", "init", "--at", &target.to_string_lossy()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already exists"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Every family answers `--help` and `-h` on stdout with exit 0 and no
/// session: `atrium ctl --help` used to fail before parsing because it
/// demanded ATRIUM_CTL, `recover --help` was "unexpected argument", and the
/// top-level help went to stderr.
#[test]
fn every_family_answers_help_on_stdout_without_a_session() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["ctl", "--help"],
        vec!["ctl", "-h"],
        vec!["fleet", "--help"],
        vec!["config", "-h"],
        vec!["recover", "--help"],
        vec!["reap", "--help"],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_atrium"))
            .args(&args)
            .env_remove("ATRIUM_CTL")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("usage:"), "{args:?}: {stdout}");
        assert!(
            out.stderr.is_empty(),
            "{args:?} wrote to stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // A bare `atrium ctl` is a usage error that still shows the commands.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_atrium"))
        .arg("ctl")
        .env_remove("ATRIUM_CTL")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("respawn"));
}

/// `atrium mod install --at <dir>` writes the plugin built into the binary, a
/// second install changes nothing, a hand edit is restored, and `mod status`
/// reports it through ATRIUM_MOD; with nothing installed it says so and names
/// the default folder. The platform place is redirected, so nothing touches
/// the developer's own.
#[test]
fn mod_install_writes_the_plugin_and_status_reports_it() {
    let base = std::env::temp_dir().join(format!("atrium-mod-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let platform = base.join("platform");
    let dir = base.join("m");
    let run = |args: &[&str], at: Option<&std::path::Path>| {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_atrium"));
        c.args(args)
            .env("APPDATA", &platform)
            .env("XDG_CONFIG_HOME", &platform)
            .env("ATRIUM_YES", "1")
            .env_remove("ATRIUM_CONFIG")
            .env_remove("ATRIUM_MOD");
        if let Some(d) = at {
            c.env("ATRIUM_MOD", d);
        }
        c.output().unwrap()
    };
    let text = |o: &std::process::Output| String::from_utf8_lossy(&o.stdout).into_owned();

    let out = run(&["mod", "status"], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let default_dir = platform.join("atrium").join("mod");
    assert!(
        text(&out).contains("not installed")
            && text(&out).contains(&default_dir.display().to_string()),
        "{}",
        text(&out)
    );

    let out = run(&["mod", "install", "--at", &dir.to_string_lossy()], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text(&out).contains("0 unchanged"), "{}", text(&out));
    assert!(dir.join(".claude-plugin").join("plugin.json").is_file());
    assert!(dir.join("hooks").join("hooks.json").is_file());
    assert!(dir.join("hooks").join("register.ts").is_file());

    let out = run(&["mod", "install", "--at", &dir.to_string_lossy()], None);
    assert!(
        text(&out).contains("0 written"),
        "idempotent: {}",
        text(&out)
    );

    std::fs::write(dir.join("hooks").join("status.ts"), "edited").unwrap();
    let out = run(&["mod", "install", "--at", &dir.to_string_lossy()], None);
    assert!(text(&out).contains("1 written"), "restored: {}", text(&out));

    let out = run(&["mod", "status"], Some(&dir));
    assert!(
        text(&out).contains("mod: installed ") && text(&out).contains(&dir.display().to_string()),
        "{}",
        text(&out)
    );
    let out = run(&["mod", "--help"], None);
    assert!(out.status.success() && text(&out).contains("usage: atrium mod"));
    let _ = std::fs::remove_dir_all(&base);
}

/// A claude pane is spawned with CLAUDE_CODE_PLUGIN_DIRS naming the installed
/// mod and ATRIUM_BIN naming the atrium that hosts it; a shell pane gets
/// neither. Proven without a claude: a shell script registered as a claude
/// alias (ATRIUM_CLAUDE_ALIASES) records the environment it was started with.
#[cfg(unix)]
#[test]
fn a_claude_pane_is_told_where_the_mod_is_and_which_atrium_to_call() {
    use crate::support::{read_until, wait_exit};
    use std::time::Duration;
    let base = std::env::temp_dir().join(format!("atrium-mod-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("bin")).unwrap();
    let modir = base.join("m");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_atrium"))
        .args(["mod", "install", "--at", &modir.to_string_lossy()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record = base.join("env.txt");
    let shim = base.join("bin").join("claude2");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf 'DIRS=%s\\nBIN=%s\\n' \"$CLAUDE_CODE_PLUGIN_DIRS\" \"$ATRIUM_BIN\" > {}\nexec sh -i\n",
            record.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        base.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &["claude2"],
        24,
        100,
        &[
            ("PATH".to_string(), path),
            ("ATRIUM_MOD".to_string(), modir.display().to_string()),
            ("ATRIUM_CLAUDE_ALIASES".to_string(), "claude2".to_string()),
            (
                "CLAUDE_CODE_PLUGIN_DIRS".to_string(),
                "/me/plugins".to_string(),
            ),
        ],
        None,
    )
    .unwrap();
    read_until(&mut p, b"1:claude2", Duration::from_secs(15));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut text = String::new();
    while std::time::Instant::now() < deadline {
        if let Ok(t) = std::fs::read_to_string(&record) {
            if t.contains("BIN=") {
                text = t;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        text.contains(&format!("DIRS=/me/plugins:{}", modir.display())),
        "the mod folder joins the dirs the variable already held: {text:?}"
    );
    assert!(
        text.contains(&format!("BIN={}", env!("CARGO_BIN_EXE_atrium"))),
        "{text:?}"
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    let _ = std::fs::remove_dir_all(&base);
}
