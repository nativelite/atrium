//! The prefix-key scanner: pure bytes in, actions out.
//!
//! Everything the user types passes through untouched except the prefix
//! (`Ctrl+A`, byte `0x01`), which arms a one-byte command. `Ctrl+A Ctrl+A`
//! sends a literal `0x01` to the pane. The armed state survives chunk
//! boundaries, so a prefix arriving at the end of one read and its command
//! at the start of the next behave identically to both in one read.

/// The prefix byte: Ctrl+A.
pub const PREFIX: u8 = 0x01;

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
    Quit,
}

#[derive(Debug, Default)]
pub struct PrefixScanner {
    armed: bool,
}

impl PrefixScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when a prefix has been seen and its command byte is pending.
    pub fn armed(&self) -> bool {
        self.armed
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
            if self.armed {
                self.armed = false;
                match b {
                    PREFIX => run.push(PREFIX), // literal Ctrl+A
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
                    d @ b'1'..=b'9' => {
                        flush(&mut run, &mut actions);
                        actions.push(Action::SwitchTo((d - b'1') as usize));
                    }
                    _ => {} // unknown command: swallow prefix and byte
                }
                continue;
            }
            if b == PREFIX {
                self.armed = true;
                continue;
            }
            run.push(b);
        }
        flush(&mut run, &mut actions);
        actions
    }
}
