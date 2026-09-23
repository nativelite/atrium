//! Helpers the e2e areas share: spawning atrium in a pty, reading its
//! output under a deadline, and waiting for it to exit.

use std::time::{Duration, Instant};

/// Environment that makes a spawned atrium hermetic: point the user-global config
/// location (both spellings) at a directory we control, so the child cannot read
/// the developer's real `~/.config/atrium/fleet.json` or `%APPDATA%\\atrium\\`.
///
/// Passing `&[]` inherits the parent environment — it does NOT mean "empty env" —
/// so a test that means to prove "no fleet file exists" was really proving "this
/// developer happens to have none". That has already bitten this suite once.
#[allow(dead_code)]
pub(crate) fn hermetic_env(dir: &std::path::Path) -> Vec<(String, String)> {
    let d = dir.to_string_lossy().to_string();
    vec![
        ("XDG_CONFIG_HOME".to_string(), d.clone()),
        ("APPDATA".to_string(), d),
        // `fleet up` holds for an Enter before handing the screen to the TUI, so
        // the operator actually sees what the roster granted — the banner is
        // otherwise drawn and then wiped by the alt-screen switch on the next
        // line. A test's stdin IS a tty (it runs under a pty), so without this it
        // waits for a keypress nobody sends.
        ("ATRIUM_YES".to_string(), "1".to_string()),
    ]
}

pub(crate) fn read_until(p: &mut pty::Pty, needle: &[u8], deadline: Duration) -> Vec<u8> {
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
pub(crate) fn strip_csi_bytes(s: &[u8]) -> Vec<u8> {
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

pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let visible = strip_csi_bytes(haystack);
    visible.windows(needle.len().max(1)).any(|w| w == needle)
}

pub(crate) fn wait_exit(p: &mut pty::Pty, secs: u64) -> i32 {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut buf = [0u8; 8192];
    loop {
        if let Some(code) = p.try_wait().unwrap() {
            return code;
        }
        assert!(Instant::now() < end, "atrium did not exit in time");
        // Keep draining like a real terminal would: a host that stops
        // reading would block any child mid-write.
        let _ = p.read_timeout(&mut buf, Duration::from_millis(100));
    }
}

/// Spawn atrium hosting an interactive shell in a pty, returning it once the bar
/// has appeared (the shell is up). Shared setup for the tiling e2e tests.
pub(crate) fn spawn_atrium_shell(rows: u16, cols: u16) -> pty::Pty {
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("cmd", vec!["/Q"])
    } else {
        ("sh", vec!["-i"])
    };
    let mut argv = vec![shell];
    argv.extend(args);
    let mut p = pty::Pty::spawn(env!("CARGO_BIN_EXE_atrium"), &argv, rows, cols).unwrap();
    let bar: &[u8] = if cfg!(windows) { b"1:cmd" } else { b"1:sh" };
    read_until(&mut p, bar, Duration::from_secs(15));
    p
}
