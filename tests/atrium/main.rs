//! End-to-end tests for atrium: the real thing — atrium itself spawned inside
//! a `pty`, driven with keystrokes, its passthrough output read back.
//! Deadline-bounded throughout. (The pure scanner / bar / filter / resolver
//! tests are unit tests in the library, beside the code they check.)

#![allow(
    clippy::unwrap_used,
    reason = "test code: a panic is the failure report; allow-unwrap-in-tests covers #[test] fns, not the helpers they share"
)]

mod support;

mod cli;
mod ctl;
mod fleet;
mod process_tree;
mod recover;
#[cfg(any(windows, target_os = "linux"))]
mod resources;
mod session;
mod tiling;
#[cfg(unix)]
mod trust_ceiling;
