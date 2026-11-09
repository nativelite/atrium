//! The loop's pure decisions.

use super::resize::adopt_size;
use super::*;

#[test]
fn a_resize_to_the_same_size_is_not_adopted() {
    assert!(!adopt_size((24, 80), (24, 80)));
}

#[test]
fn a_wider_narrower_or_taller_terminal_is_adopted() {
    assert!(adopt_size((24, 100), (24, 80)));
    assert!(adopt_size((24, 60), (24, 80)));
    assert!(adopt_size((30, 80), (24, 80)));
}

/// ConPTY reports a few rows fewer once atrium is in the alternate screen; a
/// height-only shrink of up to `ALT_SCREEN_RESERVE_ROWS` is that, not a resize.
#[test]
fn a_small_height_only_shrink_is_the_alt_screen_reservation() {
    assert!(!adopt_size((20, 80), (24, 80)));
    assert!(!adopt_size((24 - ALT_SCREEN_RESERVE_ROWS, 80), (24, 80)));
    assert!(adopt_size((24 - ALT_SCREEN_RESERVE_ROWS - 1, 80), (24, 80)));
    // With the width changing too, it is a real resize.
    assert!(adopt_size((20, 70), (24, 80)));
}

/// Below three rows there is no pane to show, and the passthrough double resize
/// (to `rows - 2`, then `rows - 1`) needs two distinct sizes.
#[test]
fn a_terminal_under_three_rows_is_not_adopted() {
    assert!(!adopt_size((2, 100), (24, 80)));
    assert!(adopt_size((3, 100), (24, 80)));
}

#[test]
fn the_active_window_follows_the_windows_removed_before_it() {
    assert_eq!(active_after_reap(2, 1, 3), 1);
    assert_eq!(active_after_reap(2, 0, 3), 2);
    assert_eq!(active_after_reap(0, 0, 1), 0);
}

#[test]
fn the_active_window_stays_inside_what_is_left() {
    // The active window itself was removed, and it was the last one.
    assert_eq!(active_after_reap(3, 0, 3), 2);
    assert_eq!(active_after_reap(1, 2, 4), 0);
}

/// Once the accept thread is gone, `poll` errs on every tick: the loop says so
/// once, names the remedy, and lets the listener go — so there is no second
/// poll to say it again. A real listener stands in for the one the loop owns;
/// the error is the one `nativelite-ipc` hands back.
#[test]
fn a_lost_ctl_channel_is_said_once_and_the_listener_let_go() {
    let addr = atrium::ipc::address_for(u32::MAX - 13);
    let mut listener = Some(atrium::ipc::Listener::bind(&addr).expect("bind a test endpoint"));
    let mut held = Vec::new();
    let mut flash = None;
    let err = std::io::Error::other("the ctl accept thread has exited");

    ctl_channel_lost(&mut listener, &mut held, &mut flash, &err);
    assert!(listener.is_none(), "the dead listener is let go");
    let (text, _) = flash.take().expect("said on the bar");
    assert!(
        text.starts_with("ctl channel lost: the ctl accept thread has exited"),
        "names the cause: {text}"
    );
    assert!(text.contains("restart atrium"), "names the remedy: {text}");

    // The next tick finds no listener: nothing to poll, nothing to say again.
    ctl_channel_lost(&mut listener, &mut held, &mut flash, &err);
    assert!(flash.is_none(), "said once");
}
