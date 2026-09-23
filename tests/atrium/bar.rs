//! The status bar.

use atrium::bar::{bar_paint, bar_text, Attention, PaneInfo};

// --- bar --------------------------------------------------------------------

fn info(title: &str, active: bool, activity: bool, exited: bool) -> PaneInfo {
    PaneInfo {
        title: title.into(),
        active,
        attention: Attention::from_facts(exited, false, activity),
        identity: None,
        role: None,
    }
}

/// A window whose bound agent is waiting on the human.
fn waiting_info(title: &str, active: bool) -> PaneInfo {
    PaneInfo {
        title: title.into(),
        active,
        attention: Attention::Waiting,
        identity: None,
        role: None,
    }
}

#[test]
fn bar_shows_every_pane_with_markers() {
    let text = bar_text(
        &[
            info("claude", true, false, false),
            info("cmd", false, true, false),
            info("sh", false, false, true),
        ],
        120,
        "",
    );
    assert!(text.contains("1:claude*"), "{text}");
    assert!(text.contains("2:cmd+"), "{text}");
    assert!(text.contains("3:sh!"), "{text}");
    assert!(text.contains("^A"), "{text}");
    assert_eq!(text.chars().count(), 120);
}

#[test]
fn bar_truncates_and_pads_to_width() {
    let panes = vec![info("a-very-long-title", true, false, false); 6];
    assert_eq!(bar_text(&panes, 40, "").chars().count(), 40);
    assert_eq!(bar_text(&[], 10, "").chars().count(), 10);
}

#[test]
fn bar_note_replaces_keys_help() {
    let panes = [info("claude", true, false, false)];
    let text = bar_text(&panes, 120, "cannot start \"claude\": not found");
    assert!(text.contains("cannot start"), "{text}");
    assert!(!text.contains("c:new"), "{text}");
}

#[test]
fn bar_waiting_agent_shows_question_marker() {
    // A backgrounded window whose agent is blocked: `?` marker, not `-`/`+`.
    let text = bar_text(
        &[
            info("cmd", true, false, false),
            waiting_info("claude", false),
        ],
        120,
        "",
    );
    assert!(text.contains("2:claude?"), "{text}");
    assert!(text.contains("| 1 waiting"), "fleet note missing: {text}");
}

#[test]
fn bar_exit_outranks_waiting_marker() {
    // Priority: `!` (exited) beats `?` (waiting) beats `*`/`+`/`-`.
    // A waiting agent in a window whose children have all exited: exit wins.
    let mut p = waiting_info("claude", false);
    p.attention = Attention::from_facts(true, true, false);
    let text = bar_text(&[p], 120, "");
    assert!(text.contains("1:claude!"), "exit should win: {text}");
    // An exited window is a dead child, not a live waiter: it neither shows `?`
    // nor counts toward the fleet note.
    assert!(!text.contains("waiting"), "exited is not a waiter: {text}");
}

#[test]
fn bar_waiting_in_active_window_gets_no_fleet_note() {
    // The fleet note surfaces waiters you are NOT looking at. A waiting agent
    // in the *active* window needs no count — you are already there.
    let text = bar_text(&[waiting_info("claude", true)], 120, "");
    assert!(text.contains("1:claude?"), "{text}");
    assert!(
        !text.contains("waiting |") && !text.contains("| 1 waiting"),
        "{text}"
    );
}

#[test]
fn bar_counts_multiple_backgrounded_waiters() {
    let text = bar_text(
        &[
            info("cmd", true, false, false),
            waiting_info("claude", false),
            waiting_info("claude", false),
        ],
        160,
        "",
    );
    assert!(text.contains("| 2 waiting"), "{text}");
}

#[test]
fn bar_shows_identity_name_tag_next_to_the_window_entry() {
    // A window whose agent runs under the `work` identity shows `·work` next to
    // its entry — the name only, never a secret, and the text is always present.
    let mut p = info("claude", true, false, false);
    p.identity = Some("work".into());
    let text = bar_text(&[p], 120, "");
    assert!(
        text.contains("1:claude*·work"),
        "identity tag missing: {text}"
    );
}

#[test]
fn bar_wif_identity_reads_apart_from_static() {
    let mut p = info("claude", true, false, false);
    p.identity = Some("wif:prod".into());
    let text = bar_text(&[p], 120, "");
    assert!(text.contains("·wif:prod"), "wif tag missing: {text}");
}

#[test]
fn bar_paint_colors_the_identity_tag_as_text_not_a_chip() {
    // The `·<name>` tag must read as colored *text* — the identity color as the
    // foreground over the bar's themed background — NOT a reverse-video filled
    // chip. (The themed statusline sets an explicit bg per segment; a segment's
    // SGR is an absolute `reset + fg + bg`.) The text must stay intact regardless
    // of color (a11y).
    let mut p = info("claude", true, false, false);
    p.identity = Some("work".into());
    let painted = bar_paint(&[p], 25, 120, "");

    // Palette color for "work" maps via `sgr()` to a `90 + (n-8)` fg param
    // (12 -> 94, 13 -> 95). The tag's SGR is an absolute run beginning `reset +
    // fg` (then the themed bar bg); assert on that prefix so the test doesn't
    // couple to the exact bg truecolor value.
    let idx = atrium::identity::palette_index("work");
    let fg_param = 90 + (idx - 8) as u16;
    let tag_prefix = format!("\x1b[0;{fg_param}");
    assert!(
        painted.contains(&tag_prefix),
        "tag should be colored text (reset+fg); prefix {tag_prefix:?} not in {painted:?}"
    );
    // It must NOT be the old reverse-video chip form (`0;7;<fg>`), a filled block.
    let chip_sgr = format!("\x1b[0;7;{fg_param}");
    assert!(
        !painted.contains(&chip_sgr),
        "tag must not be a reverse-video chip; found {chip_sgr:?} in {painted:?}"
    );

    // The `·work` text is present and intact (the load-bearing a11y channel).
    assert!(
        painted.contains("·work"),
        "colored tag text missing: {painted:?}"
    );

    // A fresh absolute SGR reset begins right after the tag text, so neither the
    // color nor any attribute bleeds into the following segment.
    assert!(
        painted.contains("·work\x1b[0;"),
        "next segment not reset after the tag: {painted:?}"
    );
}
