//! The status bar: one reverse-video line at the bottom of the real
//! terminal, drawn around the passthrough stream (cursor saved/restored),
//! naming every pane and flagging activity. Pure string building —
//! testable without a terminal.

use ansi::{Color, Style};

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub title: String,
    pub active: bool,
    /// Output arrived while the pane was in the background.
    pub activity: bool,
    pub exited: bool,
}

/// The visible bar text (no escapes), truncated/padded to `cols`. A
/// non-empty `note` (e.g. a spawn error) replaces the keys help so
/// failures are visible instead of silent.
pub fn bar_text(panes: &[PaneInfo], cols: usize, note: &str) -> String {
    let mut s = String::from(" amux ");
    for (i, p) in panes.iter().enumerate() {
        let mark = if p.exited {
            "!"
        } else if p.active {
            "*"
        } else if p.activity {
            "+"
        } else {
            "-"
        };
        s.push_str(&format!("| {}:{}{} ", i + 1, p.title, mark));
    }
    if note.is_empty() {
        s.push_str("| ^A c:new n/p:cycle 1-9:go x:kill q:quit");
    } else {
        s.push_str(&format!("| {note}"));
    }
    let mut out: String = s.chars().take(cols).collect();
    while out.chars().count() < cols {
        out.push(' ');
    }
    out
}

/// The full escape sequence that paints the bar on `row` (1-based) without
/// disturbing the pane: save cursor, jump, style, text, reset, restore.
pub fn bar_paint(panes: &[PaneInfo], row: u16, cols: usize, note: &str) -> String {
    let style = Style {
        reverse: true,
        fg: Color::Default,
        ..Style::default()
    };
    format!(
        "\x1b7\x1b[{row};1H{}{}\x1b[0m\x1b8",
        style.sgr(),
        bar_text(panes, cols, note)
    )
}
