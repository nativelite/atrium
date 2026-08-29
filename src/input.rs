//! The prefix-key scanner: pure bytes in, actions out.
//!
//! Everything the user types passes through untouched except the prefix
//! (`Ctrl+A`, byte `0x01`), which arms a one-byte command. `Ctrl+A Ctrl+A`
//! sends a literal `0x01` to the pane. The armed state survives chunk
//! boundaries, so a prefix arriving at the end of one read and its command
//! at the start of the next behave identically to both in one read.
//!
//! 0.2 adds the tiling commands (splits, focus movement, zoom) on top of the
//! 0.1 window commands, all additive — every 0.1 key still means what it did.
//! Focus movement accepts both `h/j/k/l` and the arrow keys; an arrow arrives
//! as the three-byte `ESC [ A/B/C/D`, so after the prefix arms, an `ESC` puts
//! the scanner into a short "arrow pending" state that consumes the `[` and the
//! final letter before emitting the move.

/// The prefix byte: Ctrl+A.
pub const PREFIX: u8 = 0x01;

/// A focus-movement direction (arrows or `h/j/k/l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Bytes to forward to the active pane verbatim.
    Forward(Vec<u8>),
    NextPane,
    PrevPane,
    NewPane,
    KillPane,
    /// Switch to pane index (0-based; from digits 1-9).
    SwitchTo(usize),
    /// Split the focused pane horizontally (stacked) — `Ctrl+A "`.
    SplitH,
    /// Split the focused pane vertically (side by side) — `Ctrl+A %`.
    SplitV,
    /// Move focus between tiled panes — `Ctrl+A` + arrow or `h/j/k/l`.
    MoveFocus(Dir),
    /// Toggle the focused pane to/from full-screen passthrough — `Ctrl+A z`.
    Zoom,
    Quit,
}

/// The scanner's arming state: idle, prefix seen (command pending), or an
/// arrow escape mid-parse after a prefixed `ESC`.
#[derive(Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Armed,
    /// Prefix + `ESC` seen; expecting `[`.
    ArrowEsc,
    /// Prefix + `ESC [` seen; expecting the final letter `A`/`B`/`C`/`D`.
    ArrowCsi,
}

#[derive(Debug, Default)]
pub struct PrefixScanner {
    state: State,
}

impl PrefixScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when a prefix has been seen and a command (or arrow) is pending.
    pub fn armed(&self) -> bool {
        self.state != State::Idle
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut run: Vec<u8> = Vec::new();
        let mut flush = |run: &mut Vec<u8>, actions: &mut Vec<Action>| {
            if !run.is_empty() {
                actions.push(Action::Forward(std::mem::take(run)));
            }
        };
        for &b in bytes {
            match self.state {
                State::ArrowEsc => {
                    // After `Ctrl+A ESC`, a `[` continues the arrow sequence;
                    // anything else aborts (swallowed — the prefix is spent).
                    self.state = if b == b'[' {
                        State::ArrowCsi
                    } else {
                        State::Idle
                    };
                    continue;
                }
                State::ArrowCsi => {
                    self.state = State::Idle;
                    if let Some(dir) = arrow_dir(b) {
                        flush(&mut run, &mut actions);
                        actions.push(Action::MoveFocus(dir));
                    }
                    continue;
                }
                State::Armed => {
                    self.state = State::Idle;
                    match b {
                        PREFIX => run.push(PREFIX), // literal Ctrl+A
                        0x1b => {
                            // Prefixed ESC: begin an arrow sequence.
                            self.state = State::ArrowEsc;
                        }
                        b'n' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::NextPane);
                        }
                        b'p' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::PrevPane);
                        }
                        b'c' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::NewPane);
                        }
                        b'x' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::KillPane);
                        }
                        b'q' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::Quit);
                        }
                        b'"' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::SplitH);
                        }
                        b'%' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::SplitV);
                        }
                        b'z' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::Zoom);
                        }
                        b'h' => push_move(Dir::Left, &mut run, &mut actions, &mut flush),
                        b'j' => push_move(Dir::Down, &mut run, &mut actions, &mut flush),
                        b'k' => push_move(Dir::Up, &mut run, &mut actions, &mut flush),
                        b'l' => push_move(Dir::Right, &mut run, &mut actions, &mut flush),
                        d @ b'1'..=b'9' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::SwitchTo((d - b'1') as usize));
                        }
                        _ => {} // unknown command: swallow prefix and byte
                    }
                }
                State::Idle => {
                    if b == PREFIX {
                        self.state = State::Armed;
                    } else {
                        run.push(b);
                    }
                }
            }
        }
        flush(&mut run, &mut actions);
        actions
    }
}

/// Emit a focus-move action, flushing any pending forward run first.
fn push_move(
    dir: Dir,
    run: &mut Vec<u8>,
    actions: &mut Vec<Action>,
    flush: &mut impl FnMut(&mut Vec<u8>, &mut Vec<Action>),
) {
    flush(run, actions);
    actions.push(Action::MoveFocus(dir));
}

/// Map an arrow CSI final byte to a direction.
fn arrow_dir(b: u8) -> Option<Dir> {
    match b {
        b'A' => Some(Dir::Up),
        b'B' => Some(Dir::Down),
        b'C' => Some(Dir::Right),
        b'D' => Some(Dir::Left),
        _ => None,
    }
}
