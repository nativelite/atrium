//! Tests for amux: the pure prefix scanner and bar builder, then the real
//! thing — amux itself spawned inside a `pty`, driven with keystrokes,
//! its passthrough output read back. Deadline-bounded throughout.

use amux::bar::{bar_text, PaneInfo};
use amux::input::{Action, Dir, PrefixScanner};
use std::time::{Duration, Instant};

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

// --- bar --------------------------------------------------------------------

fn info(title: &str, active: bool, activity: bool, exited: bool) -> PaneInfo {
    PaneInfo {
        title: title.into(),
        active,
        activity,
        exited,
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

// --- command resolution (the npm .cmd shim trap) -----------------------------

#[test]
fn resolver_finds_shims_and_flags_shell_hosting() {
    use amux::resolve::{needs_shell, resolve};
    let td = std::env::temp_dir().join(format!("amux-resolve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    std::fs::write(td.join("claude.CMD"), "@echo shim").unwrap();
    std::fs::write(td.join("tool.exe"), "MZ").unwrap();
    let dirs = vec![td.clone()];
    let exts = vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()];

    let shim = resolve("claude", &dirs, &exts).expect("shim found");
    assert!(needs_shell(&shim), "{shim:?}");
    let exe = resolve("tool", &dirs, &exts).expect("exe found");
    assert!(!needs_shell(&exe), "{exe:?}");
    assert_eq!(resolve("missing", &dirs, &exts), None);
    // explicit paths pass through untouched
    assert_eq!(
        resolve("dir\\thing", &dirs, &exts),
        Some(std::path::PathBuf::from("dir\\thing"))
    );
    let _ = std::fs::remove_dir_all(&td);
}

// --- passthrough filter -----------------------------------------------------

#[test]
fn filter_strips_win32_input_mode_requests() {
    use amux::filter::Passthrough;
    let mut f = Passthrough::new();
    assert_eq!(f.feed(b"a\x1b[?9001hb\x1b[?9001lc"), b"abc".to_vec());
}

#[test]
fn filter_passes_other_escapes_untouched() {
    use amux::filter::Passthrough;
    let mut f = Passthrough::new();
    let input = b"\x1b[31mred\x1b[?25l\x1b[?9001x\x1b[2J";
    assert_eq!(f.feed(input), input.to_vec());
}

#[test]
fn filter_survives_splits_at_every_boundary() {
    use amux::filter::Passthrough;
    let input = b"pre\x1b[?9001hmid\x1b[?9001lpost\x1b[?900";
    for cut in 0..=input.len() {
        let mut f = Passthrough::new();
        let mut out = f.feed(&input[..cut]);
        out.extend(f.feed(&input[cut..]));
        // trailing partial candidate is held, everything else is clean
        assert_eq!(out, b"premidpost".to_vec(), "cut at {cut}");
    }
}

// --- end to end: amux inside a pty ------------------------------------------

fn read_until(p: &mut pty::Pty, needle: &[u8], deadline: Duration) -> Vec<u8> {
    let end = Instant::now() + deadline;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    while Instant::now() < end {
        match p
            .read_timeout(&mut buf, Duration::from_millis(200))
            .unwrap()
        {
            Some(0) => break,
            Some(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.windows(needle.len().max(1)).any(|w| w == needle) {
                    break;
                }
            }
            None => {}
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len().max(1)).any(|w| w == needle)
}

fn wait_exit(p: &mut pty::Pty, secs: u64) -> i32 {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut buf = [0u8; 8192];
    loop {
        if let Some(code) = p.try_wait().unwrap() {
            return code;
        }
        assert!(Instant::now() < end, "amux did not exit in time");
        // Keep draining like a real terminal would: a host that stops
        // reading would block any child mid-write.
        let _ = p.read_timeout(&mut buf, Duration::from_millis(100));
    }
}

/// amux hosting a one-shot command: output passes through, and when the
/// only pane's child exits, amux itself exits cleanly.
#[test]
fn passthrough_and_auto_exit() {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut p = pty::Pty::spawn(
        env!("CARGO_BIN_EXE_amux"),
        &[shell, flag, "echo amux-e2e-marker"],
        24,
        80,
    )
    .unwrap();
    let out = read_until(&mut p, b"amux-e2e-marker", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-e2e-marker"),
        "output: {:?}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Interactive session: keystrokes reach the hosted shell through amux,
/// its response comes back, the bar is painted, and Ctrl+A q quits.
#[test]
fn interactive_roundtrip_bar_and_quit() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &argv, 24, 80).unwrap();
    // The bar names the pane after the command.
    let bar_needle: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    let out = read_until(&mut p, bar_needle, Duration::from_secs(15));
    assert!(
        contains(&out, bar_needle),
        "no bar in: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"echo amux-rt-42\r\n").unwrap();
    let out = read_until(&mut p, b"amux-rt-42", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-rt-42"),
        "output: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap(); // Ctrl+A q
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// Ctrl+A c opens a second pane (bar shows it) and 1/2 switch between
/// them with the round-trip still working afterwards.
#[test]
fn new_pane_opens_and_switches() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &argv, 24, 80).unwrap();
    let one: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    let out = read_until(&mut p, one, Duration::from_secs(15));
    assert!(contains(&out, one), "no first pane bar");
    p.write(b"\x01c").unwrap();
    let out = read_until(&mut p, two, Duration::from_secs(15));
    assert!(
        contains(&out, two),
        "no second pane in bar after Ctrl+A c: {:?}",
        String::from_utf8_lossy(&out)
    );
    // Switch back to 1 and prove the pane still talks.
    p.write(b"\x011").unwrap();
    p.write(b"echo amux-np-9\r\n").unwrap();
    let out = read_until(&mut p, b"amux-np-9", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-np-9"),
        "pane 1 dead after switching: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

// --- tiling (0.2): splits, focus, zoom, kill-retile -------------------------

/// Spawn amux hosting an interactive shell in a pty, returning it once the bar
/// has appeared (the shell is up). Shared setup for the tiling e2e tests.
fn spawn_amux_shell(rows: u16, cols: u16) -> pty::Pty {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &argv, rows, cols).unwrap();
    let bar: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    read_until(&mut p, bar, Duration::from_secs(15));
    p
}

/// Ctrl+A % splits the focused pane into a second live tile, and *both* shells
/// round-trip: a marker echoed in each appears in the composited output.
#[test]
fn split_creates_a_second_live_tile_and_both_shells_roundtrip() {
    let mut p = spawn_amux_shell(24, 100);
    // Echo a marker in the first pane, then split vertically and echo another
    // marker in the new (focused) pane.
    p.write(b"echo amux-tileA\r\n").unwrap();
    let out = read_until(&mut p, b"amux-tileA", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-tileA"),
        "first pane silent: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01%").unwrap(); // split vertical -> focus the new pane
    p.write(b"echo amux-tileB\r\n").unwrap();
    let out = read_until(&mut p, b"amux-tileB", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-tileB"),
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
    let mut p = spawn_amux_shell(30, 120);
    // Pane 1 (top-left after the splits below) — mark it before splitting so the
    // first shell is proven live.
    p.write(b"echo amux-q1\r\n").unwrap();
    read_until(&mut p, b"amux-q1", Duration::from_secs(15));
    // Build the square: % (vertical) then " (horizontal on the right), then move
    // focus back to the left column with h and " to split it.
    p.write(b"\x01%").unwrap(); // now two columns, focus right
    p.write(b"echo amux-q2\r\n").unwrap();
    read_until(&mut p, b"amux-q2", Duration::from_secs(15));
    p.write(b"\x01\"").unwrap(); // split right column -> focus bottom-right
    p.write(b"echo amux-q3\r\n").unwrap();
    read_until(&mut p, b"amux-q3", Duration::from_secs(15));
    p.write(b"\x01h").unwrap(); // focus back to the left column
    p.write(b"\x01\"").unwrap(); // split it -> focus bottom-left
    p.write(b"echo amux-q4\r\n").unwrap();
    // Collect everything for a few seconds and assert all four markers appeared.
    let out = read_until(&mut p, b"amux-q4", Duration::from_secs(15));
    for m in [
        &b"amux-q1"[..],
        &b"amux-q2"[..],
        &b"amux-q3"[..],
        &b"amux-q4"[..],
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
    let mut p = spawn_amux_shell(24, 100);
    p.write(b"\x01%").unwrap(); // split -> focus the new (right) pane
    p.write(b"echo amux-right-pane\r\n").unwrap();
    read_until(&mut p, b"amux-right-pane", Duration::from_secs(15));
    // Move focus left (h) back to the original pane and run a distinct command.
    p.write(b"\x01h").unwrap();
    p.write(b"echo amux-left-again\r\n").unwrap();
    let out = read_until(&mut p, b"amux-left-again", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-left-again"),
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
    let mut p = spawn_amux_shell(24, 100);
    p.write(b"\x01%").unwrap(); // two tiles
    p.write(b"echo amux-prezoom\r\n").unwrap();
    read_until(&mut p, b"amux-prezoom", Duration::from_secs(15));
    p.write(b"\x01z").unwrap(); // zoom the focused pane full-screen
    p.write(b"echo amux-zoomed\r\n").unwrap();
    let out = read_until(&mut p, b"amux-zoomed", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-zoomed"),
        "zoomed pane silent: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01z").unwrap(); // un-zoom back to tiled
    p.write(b"echo amux-unzoomed\r\n").unwrap();
    let out = read_until(&mut p, b"amux-unzoomed", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-unzoomed"),
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
    let mut p = spawn_amux_shell(24, 100);
    p.write(b"echo amux-keepme\r\n").unwrap();
    read_until(&mut p, b"amux-keepme", Duration::from_secs(15));
    p.write(b"\x01%").unwrap(); // split -> focus new pane
    p.write(b"echo amux-killme\r\n").unwrap();
    read_until(&mut p, b"amux-killme", Duration::from_secs(15));
    p.write(b"\x01x").unwrap(); // kill the focused (new) pane
                                // The survivor takes over full-screen and still talks:
    p.write(b"echo amux-survivor\r\n").unwrap();
    let out = read_until(&mut p, b"amux-survivor", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-survivor"),
        "survivor dead after kill/re-tile: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// A literal Ctrl+A goes through with the doubled prefix.
#[test]
fn double_prefix_reaches_the_child() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &argv, 24, 80).unwrap();
    read_until(&mut p, b"amux", Duration::from_secs(15));
    // Ctrl+A Ctrl+A -> literal 0x01 -> most shells show nothing fatal;
    // then a normal command still round-trips (the stream is not desynced).
    p.write(b"\x01\x01").unwrap();
    p.write(b"echo amux-lit-7\r\n").unwrap();
    let out = read_until(&mut p, b"amux-lit-7", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-lit-7"),
        "output: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}
