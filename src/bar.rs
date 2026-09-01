//! The status bar: one themed line at the bottom of the real terminal, drawn
//! around the passthrough stream (cursor saved/restored), naming every pane and
//! flagging activity. A dark slate with a cyan `amux` signature chip and entries
//! colored by pane state in the shared [`crate::theme`] language (the same colors
//! the tile borders use). Pure string building — testable without a terminal.

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

/// The color role of a bar segment. Text is identical to [`bar_text`]; only the
/// SGR differs, and escapes never count toward `cols`.
#[derive(Clone)]
enum Role {
    /// The `amux` wordmark — the cyan signature chip.
    Brand,
    /// A window entry, colored by the pane's state.
    Entry(State),
    /// The `| N waiting` fleet note (amber) or a flash note.
    Note(bool),
    /// The keys hint (dim).
    Keys,
    /// An identity `·name` tag in its per-identity color.
    Identity(String),
}

/// A window entry's state → its color, sharing the tile-border language.
#[derive(Clone, Copy)]
enum State {
    Active,
    Waiting,
    Exited,
    Activity,
    Idle,
}

/// One visible run of bar text plus its color role. Splitting into roled
/// segments lets the painter color each without bleeding. Segment *text* never
/// contains escapes, so column accounting stays exact.
struct Segment {
    text: String,
    role: Role,
}

/// Build the bar as an ordered list of roled segments.
fn bar_segments(panes: &[PaneInfo], note: &str) -> Vec<Segment> {
    let seg = |text: String, role: Role| Segment { text, role };
    let mut segs = vec![seg(String::from(" amux "), Role::Brand)];
    for (i, p) in panes.iter().enumerate() {
        // Marker priority (§4.2): a dead child, then a waiting agent (it needs
        // you), then active, then background activity, then idle. `?` slots in
        // between `!` and `*` so an unfocused window whose agent is blocked
        // outranks mere activity but never masks an exit. The entry's *color*
        // tracks the same state, so the bar reads at a glance.
        let (mark, state) = if p.exited {
            ("!", State::Exited)
        } else if p.waiting {
            ("?", State::Waiting)
        } else if p.active {
            ("*", State::Active)
        } else if p.activity {
            ("+", State::Activity)
        } else {
            ("-", State::Idle)
        };
        segs.push(seg(format!("| {}:{}{} ", i + 1, p.title, mark), Role::Entry(state)));
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
            segs.push(seg(format!("·{name}"), Role::Identity(name.clone())));
            segs.push(seg(String::from(" "), Role::Entry(state)));
        }
    }
    // Fleet note: when any *non-active* window has a waiting agent, count them
    // so a blocked agent in a backgrounded window surfaces even off-screen.
    let waiting = panes
        .iter()
        .filter(|p| p.waiting && !p.active && !p.exited)
        .count();
    if waiting > 0 {
        segs.push(seg(format!("| {waiting} waiting "), Role::Note(true)));
    }
    if note.is_empty() {
        segs.push(seg(
            String::from("| ^A c:win \":% split hjkl:focus z:zoom m:mouse x:kill q:quit"),
            Role::Keys,
        ));
    } else {
        segs.push(seg(format!("| {note}"), Role::Note(false)));
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

/// The [`Style`] for a segment's color role — the themed statusline palette.
/// Every style sets the bar background explicitly (so it fills the whole line
/// with no reverse-video gaps), with a per-role foreground; the `amux` brand is a
/// filled cyan chip, and identity tags keep their per-identity color.
fn role_style(role: &Role) -> Style {
    use crate::theme;
    let on_bar = |fg: Color, bold: bool| Style {
        fg,
        bg: theme::BAR_BG,
        bold,
        ..Style::default()
    };
    match role {
        Role::Brand => Style {
            fg: theme::BRAND_FG,
            bg: theme::BRAND_BG,
            bold: true,
            ..Style::default()
        },
        Role::Entry(State::Active) => on_bar(theme::FOCUSED, true),
        Role::Entry(State::Waiting) => on_bar(theme::WAITING, true),
        Role::Entry(State::Exited) => on_bar(theme::EXITED, false),
        Role::Entry(State::Activity) => on_bar(theme::ACTIVITY, false),
        Role::Entry(State::Idle) => on_bar(theme::IDLE, false),
        Role::Note(true) => on_bar(theme::WAITING, true),
        Role::Note(false) => on_bar(theme::EXITED, false),
        Role::Keys => on_bar(theme::KEYS, false),
        Role::Identity(name) => on_bar(Color::Indexed(identity::palette_index(name)), false),
    }
}

/// The full escape sequence that paints the themed bar on `row` (1-based) without
/// disturbing the pane: save cursor, jump, then each segment in its role color
/// over the bar's dark background, padded to `cols`, then reset and restore. The
/// visible text is exactly [`bar_text`]'s (same truncation/padding); only color
/// escapes differ, and escapes never count toward the `cols` budget.
pub fn bar_paint(panes: &[PaneInfo], row: u16, cols: usize, note: &str) -> String {
    let base = Style {
        fg: crate::theme::BAR_FG,
        bg: crate::theme::BAR_BG,
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
        // Absolute SGR per segment (each is a full reset+set), so a color can
        // never bleed into the next; the base style is re-applied for padding.
        body.push_str(&role_style(&seg.role).sgr());
        body.push_str(&text);
        used += n;
    }
    // Pad the rest of the line in the base style so the bar fills cols evenly.
    body.push_str(&base_sgr);
    for _ in used..cols {
        body.push(' ');
    }
    format!("\x1b7\x1b[{row};1H{base_sgr}{body}\x1b[0m\x1b8")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip ANSI escapes (`ESC7`/`ESC8` and CSI `ESC[…X`) to recover the visible
    /// glyphs — the a11y/column-accounting authority the painter must preserve.
    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            match it.peek() {
                Some('[') => {
                    it.next();
                    for c2 in it.by_ref() {
                        if c2.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(_) => {
                    it.next();
                }
                None => {}
            }
        }
        out
    }

    fn pane(title: &str, active: bool, activity: bool) -> PaneInfo {
        PaneInfo {
            title: title.into(),
            active,
            activity,
            exited: false,
            waiting: false,
            identity: None,
        }
    }

    #[test]
    fn painted_visible_text_equals_bar_text() {
        // The theme adds only color — the visible glyphs (and thus every column)
        // must be byte-for-byte what `bar_text` promises.
        let panes = vec![pane("claude", true, false), pane("cmd", false, true)];
        for cols in [20usize, 40, 80, 200] {
            let painted = bar_paint(&panes, 24, cols, "");
            assert_eq!(strip(&painted), bar_text(&panes, cols, ""), "cols={cols}");
        }
    }

    #[test]
    fn every_state_color_is_distinct() {
        // The whole point of the follow-up: states must be told apart at a glance.
        let sgr = |st| role_style(&Role::Entry(st)).sgr();
        let all = [
            sgr(State::Active),
            sgr(State::Waiting),
            sgr(State::Exited),
            sgr(State::Activity),
            sgr(State::Idle),
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "states {i} and {j} share a color");
            }
        }
    }

    #[test]
    fn brand_is_a_filled_bold_chip() {
        let s = role_style(&Role::Brand);
        assert_eq!(s.bg, crate::theme::BRAND_BG);
        assert_eq!(s.fg, crate::theme::BRAND_FG);
        assert!(s.bold);
    }
}
