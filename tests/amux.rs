//! Tests for amux: the pure prefix scanner and bar builder, then the real
//! thing — amux itself spawned inside a `pty`, driven with keystrokes,
//! its passthrough output read back. Deadline-bounded throughout.

use amux::bar::{bar_paint, bar_text, PaneInfo};
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
    assert_eq!(s.feed(b"\x1b[<0;12;7m"), vec![]); // release
                                                  // Wheel-up is not a click — it scrolls the hovered tile (the scroll feature).
    assert_eq!(
        s.feed(b"\x1b[<64;1;1M"),
        vec![Action::MouseScroll {
            up: true,
            col: 1,
            row: 1
        }]
    );
    assert_eq!(s.feed(b"\x1b[<32;1;1M"), vec![]); // motion/drag
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

// --- bar --------------------------------------------------------------------

fn info(title: &str, active: bool, activity: bool, exited: bool) -> PaneInfo {
    PaneInfo {
        title: title.into(),
        active,
        activity,
        exited,
        waiting: false,
        identity: None,
        role: None,
    }
}

/// A window whose bound agent is waiting on the human.
fn waiting_info(title: &str, active: bool) -> PaneInfo {
    PaneInfo {
        title: title.into(),
        active,
        activity: false,
        exited: false,
        waiting: true,
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
    let mut p = waiting_info("claude", false);
    p.exited = true;
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
    let idx = amux::identity::palette_index("work");
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

// --- command resolution (the npm .cmd shim trap) -----------------------------

// Windows-only: `resolve` mimics cmd.exe's PATHEXT lookup, whose case-insensitive
// extension match (`.EXE` finds `tool.exe`) is a property of the Windows/macOS
// case-insensitive filesystem, not of `resolve` itself. On a case-sensitive
// volume (Linux ext4, or a case-sensitive APFS/macOS volume) this fixture would
// not resolve `tool` against `.EXE` and the test would panic. The resolver is
// only *used* on Windows (`effective_command` calls it under `#[cfg(windows)]`),
// so gate the test there rather than assume a case-insensitive FS off-platform.
#[cfg(windows)]
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

#[test]
fn filter_strips_alt_screen_toggles() {
    // amux owns the alt screen: a pane's alt-buffer enter/leave — the modern
    // ?1049 and the legacy ?1047 / ?47 — must never reach the real terminal.
    use amux::filter::Passthrough;
    let mut f = Passthrough::new();
    assert_eq!(
        f.feed(b"a\x1b[?1049hb\x1b[?1049lc\x1b[?1047hd\x1b[?1047le\x1b[?47hf\x1b[?47lg"),
        b"abcdefg".to_vec()
    );
}

#[test]
fn filter_strips_alt_screen_across_every_split() {
    // Each alt-screen sequence must survive a cut at any byte boundary; the
    // shorter ?47 has to co-exist with the longer ?1047/?1049 in the matcher.
    use amux::filter::Passthrough;
    let input = b"pre\x1b[?1049hmid\x1b[?47lpost\x1b[?1047hend\x1b[?104";
    for cut in 0..=input.len() {
        let mut f = Passthrough::new();
        let mut out = f.feed(&input[..cut]);
        out.extend(f.feed(&input[cut..]));
        // The trailing "\x1b[?104" is an unresolved prefix, held back.
        assert_eq!(out, b"premidpostend".to_vec(), "cut at {cut}");
    }
}

#[test]
fn filter_does_not_eat_a_prefix_that_resolves_to_a_non_strip_sequence() {
    // "\x1b[?104" is a prefix of "\x1b[?1049h" but "\x1b[?104x" is not any
    // strip sequence — it must pass through intact once resolved, even across
    // a split at the ambiguous boundary.
    use amux::filter::Passthrough;
    let input = b"\x1b[?104x";
    for cut in 0..=input.len() {
        let mut f = Passthrough::new();
        let mut out = f.feed(&input[..cut]);
        out.extend(f.feed(&input[cut..]));
        assert_eq!(out, input.to_vec(), "cut at {cut}");
    }
}

// --- end to end: amux inside a pty ------------------------------------------

/// Environment that makes a spawned amux hermetic: point the user-global config
/// location (both spellings) at a directory we control, so the child cannot read
/// the developer's real `~/.config/amux/fleet.json` or `%APPDATA%\\amux\\`.
///
/// Passing `&[]` inherits the parent environment — it does NOT mean "empty env" —
/// so a test that means to prove "no fleet file exists" was really proving "this
/// developer happens to have none". That has already bitten this suite once.
#[allow(dead_code)]
fn hermetic_env(dir: &std::path::Path) -> Vec<(String, String)> {
    let d = dir.to_string_lossy().to_string();
    vec![
        ("XDG_CONFIG_HOME".to_string(), d.clone()),
        ("APPDATA".to_string(), d),
    ]
}

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
                if contains(&out, needle) {
                    break;
                }
            }
            None => {}
        }
    }
    out
}

/// Strip CSI escape sequences (`ESC [ … final`) from a byte stream, leaving the
/// visible glyphs. The painter emits a label like ` 2:sh` interleaved with SGR
/// color codes and, on a diff frame, cursor-position jumps — so the visible label
/// is on screen but its bytes are not contiguous. Matching on the CSI-stripped
/// stream makes the label waits/asserts robust to that (it bit macOS, where a
/// prompt-driven repaint split the label across a diff; the frame itself is
/// correct). Every needle these tests search for is visible text, so stripping
/// the haystack never hides a real match.
fn strip_csi_bytes(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == 0x1b && i + 1 < s.len() && s[i + 1] == b'[' {
            i += 2;
            // Parameters/intermediates (0x20–0x3F) then a final byte (0x40–0x7E).
            while i < s.len() && !(0x40..=0x7e).contains(&s[i]) {
                i += 1;
            }
            if i < s.len() {
                i += 1; // consume the final byte
            }
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let visible = strip_csi_bytes(haystack);
    visible.windows(needle.len().max(1)).any(|w| w == needle)
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

// --- mass-spawn (0.5): -n / --grid open N panes at once ----------------------

/// `amux -n 4 <shell>` opens ONE window of four tiled panes in a 2x2 grid. The
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
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &argv, 40, 160).unwrap();
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
    p.write(b"echo amux-grid-p1\r\n").unwrap();
    let out = read_until(&mut p, b"amux-grid-p1", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-grid-p1"),
        "focused grid pane silent: {:?}",
        String::from_utf8_lossy(&out)
    );
    // Move focus right to another tile and prove IT is live too — a different
    // pane receiving input confirms four independent sessions, not one echoed
    // four times.
    p.write(b"\x01l").unwrap();
    p.write(b"echo amux-grid-p2\r\n").unwrap();
    let out = read_until(&mut p, b"amux-grid-p2", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-grid-p2"),
        "second grid pane silent after focus move: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
}

/// An odd `-n` is a clean startup error, not a silent single pane: amux prints
/// the reason and exits non-zero.
#[test]
fn mass_spawn_rejects_odd_n() {
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &["-n", "3", "cmd"], 24, 80).unwrap();
    // It must exit non-zero (bad argument), and not sit there hosting a shell.
    let code = wait_exit(&mut p, 15);
    assert_ne!(code, 0, "odd -n should be a startup error");
}

// --- fleet (0.6): `amux fleet up <name>` brings up a saved roster ------------

/// `amux fleet up test` reads a temp `amux.fleet.json` with two shell agents and
/// brings up ONE tiled window with two panes. Both pane labels appear in the
/// composited frame and each round-trips a marker — proving two live sessions,
/// laid out by the loader. Hermetic: the agents run the shell, not claude.
#[test]
fn fleet_up_opens_a_two_agent_window() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    // A fleet file in a fresh temp dir; run amux with that dir as cwd so the
    // loader's cwd-first discovery finds it.
    let td = std::env::temp_dir().join(format!("amux-fleet-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    let cmd_json = {
        let mut parts = vec![format!("{shell:?}")];
        parts.extend(args.iter().map(|a| format!("{a:?}")));
        parts.join(", ")
    };
    let fleet_json = format!(
        r#"{{ "fleets": {{ "test": {{ "grid": "1x2", "agents": [
          {{ "name": "one", "cmd": [{cmd_json}] }},
          {{ "name": "two", "cmd": [{cmd_json}] }}
        ] }} }} }}"#
    );
    std::fs::write(td.join("amux.fleet.json"), fleet_json).unwrap();

    // Launch amux via its own binary, spawned with the temp dir as its working
    // directory so `fleet up` discovers the local file. pty::spawn_full sets cwd.
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_amux"),
        &["fleet", "up", "test"],
        30,
        120,
        &hermetic_env(&td),
        Some(&td.to_string_lossy()),
    )
    .unwrap();

    let stem: &[u8] = if cfg!(windows) { b"cmd" } else { b"sh" };
    // Two pane labels appear in the tiled frame: `1:<stem>` and `2:<stem>` (the
    // leading blank is not matched — see the note in the mass-spawn test above).
    let mut label2 = b"2:".to_vec();
    label2.extend_from_slice(stem);
    let out = read_until(&mut p, &label2, Duration::from_secs(20));
    for i in 1..=2u8 {
        let mut label = vec![b'0' + i, b':'];
        label.extend_from_slice(stem);
        assert!(
            contains(&out, &label),
            "fleet pane {i} label missing: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    // The focused (pane 1) shell round-trips.
    p.write(b"echo amux-fleet-p1\r\n").unwrap();
    let out = read_until(&mut p, b"amux-fleet-p1", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-fleet-p1"),
        "fleet pane 1 silent: {:?}",
        String::from_utf8_lossy(&out)
    );
    // Move focus to the second pane and prove it is live too.
    p.write(b"\x01l").unwrap();
    p.write(b"echo amux-fleet-p2\r\n").unwrap();
    let out = read_until(&mut p, b"amux-fleet-p2", Duration::from_secs(15));
    assert!(
        contains(&out, b"amux-fleet-p2"),
        "fleet pane 2 silent after focus move: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    assert_eq!(wait_exit(&mut p, 15), 0);
    let _ = std::fs::remove_dir_all(&td);
}

/// `amux fleet up nope` with no fleet file is a clean error, not a hung window.
#[test]
fn fleet_up_unknown_file_is_a_startup_error() {
    let td = std::env::temp_dir().join(format!("amux-fleet-nofile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_amux"),
        &["fleet", "up", "nope"],
        24,
        80,
        &hermetic_env(&td),
        Some(&td.to_string_lossy()),
    )
    .unwrap();
    assert_ne!(wait_exit(&mut p, 15), 0, "missing fleet file should error");
    let _ = std::fs::remove_dir_all(&td);
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

// --- end to end: the ctl control channel (C1) -------------------------------

/// Spawn `amux --allow-ctl <shell>` in a pty, with `AMUX_CTL_ALLOW` set so the
/// harmless shell counts as a spawnable worker, and return the pty once the bar
/// (pane 1) is up. The hosted shell inherits `AMUX_CTL`/`AMUX_PANE`, so a client
/// typed into it drives the real channel — exactly as an agent-in-a-pane would.
fn spawn_amux_ctl_shell() -> (pty::Pty, &'static str, &'static str) {
    let (shell, flag): (&str, &str) = if cfg!(windows) {
        ("cmd", "/Q")
    } else {
        ("sh", "-i")
    };
    let mut p = pty::Pty::spawn_full(
        env!("CARGO_BIN_EXE_amux"),
        &["--allow-ctl", shell, flag],
        24,
        100,
        &[("AMUX_CTL_ALLOW".to_string(), shell.to_string())],
        None,
    )
    .unwrap();
    let bar: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    read_until(&mut p, bar, Duration::from_secs(15));
    (p, shell, flag)
}

/// `amux ctl list`, run inside a live `--allow-ctl` amux pane, connects over the
/// real channel and returns the org chart as JSON — proving the whole loop:
/// bind → inject env → client connect → non-blocking server drain → reply.
#[test]
fn ctl_list_roundtrips_through_a_live_amux() {
    let (mut p, _shell, _flag) = spawn_amux_ctl_shell();
    let amux = env!("CARGO_BIN_EXE_amux");
    p.write(format!("\"{amux}\" ctl list\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"tree\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"tree\""),
        "no ctl list reply in: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `amux ctl spawn -- <shell>` opens a *visible* new worker window: after the
/// call the bar gains a second pane entry. This is the C1 "an in-pane
/// `ctl spawn` opens a visible worker" done-criterion, end to end.
#[test]
fn ctl_spawn_opens_a_visible_worker_window() {
    let (mut p, shell, _flag) = spawn_amux_ctl_shell();
    let amux = env!("CARGO_BIN_EXE_amux");
    p.write(format!("\"{amux}\" ctl spawn --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    let out = read_until(&mut p, two, Duration::from_secs(20));
    assert!(
        contains(&out, two),
        "no second (worker) pane in the bar after ctl spawn: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// A spawn of a command that is *not* on the allowlist is refused cleanly over
/// the channel — the guard reaches the client as a JSON error, no worker opens.
#[test]
fn ctl_spawn_off_the_allowlist_is_refused() {
    let (mut p, _shell, _flag) = spawn_amux_ctl_shell();
    let amux = env!("CARGO_BIN_EXE_amux");
    // `whoami` is a real binary on both platforms but not an agent/allowlisted.
    p.write(format!("\"{amux}\" ctl spawn -- whoami\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"allowlist", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":false") && contains(&out, b"allowlist"),
        "expected an allowlist refusal, got: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `amux ctl status <id>` returns a target's status as JSON. Pane 0 is always
/// the initial pane (agent ids are process-global from 0), so this is stable.
#[test]
fn ctl_status_reports_a_pane() {
    let (mut p, _shell, _flag) = spawn_amux_ctl_shell();
    let amux = env!("CARGO_BIN_EXE_amux");
    p.write(format!("\"{amux}\" ctl status 0\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"pane\":0", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"pane\":0"),
        "no status reply: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `amux ctl send <role> <text>` delivers the text to the worker as input. We
/// spawn the worker in a *new* window, task it from the caller window, then
/// switch to the worker window to observe: the marker appears there only if the
/// send actually reached the worker's pty (the caller's command echo lives in a
/// different window, so it can't produce a false positive).
#[test]
fn ctl_send_delivers_a_task_to_a_worker() {
    let (mut p, shell, _flag) = spawn_amux_ctl_shell();
    let amux = env!("CARGO_BIN_EXE_amux");
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    p.write(format!("\"{amux}\" ctl spawn --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    read_until(&mut p, two, Duration::from_secs(20)); // worker window opened
    p.write(format!("\"{amux}\" ctl send dev_1 echo WORKERMARK7\r\n").as_bytes())
        .unwrap();
    p.write(b"\x012").unwrap(); // Ctrl+A 2 -> watch the worker window
    let out = read_until(&mut p, b"WORKERMARK7", Duration::from_secs(20));
    assert!(
        contains(&out, b"WORKERMARK7"),
        "worker never received the send: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `amux ctl spawn --here` succeeds over the channel (tiles the worker beside its
/// caller): the reply carries the worker's role, proving the split-placement path
/// ran end to end.
#[test]
fn ctl_spawn_here_succeeds() {
    let (mut p, shell, _flag) = spawn_amux_ctl_shell();
    let amux = env!("CARGO_BIN_EXE_amux");
    p.write(format!("\"{amux}\" ctl spawn --here --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"role\":\"dev_1\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"role\":\"dev_1\""),
        "no --here spawn reply: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

// --- end to end: the ctl control channel (C3) -------------------------------

/// `amux ctl kill <role>` tears down the worker over the live channel: spawn a
/// worker in a new window, then kill it by role — the reply carries the
/// `killed` set (the torn-down subtree), proving `kill` ran end to end and the
/// reap path took the worker's window down.
#[test]
fn ctl_kill_tears_down_a_worker() {
    let (mut p, shell, _flag) = spawn_amux_ctl_shell();
    let amux = env!("CARGO_BIN_EXE_amux");
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    p.write(format!("\"{amux}\" ctl spawn --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    read_until(&mut p, two, Duration::from_secs(20)); // worker window opened
    p.write(format!("\"{amux}\" ctl kill dev_1\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"killed\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"killed\""),
        "no ctl kill reply with a torn-down set: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `amux ctl audit` returns the recorded control-request log. After a spawn the
/// log holds a `spawn` entry; the operator (the initial root pane) sees it —
/// proving requests are recorded and the log is readable live over the channel.
#[test]
fn ctl_audit_records_requests() {
    let (mut p, shell, _flag) = spawn_amux_ctl_shell();
    let amux = env!("CARGO_BIN_EXE_amux");
    let two: &[u8] = if cfg!(windows) { b"2:cmd" } else { b"2:sh" };
    p.write(format!("\"{amux}\" ctl spawn --role dev_1 -- {shell}\r\n").as_bytes())
        .unwrap();
    read_until(&mut p, two, Duration::from_secs(20)); // spawn recorded
    p.write(format!("\"{amux}\" ctl audit\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"\"audit\"", Duration::from_secs(20));
    assert!(
        contains(&out, b"\"ok\":true") && contains(&out, b"\"action\":\"spawn\""),
        "audit log missing the spawn entry: {:?}",
        String::from_utf8_lossy(&out)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

// --- process lifecycle (F13): amux must not orphan its agents ---------------

/// Read a pid the hosted shell prints for itself, so the test can watch that
/// exact process after amux is gone. Unix-only: the whole property is about
/// POSIX signal disposition.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The pid of a process's parent. amux is located this way — as the parent of
/// the shell it hosts — rather than by scanning for its own binary name: the
/// integration tests run in parallel, so several amux processes share this test
/// harness as their parent and a name scan can match somebody else's. (It did:
/// an earlier version of this test SIGHUP'd another test's amux and made that
/// test fail instead of this one.)
#[cfg(unix)]
fn parent_of(pid: u32) -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// **Closing the terminal window must not orphan the agents.** A terminal that
/// goes away sends SIGHUP; amux's teardown (which kills every pane) only runs
/// when the event loop exits normally, so without a handler the default action
/// kills amux outright and every hosted agent is reparented to init and runs on
/// — invisibly, and in a real fleet, billably. Observed live on macOS: ten agent
/// processes still resident 19 minutes after the operator closed the window.
#[cfg(unix)]
#[test]
fn sighup_does_not_orphan_hosted_panes() {
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &["sh", "-i"], 24, 80).unwrap();

    // Have the hosted shell tell us its own pid, so we can watch it directly.
    // The marker is split across two quoted strings so the shell's echo of the
    // typed line ("echo \"$m\"\"=$$\"") cannot contain `amuxpid=` — only the
    // command's actual output does. Without that, read_until matches the echo
    // and returns before the shell has run anything.
    p.write(b"m=amuxpid; echo \"$m\"\"=$$\"\r\n").unwrap();
    let out = read_until(&mut p, b"amuxpid=", Duration::from_secs(15));
    let text = String::from_utf8_lossy(&out);
    let shell_pid: u32 = text
        .split("amuxpid=")
        .nth(1)
        .and_then(|s| {
            let d: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            d.parse().ok()
        })
        .unwrap_or_else(|| panic!("no shell pid in: {text:?}"));
    assert!(pid_alive(shell_pid), "hosted shell never came up");

    let amux = parent_of(shell_pid).expect("could not locate the amux process");
    let _ = std::process::Command::new("kill")
        .args(["-HUP", &amux.to_string()])
        .status();

    // The pane must go with it. Poll rather than sleep a fixed time.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !pid_alive(shell_pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &shell_pid.to_string()])
        .status();
    panic!("pane {shell_pid} survived SIGHUP to amux {amux} — orphaned agent");
}

/// **Closing a full-screen overlay must repaint the pane** (F10).
///
/// `repaint_focused` cleared the screen and then asked the hosted app to redraw
/// by resizing its pty `h-1` then straight back to `h` — ending at the size it
/// already had. An app that only repaints on a real dimension change (or that
/// is mid-turn and defers) correctly does nothing, and the operator is left
/// staring at a blank screen with just the status bar. Reported on macOS after
/// pressing `g` on a board decision; the fix paints from amux's own emulator
/// instead of asking the child.
///
/// `sh` never redraws on SIGWINCH, which makes it the perfect probe: if the
/// prompt comes back, amux painted it.
#[test]
fn closing_an_overlay_repaints_the_pane() {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &argv, 24, 80).unwrap();

    // A marker on screen that only amux can bring back. Written so `repaint=marker`
    // appears in the command's OUTPUT but not in the typed line itself, so the
    // match below can't be satisfied by the input echo: sh uses `$m` indirection;
    // cmd escapes the `=` with a caret (`repaint^=marker` on the line → `repaint=
    // marker` in the output). Both shells are covered — the shell is already
    // branched above, and the marker command must be too (this test shipped
    // bash-only and could never pass on Windows).
    let marker_cmd: &[u8] = if cfg!(windows) {
        b"echo repaint^=marker\r\n"
    } else {
        b"m=repaint; echo \"$m\"\"=marker\"\r\n"
    };
    p.write(marker_cmd).unwrap();
    let out = read_until(&mut p, b"repaint=marker", Duration::from_secs(15));
    assert!(
        contains(&out, b"repaint=marker"),
        "pane never painted: {:?}",
        String::from_utf8_lossy(&out)
    );

    // Open the board overlay, which clears the screen and covers the pane.
    p.write(b"\x01b").unwrap();
    read_until(&mut p, b"board", Duration::from_secs(10));

    // Close it. That is a real view transition, so the pane must come back —
    // painted by amux, because `sh` will not repaint itself.
    p.write(b"\x01b").unwrap();
    let back = read_until(&mut p, b"repaint=marker", Duration::from_secs(10));
    assert!(
        contains(&back, b"repaint=marker"),
        "pane not repainted after the overlay closed — blank screen: {:?}",
        String::from_utf8_lossy(&back)
    );

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// **A pane's whole process tree must die with it** (review finding #2).
///
/// `Pty::kill` is `SIGKILL` to the direct child only, so everything that child
/// spawned survives and is reparented to `init`. For an agent pane that means
/// its MCP servers, language servers and node helpers leak — measured at 23
/// processes holding 4.6 GB during a fleet review. This is not a signal-handling
/// edge case: it happens on a **clean** `Ctrl+A q`, and has since day one.
///
/// `sleep` stands in for whatever an agent spawns. It is found via `pgrep`
/// rather than by parsing pane output, so the test does not depend on rendering.
#[cfg(unix)]
#[test]
fn quitting_kills_the_panes_whole_process_tree() {
    // A NON-interactive shell, deliberately. An interactive `sh -i` runs job
    // control, which puts each background job in its own process group — the one
    // arrangement `killpg` cannot follow. A real agent does not background jobs
    // through a job-control shell; it forks helpers that inherit its group, which
    // is what this reproduces.
    //
    // The grandchild reports its own pid to a file keyed by this test process, so
    // the test never has to guess which amux is its own. Scanning the process
    // table for that is racy under the parallel suite and picks somebody else's.
    let marker = std::env::temp_dir().join(format!("amux-tree-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = format!("sleep 600 & echo $! > {} ; wait", marker.display());
    let mut p =
        pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &["sh", "-c", &script], 24, 80).unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let grandkid: u32 = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            if let Ok(v) = t.trim().parse::<u32>() {
                break v;
            }
        }
        assert!(
            Instant::now() < deadline,
            "grandchild never reported its pid"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(pid_alive(grandkid), "grandchild died before the test began");
    let grandkids = [grandkid];

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    std::thread::sleep(Duration::from_millis(1500));

    let survivors: Vec<u32> = grandkids
        .iter()
        .copied()
        .filter(|g| pid_alive(*g))
        .collect();
    for g in &survivors {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &g.to_string()])
            .status();
    }
    assert!(
        survivors.is_empty(),
        "pane grandchildren survived a clean quit: {survivors:?}"
    );
}

/// **A SIGKILLed amux must not leave the tree behind** (lifecycle layer 4).
///
/// Signal handlers cannot run on `SIGKILL`, a panic, or an OOM kill, so no
/// teardown inside amux can cover them — the whole tree simply leaks. The
/// watchdog is a re-exec of amux that outlives the session: when amux vanishes
/// it kills every process group the crash registry names. This is the only layer
/// that covers a death amux cannot observe.
#[cfg(unix)]
#[test]
fn a_sigkilled_amux_still_takes_its_tree_down() {
    let marker = std::env::temp_dir().join(format!("amux-kill9-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = format!("sleep 600 & echo $! > {} ; wait", marker.display());
    let p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &["sh", "-c", &script], 24, 80).unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let grandkid: u32 = loop {
        if let Ok(t) = std::fs::read_to_string(&marker) {
            if let Ok(v) = t.trim().parse::<u32>() {
                break v;
            }
        }
        assert!(
            Instant::now() < deadline,
            "grandchild never reported its pid"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    // Give the loop a tick to register the pane and start the watchdog.
    std::thread::sleep(Duration::from_millis(600));

    // The one death amux cannot handle.
    let amux = p.pid();
    assert!(amux != 0, "no amux pid");
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &amux.to_string()])
        .status();

    // Watchdog poll (250ms) + SIGTERM + grace (750ms), with room to spare.
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if !pid_alive(grandkid) {
            let _ = std::fs::remove_file(&marker);
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &grandkid.to_string()])
        .status();
    let _ = std::fs::remove_file(&marker);
    panic!("tree survived SIGKILL of amux {amux} — watchdog did not clean up");
}

// --- Windows tree teardown (Job Object, kill-on-close) ----------------------
//
// The Windows counterparts of the two tests above. There is no process group and
// no watchdog on Windows; the guarantee is a Job Object with
// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` that amux holds for its whole life, so the
// kernel terminates every pane and everything a pane spawned the instant amux's
// handle closes — however amux exits.

/// A pane script that spawns a long-lived grandchild (`ping`), records its pid to
/// `marker`, and waits on it — the Windows analog of `sleep 600 & echo $! ; wait`.
#[cfg(windows)]
fn ping_grandchild_argv(marker: &std::path::Path) -> [String; 5] {
    let mpath = marker.display().to_string().replace('\\', "/");
    let script = format!(
        "$p = Start-Process -FilePath ping -ArgumentList @('-n','601','127.0.0.1') \
         -PassThru -WindowStyle Hidden; Set-Content -Path '{mpath}' -Value $p.Id; \
         Wait-Process -Id $p.Id"
    );
    [
        "powershell".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        script,
    ]
}

/// Block until `marker` holds a pid, or panic past `deadline`.
#[cfg(windows)]
fn read_marker_pid(marker: &std::path::Path, within: Duration) -> u32 {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(t) = std::fs::read_to_string(marker) {
            if let Ok(v) = t.trim().parse::<u32>() {
                return v;
            }
        }
        assert!(
            Instant::now() < deadline,
            "grandchild never reported its pid"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// True once `pid` is gone, polled until `within` elapses.
#[cfg(windows)]
fn wait_pid_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !amux::reap::pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// **A clean `Ctrl+A q` kills the pane's whole tree on Windows** (acceptance #2).
/// amux exits, its last handle to the session Job Object closes, and
/// kill-on-close terminates the pane and its grandchild together.
#[cfg(windows)]
#[test]
fn quitting_kills_the_pane_tree_windows() {
    let marker = std::env::temp_dir().join(format!("amux-wtree-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let argv = ping_grandchild_argv(&marker);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &argv_ref, 24, 80).unwrap();

    let grandkid = read_marker_pid(&marker, Duration::from_secs(25));
    assert!(
        amux::reap::pid_alive(grandkid),
        "grandchild died before the test began"
    );

    p.write(b"\x01q").unwrap(); // Ctrl+A q
    let _ = wait_exit(&mut p, 15);

    let gone = wait_pid_gone(grandkid, Duration::from_secs(10));
    if !gone {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &grandkid.to_string()])
            .status();
    }
    let _ = std::fs::remove_file(&marker);
    assert!(gone, "pane grandchild {grandkid} survived a clean quit");
}

/// **A hard-killed amux still takes its tree down on Windows** (acceptance #3).
/// `taskkill /F` is `TerminateProcess` — uncatchable, no teardown runs — but the
/// kernel closes amux's job handle on exit, so kill-on-close fires anyway. This
/// is the death mode no macOS mechanism can match.
#[cfg(windows)]
#[test]
fn hard_killed_amux_still_takes_its_tree_down_windows() {
    let marker = std::env::temp_dir().join(format!("amux-wkill-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let argv = ping_grandchild_argv(&marker);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let p = pty::Pty::spawn(env!("CARGO_BIN_EXE_amux"), &argv_ref, 24, 80).unwrap();

    let grandkid = read_marker_pid(&marker, Duration::from_secs(25));
    // Give the run loop a tick to register + assign the pane to the job.
    std::thread::sleep(Duration::from_millis(800));

    let amux = p.pid();
    assert!(amux != 0, "no amux pid");
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &amux.to_string()])
        .status();

    let gone = wait_pid_gone(grandkid, Duration::from_secs(12));
    if !gone {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &grandkid.to_string()])
            .status();
    }
    let _ = std::fs::remove_file(&marker);
    assert!(
        gone,
        "tree survived taskkill /F of amux {amux} — kill-on-close did not fire"
    );
}

/// **A pane cannot raise its own posture by launching its own amux.**
///
/// The trust ceiling governs `ctl spawn`, but an agent that can run commands can
/// sidestep ctl entirely: `amux --trust skip claude` is just a shell command, and
/// a fresh amux session sets its own policy. That escape made the ceiling half a
/// ceiling.
///
/// The cap hangs off process ANCESTRY rather than the environment, because an
/// agent owns its environment — an `AMUX_AGENT=1` marker or an inherited policy
/// variable dies to `env -u`. It cannot unset its own parent.
///
/// Here the outer session is `plan`; the inner launch asks for `skip` and must be
/// refused down to `plan`, saying so.
#[cfg(unix)]
#[test]
fn a_nested_amux_cannot_raise_its_own_trust() {
    let amux = env!("CARGO_BIN_EXE_amux");
    let mut p = pty::Pty::spawn(
        amux,
        &["--allow-ctl", "--trust", "plan", "sh", "-i"],
        24,
        80,
    )
    .unwrap();
    // Let the outer session record its policy in the registry the cap reads.
    p.write(b"m=outer; echo \"$m\"\"=up\"\r\n").unwrap();
    read_until(&mut p, b"outer=up", Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(600));

    // The escape attempt, exactly as an agent would make it.
    p.write(format!("\"{amux}\" --trust skip sh -i\r\n").as_bytes())
        .unwrap();
    let out = read_until(&mut p, b"capped to", Duration::from_secs(15));
    assert!(
        contains(&out, b"capped to"),
        "a nested amux was allowed to elevate itself: {:?}",
        String::from_utf8_lossy(&out)
    );

    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}
