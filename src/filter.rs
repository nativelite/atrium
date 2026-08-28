//! Passthrough filter: terminate host-level mode negotiations at amux.
//!
//! A pane's ConPTY asks *its host* to switch input to win32-input-mode by
//! emitting `ESC[?9001h`. amux is that host — if the request tunnels
//! through to amux's own terminal, *that* terminal flips amux's stdin into
//! win32-encoded key sequences and every hotkey goes dark (this exact bug
//! shipped the first e2e run). The filter removes the enable/disable
//! sequences from the passthrough stream, chunk-split-safely; everything
//! else passes untouched.

const ENABLE: &[u8] = b"\x1b[?9001h";
const DISABLE: &[u8] = b"\x1b[?9001l";

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
            if rest == &ENABLE[..rest.len().min(ENABLE.len())] && rest.len() < ENABLE.len() {
                // Could still become the sequence: hold it for next chunk.
                self.pending = rest.to_vec();
                return out;
            }
            if rest.starts_with(ENABLE) || rest.starts_with(DISABLE) {
                i += ENABLE.len(); // both are the same length
                continue;
            }
            // A different escape: emit the ESC and move on.
            out.push(0x1B);
            i += 1;
        }
        out
    }
}
