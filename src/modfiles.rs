//! The mod on disk: atrium carries its plugin of function hooks (`mod/` in the
//! repo, see [`crate::modstate`] for what it says) inside its own binary and
//! writes it out on `atrium mod install`, so a claude pane can load it with no
//! checkout and no marketplace. A claude pane then gets
//! `CLAUDE_CODE_PLUGIN_DIRS` naming that folder (joined onto whatever the
//! variable already held) and `ATRIUM_BIN` naming this binary, so the mod
//! calls the atrium that hosts it even when none is on `PATH`.
//!
//! The folder is atrium's own, regenerated from the binary: `install` writes
//! what differs and leaves what matches, so an upgrade of atrium is an
//! upgrade of the mod, and a hand edit is restored. Nothing is written
//! silently: only `atrium mod install` writes, as `config init` does for the
//! config.
//!
//! Where it lives: `ATRIUM_MOD` (the folder's full path) when set, else the
//! config's `mod.path`, else `mod/` under the platform config directory.
//! `mod: false` in the config, or in a fleet, injects it into no pane.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Every file of the mod: its path under the install folder and the content
/// built into this binary.
pub const FILES: &[(&str, &str)] = &[
    (
        ".claude-plugin/plugin.json",
        include_str!("../mod/.claude-plugin/plugin.json"),
    ),
    ("hooks/hooks.json", include_str!("../mod/hooks/hooks.json")),
    (
        "hooks/register.ts",
        include_str!("../mod/hooks/register.ts"),
    ),
    ("hooks/status.ts", include_str!("../mod/hooks/status.ts")),
    ("hooks/ctl.ts", include_str!("../mod/hooks/ctl.ts")),
    ("hooks/control.ts", include_str!("../mod/hooks/control.ts")),
    ("hooks/role.ts", include_str!("../mod/hooks/role.ts")),
    ("hooks/guard.ts", include_str!("../mod/hooks/guard.ts")),
    ("hooks/agents.ts", include_str!("../mod/hooks/agents.ts")),
    ("hooks/view.ts", include_str!("../mod/hooks/view.ts")),
    ("hooks/caption.ts", include_str!("../mod/hooks/caption.ts")),
    ("types/index.d.ts", include_str!("../mod/types/index.d.ts")),
    ("README.md", include_str!("../mod/README.md")),
];

/// The manifest's path under the install folder; its presence is the install.
pub const MANIFEST: &str = ".claude-plugin/plugin.json";

/// Environment knob: the install folder's full path, when it is kept elsewhere.
pub const ENV_MOD: &str = "ATRIUM_MOD";

/// What Claude Code reads for extra plugin folders (the platform's path-list
/// separator between them).
pub const ENV_PLUGIN_DIRS: &str = "CLAUDE_CODE_PLUGIN_DIRS";

/// What the mod runs for `atrium ctl …`: this binary, so a pane needs no
/// `atrium` on its `PATH`.
pub const ENV_BIN: &str = "ATRIUM_BIN";

#[cfg(windows)]
const PATH_LIST_SEP: &str = ";";
#[cfg(not(windows))]
const PATH_LIST_SEP: &str = ":";

/// The version of the mod built into this binary (the manifest's `version`).
pub fn embedded_version() -> String {
    FILES
        .iter()
        .find(|(p, _)| *p == MANIFEST)
        .and_then(|(_, text)| version_of(text))
        .unwrap_or_default()
}

/// The `version` of a plugin manifest's text.
fn version_of(manifest: &str) -> Option<String> {
    json::parse(manifest)
        .ok()?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

/// `mod/` under the platform config directory ([`crate::fleet::global_dir`]).
pub fn default_dir() -> Option<PathBuf> {
    crate::fleet::global_dir().map(|d| d.join("mod"))
}

/// Where the mod is kept: [`ENV_MOD`], else the config's `mod.path`, else
/// [`default_dir`]. `None` only where there is no platform config directory
/// and nothing names a folder.
pub fn configured_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(ENV_MOD).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = crate::config::get().mod_path.clone() {
        return Some(p);
    }
    default_dir()
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub dir: PathBuf,
    /// Files written because they were absent or differed.
    pub written: Vec<&'static str>,
    /// Files left alone because they already matched.
    pub unchanged: Vec<&'static str>,
}

/// Write the mod under `dir`, creating it. Idempotent: a file that already
/// holds the embedded text is left untouched; one that differs, or is missing,
/// is written. The folder is regenerated from the binary, never merged.
pub fn install_at(dir: &Path) -> Result<Install, String> {
    let mut done = Install {
        dir: dir.to_path_buf(),
        written: Vec::new(),
        unchanged: Vec::new(),
    };
    for (rel, text) in FILES {
        let path = dir.join(rel);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(*text) {
            done.unchanged.push(rel);
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        done.written.push(rel);
    }
    Ok(done)
}

/// What is on disk at the configured folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The folder, when one is named or there is a platform default.
    pub dir: Option<PathBuf>,
    /// The installed manifest's version, when the manifest is there.
    pub installed: Option<String>,
    /// Every file matches the binary's copy.
    pub current: bool,
}

/// [`Status`] of `dir`.
pub fn status_at(dir: &Path) -> Status {
    let installed = std::fs::read_to_string(dir.join(MANIFEST))
        .ok()
        .and_then(|t| version_of(&t));
    let current = installed.is_some()
        && FILES.iter().all(|(rel, text)| {
            std::fs::read_to_string(dir.join(rel)).ok().as_deref() == Some(*text)
        });
    Status {
        dir: Some(dir.to_path_buf()),
        installed,
        current,
    }
}

/// [`Status`] of the configured folder.
pub fn status() -> Status {
    match configured_dir() {
        Some(dir) => status_at(&dir),
        None => Status {
            dir: None,
            installed: None,
            current: false,
        },
    }
}

/// The configured folder when the mod is installed there (its manifest and
/// hooks file exist), else `None`: the folder a claude pane is told to load.
pub fn installed_dir() -> Option<PathBuf> {
    let dir = configured_dir()?;
    (dir.join(MANIFEST).is_file() && dir.join("hooks/hooks.json").is_file()).then_some(dir)
}

static SESSION_ENABLED: OnceLock<bool> = OnceLock::new();

/// Record whether this session injects the mod at all (a fleet's `mod: false`
/// turns it off). First call wins, like the fleet's deny rules.
pub fn set_session_enabled(on: bool) {
    let _ = SESSION_ENABLED.set(on);
}

/// Does this session inject the mod into claude panes? The fleet's say if it
/// spoke, else the config's `mod` key, else yes.
pub fn session_enabled() -> bool {
    match SESSION_ENABLED.get() {
        Some(on) => *on,
        None => !crate::config::get().mod_off,
    }
}

/// The environment a pane gets for the mod: nothing for a non-claude pane or
/// when the mod is not installed; otherwise [`ENV_PLUGIN_DIRS`] holding
/// `existing` (what the variable already held, kept first) joined with the
/// install folder, and [`ENV_BIN`] naming `exe`. Pure, so the join is tested
/// on every platform.
pub fn pane_env(
    is_claude: bool,
    installed: Option<&Path>,
    existing: Option<&str>,
    exe: Option<&Path>,
) -> Vec<(String, String)> {
    let Some(dir) = installed.filter(|_| is_claude) else {
        return Vec::new();
    };
    let dir = dir.display().to_string();
    let dirs = match existing.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) if e.split(PATH_LIST_SEP).any(|d| d == dir) => e.to_string(),
        Some(e) => format!("{e}{PATH_LIST_SEP}{dir}"),
        None => dir,
    };
    let mut env = vec![(ENV_PLUGIN_DIRS.to_string(), dirs)];
    if let Some(exe) = exe {
        env.push((ENV_BIN.to_string(), exe.display().to_string()));
    }
    env
}

/// One line for the operator (`atrium mod status`, the fleet preflight): what
/// is installed and whether panes will load it. The second value says the
/// line is a warning: the session would inject the mod and none is installed.
pub fn describe(s: &Status, enabled: bool) -> (String, bool) {
    let built = embedded_version();
    let dir = s
        .dir
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "(no platform config directory)".to_string());
    match (&s.installed, enabled) {
        (_, false) => (
            "mod: off for this session (claude panes report status by inference)".to_string(),
            false,
        ),
        (Some(v), true) if s.current => (format!("mod: installed {v} at {dir}"), false),
        (Some(v), true) => (
            format!(
                "mod: installed {v} at {dir} differs from the {built} built into atrium; \
                 run `atrium mod install` to refresh it"
            ),
            true,
        ),
        (None, true) => (
            format!(
                "mod: not installed at {dir}; run `atrium mod install` so claude panes \
                 report status for certain (by inference until then)"
            ),
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atrium-modfiles-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_embedded_mod_is_whole_and_its_versions_agree() {
        assert!(FILES.iter().all(|(_, t)| !t.trim().is_empty()));
        let v = embedded_version();
        assert!(!v.is_empty(), "the manifest names a version");
        let status_ts = FILES
            .iter()
            .find(|(p, _)| *p == "hooks/status.ts")
            .map(|(_, t)| *t)
            .expect("status.ts is embedded");
        assert!(
            status_ts.contains(&format!("MOD_VERSION = '{v}'")),
            "hooks/status.ts MOD_VERSION must match plugin.json's {v}"
        );
        assert_eq!(version_of(r#"{"name":"x"}"#), None);
        assert_eq!(version_of("not json"), None);
    }

    #[test]
    fn install_writes_once_then_leaves_matching_files_and_restores_edits() {
        let dir = temp("install");
        let first = install_at(&dir).expect("install");
        assert_eq!(first.written.len(), FILES.len());
        assert!(first.unchanged.is_empty());
        assert!(dir.join(MANIFEST).is_file());
        let st = status_at(&dir);
        assert_eq!(st.installed.as_deref(), Some(embedded_version().as_str()));
        assert!(st.current);

        let again = install_at(&dir).expect("install");
        assert!(again.written.is_empty());
        assert_eq!(again.unchanged.len(), FILES.len());

        std::fs::write(dir.join("hooks/status.ts"), "edited").expect("edit");
        assert!(!status_at(&dir).current, "an edit is not current");
        let third = install_at(&dir).expect("install");
        assert_eq!(third.written, vec!["hooks/status.ts"]);
        assert!(status_at(&dir).current);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_of_an_empty_folder_is_not_installed() {
        let dir = temp("empty");
        let st = status_at(&dir);
        assert_eq!(st.installed, None);
        assert!(!st.current);
        assert_eq!(st.dir.as_deref(), Some(dir.as_path()));
    }

    #[test]
    fn pane_env_is_for_installed_claude_panes_only_and_keeps_existing_dirs() {
        let dir = Path::new("/cfg/mod");
        let exe = Path::new("/bin/atrium");
        assert!(pane_env(false, Some(dir), None, Some(exe)).is_empty());
        assert!(pane_env(true, None, None, Some(exe)).is_empty());
        let env = pane_env(true, Some(dir), None, Some(exe));
        assert_eq!(
            env,
            vec![
                (ENV_PLUGIN_DIRS.to_string(), "/cfg/mod".to_string()),
                (ENV_BIN.to_string(), "/bin/atrium".to_string()),
            ]
        );
        let env = pane_env(true, Some(dir), Some("/me/plugins"), None);
        assert_eq!(
            env,
            vec![(
                ENV_PLUGIN_DIRS.to_string(),
                format!("/me/plugins{PATH_LIST_SEP}/cfg/mod")
            )]
        );
        let already = format!("/me/plugins{PATH_LIST_SEP}/cfg/mod");
        let env = pane_env(true, Some(dir), Some(&already), None);
        assert_eq!(env[0].1, already, "never listed twice");
        let env = pane_env(true, Some(dir), Some("  "), None);
        assert_eq!(env[0].1, "/cfg/mod", "blank counts as unset");
    }

    #[test]
    fn describe_names_each_state_and_warns_only_when_panes_would_load_nothing() {
        let v = embedded_version();
        let none = Status {
            dir: Some(PathBuf::from("/d")),
            installed: None,
            current: false,
        };
        let (line, warn) = describe(&none, true);
        assert!(
            line.contains("not installed") && line.contains("/d"),
            "{line}"
        );
        assert!(warn);
        let (line, warn) = describe(&none, false);
        assert!(line.contains("off"), "{line}");
        assert!(!warn);
        let ok = Status {
            dir: Some(PathBuf::from("/d")),
            installed: Some(v.clone()),
            current: true,
        };
        let (line, warn) = describe(&ok, true);
        assert_eq!(line, format!("mod: installed {v} at /d"));
        assert!(!warn);
        let stale = Status {
            installed: Some("0.0.1".to_string()),
            current: false,
            ..ok
        };
        let (line, warn) = describe(&stale, true);
        assert!(line.contains("0.0.1") && line.contains("differs"), "{line}");
        assert!(warn);
        let nowhere = Status {
            dir: None,
            installed: None,
            current: false,
        };
        assert!(describe(&nowhere, true)
            .0
            .contains("no platform config directory"));
    }
}
