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
    /// A bound agent in this window is waiting on the human (`WaitingApproval`).
    /// Rung 3 of the attention ladder (§4.2): drives the `?` marker and the
    /// fleet `| N waiting` note. Status only — never any transcript text.
    pub waiting: bool,
    /// The credential identity **name** the window's agent runs under, if any —
    /// the name the user typed (`work`, `wif:prod`), never the secret. Shown as
    /// a `·<name>` tag next to the window entry (name-only, a11y text always
    /// present; see `identity::IDENTITY_PALETTE` for the tunable color).
    pub identity: Option<String>,
}

/// The visible bar text (no escapes), truncated/padded to `cols`. A
/// non-empty `note` (e.g. a spawn error) replaces the keys help so
/// failures are visible instead of silent.
pub fn bar_text(panes: &[PaneInfo], cols: usize, note: &str) -> String {
    let mut s = String::from(" amux ");
    for (i, p) in panes.iter().enumerate() {
        // Marker priority (§4.2): a dead child, then a waiting agent (it needs
        // you), then active, then background activity, then idle. `?` slots in
        // between `!` and `*` so an unfocused window whose agent is blocked
        // outranks mere activity but never masks an exit.
        let mark = if p.exited {
            "!"
        } else if p.waiting {
            "?"
        } else if p.active {
            "*"
        } else if p.activity {
            "+"
        } else {
            "-"
        };
        // The identity name-tag rides next to the window entry — the name only,
        // never the secret. Text is always present so the signal survives
        // without color (the tag's per-identity palette color is applied when
        // the bar paints, but the `·<name>` text is the load-bearing channel).
        let tag = match &p.identity {
            Some(name) => format!("·{name}"),
            None => String::new(),
        };
        s.push_str(&format!("| {}:{}{}{} ", i + 1, p.title, mark, tag));
    }
    // Fleet note: when any *non-active* window has a waiting agent, count them
    // so a blocked agent in a backgrounded window surfaces even off-screen.
    let waiting = panes
        .iter()
        .filter(|p| p.waiting && !p.active && !p.exited)
        .count();
    if waiting > 0 {
        s.push_str(&format!("| {waiting} waiting "));
    }
    if note.is_empty() {
        s.push_str("| ^A c:win \":% split hjkl:focus z:zoom x:kill q:quit");
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
