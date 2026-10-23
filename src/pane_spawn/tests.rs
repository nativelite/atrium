//! Building a pane's launch: the Windows shim argv, the folded system prompt, and the base environment.

use super::*;

// -- assemble_command pure seam ------------------------------------------

fn argv(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_resolved_shim_is_spawned_directly_not_wrapped_in_cmd() {
    // pty owns the cmd.exe line for batch targets; wrapping here would put
    // the args back under plain argv quoting (the r10 B1 bug).
    let cmd = argv(&["claude", "-p", "a & b\nnext", "--permission-mode", "auto"]);
    let got = assemble_command(&cmd, Some(r"C:\npm\claude.cmd".into()));
    assert_eq!(
        got,
        argv(&[
            r"C:\npm\claude.cmd",
            "-p",
            "a & b next",
            "--permission-mode",
            "auto"
        ])
    );
}

#[test]
fn a_resolved_exe_keeps_its_args_verbatim() {
    let cmd = argv(&["node", "multi\nline", "x&y"]);
    let got = assemble_command(&cmd, Some(r"C:\node\node.exe".into()));
    assert_eq!(got, argv(&[r"C:\node\node.exe", "multi\nline", "x&y"]));
}

#[test]
fn an_unresolved_command_is_left_for_the_spawn_to_report() {
    let cmd = argv(&["nope", "a"]);
    assert_eq!(assemble_command(&cmd, None), cmd);
}

// -- sanitize_shim_arg pure seam -------------------------------------------

#[test]
fn sanitize_shim_arg_collapses_all_newline_forms() {
    // CRLF counts as one space, lone CR and LF each become one space.
    assert_eq!(sanitize_shim_arg("a\nb\r\nc"), "a b c");
}

#[test]
fn sanitize_shim_arg_leaves_single_line_unchanged() {
    assert_eq!(sanitize_shim_arg("hello"), "hello");
}

#[test]
fn sanitize_shim_arg_leaves_angle_brackets_and_backticks_untouched() {
    let s = "<name> `echo hi`";
    assert_eq!(sanitize_shim_arg(s), s);
}

/// Regression guard for the r8 fleet launch bug: a multi-line kickoff
/// prompt caused `cmd.exe` to truncate the command at the first newline,
/// dropping `--permission-mode auto` and `--session-id` so the pane ran in
/// the wrong mode under a self-generated session id.
#[test]
fn sanitize_shim_arg_regression_trailing_flags_survive_newline_in_prompt() {
    let args: Vec<String> = vec![
        "--model".into(),
        "opus".into(),
        "first line\nsecond line".into(), // multi-line prompt
        "--permission-mode".into(),
        "auto".into(),
        "--session-id".into(),
        "X".into(),
    ];
    let sanitized: Vec<String> = args.iter().map(|a| sanitize_shim_arg(a)).collect();
    assert_eq!(sanitized.len(), 7, "no args dropped");
    assert_eq!(sanitized[2], "first line second line", "prompt collapsed");
    assert_eq!(sanitized[3], "--permission-mode");
    assert_eq!(sanitized[4], "auto");
    assert_eq!(sanitized[5], "--session-id");
    assert_eq!(sanitized[6], "X");
}

// -- combine_system_prompt pure seam ------------------------------------

#[test]
fn combine_both_blocks_into_one_payload() {
    // The key regression guard: norms + ctl must both survive in a single
    // block so callers push exactly ONE --append-system-prompt flag.
    let result = combine_system_prompt(Some("NORMS"), Some("CTL")).unwrap();
    assert!(result.contains("NORMS"), "norms must be present: {result}");
    assert!(
        result.contains("CTL"),
        "ctl directive must be present: {result}"
    );
    // Exactly one block — no embedded flag that would re-introduce last-wins.
    assert!(
        !result.contains("--append-system-prompt"),
        "payload must not contain the flag itself: {result}"
    );
}

#[test]
fn combine_norms_only_returns_norms() {
    let result = combine_system_prompt(Some("NORMS"), None).unwrap();
    assert_eq!(result, "NORMS");
}

#[test]
fn combine_ctl_only_returns_ctl() {
    let result = combine_system_prompt(None, Some("CTL")).unwrap();
    assert_eq!(result, "CTL");
}

#[test]
fn combine_neither_returns_none() {
    assert!(combine_system_prompt(None, None).is_none());
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_fleet_agents_own_prompt_survives_the_ctl_block() {
    // A fleet agent's `prompt` key is already in its command as
    // --append-system-prompt. Pushing the ctl/norms block as a second flag
    // made claude (last-wins) silently drop the agent's own instructions.
    let cmd = args(&[
        "claude",
        "--append-system-prompt",
        "ROLE",
        "--model",
        "opus",
        "go",
        "--append-system-prompt=INLINE",
    ]);
    let out = fold_system_prompt(cmd, Some("BLOCK"));
    let flags: Vec<_> = out
        .iter()
        .filter(|a| a.starts_with("--append-system-prompt"))
        .collect();
    assert_eq!(flags.len(), 1, "exactly one flag: {out:?}");
    let payload = out.last().unwrap();
    assert_eq!(payload, "ROLE\n\nINLINE\n\nBLOCK");
    assert_eq!(&out[..4], &args(&["claude", "--model", "opus", "go"])[..]);
}

#[test]
fn fold_without_a_block_leaves_the_command_untouched() {
    let cmd = args(&["claude", "--append-system-prompt", "ROLE", "go"]);
    assert_eq!(fold_system_prompt(cmd.clone(), None), cmd);
}

/// **A pane must carry the session marker whether or not ctl is on.**
///
/// The marker used to be pushed inside the ctl block, so the DEFAULT launch
/// (no `--allow-ctl`) produced a pane with no atrium environment at all —
/// nothing identified the process as ours, so nothing could ever find it
/// again. Move the `ATRIUM_SESSION` push back inside the `if let Some((addr,
/// …)) = ctl` arm and the first assertion here fails, which is exactly the
/// shipped bug.
#[test]
fn every_pane_is_marked_even_with_no_control_channel() {
    let key = atrium::orphan::SessionKey {
        owner: 17686,
        started: 99,
    };
    let bare = pane_base_env(Some(&key), None, None);
    assert_eq!(
        bare,
        vec![("ATRIUM_SESSION".to_string(), "17686:99".to_string())],
        "a pane without ctl must still be findable"
    );

    // With ctl on, the marker is still there, still first, and the three ctl
    // variables are unchanged. Spelled as literals on purpose: this pins the
    // WIRE names a child reads, so renaming a const without the child is caught.
    let full = pane_base_env(Some(&key), Some(("/tmp/sock", "3", "deadbeef")), None);
    let names: Vec<&str> = full.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "ATRIUM_SESSION",
            "ATRIUM_CTL",
            "ATRIUM_PANE",
            "ATRIUM_TOKEN"
        ]
    );
    assert_eq!(full[3].1, "deadbeef");
}

/// No key (Windows, where the Job Object makes the whole sweep unnecessary)
/// means no marker — never a bare pid, which would be a marker that points at
/// a live unrelated process after the pid is reused.
#[test]
fn a_pane_is_never_marked_with_a_key_we_could_not_compute() {
    assert!(pane_base_env(None, None, None).is_empty());
    let ctl_only = pane_base_env(None, Some(("/tmp/sock", "3", "deadbeef")), None);
    assert!(!ctl_only.iter().any(|(k, _)| k == "ATRIUM_SESSION"));
    assert_eq!(ctl_only.len(), 3);
}

/// Every pane gets the session's compile pool, with or without ctl, under
/// the wire name cargo reads. A pane that misses it builds unpooled — the
/// exact fleet OOM this exists to prevent — and nothing would say so.
#[test]
fn every_pane_gets_the_build_pool() {
    let flags = "-j --jobserver-fds=atrium-build-1-ab --jobserver-auth=atrium-build-1-ab";
    let bare = pane_base_env(None, None, Some(flags));
    assert_eq!(
        bare,
        vec![("CARGO_MAKEFLAGS".to_string(), flags.to_string())]
    );
    let full = pane_base_env(None, Some(("/tmp/sock", "3", "deadbeef")), Some(flags));
    assert!(full.contains(&("CARGO_MAKEFLAGS".to_string(), flags.to_string())));
    // No pool (disabled, inherited, or failed): nothing is injected.
    assert!(pane_base_env(None, None, None).is_empty());
}

// -- codex daemon probe ---------------------------------------------------

/// The probe reads the codex's own `--help`: one that advertises
/// `--no-daemon` has it, one that does not (an older codex) or cannot be run
/// at all does not, so its launch stays as written.
#[cfg(unix)]
#[test]
fn the_codex_probe_trusts_only_a_help_that_names_the_flag() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("atrium-codex-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = |name: &str, help: &str| {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\necho '{help}'\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    };
    let new = fake(
        "codex-new",
        "      --no-daemon  Run without the shared background server",
    );
    let old = fake("codex-old", "  -m, --model <MODEL>");
    assert!(codex_has_no_daemon(&new));
    assert!(!codex_has_no_daemon(&old));
    assert!(!codex_has_no_daemon(&dir.join("absent").to_string_lossy()));
    let _ = std::fs::remove_dir_all(&dir);
}

// -- codex instructions and sandbox --------------------------------------

/// Codex exits on `--append-system-prompt`, so a codex command never keeps it:
/// a fleet agent's prompt and atrium's block become one
/// `-c developer_instructions`, a TOML string, even with no block.
#[test]
fn a_codex_prompt_becomes_one_developer_instructions_override() {
    let cmd = args(&[
        "codex",
        "--append-system-prompt",
        "ROLE \"quoted\"",
        "--model",
        "m",
        "go",
    ]);
    assert_eq!(
        fold_codex_instructions(cmd.clone(), Some("BLOCK")),
        args(&[
            "codex",
            "--model",
            "m",
            "go",
            "-c",
            r#"developer_instructions="ROLE \"quoted\"\n\nBLOCK""#,
        ])
    );
    assert_eq!(
        fold_codex_instructions(cmd, None),
        args(&[
            "codex",
            "--model",
            "m",
            "go",
            "-c",
            r#"developer_instructions="ROLE \"quoted\"""#,
        ])
    );
    // Nothing to say: the command is left as it was.
    let bare = args(&["codex", "go"]);
    assert_eq!(fold_codex_instructions(bare.clone(), None), bare);
}

/// A sandboxed codex pane given the ctl socket launches with the ctl profile
/// in place of `--sandbox`; without it, with the plain sandbox as before. No
/// claude flag reaches either.
#[test]
fn a_codex_pane_given_the_ctl_socket_swaps_its_sandbox_for_the_profile() {
    let cmd = args(&["codex", "--append-system-prompt", "ROLE", "go"]);
    let mode = atrium::ctl::TrustMode::Edits;
    let opened = agent_args(&cmd, "codex", mode, &[], None, Some("/t/a.sock"), &[]).join(" ");
    assert!(
        opened.contains(r#"default_permissions="atrium_ctl""#),
        "{opened}"
    );
    assert!(
        opened.contains(r#"unix_sockets={"/t/a.sock"="allow"}"#),
        "{opened}"
    );
    assert!(!opened.contains("--sandbox"), "{opened}");
    let plain = agent_args(&cmd, "codex", mode, &[], None, None, &[]).join(" ");
    assert!(plain.contains("--sandbox workspace-write"), "{plain}");
    assert!(!plain.contains("atrium_ctl"), "{plain}");
    for a in [&opened, &plain] {
        assert!(!a.contains("--append-system-prompt"), "{a}");
        assert!(!a.contains("--disallowedTools"), "{a}");
        assert!(a.contains(r#"developer_instructions="ROLE""#), "{a}");
    }
}

/// A sandboxed codex pane in a checkout gets its git dirs as filesystem rules
/// of the profile, socket or not, so it can commit; `Plan` keeps git read-only
/// and, with no socket, its plain sandbox. The rules are codex's, so a claude
/// pane never sees them.
#[test]
fn a_codex_pane_in_a_checkout_gets_its_git_dirs_opened() {
    let cmd = args(&["codex", "go"]);
    let git = atrium::vendors::codex_git_rules("/r/.git/worktrees/dev", "/r/.git");
    let edits = agent_args(
        &cmd,
        "codex",
        atrium::ctl::TrustMode::Edits,
        &[],
        None,
        None,
        &git,
    )
    .join(" ");
    assert!(
        edits.contains(r#"default_permissions="atrium_ctl""#),
        "{edits}"
    );
    assert!(
        edits.contains(
            r#"permissions.atrium_ctl.filesystem={"/r/.git"="write","/r/.git/worktrees/dev"="write","/r/.git/hooks"="read","/r/.git/config"="read","/r/.git/worktrees/dev/config.worktree"="read"}"#
        ),
        "{edits}"
    );
    assert!(!edits.contains("--sandbox"), "{edits}");
    assert!(!edits.contains("network"), "{edits}");
    let both = agent_args(
        &cmd,
        "codex",
        atrium::ctl::TrustMode::Auto,
        &[],
        None,
        Some("/t/a.sock"),
        &git,
    )
    .join(" ");
    assert!(
        both.contains("permissions.atrium_ctl.filesystem="),
        "{both}"
    );
    assert!(
        both.contains(r#"unix_sockets={"/t/a.sock"="allow"}"#),
        "{both}"
    );
    let plan = agent_args(
        &cmd,
        "codex",
        atrium::ctl::TrustMode::Plan,
        &[],
        None,
        None,
        &git,
    )
    .join(" ");
    assert!(plan.contains("--sandbox read-only"), "{plan}");
    assert!(!plan.contains("filesystem"), "{plan}");
    let claude = agent_args(
        &args(&["claude"]),
        "claude",
        atrium::ctl::TrustMode::Edits,
        &[],
        None,
        None,
        &git,
    )
    .join(" ");
    assert!(!claude.contains("filesystem"), "{claude}");
    assert!(!claude.contains("atrium_ctl"), "{claude}");
}

/// The network-proxy probe reads `codex features list` and wants a row named
/// for the feature, not a mention of it in some other row.
#[cfg(unix)]
#[test]
fn the_network_proxy_probe_wants_the_features_row() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("atrium-codex-proxy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = |name: &str, out: &str| {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\nprintf '{out}'\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    };
    let has = fake(
        "codex-has",
        "js_repl        stable        true\\nnetwork_proxy  experimental  false\\n",
    );
    let lacks = fake(
        "codex-lacks",
        "respect_network_proxy  under development  false\\n",
    );
    assert!(codex_has_network_proxy(&has));
    assert!(!codex_has_network_proxy(&lacks));
    assert!(!codex_has_network_proxy(
        &dir.join("absent").to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
