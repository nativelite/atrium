//! Tests for amux: the pure prefix scanner and bar builder, then the real
//! thing — amux itself spawned inside a `pty`, driven with keystrokes,
//! its passthrough output read back. Deadline-bounded throughout.

use amux::bar::{bar_text, PaneInfo};
use amux::input::{Action, PrefixScanner};
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
    let mut s = PrefixScanner::new();
    assert_eq!(s.feed(b"a\x01zb"), vec![Action::Forward(b"ab".to_vec())]);
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
    assert_eq!(bar_text(&panes, 40).chars().count(), 40);
    assert_eq!(bar_text(&[], 10).chars().count(), 10);
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
