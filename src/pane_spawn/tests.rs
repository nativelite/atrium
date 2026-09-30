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
