//! `atrium fleet`: bringing a roster up, its banner, `ls`, `init` and context.

use crate::support::*;
use std::time::Duration;

// --- fleet (0.6): `atrium fleet up <name>` brings up a saved roster ------------

/// `atrium fleet up test` reads a temp `atrium.fleet.json` with two shell agents and
/// brings up ONE tiled window with two panes. Both pane labels appear in the
/// composited frame and each round-trips a marker — proving two live sessions,
/// laid out by the loader. Hermetic: the agents run the shell, not claude.
#[test]
fn fleet_up_opens_a_two_agent_window() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    // A fleet file in a fresh temp dir; run atrium with that dir as cwd so the
    // loader's cwd-first discovery finds it.
    let td = std::env::temp_dir().join(format!("atrium-fleet-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    let cmd_json = {
        let mut parts = vec![format!("{shell:?}")];
        parts.extend(args.iter().map(|a| format!("{a:?}")));
        parts.join(", ")
    };
    let fleet_json = format!(
        r#"{{ "fleets": {{ "test": {{ "grid": "1x2", "agents": [
          {{ "name": "one", "cmd": [{cmd_json}] }},
          {{ "name": "two", "cmd": [{cmd_json}] }}
        ] }} }} }}"#
    );
    std::fs::write(td.join("atrium.fleet.json"), fleet_json).unwrap();

    // Launch atrium via its own binary, spawned with the temp dir as its working
    // directory so `fleet up` discovers the local file. pty::spawn_full sets cwd.
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &["fleet", "up", "test"],
        30,
        120,
        &hermetic_env(&td),
        Some(&td.to_string_lossy()),
    )
    .unwrap();

    // Two pane labels appear in the tiled frame, named for the fleet agents —
    // `1:one` and `2:two`, not the shared command stem (the leading blank is not
    // matched — see the note in the mass-spawn test above).
    let out = read_until(&mut p, b"2:two", Duration::from_secs(20));
    for (i, label) in [(1, &b"1:one"[..]), (2, &b"2:two"[..])] {
        assert!(
            contains(&out, &label),
            "fleet pane {i} label missing: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    // The focused (pane 1) shell round-trips.
    p.write(b"echo atrium-fleet-p1\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-fleet-p1", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-fleet-p1"),
        "fleet pane 1 silent: {:?}",
        String::from_utf8_lossy(&out)
    );
    // Move focus to the second pane and prove it is live too.
    p.write(b"\x01l").unwrap();
    p.write(b"echo atrium-fleet-p2\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-fleet-p2", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-fleet-p2"),
        "fleet pane 2 silent after focus move: {:?}",
        String::from_utf8_lossy(&out)
    );
    // The bar names the window by its FOCUSED agent, not its first one — so a
    // zoomed or focused fleet pane reads as who it is.
    let stem: &str = if cfg!(windows) { "cmd" } else { "sh" };
    let bar = format!("1:{stem}:two");
    let out = read_until(&mut p, bar.as_bytes(), Duration::from_secs(10));
    assert!(
        contains(&out, bar.as_bytes()),
        "bar did not follow focus to pane 2: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
    let _ = std::fs::remove_dir_all(&td);
}

/// `atrium fleet up nope` with no fleet file is a clean error, not a hung window.
#[test]
fn fleet_up_unknown_file_is_a_startup_error() {
    let td = std::env::temp_dir().join(format!("atrium-fleet-nofile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &["fleet", "up", "nope"],
        24,
        80,
        &hermetic_env(&td),
        Some(&td.to_string_lossy()),
    )
    .unwrap();
    assert_ne!(wait_exit(&mut p, 15), 0, "missing fleet file should error");
    let _ = std::fs::remove_dir_all(&td);
}

// --- fleet disclosure: what the roster grants, on the screen it is approved on

/// A fleet fixture plus the atrium run over it, WITHOUT a pty.
///
/// `fleet up` is deliberately driven with a non-terminal stdin here: the ack is
/// skipped (there is nobody to ask), the whole banner is still printed, and the
/// launch then stops at `rawterm::Terminal::raw()`. That gives an exact,
/// non-flaky assertion on the real binary's approval-time output on a host whose
/// pty table is finite — and, crucially, it exercises the WIRING. The same
/// classification once ran green in unit tests while the loader ignored its
/// verdict; only a test that runs the binary catches that.
struct FleetLab {
    root: std::path::PathBuf,
}

impl FleetLab {
    fn new(tag: &str) -> FleetLab {
        let root = std::env::temp_dir().join(format!("atrium-disc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proj")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        FleetLab { root }
    }
    fn proj(&self) -> std::path::PathBuf {
        self.root.join("proj")
    }
    fn dir(&self, rel: &str) -> std::path::PathBuf {
        let p = self.root.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn file(&self, json: &str) {
        std::fs::write(self.proj().join("atrium.fleet.json"), json).unwrap();
    }
    /// Run `atrium <args…>` in the fixture project with a fixture HOME and a
    /// fixture global-config location, and return `(exit code, stdout, stderr)`.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_atrium"))
            .args(args)
            .current_dir(self.proj())
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("APPDATA", self.root.join("cfg"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

impl Drop for FleetLab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A `cwd` / `add_dirs` that leaves the tree is NAMED on the banner the operator
/// acknowledges, resolved through the symlink, by the real binary.
#[test]
#[cfg(unix)]
fn fleet_up_discloses_a_symlink_that_leaves_the_tree() {
    let lab = FleetLab::new("symlink");
    let secrets = lab.dir("secrets");
    std::os::unix::fs::symlink(&secrets, lab.proj().join("context")).unwrap();
    lab.file(
        r#"{"fleets":{"t":{"agents":[
            {"name":"one","cmd":["claude"],"add_dirs":["./context"]},
            {"name":"two","cmd":["claude"]}]}}}"#,
    );
    let (_code, _out, err) = lab.run(&["fleet", "up", "t"]);
    assert!(err.contains("OUTSIDE"), "no disclosure in: {err}");
    assert!(
        err.contains("./context") && err.contains("secrets"),
        "the line must name the entry AND where it really lands: {err}"
    );
    // It got all the way past the reach check to the tty handoff — the
    // disclosure is a banner, not a refusal.
    assert!(err.contains("must be a terminal"), "{err}");
}

/// A roster pointed at a credential store says which store, in words.
#[test]
#[cfg(unix)]
fn fleet_up_names_the_credential_store_a_roster_points_at() {
    let lab = FleetLab::new("creds");
    let ssh = lab.dir("home/.ssh");
    std::fs::write(ssh.join("id_rsa"), "k").unwrap();
    std::os::unix::fs::symlink(&ssh, lab.proj().join("ctx")).unwrap();
    lab.file(
        r#"{"fleets":{"t":{"agents":[{"name":"one","cmd":["claude"],"add_dirs":["./ctx"]}]}}}"#,
    );
    let (_code, _out, err) = lab.run(&["fleet", "up", "t"]);
    assert!(
        err.contains("SSH private keys"),
        "a credential store must be named as one: {err}"
    );
    assert!(err.contains("CREDENTIALS"), "{err}");
}

/// The fleet file cannot repaint the screen it is being approved on.
#[test]
fn a_fleet_file_cannot_write_escape_sequences_into_the_approval_banner() {
    let lab = FleetLab::new("inject");
    // `\u001b` in the file, decoded to a real ESC by the JSON parser — the
    // crossing this defends.
    lab.file(
        r#"{"fleets":{"t":{"agents":[
            {"name":"ev\u001b[2J\u001b[1;31mil","cmd":["claude"],
             "add_dirs":["./\u001b[2Kx"]}]}}}"#,
    );
    let (_code, _out, err) = lab.run(&["fleet", "up", "t"]);
    assert!(
        !err.contains('\u{1b}'),
        "a raw ESC from the fleet file reached the terminal: {err:?}"
    );
    assert!(
        err.contains("\\u{1b}"),
        "and it is still shown, defanged: {err}"
    );
}

/// `fleet ls` prints file-supplied names at a terminal too.
#[test]
fn fleet_ls_defangs_a_name_from_the_file() {
    let lab = FleetLab::new("ls");
    lab.file(r#"{"fleets":{"pro\u001b[2Jd":{"agents":[{"name":"a","cmd":["claude"]}]}}}"#);
    let (code, out, _err) = lab.run(&["fleet", "ls"]);
    assert_eq!(code, 0);
    assert!(!out.contains('\u{1b}'), "raw ESC on stdout: {out:?}");
    assert!(out.contains("\\u{1b}"), "{out}");
}

/// A `cwd` naming a regular FILE fails before the screen is taken.
///
/// `exists()` is true for a file: an existence check passes it through the
/// banner, through raw mode and through `bind_ctl`, and it dies in the pty spawn
/// with ENOTDIR after panes are already up — the half-open fleet this check is
/// there to prevent.
#[test]
fn a_cwd_that_names_a_regular_file_is_refused_before_the_terminal_is_taken() {
    let lab = FleetLab::new("filecwd");
    std::fs::write(lab.proj().join("notadir"), "hi").unwrap();
    lab.file(r#"{"fleets":{"t":{"agents":[{"name":"a","cmd":["claude"],"cwd":"./notadir"}]}}}"#);
    let (code, _out, err) = lab.run(&["fleet", "up", "t"]);
    assert_ne!(code, 0, "must be a startup error: {err}");
    assert!(err.contains("is not a directory"), "{err}");
    assert!(
        !err.contains("must be a terminal"),
        "it must stop BEFORE the tty handoff: {err}"
    );
}

/// A roster past the pane cap is a preflight WARNING: it is printed with the
/// other warnings before the verdict, and the launch carries on to the terminal
/// handoff instead of stopping.
#[test]
fn a_roster_past_the_pane_cap_warns_and_still_launches() {
    let lab = FleetLab::new("panecap");
    lab.file(
        r#"{"fleets":{"t":{"agents":[
            {"name":"one","cmd":["claude"]},
            {"name":"two","cmd":["claude"]},
            {"name":"three","cmd":["claude"]}]}}}"#,
    );
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_atrium"))
        .args(["fleet", "up", "t"])
        .current_dir(lab.proj())
        .env("HOME", lab.root.join("home"))
        .env("USERPROFILE", lab.root.join("home"))
        .env("XDG_CONFIG_HOME", lab.root.join("cfg"))
        .env("APPDATA", lab.root.join("cfg"))
        .env("ATRIUM_MAX_PANES", "2")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let warning = err
        .lines()
        .find(|l| l.contains("pane cap of 2"))
        .unwrap_or_else(|| panic!("no pane-cap warning: {err}"));
    // Piped stderr is a log: plain and greppable, no escapes.
    assert!(
        warning.starts_with("atrium fleet: warning: 3 agents"),
        "{warning}"
    );
    assert!(!err.contains('\x1b'), "{err}");
    let verdict = err
        .find("every agent dir resolves inside")
        .unwrap_or_else(|| panic!("no verdict: {err}"));
    assert!(
        err.find("pane cap of 2").unwrap() < verdict,
        "warnings precede the verdict: {err}"
    );
    // Not refused: it went on to take the terminal (and, with none, stopped there).
    assert!(err.contains("must be a terminal"), "{err}");
}

/// The decisive lines are the LAST lines: on a 24-row terminal whatever prints
/// first is what scrolls away before the Enter is asked for.
#[test]
fn the_posture_and_the_verdict_are_the_last_lines_before_the_prompt() {
    let lab = FleetLab::new("order");
    lab.dir("shared");
    let agents: Vec<String> = (0..8)
        .map(|i| format!(r#"{{"name":"a{i}","cmd":["claude"],"add_dirs":["../shared","./s{i}"]}}"#))
        .collect();
    for i in 0..8 {
        lab.dir(&format!("proj/s{i}"));
    }
    lab.file(&format!(
        r#"{{"fleets":{{"t":{{"agents":[{}]}}}}}}"#,
        agents.join(",")
    ));
    let (_code, _out, err) = lab.run(&["fleet", "up", "t"]);
    let at = |needle: &str| {
        err.find(needle)
            .unwrap_or_else(|| panic!("missing {needle}: {err}"))
    };
    assert!(at("OUTSIDE") < at("starting 8 agent(s)"), "{err}");
    assert!(
        at("starting 8 agent(s)") < at("may spawn teammates"),
        "{err}"
    );
    assert!(at("may spawn teammates") < at("GRANTS"), "{err}");
    // The compile budget is stated on the posture line itself, not a line of its
    // own that would push the banner past a short terminal.
    let posture = err
        .lines()
        .find(|l| l.contains("starting 8 agent(s)"))
        .unwrap_or_default();
    assert!(posture.contains("compile jobs shared"), "{posture}");
    assert!(posture.contains("memory"), "{posture}");
    assert!(
        posture.contains("2 deny rules"),
        "built-in fail-safes only: {posture}"
    );
    // Deduplicated: eight agents naming ONE sibling checkout is one line.
    assert_eq!(err.matches("OUTSIDE").count(), 1, "{err}");
    // And the banner as a whole still fits a terminal.
    assert!(
        err.lines()
            .filter(|l| l.starts_with("atrium fleet:"))
            .count()
            <= 8,
        "banner too tall: {err}"
    );
}

/// A fleet that declares a `context` block actually reaches its panes: the
/// agent's *process* has `CONTEXT_MODE_DIR` and `CONTEXT_MODE_SESSION_SUFFIX`
/// set, which is how the context-mode plugin is pointed at a shared store.
///
/// The unit tests cover the mapping (provider, share) → env pairs; this covers
/// the rest of the chain — fleet parse → `context_env` → `PaneSpec.extra_env` →
/// the pty's environment — which was otherwise only verified by reading it. A
/// refactor that dropped `extra_env` on the way to the spawn would leave every
/// unit test green and silently stop configuring the store.
///
/// `share: knowledge` is the interesting case: every agent shares one directory
/// and gets its own session suffix, so the two panes must disagree on the
/// suffix while agreeing on the dir.
#[test]
fn a_fleet_context_block_reaches_the_agent_process() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let td = std::env::temp_dir().join(format!("atrium-ctx-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    let cmd_json = {
        let mut parts = vec![format!("{shell:?}")];
        parts.extend(args.iter().map(|a| format!("{a:?}")));
        parts.join(", ")
    };
    let fleet_json = format!(
        r#"{{ "fleets": {{ "ctx": {{ "grid": "1x2",
          "context": {{ "provider": "context-mode", "share": "knowledge" }},
          "agents": [
          {{ "name": "one", "cmd": [{cmd_json}] }},
          {{ "name": "two", "cmd": [{cmd_json}] }}
        ] }} }} }}"#
    );
    std::fs::write(td.join("atrium.fleet.json"), fleet_json).unwrap();

    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_atrium"),
        &["fleet", "up", "ctx"],
        30,
        140,
        &hermetic_env(&td),
        Some(&td.to_string_lossy()),
    )
    .unwrap();
    read_until(&mut p, b"2:two", Duration::from_secs(20));

    // Pane 1 prints its suffix. Marked either side so the echoed command line
    // itself can't satisfy the assertion.
    let echo: &[u8] = if cfg!(windows) {
        b"echo ctx[%CONTEXT_MODE_SESSION_SUFFIX%]end\r\n"
    } else {
        b"echo \"ctx[$CONTEXT_MODE_SESSION_SUFFIX]end\"\r\n"
    };
    p.write(echo).unwrap();
    let out = read_until(&mut p, b"ctx[one]end", Duration::from_secs(15));
    assert!(
        contains(&out, b"ctx[one]end"),
        "agent `one` did not get CONTEXT_MODE_SESSION_SUFFIX=one: {:?}",
        String::from_utf8_lossy(&out)
    );

    // The shared directory is set too, and points inside the fleet's ctx root.
    let echo_dir: &[u8] = if cfg!(windows) {
        b"echo dir[%CONTEXT_MODE_DIR%]end\r\n"
    } else {
        b"echo \"dir[$CONTEXT_MODE_DIR]end\"\r\n"
    };
    p.write(echo_dir).unwrap();
    let out = read_until(&mut p, b"]end", Duration::from_secs(15));
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("dir[") && !text.contains("dir[]end"),
        "CONTEXT_MODE_DIR is empty in the pane: {text}"
    );

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    let _ = std::fs::remove_dir_all(&td);
}

/// `atrium fleet init` writes `./atrium.fleet.json` from a built-in, scales its
/// builders with `--agents`, refuses to overwrite, and prefers the user's own
/// fleet of the same name from the global `fleet.json` — saying so. The global
/// file is a temp one, and every run happens in its own temp directory.
#[test]
fn fleet_init_writes_a_builtin_or_the_users_template_and_never_overwrites() {
    let base = std::env::temp_dir().join(format!("atrium-fleet-init-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let global = base.join("global").join("fleet.json");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(
        &global,
        r#"{"fleets":{"crew":{"agents":[{"name":"only","cmd":["claude"],"prompt":"mine, not the built-in"}]},
                     "mine":{"agents":[{"name":"m","cmd":["claude"]}]}}}"#,
    )
    .unwrap();
    let run = |dir: &std::path::Path, args: &[&str]| {
        std::fs::create_dir_all(dir).unwrap();
        std::process::Command::new(env!("CARGO_BIN_EXE_atrium"))
            .args(args)
            .current_dir(dir)
            .env("ATRIUM_FLEET", &global)
            .env("ATRIUM_YES", "1")
            .output()
            .unwrap()
    };
    let text =
        |dir: &std::path::Path| std::fs::read_to_string(dir.join("atrium.fleet.json")).unwrap();

    // A built-in, scaled.
    let d = base.join("pair");
    let out = run(&d, &["fleet", "init", "pair", "--agents", "3"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let t = text(&d);
    for name in ["builder-1", "builder-2", "builder-3", "reviewer"] {
        assert!(
            t.contains(&format!("\"name\": \"{name}\"")),
            "{name} in {t}"
        );
    }
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("built-in \"pair\""),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Never overwrite.
    let out = run(&d, &["fleet", "init", "solo"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already exists"));
    assert_eq!(text(&d), t, "the file is untouched");

    // The user's fleet wins over the built-in of the same name, verbatim.
    let d = base.join("crew");
    let out = run(&d, &["fleet", "init", "crew"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let t = text(&d);
    assert!(
        t.contains("mine, not the built-in") && !t.contains("integrator"),
        "{t}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("shadowing the built-in"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // A user's fleet with its own name; an unknown name is refused.
    let d = base.join("mine");
    let out = run(&d, &["fleet", "init", "mine"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text(&d).contains("\"name\":\"m\""),
        "verbatim: {}",
        text(&d)
    );
    let out = run(&base.join("nope"), &["fleet", "init", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no template named"));

    // The menu names both sources and the shadow.
    let out = run(&base.join("ls"), &["fleet", "ls", "--templates"]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("built-in:") && stdout.contains("solo") && stdout.contains("yours ("),
        "{stdout}"
    );
    assert!(stdout.contains("(shadows the built-in)"), "{stdout}");
    let _ = std::fs::remove_dir_all(&base);
}
