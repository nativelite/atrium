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
    /// Open a plain **shell** in a new window — `Ctrl+A !`. Unlike `NewPane`
    /// (which runs the launch command, e.g. claude), this gives you a shell to
    /// drive `amux ctl` from and see its rendered output.
    NewShellPane,
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
    /// Toggle mouse mode — `Ctrl+A m`. When on, clicks focus panes (and the
    /// host captures the mouse); when off, the terminal's native drag-to-select
    /// / copy is restored.
    ToggleMouse,
    /// Toggle the full-screen **board** dashboard — `Ctrl+A b`. An overlay of the
    /// shared board (source-of-truth state); keystrokes don't reach the panes
    /// while it's up.
    ToggleBoard,
    /// A left-button press at 1-based terminal cell (`col`, `row`) — only
    /// emitted while mouse mode is on. Used to focus the pane under the cursor.
    MouseClick {
        col: u16,
        row: u16,
    },
    /// A scroll-wheel notch at 1-based terminal cell (`col`, `row`) — only
    /// emitted while mouse mode is on. `up` is true for wheel-up. Routed to the
    /// pane under the cursor so you can scroll whichever tile you're hovering.
    MouseScroll {
        up: bool,
        col: u16,
        row: u16,
    },
    Quit,
}

/// The scanner's arming state: idle, prefix seen (command pending), an arrow
/// escape mid-parse after a prefixed `ESC`, or an SGR-mouse escape mid-parse
/// (only reached while mouse mode is on).
#[derive(Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Armed,
    /// Prefix + `ESC` seen; expecting `[`.
    ArrowEsc,
    /// Prefix + `ESC [` seen; expecting the final letter `A`/`B`/`C`/`D`.
    ArrowCsi,
    /// Bare `ESC` seen with mouse mode on; expecting `[` to continue a possible
    /// SGR mouse sequence (anything else is forwarded verbatim).
    MouseEsc,
    /// Bare `ESC [` seen with mouse mode on; expecting `<` (SGR mouse) — else
    /// forward verbatim.
    MouseCsi,
    /// Inside an SGR mouse sequence (`ESC [ <` seen); accumulating
    /// `Cb;Cx;Cy` until the terminating `M` (press) or `m` (release).
    MouseParams,
}

#[derive(Debug, Default)]
pub struct PrefixScanner {
    state: State,
    /// Whether mouse mode is on. Only when on does a *bare* `ESC` begin SGR
    /// mouse detection; off, bare escapes forward immediately (0.1 behavior), so
    /// a lone ESC keypress is never delayed for a TUI in the pane.
    mouse: bool,
    /// Accumulates the `Cb;Cx;Cy` digits of an in-progress SGR mouse sequence.
    mouse_buf: Vec<u8>,
}

impl PrefixScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when a prefix has been seen and a command (or arrow) is pending.
    pub fn armed(&self) -> bool {
        self.state != State::Idle
    }

    /// Turn mouse-sequence interception on or off. Kept in sync with the host's
    /// actual mouse capture (`rawterm::Terminal::set_mouse`) so the scanner only
    /// intercepts `ESC [ <` sequences when the terminal is really sending them.
    pub fn set_mouse(&mut self, on: bool) {
        self.mouse = on;
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
                        b'!' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::NewShellPane);
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
                        b'm' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::ToggleMouse);
                        }
                        b'b' => {
                            flush(&mut run, &mut actions);
                            actions.push(Action::ToggleBoard);
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
                State::MouseEsc => {
                    // Bare `ESC` (mouse mode): only `[` continues toward an SGR
                    // mouse sequence; anything else is a normal escape — forward
                    // the `ESC` and re-handle this byte as an idle byte.
                    if b == b'[' {
                        self.state = State::MouseCsi;
                    } else {
                        run.push(0x1b);
                        self.state = State::Idle;
                        if b == PREFIX {
                            self.state = State::Armed;
                        } else {
                            run.push(b);
                        }
                    }
                    continue;
                }
                State::MouseCsi => {
                    // `ESC [` (mouse mode): `<` confirms SGR mouse; else forward
                    // `ESC [` and re-handle this byte.
                    if b == b'<' {
                        self.mouse_buf.clear();
                        self.state = State::MouseParams;
                    } else {
                        run.push(0x1b);
                        run.push(b'[');
                        self.state = State::Idle;
                        if b == PREFIX {
                            self.state = State::Armed;
                        } else {
                            run.push(b);
                        }
                    }
                    continue;
                }
                State::MouseParams => {
                    // Accumulate `Cb;Cx;Cy` until the terminator: `M` = press,
                    // `m` = release. We act on a left-button press only.
                    if b == b'M' || b == b'm' {
                        if b == b'M' {
                            if let Some((cb, col, row)) = parse_sgr_mouse(&self.mouse_buf) {
                                if is_left_press(cb) {
                                    flush(&mut run, &mut actions);
                                    actions.push(Action::MouseClick { col, row });
                                } else if let Some(up) = wheel_dir(cb) {
                                    flush(&mut run, &mut actions);
                                    actions.push(Action::MouseScroll { up, col, row });
                                }
                            }
                        }
                        self.mouse_buf.clear();
                        self.state = State::Idle;
                    } else if self.mouse_buf.len() < 32 {
                        // Bound the buffer so a malformed stream can't grow it.
                        self.mouse_buf.push(b);
                    }
                    continue;
                }
                State::Idle => {
                    if b == PREFIX {
                        self.state = State::Armed;
                    } else if self.mouse && b == 0x1b {
                        self.state = State::MouseEsc;
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

/// Parse an SGR mouse body `Cb;Cx;Cy` into `(button_flags, col, row)`. Columns
/// and rows are 1-based terminal cells. Returns `None` on a malformed body.
fn parse_sgr_mouse(buf: &[u8]) -> Option<(u32, u16, u16)> {
    let s = std::str::from_utf8(buf).ok()?;
    let mut parts = s.split(';');
    let cb: u32 = parts.next()?.parse().ok()?;
    let col: u16 = parts.next()?.parse().ok()?;
    let row: u16 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None; // extra fields → not a well-formed mouse report
    }
    Some((cb, col, row))
}

/// True when an SGR button code is a plain **left-button** event (button bits
/// `00`) that is neither pointer **motion** (bit 5, `0x20`) nor a **wheel**
/// event (bit 6, `0x40`) — i.e. a real left click, not a drag or scroll.
fn is_left_press(cb: u32) -> bool {
    cb & 0b0110_0011 == 0
}

/// If `cb` is a **wheel** event (bit 6, `0x40`, set) and not pointer motion
/// (bit 5, `0x20`, clear), return `Some(up)` — `up == true` for wheel-up
/// (button 64) and `false` for wheel-down (button 65). The low bit selects the
/// direction; modifier bits (2–4) are ignored. `None` for non-wheel events.
fn wheel_dir(cb: u32) -> Option<bool> {
    if cb & 0x40 != 0 && cb & 0x20 == 0 {
        Some(cb & 0x01 == 0)
    } else {
        None
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

#[cfg(test)]
mod scroll_tests {
    use super::*;

    fn scan(bytes: &[u8]) -> Vec<Action> {
        let mut s = PrefixScanner::new();
        s.set_mouse(true);
        s.feed(bytes)
    }

    #[test]
    fn wheel_up_and_down_parse_to_scroll() {
        // SGR: `ESC [ < 64 ; col ; row M` = wheel up; 65 = wheel down.
        assert_eq!(
            scan(b"\x1b[<64;12;7M"),
            vec![Action::MouseScroll { up: true, col: 12, row: 7 }]
        );
        assert_eq!(
            scan(b"\x1b[<65;3;20M"),
            vec![Action::MouseScroll { up: false, col: 3, row: 20 }]
        );
    }

    #[test]
    fn wheel_with_modifiers_still_scrolls() {
        // Ctrl+wheel-up: button 64 + 0x10 = 80; still a wheel-up.
        assert_eq!(
            scan(b"\x1b[<80;1;1M"),
            vec![Action::MouseScroll { up: true, col: 1, row: 1 }]
        );
    }

    #[test]
    fn left_click_is_not_a_scroll() {
        assert_eq!(
            scan(b"\x1b[<0;5;5M"),
            vec![Action::MouseClick { col: 5, row: 5 }]
        );
    }

    #[test]
    fn scroll_only_when_mouse_mode_on() {
        // Mouse off: the sequence is forwarded verbatim, never a scroll.
        let mut s = PrefixScanner::new();
        let out = s.feed(b"\x1b[<64;12;7M");
        assert!(matches!(out.as_slice(), [Action::Forward(_)]));
    }

    #[test]
    fn prefix_bang_opens_a_shell_pane() {
        let mut s = PrefixScanner::new();
        assert_eq!(s.feed(&[PREFIX, b'!']), vec![Action::NewShellPane]);
    }

    #[test]
    fn prefix_b_toggles_the_board() {
        let mut s = PrefixScanner::new();
        assert_eq!(s.feed(&[PREFIX, b'b']), vec![Action::ToggleBoard]);
    }
}
