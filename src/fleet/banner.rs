//! Making file-supplied text safe to show on a terminal, and the preflight block.

use std::path::Path;

/// Longest rendering of one file-supplied string in a banner line. A 40 KB
/// `add_dirs` entry would otherwise scroll the rest of the disclosure — the part
/// naming the other grants — off the screen the operator is about to approve.
pub const BANNER_MAX: usize = 120;

/// Most loud (non-quiet) grant lines printed before the rest is summarised. The
/// banner competes for a 24-row terminal with the trust posture and can-spawn
/// lines, and those scrolling off is the failure 31774f0 exists to prevent.
pub const MAX_LOUD_LINES: usize = 6;

/// Render a fleet-file string inert for a terminal.
///
/// The `json` parser decodes `\u001b`, so any string in the file — an agent
/// name, a path, an identity — can carry a real ESC. Printed raw into the
/// approval banner it can clear the screen, repaint a forged "0 dirs outside"
/// line, or reorder a path with a bidi override so `/etc/shadow` reads as
/// something harmless. The banner is the control; a string that can rewrite it
/// defeats the control.
///
/// Escaped: every `Cc` (C0 controls, DEL, C1 — `char::is_control` covers all
/// three), the bidi/invisible formatting characters that reorder or hide text
/// without being controls, and `"` so a name cannot close its own quoted field.
///
/// Deliberately NOT escaped: `\`. Doubling it mangles every Windows path in the
/// banner (`C:\Users\dev\repo` → `C:\\Users\\dev\\repo`), on the platform with
/// the longest paths, to buy an injectivity this rendering never claims: a file
/// name containing the literal text `\u{1b}` renders identically to a real ESC.
/// This defangs; it does not round-trip.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let hostile = c.is_control()
            || matches!(
                c,
                '\u{061c}' | '\u{180e}' | '\u{feff}' | '\u{2028}' | '\u{2029}'
            )
            || ('\u{200b}'..='\u{200f}').contains(&c)
            || ('\u{202a}'..='\u{202e}').contains(&c)
            || ('\u{2060}'..='\u{2064}').contains(&c)
            || ('\u{2066}'..='\u{206f}').contains(&c);
        if hostile {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else if c == '"' {
            out.push_str("\\\"");
        } else {
            out.push(c);
        }
    }
    out
}

/// Cap a rendered string at `max` characters, eliding the **middle**.
///
/// Cutting the tail would eat the destination, which is the one fact a
/// disclosure line exists to carry: `…/Library/CloudStorage/OneDri…(truncated)`
/// tells the operator a grant leaves the tree and then withholds where it goes.
/// The `…` marks the cut so a shortened path cannot be mistaken for a complete
/// one.
pub fn shorten(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max || max < 8 {
        return s.to_string();
    }
    let keep = max - 1;
    let head = keep / 3;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

/// A path as it appears in the banner: defanged, then middle-elided.
pub fn show_path(p: &Path) -> String {
    shorten(&sanitize(&p.display().to_string()), BANNER_MAX)
}

/// A file-supplied string as it appears in the banner, quoted.
pub(super) fn show_str(s: &str) -> String {
    format!("\"{}\"", shorten(&sanitize(s), BANNER_MAX))
}

/// Width of the preflight block's top and bottom rules.
const PREFLIGHT_RULE: usize = 64;

/// The banner's preflight warnings as one block that is hard to skim past.
///
/// A preflight warning never stops a launch — the operator decides at the
/// Enter prompt — so it has to be loud instead: a bold yellow header band, a
/// yellow bar down every warning, and a closing rule. `color` is false when
/// stderr is not a terminal (or `NO_COLOR` is set); the block is then plain
/// `atrium fleet: warning: …` lines, so a log stays greppable and escape-free.
/// No lines at all when there is nothing to warn about.
///
/// Callers pass text that is already defanged: every file-supplied string in a
/// warning goes through [`sanitize`] first.
pub fn preflight_block(warnings: &[String], color: bool) -> Vec<String> {
    if warnings.is_empty() {
        return Vec::new();
    }
    if !color {
        return warnings
            .iter()
            .map(|w| format!("atrium fleet: warning: {w}"))
            .collect();
    }
    const BAND: &str = "\x1b[1;30;43m";
    const YELLOW: &str = "\x1b[1;33m";
    const RESET: &str = "\x1b[0m";
    let count = match warnings.len() {
        1 => "1 warning".to_string(),
        n => format!("{n} warnings"),
    };
    let title = format!(" ▲ PREFLIGHT · {count} · launching anyway ");
    let fill = PREFLIGHT_RULE.saturating_sub(title.chars().count() + 3);
    let mut lines = vec![
        String::new(),
        format!(
            "{YELLOW}┏━━{RESET}{BAND}{title}{RESET}{YELLOW}{}{RESET}",
            "━".repeat(fill)
        ),
    ];
    for w in warnings {
        lines.push(format!("{YELLOW}┃  ▲ {w}{RESET}"));
    }
    lines.push(format!(
        "{YELLOW}┗{}{RESET}",
        "━".repeat(PREFLIGHT_RULE - 1)
    ));
    lines
}

#[cfg(test)]
mod tests;
