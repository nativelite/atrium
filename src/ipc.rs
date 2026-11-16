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
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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

/// The launch flag that names the endpoint: `--ctl-socket <path>`.
pub const SOCKET_FLAG: &str = "--ctl-socket";
/// The same choice from the environment; the flag wins over it.
pub const ENV_CTL_SOCKET: &str = "ATRIUM_CTL_SOCKET";

/// The `--ctl-socket` value of this launch, once `main` has parsed it.
static FLAG_SOCKET: OnceLock<String> = OnceLock::new();

/// Where this session's endpoint goes, and whether the operator chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// What is bound, and what panes receive as `ATRIUM_CTL`.
    pub address: String,
    /// Named by `--ctl-socket` or `ATRIUM_CTL_SOCKET`, not derived from the
    /// pid. A bind failure on a chosen address fails the launch: an operator
    /// who asked for a path must not get a session silently without it.
    pub explicit: bool,
}

/// Pull `--ctl-socket <path>` (or `--ctl-socket=<path>`) out of atrium's
/// leading launch flags, wherever it stands among them, and return the
/// normalized address with the other arguments untouched. The walk knows the
/// launch flags [`crate::ctl::parse_flags`] takes and stops at the first
/// token that is not one, so a `--ctl-socket` the hosted program takes is
/// never eaten. A value that cannot be an endpoint, a missing value, or the
/// flag given twice is the `Err` line.
pub fn take_socket_flag(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let assigned = format!("{SOCKET_FLAG}=");
    let mut found: Option<String> = None;
    let mut rest: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (value, width) = if arg == SOCKET_FLAG {
            let v = args
                .get(i + 1)
                .ok_or_else(|| format!("{SOCKET_FLAG} needs a path"))?;
            (v.as_str(), 2)
        } else if let Some(v) = arg.strip_prefix(&assigned) {
            (v, 1)
        } else {
            // Another launch flag: step over it and, where it has one, its value.
            let width =
                match arg {
                    "--allow-ctl" | "--skip-permissions" => 1,
                    "--max-depth" => 2,
                    "--trust" => {
                        let policy = args.get(i + 1).map(String::as_str);
                        1 + usize::from(policy.is_some_and(|p| {
                            crate::ctl::TrustMode::from_policy_keyword(p).is_some()
                        }))
                    }
                    a if a.starts_with("--trust=") || a.starts_with("--max-depth=") => 1,
                    _ => break,
                };
            let end = (i + width).min(args.len());
            rest.extend_from_slice(&args[i..end]);
            i = end;
            continue;
        };
        if found.is_some() {
            return Err(format!("{SOCKET_FLAG} given twice"));
        }
        found = Some(normalize_socket(value, cfg!(windows))?);
        i += width;
    }
    rest.extend_from_slice(&args[i..]);
    Ok((found, rest))
}

/// Record this launch's `--ctl-socket` value (already normalized by
/// [`take_socket_flag`]). The first call wins; there is one launch.
pub fn set_socket_flag(address: String) {
    let _ = FLAG_SOCKET.set(address);
}

/// Turn an operator's `--ctl-socket` / `ATRIUM_CTL_SOCKET` value into the
/// address to bind, or say why it cannot be one.
///
/// On unix it is a socket path and must be absolute: every pane gets it as
/// `ATRIUM_CTL` and runs in a directory of its own. On Windows it is a
/// named-pipe name, given whole (`\\.\pipe\<name>`) or bare (`<name>`, which
/// gets the prefix). A comma is refused on both: the tmux shim hands the
/// address to Claude Code as the first field of `TMUX=<address>,<pid>,0`.
pub fn normalize_socket(raw: &str, windows: bool) -> Result<String, String> {
    const PIPE: &str = r"\\.\pipe\";
    if raw.is_empty() {
        return Err("the ctl socket path is empty".into());
    }
    if raw.contains(',') {
        return Err(format!(
            "ctl socket {raw:?} has a comma in it, which the TMUX value handed to \
             Claude Code's teams (<address>,<pid>,0) cannot carry"
        ));
    }
    if raw.chars().any(char::is_control) {
        return Err(format!("ctl socket {raw:?} has a control character in it"));
    }
    if windows {
        let name = raw.strip_prefix(PIPE).unwrap_or(raw);
        if name.is_empty() || name.contains(['\\', '/', ':']) {
            return Err(format!(
                "ctl socket {raw:?} is not a named-pipe name: on Windows give \
                 {PIPE}<name> or a bare <name>"
            ));
        }
        Ok(format!("{PIPE}{name}"))
    } else if raw.starts_with('/') {
        Ok(raw.to_string())
    } else {
        Err(format!(
            "ctl socket {raw:?} is not an absolute path (every pane gets it as \
             ATRIUM_CTL, each in its own directory)"
        ))
    }
}

/// Choose the endpoint from a flag value (already normalized), an
/// environment value (not yet) and the pid: the flag wins, then the
/// environment, then the per-pid default. An empty environment value is
/// unset. Pure, so the precedence is testable without a process environment.
pub fn resolve(flag: Option<&str>, env: Option<&str>, pid: u32) -> Result<Endpoint, String> {
    if let Some(address) = flag {
        return Ok(Endpoint {
            address: address.to_string(),
            explicit: true,
        });
    }
    match env.filter(|v| !v.is_empty()) {
        Some(raw) => Ok(Endpoint {
            address: normalize_socket(raw, cfg!(windows))
                .map_err(|why| format!("{ENV_CTL_SOCKET}: {why}"))?,
            explicit: true,
        }),
        None => Ok(Endpoint {
            address: address_for(pid),
            explicit: false,
        }),
    }
}

/// This process's endpoint: [`resolve`] over the parsed flag, the
/// environment and our pid.
pub fn endpoint() -> Result<Endpoint, String> {
    resolve(
        FLAG_SOCKET.get().map(String::as_str),
        std::env::var(ENV_CTL_SOCKET).ok().as_deref(),
        std::process::id(),
    )
}

/// Bind `endpoint` and return the listener with the address it serves: the
/// one to publish to panes. The transport refuses an address with a live
/// listener and takes over a stale one (`nlipc::Listener::bind`); the `Err`
/// line names a chosen address, since the operator has to act on it.
pub fn bind_endpoint(endpoint: &Endpoint) -> Result<(Listener, String), String> {
    match bind(&endpoint.address) {
        Ok(listener) => Ok((listener, endpoint.address.clone())),
        Err(e) if endpoint.explicit => Err(format!(
            "ctl socket {} could not be bound: {e}",
            endpoint.address
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// The directory of discovery files: one per running ctl session, named
/// `<pid>.endpoint`, holding that session's address. Under the per-user
/// state root, beside the per-project session directories.
pub fn discovery_dir() -> Option<PathBuf> {
    crate::session_store::state_root().map(|root| root.join("ctl"))
}

/// The discovery file this process wrote, for [`retract_endpoint`].
static PUBLISHED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Record `address` as this process's endpoint in the discovery directory,
/// so `atrium ctl` outside a pane, or a host's helper, can find the session
/// without searching the temp directory. Best effort: a session with no
/// state root, or one that cannot write there, runs without the file.
pub fn publish_endpoint(address: &str) {
    let Some(dir) = discovery_dir() else { return };
    if let Ok(path) = publish_endpoint_in(&dir, std::process::id(), address) {
        *PUBLISHED.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
    }
}

/// [`publish_endpoint`] into `dir` for `pid`: owner-only, written whole.
pub fn publish_endpoint_in(dir: &Path, pid: u32, address: &str) -> std::io::Result<PathBuf> {
    crate::session_store::ensure_private_dir(dir)?;
    let path = dir.join(format!("{pid}.endpoint"));
    crate::session_store::write_private(&path, &format!("{address}\n"))?;
    Ok(path)
}

/// Remove the discovery file this process published, if it did. Called as
/// atrium exits; a session that dies without it leaves a file whose endpoint
/// no longer answers, which [`discover_in`] removes.
pub fn retract_endpoint() {
    if let Some(path) = PUBLISHED.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = std::fs::remove_file(path);
    }
}

/// The sessions `dir` names whose endpoint `live` says is answering, as
/// `(pid, address)` in pid order. A file whose endpoint does not answer is
/// stale (its session was killed) and is removed on the way.
pub fn discover_in(dir: &Path, live: impl Fn(&str) -> bool) -> Vec<(u32, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("endpoint") {
            continue;
        }
        let pid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok());
        let address = std::fs::read_to_string(&path)
            .ok()
            .map(|body| body.trim().to_string())
            .filter(|a| !a.is_empty());
        match (pid, address) {
            (Some(pid), Some(address)) if live(&address) => found.push((pid, address)),
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    found.sort();
    found
}

/// The one running ctl session's address, for a caller that was handed no
/// `ATRIUM_CTL`. `Err` says why there is no single answer: none is running,
/// or several are and the caller must name one.
pub fn discover_one() -> Result<String, String> {
    let dir = discovery_dir().ok_or("no per-user state directory to look in")?;
    let mut found = discover_in(&dir, nlipc::endpoint_live);
    match found.len() {
        0 => Err("no running ctl session was found".into()),
        1 => Ok(found.remove(0).1),
        n => Err(format!(
            "{n} ctl sessions are running ({}); set ATRIUM_CTL to the one you mean",
            found
                .iter()
                .map(|(pid, address)| format!("pid {pid} at {address}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
mod exploit_replays;
