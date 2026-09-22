//! Helpers shared by this module's test files.

use std::sync::OnceLock;

/// A unique endpoint per test (tests run in parallel; a shared per-pid name
/// would collide). Platform-appropriate: a named pipe on Windows, a temp
/// socket path on unix.
pub(super) fn test_addr(nonce: u32) -> String {
    let pid = std::process::id();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\atrium-ctl-test-{pid}-{nonce}")
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("atrium-ctl-test-{pid}-{nonce}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

// =======================================================================
// The pure core. These run on every platform and cover the byte-level
// decisions BOTH `sys` modules make, so the Windows sequencing that cannot
// be executed on a mac is nevertheless tested there.
// =======================================================================

/// One shipping file with every comment line removed.
///
/// Both exclusions are load-bearing, and both were learned the hard way.
/// Dropping comments means a guard trips on code and never on the prose
/// explaining why the code is gone — this transport argues at length about the
/// blocking flush and about socket timeouts, and a guard that greps for those
/// names would fire on its own documentation. Keeping test code out (the tests
/// live in their own files now) means a guard cannot match the text of its own
/// failure message, which is exactly what a bare-identifier needle does when
/// the message names the thing it bans.
///
/// CRLF is normalized FIRST: `include_str!` embeds the file's bytes verbatim,
/// and on a Windows checkout (autocrlf) these files are CRLF, so an LF needle
/// would never match. (Found on Windows; the author ran this on a mac, where
/// the file is LF and the bug is invisible.)
fn strip_comments(src: &str) -> String {
    src.replace("\r\n", "\n")
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every shipping line of the transport: the API file and all five modules.
pub fn shipping_source() -> &'static str {
    static ONCE: OnceLock<String> = OnceLock::new();
    ONCE.get_or_init(|| {
        [
            include_str!("../ipc.rs"),
            include_str!("wire.rs"),
            include_str!("chan.rs"),
            include_str!("winmap.rs"),
            include_str!("sys_windows.rs"),
            include_str!("sys_unix.rs"),
        ]
        .iter()
        .map(|s| strip_comments(s))
        .collect::<Vec<_>>()
        .join("\n")
    })
}

/// All shipping code, comments stripped.
pub fn code_only() -> &'static str {
    shipping_source()
}

/// The unix `sys` module's shipping code.
pub fn unix_sys_source() -> &'static str {
    static ONCE: OnceLock<String> = OnceLock::new();
    ONCE.get_or_init(|| strip_comments(include_str!("sys_unix.rs")))
}

/// The windows `sys` module's shipping code.
pub fn windows_sys_source() -> &'static str {
    static ONCE: OnceLock<String> = OnceLock::new();
    ONCE.get_or_init(|| strip_comments(include_str!("sys_windows.rs")))
}
