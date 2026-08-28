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

/// Batch scripts cannot be a process image; they need `cmd /C`.
pub fn needs_shell(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        .unwrap_or(false)
}
