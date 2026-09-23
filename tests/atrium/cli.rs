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
