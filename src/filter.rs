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
