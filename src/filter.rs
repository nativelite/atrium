//! Passthrough filter: terminate host-level mode negotiations at amux.
//!
//! A pane's ConPTY asks *its host* to switch input to win32-input-mode by
//! emitting `ESC[?9001h`. amux is that host — if the request tunnels
//! through to amux's own terminal, *that* terminal flips amux's stdin into
//! win32-encoded key sequences and every hotkey goes dark (this exact bug
//! shipped the first e2e run). The filter removes such host-level mode
//! sequences from the passthrough stream, chunk-split-safely; everything
//! else passes untouched.
//!
//! amux also **owns the alternate screen** (tmux-style): it enters the alt
//! buffer at startup and restores it on exit. A hosted app (claude, vim) that
//! toggles the alt buffer itself — `ESC[?1049h/l`, or the older `?1047`/`?47`
//! — would fight amux's own alt screen, so on exit amux's `\x1b[?1049l` would
//! leave the hosted app's last frame on screen instead of the user's original
//! shell. We strip the pane's alt-screen enter/leave here so panes render into
//! amux's buffer and never toggle the real terminal's; amux alone owns it.
//!
//! Finally, amux **owns the terminal window**. A pane's ConPTY announces its
//! size to *its host* by emitting an XTWINOPS resize, `ESC [ 8 ; H ; W t` (and a
//! full-screen app may move/resize/iconify the window with the rest of the
//! `ESC [ … t` family). amux is that host — if such a sequence tunnels through to
//! the real terminal, the terminal window itself resizes to the pane's size
//! (measured: a 70-row Windows Terminal shrank to 67 the instant a pane started).
//! We strip the whole `ESC [ <params> t` window-manipulation family from the
//! passthrough; amux already knows and sets each pane's size.

/// The host-level mode sequences we terminate at amux. Every entry shares the
/// `ESC [ ? … (h|l)` shape but the numeric parameter — and thus the length —
/// varies, so the matcher is length-agnostic (see [`Passthrough::feed`]).
const STRIP: &[&[u8]] = &[
    // win32-input-mode enable / disable (the original hotkey-eating bug).
    b"\x1b[?9001h",
    b"\x1b[?9001l",
    // Alternate screen buffer enter / leave — amux owns the alt screen.
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?1047h",
    b"\x1b[?1047l",
    b"\x1b[?47h",
    b"\x1b[?47l",
];

/// The longest sequence in [`STRIP`] — the most bytes we may need to hold at a
/// chunk boundary before a candidate can be resolved.
const MAX_LEN: usize = 8; // "\x1b[?1049h" / "\x1b[?9001h"

/// How a `rest` slice (starting at an ESC) relates to the XTWINOPS
/// window-manipulation family, `ESC [ <digits/semicolons> t`.
enum Xt {
    /// A complete `ESC [ … t` of this many bytes — strip it.
    Strip(usize),
    /// A prefix that could still grow into one — hold for the next chunk.
    Hold,
    /// Not a window-manipulation sequence — leave it to the rest of the filter.
    No,
}

/// Longest XTWINOPS parameter run we'll hold before giving up (real ones are
/// short, e.g. `8;68;280`); bounds the hold so a long unrelated CSI can't stall.
const XT_MAX_PARAMS: usize = 24;

/// Classify `rest` (guaranteed to start with `ESC`) against the `ESC [ … t`
/// window-op family. Only digits and `;` are allowed between `[` and the final
/// `t`; any other final byte (an SGR `m`, a cursor `H`, the alt-screen `?…h`) is
/// [`Xt::No`] so the normal filter handles it.
fn xtwinops(rest: &[u8]) -> Xt {
    match rest.get(1) {
        None => return Xt::Hold,      // lone ESC: could become `ESC [ … t`
        Some(&b'[') => {}             // a CSI — inspect the params
        Some(_) => return Xt::No,     // ESC-something-else
    }
    let mut j = 2;
    while let Some(&b) = rest.get(j) {
        match b {
            b'0'..=b'9' | b';' => {
                j += 1;
                if j - 2 > XT_MAX_PARAMS {
                    return Xt::No; // implausibly long — not XTWINOPS
                }
            }
            b't' => return Xt::Strip(j + 1),
            _ => return Xt::No, // some other CSI final byte
        }
    }
    Xt::Hold // ran out of bytes mid-params; may still end in `t`
}

/// Stateful stream filter; keep one per pane.
#[derive(Debug, Default)]
pub struct Passthrough {
    pending: Vec<u8>,
}

impl Passthrough {
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter the next chunk; returns the bytes to forward. A partial
    /// candidate sequence at the chunk end is held until resolved.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let buf = std::mem::take(&mut self.pending);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            let Some(esc_off) = buf[i..].iter().position(|&b| b == 0x1B) else {
                out.extend_from_slice(&buf[i..]);
                return out;
            };
            out.extend_from_slice(&buf[i..i + esc_off]);
            i += esc_off;
            let rest = &buf[i..];
            // Window-manipulation ops (`ESC [ … t`) are stripped so a pane can
            // never resize/move the real terminal window. Disjoint from `STRIP`
            // (those are `ESC [ ? …`), so check it first.
            match xtwinops(rest) {
                Xt::Strip(n) => {
                    i += n;
                    continue;
                }
                Xt::Hold => {
                    self.pending = rest.to_vec();
                    return out;
                }
                Xt::No => {}
            }
            // If `rest` is a strict prefix of any strip sequence (and could
            // still grow into it once more bytes arrive), hold it for the next
            // chunk. Only ever holds < MAX_LEN bytes.
            if rest.len() < MAX_LEN
                && STRIP
                    .iter()
                    .any(|seq| seq.len() > rest.len() && seq.starts_with(rest))
            {
                self.pending = rest.to_vec();
                return out;
            }
            // A full match strips the whole sequence; longest match wins so a
            // shorter prefix never eats part of a longer one.
            if let Some(seq) = STRIP
                .iter()
                .filter(|seq| rest.starts_with(seq))
                .max_by_key(|seq| seq.len())
            {
                i += seq.len();
                continue;
            }
            // A different escape: emit the ESC and move on.
            out.push(0x1B);
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(seq: &[&[u8]]) -> Vec<u8> {
        let mut f = Passthrough::new();
        let mut out = Vec::new();
        for chunk in seq {
            out.extend(f.feed(chunk));
        }
        out
    }

    #[test]
    fn strips_the_conpty_window_resize() {
        // The exact sequence a pane's ConPTY emits at startup: hide cursor,
        // resize the window to 68x280, home. Only the resize is removed.
        let input = b"\x1b[?25l\x1b[8;68;280t\x1b[HMicrosoft Windows";
        assert_eq!(
            feed_all(&[input]),
            b"\x1b[?25l\x1b[HMicrosoft Windows".to_vec()
        );
    }

    #[test]
    fn strips_the_whole_winop_family() {
        // move (3), pixel-resize (4), maximize (9;1), iconify (2) — all gone.
        let input = b"a\x1b[3;10;20tb\x1b[4;600;800tc\x1b[9;1td\x1b[2te";
        assert_eq!(feed_all(&[input]), b"abcde".to_vec());
    }

    #[test]
    fn resize_split_across_chunks_is_still_stripped() {
        assert_eq!(
            feed_all(&[b"X\x1b[8;", b"68;2", b"80tY"]),
            b"XY".to_vec()
        );
    }

    #[test]
    fn leaves_ordinary_csi_untouched() {
        // SGR, cursor moves, erase, and a device query all end in non-`t` finals.
        let input = b"\x1b[0m\x1b[1;44H\x1b[2K\x1b[38;2;70;235;255mhi\x1b[H";
        assert_eq!(feed_all(&[input]), input.to_vec());
    }

    #[test]
    fn still_strips_alt_screen_and_win32_input() {
        assert_eq!(
            feed_all(&[b"\x1b[?1049h\x1b[?9001hkeep\x1b[?1049l"]),
            b"keep".to_vec()
        );
    }
}
