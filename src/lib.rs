//! atrium — tmux for agents, on the nativelite stack alone (`pty` +
//! `rawterm` + `ansi`; zero third-party dependencies).
//!
//! A multi-agent terminal: each pane hosts one of *your own* CLIs — a
//! coding agent, a shell — on a real pseudo-terminal, full screen, with a
//! one-line status bar naming every pane and flagging activity in the ones
//! you are not watching. `Ctrl+A` is the prefix: digits switch panes,
//! `c` opens a new one, `n`/`p` cycle, `x` kills, `q` quits.
//!
//! The architecture is deliberate **passthrough**: the active pane's VT
//! byte stream goes straight to your real terminal (perfect fidelity — the
//! outer terminal does the emulation), and your keystrokes go straight
//! back, undecoded. On Windows, ConPTY maintains each pane's screen
//! itself and repaints in full on resize, so switching panes is a resize
//! nudge, not a rebuilt frame. atrium hosts programs; it never inspects,
//! modifies, or credentials them.

pub mod audit;
pub mod bar;
pub mod bind;
pub mod context;
pub mod ctl;
// The coordination layer (board + bus) now lives in the `abus` org crate;
// re-exported here so `atrium::board` / `atrium::bus` (and `crate::bus` inside the
// crate) keep resolving unchanged.
pub use abus::{board, bus};
pub mod filter;
pub mod fleet;
pub mod identity;
pub mod input;
pub mod ipc;
pub mod layout;
pub mod orphan;
pub mod reap;
pub mod resolve;
pub mod resources;
pub mod session;
pub mod signals;
pub mod spawn;
pub mod theme;
pub mod tile;
pub mod trust;
pub mod uid;
pub mod vendors;
pub mod warden;
pub mod worktree;
