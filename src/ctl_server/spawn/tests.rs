//! Placing a spawned worker in its own worktree.

use super::*;

// -- worktree_spawn_params error-path -------------------------------------

/// A non-git directory must produce a clear `Err` rather than silently
/// returning a path that was never created (the original bug).
#[test]
fn worktree_spawn_params_non_git_dir_returns_err() {
    let tmp = std::env::temp_dir().join("atrium_test_non_git");
    std::fs::create_dir_all(&tmp).ok();
    let result = worktree_spawn_params(&tmp, Some("my-wt"));
    assert!(
        result.is_err(),
        "expected Err for non-git dir, got Ok: {result:?}"
    );
    let msg = result.unwrap_err();
    assert!(
        msg.contains("could not create worktree"),
        "message should identify the failure: {msg}"
    );
}

/// A real git repo must produce `Ok` with a `Some` dir that actually exists.
#[test]
fn worktree_spawn_params_git_repo_yields_ok_and_creates_dir() {
    let tmp = std::env::temp_dir().join("atrium_test_git_repo");
    // Clean slate so the test is idempotent.
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // Initialise a minimal git repo with one commit so `worktree add` has HEAD.
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&tmp)
            .output()
    };
    git(&["init"]).unwrap();
    // An explicit identity: on a machine with no global git user (a fresh
    // Linux box, CI) the commit silently fails and there is no HEAD to branch.
    let commit = git(&[
        "-c",
        "user.name=atrium-test",
        "-c",
        "user.email=atrium-test@example.invalid",
        "commit",
        "--allow-empty",
        "-m",
        "init",
    ])
    .unwrap();
    assert!(commit.status.success(), "fixture commit failed: {commit:?}");
    let result = worktree_spawn_params(&tmp, Some("test-wt"));
    assert!(result.is_ok(), "expected Ok for valid git repo: {result:?}");
    let (dir, norms) = result.unwrap();
    let dir = dir.expect("dir must be Some");
    let norms = norms.expect("norms must be Some");
    assert!(
        std::path::Path::new(&dir).exists(),
        "worktree directory must have been created: {dir}"
    );
    assert!(!norms.is_empty(), "norms must not be empty");
    // Cleanup.
    let _ = std::fs::remove_dir_all(&tmp);
}

// -- pin_to_path: a ctl spawn runs the file its command names on PATH ---------

/// A bare name is looked up over the absolute directories of PATH and
/// becomes that file's full path; a name found in none is refused, and so is
/// one that exists only in a relative or empty PATH entry (the current
/// directory under another name). Extension matching is the file system's,
/// so the lookup itself is exercised on Windows, where it is applied.
#[cfg(windows)]
#[test]
fn a_ctl_spawn_is_pinned_to_the_file_its_command_names_on_path() {
    // Under the working directory, so the same folder has a relative name.
    let rel = std::path::PathBuf::from("target").join(format!("atrium-pin-{}", std::process::id()));
    let abs = std::env::current_dir().unwrap().join(&rel);
    let _ = std::fs::remove_dir_all(&abs);
    std::fs::create_dir_all(&abs).unwrap();
    std::fs::write(abs.join("zzagent.CMD"), "@echo shim").unwrap();
    let exts = Some(".COM;.EXE;.BAT;.CMD");
    let path_of = |dirs: &[&std::path::Path]| std::env::join_paths(dirs).unwrap();

    // Found in an absolute entry: the full path, which is what is spawned.
    let path = path_of(&[std::path::Path::new(r"C:\nowhere-atrium"), &abs]);
    let pinned = pin_to_path("zzagent", Some(&path), exts)
        .expect("found")
        .expect("a bare name is pinned");
    assert!(std::path::Path::new(&pinned).is_absolute(), "{pinned}");
    assert!(pinned.to_lowercase().ends_with(r"\zzagent.cmd"), "{pinned}");

    // The same folder by a relative name, the current directory, an empty
    // entry: none of them is searched.
    for dirs in [
        vec![rel.as_path()],
        vec![std::path::Path::new(".")],
        vec![std::path::Path::new("")],
        vec![std::path::Path::new(r"\rooted-but-no-drive")],
    ] {
        let why = pin_to_path("zzagent", Some(&path_of(&dirs)), exts).unwrap_err();
        assert!(
            why.contains("was not found in any directory of PATH"),
            "{why}"
        );
    }
    // Not installed at all, and no PATH at all.
    assert!(pin_to_path("zznothere", Some(&path), exts).is_err());
    assert!(pin_to_path("zzagent", None, exts).is_err());
    let _ = std::fs::remove_dir_all(&abs);
}

/// The caller's own kind may be a path a fleet file approved: it is run as
/// written, on every platform.
#[test]
fn a_recorded_path_is_left_as_the_fleet_file_wrote_it() {
    for path in [
        r"C:\tools\claude.cmd",
        "/opt/tools/claude",
        "./bin/claude",
        r".\claude.cmd",
    ] {
        assert_eq!(pin_to_path(path, None, None), Ok(None), "{path}");
    }
    // A bare name is never passed through unresolved.
    assert!(pin_to_path("zz-no-such-agent", None, None).is_err());
}

/// The same pin off Windows: the name as it is, over the absolute entries of
/// PATH, and only a regular file with an execute bit. A relative entry
/// (`./node_modules/.bin`, `.`, an empty one) is where a pane could plant a
/// file named for an agent, in the project or in the worktree a spawn names:
/// it is not searched.
#[cfg(unix)]
#[test]
fn a_ctl_spawn_is_pinned_to_an_executable_on_an_absolute_path_entry() {
    use std::os::unix::fs::PermissionsExt;
    let rel = std::path::PathBuf::from("target").join(format!("atrium-pin-{}", std::process::id()));
    let abs = std::env::current_dir().unwrap().join(&rel);
    let _ = std::fs::remove_dir_all(&abs);
    std::fs::create_dir_all(abs.join("bin")).unwrap();
    std::fs::create_dir_all(abs.join("zzdir")).unwrap();
    let exe = abs.join("bin").join("zzagent");
    std::fs::write(&exe, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plain = abs.join("bin").join("zzplain");
    std::fs::write(&plain, "not a program").unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path_of = |dirs: &[&std::path::Path]| std::env::join_paths(dirs).unwrap();

    let bin = abs.join("bin");
    let path = path_of(&[std::path::Path::new("/nowhere-atrium"), &bin]);
    assert_eq!(
        pin_to_path("zzagent", Some(&path), None),
        Ok(Some(exe.to_string_lossy().into_owned()))
    );
    // Not executable, a directory, not there: refused.
    assert!(pin_to_path("zzplain", Some(&path), None).is_err());
    assert!(pin_to_path("zzdir", Some(&path_of(&[&abs])), None).is_err());
    assert!(pin_to_path("zznothere", Some(&path), None).is_err());
    // The same folder by a relative name, `.`, an empty entry: not searched.
    let rel_bin = rel.join("bin");
    for dirs in [
        vec![rel_bin.as_path()],
        vec![std::path::Path::new(".")],
        vec![std::path::Path::new("")],
    ] {
        let why = pin_to_path("zzagent", Some(&path_of(&dirs)), None).unwrap_err();
        assert!(
            why.contains("was not found in any directory of PATH"),
            "{why}"
        );
    }
    // A relative entry ahead of the real one does not shadow it.
    let shadowed = path_of(&[rel_bin.as_path(), &bin]);
    assert_eq!(
        pin_to_path("zzagent", Some(&shadowed), None),
        Ok(Some(exe.to_string_lossy().into_owned()))
    );
    assert!(pin_to_path("zzagent", None, None).is_err());
    let _ = std::fs::remove_dir_all(&abs);
}
