//! Termination-signal disposition — so a terminal that goes away does not
//! orphan the agents.
//!
//! amux already tears its panes down correctly: when the event loop exits, it
//! kills every pane's pty. The problem was that only a *graceful* exit reached
//! that code. A closed terminal window sends `SIGHUP`, whose default action
//! terminates the process outright — the loop never unwinds, the teardown never
//! runs, and every hosted agent is reparented to `init` and keeps running.
//!
//! Observed live on macOS 26.6.2: ten Claude agents still resident, 1.5 GB, 19
//! minutes after the operator closed the window. Invisible, and billable.
//!
//! The fix is deliberately minimal. The handler does exactly one thing — a
//! relaxed atomic store — which is async-signal-safe; it emphatically does not
//! allocate, lock, or write to the terminal. The event loop polls stdin on a
//! 15 ms timeout, so it observes the flag almost immediately and then leaves
//! through its **normal** path, meaning the existing teardown does the work. No
//! second cleanup path to keep in sync with the first.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set from the signal handler; read by the event loop.
static TERMINATING: AtomicBool = AtomicBool::new(false);

/// Has a termination signal arrived? The event loop breaks when this is true.
pub fn terminating() -> bool {
    TERMINATING.load(Ordering::Relaxed)
}

/// Install handlers for the signals that mean "the session is over".
///
/// Idempotent, and safe to call before the terminal is put in raw mode.
pub fn install() {
    sys::install();
}

#[cfg(unix)]
mod sys {
    use super::{Ordering, TERMINATING};

    /// Controlling terminal went away — a closed window or tab. This is the one
    /// that bit us.
    const SIGHUP: i32 = 1;
    /// Interrupt. In raw mode `ISIG` is off, so Ctrl+C reaches the focused pane
    /// as a byte and never becomes a signal here; this covers an interrupt sent
    /// from outside (`kill -INT`).
    const SIGINT: i32 = 2;
    /// Polite termination — what `kill` sends by default.
    const SIGTERM: i32 = 15;

    extern "C" {
        /// `sighandler_t signal(int signum, sighandler_t handler)`. Both the
        /// handler and the return are function pointers, carried as `usize`.
        fn signal(signum: i32, handler: usize) -> usize;
    }

    /// The whole handler: one relaxed store. Nothing here allocates, locks, or
    /// touches the terminal, so it is safe in async-signal context.
    extern "C" fn on_term(_signum: i32) {
        TERMINATING.store(true, Ordering::Relaxed);
    }

    pub fn install() {
        let h = on_term as extern "C" fn(i32) as usize;
        for sig in [SIGHUP, SIGINT, SIGTERM] {
            // SAFETY: installing a handler that only stores to a static atomic.
            unsafe { signal(sig, h) };
        }
    }
}

#[cfg(not(unix))]
mod sys {
    /// Windows has no SIGHUP, and console-close notification arrives through
    /// `SetConsoleCtrlHandler` instead. The durable answer there is a Job
    /// Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, which the kernel
    /// honours however the process dies — a stronger guarantee than this, and
    /// tracked separately. No-op until then; Windows teardown is unchanged.
    pub fn install() {}
}
