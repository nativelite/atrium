//! Windows command resolution: `CreateProcessW` finds `foo.exe` on PATH
//! but silently cannot launch `foo.cmd` / `foo.bat` — and npm-installed
//! CLIs (Claude Code included) are exactly such shims. Resolve the command
//! the way the shell would (PATH x PATHEXT) and report when a shell host
//! is required. Pure over injected search dirs/extensions, so it is
//! testable anywhere.
//!
//! Also home to [`normalize_path`], the one lexical path normalizer that
//! worktree planning and folder-trust keys share.

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

/// Lexically collapse `.` and `..` components in a path without touching the
/// filesystem (`std::fs::canonicalize` is not used — on Windows it prepends
/// `\\?\`, does disk IO, and breaks the pure-plan seam).
///
/// `Prefix` and `RootDir` components pass through unchanged. `CurDir` (`.`) is
/// dropped. `Normal` is pushed. `ParentDir` (`..`) pops the last component
/// unless that would escape above the root/prefix, in which case it is silently
/// clamped. A relative path (no root) is returned as-is; nothing to collapse
/// without an anchor.
///
/// The single copy: the worktree planner (containment) and `trust` (the claude
/// project-map key) both call this. It used to be duplicated byte-for-byte in
/// `trust.rs`, so a fix to one would silently miss the other (r10 audit B6).
pub fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                let at_root = matches!(
                    out.components().last(),
                    Some(Component::RootDir) | Some(Component::Prefix(_)) | None
                );
                if !at_root {
                    out.pop();
                }
            }
            Component::Normal(_) => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize_path unit tests ---

    #[test]
    fn normalize_path_collapses_dotdot() {
        assert_eq!(
            normalize_path(Path::new("/home/dev/repo/../wt")),
            Path::new("/home/dev/wt")
        );
        assert_eq!(normalize_path(Path::new("/a/b/../../c")), Path::new("/c"));
    }

    #[test]
    fn normalize_path_drops_curdirs() {
        assert_eq!(normalize_path(Path::new("/a/./b/./c")), Path::new("/a/b/c"));
    }

    #[test]
    fn normalize_path_clamps_dotdot_at_root() {
        // `..` above the root is silently clamped — no panic, no escape.
        assert_eq!(normalize_path(Path::new("/../..")), Path::new("/"));
        assert_eq!(normalize_path(Path::new("/a/../..")), Path::new("/"));
    }

    #[test]
    fn normalize_path_leaves_clean_absolute_path_unchanged() {
        assert_eq!(
            normalize_path(Path::new("/home/dev/repo")),
            Path::new("/home/dev/repo")
        );
    }
}
