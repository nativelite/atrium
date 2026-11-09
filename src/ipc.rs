//! The `ctl` control channel, served by the shared [`nlipc`] crate
//! (`nativelite-ipc`): a Windows named pipe or a unix-domain socket carrying
//! one JSON request line and one JSON reply line per client connection, which
//! atrium drains from its run loop without blocking and `atrium ctl <cmd>`
//! speaks as a short-lived client. The transport, its hardening (same-user
//! endpoint, bounded and deadlined replies, parked half-open clients) and its
//! threat model are documented in that crate; this module only names atrium's
//! endpoint and keeps the four audited exploits replaying against the real
//! atrium binary.

pub use nlipc::{idle_ms, request, Listener};

/// The application name atrium's endpoint carries:
/// `\\.\pipe\atrium-ctl-<pid>` on Windows, `<tmp>/atrium-ctl-<pid>.sock` on unix.
pub const APP: &str = "atrium";

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
