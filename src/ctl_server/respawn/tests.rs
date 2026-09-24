//! Restarting a pane: the argv a respawn replays.

use super::*;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// A respawn restarts the agent that was running — its model, its kickoff —
/// as a new session with no permission flags of its own.
#[test]
fn a_respawned_claude_keeps_its_arguments_but_not_its_session() {
    let argv = args(&[
        "claude",
        "--model",
        "opus",
        "--resume",
        "abc",
        "--dangerously-skip-permissions",
        "--session-id=def",
        "-c",
        "You lead. Read PLAN.md.",
    ]);
    assert_eq!(
        respawn_argv(&argv, "claude"),
        args(&["claude", "--model", "opus", "You lead. Read PLAN.md."])
    );
}

#[test]
fn a_respawned_shell_keeps_a_dash_c_command() {
    let argv = args(&["sh", "-c", "make watch"]);
    assert_eq!(respawn_argv(&argv, "sh"), argv);
}

#[test]
fn a_pane_with_no_recorded_command_respawns_its_title() {
    assert_eq!(respawn_argv(&[], "claude"), args(&["claude"]));
}
