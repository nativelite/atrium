//! Tests for atrium: the pure prefix scanner and bar builder, then the real
//! thing — atrium itself spawned inside a `pty`, driven with keystrokes,
//! its passthrough output read back. Deadline-bounded throughout.

mod support;

mod bar;
mod cli;
mod ctl;
mod filter;
mod fleet;
mod input;
mod process_tree;
mod recover;
mod resolve;
#[cfg(any(windows, target_os = "linux"))]
mod resources;
mod session;
mod tiling;
#[cfg(unix)]
mod trust_ceiling;
