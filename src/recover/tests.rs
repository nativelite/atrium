//! Resume argv, placement, notices and the posture a recovery runs at.

use atrium::ctl::TrustMode;

// --- resume_argv --------------------------------------------------------

fn pane_record(argv: Vec<String>, session_id: Option<String>) -> atrium::session::PaneRecord {
    atrium::session::PaneRecord {
        id: 0,
        role: None,
        argv,
        cwd: None,
        identity: None,
        session_id,
        worktree: None,
        deny: Vec::new(),
        can_spawn: true,
        depth: 0,
        parent_pane: None,
        mode: None,
        kickoff: false,
        norms: None,
        context_env: Vec::new(),
    }
}

#[test]
fn resume_argv_claude_session_id_appends_resume_no_session_id_flag() {
    let r = pane_record(vec!["claude".into()], Some("abc-123".into()));
    let got = super::resume_argv(&r);
    assert_eq!(got, vec!["claude", "--resume", "abc-123"]);
    assert!(!got.iter().any(|a| a == "--session-id"));
}

#[test]
fn resume_argv_claude_continue_stripped_and_replaced_with_resume() {
    let r = pane_record(
        vec!["claude".into(), "--continue".into()],
        Some("abc-123".into()),
    );
    let got = super::resume_argv(&r);
    assert!(!got.contains(&"--continue".to_string()));
    assert!(got.contains(&"--resume".to_string()));
    assert!(got.contains(&"abc-123".to_string()));
}

#[test]
fn resume_argv_non_claude_verbatim() {
    let r = pane_record(vec!["bash".into(), "-l".into()], Some("any-id".into()));
    let got = super::resume_argv(&r);
    assert_eq!(got, vec!["bash", "-l"]);
}

#[test]
fn resume_argv_claude_no_session_id_unchanged() {
    let r = pane_record(vec!["claude".into(), "--continue".into()], None);
    let got = super::resume_argv(&r);
    // no session_id → argv returned verbatim, --continue preserved
    assert_eq!(got, vec!["claude", "--continue"]);
}

/// A pane whose directory is gone starts in the project directory, loses
/// the worktree instructions that would now be false, and says so; one
/// whose directory exists keeps both, quietly.
#[test]
fn a_pane_whose_directory_is_gone_starts_in_the_project_without_its_norms() {
    let mut r = pane_record(vec!["claude".into()], None);
    r.role = Some("builder".into());
    r.cwd = Some("/wt/piece".into());
    r.norms = Some("You are in git worktree piece".into());
    let gone = super::resume_place(&r, |_| false);
    assert_eq!(gone.cwd, None);
    assert_eq!(gone.norms, None);
    let w = gone.warning.expect("the operator is told");
    assert!(w.starts_with("builder: "), "{w}");
    assert!(w.contains("/wt/piece is gone"), "{w}");
    let kept = super::resume_place(&r, |_| true);
    assert_eq!(kept.cwd.as_deref(), Some("/wt/piece"));
    assert_eq!(kept.norms.as_deref(), Some("You are in git worktree piece"));
    assert_eq!(kept.warning, None);
    // No directory recorded: nothing to check, nothing to say.
    r.cwd = None;
    let none = super::resume_place(&r, |_| false);
    assert_eq!(none.cwd, None);
    assert_eq!(none.warning, None);
}

#[test]
fn resume_argv_empty_argv_no_panic() {
    let r = pane_record(vec![], Some("any-id".into()));
    let got = super::resume_argv(&r);
    assert!(got.is_empty());
}

fn notice_panes(n: usize) -> Vec<atrium::session::PaneRecord> {
    (0..n)
        .map(|_| pane_record(vec!["claude".into()], None))
        .collect()
}

/// The consent line must show what a snapshot actually controls. Approving a
/// pane *count* let a planted file run any command with any spawn right; each
/// pane's command, posture, spawn right and own deny rules are now listed, a
/// governed flag the file tried to smuggle is named as ignored, and a role
/// carrying an escape sequence cannot repaint the prompt.
#[test]
fn the_resume_notice_lists_what_each_pane_will_run_and_may_do() {
    let mut lead = pane_record(
        vec![
            "claude".into(),
            "--allowedTools".into(),
            "Bash(*)".into(),
            "--model".into(),
            "opus".into(),
            "You are the lead of a long-running fleet.".into(),
        ],
        Some("t-lead".into()),
    );
    lead.role = Some("lead\u{1b}[2J".to_string());
    lead.can_spawn = true;
    lead.mode = Some(TrustMode::Skip);
    let mut worker = pane_record(vec!["claude".into()], None);
    worker.role = Some("builder".into());
    worker.can_spawn = false;
    worker.deny = vec!["git push".into()];
    let notice = super::recovery_notice(&[lead, worker], None, false, TrustMode::Edits);
    assert!(
        !notice.contains('\u{1b}'),
        "an ESC from the file reached the terminal"
    );
    assert!(notice.contains("--model opus"), "flags are shown: {notice}");
    assert!(
        !notice.contains("Bash(*)"),
        "a governed flag is not part of what will run: {notice}"
    );
    assert!(notice.contains("ignored from the saved command: --allowedTools"));
    assert!(
        notice.contains("<41 chars>"),
        "a prompt is reduced to its length"
    );
    assert!(notice.contains("--resume t-lead"));
    assert!(
        notice.contains("mode accept"),
        "a recorded `skip` is shown capped to the ceiling: {notice}"
    );
    assert!(notice.contains("may spawn teammates") && notice.contains("cannot spawn"));
    assert!(notice.contains("1 own deny rule(s)"));
}

/// A kickoff is the agent's opening instructions. Resuming a transcript the
/// agent is mid-task, so the kickoff is left out; starting fresh (nothing to
/// resume) it is exactly what the agent needs, so it stays.
#[test]
fn a_kickoff_is_replayed_only_when_the_agent_starts_fresh() {
    let argv = vec![
        "claude".into(),
        "--model".into(),
        "opus".into(),
        "Start by reading CLAUDE.md".into(),
    ];
    let mut resumed = pane_record(argv.clone(), Some("t-1".into()));
    resumed.kickoff = true;
    assert_eq!(
        super::resume_argv(&resumed),
        vec!["claude", "--model", "opus", "--resume", "t-1"]
    );
    let mut fresh = pane_record(argv.clone(), None);
    fresh.kickoff = true;
    assert_eq!(
        super::resume_argv(&fresh),
        argv,
        "no transcript: keep the kickoff"
    );
    // Another vendor's pane can carry an adopted session id, but nothing
    // resumes it — it starts a new session and needs its kickoff.
    let mut codex = pane_record(
        vec!["codex".into(), "Start by reading AGENTS.md".into()],
        Some("adopted".into()),
    );
    codex.kickoff = true;
    assert_eq!(
        super::resume_argv(&codex),
        vec!["codex", "Start by reading AGENTS.md"],
        "a non-claude pane keeps its kickoff"
    );
    let not_a_kickoff = pane_record(argv, Some("t-2".into()));
    assert!(
        super::resume_argv(&not_a_kickoff).contains(&"Start by reading CLAUDE.md".to_string()),
        "only a pane marked as having a kickoff loses its last argument"
    );
}

/// The consent notice names the parts of a resume the command line does not
/// show: worktree instructions and the context store.
#[test]
fn the_resume_notice_names_worktree_instructions_and_the_context_store() {
    let mut p = pane_record(vec!["claude".into()], None);
    p.norms = Some("stay in the worktree".into());
    p.context_env = vec![("CONTEXT_MODE_DIR".into(), "/ctx".into())];
    let notice = super::recovery_notice(&[p], None, false, TrustMode::Edits);
    assert!(
        notice.contains("worktree instructions \"stay in the worktree"),
        "the instructions are previewed, not just named: {notice}"
    );
    assert!(
        notice.contains("CONTEXT_MODE_DIR=/ctx"),
        "the context store path is shown: {notice}"
    );
}

/// A pane that was itself resumed carries `--resume <id>` in its argv; the next
/// resume must replace it, not append a second one.
#[test]
fn resuming_a_resumed_pane_keeps_exactly_one_session_argument() {
    let r = pane_record(
        vec![
            "claude".into(),
            "--model".into(),
            "opus".into(),
            "--resume".into(),
            "old".into(),
            "--session-id=older".into(),
            "-c".into(),
        ],
        Some("current".into()),
    );
    assert_eq!(
        super::resume_argv(&r),
        vec!["claude", "--model", "opus", "--resume", "current"]
    );
}

/// Governed permission flags in a saved command never reach the agent: the
/// posture comes from the policy the operator confirmed.
#[test]
fn a_saved_command_cannot_carry_its_own_permission_flags() {
    let r = pane_record(
        vec![
            "claude".into(),
            "--dangerously-skip-permissions".into(),
            "--permission-mode".into(),
            "bypassPermissions".into(),
        ],
        None,
    );
    assert_eq!(super::resume_argv(&r), vec!["claude"]);
}

// --- find_latest_snapshot -----------------------------------------------

#[test]
fn flag_typed_sees_bare_and_glued_forms_only() {
    let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(super::flag_typed(
        &a(&["--trust", "automode"]),
        &["--trust"]
    ));
    assert!(super::flag_typed(&a(&["--max-depth=3"]), &["--max-depth"]));
    assert!(super::flag_typed(
        &a(&["--skip-permissions"]),
        &["--trust", "--skip-permissions"]
    ));
    assert!(
        !super::flag_typed(&a(&["--allow-ctl"]), &["--trust"]),
        "an unrelated flag is not a trust choice"
    );
    assert!(
        !super::flag_typed(&a(&[]), &["--trust"]),
        "recovering with no flags defers to the snapshot"
    );
}

/// The precedence rule: what the operator typed wins, what the snapshot
/// recorded fills the gap, and only then the parser's default. Without the
/// middle step a bare `atrium recover` silently downgraded an `automode`
/// fleet to the default posture.
#[test]
fn recovered_trust_prefers_the_typed_flag_then_the_snapshot() {
    assert_eq!(
        super::recovered_trust(true, TrustMode::Plan, Some(TrustMode::Auto)),
        TrustMode::Plan,
        "a typed flag wins over the snapshot"
    );
    assert_eq!(
        super::recovered_trust(false, TrustMode::Off, Some(TrustMode::Auto)),
        TrustMode::Auto,
        "no flag: the recorded posture"
    );
    assert_eq!(
        super::recovered_trust(false, TrustMode::Off, None),
        TrustMode::Off,
        "no flag and no record: the parsed default"
    );
}

/// A pane comes back where it was, and a snapshot can never promote one: the
/// ceiling is applied to the recorded posture, not replaced by it.
#[test]
fn recovered_pane_mode_restores_below_the_ceiling_and_caps_above_it() {
    assert_eq!(
        super::recovered_pane_mode(Some(TrustMode::Edits), TrustMode::Auto),
        TrustMode::Edits,
        "a de-escalated pane must not be promoted to the ceiling"
    );
    assert_eq!(
        super::recovered_pane_mode(Some(TrustMode::Skip), TrustMode::Plan),
        TrustMode::Plan,
        "a snapshot must never climb above the session ceiling"
    );
    assert_eq!(
        super::recovered_pane_mode(None, TrustMode::Auto),
        TrustMode::Auto,
        "an unrecorded posture falls back to the ceiling"
    );
}

/// A pre-policy snapshot must SAY that the guards are not coming back. The
/// silent version of this is the bug: a recovered fleet that looks identical
/// and runs with its deny list, compile pool and memory cap gone.
#[test]
fn recovery_notice_names_the_guards_or_warns_they_are_missing() {
    let policy = atrium::session::PolicyRecord {
        deny: vec!["git push".to_string()],
        claude_aliases: Vec::new(),
        build_jobs: Some(10),
        memory_mb: Some(32768),
        trust: Some(TrustMode::Auto),
        allow_ctl: true,
        max_depth: 6,
        topics: None,
    };
    let full = super::recovery_notice(&notice_panes(4), Some(&policy), false, TrustMode::Auto);
    for expected in [
        "4 pane(s)",
        "trust automode",
        "ctl on (max depth 6)",
        "1 session deny rule(s)",
        "build pool 10",
        "memory cap 32768 MB",
    ] {
        assert!(
            full.contains(expected),
            "{full:?} must mention {expected:?}"
        );
    }
    let legacy = super::recovery_notice(&notice_panes(4), None, true, TrustMode::Off);
    assert!(
        legacy.contains("NOT restored"),
        "a v1 snapshot must warn, not pretend: {legacy:?}"
    );
}

/// The posture is the one value a recovery can raise above the flags the
/// operator typed, and the snapshot lives in a shared temp directory — so
/// when it comes from the file, the line that precedes the confirmation
/// prompt has to say so.
#[test]
fn recovery_notice_marks_a_posture_that_came_from_the_file() {
    let policy = atrium::session::PolicyRecord {
        deny: Vec::new(),
        claude_aliases: Vec::new(),
        build_jobs: None,
        memory_mb: None,
        trust: Some(TrustMode::Auto),
        allow_ctl: true,
        max_depth: usize::MAX,
        topics: None,
    };
    let from_file = super::recovery_notice(&notice_panes(1), Some(&policy), true, TrustMode::Auto);
    assert!(
        from_file.contains("trust automode (from the snapshot)"),
        "{from_file:?}"
    );
    assert!(
        from_file.contains("max depth unlimited"),
        "an unlimited guard must read as unlimited, not as a 19-digit number: {from_file:?}"
    );
    let typed = super::recovery_notice(&notice_panes(1), Some(&policy), false, TrustMode::Auto);
    assert!(
        typed.contains("trust automode") && !typed.contains("from the snapshot"),
        "a posture the operator typed is not attributed to the file: {typed:?}"
    );
}

/// Enter is yes; only an explicit no declines. A resume offer the operator
/// dismisses by reflex would otherwise be gone for good.
#[test]
fn a_resume_prompt_defaults_to_yes_and_only_no_declines() {
    for yes in ["\n", "y\n", "Yes\r\n", "sure\n"] {
        assert!(super::resume_answer(yes), "{yes:?} should resume");
    }
    for no in ["n\n", "N", " no \r\n", "NO"] {
        assert!(!super::resume_answer(no), "{no:?} should decline");
    }
}

/// The offer is for the operator at a terminal: never from inside a pane (an
/// agent launching atrium must not be handed the operator's crashed session),
/// never without a terminal, never when switched off.
#[test]
fn the_resume_offer_needs_an_operator_at_a_terminal() {
    assert!(super::offer_enabled(None, false, true));
    assert!(super::offer_enabled(Some("1"), false, true));
    assert!(
        !super::offer_enabled(Some("0"), false, true),
        "switched off"
    );
    assert!(
        !super::offer_enabled(None, true, true),
        "inside an atrium pane"
    );
    assert!(
        !super::offer_enabled(None, false, false),
        "no terminal to ask on"
    );
}

#[test]
fn ages_read_at_human_scale() {
    assert_eq!(super::human_age(42_000), "42s");
    assert_eq!(super::human_age(7 * 60_000), "7m");
    assert_eq!(super::human_age(3 * 3_600_000), "3h");
    assert_eq!(super::human_age(2 * 86_400_000), "2d");
}
