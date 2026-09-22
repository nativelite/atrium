//! Filesystem facts the plan is built on: where a path really lands through
//! symlinks, the home directory, and which credential stores exist.

use super::show_path;
use std::path::{Path, PathBuf};

/// Strip Windows' verbatim `\\?\` prefix that `canonicalize` adds.
///
/// The resolved path is not just printed — it is what the child is spawned with,
/// and `\\?\C:\proj` is neither what an agent CLI expects on its `--add-dir` nor
/// something an operator recognises as their own directory.
fn presentable(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest.to_string()),
        None => p,
    }
}

/// Resolve a path **through symlinks** to where it really lands, or `None` when
/// atrium cannot tell.
///
/// Never string surgery. A previous attempt at this defect banned `".."`
/// lexically and its own test showed a symlink walking straight out of the tree
/// and reporting as inside — a lexical normaliser also gets `link/..` backwards,
/// popping the link's parent instead of the target's.
///
/// A path that does not exist yet is still *placed*: the deepest existing
/// ancestor is canonicalised (so symlinks on the part that does exist are
/// followed) and the missing tail re-applied. A missing tail containing `..`
/// returns `None` — where it lands depends on a directory that is not there, and
/// "cannot tell" must never render as "inside", the fail-open shape this
/// codebase has shipped five times.
pub fn real_path(p: &Path) -> Option<PathBuf> {
    // A `..` only has a well-defined target if the directory it pops from exists.
    // unix's `canonicalize` enforces that (it fails on a missing component, so the
    // walk below returns None); **Windows' `canonicalize` collapses `..` lexically
    // before touching the filesystem**, so `<missing>/..` resolves and would report
    // "inside" — the fail-open shape this codebase has shipped five times. Guard it
    // on both platforms with a filesystem check (not string surgery): if any `..`
    // pops a path that does not exist, where it lands cannot be known → None.
    {
        let mut prefix = PathBuf::new();
        for comp in p.components() {
            if matches!(comp, std::path::Component::ParentDir) && !prefix.exists() {
                return None;
            }
            prefix.push(comp);
        }
    }
    if let Ok(real) = std::fs::canonicalize(p) {
        return Some(presentable(real));
    }
    // The path itself is a symlink atrium could not follow (a dangling target, an
    // unreadable ancestor). Falling through to the walk below would place it at
    // the LINK's own location and report "inside" for a pointer to somewhere
    // unknown - and the moment the target is created, the grant is wherever that
    // is. "Cannot tell" is the honest answer.
    if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        // `file_name` is None for a path ending in `..` (and for a bare root):
        // the tail is not a plain name, so it cannot be re-applied lexically.
        let name = cur.file_name()?.to_os_string();
        let parent = cur.parent()?.to_path_buf();
        tail.push(name);
        if let Ok(real) = std::fs::canonicalize(&parent) {
            let mut out = presentable(real);
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return Some(out);
        }
        cur = parent;
    }
}

/// This user's home directory (`$HOME`, `%USERPROFILE%`), if the environment
/// names one.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Well-known credential directories under the home directory, as
/// `(relative path, what a human would call it)`.
const HOME_STORES: &[(&str, &str)] = &[
    (".ssh", "SSH private keys"),
    (".aws", "AWS credentials"),
    (".gnupg", "GPG private keys"),
    (".kube", "Kubernetes cluster credentials"),
    (".config/gcloud", "Google Cloud credentials"),
    (".claude", "Claude Code's own credentials and settings"),
];

/// The credential stores that actually exist on this machine, used to make a
/// disclosure line louder — never to refuse.
///
/// Only directories that are really there are listed. A refusal (or a warning)
/// citing `~/.config/gcloud` on a machine with no gcloud installed is one the
/// operator can see is wrong, and a control the operator can see is wrong is one
/// they learn to skip. For the same reason nothing here blocks: an earlier
/// attempt refused `add_dirs: ["~/.claude/skills"]` — editing your own skills,
/// which is a normal atrium job — with no override anywhere.
#[derive(Debug, Clone, Default)]
pub struct Stores {
    roots: Vec<(PathBuf, &'static str)>,
}

impl Stores {
    /// The stores under `home`. Injected rather than read from the environment
    /// so this is testable against a fixture home.
    pub fn under(home: &Path) -> Stores {
        let mut roots = Vec::new();
        for (rel, what) in HOME_STORES {
            let p = home.join(rel);
            if p.is_dir() {
                if let Some(real) = real_path(&p) {
                    roots.push((real, *what));
                }
            }
        }
        Stores { roots }
    }

    /// The stores under this user's home directory (`$HOME`, `%USERPROFILE%`).
    pub fn live() -> Stores {
        match home_dir() {
            Some(h) => Stores::under(&h),
            None => Stores::default(),
        }
    }

    /// How this resolved path relates to a credential store, if at all.
    ///
    /// Bidirectional on purpose: `--add-dir ~` hands over `~/.ssh` exactly as
    /// surely as naming it, and the enclosing case is the one an operator is
    /// least likely to work out from the path alone.
    pub fn describe(&self, real: Option<&Path>) -> Option<String> {
        let real = real?;
        let mut encloses: Vec<&(PathBuf, &'static str)> = Vec::new();
        for entry in &self.roots {
            if real.starts_with(&entry.0) {
                return Some(format!("holds {}", entry.1));
            }
            if entry.0.starts_with(real) {
                encloses.push(entry);
            }
        }
        let first = encloses.first()?;
        let more = if encloses.len() > 1 {
            format!(" (and {} other store(s))", encloses.len() - 1)
        } else {
            String::new()
        };
        Some(format!(
            "encloses {}, which holds {}{more}",
            show_path(&first.0),
            first.1
        ))
    }
}
