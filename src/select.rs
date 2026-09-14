//! Copy-selection inside one tile (mouse mode, tiled layout).
//!
//! In a tiled window the host terminal sees one full-screen grid, so its native
//! drag-selection runs across every neighbouring tile and the borders. With mouse
//! mode on (`Ctrl+A m`) the host stops selecting and atrium used to drop drags,
//! so nothing could be selected at all. This is atrium's own selection, the tmux
//! shape: press inside a tile, drag, release — the highlight is clamped to that
//! tile's content area however far the pointer strays, and the release copies the
//! text to the clipboard.
//!
//! Coordinates are **content** cells of the tile (0-based `(row, col)` inside the
//! one-cell border), so the selection is independent of where the tile sits.
//! Pure over [`ansi::Screen`] and [`Rect`]; the clipboard write is the only
//! side effect, isolated in [`copy`].

use crate::layout::Rect;
use ansi::{CellWidth, Screen};

/// A selection in progress: the tile it belongs to and its two ends in that
/// tile's content coordinates. `anchor` is where the press landed; `head`
/// follows the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub pane_id: usize,
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    /// Start a selection at a press on master cell `(mx, my)` inside `rect`.
    pub fn start(pane_id: usize, mx: usize, my: usize, rect: &Rect) -> Selection {
        let at = content_cell(mx, my, rect);
        Selection {
            pane_id,
            anchor: at,
            head: at,
        }
    }

    /// Move the head to master cell `(mx, my)`, clamped into `rect`'s content.
    pub fn drag(&mut self, mx: usize, my: usize, rect: &Rect) {
        self.head = content_cell(mx, my, rect);
    }

    /// A press and release on the same cell is a click, not a selection.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// `(start, end)` in reading order (row-major), inclusive.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn contains(&self, row: usize, col: usize) -> bool {
        let ((r0, c0), (r1, c1)) = self.ordered();
        (row > r0 || (row == r0 && col >= c0)) && (row < r1 || (row == r1 && col <= c1))
    }
}

/// Master cell `(mx, my)` → `rect`'s content cell, clamped into the content area
/// (the rect inset one cell for the border).
fn content_cell(mx: usize, my: usize, rect: &Rect) -> (usize, usize) {
    let inner_rows = rect.rows.saturating_sub(2).max(1);
    let inner_cols = rect.cols.saturating_sub(2).max(1);
    let row = my.saturating_sub(rect.row + 1).min(inner_rows - 1);
    let col = mx.saturating_sub(rect.col + 1).min(inner_cols - 1);
    (row, col)
}

/// The selected text of the tile's `screen`: rows joined with `\n`, trailing
/// blanks trimmed from each row (a terminal pads every row to its width). The
/// right half of a double-width glyph is skipped so the glyph appears once.
pub fn text(sel: &Selection, screen: &Screen) -> String {
    let ((r0, c0), (r1, c1)) = sel.ordered();
    let (rows, cols) = (screen.rows(), screen.cols());
    let mut lines = Vec::new();
    for r in r0..=r1.min(rows.saturating_sub(1)) {
        let from = if r == r0 { c0 } else { 0 };
        let to = if r == r1 { c1 } else { cols.saturating_sub(1) };
        let mut line = String::new();
        for c in from..=to.min(cols.saturating_sub(1)) {
            let cell = screen.cell(r, c);
            if cell.width != CellWidth::Continuation {
                line.push(cell.ch);
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

/// Reverse-video the selected cells of the tile at `rect` in the composed
/// `master` frame, so the selection is visible while dragging.
pub fn highlight(sel: &Selection, master: &mut Screen, rect: &Rect) {
    let inner_rows = rect.rows.saturating_sub(2);
    let inner_cols = rect.cols.saturating_sub(2);
    for r in 0..inner_rows {
        for c in 0..inner_cols {
            let (mr, mc) = (rect.row + 1 + r, rect.col + 1 + c);
            if sel.contains(r, c) && mr < master.rows() && mc < master.cols() {
                let mut cell = master.cell(mr, mc);
                cell.style.reverse = !cell.style.reverse;
                master.set(mr, mc, cell);
            }
        }
    }
}

/// The OSC 52 sequence that asks the host terminal to put `text` on the system
/// clipboard (`ESC ] 52 ; c ; <base64> BEL`). Understood by Windows Terminal,
/// iTerm2, kitty, WezTerm, foot, and tmux/xterm with it allowed.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Standard (RFC 4648) base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Put `text` on the clipboard. Writes OSC 52 to `out` (the host terminal) and,
/// on Windows, also sets the system clipboard directly — OSC 52 needs the host
/// terminal's support and, under ConPTY, its passthrough; the Win32 clipboard
/// needs neither. Returns whether the native write succeeded (always `false`
/// off Windows, where OSC 52 is the only route).
pub fn copy(text: &str, out: &mut impl std::io::Write) -> bool {
    let _ = out.write_all(osc52(text).as_bytes());
    let _ = out.flush();
    native::set_clipboard(text)
}

#[cfg(windows)]
mod native {
    use std::ffi::c_void;

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
        fn CloseClipboard() -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    }

    pub fn set_clipboard(text: &str) -> bool {
        // CF_UNICODETEXT is NUL-terminated UTF-16 with CRLF line breaks.
        let wide: Vec<u16> = text
            .replace('\n', "\r\n")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let bytes = wide.len() * 2;
        // SAFETY: plain Win32 clipboard protocol. The global block is sized for
        // `wide` and written only between Lock/Unlock; ownership passes to the
        // clipboard on a successful SetClipboardData, otherwise we free it; the
        // clipboard is closed on every path after a successful open.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return false;
            }
            let ok = (|| {
                if EmptyClipboard() == 0 {
                    return false;
                }
                let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
                if mem.is_null() {
                    return false;
                }
                let dst = GlobalLock(mem) as *mut u16;
                if dst.is_null() {
                    GlobalFree(mem);
                    return false;
                }
                std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
                GlobalUnlock(mem);
                if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                    GlobalFree(mem);
                    return false;
                }
                true
            })();
            CloseClipboard();
            ok
        }
    }
}

#[cfg(not(windows))]
mod native {
    pub fn set_clipboard(_text: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ansi::{Cell, Style};

    /// A 7x12 tile at (2, 3): content is 5x10 at master (3, 4).
    const RECT: Rect = Rect {
        row: 2,
        col: 3,
        rows: 7,
        cols: 12,
    };

    fn screen(lines: &[&str]) -> Screen {
        let mut s = Screen::new(5, 10);
        for (r, line) in lines.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                s.set(r, c, Cell::new(ch, Style::default()));
            }
        }
        s
    }

    #[test]
    fn press_maps_through_the_border_into_content_cells() {
        let s = Selection::start(7, 4, 3, &RECT);
        assert_eq!(s.anchor, (0, 0));
        let s = Selection::start(7, 8, 5, &RECT);
        assert_eq!(s.anchor, (2, 4));
    }

    #[test]
    fn dragging_outside_the_tile_clamps_to_its_content() {
        let mut s = Selection::start(7, 5, 4, &RECT);
        s.drag(200, 200, &RECT); // far into a neighbouring tile
        assert_eq!(s.head, (4, 9));
        s.drag(0, 0, &RECT);
        assert_eq!(s.head, (0, 0));
    }

    #[test]
    fn a_click_is_an_empty_selection() {
        let mut s = Selection::start(1, 6, 4, &RECT);
        assert!(s.is_empty());
        s.drag(7, 4, &RECT);
        assert!(!s.is_empty());
    }

    #[test]
    fn single_row_text_is_the_inclusive_span() {
        let scr = screen(&["hello world"]);
        let mut s = Selection::start(1, 4, 3, &RECT); // (0,0)
        s.drag(8, 3, &RECT); // (0,4)
        assert_eq!(text(&s, &scr), "hello");
    }

    #[test]
    fn multi_row_text_is_reading_order_and_trims_padding() {
        let scr = screen(&["ab  ", "cdef", "gh"]);
        // Drag backwards (head before anchor): order must still be row-major.
        let mut s = Selection::start(1, 5, 5, &RECT); // (2,1)
        s.drag(5, 3, &RECT); // (0,1)
        assert_eq!(text(&s, &scr), "b\ncdef\ngh");
    }

    #[test]
    fn a_wide_glyph_is_copied_once() {
        let mut scr = Screen::new(5, 10);
        scr.set(0, 0, Cell::wide('日', Style::default()));
        scr.set(0, 1, Cell::continuation(Style::default()));
        scr.set(0, 2, Cell::new('x', Style::default()));
        let mut s = Selection::start(1, 4, 3, &RECT);
        s.drag(6, 3, &RECT);
        assert_eq!(text(&s, &scr), "日x");
    }

    #[test]
    fn highlight_reverses_only_selected_content_cells() {
        let mut master = Screen::new(12, 20);
        let mut s = Selection::start(1, 5, 3, &RECT); // (0,1)
        s.drag(4, 4, &RECT); // (1,0)
        highlight(&s, &mut master, &RECT);
        let rev = |r, c| master.cell(r, c).style.reverse;
        assert!(!rev(3, 4)); // (0,0) before the anchor
        assert!(rev(3, 5)); // (0,1) anchor
        assert!(rev(3, 13)); // (0,9) end of first row
        assert!(rev(4, 4)); // (1,0) head
        assert!(!rev(4, 5)); // past the head
        assert!(!rev(2, 5)); // the border row is never touched
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), enc, "{plain:?}");
        }
    }

    #[test]
    fn osc52_wraps_the_base64_payload() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }
}
