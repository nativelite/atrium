//! The user-global config file: the values an operator wants in force on this
//! machine for every project and every terminal, without exporting the same
//! `ATRIUM_*` variables into each shell.
//!
//! `%APPDATA%\atrium\config.json` on Windows, `~/.config/atrium/config.json`
//! elsewhere — beside the global `fleet.json`. Absent is fine (every value has
//! today's default); malformed is an error at launch, never a silent fallback,
//! for the same reason a fleet's `deny` is strict.
//!
//! ```json
//! {
//!   "claude_aliases": { "claude2": { "config_dir": "~/.claude-2" } },
//!   "deny": ["Bash(git push --force*)"],
//!   "ctl_allow": [],
//!   "trust_allow": [],
//!   "build_jobs": null,
//!   "memory_mb": null,
//!   "fleet_defaults": { "trust": "automode", "identity": "work", "allow_ctl": true }
//! }
//! ```
//!
//! Precedence, lowest to highest: this file → the project fleet's key → the
//! `ATRIUM_*` variable → a command-line flag. List keys (`deny`, `ctl_allow`,
//! `trust_allow`, `claude_aliases`) **merge** with what the fleet and the
//! environment add; scalars (`build_jobs`, `memory_mb`) are **overridden**.
//! That is the rule the fleet and the environment already follow between
//! themselves (`ATRIUM_MEMORY_MB` beats a fleet's `memory_mb`), extended one
//! level down.
//!
//! The file holds no secrets (identity has its own store) and no rosters
//! (those are `fleet.json`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A command that is claude under another name: a second account's shim
/// (`claude2`), a wrapper, a renamed install. With a `config_dir`, atrium also
/// knows where that claude keeps its transcripts (`<dir>/projects`, for agent
/// status and recovery) and its folder-trust map (`<dir>/.claude.json`).
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeAlias {
    /// The command as written in a fleet or on the command line; matched by
    /// stem, like every other command ([`crate::bind::command_stem`]).
    pub name: String,
    /// The `CLAUDE_CONFIG_DIR` that command runs under, `~` expanded.
    pub config_dir: Option<PathBuf>,
}

/// Fleet-level keys a project's fleet may leave out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetDefaults {
    pub trust: Option<String>,
    pub identity: Option<String>,
    pub allow_ctl: Option<bool>,
    pub grid: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalConfig {
    pub claude_aliases: Vec<ClaudeAlias>,
    pub deny: Vec<String>,
    pub ctl_allow: Vec<String>,
    pub trust_allow: Vec<String>,
    pub build_jobs: Option<usize>,
    pub memory_mb: Option<u64>,
    pub fleet_defaults: FleetDefaults,
}

impl GlobalConfig {
    /// The alias names alone, for the stem rules.
    pub fn alias_names(&self) -> Vec<String> {
        self.claude_aliases.iter().map(|a| a.name.clone()).collect()
    }

    /// The config dir of the alias whose stem is `stem`, if it has one.
    pub fn alias_config_dir(&self, stem: &str) -> Option<&Path> {
        self.claude_aliases
            .iter()
            .find(|a| crate::bind::command_stem(&a.name) == stem)
            .and_then(|a| a.config_dir.as_deref())
    }

    /// The transcript roots of every alias with a config dir (`<dir>/projects`),
    /// for the agent-status worlds.
    pub fn alias_transcript_roots(&self) -> Vec<PathBuf> {
        self.claude_aliases
            .iter()
            .filter_map(|a| a.config_dir.as_ref().map(|d| d.join("projects")))
            .collect()
    }

    /// A fleet with this config's defaults filled into the keys it left out.
    /// Only absent keys are touched: the fleet's own `trust`, `identity`,
    /// `allow_ctl`, `grid`, `build_jobs` and `memory_mb` always win, and the
    /// environment still wins over both where it applies. Lists (`deny`,
    /// `claude_aliases`) are not filled here; they merge at their session seams.
    pub fn apply_to_fleet(&self, mut fleet: crate::fleet::Fleet) -> crate::fleet::Fleet {
        let d = &self.fleet_defaults;
        fleet.trust = fleet.trust.or_else(|| d.trust.clone());
        fleet.identity = fleet.identity.or_else(|| d.identity.clone());
        fleet.allow_ctl = fleet.allow_ctl.or(d.allow_ctl);
        fleet.grid = fleet.grid.or_else(|| d.grid.clone());
        fleet.build_jobs = fleet.build_jobs.or(self.build_jobs);
        fleet.memory_mb = fleet.memory_mb.or(self.memory_mb);
        fleet
    }
}

/// Environment knob: the file's path, in full, when someone keeps it somewhere
/// else. Unset ⇒ `config.json` under [`crate::fleet::global_dir`].
pub const ENV_CONFIG: &str = "ATRIUM_CONFIG";

/// The file's path, however it was resolved ([`resolve`]).
pub fn global_path() -> Option<PathBuf> {
    resolve().path
}

/// The platform default: `config.json` under [`crate::fleet::global_dir`].
pub fn default_path() -> Option<PathBuf> {
    crate::fleet::global_dir().map(|d| d.join("config.json"))
}

/// The pointer atrium keeps at the platform place, naming where the config
/// actually lives: one line, the full path. Written by first-run setup and
/// `atrium config init`, so a config kept elsewhere is found on every later
/// launch without a variable in every shell. Its presence is also the "asked
/// once" marker: with it there, first-run setup never asks again. atrium's
/// file, not the operator's — though it is plain text if they need it.
pub fn pointer_path() -> Option<PathBuf> {
    crate::fleet::global_dir().map(|d| d.join("config.path"))
}

/// How the config's path was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// [`ENV_CONFIG`] named it.
    Env,
    /// The pointer file named it.
    Pointer,
    /// The platform default, nothing else said.
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: Option<PathBuf>,
    pub source: Source,
}

/// Where the config is read from: [`ENV_CONFIG`] first (explicit, for a script
/// or a test), then the pointer, then the platform default.
pub fn resolve() -> Resolved {
    let pointer = pointer_path().and_then(|p| std::fs::read_to_string(p).ok());
    resolve_from(
        std::env::var_os(ENV_CONFIG).as_deref(),
        pointer.as_deref(),
        default_path(),
    )
}

/// [`resolve`] over explicit inputs. An empty variable or a blank pointer
/// counts as unset.
pub fn resolve_from(
    env: Option<&std::ffi::OsStr>,
    pointer_text: Option<&str>,
    default: Option<PathBuf>,
) -> Resolved {
    if let Some(p) = env.filter(|v| !v.is_empty()) {
        return Resolved {
            path: Some(PathBuf::from(p)),
            source: Source::Env,
        };
    }
    if let Some(line) = pointer_text.and_then(|t| t.lines().next()).map(str::trim) {
        if !line.is_empty() {
            return Resolved {
                path: Some(PathBuf::from(line)),
                source: Source::Pointer,
            };
        }
    }
    Resolved {
        path: default,
        source: Source::Default,
    }
}

/// True when nothing has decided where the config lives and no default file
/// exists: the state first-run setup asks in.
pub fn needs_first_run() -> bool {
    let r = resolve();
    r.source == Source::Default && !r.path.as_deref().is_some_and(Path::is_file)
}

/// A starter file: every key present with its default, so it parses to the
/// defaults and reads as documentation of what can be set.
pub const STARTER: &str = r#"{
  "claude_aliases": {},
  "deny": [],
  "ctl_allow": [],
  "trust_allow": [],
  "build_jobs": null,
  "memory_mb": null,
  "fleet_defaults": {
    "trust": null,
    "identity": null,
    "allow_ctl": null,
    "grid": null
  }
}
"#;

/// What `init` did.
#[derive(Debug, PartialEq, Eq)]
pub struct Init {
    pub config: PathBuf,
    /// The pointer written, when the platform place exists to hold one.
    pub pointer: Option<PathBuf>,
}

/// Write the starter config at `path` and point the platform place at it.
/// Refuses to overwrite: an existing file is the operator's. The pointer is
/// written even for the default path, so first-run setup asks exactly once.
pub fn init_at(path: &Path) -> Result<Init, String> {
    init_with_pointer(path, pointer_path().as_deref())
}

/// [`init_at`] with the pointer's location injected.
pub fn init_with_pointer(path: &Path, pointer: Option<&Path>) -> Result<Init, String> {
    if path.exists() {
        return Err(format!(
            "{} already exists; edit it, or remove it to start over",
            path.display()
        ));
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, STARTER).map_err(|e| format!("write {}: {e}", path.display()))?;
    let mut written = None;
    if let Some(ptr) = pointer {
        if let Some(dir) = ptr.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        std::fs::write(ptr, format!("{}\n", path.display()))
            .map_err(|e| format!("write {}: {e}", ptr.display()))?;
        written = Some(ptr.to_path_buf());
    }
    Ok(Init {
        config: path.to_path_buf(),
        pointer: written,
    })
}

/// The path a first-run answer means: Enter (or blanks) is the default; anything
/// else is a path, `~` expanded, surrounding quotes dropped.
pub fn answer_to_path(line: &str, default: &Path, home: Option<&Path>) -> PathBuf {
    let t = line.trim().trim_matches('"').trim_matches('\'').trim();
    if t.is_empty() {
        default.to_path_buf()
    } else {
        expand_home(t, home)
    }
}

/// Ask where the config should live and write it there. Only when a human is
/// present (stdin is a terminal and `ATRIUM_YES` is unset) and nothing has
/// decided yet ([`needs_first_run`]); a script or a test gets no question and
/// no file. Errors are reported, not fatal: the launch goes on with defaults.
pub fn first_run_setup() {
    use std::io::IsTerminal;
    if !needs_first_run()
        || std::env::var_os("ATRIUM_YES").is_some()
        || !std::io::stdin().is_terminal()
    {
        return;
    }
    let Some(default) = default_path() else {
        return;
    };
    eprintln!("atrium: first run — a user-global config.json holds what you want in force for every project.");
    eprint!(
        "Global config file location (Enter for default: {}): ",
        default.display()
    );
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    if !matches!(std::io::stdin().read_line(&mut line), Ok(n) if n > 0) {
        return; // end of input: nobody answered, ask again next time
    }
    let path = answer_to_path(&line, &default, home().as_deref());
    match init_at(&path) {
        Ok(done) => {
            eprintln!("atrium: wrote {}", done.config.display());
            if let Some(p) = done.pointer {
                eprintln!("atrium: remembered it in {}", p.display());
            }
        }
        Err(e) => eprintln!("atrium: config not written: {e}"),
    }
}

/// Parse the file's text. Every key is optional; a key of the wrong shape is an
/// error naming it, so a typo never quietly becomes "no rule".
pub fn parse(text: &str) -> Result<GlobalConfig, String> {
    parse_with_home(text, home().as_deref())
}

/// [`parse`] with the home directory injected, for `~` expansion.
pub fn parse_with_home(text: &str, home: Option<&Path>) -> Result<GlobalConfig, String> {
    let root = json::parse(text).map_err(|e| format!("config.json: {e}"))?;
    let obj = root
        .as_object()
        .ok_or_else(|| "config.json: the top level must be an object".to_string())?;
    let get = |key: &str| obj.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v);
    let mut cfg = GlobalConfig {
        claude_aliases: aliases(get("claude_aliases"), home)?,
        deny: str_list(get("deny"), "deny")?,
        ctl_allow: str_list(get("ctl_allow"), "ctl_allow")?,
        trust_allow: str_list(get("trust_allow"), "trust_allow")?,
        build_jobs: None,
        memory_mb: None,
        fleet_defaults: FleetDefaults::default(),
    };
    cfg.build_jobs = whole(get("build_jobs"), "build_jobs")?.map(|n| n as usize);
    cfg.memory_mb = whole(get("memory_mb"), "memory_mb")?;
    if let Some(v) = get("fleet_defaults").filter(|v| !matches!(v, json::Value::Null)) {
        let d = v
            .as_object()
            .ok_or_else(|| "config.json: \"fleet_defaults\" must be an object".to_string())?;
        let dget = |key: &str| d.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v);
        cfg.fleet_defaults = FleetDefaults {
            trust: opt_str(dget("trust"), "fleet_defaults.trust")?,
            identity: opt_str(dget("identity"), "fleet_defaults.identity")?,
            allow_ctl: match dget("allow_ctl") {
                None | Some(json::Value::Null) => None,
                Some(json::Value::Bool(b)) => Some(*b),
                Some(_) => {
                    return Err(
                        "config.json: \"fleet_defaults.allow_ctl\" must be true or false"
                            .to_string(),
                    )
                }
            },
            grid: opt_str(dget("grid"), "fleet_defaults.grid")?,
        };
    }
    Ok(cfg)
}

/// `claude_aliases` as a list of names, or a map of name → `{ "config_dir" }`.
fn aliases(v: Option<&json::Value>, home: Option<&Path>) -> Result<Vec<ClaudeAlias>, String> {
    let err = || {
        "config.json: \"claude_aliases\" must be an array of strings or an object of \
         name → { \"config_dir\": … }"
            .to_string()
    };
    match v {
        None | Some(json::Value::Null) => Ok(Vec::new()),
        Some(json::Value::Array(arr)) => arr
            .iter()
            .map(|e| {
                e.as_str()
                    .map(|s| ClaudeAlias {
                        name: s.to_string(),
                        config_dir: None,
                    })
                    .ok_or_else(err)
            })
            .collect(),
        Some(json::Value::Object(entries)) => entries
            .iter()
            .map(|(name, spec)| {
                let config_dir = match spec {
                    json::Value::Null => None,
                    json::Value::Object(_) => match spec.get("config_dir") {
                        None | Some(json::Value::Null) => None,
                        Some(json::Value::String(s)) if !s.is_empty() => Some(expand_home(s, home)),
                        Some(_) => {
                            return Err(format!(
                                "config.json: \"claude_aliases.{name}.config_dir\" must be a \
                                 non-empty string"
                            ))
                        }
                    },
                    _ => return Err(err()),
                };
                Ok(ClaudeAlias {
                    name: name.clone(),
                    config_dir,
                })
            })
            .collect(),
        Some(_) => Err(err()),
    }
}

fn str_list(v: Option<&json::Value>, key: &str) -> Result<Vec<String>, String> {
    let err = || format!("config.json: {key:?} must be an array of strings");
    match v {
        None | Some(json::Value::Null) => Ok(Vec::new()),
        Some(json::Value::Array(arr)) => arr
            .iter()
            .map(|e| e.as_str().map(str::to_string).ok_or_else(err))
            .collect(),
        Some(_) => Err(err()),
    }
}

fn opt_str(v: Option<&json::Value>, key: &str) -> Result<Option<String>, String> {
    match v {
        None | Some(json::Value::Null) => Ok(None),
        Some(json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("config.json: {key:?} must be a string")),
    }
}

/// A whole number ≥ 0, or null/absent.
fn whole(v: Option<&json::Value>, key: &str) -> Result<Option<u64>, String> {
    match v {
        None | Some(json::Value::Null) => Ok(None),
        Some(json::Value::Number(json::Number::Int(n))) if *n >= 0 => Ok(Some(*n as u64)),
        Some(_) => Err(format!(
            "config.json: {key:?} must be a whole number or null"
        )),
    }
}

/// `~`, `~/x` and `~\x` against `home`; anything else as written. With no home
/// known the path is kept as written, so the error surfaces where it is used.
fn expand_home(s: &str, home: Option<&Path>) -> PathBuf {
    match (s.strip_prefix('~'), home) {
        (Some(rest), Some(h)) if rest.is_empty() => h.to_path_buf(),
        (Some(rest), Some(h)) if rest.starts_with('/') || rest.starts_with('\\') => {
            h.join(&rest[1..])
        }
        _ => PathBuf::from(s),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Read the file if it exists. `Ok(None)` when there is none at the default
/// place; a path named by [`ENV_CONFIG`] must exist (you asked for a file that
/// is not there).
pub fn load() -> Result<Option<GlobalConfig>, String> {
    let r = resolve();
    let Some(path) = r.path else {
        return Ok(None);
    };
    read(&path, r.source)
}

/// [`load`] over an explicit path. A missing file is no config at the platform
/// default, and an error naming what pointed there otherwise.
pub fn read(path: &Path, source: Source) -> Result<Option<GlobalConfig>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match source {
            Source::Default => Ok(None),
            Source::Env => Err(format!(
                "{}: named by {ENV_CONFIG} but not there",
                path.display()
            )),
            Source::Pointer => Err(format!(
                "{}: named by {} but not there (run `atrium config init`, or fix the pointer)",
                path.display(),
                pointer_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "the config pointer".to_string())
            )),
        },
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The config in force for this process, installed once at launch.
static INSTALLED: OnceLock<GlobalConfig> = OnceLock::new();

/// Record the process's config. First call wins.
pub fn install(cfg: GlobalConfig) {
    let _ = INSTALLED.set(cfg);
}

/// Load the file and install it. A missing file installs the defaults.
pub fn install_from_disk() -> Result<(), String> {
    install(load()?.unwrap_or_default());
    Ok(())
}

/// The installed config, or the defaults when nothing was installed (tests, the
/// ctl client).
pub fn get() -> &'static GlobalConfig {
    static DEFAULT: OnceLock<GlobalConfig> = OnceLock::new();
    INSTALLED
        .get()
        .unwrap_or_else(|| DEFAULT.get_or_init(GlobalConfig::default))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Option<&'static Path> {
        Some(Path::new("/home/u"))
    }

    #[test]
    fn an_empty_or_absent_file_is_the_defaults() {
        assert_eq!(
            parse_with_home("{}", home()).unwrap(),
            GlobalConfig::default()
        );
        let nulls = r#"{"claude_aliases":null,"deny":null,"build_jobs":null,"memory_mb":null,"fleet_defaults":null}"#;
        assert_eq!(
            parse_with_home(nulls, home()).unwrap(),
            GlobalConfig::default()
        );
    }

    /// Aliases come as a plain list or as a map; a map entry may name the
    /// config dir that claude runs under, with `~` expanded.
    #[test]
    fn aliases_as_a_list_or_a_map_with_config_dirs() {
        let list = parse_with_home(r#"{"claude_aliases":["claude2","cc.cmd"]}"#, home()).unwrap();
        assert_eq!(list.alias_names(), vec!["claude2", "cc.cmd"]);
        assert_eq!(list.alias_config_dir("claude2"), None);

        let map = parse_with_home(
            r#"{"claude_aliases":{"claude2":{"config_dir":"~/.claude-2"},"cc.cmd":{},"plain":null}}"#,
            home(),
        )
        .unwrap();
        assert_eq!(map.alias_names(), vec!["claude2", "cc.cmd", "plain"]);
        assert_eq!(
            map.alias_config_dir("claude2"),
            Some(Path::new("/home/u").join(".claude-2").as_path())
        );
        // Looked up by stem, like a pane's command.
        assert_eq!(map.alias_config_dir("cc"), None);
        assert!(map.alias_names().iter().any(|n| n == "cc.cmd"));
    }

    #[test]
    fn home_expansion_covers_both_separators_and_keeps_other_paths() {
        let h = Path::new("/home/u");
        assert_eq!(expand_home("~", Some(h)), h);
        assert_eq!(expand_home("~/.claude-2", Some(h)), h.join(".claude-2"));
        assert_eq!(expand_home("~\\.claude-2", Some(h)), h.join(".claude-2"));
        assert_eq!(expand_home("/abs/dir", Some(h)), PathBuf::from("/abs/dir"));
        assert_eq!(expand_home("~user/x", Some(h)), PathBuf::from("~user/x"));
        assert_eq!(expand_home("~/x", None), PathBuf::from("~/x"));
    }

    #[test]
    fn lists_scalars_and_fleet_defaults_parse() {
        let cfg = parse_with_home(
            r#"{
              "deny": ["Bash(git push --force*)", "npm publish"],
              "ctl_allow": ["sh"], "trust_allow": ["make"],
              "build_jobs": 8, "memory_mb": 16384,
              "fleet_defaults": {"trust": "automode", "identity": "work", "allow_ctl": true, "grid": "2x2"}
            }"#,
            home(),
        )
        .unwrap();
        assert_eq!(cfg.deny, vec!["Bash(git push --force*)", "npm publish"]);
        assert_eq!(cfg.ctl_allow, vec!["sh"]);
        assert_eq!(cfg.trust_allow, vec!["make"]);
        assert_eq!(cfg.build_jobs, Some(8));
        assert_eq!(cfg.memory_mb, Some(16384));
        assert_eq!(
            cfg.fleet_defaults,
            FleetDefaults {
                trust: Some("automode".into()),
                identity: Some("work".into()),
                allow_ctl: Some(true),
                grid: Some("2x2".into()),
            }
        );
    }

    /// A wrong shape is an error that names the key — never a silent default.
    #[test]
    fn a_key_of_the_wrong_shape_is_named_in_the_error() {
        for (text, key) in [
            (r#"{"deny":"git push"}"#, "\"deny\""),
            (r#"{"deny":[1]}"#, "\"deny\""),
            (r#"{"ctl_allow":{}}"#, "\"ctl_allow\""),
            (r#"{"claude_aliases":"claude2"}"#, "\"claude_aliases\""),
            (
                r#"{"claude_aliases":{"x":{"config_dir":3}}}"#,
                "claude_aliases.x.config_dir",
            ),
            (r#"{"claude_aliases":{"x":"y"}}"#, "\"claude_aliases\""),
            (r#"{"build_jobs":-1}"#, "\"build_jobs\""),
            (r#"{"memory_mb":"lots"}"#, "\"memory_mb\""),
            (r#"{"fleet_defaults":[]}"#, "\"fleet_defaults\""),
            (r#"{"fleet_defaults":{"allow_ctl":"yes"}}"#, "allow_ctl"),
            (r#"{"fleet_defaults":{"trust":1}}"#, "fleet_defaults.trust"),
            (r#"[]"#, "top level"),
        ] {
            let err = parse_with_home(text, home()).unwrap_err();
            assert!(err.contains(key), "{text}: {err}");
        }
    }

    /// Defaults fill only what the fleet left out; its own keys are never
    /// overridden.
    #[test]
    fn fleet_defaults_fill_absent_keys_only() {
        let cfg = parse_with_home(
            r#"{"build_jobs": 4, "memory_mb": 1024,
                "fleet_defaults": {"trust": "automode", "identity": "work", "allow_ctl": true, "grid": "2x2"}}"#,
            home(),
        )
        .unwrap();
        let bare =
            crate::fleet::parse(r#"{"fleets":{"f":{"agents":[{"name":"a","cmd":["claude"]}]}}}"#)
                .unwrap();
        let f = cfg.apply_to_fleet(bare.get("f").unwrap().clone());
        assert_eq!(f.trust.as_deref(), Some("automode"));
        assert_eq!(f.identity.as_deref(), Some("work"));
        assert_eq!(f.allow_ctl, Some(true));
        assert_eq!(f.grid.as_deref(), Some("2x2"));
        assert_eq!(f.build_jobs, Some(4));
        assert_eq!(f.memory_mb, Some(1024));

        let own = crate::fleet::parse(
            r#"{"fleets":{"f":{"trust":"plan","identity":"me","allow_ctl":false,"grid":"1x1",
                "build_jobs":2,"memory_mb":512,"agents":[{"name":"a","cmd":["claude"]}]}}}"#,
        )
        .unwrap();
        let f = cfg.apply_to_fleet(own.get("f").unwrap().clone());
        assert_eq!(f.trust.as_deref(), Some("plan"));
        assert_eq!(f.identity.as_deref(), Some("me"));
        assert_eq!(f.allow_ctl, Some(false));
        assert_eq!(f.grid.as_deref(), Some("1x1"));
        assert_eq!(f.build_jobs, Some(2));
        assert_eq!(f.memory_mb, Some(512));
    }

    #[test]
    fn the_file_sits_beside_the_global_fleet_file_by_default() {
        if std::env::var_os(ENV_CONFIG).is_some()
            || std::env::var_os(crate::fleet::ENV_FLEET).is_some()
        {
            return; // the developer moved one; the default is not observable here
        }
        if let (Some(cfg), Some(fleet)) = (global_path(), crate::fleet::global_path()) {
            assert_eq!(cfg.parent(), fleet.parent());
            assert_eq!(cfg.file_name().unwrap(), "config.json");
        }
    }

    /// A missing file at the default place is no config; a missing file at a
    /// path someone named is an error that says who named it. A present file
    /// reads.
    #[test]
    fn a_named_but_missing_file_is_an_error_a_default_one_is_none() {
        let dir = std::env::temp_dir().join(format!("atrium-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nope.json");
        assert_eq!(read(&missing, Source::Default).unwrap(), None);
        let err = read(&missing, Source::Env).unwrap_err();
        assert!(
            err.contains(ENV_CONFIG) && err.contains("nope.json"),
            "{err}"
        );
        let err = read(&missing, Source::Pointer).unwrap_err();
        assert!(
            err.contains("config init") && err.contains("nope.json"),
            "{err}"
        );
        let present = dir.join("config.json");
        std::fs::write(&present, r#"{"claude_aliases":["claude2"]}"#).unwrap();
        assert_eq!(
            read(&present, Source::Pointer)
                .unwrap()
                .unwrap()
                .alias_names(),
            vec!["claude2"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The variable wins, then the pointer's first non-blank line, then the
    /// default; an empty variable or a blank pointer is unset.
    #[test]
    fn resolution_is_env_then_pointer_then_default() {
        use std::ffi::OsStr;
        let default = Some(PathBuf::from("/cfg/config.json"));
        let r = resolve_from(
            Some(OsStr::new("/env/c.json")),
            Some("/ptr/c.json\n"),
            default.clone(),
        );
        assert_eq!(
            r,
            Resolved {
                path: Some("/env/c.json".into()),
                source: Source::Env
            }
        );
        let r = resolve_from(
            Some(OsStr::new("")),
            Some("  /ptr/c.json  \n"),
            default.clone(),
        );
        assert_eq!(
            r,
            Resolved {
                path: Some("/ptr/c.json".into()),
                source: Source::Pointer
            }
        );
        let r = resolve_from(None, Some("\n"), default.clone());
        assert_eq!(
            r,
            Resolved {
                path: default.clone(),
                source: Source::Default
            }
        );
        let r = resolve_from(None, None, None);
        assert_eq!(
            r,
            Resolved {
                path: None,
                source: Source::Default
            }
        );
    }

    /// `init` writes the starter (which parses to the defaults) and the pointer,
    /// refuses to overwrite, and the pointer names the config's full path.
    #[test]
    fn init_writes_the_starter_and_the_pointer_once() {
        assert_eq!(
            parse_with_home(STARTER, home()).unwrap(),
            GlobalConfig::default()
        );
        let dir = std::env::temp_dir().join(format!("atrium-config-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = dir.join("elsewhere").join("config.json");
        let ptr = dir.join("platform").join("config.path");
        let done = init_with_pointer(&cfg, Some(&ptr)).unwrap();
        assert_eq!(done.config, cfg);
        assert_eq!(done.pointer.as_deref(), Some(ptr.as_path()));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), STARTER);
        let pointed = std::fs::read_to_string(&ptr).unwrap();
        assert_eq!(pointed.trim(), cfg.display().to_string());
        let again = init_with_pointer(&cfg, Some(&ptr)).unwrap_err();
        assert!(again.contains("already exists"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_first_run_answer_is_the_default_or_a_path() {
        let d = Path::new("/cfg/config.json");
        let h = Path::new("/home/u");
        assert_eq!(answer_to_path("", d, Some(h)), d);
        assert_eq!(answer_to_path("  \n", d, Some(h)), d);
        assert_eq!(
            answer_to_path("~/dots/atrium.json\n", d, Some(h)),
            h.join("dots/atrium.json")
        );
        assert_eq!(
            answer_to_path("\"/x/y.json\"", d, Some(h)),
            PathBuf::from("/x/y.json")
        );
    }
}
