//! A nested atrium can never raise the posture it runs under.

use crate::support::*;
use std::time::Duration;

/// **A pane cannot raise its own posture by launching its own atrium.**
///
/// The trust ceiling governs `ctl spawn`, but an agent that can run commands can
/// sidestep ctl entirely: `atrium --trust skip claude` is just a shell command, and
/// a fresh atrium session sets its own policy. That escape made the ceiling half a
/// ceiling.
///
/// The cap hangs off process ANCESTRY rather than the environment, because an
/// agent owns its environment — an `ATRIUM_AGENT=1` marker or an inherited policy
/// variable dies to `env -u`. It cannot unset its own parent.
///
/// Here the outer session is `plan`; the inner launch asks for `skip` and must be
/// refused down to `plan`, saying so.
#[cfg(unix)]
#[test]
fn a_nested_atrium_cannot_raise_its_own_trust() {
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let mut p = pty::Pty::spawn(
        atrium,
        &["--allow-ctl", "--trust", "plan", "sh", "-i"],
        24,
        80,
    )
    .unwrap();
    // Let the outer session record its policy in the registry the cap reads.
    p.write(b"m=outer; echo \"$m\"\"=up\"\r\n").unwrap();
    read_until(&mut p, b"outer=up", Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(600));

    // The escape attempt, exactly as an agent would make it.
    p.write(format!("\"{atrium}\" --trust skip sh -i\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"capped to", Duration::from_secs(15));
    assert!(
        contains(&out, b"capped to"),
        "a nested atrium was allowed to elevate itself: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// **`fleet up` is gated exactly like a single pane** (nesting escape).
///
/// `fleet` is dispatched before the single-pane path's ancestry cap, so for a
/// while an agent could escape its session's ceiling simply by launching a fleet
/// instead of a pane — the ceiling held for one door and not the other. Worse in
/// the workflow the fleet file is built for: an agent writes the roster, a human
/// reviews and runs it, so a posture that slips past the review defeats the
/// review.
#[cfg(unix)]
#[test]
fn a_nested_fleet_up_is_capped_like_a_pane() {
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let td = std::env::temp_dir().join(format!("atrium-nestfleet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    // A fleet file that asks for full bypass — as an agent-authored one might.
    std::fs::write(
        td.join("atrium.fleet.json"),
        r#"{"fleets":{"esc":{"trust":"skip","agents":[{"name":"a","cmd":["sh","-i"]}]}}}"#,
    )
    .unwrap();

    let mut p = pty::Pty::spawn(
        atrium,
        &["--allow-ctl", "--trust", "plan", "sh", "-i"],
        24,
        80,
    )
    .unwrap();
    p.write(b"m=outer; echo \"$m\"\"=up\"\r\n").unwrap();
    read_until(&mut p, b"outer=up", Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(600));

    p.write(format!("cd {} && \"{atrium}\" fleet up esc\r\n", td.display()).as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"capped to", Duration::from_secs(15));
    assert!(
        contains(&out, b"capped to"),
        "a nested `fleet up` escaped the session ceiling: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    let _ = std::fs::remove_dir_all(&td);
}

/// **An `exec -a atrium` shim must not shadow the real parent** (same-uid escape).
///
/// The ancestry walk decided "is this hop an atrium?" by comparing `ps -eo comm=`
/// against the string `"atrium"`. Measured on macOS 25.6.0, `ps -o comm=` prints
/// **argv[0]**, which the watched process chooses: `exec -a atrium /bin/sh` reports
/// `comm=atrium` while the kernel's `proc_pidpath` still says `/bin/sh`. So a pane
/// could interpose a fake "atrium" between itself and the session it is nested in;
/// the walk stopped at the fake, read the fake's (nonexistent, or agent-authored)
/// registry, and applied no ceiling. One line, no double-fork.
///
/// Identity is now the executable FILE the kernel reports — `(st_dev, st_ino)` —
/// so a shim has to actually *be* the atrium binary, which caps by itself.
#[cfg(unix)]
#[test]
fn an_argv0_shim_does_not_shadow_the_real_parent() {
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let mut p = pty::Pty::spawn(
        atrium,
        &["--allow-ctl", "--trust", "plan", "sh", "-i"],
        24,
        80,
    )
    .unwrap();
    p.write(b"m=outer; echo \"$m\"\"=up\"\r\n").unwrap();
    read_until(&mut p, b"outer=up", Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(700));

    // Replace the pane shell with one that LOOKS like atrium to `ps`, then launch
    // the real atrium underneath it. The shim is /bin/sh run through a symlink
    // NAMED `atrium` — the same forged `comm`/argv[0] as `exec -a atrium`, but
    // portable: Ubuntu's /bin/sh is dash, which has no `exec -a`. The trailing
    // `; :` matters: with a single command `sh -c` execs it in place, which
    // would leave the shim out of the chain and quietly test nothing.
    let shim_dir = std::env::temp_dir().join(format!("atrium-shim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&shim_dir);
    std::fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("atrium");
    std::os::unix::fs::symlink("/bin/sh", &shim).unwrap();
    p.write(
        format!(
            "exec '{}' -c '\"{atrium}\" --trust skip sh -i; :'\r\n",
            shim.display()
        )
        .as_bytes(),
    )
    .unwrap();
    let out = read_until(&mut p, b"capped to", Duration::from_secs(15));
    assert!(
        contains(&out, b"capped to"),
        "an argv[0] shim shadowed the real atrium parent and lifted the ceiling: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    let _ = std::fs::remove_dir_all(&shim_dir);
}

/// **Deleting the parent's registry must not lift the ceiling** (fail-closed).
///
/// The cap read the parent's policy from disk and, finding nothing, applied no
/// ceiling at all. The registry is mode 0644 at a fixed path owned by the same
/// uid the agent runs as, so `rm -f /tmp/atrium-session-$PPID.pids` turned the
/// ceiling off — one command, no double-fork, no forgery.
///
/// The kernel still says we are nested, and that half cannot be deleted. An
/// unreadable policy is now "cap to the safe floor", not "no ceiling".
#[cfg(unix)]
#[test]
fn deleting_the_parents_registry_does_not_lift_the_ceiling() {
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let mut p = pty::Pty::spawn(
        atrium,
        &["--allow-ctl", "--trust", "plan", "sh", "-i"],
        24,
        80,
    )
    .unwrap();
    p.write(b"m=outer; echo \"$m\"\"=up\"\r\n").unwrap();
    read_until(&mut p, b"outer=up", Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(700));

    // The pane shell's parent IS the outer atrium, so its registry is one `rm`
    // away. Then make the escape attempt.
    p.write(b"rm -f /tmp/atrium-session-$PPID.pids\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    p.write(format!("\"{atrium}\" --trust skip sh -i\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"capped to", Duration::from_secs(15));
    assert!(
        contains(&out, b"capped to"),
        "deleting the parent's registry lifted the session ceiling: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// **A redirected `$TMPDIR` must not lift the ceiling** (re-review, critical).
///
/// The cap reads the parent's policy from a registry on disk — deliberately, so
/// it is not an environment variable an agent can `env -u`. But the registry was
/// addressed through `std::env::temp_dir()`, i.e. `$TMPDIR`, which the agent also
/// owns. So `TMPDIR=/tmp/x atrium --trust skip` walked the ancestry correctly, then
/// looked for the parent's file under the CHILD's TMPDIR, found nothing, and
/// capped nothing. One variable, no double-fork.
///
/// Moving the ceiling out of the environment is pointless if the PATH to it is
/// still in the environment.
#[cfg(unix)]
#[test]
fn a_redirected_tmpdir_does_not_lift_the_ceiling() {
    let atrium = env!("CARGO_BIN_EXE_atrium");
    let decoy = std::env::temp_dir().join(format!("atrium-decoy-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&decoy);

    let mut p = pty::Pty::spawn(
        atrium,
        &["--allow-ctl", "--trust", "plan", "sh", "-i"],
        24,
        80,
    )
    .unwrap();
    p.write(b"m=outer; echo \"$m\"\"=up\"\r\n").unwrap();
    read_until(&mut p, b"outer=up", Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(700));

    // The escape, exactly as reported.
    p.write(
        format!(
            "TMPDIR={} \"{atrium}\" --trust skip sh -i\r\n",
            decoy.display()
        )
        .as_bytes(),
    )
    .unwrap();
    let out = read_until(&mut p, b"capped to", Duration::from_secs(15));
    assert!(
        contains(&out, b"capped to"),
        "a redirected TMPDIR lifted the session ceiling: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    let _ = std::fs::remove_dir_all(&decoy);
}
