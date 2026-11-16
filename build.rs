//! Build facts for `atrium --version --json`: the target triple rustc was
//! asked for, and the git commit of the tree when there is one.
//!
//! Standard library only. Nothing here can fail the build: a missing `git`, a
//! tree that is not a checkout (a crates.io install) or one that belongs to
//! some other repository simply leaves the commit unset, and the binary says
//! `"commit":null`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=ATRIUM_BUILD_COMMIT");
    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=ATRIUM_BUILD_TARGET={target}");
    }
    // A packager's own value wins and is passed through by rustc as it is.
    if std::env::var_os("ATRIUM_BUILD_COMMIT").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
        return;
    };
    // Only a checkout of atrium itself: a `.git` in this directory, not one
    // found by walking up into whatever repository the sources were unpacked
    // inside.
    let dot_git = dir.join(".git");
    if !dot_git.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", dot_git.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", dot_git.join("refs").display());
    if let Some(commit) = head_commit(&dir) {
        println!("cargo:rustc-env=ATRIUM_BUILD_COMMIT={commit}");
    }
}

/// `git rev-parse HEAD` in `dir`, when it answers with a full hex object id.
/// The commit the tree was at: local changes on top of it are not reported.
fn head_commit(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let id = text.trim();
    let hex = id.len() >= 40 && id.bytes().all(|b| b.is_ascii_hexdigit());
    hex.then(|| id[..12].to_string())
}
