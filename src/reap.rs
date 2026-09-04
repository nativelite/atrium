//! Process-**tree** teardown: kill what a pane spawned, not just the pane.
//!
//! [`pty::Pty::kill`] terminates the direct child only. An agent CLI spawns its
//! own subprocesses — MCP servers, language servers, node helpers — and those
//! survive, reparented onto `init`, running and billing. This is not a
//! signal-handling edge case: it happens on a clean `Ctrl+A q`, and a fleet
//! review measured 23 leaked agent processes holding 4.6 GB.
//!
//! Unix has no "kill this tree" primitive, but `pty` runs `setsid()` in
//! `pre_exec`, so every pane child is a session leader whose **process-group id
//! equals its pid**. One `killpg` therefore reaches the whole subtree.
//!
//! Two deliberate policy choices live here rather than in `pty`:
//!
//! - **`SIGTERM` before `SIGKILL`.** Today's bare `SIGKILL` is uncatchable, so
//!   it *guarantees* orphans: the agent never gets the chance to reap its own
//!   children. Terming first lets a well-behaved one tear its tree down itself.
//! - **Signal every group, then wait once.** A per-pane grace period would make
//!   teardown O(panes) in wall clock — ten panes at 750 ms is a seven-second
//!   quit. Signalling all of them before the single wait keeps it O(1).
//!
//! The residual gap, stated plainly: a grandchild that calls `setsid()` itself
//! leaves the group and this cannot reach it. macOS offers no container to catch
//! that (no cgroups, no Job Objects); the backstop is the registry sweep.

use std::time::Duration;

/// How long a pane's tree gets to exit on `SIGTERM` before `SIGKILL`. Long
/// enough for an agent to reap its children, short enough not to feel like a
/// hang on the way out.
pub const GRACE: Duration = Duration::from_millis(750);

/// Ask the process group led by `pid` to exit. Returns whether the signal was
/// delivered — `false` means the group is already gone, which is success.
pub fn term_tree(pid: u32) -> bool {
    sys::signal_group(pid, sys::SIGTERM)
}

/// Force the process group led by `pid` to exit. Call after [`GRACE`].
pub fn kill_tree(pid: u32) -> bool {
    sys::signal_group(pid, sys::SIGKILL)
}

/// Is any process still alive in the group led by `pid`? Signal 0 delivers
/// nothing and only reports reachability.
pub fn tree_alive(pid: u32) -> bool {
    sys::signal_group(pid, 0)
}

#[cfg(unix)]
mod sys {
    pub const SIGKILL: i32 = 9;
    pub const SIGTERM: i32 = 15;

    extern "C" {
        /// `int killpg(pid_t pgrp, int sig)` — signal every process in a group.
        fn killpg(pgrp: i32, sig: i32) -> i32;
    }

    pub fn signal_group(pid: u32, sig: i32) -> bool {
        if pid == 0 {
            return false;
        }
        // SAFETY: a plain signal send; `pid` is a pane child's pid, which is its
        // process-group id because `pty` makes every child a session leader.
        unsafe { killpg(pid as i32, sig) == 0 }
    }
}

#[cfg(not(unix))]
mod sys {
    pub const SIGKILL: i32 = 9;
    pub const SIGTERM: i32 = 15;

    /// Windows has no process groups in the POSIX sense. The equivalent bound on
    /// a tree is a Job Object created at spawn with
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, which the kernel honours however
    /// the parent dies — a stronger guarantee than this module can offer on
    /// unix. Tracked separately; until it lands, Windows teardown is unchanged
    /// (`Pty::kill` on the direct child) and this reports "nothing signalled"
    /// rather than pretending to have torn down a tree.
    pub fn signal_group(_pid: u32, _sig: i32) -> bool {
        false
    }
}
