//! Resolving a command to what actually runs (Windows shims).

// --- command resolution (the npm .cmd shim trap) -----------------------------

// Windows-only: `resolve` mimics cmd.exe's PATHEXT lookup, whose case-insensitive
// extension match (`.EXE` finds `tool.exe`) is a property of the Windows/macOS
// case-insensitive filesystem, not of `resolve` itself. On a case-sensitive
// volume (Linux ext4, or a case-sensitive APFS/macOS volume) this fixture would
// not resolve `tool` against `.EXE` and the test would panic. The resolver is
// only *used* on Windows (`effective_command` calls it under `#[cfg(windows)]`),
// so gate the test there rather than assume a case-insensitive FS off-platform.
#[cfg(windows)]
#[test]
fn resolver_finds_shims_and_flags_shell_hosting() {
    use atrium::resolve::{needs_shell, resolve};
    let td = std::env::temp_dir().join(format!("atrium-resolve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    std::fs::write(td.join("claude.CMD"), "@echo shim").unwrap();
    std::fs::write(td.join("tool.exe"), "MZ").unwrap();
    let dirs = vec![td.clone()];
    let exts = vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()];

    let shim = resolve("claude", &dirs, &exts).expect("shim found");
    assert!(needs_shell(&shim), "{shim:?}");
    let exe = resolve("tool", &dirs, &exts).expect("exe found");
    assert!(!needs_shell(&exe), "{exe:?}");
    assert_eq!(resolve("missing", &dirs, &exts), None);
    // explicit paths pass through untouched
    assert_eq!(
        resolve("dir\\thing", &dirs, &exts),
        Some(std::path::PathBuf::from("dir\\thing"))
    );
    let _ = std::fs::remove_dir_all(&td);
}
