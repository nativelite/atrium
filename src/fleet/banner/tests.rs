//! Terminal-safe rendering of file-supplied strings.

use super::*;

#[test]
fn sanitize_defangs_controls_and_bidi_but_leaves_a_windows_path_alone() {
    let out = sanitize("clear\x1b[2Jhere\x07\nand\u{202e}back\u{2028}x\u{7f}");
    assert!(!out.chars().any(|c| c.is_control()), "{out}");
    assert!(out.contains("\\u{1b}") && out.contains("\\u{7}") && out.contains("\\u{a}"));
    assert!(
        out.contains("\\u{202e}"),
        "a bidi override reorders a path: {out}"
    );
    assert!(out.contains("\\u{2028}"), "{out}");
    assert!(out.contains("\\u{7f}"), "{out}");
    // Backslashes are NOT doubled: that mangles every Windows path in the
    // banner, on the platform with the longest paths.
    assert_eq!(sanitize(r"C:\Users\dev\repo"), r"C:\Users\dev\repo");
    assert_eq!(sanitize(r"\\?\C:\proj"), r"\\?\C:\proj");
    assert_eq!(sanitize("ordinary-name"), "ordinary-name");
}

#[test]
fn shorten_elides_the_middle_so_the_destination_survives() {
    // Cutting the tail hides where a grant goes, which is the one fact the
    // line exists to carry.
    let long = format!("/a/{}/secrets-here", "x".repeat(300));
    let out = shorten(&long, 40);
    assert!(out.chars().count() <= 40, "{out}");
    assert!(out.ends_with("secrets-here"), "{out}");
    assert!(out.starts_with("/a/"), "{out}");
    assert!(out.contains('…'), "an elision must be visible: {out}");
    assert_eq!(shorten("short", 40), "short");
}

#[test]
fn preflight_warnings_are_a_loud_block_on_a_terminal_and_plain_in_a_log() {
    let warnings = vec![
        "40 agents is over the pane cap".to_string(),
        "ctl is on but no agent may spawn".to_string(),
    ];
    assert!(
        preflight_block(&[], true).is_empty(),
        "nothing to warn about"
    );

    let loud = preflight_block(&warnings, true);
    // Header band, one bar per warning, a closing rule (plus the spacer).
    assert_eq!(loud.len(), warnings.len() + 3, "{loud:#?}");
    assert!(loud[1].contains("PREFLIGHT · 2 warnings · launching anyway"));
    assert!(
        loud[1].contains("\x1b[1;30;43m"),
        "the header is a yellow band"
    );
    for (line, w) in loud[2..4].iter().zip(&warnings) {
        assert!(line.starts_with("\x1b[1;33m┃") && line.contains(w.as_str()));
    }
    assert!(loud[4].contains('┗'));

    // A log gets greppable, escape-free lines instead.
    let plain = preflight_block(&warnings, false);
    assert_eq!(
        plain,
        vec![
            "atrium fleet: warning: 40 agents is over the pane cap",
            "atrium fleet: warning: ctl is on but no agent may spawn",
        ]
    );
    assert!(plain.iter().all(|l| !l.contains('\x1b')));
}
