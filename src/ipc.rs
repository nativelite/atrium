//! The `ctl` control channel, served by the shared [`nlipc`] crate
//! (`nativelite-ipc`): a Windows named pipe or a unix-domain socket carrying
//! one JSON request line and one JSON reply line per client connection, which
//! atrium drains from its run loop without blocking and `atrium ctl <cmd>`
//! speaks as a short-lived client. The transport, its hardening (same-user
//! endpoint, bounded and deadlined replies, parked half-open clients) and its
//! threat model are documented in that crate; this module only names atrium's
//! endpoint and keeps the four audited exploits replaying against the real
//! atrium binary.

pub use nlipc::{idle_ms, request, request_held, Listener, Ticket, HOLD_DEADLINE};

use crate::events::Doorbell;
use std::sync::Mutex;

/// The application name atrium's endpoint carries:
/// `\\.\pipe\atrium-ctl-<pid>` on Windows, `<tmp>/atrium-ctl-<pid>.sock` on unix.
pub const APP: &str = "atrium";

/// The run loop's doorbell, once the loop has one. The endpoint is bound before
/// the loop exists (a fleet binds before it spawns its roster, so the panes are
/// born with `ATRIUM_CTL`), so the listener's accept thread rings through this
/// slot rather than a bell handed to `bind`: empty until [`set_bell`], and a
/// ring into an empty slot is a no-op the loop's next tick makes good.
static BELL: Mutex<Option<Doorbell>> = Mutex::new(None);

/// Bind atrium's endpoint at `addr`, woken on connect through [`ring`].
pub fn bind(addr: &str) -> std::io::Result<Listener> {
    Listener::bind_waking(addr, Box::new(ring))
}

/// Install the run loop's doorbell, so connects wake it. One loop per process;
/// a later call replaces the bell (a `recover` after a session ends).
pub fn set_bell(bell: Doorbell) {
    *BELL.lock().unwrap_or_else(|e| e.into_inner()) = Some(bell);
}

/// Ring the run loop's doorbell, if it has one. Called on the accept thread.
pub fn ring() {
    if let Some(bell) = BELL.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        bell.ring();
    }
}

/// The endpoint atrium binds for this process, unique per pid so two atrium
/// instances never collide. Injected as `ATRIUM_CTL` into spawned panes.
pub fn default_address() -> String {
    nlipc::address(APP, std::process::id())
}

/// The endpoint the atrium with process id `pid` binds: what a host that
/// started atrium (or a bench harness) connects to without being handed
/// `ATRIUM_CTL`. The same naming as [`default_address`] for this process.
pub fn address_for(pid: u32) -> String {
    nlipc::address(APP, pid)
}

#[cfg(all(test, unix))]
mod exploit_replays;
