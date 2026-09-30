//! Best-effort failures surface once per streak.

use super::*;

// -- surface_once (r10 B8) -------------------------------------------------

#[test]
fn a_failure_streak_flashes_once_and_success_resets_it() {
    let fail = || -> std::io::Result<u8> {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "disk full"))
    };
    let (mut failing, mut flash) = (false, None);

    assert_eq!(
        surface_once(&mut failing, fail(), "snapshot", &mut flash),
        None
    );
    let first = flash.take().expect("the first failure is surfaced");
    assert_eq!(first.0, "snapshot failed: disk full");

    // Still failing: quiet, so a persistent error doesn't strobe the bar.
    assert_eq!(
        surface_once(&mut failing, fail(), "snapshot", &mut flash),
        None
    );
    assert!(flash.is_none(), "a repeat failure re-flashed");

    // Recovery returns the value and re-arms the next streak.
    assert_eq!(
        surface_once(&mut failing, Ok(7), "snapshot", &mut flash),
        Some(7)
    );
    surface_once(&mut failing, fail(), "snapshot", &mut flash);
    assert!(
        flash.is_some(),
        "a new streak after recovery must surface again"
    );
}
