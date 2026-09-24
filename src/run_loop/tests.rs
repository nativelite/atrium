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
