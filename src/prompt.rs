//! The command prompt (`Ctrl+A :`): a command line typed into the bar row and
//! opened in a new window on Enter. While it is open, keystrokes edit the line
//! instead of reaching the panes.
//!
//! Carved out of `run()` (r10 audit B11 residue, step 2) together with the line
//! editor and argv splitter it already used.

use crate::*;

/// Feed a chunk of raw input to the open prompt (`line` is `Some` while open).
/// Enter opens the typed command as a new window under `launch`'s identity and
/// session job; Esc / Ctrl+C closes the prompt. A launch failure is flashed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn feed_prompt(
    line: &mut Option<String>,
    bytes: &[u8],
    windows: &mut Vec<Window>,
    active: &mut usize,
    launch: &Launch,
    rows: u16,
    cols: u16,
    out: &mut impl std::io::Write,
    flash: &mut Option<(String, Instant)>,
) -> KeyOutcome {
    let mut outcome = KeyOutcome::default();
    let Some(buf) = line.as_mut() else {
        return outcome;
    };
    match edit_prompt(buf, bytes) {
        PromptEdit::Continue => {}
        PromptEdit::Cancel => *line = None,
        PromptEdit::Submit => {
            let argv = split_cmdline(line.take().unwrap_or_default().trim());
            if !argv.is_empty() {
                match open_window(
                    windows,
                    active,
                    &argv,
                    launch.identity,
                    trust_mode(),
                    rows,
                    cols,
                    out,
                    flash,
                    launch.job,
                ) {
                    Ok(()) => outcome.reset_frame = true,
                    Err(e) => {
                        *flash = Some((format!("cannot start {:?}: {e}", argv[0]), Instant::now()));
                    }
                }
            }
        }
    }
    // Repaint only when the user actually pressed something (the line changed,
    // or it opened/closed) — NOT every idle tick, which made the prompt row
    // flash rapidly.
    outcome.repaint = !bytes.is_empty();
    outcome
}

/// The outcome of feeding a keystroke chunk to the open command prompt.
pub(crate) enum PromptEdit {
    /// The line changed (or nothing happened); keep the prompt open.
    Continue,
    /// Esc / Ctrl+C: close the prompt without running anything.
    Cancel,
    /// Enter: the caller runs the accumulated line.
    Submit,
}

/// Apply a chunk of raw input `bytes` to the prompt line `buf`: printable ASCII is
/// appended, Backspace deletes, Enter submits, Esc/Ctrl+C cancels. Other control
/// bytes (including the rest of an arrow-key escape) are ignored. Returns as soon
/// as a terminating key (Enter/Esc) is seen so trailing bytes don't leak.
pub(crate) fn edit_prompt(buf: &mut String, bytes: &[u8]) -> PromptEdit {
    for &b in bytes {
        match b {
            b'\r' | b'\n' => return PromptEdit::Submit,
            0x1b | 0x03 => return PromptEdit::Cancel, // Esc or Ctrl+C
            0x7f | 0x08 => {
                buf.pop();
            }
            0x20..=0x7e => buf.push(b as char),
            _ => {} // ignore other control bytes
        }
    }
    PromptEdit::Continue
}

/// Split a command line into argv, honoring double quotes so a path with spaces
/// stays one argument (`"C:\Program Files\Git\bin\bash.exe" --login`). Whitespace
/// separates unquoted words; quotes are removed. Minimal by design — enough to
/// launch a shell with a flag, not a full shell parser.
pub(crate) fn split_cmdline(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has = false;
    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_cmdline_splits_on_whitespace() {
        assert_eq!(split_cmdline("wsl -d Ubuntu"), vec!["wsl", "-d", "Ubuntu"]);
        assert_eq!(split_cmdline("  pwsh   -NoLogo "), vec!["pwsh", "-NoLogo"]);
        assert!(split_cmdline("   ").is_empty());
    }

    #[test]
    fn split_cmdline_keeps_quoted_paths_whole() {
        assert_eq!(
            split_cmdline(r#""C:\Program Files\Git\bin\bash.exe" --login"#),
            vec![r"C:\Program Files\Git\bin\bash.exe", "--login"]
        );
    }

    #[test]
    fn edit_prompt_appends_backspaces_submits_and_cancels() {
        let mut b = String::new();
        assert!(matches!(edit_prompt(&mut b, b"wsl"), PromptEdit::Continue));
        assert_eq!(b, "wsl");
        // backspace deletes the last char
        assert!(matches!(edit_prompt(&mut b, &[0x7f]), PromptEdit::Continue));
        assert_eq!(b, "ws");
        // Enter submits, keeping what was typed so far
        assert!(matches!(edit_prompt(&mut b, b"l\r"), PromptEdit::Submit));
        assert_eq!(b, "wsl");
        // Esc cancels
        let mut c = String::from("pwsh");
        assert!(matches!(edit_prompt(&mut c, &[0x1b]), PromptEdit::Cancel));
    }
}
