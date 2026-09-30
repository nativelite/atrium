//! The board, decision and activity-log panels.

use super::*;
use crate::test_support::strip_csi;

#[test]
fn feed_scroll_pages_back_through_history() {
    // Six FYI events, a 3-row feed. One row is the scroll hint, so two events
    // show at a time; scroll offsets the window toward older events.
    let mut bus = atrium::bus::Bus::new();
    for i in 1..=6u64 {
        bus.publish(
            "t",
            atrium::bus::Kind::Fyi,
            Some("w"),
            &[("msg".to_string(), format!("e{i}"))],
            i,
        )
        .unwrap();
    }
    let render = |scroll| {
        let mut out = String::new();
        render_fyi_feed(&mut out, &bus, &[], 1, 3, scroll);
        strip_csi(&out)
    };
    // Live (scroll 0): the two newest, not the oldest.
    let top = render(0);
    assert!(top.contains("e6") && top.contains("e5"), "scroll 0: {top}");
    assert!(!top.contains("e1"), "oldest hidden at scroll 0: {top}");
    // Scrolled back two: the window slides to older events.
    let back = render(2);
    assert!(
        back.contains("e4") && back.contains("e3"),
        "scroll 2: {back}"
    );
    assert!(
        !back.contains("e6"),
        "newest scrolled off at scroll 2: {back}"
    );
}

#[test]
fn wrap_to_wraps_within_width_and_marks_overflow() {
    // Fits: two short lines, each within the width.
    let w = wrap_to("alpha beta gamma delta", 11, 3);
    assert!(
        w.iter().all(|l| l.chars().count() <= 11),
        "within width: {w:?}"
    );
    assert_eq!(w.join(" "), "alpha beta gamma delta");
    // Overflows the line budget: the last line ends with an ellipsis.
    let long = "aaa bbb ccc ddd eee fff ggg hhh iii jjj";
    let w2 = wrap_to(long, 7, 2);
    assert_eq!(w2.len(), 2);
    assert!(w2.last().unwrap().ends_with('…'), "overflow marked: {w2:?}");
}

#[test]
fn decision_detail_shows_the_full_question() {
    // The panel line can truncate; the detail bar must show the whole question.
    let mut bus = atrium::bus::Bus::new();
    let q = "Should the initial release be tagged 0.1.0 as a pre-release alpha or 1.0.0 as the first stable release";
    bus.publish(
        "ui",
        atrium::bus::Kind::DecisionNeeded,
        Some("grace"),
        &[("q".to_string(), q.to_string())],
        1,
    )
    .unwrap();
    let decisions = bus.pending_decisions();
    let mut out = String::new();
    render_decision_detail(&mut out, decisions.first(), 24, 80);
    let flat: String = strip_csi(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(flat.contains(q), "full question visible: {flat}");
    assert!(
        flat.contains("#1") && flat.contains("grace"),
        "seq + source shown: {flat}"
    );
}

/// r10 B10: the unit thresholds are exactly where an off-by-one hides.
#[test]
fn ago_switches_units_exactly_at_the_thresholds() {
    let s = |secs: u64| ago(1_000_000_000 + secs * 1000, 1_000_000_000);
    assert_eq!(s(0), "0s");
    assert_eq!(s(59), "59s");
    assert_eq!(s(60), "1m");
    assert_eq!(s(3599), "59m");
    assert_eq!(s(3600), "1h");
    assert_eq!(s(86_399), "23h");
    assert_eq!(s(86_400), "1d");
    // A timestamp from the future (clock skew) saturates, never underflows.
    assert_eq!(ago(1_000, 5_000), "0s");
}

/// r10 B10: wrap_to's edges — empty input, an exact fit, a word longer than
/// the width (kept whole, not broken), and overflow marking.
#[test]
fn wrap_to_edge_cases() {
    assert!(wrap_to("", 10, 3).is_empty());
    assert!(wrap_to("   ", 10, 3).is_empty());
    // "aa bb" is exactly 5 columns: it fits on one line.
    assert_eq!(wrap_to("aa bb cc", 5, 3), vec!["aa bb", "cc"]);
    // An over-width single word is emitted whole on its own line.
    assert_eq!(wrap_to("abcdefghij", 4, 3), vec!["abcdefghij"]);
    // Exactly max_lines of content: no ellipsis.
    assert_eq!(wrap_to("aa bb", 2, 2), vec!["aa", "bb"]);
    // More than fits: the last kept line is cut to width-1 plus the marker.
    assert_eq!(wrap_to("aa bb cc dd", 2, 2), vec!["aa", "b…"]);
}

#[test]
fn ago_formats_coarsely() {
    assert_eq!(ago(10_000, 5_000), "5s");
    assert_eq!(ago(200_000, 20_000), "3m");
    assert_eq!(ago(10_000_000, 2_800_000), "2h");
    assert_eq!(ago(200_000_000, 200_000), "2d");
}

#[test]
fn collect_log_merges_bus_and_board_in_time_order() {
    let mut bus = atrium::bus::Bus::new();
    bus.publish(
        "ui",
        atrium::bus::Kind::Fyi,
        Some("ada"),
        &[("msg".to_string(), "hi".to_string())],
        300,
    )
    .unwrap();
    let mut board = atrium::board::Board::new();
    board.set(
        "title",
        &[("status".to_string(), "done".to_string())],
        Some("lead"),
        100,
    );
    let world = atrium::vendors::AgentState::default(); // no sessions
    let rows = collect_log(&[], &world, &board, &bus);
    assert!(rows.len() >= 2, "bus + board rows present");
    assert!(
        rows.windows(2).all(|w| w[0].ts <= w[1].ts),
        "sorted ascending by ts"
    );
    assert!(rows
        .iter()
        .any(|r| r.who == "lead" && r.text.contains("title")));
    assert!(rows.iter().any(|r| r.who == "ada" && r.text.contains("ui")));
}
