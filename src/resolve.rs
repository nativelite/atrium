//! Windows command resolution: `CreateProcessW` finds `foo.exe` on PATH
//! but silently cannot launch `foo.cmd` / `foo.bat` — and npm-installed
//! CLIs (Claude Code included) are exactly such shims. Resolve the command
//! the way the shell would (PATH x PATHEXT) and report when a shell host
//! is required. Pure over injected search dirs/extensions, so it is
//! testable anywhere.

use std::path::{Path, PathBuf};

/// Find `cmd` in `dirs` trying `exts` (e.g. `.EXE`, `.CMD`) the way
/// cmd.exe would. A command containing a path separator is returned as
/// given (the caller meant that file). `None` = not found; let the
/// spawn produce its own error.
pub fn resolve(cmd: &str, dirs: &[PathBuf], exts: &[String]) -> Option<PathBuf> {
    if cmd.contains('/') || cmd.contains('\\') {
        return Some(PathBuf::from(cmd));
    }
    let has_ext = Path::new(cmd).extension().is_some();
    for dir in dirs {
        if has_ext {
            let exact = dir.join(cmd);
            if exact.is_file() {
                return Some(exact);
            }
        }
        for ext in exts {
            let cand = dir.join(format!("{cmd}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Batch scripts cannot be a process image; `pty` runs them under `cmd.exe`
/// (with cmd-safe argument encoding). Delegates to [`pty::cmdline::is_batch`] so
/// atrium and the spawn agree on what a batch file is.
pub fn needs_shell(path: &Path) -> bool {
    path.to_str().is_some_and(pty::cmdline::is_batch)
}
