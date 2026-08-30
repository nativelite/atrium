//! The status bar: one reverse-video line at the bottom of the real
//! terminal, drawn around the passthrough stream (cursor saved/restored),
//! naming every pane and flagging activity. Pure string building —
//! testable without a terminal.

use crate::identity;
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

/// One visible run of bar text and whether it is an identity tag (which the
/// painter colors). Non-tag segments carry the bar's own (reverse-video) style.
struct Segment {
    text: String,
    /// The identity **name** if this segment is a `·<name>` tag, else `None`.
    /// Drives the per-identity tag color; the text is always present regardless.
    tag: Option<String>,
}

/// Build the bar as an ordered list of visible segments. Splitting the identity
/// tag into its own segment lets the painter color just that run without
/// bleeding into the rest of the reverse-video line. Segment *text* never
/// contains escapes, so column accounting stays exact.
fn bar_segments(panes: &[PaneInfo], note: &str) -> Vec<Segment> {
    let plain = |text: String| Segment { text, tag: None };
    let mut segs = vec![plain(String::from(" amux "))];
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
        segs.push(plain(format!("| {}:{}{} ", i + 1, p.title, mark)));
        // The identity name-tag rides next to the window entry — the name only,
        // never the secret. Its own segment so the painter can color it; the
        // `·<name>` text is the load-bearing channel and is always present.
        if let Some(name) = &p.identity {
            // The trailing space belongs to the entry, not the colored tag, so
            // the color stops at the name. Fix up the entry's trailing space by
            // moving it after the tag.
            if let Some(last) = segs.last_mut() {
                if last.text.ends_with(' ') {
                    last.text.pop();
                }
            }
            segs.push(Segment {
                text: format!("·{name}"),
                tag: Some(name.clone()),
            });
            segs.push(plain(String::from(" ")));
        }
    }
    // Fleet note: when any *non-active* window has a waiting agent, count them
    // so a blocked agent in a backgrounded window surfaces even off-screen.
    let waiting = panes
        .iter()
        .filter(|p| p.waiting && !p.active && !p.exited)
        .count();
    if waiting > 0 {
        segs.push(plain(format!("| {waiting} waiting ")));
    }
    if note.is_empty() {
        segs.push(plain(String::from(
            "| ^A c:win \":% split hjkl:focus z:zoom x:kill q:quit",
        )));
    } else {
        segs.push(plain(format!("| {note}")));
    }
    segs
}

/// The visible bar text (no escapes), truncated/padded to `cols`. A
/// non-empty `note` (e.g. a spawn error) replaces the keys help so
/// failures are visible instead of silent. This is the plain-text authority:
/// [`bar_paint`] colors the identity tags but paints exactly this text.
pub fn bar_text(panes: &[PaneInfo], cols: usize, note: &str) -> String {
    let s: String = bar_segments(panes, note)
        .into_iter()
        .map(|seg| seg.text)
        .collect();
    let mut out: String = s.chars().take(cols).collect();
    while out.chars().count() < cols {
        out.push(' ');
    }
    out
}

/// The full escape sequence that paints the bar on `row` (1-based) without
/// disturbing the pane: save cursor, jump, base (reverse-video) style, the
/// segments — identity tags in their own per-identity color, everything else in
/// the base style — then reset and restore. The visible text is exactly
/// [`bar_text`]'s (same truncation/padding); only color escapes differ, and
/// escapes never count toward the `cols` budget.
pub fn bar_paint(panes: &[PaneInfo], row: u16, cols: usize, note: &str) -> String {
    let base = Style {
        reverse: true,
        fg: Color::Default,
        ..Style::default()
    };
    let base_sgr = base.sgr();
    let mut body = String::new();
    let mut used = 0usize; // visible columns emitted so far
    for seg in bar_segments(panes, note) {
        if used >= cols {
            break;
        }
        // Truncate this segment to the remaining column budget.
        let remaining = cols - used;
        let text: String = seg.text.chars().take(remaining).collect();
        let n = text.chars().count();
        if n == 0 {
            continue;
        }
        match &seg.tag {
            // An identity tag: render it as colored *text*, matching the tiled
            // border tag. The bar's base line is reverse-video, so setting a
            // foreground on a still-reversed cell would swap to a filled color
            // block (a chip). Dropping `reverse` for the tag makes the identity
            // color the *foreground* on the bar's normal background — legible
            // colored text, the same SGR the border uses (`fg`, no reverse).
            // The base reverse-video style is restored right after so the color
            // (and the reverse drop) can't bleed into the following segments.
            Some(name) => {
                let tag_style = Style {
                    fg: Color::Indexed(identity::palette_index(name)),
                    ..Style::default()
                };
                body.push_str(&tag_style.sgr());
                body.push_str(&text);
                body.push_str(&base_sgr);
            }
            None => body.push_str(&text),
        }
        used += n;
    }
    // Pad the rest of the line (base style already active) so the bar fills cols.
    for _ in used..cols {
        body.push(' ');
    }
    format!("\x1b7\x1b[{row};1H{base_sgr}{body}\x1b[0m\x1b8")
}
