//! The fleet loader — `amux fleet up <name>`, the saved-roster "swarm with
//! presets" feature (design doc §6b).
//!
//! A fleet is a named set of agents that come up **in-role** — identity, working
//! directory, context dirs, and instructions — in one command. Its definition
//! lives in a project-local `amux.fleet.json` (checked into the repo so a team
//! shares the fleet), with a user-global fallback. The file is read **read-only**;
//! amux never writes it.
//!
//! This module owns the *pure* seams so the wiring is testable without a pty, a
//! vault, or a layout tree:
//!
//! * [`parse`] turns the `amux.fleet.json` text into the typed [`Fleets`] map
//!   (the §6b schema), rejecting a malformed file / missing `fleets` / a fleet
//!   with zero agents with a clear message — the loader spawns nothing on error.
//! * [`Agent::args`] is the pure arg-builder: an agent def → the argv it
//!   produces (`--add-dir` / `--append-system-prompt` / `--model` / `--effort`
//!   present exactly when the corresponding field is set). The `--session-id`
//!   inject stays in the shared spawn path, so this builder is only the
//!   fleet-specific extras on top of `cmd`.
//! * [`discover`] locates the fleet file (cwd first, then the user-global
//!   fallback) and returns its path + directory, or a clear not-found error
//!   naming both locations.
//! * [`resolve_dir`] resolves an agent's `cwd` / `add_dirs` relative to the
//!   fleet file's directory (absolute paths used as-is).
//!
//! The run loop reads a [`Fleet`] out of the parsed map, builds one pane per
//! agent (each `cmd` + [`Agent::args`], under its resolved identity and cwd), and
//! lays them out in one tiled window. Identity resolution, `--session-id`
//! injection, and the name-only chrome are the existing per-pane machinery —
//! this module adds no secret handling.

use std::path::{Path, PathBuf};

/// The whole `amux.fleet.json`: a name → [`Fleet`] map. Order-preserving so
/// `fleet ls` lists fleets in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct Fleets {
    /// `(name, fleet)` pairs in document order.
    pub fleets: Vec<(String, Fleet)>,
}

impl Fleets {
    /// Look a fleet up by name. `None` if there is no such fleet.
    pub fn get(&self, name: &str) -> Option<&Fleet> {
        self.fleets.iter().find(|(n, _)| n == name).map(|(_, f)| f)
    }

    /// The fleet names in file order, for `fleet ls`.
    pub fn names(&self) -> Vec<&str> {
        self.fleets.iter().map(|(n, _)| n.as_str()).collect()
    }
}

/// One named fleet: an optional layout grid, an optional default identity, and
/// its agents. A fleet always has at least one agent ([`parse`] rejects an empty
/// `agents` list).
#[derive(Debug, Clone, PartialEq)]
pub struct Fleet {
    /// Optional `"RxC"` grid; `None` → auto balanced-grid from the agent count.
    pub grid: Option<String>,
    /// Optional default identity applied to every agent that does not name its
    /// own. `None` → agents run under their own identity or ambient creds.
    pub identity: Option<String>,
    /// The agents, in file order — one pane each.
    pub agents: Vec<Agent>,
}

/// One agent in a fleet. `name` and `cmd` are required; everything else is
/// optional (absent → default). Unknown JSON fields are ignored on parse, so the
/// schema can grow without breaking older files.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Agent {
    /// The agent's label (currently informational — the pane title still comes
    /// from the command stem, as for every other pane).
    pub name: String,
    /// The command and its args, e.g. `["claude"]` or `["claude", "--continue"]`.
    pub cmd: Vec<String>,
    /// Per-agent identity; overrides the fleet default when set.
    pub identity: Option<String>,
    /// Working directory the agent is spawned in (so its `CLAUDE.md` auto-loads).
    /// Resolved relative to the fleet file's directory.
    pub cwd: Option<String>,
    /// Extra directories the agent may read (→ `--add-dir <dirs…>`). Resolved
    /// relative to the fleet file's directory.
    pub add_dirs: Vec<String>,
    /// A system-prompt suffix (→ `--append-system-prompt <prompt>`).
    pub prompt: Option<String>,
    /// The model (→ `--model <model>`).
    pub model: Option<String>,
    /// The reasoning effort (→ `--effort <effort>`).
    pub effort: Option<String>,
}

impl Agent {
    /// The **pure arg-builder**: the full launch argv for this agent, as amux
    /// hands it to the spawn path — the base `cmd`, then the fleet extras in a
    /// fixed order. Each extra appears exactly when its field is set and is
    /// omitted when absent.
    ///
    /// `add_dirs` and `cwd` are resolved by the caller against the fleet file's
    /// directory *before* this is called, so `add_dirs` here already holds the
    /// resolved directory strings. The `--session-id` inject is NOT added here —
    /// it lives in the shared spawn path so fleet and non-fleet panes bind
    /// identically.
    ///
    /// Order: `cmd… [--add-dir d1 d2…] [--append-system-prompt P] [--model M]
    /// [--effort E]`.
    pub fn args(&self, add_dirs: &[String]) -> Vec<String> {
        let mut v = self.cmd.clone();
        if !add_dirs.is_empty() {
            v.push("--add-dir".to_string());
            for d in add_dirs {
                v.push(d.clone());
            }
        }
        if let Some(p) = &self.prompt {
            v.push("--append-system-prompt".to_string());
            v.push(p.clone());
        }
        if let Some(m) = &self.model {
            v.push("--model".to_string());
            v.push(m.clone());
        }
        if let Some(e) = &self.effort {
            v.push("--effort".to_string());
            v.push(e.clone());
        }
        v
    }
}

/// Parse an `amux.fleet.json` text into the typed [`Fleets`] map.
///
/// Rejects, with a clear message and *no* partial result, any of:
/// * malformed JSON (the `json` crate's line/column error is surfaced),
/// * a top-level that is not an object, or one missing the `fleets` object,
/// * a fleet whose `agents` is missing, not an array, or empty,
/// * an agent missing `name` or `cmd`, or whose `cmd` is not a non-empty array
///   of strings.
///
/// Unknown fields (top-level, per-fleet, per-agent) are ignored — forward
/// compatibility is a design requirement (§6b).
pub fn parse(text: &str) -> Result<Fleets, String> {
    let root = json::parse(text).map_err(|e| format!("malformed JSON: {e}"))?;
    let obj = root
        .as_object()
        .ok_or_else(|| "fleet file must be a JSON object".to_string())?;
    let fleets_val = obj
        .iter()
        .rev()
        .find(|(k, _)| k == "fleets")
        .map(|(_, v)| v)
        .ok_or_else(|| "fleet file has no \"fleets\" object".to_string())?;
    let fleet_entries = fleets_val
        .as_object()
        .ok_or_else(|| "\"fleets\" must be an object of name → fleet".to_string())?;

    let mut fleets = Vec::with_capacity(fleet_entries.len());
    for (name, fval) in fleet_entries {
        let fleet = parse_fleet(name, fval)?;
        fleets.push((name.clone(), fleet));
    }
    Ok(Fleets { fleets })
}

fn parse_fleet(name: &str, val: &json::Value) -> Result<Fleet, String> {
    let obj = val
        .as_object()
        .ok_or_else(|| format!("fleet {name:?} must be an object"))?;
    let get = |key: &str| obj.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v);

    let grid = match get("grid") {
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| format!("fleet {name:?}: \"grid\" must be a string like \"2x2\""))?
                .to_string(),
        ),
        None => None,
    };
    let identity = match get("identity") {
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| format!("fleet {name:?}: \"identity\" must be a string"))?
                .to_string(),
        ),
        None => None,
    };

    let agents_val =
        get("agents").ok_or_else(|| format!("fleet {name:?} has no \"agents\" array"))?;
    let agent_items = agents_val
        .as_array()
        .ok_or_else(|| format!("fleet {name:?}: \"agents\" must be an array"))?;
    if agent_items.is_empty() {
        return Err(format!("fleet {name:?} has zero agents"));
    }
    let mut agents = Vec::with_capacity(agent_items.len());
    for (i, av) in agent_items.iter().enumerate() {
        agents.push(parse_agent(name, i, av)?);
    }

    Ok(Fleet {
        grid,
        identity,
        agents,
    })
}

fn parse_agent(fleet: &str, idx: usize, val: &json::Value) -> Result<Agent, String> {
    let obj = val
        .as_object()
        .ok_or_else(|| format!("fleet {fleet:?} agent #{idx}: must be an object"))?;
    let get = |key: &str| obj.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v);

    let name = get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("fleet {fleet:?} agent #{idx}: missing string \"name\""))?
        .to_string();

    let cmd_val = get("cmd")
        .ok_or_else(|| format!("fleet {fleet:?} agent {name:?}: missing \"cmd\" array"))?;
    let cmd_items = cmd_val
        .as_array()
        .ok_or_else(|| format!("fleet {fleet:?} agent {name:?}: \"cmd\" must be an array"))?;
    let mut cmd = Vec::with_capacity(cmd_items.len());
    for c in cmd_items {
        cmd.push(
            c.as_str()
                .ok_or_else(|| {
                    format!("fleet {fleet:?} agent {name:?}: \"cmd\" entries must be strings")
                })?
                .to_string(),
        );
    }
    if cmd.is_empty() {
        return Err(format!("fleet {fleet:?} agent {name:?}: \"cmd\" is empty"));
    }

    let str_field = |key: &str| -> Result<Option<String>, String> {
        match get(key) {
            Some(v) => Ok(Some(
                v.as_str()
                    .ok_or_else(|| {
                        format!("fleet {fleet:?} agent {name:?}: {key:?} must be a string")
                    })?
                    .to_string(),
            )),
            None => Ok(None),
        }
    };

    let identity = str_field("identity")?;
    let cwd = str_field("cwd")?;
    let prompt = str_field("prompt")?;
    let model = str_field("model")?;
    let effort = str_field("effort")?;

    let add_dirs = match get("add_dirs") {
        Some(v) => {
            let items = v.as_array().ok_or_else(|| {
                format!("fleet {fleet:?} agent {name:?}: \"add_dirs\" must be an array")
            })?;
            let mut dirs = Vec::with_capacity(items.len());
            for d in items {
                dirs.push(
                    d.as_str()
                        .ok_or_else(|| {
                            format!(
                                "fleet {fleet:?} agent {name:?}: \"add_dirs\" entries must be strings"
                            )
                        })?
                        .to_string(),
                );
            }
            dirs
        }
        None => Vec::new(),
    };

    Ok(Agent {
        name,
        cmd,
        identity,
        cwd,
        add_dirs,
        prompt,
        model,
        effort,
    })
}

/// The fleet-file name looked for in the current directory.
pub const FILE_NAME: &str = "amux.fleet.json";

/// A located fleet file: the path we read and the directory that agent `cwd` /
/// `add_dirs` are resolved against (the file's own directory).
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub path: PathBuf,
    pub dir: PathBuf,
}

/// The user-global fleet-file path (`%APPDATA%\amux\fleet.json` on Windows,
/// `~/.config/amux/fleet.json` elsewhere), or `None` if the base dir is unset.
pub fn global_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.map(|b| b.join("amux").join("fleet.json"))
}

/// A human-readable rendering of the global path for error messages, even when
/// the base dir is unset (so the message can still name *where* it would look).
fn global_path_display() -> String {
    match global_path() {
        Some(p) => p.display().to_string(),
        None => {
            if cfg!(windows) {
                "%APPDATA%\\amux\\fleet.json".to_string()
            } else {
                "~/.config/amux/fleet.json".to_string()
            }
        }
    }
}

/// Locate the fleet file: `./amux.fleet.json` first, then the user-global
/// fallback. Returns the located file, or a clear not-found error naming **both**
/// locations. `cwd` is the current directory (injected so this is testable).
pub fn discover(cwd: &Path) -> Result<Located, String> {
    let local = cwd.join(FILE_NAME);
    if local.is_file() {
        return Ok(Located {
            dir: cwd.to_path_buf(),
            path: local,
        });
    }
    if let Some(global) = global_path() {
        if global.is_file() {
            let dir = global
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            return Ok(Located { path: global, dir });
        }
    }
    Err(format!(
        "no fleet file found: looked for {} then {}",
        local.display(),
        global_path_display()
    ))
}

/// Resolve one directory string against the fleet file's directory: an absolute
/// path is used as-is, a relative one is joined onto `base`. Pure path algebra —
/// no filesystem access, so it is testable and does not itself decide existence.
pub fn resolve_dir(base: &Path, dir: &str) -> PathBuf {
    let p = Path::new(dir);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // --- parse ---------------------------------------------------------------

    #[test]
    fn parses_a_full_fleet_with_every_field() {
        let text = r#"{
          "fleets": {
            "review-crew": {
              "grid": "2x2",
              "identity": "work",
              "agents": [
                {
                  "name": "reviewer",
                  "cmd": ["claude", "--continue"],
                  "identity": "wif:prod",
                  "cwd": "./review",
                  "add_dirs": ["../shared", "./specs"],
                  "prompt": "You review PRs for safety.",
                  "model": "opus",
                  "effort": "high"
                }
              ]
            }
          }
        }"#;
        let fleets = parse(text).unwrap();
        assert_eq!(fleets.names(), vec!["review-crew"]);
        let f = fleets.get("review-crew").unwrap();
        assert_eq!(f.grid.as_deref(), Some("2x2"));
        assert_eq!(f.identity.as_deref(), Some("work"));
        assert_eq!(f.agents.len(), 1);
        let a = &f.agents[0];
        assert_eq!(a.name, "reviewer");
        assert_eq!(a.cmd, s(&["claude", "--continue"]));
        assert_eq!(a.identity.as_deref(), Some("wif:prod"));
        assert_eq!(a.cwd.as_deref(), Some("./review"));
        assert_eq!(a.add_dirs, s(&["../shared", "./specs"]));
        assert_eq!(a.prompt.as_deref(), Some("You review PRs for safety."));
        assert_eq!(a.model.as_deref(), Some("opus"));
        assert_eq!(a.effort.as_deref(), Some("high"));
    }

    #[test]
    fn parses_a_minimal_agent_with_only_name_and_cmd() {
        let text = r#"{ "fleets": { "solo": { "agents": [
          { "name": "worker", "cmd": ["claude"] }
        ] } } }"#;
        let f = parse(text).unwrap();
        let fleet = f.get("solo").unwrap();
        assert_eq!(fleet.grid, None);
        assert_eq!(fleet.identity, None);
        let a = &fleet.agents[0];
        assert_eq!(a.cmd, s(&["claude"]));
        assert_eq!(a.identity, None);
        assert_eq!(a.cwd, None);
        assert!(a.add_dirs.is_empty());
        assert_eq!(a.prompt, None);
        assert_eq!(a.model, None);
        assert_eq!(a.effort, None);
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compat() {
        let text = r#"{ "future": 1, "fleets": {
          "f": { "when": "later", "agents": [
            { "name": "a", "cmd": ["claude"], "tools": ["x"] }
          ] }
        } }"#;
        let f = parse(text).unwrap();
        assert_eq!(f.get("f").unwrap().agents[0].cmd, s(&["claude"]));
    }

    #[test]
    fn preserves_fleet_order() {
        let text = r#"{ "fleets": {
          "b": { "agents": [{ "name": "x", "cmd": ["sh"] }] },
          "a": { "agents": [{ "name": "y", "cmd": ["sh"] }] }
        } }"#;
        assert_eq!(parse(text).unwrap().names(), vec!["b", "a"]);
    }

    #[test]
    fn malformed_json_is_a_clear_error() {
        let err = parse("{ not json").unwrap_err();
        assert!(err.contains("malformed JSON"), "{err}");
    }

    #[test]
    fn missing_fleets_object_is_rejected() {
        let err = parse(r#"{ "other": {} }"#).unwrap_err();
        assert!(err.contains("no \"fleets\""), "{err}");
    }

    #[test]
    fn non_object_top_level_is_rejected() {
        assert!(parse("[]").unwrap_err().contains("must be a JSON object"));
    }

    #[test]
    fn empty_agents_is_rejected() {
        let err = parse(r#"{ "fleets": { "f": { "agents": [] } } }"#).unwrap_err();
        assert!(err.contains("zero agents"), "{err}");
    }

    #[test]
    fn missing_agents_is_rejected() {
        let err = parse(r#"{ "fleets": { "f": {} } }"#).unwrap_err();
        assert!(err.contains("no \"agents\""), "{err}");
    }

    #[test]
    fn agent_without_cmd_is_rejected() {
        let err = parse(r#"{ "fleets": { "f": { "agents": [ { "name": "a" } ] } } }"#).unwrap_err();
        assert!(err.contains("missing \"cmd\""), "{err}");
    }

    #[test]
    fn agent_without_name_is_rejected() {
        let err =
            parse(r#"{ "fleets": { "f": { "agents": [ { "cmd": ["sh"] } ] } } }"#).unwrap_err();
        assert!(err.contains("missing string \"name\""), "{err}");
    }

    #[test]
    fn empty_cmd_array_is_rejected() {
        let err = parse(r#"{ "fleets": { "f": { "agents": [ { "name": "a", "cmd": [] } ] } } }"#)
            .unwrap_err();
        assert!(err.contains("\"cmd\" is empty"), "{err}");
    }

    #[test]
    fn unknown_fleet_name_is_none() {
        let f =
            parse(r#"{ "fleets": { "a": { "agents": [{"name":"x","cmd":["sh"]}] } } }"#).unwrap();
        assert!(f.get("nope").is_none());
    }

    // --- the pure arg-builder ------------------------------------------------

    #[test]
    fn args_of_a_bare_agent_is_just_its_cmd() {
        let a = Agent {
            name: "x".into(),
            cmd: s(&["claude"]),
            ..Agent::default()
        };
        assert_eq!(a.args(&[]), s(&["claude"]));
    }

    #[test]
    fn args_appends_every_set_field_in_order() {
        let a = Agent {
            name: "x".into(),
            cmd: s(&["claude", "--continue"]),
            prompt: Some("be careful".into()),
            model: Some("opus".into()),
            effort: Some("high".into()),
            ..Agent::default()
        };
        let dirs = s(&["/abs/shared", "/abs/specs"]);
        assert_eq!(
            a.args(&dirs),
            s(&[
                "claude",
                "--continue",
                "--add-dir",
                "/abs/shared",
                "/abs/specs",
                "--append-system-prompt",
                "be careful",
                "--model",
                "opus",
                "--effort",
                "high",
            ])
        );
    }

    #[test]
    fn args_omits_absent_fields() {
        let a = Agent {
            name: "x".into(),
            cmd: s(&["claude"]),
            model: Some("sonnet".into()),
            ..Agent::default()
        };
        // Only --model is present; no --add-dir / --append-system-prompt / --effort.
        assert_eq!(a.args(&[]), s(&["claude", "--model", "sonnet"]));
    }

    #[test]
    fn args_uses_the_resolved_add_dirs_not_the_fields_own() {
        // The builder takes resolved dirs as an argument; a.add_dirs is not read
        // directly, so the caller's resolution against the fleet dir is honored.
        let a = Agent {
            name: "x".into(),
            cmd: s(&["claude"]),
            add_dirs: s(&["./relative"]),
            ..Agent::default()
        };
        assert_eq!(
            a.args(&s(&["/base/relative"])),
            s(&["claude", "--add-dir", "/base/relative"])
        );
    }

    // --- path resolution -----------------------------------------------------

    #[test]
    fn resolve_dir_joins_relative_onto_base() {
        let base = Path::new("/proj");
        assert_eq!(resolve_dir(base, "review"), PathBuf::from("/proj/review"));
        assert_eq!(
            resolve_dir(base, "./sub"),
            PathBuf::from("/proj/./sub"),
            "relative kept as a join (normalization is the OS's job)"
        );
    }

    #[test]
    fn resolve_dir_keeps_absolute_as_is() {
        let base = Path::new("/proj");
        #[cfg(not(windows))]
        assert_eq!(resolve_dir(base, "/etc/x"), PathBuf::from("/etc/x"));
        #[cfg(windows)]
        assert_eq!(resolve_dir(base, "C:\\etc\\x"), PathBuf::from("C:\\etc\\x"));
    }

    // --- discovery -----------------------------------------------------------

    #[test]
    fn discover_finds_the_local_file_first() {
        let td = std::env::temp_dir().join(format!("amux-fleet-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(&td).unwrap();
        std::fs::write(td.join(FILE_NAME), "{}").unwrap();
        let found = discover(&td).unwrap();
        assert_eq!(found.path, td.join(FILE_NAME));
        assert_eq!(found.dir, td);
        let _ = std::fs::remove_dir_all(&td);
    }

    #[test]
    fn discover_missing_names_both_locations() {
        let td = std::env::temp_dir().join(format!("amux-fleet-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(&td).unwrap();
        let err = discover(&td).unwrap_err();
        assert!(err.contains(FILE_NAME), "names local: {err}");
        // Names the global location too (the word "then" separates the two).
        assert!(err.contains("then"), "names both: {err}");
        let _ = std::fs::remove_dir_all(&td);
    }
}
