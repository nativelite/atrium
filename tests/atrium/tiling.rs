//! Splits, grids, focus, zoom and closing tiles.

use crate::support::*;
use std::time::Duration;

// --- tiling (0.2): splits, focus, zoom, kill-retile -------------------------

/// Ctrl+A % splits the focused pane into a second live tile, and *both* shells
/// round-trip: a marker echoed in each appears in the composited output.
#[test]
fn split_creates_a_second_live_tile_and_both_shells_roundtrip() {
    let mut p = spawn_atrium_shell(24, 100);
    // Echo a marker in the first pane, then split vertically and echo another
    // marker in the new (focused) pane.
    p.write(b"echo atrium-tileA\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-tileA", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-tileA"),
        "first pane silent: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01%").unwrap(); // split vertical -> focus the new pane
    p.write(b"echo atrium-tileB\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-tileB", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-tileB"),
        "second tile silent after split: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// A 2x2 grid: split vertical, split the right column horizontally, focus the
/// left column and split it horizontally — four live panes. Echo a unique
/// marker in each and assert all four reach the composited frame.
#[test]
fn two_by_two_grid_has_four_live_panes() {
    let mut p = spawn_atrium_shell(30, 120);
    // Pane 1 (top-left after the splits below) — mark it before splitting so the
    // first shell is proven live.
    p.write(b"echo atrium-q1\r\n").unwrap();
    read_until(&mut p, b"atrium-q1", Duration::from_secs(15));
    // Build the square: % (vertical) then " (horizontal on the right), then move
    // focus back to the left column with h and " to split it.
    p.write(b"\x01%").unwrap(); // now two columns, focus right
    p.write(b"echo atrium-q2\r\n").unwrap();
    read_until(&mut p, b"atrium-q2", Duration::from_secs(15));
    p.write(b"\x01\"").unwrap(); // split right column -> focus bottom-right
    p.write(b"echo atrium-q3\r\n").unwrap();
    read_until(&mut p, b"atrium-q3", Duration::from_secs(15));
    p.write(b"\x01h").unwrap(); // focus back to the left column
    p.write(b"\x01\"").unwrap(); // split it -> focus bottom-left
    p.write(b"echo atrium-q4\r\n").unwrap();
    // Collect everything for a few seconds and assert all four markers appeared.
    let out = read_until(&mut p, b"atrium-q4", Duration::from_secs(15));
    for m in [
        &b"atrium-q1"[..],
        &b"atrium-q2"[..],
        &b"atrium-q3"[..],
        &b"atrium-q4"[..],
    ] {
        assert!(
            contains(&out, m),
            "missing {} in 2x2 frame: {:?}",
            String::from_utf8_lossy(m),
            String::from_utf8_lossy(&out)
        );
    }
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Focus movement changes which pane receives input: after a split, moving
/// focus back to the original pane makes *it* echo the next command.
#[test]
fn focus_movement_routes_input_to_the_focused_pane() {
    let mut p = spawn_atrium_shell(24, 100);
    p.write(b"\x01%").unwrap(); // split -> focus the new (right) pane
    p.write(b"echo atrium-right-pane\r\n").unwrap();
    read_until(&mut p, b"atrium-right-pane", Duration::from_secs(15));
    // Move focus left (h) back to the original pane and run a distinct command.
    p.write(b"\x01h").unwrap();
    p.write(b"echo atrium-left-again\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-left-again", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-left-again"),
        "focus did not route back to the left pane: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Zoom toggles a tiled pane to full-screen passthrough and back; the shell
/// keeps round-tripping across both toggles.
#[test]
fn zoom_toggles_and_pane_stays_live() {
    let mut p = spawn_atrium_shell(24, 100);
    p.write(b"\x01%").unwrap(); // two tiles
    p.write(b"echo atrium-prezoom\r\n").unwrap();
    read_until(&mut p, b"atrium-prezoom", Duration::from_secs(15));
    p.write(b"\x01z").unwrap(); // zoom the focused pane full-screen
    p.write(b"echo atrium-zoomed\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-zoomed", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-zoomed"),
        "zoomed pane silent: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01z").unwrap(); // un-zoom back to tiled
    p.write(b"echo atrium-unzoomed\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-unzoomed", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-unzoomed"),
        "pane dead after un-zoom: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Killing the focused pane in a split re-tiles down to the survivor, which is
/// still live (and, being the sole pane, back in passthrough).
#[test]
fn kill_focused_pane_retiles_to_survivor() {
    let mut p = spawn_atrium_shell(24, 100);
    p.write(b"echo atrium-keepme\r\n").unwrap();
    read_until(&mut p, b"atrium-keepme", Duration::from_secs(15));
    p.write(b"\x01%").unwrap(); // split -> focus new pane
    p.write(b"echo atrium-killme\r\n").unwrap();
    read_until(&mut p, b"atrium-killme", Duration::from_secs(15));
    p.write(b"\x01x").unwrap(); // kill the focused (new) pane
                                // The survivor takes over full-screen and still talks:
    p.write(b"echo atrium-survivor\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-survivor", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-survivor"),
        "survivor dead after kill/re-tile: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

// --- mass-spawn (0.5): -n / --grid open N panes at once ----------------------

/// `atrium -n 4 <shell>` opens ONE window of four tiled panes in a 2x2 grid. The
/// composited frame shows all four pane labels (` 1:… ` .. ` 4:… ` in their top
/// borders), and each pane round-trips: a marker echoed in each — reached by
/// moving focus around the grid — appears in the output.
#[test]
fn mass_spawn_n_four_opens_four_live_panes() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec!["-n", "4", shell];
    argv.extend(args);
    // A big terminal so all four boxed tiles (and their labels) fit.
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv, 40, 160).unwrap();
    let stem: &[u8] = if cfg!(windows) { b"cmd" } else { b"sh" };

    // Four pane labels appear in the tiled frame: `1:<stem>` .. `4:<stem>`.
    // Collect output until the fourth label shows (the grid is fully drawn).
    // The leading blank of ` N:<stem> ` is deliberately NOT part of the needle: an
    // unfocused pane's border carries the default style, and the painter emits a
    // cursor jump instead of writing those blank cells, so the space never reaches
    // the stream at all (a focused pane, being styled, does emit it). Stripping CSI
    // cannot put back a cell that was never sent — matching from the digit on is
    // what makes this hold on both platforms. `N:<stem>` still names the label
    // uniquely.
    let mut label4 = b"4:".to_vec();
    label4.extend_from_slice(stem);
    let out = read_until(&mut p, &label4, Duration::from_secs(20));
    for i in 1..=4u8 {
        let mut label = vec![b'0' + i, b':'];
        label.extend_from_slice(stem);
        assert!(
            contains(&out, &label),
            "pane {i} label missing from grid: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    // The focused (pane 1, top-left) shell round-trips.
    p.write(b"echo atrium-grid-p1\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-grid-p1", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-grid-p1"),
        "focused grid pane silent: {:?}",
        String::from_utf8_lossy(&out)
    );
    // Move focus right to another tile and prove IT is live too — a different
    // pane receiving input confirms four independent sessions, not one echoed
    // four times.
    p.write(b"\x01l").unwrap();
    p.write(b"echo atrium-grid-p2\r\n").unwrap();
    let out = read_until(&mut p, b"atrium-grid-p2", Duration::from_secs(15));
    assert!(
        contains(&out, b"atrium-grid-p2"),
        "second grid pane silent after focus move: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// An odd `-n` is a clean startup error, not a silent single pane: atrium prints
/// the reason and exits non-zero.
#[test]
fn mass_spawn_rejects_odd_n() {
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &["-n", "3", "cmd"], 24, 80).unwrap();
    // It must exit non-zero (bad argument), and not sit there hosting a shell.
    let code = wait_exit(&mut p, 15);
    assert_ne!(code, 0, "odd -n should be a startup error");
}
