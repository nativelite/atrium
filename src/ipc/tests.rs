//! The chosen endpoint: the `--ctl-socket` flag, `ATRIUM_CTL_SOCKET`, what
//! gets bound and published, and the discovery files.

use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// An address this test process may bind, in the platform's own form.
fn test_address(tag: &str) -> String {
    let name = format!("atrium-ipc-test-{}-{tag}", std::process::id());
    if cfg!(windows) {
        normalize_socket(&name, true).unwrap()
    } else {
        let path = std::env::temp_dir().join(format!("{name}.sock"));
        normalize_socket(&path.to_string_lossy(), false).unwrap()
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atrium_ipc_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_unix_socket_must_be_an_absolute_path() {
    assert_eq!(
        normalize_socket("/run/user/1000/atrium.sock", false).as_deref(),
        Ok("/run/user/1000/atrium.sock")
    );
    let err = normalize_socket("atrium.sock", false).unwrap_err();
    assert!(err.contains("absolute"), "{err}");
    assert!(normalize_socket("", false).is_err());
}

#[test]
fn a_windows_socket_is_a_pipe_name_bare_or_whole() {
    assert_eq!(
        normalize_socket("atrium-main", true).as_deref(),
        Ok(r"\\.\pipe\atrium-main")
    );
    assert_eq!(
        normalize_socket(r"\\.\pipe\atrium-main", true).as_deref(),
        Ok(r"\\.\pipe\atrium-main")
    );
    for bad in [r"C:\tmp\atrium.sock", "/tmp/atrium.sock", r"\\.\pipe\"] {
        let err = normalize_socket(bad, true).unwrap_err();
        assert!(err.contains("named-pipe name"), "{bad}: {err}");
    }
}

/// The tmux shim's `TMUX=<address>,<pid>,0` splits on the comma.
#[test]
fn a_comma_in_the_socket_is_refused_with_the_reason() {
    for windows in [false, true] {
        let err = normalize_socket("/tmp/a,b.sock", windows).unwrap_err();
        assert!(err.contains("comma") && err.contains("TMUX"), "{err}");
    }
    assert!(normalize_socket("/tmp/a\nb.sock", false).is_err());
}

#[test]
fn the_socket_flag_is_taken_from_anywhere_among_the_launch_flags() {
    let want = normalize_socket(if cfg!(windows) { "s" } else { "/tmp/s" }, cfg!(windows)).unwrap();
    let raw = if cfg!(windows) { "s" } else { "/tmp/s" };
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["--ctl-socket", raw, "--allow-ctl", "claude"],
            &["--allow-ctl", "claude"],
        ),
        (
            &["--allow-ctl", "--ctl-socket", raw, "claude"],
            &["--allow-ctl", "claude"],
        ),
        (
            &["--max-depth", "3", "--trust", "accept", "--ctl-socket", raw],
            &["--max-depth", "3", "--trust", "accept"],
        ),
        (
            &["--trust", "--ctl-socket", raw, "up", "team"],
            &["--trust", "up", "team"],
        ),
        (
            &[
                "--trust=plan",
                "--max-depth=2",
                "--skip-permissions",
                "--ctl-socket",
                raw,
            ],
            &["--trust=plan", "--max-depth=2", "--skip-permissions"],
        ),
    ];
    for (given, rest) in cases {
        let (found, left) = take_socket_flag(&args(given)).unwrap();
        assert_eq!(found.as_deref(), Some(want.as_str()), "{given:?}");
        assert_eq!(left, args(rest), "{given:?}");
    }
    let assigned = format!("--ctl-socket={raw}");
    let (found, left) = take_socket_flag(&args(&[&assigned, "claude"])).unwrap();
    assert_eq!(found.as_deref(), Some(want.as_str()));
    assert_eq!(left, args(&["claude"]));
}

/// Past the first token that is not a launch flag the arguments are the
/// hosted program's, `--ctl-socket` included.
#[test]
fn the_hosted_programs_own_socket_flag_is_never_eaten() {
    for given in [
        &["claude", "--ctl-socket", "x"][..],
        &["--allow-ctl", "claude", "--ctl-socket", "x"],
        &["--trust", "claude", "--ctl-socket", "x"],
        &["-n", "2", "--ctl-socket", "x"],
        &[],
    ] {
        let (found, left) = take_socket_flag(&args(given)).unwrap();
        assert_eq!(found, None, "{given:?}");
        assert_eq!(left, args(given), "{given:?}");
    }
}

#[test]
fn a_bad_socket_flag_is_a_startup_error() {
    let raw = if cfg!(windows) { "s" } else { "/tmp/s" };
    let twice = take_socket_flag(&args(&["--ctl-socket", raw, "--ctl-socket", raw]));
    assert!(twice.unwrap_err().contains("twice"));
    let bare = take_socket_flag(&args(&["--allow-ctl", "--ctl-socket"]));
    assert!(bare.unwrap_err().contains("needs a path"));
    let comma = take_socket_flag(&args(&["--ctl-socket=a,b"]));
    assert!(comma.unwrap_err().contains("comma"));
}

#[test]
fn the_flag_wins_over_the_environment_and_both_over_the_pid_default() {
    let from_flag = test_address("flag");
    let env_raw = if cfg!(windows) {
        "from-env"
    } else {
        "/tmp/from-env.sock"
    };
    let from_env = normalize_socket(env_raw, cfg!(windows)).unwrap();

    let both = resolve(Some(&from_flag), Some(env_raw), 7).unwrap();
    assert_eq!(
        (both.address.as_str(), both.explicit),
        (from_flag.as_str(), true)
    );
    let env_only = resolve(None, Some(env_raw), 7).unwrap();
    assert_eq!(
        (env_only.address.as_str(), env_only.explicit),
        (from_env.as_str(), true)
    );
    for unset in [None, Some("")] {
        let neither = resolve(None, unset, 7).unwrap();
        assert_eq!(neither.address, address_for(7));
        assert!(!neither.explicit, "the per-pid default is nobody's choice");
    }
    let err = resolve(None, Some("a,b"), 7).unwrap_err();
    assert!(err.starts_with(ENV_CTL_SOCKET), "{err}");
}

/// The override is what gets bound, and the address handed back for
/// publication (`CTL_ADDRESS`, so `ATRIUM_CTL` in every pane) is the override
/// itself. A second session on it is refused in words an operator can act
/// on, and the first keeps the endpoint.
#[test]
fn the_override_is_bound_and_is_the_address_published_and_a_second_bind_is_refused() {
    let chosen = test_address("bound");
    let endpoint = resolve(Some(&chosen), None, std::process::id()).unwrap();
    let (listener, published) = bind_endpoint(&endpoint).expect("bind the override");
    assert_eq!(published, chosen);
    assert_ne!(published, default_address());
    assert!(nlipc::endpoint_live(&chosen), "the override is served");
    assert!(
        !nlipc::endpoint_live(&default_address()),
        "and the per-pid address is not"
    );

    let err = bind_endpoint(&endpoint).map(|_| ()).unwrap_err();
    assert!(
        err.contains(&chosen) && err.contains("already listening"),
        "{err}"
    );
    assert!(nlipc::endpoint_live(&chosen), "the first session kept it");

    drop(listener);
    assert!(!nlipc::endpoint_live(&chosen));
    let (again, _) = bind_endpoint(&endpoint).expect("free once the first session is gone");
    drop(again);
}

#[test]
fn a_discovery_file_names_a_live_session_and_a_stale_one_is_removed() {
    let dir = scratch("discover");
    let live_file = publish_endpoint_in(&dir, 41, "live-address").unwrap();
    let stale_file = publish_endpoint_in(&dir, 42, "dead-address").unwrap();
    std::fs::write(dir.join("notes.txt"), b"not ours").unwrap();
    std::fs::write(dir.join("junk.endpoint"), b"no pid in the name").unwrap();
    assert_eq!(
        std::fs::read_to_string(&live_file).unwrap(),
        "live-address\n"
    );

    let found = discover_in(&dir, |address| address == "live-address");
    assert_eq!(found, vec![(41, "live-address".to_string())]);
    assert!(live_file.exists());
    assert!(!stale_file.exists(), "a file whose endpoint is silent goes");
    assert!(!dir.join("junk.endpoint").exists());
    assert!(dir.join("notes.txt").exists(), "other files are not ours");

    let none = discover_in(&dir, |_| false);
    assert!(none.is_empty() && !live_file.exists());
    assert!(discover_in(&dir.join("absent"), |_| true).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_discovery_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("mode");
    let file = publish_endpoint_in(&dir, 7, "/tmp/x.sock").unwrap();
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let _ = std::fs::remove_dir_all(&dir);
}
