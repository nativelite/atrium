//! `Ctrl+A ?`: ask the focused pane what it is doing, and show the reply.
//!
//! The question is queued at the broker like any `ctl ask` (the pane's mod
//! answers it from a fork of the pane's own context, so the pane's turn is
//! untouched and its transcript never leaves its pane); the run loop copies
//! the reply here when it lands, and the overlay draws it. The overlay is a
//! reading pane: any key closes it, and the reply stays readable with
//! `atrium ctl asked <pane> <id>` afterwards.

use std::time::Instant;

/// One question put to a pane from the keyboard, and its reply once given.
#[derive(Debug, Clone)]
pub(crate) struct AskState {
    /// The pane's global agent id.
    pub(crate) pane: usize,
    /// The pane's label (its role, or `pane N`).
    pub(crate) label: String,
    /// The ask's id at the broker.
    pub(crate) id: u64,
    pub(crate) question: String,
    pub(crate) reply: Option<String>,
    pub(crate) since: Instant,
}

/// `text` wrapped to `width` columns on spaces, long words cut.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut cols = 0usize;
        for word in para.split_whitespace() {
            let w = word.chars().count();
            if cols > 0 && cols + 1 + w > width {
                lines.push(std::mem::take(&mut line));
                cols = 0;
            }
            if w > width {
                for ch in word.chars() {
                    if cols == width {
                        lines.push(std::mem::take(&mut line));
                        cols = 0;
                    }
                    line.push(ch);
                    cols += 1;
                }
                continue;
            }
            if cols > 0 {
                line.push(' ');
                cols += 1;
            }
            line.push_str(word);
            cols += w;
        }
        lines.push(line);
    }
    lines
}

/// Render the ask overlay: the question, then the reply or a waiting line.
/// Absolute CUP per line; the bar row is left for the bar.
pub(crate) fn render_ask_panel(ask: &AskState, rows: u16, cols: u16) -> String {
    let mut out = String::from("\x1b[?25l\x1b[2J");
    out.push_str(&format!(
        "\x1b[1;1H\x1b[1;38;5;37m  asked {}\x1b[0m  \x1b[2m(ask #{} · any key closes · atrium ctl asked {} {} reads it again)\x1b[0m",
        ask.label,
        ask.id,
        ask.pane,
        ask.id
    ));
    out.push_str(&format!(
        "\x1b[2;1H\x1b[38;5;238m{}\x1b[0m",
        "\u{2500}".repeat(cols as usize)
    ));
    let width = (cols as usize).saturating_sub(4);
    let last = rows.saturating_sub(1);
    let mut row = 3u16;
    let mut put = |out: &mut String, line: &str, sgr: &str| {
        if row < last {
            out.push_str(&format!("\x1b[{row};1H  {sgr}{line}\x1b[0m"));
            row += 1;
        }
    };
    for line in wrap(&ask.question, width) {
        put(&mut out, &line, "\x1b[2m");
    }
    put(&mut out, "", "");
    match &ask.reply {
        Some(reply) => {
            for line in wrap(reply, width) {
                put(&mut out, &line, "");
            }
        }
        None => {
            let secs = ask.since.elapsed().as_secs();
            put(
                &mut out,
                &format!(
                    "waiting for {} to answer\u{2026} {secs}s (its turn is not interrupted)",
                    ask.label
                ),
                "\x1b[2m",
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::strip_csi;

    #[test]
    fn wrap_breaks_on_spaces_keeps_paragraphs_and_cuts_long_words() {
        assert_eq!(wrap("a bb ccc dddd", 8), vec!["a bb ccc", "dddd"]);
        assert_eq!(
            wrap("a bb ccc dddd", 2),
            vec!["a bb ccc", "dddd"],
            "never narrower than 8"
        );
        assert_eq!(wrap("one\ntwo", 10), vec!["one", "two"]);
        assert_eq!(wrap("abcdefghijklmnop", 8), vec!["abcdefgh", "ijklmnop"]);
        assert_eq!(wrap("", 8), vec![""]);
    }

    #[test]
    fn the_panel_shows_the_question_then_the_wait_then_the_reply() {
        let mut ask = AskState {
            pane: 3,
            label: "dev_1".to_string(),
            id: 7,
            question: "What are you doing?".to_string(),
            reply: None,
            since: Instant::now(),
        };
        let r = strip_csi(&render_ask_panel(&ask, 24, 80));
        assert!(r.contains("asked dev_1"), "{r}");
        assert!(r.contains("ask #7"), "{r}");
        assert!(r.contains("atrium ctl asked 3 7"), "{r}");
        assert!(r.contains("What are you doing?"), "{r}");
        assert!(r.contains("waiting for dev_1 to answer"), "{r}");
        ask.reply = Some("Running the filter tests; nothing is blocking me.".to_string());
        let r = strip_csi(&render_ask_panel(&ask, 24, 80));
        assert!(
            r.contains("Running the filter tests; nothing is blocking me."),
            "{r}"
        );
        assert!(!r.contains("waiting for"), "{r}");
    }
}
