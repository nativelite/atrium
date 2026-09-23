//! The prefix scanner: commands, arrows and the mouse.

use atrium::input::{Action, Dir, PrefixScanner};

// --- prefix scanner ---------------------------------------------------------

#[test]
fn plain_bytes_pass_through_untouched() {
    let mut s = PrefixScanner::new();
    assert_eq!(
        s.feed(b"hello \x1b[A world"),
        vec![Action::Forward(b"hello \x1b[A world".to_vec())]
    );
}

#[test]
fn prefix_commands_are_recognized() {
    let mut s = PrefixScanner::new();
    assert_eq!(s.feed(b"\x01n"), vec![Action::NextPane]);
    assert_eq!(s.feed(b"\x01p"), vec![Action::PrevPane]);
    assert_eq!(s.feed(b"\x01c"), vec![Action::NewPane]);
    assert_eq!(s.feed(b"\x01x"), vec![Action::KillPane]);
    assert_eq!(s.feed(b"\x01q"), vec![Action::Quit]);
    assert_eq!(s.feed(b"\x013"), vec![Action::SwitchTo(2)]);
}

#[test]
fn double_prefix_sends_literal_ctrl_a() {
    let mut s = PrefixScanner::new();
    assert_eq!(
        s.feed(b"a\x01\x01b"),
        vec![Action::Forward(b"a\x01b".to_vec())]
    );
}

#[test]
fn prefix_survives_chunk_boundaries() {
    let mut s = PrefixScanner::new();
    assert_eq!(s.feed(b"ab\x01"), vec![Action::Forward(b"ab".to_vec())]);
    assert!(s.armed());
    assert_eq!(s.feed(b"n"), vec![Action::NextPane]);
    assert!(!s.armed());
}

#[test]
fn unknown_command_swallows_prefix_and_byte() {
    // `.` is not a bound command, so the prefix and it are both swallowed.
    let mut s = PrefixScanner::new();
    assert_eq!(s.feed(b"a\x01.b"), vec![Action::Forward(b"ab".to_vec())]);
}

#[test]
fn tiling_commands_are_recognized() {
    let mut s = PrefixScanner::new();
    assert_eq!(s.feed(b"\x01\""), vec![Action::SplitH]);
    assert_eq!(s.feed(b"\x01%"), vec![Action::SplitV]);
    assert_eq!(s.feed(b"\x01z"), vec![Action::Zoom]);
    assert_eq!(s.feed(b"\x01h"), vec![Action::MoveFocus(Dir::Left)]);
    assert_eq!(s.feed(b"\x01j"), vec![Action::MoveFocus(Dir::Down)]);
    assert_eq!(s.feed(b"\x01k"), vec![Action::MoveFocus(Dir::Up)]);
    assert_eq!(s.feed(b"\x01l"), vec![Action::MoveFocus(Dir::Right)]);
}

#[test]
fn prefixed_arrow_keys_move_focus() {
    // Ctrl+A then ESC [ C  -> move focus right; the whole three-byte arrow
    // sequence is consumed as one command, and nothing leaks to the pane.
    let mut s = PrefixScanner::new();
    assert_eq!(s.feed(b"\x01\x1b[C"), vec![Action::MoveFocus(Dir::Right)]);
    assert_eq!(s.feed(b"\x01\x1b[A"), vec![Action::MoveFocus(Dir::Up)]);
    assert_eq!(s.feed(b"\x01\x1b[B"), vec![Action::MoveFocus(Dir::Down)]);
    assert_eq!(s.feed(b"\x01\x1b[D"), vec![Action::MoveFocus(Dir::Left)]);
}

#[test]
fn prefixed_arrow_survives_chunk_boundaries() {
    // The arrow's bytes arrive one per read; the state machine holds across.
    let mut s = PrefixScanner::new();
    assert_eq!(s.feed(b"\x01"), vec![]);
    assert!(s.armed());
    assert_eq!(s.feed(b"\x1b"), vec![]);
    assert_eq!(s.feed(b"["), vec![]);
    assert_eq!(s.feed(b"C"), vec![Action::MoveFocus(Dir::Right)]);
    assert!(!s.armed());
}

#[test]
fn bare_arrow_keys_still_forward_to_the_pane() {
    // Without a prefix, arrows are ordinary bytes for the child (0.1 behavior).
    let mut s = PrefixScanner::new();
    assert_eq!(s.feed(b"\x1b[C"), vec![Action::Forward(b"\x1b[C".to_vec())]);
}

#[test]
fn commands_split_surrounding_forwards_in_order() {
    let mut s = PrefixScanner::new();
    assert_eq!(
        s.feed(b"ls\r\x01ncat\r"),
        vec![
            Action::Forward(b"ls\r".to_vec()),
            Action::NextPane,
            Action::Forward(b"cat\r".to_vec()),
        ]
    );
}

// --- mouse mode (Ctrl+A m + SGR click parsing) ------------------------------

#[test]
fn ctrl_a_m_toggles_mouse() {
    let mut s = PrefixScanner::new();
    assert_eq!(s.feed(b"\x01m"), vec![Action::ToggleMouse]);
}

#[test]
fn mouse_off_forwards_sgr_sequences_untouched() {
    // Default (mouse off): a bare SGR-looking sequence is just forwarded to the
    // pane, and a lone ESC is never intercepted/delayed.
    let mut s = PrefixScanner::new();
    assert_eq!(
        s.feed(b"\x1b[<0;5;9M"),
        vec![Action::Forward(b"\x1b[<0;5;9M".to_vec())]
    );
}

#[test]
fn mouse_on_left_press_becomes_a_click() {
    let mut s = PrefixScanner::new();
    s.set_mouse(true);
    assert_eq!(
        s.feed(b"\x1b[<0;12;7M"),
        vec![Action::MouseClick { col: 12, row: 7 }]
    );
}

#[test]
fn mouse_on_release_and_wheel_and_drag_are_not_clicks() {
    let mut s = PrefixScanner::new();
    s.set_mouse(true);
    // A left release ends a tile selection; it is not a click.
    assert_eq!(
        s.feed(b"\x1b[<0;12;7m"),
        vec![Action::MouseRelease { col: 12, row: 7 }]
    );
    // Wheel-up is not a click — it scrolls the hovered tile (the scroll feature).
    assert_eq!(
        s.feed(b"\x1b[<64;1;1M"),
        vec![Action::MouseScroll {
            up: true,
            col: 1,
            row: 1
        }]
    );
    // A left drag extends a tile selection; it is not a click.
    assert_eq!(
        s.feed(b"\x1b[<32;1;1M"),
        vec![Action::MouseDrag { col: 1, row: 1 }]
    );
    // Other buttons' drags and releases are still ignored.
    assert_eq!(s.feed(b"\x1b[<34;1;1M"), vec![]); // right-button drag
    assert_eq!(s.feed(b"\x1b[<2;1;1m"), vec![]); // right-button release
}

#[test]
fn mouse_on_still_forwards_a_real_arrow_key() {
    // With mouse on, a non-mouse escape (arrow key ESC [ C) still reaches the pane.
    let mut s = PrefixScanner::new();
    s.set_mouse(true);
    assert_eq!(s.feed(b"\x1b[C"), vec![Action::Forward(b"\x1b[C".to_vec())]);
}

#[test]
fn mouse_click_survives_chunk_boundaries() {
    let mut s = PrefixScanner::new();
    s.set_mouse(true);
    assert_eq!(s.feed(b"\x1b[<0;3"), vec![]);
    assert_eq!(s.feed(b";4M"), vec![Action::MouseClick { col: 3, row: 4 }]);
}
