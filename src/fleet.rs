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
//! * [`Plan`] resolves that reach **through symlinks**, classifies each grant
//!   against an [`anchor_for`] directory, and renders the banner the operator
//!   acknowledges — the same resolved paths the spawn then uses, so disclosure
//!   and launch cannot drift. It refuses nothing; read its doc comment for what
//!   it does not cover.
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
    /// Optional trust posture for the whole fleet (`"plan"`, `"accept"`,
    /// `"automode"`, `"skip"`), used when the command line does not pass
    /// `--trust`.
    ///
    /// A fleet is exactly the case where a CLI flag is the wrong home for this:
    /// you launch it to run hands-off, so it needs a posture, and typing one
    /// every time is a flag you will eventually get wrong. Declared here it lives
    /// with the roster it applies to, is reviewable in a diff, and different
    /// fleets can differ. It remains a REQUEST, not an override — the session
    /// ceiling still caps it, so a fleet file asking for `skip` inside a `plan`
    /// session gets `plan`.
    pub trust: Option<String>,
    /// Optional: bring the control plane up for this fleet, as `--allow-ctl`
    /// does. `true` in the file is equivalent to passing the flag.
    ///
    /// A fleet whose agents coordinate — the whole reason to run one — is dead
    /// without ctl, and silently so: the panes come up, the kickoffs tell them to
    /// publish to a bus that is not there, and nothing happens. That failure has
    /// already been hit in practice. If the file is meant to be the complete,
    /// reviewable definition of a spin-up, the control plane belongs in it rather
    /// than in a flag the operator has to remember.
    pub allow_ctl: Option<bool>,
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
    /// May this agent create teammates with `amux ctl spawn`? Defaults to
    /// **false** for a fleet agent.
    ///
    /// Spawning was never a capability an agent had or lacked - it was implied by
    /// having ctl access at all, then bounded after the fact by a depth cap, the
    /// spawn allowlist and the trust ceiling. So in a seven-agent review fleet
    /// every reviewer could create teammates; none should, and nothing said so.
    ///
    /// Declaring it per agent puts the answer in the file the human approves,
    /// rather than leaving it inferred from a depth cap three layers down. It
    /// composes with the existing checks instead of replacing them: an agent with
    /// `can_spawn` still cannot exceed the trust ceiling or the depth cap.
    pub can_spawn: Option<bool>,
    /// An initial **user** prompt appended as the final positional argument, so
    /// the agent starts working the moment the fleet comes up instead of waiting
    /// for the human to type. For claude this is `claude … "<kickoff>"`, which
    /// seeds an interactive session with that first message. Absent ⇒ the agent
    /// idles until prompted (the previous behavior).
    pub kickoff: Option<String>,
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
        // The kickoff is the trailing **positional** prompt, after every flag, so
        // the agent (claude) treats it as the first user message and starts.
        if let Some(k) = &self.kickoff {
            v.push(k.clone());
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
    let allow_ctl = match get("allow_ctl") {
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| format!("fleet {name:?}: \"allow_ctl\" must be true or false"))?,
        ),
        None => None,
    };
    let trust = match get("trust") {
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| format!("fleet {name:?}: \"trust\" must be a string"))?
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
        trust,
        allow_ctl,
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
    let can_spawn =
        match get("can_spawn") {
            Some(v) => Some(v.as_bool().ok_or_else(|| {
                format!("fleet agent {name:?}: \"can_spawn\" must be true or false")
            })?),
            None => None,
        };
    let kickoff = str_field("kickoff")?;

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
        can_spawn,
        kickoff,
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
    /// True when this is the user-global file rather than a project-local one.
    ///
    /// Its directory is `~/.config/amux`, which holds no project by
    /// construction, so it is the wrong thing to measure "inside" against — see
    /// [`anchor_for`], which uses the invoking directory instead.
    pub global: bool,
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
            global: false,
        });
    }
    if let Some(global) = global_path() {
        if global.is_file() {
            let dir = global
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            return Ok(Located {
                path: global,
                dir,
                global: true,
            });
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

// ---------------------------------------------------------------------------
// Disclosure: where a fleet file's directory grants actually land
// ---------------------------------------------------------------------------
//
// `resolve_dir` above is pure path algebra, so `cwd` / `add_dirs` can name any
// directory on the machine — `~/.ssh`, `/etc`, a sibling checkout — and until
// this layer existed nothing said so. `add_dirs` becomes `claude --add-dir`,
// i.e. read access handed to an agent whose roster a *different* agent wrote.
//
// The control is disclosure, not refusal, and that is a deliberate choice:
//
// * The reach itself is a documented feature. README's own example grants
//   `"../shared"` — a sibling checkout — and two independent attempts to ban
//   "outside" paths broke that example, then broke the user-global fleet file
//   (whose directory is `~/.config/amux`, so *every* useful path is outside it)
//   and a monorepo whose fleet file sits in `tools/`. A gate that refuses
//   ordinary work is a gate people route around, and the routing-around is
//   invisible.
// * Since 31774f0 `fleet up` BLOCKS for an Enter before the run loop switches to
//   the alternate screen. Before that commit the banner was printed and wiped in
//   the same breath and disclosure was worthless; now the operator genuinely
//   sees it. Disclosure is the control, so it has to be *readable*: deduped,
//   bounded, and anchored somewhere that makes "outside" rare enough to mean
//   something.
//
// What this layer therefore guarantees, and nothing more: **every directory
// amux itself grants an agent through `cwd`/`add_dirs` is resolved through
// symlinks, classified against the anchor, and named on the screen the operator
// acknowledges — and the child is handed the same resolved path that was
// shown.** What it does not and cannot cover is listed on `Plan::build`.

/// Longest rendering of one file-supplied string in a banner line. A 40 KB
/// `add_dirs` entry would otherwise scroll the rest of the disclosure — the part
/// naming the other grants — off the screen the operator is about to approve.
pub const BANNER_MAX: usize = 120;

/// Most loud (non-quiet) grant lines printed before the rest is summarised. The
/// banner competes for a 24-row terminal with the trust posture and can-spawn
/// lines, and those scrolling off is the failure 31774f0 exists to prevent.
pub const MAX_LOUD_LINES: usize = 6;

/// Render a fleet-file string inert for a terminal.
///
/// The `json` parser decodes `\u001b`, so any string in the file — an agent
/// name, a path, an identity — can carry a real ESC. Printed raw into the
/// approval banner it can clear the screen, repaint a forged "0 dirs outside"
/// line, or reorder a path with a bidi override so `/etc/shadow` reads as
/// something harmless. The banner is the control; a string that can rewrite it
/// defeats the control.
///
/// Escaped: every `Cc` (C0 controls, DEL, C1 — `char::is_control` covers all
/// three), the bidi/invisible formatting characters that reorder or hide text
/// without being controls, and `"` so a name cannot close its own quoted field.
///
/// Deliberately NOT escaped: `\`. Doubling it mangles every Windows path in the
/// banner (`C:\Users\dev\repo` → `C:\\Users\\dev\\repo`), on the platform with
/// the longest paths, to buy an injectivity this rendering never claims: a file
/// name containing the literal text `\u{1b}` renders identically to a real ESC.
/// This defangs; it does not round-trip.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let hostile = c.is_control()
            || matches!(
                c,
                '\u{061c}' | '\u{180e}' | '\u{feff}' | '\u{2028}' | '\u{2029}'
            )
            || ('\u{200b}'..='\u{200f}').contains(&c)
            || ('\u{202a}'..='\u{202e}').contains(&c)
            || ('\u{2060}'..='\u{2064}').contains(&c)
            || ('\u{2066}'..='\u{206f}').contains(&c);
        if hostile {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else if c == '"' {
            out.push_str("\\\"");
        } else {
            out.push(c);
        }
    }
    out
}

/// Cap a rendered string at `max` characters, eliding the **middle**.
///
/// Cutting the tail would eat the destination, which is the one fact a
/// disclosure line exists to carry: `…/Library/CloudStorage/OneDri…(truncated)`
/// tells the operator a grant leaves the tree and then withholds where it goes.
/// The `…` marks the cut so a shortened path cannot be mistaken for a complete
/// one.
pub fn shorten(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max || max < 8 {
        return s.to_string();
    }
    let keep = max - 1;
    let head = keep / 3;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

/// A path as it appears in the banner: defanged, then middle-elided.
pub fn show_path(p: &Path) -> String {
    shorten(&sanitize(&p.display().to_string()), BANNER_MAX)
}

/// A file-supplied string as it appears in the banner, quoted.
fn show_str(s: &str) -> String {
    format!("\"{}\"", shorten(&sanitize(s), BANNER_MAX))
}

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
/// amux cannot tell.
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
    if let Ok(real) = std::fs::canonicalize(p) {
        return Some(presentable(real));
    }
    // The path itself is a symlink amux could not follow (a dangling target, an
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
/// which is a normal amux job — with no override anywhere.
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

/// Where a grant lands relative to the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Resolved, symlinks followed, to a path under the anchor.
    Inside,
    /// Resolved to a path outside the anchor. Allowed — and always printed.
    Outside,
    /// amux could not resolve it. Printed as loudly as `Outside`: a check that
    /// treats "cannot tell" as "fine" is the fail-open shape this repo keeps
    /// shipping.
    Unverifiable,
}

/// Which field of the agent entry produced a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Cwd,
    AddDir,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Cwd => "cwd",
            Field::AddDir => "add-dir",
        }
    }
}

/// One directory this fleet grants one agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub field: Field,
    /// The string exactly as it appears in the fleet file.
    pub raw: String,
    /// What the child is actually spawned with — the resolved path when it
    /// resolves, so the banner and the launch cannot disagree.
    pub given: PathBuf,
    /// Where it really lands, symlinks followed. `None` ⇒ unresolvable.
    pub real: Option<PathBuf>,
    pub exists: bool,
    pub reach: Reach,
    /// Set when the destination is, or contains, a known credential store.
    pub holds: Option<String>,
}

impl Grant {
    /// Resolve one `cwd` / `add_dirs` entry and classify it.
    pub fn probe(anchor: &Path, base: &Path, field: Field, raw: &str, stores: &Stores) -> Grant {
        let joined = resolve_dir(base, raw);
        let real = real_path(&joined);
        let reach = match &real {
            Some(r) if r.starts_with(anchor) => Reach::Inside,
            Some(_) => Reach::Outside,
            None => Reach::Unverifiable,
        };
        let holds = stores.describe(real.as_deref());
        Grant {
            field,
            raw: raw.to_string(),
            // An unresolvable path keeps the pre-change behaviour (the plain
            // join) — this layer discloses, it does not refuse, so an agent must
            // still launch with something.
            given: real.clone().unwrap_or_else(|| joined.clone()),
            exists: joined.exists(),
            real,
            reach,
            holds,
        }
    }

    /// Can this grant be folded into the terse "N other dirs inside" count?
    ///
    /// Only when all three of "inside", "exists" and "no credential store" hold.
    /// A path that does not exist is never quiet: a typo'd or `~`-prefixed entry
    /// (amux does no tilde expansion, so `"~/notes"` becomes `<base>/~/notes`)
    /// is exactly the entry most likely to be wrong, and counting it as inside
    /// is a positive false statement rather than a mere gap.
    pub fn is_quiet(&self) -> bool {
        self.reach == Reach::Inside && self.exists && self.holds.is_none()
    }

    /// How alarming this grant is, lowest first.
    ///
    /// The banner is capped, so ORDER is a security property: without this a
    /// roster could bury `--add-dir ~/.ssh` behind fifty innocuous outside
    /// directories and push it past the cap, which is the same trick as burying
    /// a flag in `cmd` and hoping the review skims.
    pub fn severity(&self) -> u8 {
        match (self.holds.is_some(), self.reach, self.exists) {
            (true, _, _) => 0,
            (_, Reach::Unverifiable, _) => 1,
            (_, Reach::Outside, _) => 2,
            (_, Reach::Inside, false) => 3,
            (_, Reach::Inside, true) => 4,
        }
    }

    /// The word that starts this grant's disclosure line.
    pub fn tag(&self) -> &'static str {
        match (self.reach, self.exists, self.holds.is_some()) {
            (Reach::Unverifiable, _, _) => "UNVERIFIABLE",
            (_, _, true) => "CREDENTIALS",
            (Reach::Outside, _, _) => "OUTSIDE",
            (Reach::Inside, false, _) => "MISSING",
            (Reach::Inside, true, _) => "inside",
        }
    }

    /// The destination half of the line: where it really lands.
    pub fn destination(&self) -> String {
        match &self.real {
            Some(r) => show_path(r),
            None => "(amux cannot resolve this path)".to_string(),
        }
    }

    /// The trailing clause: what is there, or that nothing is yet.
    pub fn note(&self) -> String {
        let mut n = String::new();
        if let Some(h) = &self.holds {
            n.push_str(" — ");
            n.push_str(h);
        }
        if !self.exists && self.reach != Reach::Unverifiable {
            n.push_str(" (does not exist yet)");
        }
        n
    }
}

/// What "inside" is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// The repository the fleet file is checked into.
    Repo,
    /// The fleet file's own directory (no enclosing repo).
    FleetDir,
    /// The directory `amux` was run from — used for the user-global fleet file,
    /// whose own directory (`~/.config/amux`) contains no project at all.
    InvokingDir,
}

/// The directory grants are measured against, and why it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub dir: PathBuf,
    pub kind: AnchorKind,
}

impl AnchorKind {
    /// Why this directory is the anchor — printed once, so an operator can tell
    /// whether "outside" is a strong statement or a weak one.
    pub fn what(self) -> &'static str {
        match self {
            AnchorKind::Repo => "the enclosing git repository",
            AnchorKind::FleetDir => "the fleet file's own directory",
            AnchorKind::InvokingDir => "the directory you ran amux in",
        }
    }
}

/// Choose the anchor.
///
/// Anchoring on the fleet file's own directory — the obvious choice — was
/// measured and is wrong twice over. A monorepo whose fleet file lives in
/// `tools/` marks every same-repo path OUTSIDE (18 loud lines for a 6-agent
/// roster, scrolling the trust posture off a 24-row terminal), and the
/// documented user-global file at `~/.config/amux/fleet.json` can produce
/// *nothing but* OUTSIDE, because that directory holds no project by
/// construction. A tag that appears on 100% of lines carries no information and
/// trains the operator to skim past it — which is the whole control.
///
/// So: the repo the file is checked into (the unit a reviewer already trusts),
/// else the file's directory; and for the global file, the directory amux was
/// run in, since that is the project the operator meant.
///
/// The walk up stops short of `$HOME` and of any ancestor of it: a home
/// directory that happens to be a git repo would make "inside" cover `~/.ssh`,
/// i.e. mean nothing. (If the fleet file itself sits in `$HOME` the anchor still
/// ends up there — that case is caught by the credential-store tag on the grant,
/// not by the anchor.)
///
/// Stated plainly, because it is the weak seam here: the anchor is read from the
/// filesystem, and whoever writes the fleet file can usually write the
/// filesystem too. A `.git` created in an ancestor of a fleet file that is NOT
/// in a repo widens the anchor and makes a grant under that ancestor read as
/// "inside". Two things bound it — a real repo's own `.git` is found first, so
/// the ordinary case cannot be widened, and the chosen anchor is printed on the
/// banner's first line, so an anchor nobody recognises is itself visible. It is
/// not a defence against someone who controls both the file and the tree; it
/// makes the tag honest about which directory it is a statement *about*.
pub fn anchor_for(
    fleet_dir: &Path,
    invoking_dir: &Path,
    global: bool,
    home: Option<&Path>,
) -> Anchor {
    let (start, kind) = if global {
        (invoking_dir, AnchorKind::InvokingDir)
    } else {
        (fleet_dir, AnchorKind::FleetDir)
    };
    let base = real_path(start).unwrap_or_else(|| start.to_path_buf());
    let mut cur: &Path = &base;
    loop {
        let covers_home = home.is_some_and(|h| h.starts_with(cur));
        if !covers_home && cur.join(".git").exists() {
            return Anchor {
                dir: cur.to_path_buf(),
                kind: AnchorKind::Repo,
            };
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    Anchor { dir: base, kind }
}

/// One agent's disclosed launch: the label its pane wears, its credentials, its
/// directories, and whether amux can see what it will do at all.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentPlan {
    /// The agent's name, already defanged — this is both the banner label and
    /// the pane `role`, which the status bar and overview paint unfiltered.
    pub label: String,
    /// The identity (akey credential) this agent comes up on, if any.
    pub identity: Option<String>,
    /// `Some(stem)` when `cmd` is not an agent CLI amux knows — a shell, a build
    /// tool. See `Plan::build` for why that is disclosed.
    pub opaque: Option<String>,
    pub cwd: Option<Grant>,
    pub add_dirs: Vec<Grant>,
}

impl AgentPlan {
    /// Every directory this agent is granted, cwd first.
    pub fn grants(&self) -> impl Iterator<Item = &Grant> {
        self.cwd.iter().chain(self.add_dirs.iter())
    }
}

/// One deduplicated disclosure line: the rendered text, the destination it
/// names, and every agent that asked for it.
#[derive(Debug, Clone, PartialEq)]
struct Loud {
    severity: u8,
    line: String,
    dest: String,
    who: Vec<String>,
}

/// The whole fleet, resolved once: what will be granted, in the form the banner
/// prints AND the form the spawn uses.
///
/// Built once in `fleet_up` and read by both the disclosure and
/// `spawn_fleet_window`. That is structural, not tidiness: the previous version
/// resolved `cwd`/`add_dirs` a second time inside the spawn, so the banner and
/// the launch were two independent computations that could differ — and a
/// symlink flipped during the operator's Enter window made them differ, turning
/// an acknowledged "inside" into a live grant on a credential store.
///
/// # What this does NOT cover — read this before relying on it
///
/// * **A symlink inside a granted directory.** `--add-dir <d>` grants `d`'s
///   whole subtree, and a link at `d/keys -> ~/.ssh` is reachable through it.
///   Only `d` is classified; walking the subtree is unbounded, racy, and still
///   wrong a second later. An add_dir grants its transitive symlink closure.
/// * **Anything `cmd` does.** `vet_spawn_argv` refuses a literal `--add-dir` in
///   `cmd`, but `["sh","-c","claude --add-dir ~/.ssh"]` is a shell command amux
///   neither parses nor should. That is why a non-agent `cmd` is disclosed as
///   such: amux can bound the directories *it* grants, not what a program it
///   starts grants itself.
/// * **The rest of the entry.** `prompt` and `kickoff` go into the child's argv
///   and are not shown here (they are text, not access).
/// * **TOCTOU, narrowed but not closed.** The child gets the path that was
///   resolved and shown, so re-pointing the *named* link after the ack no longer
///   moves the grant; re-pointing a directory component of that resolved path
///   between the ack and the spawn still would. Closing it needs `openat`
///   /`O_NOFOLLOW` plumbing through the spawn path.
/// * **It is not a sandbox.** Same-uid: everything amux can read the agent can
///   read on its own (see `warden.rs`). This bounds what amux *hands over*, and
///   makes it visible at approval time. Nothing more.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub file: PathBuf,
    pub anchor: Anchor,
    pub agents: Vec<AgentPlan>,
}

impl Plan {
    pub fn build(fleet: &Fleet, file: &Path, base: &Path, anchor: Anchor, stores: &Stores) -> Plan {
        let agents = fleet
            .agents
            .iter()
            .map(|a| {
                let stem =
                    crate::bind::command_stem(a.cmd.first().map(String::as_str).unwrap_or(""));
                AgentPlan {
                    label: sanitize(&a.name),
                    identity: a
                        .identity
                        .clone()
                        .or_else(|| fleet.identity.clone())
                        .map(|i| sanitize(&i)),
                    opaque: if crate::bind::is_agent_stem(&stem) {
                        None
                    } else {
                        Some(sanitize(&stem))
                    },
                    cwd: a
                        .cwd
                        .as_ref()
                        .map(|d| Grant::probe(&anchor.dir, base, Field::Cwd, d, stores)),
                    add_dirs: a
                        .add_dirs
                        .iter()
                        .map(|d| Grant::probe(&anchor.dir, base, Field::AddDir, d, stores))
                        .collect(),
                }
            })
            .collect();
        Plan {
            file: file.to_path_buf(),
            anchor,
            agents,
        }
    }

    /// The grants that must be named, deduplicated by (what, where) with the
    /// agents that asked for them listed together.
    ///
    /// Without this the documented `["../shared","../protos"]` pattern across ten
    /// agents printed twenty near-identical loud lines carrying two facts, and on
    /// 80 columns each wrapped: the banner defeated itself and pushed the posture
    /// line off the screen.
    fn loud(&self) -> Vec<Loud> {
        let mut out: Vec<Loud> = Vec::new();
        for a in &self.agents {
            for g in a.grants().filter(|g| !g.is_quiet()) {
                // An absolute entry resolves to itself; printing the same long
                // path twice on one line doubles its width for no information.
                let raw = show_str(&g.raw);
                let dest = g.destination();
                let line = if raw.trim_matches('"') == dest {
                    format!("{} {} {}{}", g.tag(), g.field.label(), dest, g.note())
                } else {
                    format!(
                        "{} {} {} -> {}{}",
                        g.tag(),
                        g.field.label(),
                        raw,
                        dest,
                        g.note()
                    )
                };
                match out.iter_mut().find(|l| l.line == line) {
                    Some(l) => {
                        if !l.who.contains(&a.label) {
                            l.who.push(a.label.clone());
                        }
                    }
                    None => out.push(Loud {
                        severity: g.severity(),
                        line,
                        dest: g.destination(),
                        who: vec![a.label.clone()],
                    }),
                }
            }
        }
        // Worst first, so neither the line cap nor the verdict's short list can
        // be filled with noise while the credential store scrolls away. Stable,
        // so file order still decides between equals.
        out.sort_by_key(|l| l.severity);
        out
    }

    /// The destinations an operator would want named, worst first.
    fn escapes(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for l in self.loud() {
            if !v.contains(&l.dest) {
                v.push(l.dest);
            }
        }
        v
    }

    /// The banner, one line per entry (the caller prefixes them).
    pub fn banner_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "file {} — dirs measured against {} ({})",
            show_path(&self.file),
            show_path(&self.anchor.dir),
            self.anchor.kind.what()
        ));
        let loud = self.loud();
        for Loud { line, who, .. } in loud.iter().take(MAX_LOUD_LINES) {
            let mut names = who
                .iter()
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            if who.len() > 3 {
                names.push_str(&format!(" +{}", who.len() - 3));
            }
            lines.push(format!("  {line}   [{names}]"));
        }
        if loud.len() > MAX_LOUD_LINES {
            lines.push(format!(
                "  …and {} more disclosed dir(s), listed in {}",
                loud.len() - MAX_LOUD_LINES,
                show_path(&self.file)
            ));
        }
        let quiet = self
            .agents
            .iter()
            .flat_map(|a| a.grants())
            .filter(|g| g.is_quiet())
            .count();
        if quiet > 0 {
            lines.push(format!(
                "{quiet} other dir(s) resolve inside {}",
                show_path(&self.anchor.dir)
            ));
        }
        // Identity selects a credential through akey and was disclosed nowhere:
        // a roster could bring an agent up on `wif:prod` and nothing said so.
        //
        // Collapsed when the whole fleet shares one identity (the common case,
        // and "ambient" repeated once per agent is a quarter of the banner's
        // bulk carrying no information).
        let first = self.agents.first().map(|a| a.identity.clone());
        let uniform = self
            .agents
            .iter()
            .all(|a| Some(a.identity.clone()) == first);
        let ident = |a: &AgentPlan| a.identity.clone().unwrap_or_else(|| "ambient".to_string());
        if uniform {
            if let Some(a) = self.agents.first() {
                lines.push(format!(
                    "identities — all {} agent(s) on {}",
                    self.agents.len(),
                    ident(a)
                ));
            }
        } else {
            let ids: Vec<String> = self
                .agents
                .iter()
                .map(|a| format!("{}={}", a.label, ident(a)))
                .collect();
            lines.push(format!("identities — {}", shorten(&ids.join(", "), 200)));
        }
        let opaque: Vec<String> = self
            .agents
            .iter()
            .filter_map(|a| a.opaque.as_ref().map(|s| format!("{} ({s})", a.label)))
            .collect();
        if !opaque.is_empty() {
            lines.push(format!(
                "amux cannot see what these will read — they do not run an agent CLI: {}",
                shorten(&opaque.join(", "), 200)
            ));
        }
        lines
    }

    /// The one line printed immediately before the blocking Enter.
    ///
    /// Measured failure it fixes: on a 24-row terminal a six-agent roster's
    /// banner scrolls, and the only line guaranteed to still be on screen at the
    /// prompt was a bare count — the one line that omitted the payload. This
    /// names the destinations.
    pub fn verdict(&self) -> String {
        let esc = self.escapes();
        if esc.is_empty() {
            return format!(
                "every agent dir resolves inside {}",
                shorten(&show_path(&self.anchor.dir), 80)
            );
        }
        // Rendered tighter than the lines above: this one has to fit on a
        // terminal row next to the count, and the reader has the full paths
        // three lines up.
        let shown: Vec<String> = esc.iter().take(3).map(|d| shorten(d, 56)).collect();
        let more = if esc.len() > 3 {
            format!(" (+{} more, listed above)", esc.len() - 3)
        } else {
            String::new()
        };
        format!(
            "GRANTS {} dir(s) that are not plain subdirectories of {}: {}{more}",
            esc.len(),
            shorten(&show_path(&self.anchor.dir), 56),
            shown.join(", ")
        )
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
    fn kickoff_parses_and_is_the_trailing_positional_prompt() {
        let text = r#"{ "fleets": { "f": { "agents": [
          { "name": "a", "cmd": ["claude"], "prompt": "P", "kickoff": "go now" }
        ] } } }"#;
        let f = parse(text).unwrap();
        let a = &f.get("f").unwrap().agents[0];
        assert_eq!(a.kickoff.as_deref(), Some("go now"));
        // The kickoff is LAST — after every flag — so claude reads it as the first
        // user message and starts working immediately.
        let argv = a.args(&["/ctx".to_string()]);
        assert_eq!(
            argv,
            s(&[
                "claude",
                "--add-dir",
                "/ctx",
                "--append-system-prompt",
                "P",
                "go now"
            ])
        );
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
    fn spawning_is_a_declared_capability_defaulting_to_off() {
        // Creating teammates used to be implied by having ctl access at all, so
        // every agent in a fleet could do it. In a seven-agent review fleet that
        // meant all seven, when only the lead should.
        let f = parse(
            r#"{ "fleets": { "a": { "agents": [
                 { "name": "lead", "cmd": ["claude"], "can_spawn": true },
                 { "name": "worker", "cmd": ["claude"] }
               ] } } }"#,
        )
        .unwrap();
        let a = f.get("a").unwrap();
        assert_eq!(a.agents[0].can_spawn, Some(true), "the lead declared it");
        assert_eq!(
            a.agents[1].can_spawn, None,
            "a worker that says nothing must not silently get it"
        );
    }

    #[test]
    fn a_fleet_can_declare_the_control_plane() {
        // A coordinating fleet without ctl fails SILENTLY: panes come up, the
        // kickoffs tell them to publish to a bus that does not exist, and nothing
        // happens. Declaring it in the file removes a flag that has to be
        // remembered every launch.
        let f = parse(
            r#"{ "fleets": { "a": { "allow_ctl": true,
                 "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
        )
        .unwrap();
        assert_eq!(f.get("a").unwrap().allow_ctl, Some(true));
    }

    #[test]
    fn a_non_bool_allow_ctl_is_a_clear_error() {
        let err = parse(
            r#"{ "fleets": { "a": { "allow_ctl": "yes", "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
        )
        .unwrap_err();
        assert!(err.contains("allow_ctl"), "unhelpful error: {err}");
    }

    #[test]
    fn a_fleet_can_declare_its_own_trust_posture() {
        // A fleet is launched to run hands-off, so it needs a posture — and a CLI
        // flag typed on every launch is one you eventually get wrong. Declared in
        // the file it lives with the roster and shows up in a diff.
        let f = parse(
            r#"{ "fleets": { "a": { "trust": "accept",
                 "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
        )
        .unwrap();
        assert_eq!(f.get("a").unwrap().trust.as_deref(), Some("accept"));
    }

    #[test]
    fn a_fleet_without_a_trust_key_declares_nothing() {
        let f =
            parse(r#"{ "fleets": { "a": { "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#)
                .unwrap();
        assert_eq!(f.get("a").unwrap().trust, None);
    }

    #[test]
    fn a_non_string_trust_is_a_clear_error() {
        let err = parse(
            r#"{ "fleets": { "a": { "trust": 3, "agents": [{ "name": "x", "cmd": ["claude"] }] } } }"#,
        )
        .unwrap_err();
        assert!(err.contains("trust"), "unhelpful error: {err}");
    }

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

    /// Serializes the tests that move `XDG_CONFIG_HOME`, since the environment
    /// is process-wide and the suite runs in parallel.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn discover_missing_names_both_locations() {
        // Point the *global* location at an empty dir for the duration. Without
        // this the test reads the developer's real `~/.config/amux/fleet.json`
        // and fails the moment they have one — which is a supported, documented
        // setup, so the test was asserting "no global fleet file exists on this
        // machine" rather than the behaviour it means to cover.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // BOTH variables, not just the unix one. `global_path` reads APPDATA on
        // Windows and XDG_CONFIG_HOME elsewhere, so overriding only XDG left this
        // test reading the developer's real %APPDATA%\amux\fleet.json — it was
        // hermetic on exactly one platform, which is the same class of bug the
        // previous fix here was meant to close.
        let prev_appdata = std::env::var_os("APPDATA");
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        let empty = std::env::temp_dir().join(format!("amux-fleet-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &empty);
        std::env::set_var("APPDATA", &empty);

        let td = std::env::temp_dir().join(format!("amux-fleet-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(&td).unwrap();
        let found = discover(&td);

        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_appdata {
            Some(v) => std::env::set_var("APPDATA", v),
            None => std::env::remove_var("APPDATA"),
        }
        let _ = std::fs::remove_dir_all(&empty);

        let err = found.unwrap_err();
        assert!(err.contains(FILE_NAME), "names local: {err}");
        // Names the global location too (the word "then" separates the two).
        assert!(err.contains("then"), "names both: {err}");
        let _ = std::fs::remove_dir_all(&td);
    }

    // --- disclosure: where a fleet file's grants actually land ---------------

    /// A real directory tree for one test, canonicalised the way the code
    /// canonicalises.
    ///
    /// The canonicalisation is not cosmetic: on macOS `/tmp` is a symlink to
    /// `/private/tmp` and on Windows `canonicalize` returns the verbatim
    /// `\\?\C:\…` form, so a test comparing a resolved path against a raw
    /// `temp_dir()` join asserts something the code never produces. A previous
    /// attempt shipped four tests that could not pass on Windows for exactly
    /// this reason, and nobody saw it because only `cargo check` runs there.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("amux-reach-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(real_path(&p).unwrap())
        }
        /// Create (and canonically name) a directory under the fixture.
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(&p).unwrap();
            real_path(&p).unwrap()
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn link(target: &Path, at: &Path) {
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    fn no_stores() -> Stores {
        Stores::default()
    }

    fn probe(anchor: &Path, base: &Path, raw: &str) -> Grant {
        Grant::probe(anchor, base, Field::AddDir, raw, &no_stores())
    }

    /// One agent, one `add_dirs` list — the shape most of these tests need.
    fn one_agent_fleet(name: &str, cmd: &str, cwd: Option<&str>, dirs: &[&str]) -> Fleet {
        Fleet {
            grid: None,
            identity: None,
            trust: None,
            allow_ctl: None,
            agents: vec![Agent {
                name: name.to_string(),
                cmd: vec![cmd.to_string()],
                cwd: cwd.map(str::to_string),
                add_dirs: s(dirs),
                ..Default::default()
            }],
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_out_of_the_tree_is_disclosed_where_it_really_lands() {
        // THE bug that sank the previous attempt: it banned ".." lexically, and
        // its own test showed a symlink walking straight out of the tree and
        // reporting as inside. Classification is made on the resolved path.
        let t = Tmp::new("symout");
        let base = t.dir("proj");
        let secrets = t.dir("secrets");
        link(&secrets, &base.join("looks-local"));
        let g = probe(&base, &base, "./looks-local");
        assert_eq!(
            g.reach,
            Reach::Outside,
            "a symlink out of the tree must not read as inside: {g:?}"
        );
        assert_eq!(g.real.as_deref(), Some(secrets.as_path()));
        assert!(!g.is_quiet());
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_that_stays_in_the_tree_is_still_quiet() {
        // The mirror case. Following symlinks must not turn into "every alias is
        // suspicious" — a banner that shouts at ordinary layouts is one people
        // stop reading.
        let t = Tmp::new("symin");
        let base = t.dir("proj");
        let real = t.dir("proj/real");
        link(&real, &base.join("alias"));
        let g = probe(&base, &base, "./alias");
        assert_eq!(g.reach, Reach::Inside);
        assert!(g.is_quiet(), "an in-tree alias must not be loud: {g:?}");
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_that_cannot_be_followed_is_unverifiable_not_assumed_local() {
        // A dangling link placed at <base>/x would otherwise be "placed" at its
        // own location and reported inside — and the moment its target exists,
        // the grant is wherever that is.
        let t = Tmp::new("dangle");
        let base = t.dir("proj");
        link(Path::new("/no-such-target-amux"), &base.join("ghost"));
        let g = probe(&base, &base, "./ghost");
        assert_eq!(g.reach, Reach::Unverifiable);
        assert_eq!(g.real, None);
        assert!(!g.is_quiet(), "cannot-tell must never render as inside");
    }

    #[test]
    #[cfg(unix)]
    fn dot_dot_is_applied_by_the_filesystem_not_by_string_surgery() {
        // `link/..` pops the TARGET's parent, not the link's. A lexical
        // normaliser gets this exactly backwards and calls an escape "inside".
        let t = Tmp::new("dotdot");
        let base = t.dir("proj");
        let elsewhere = t.dir("elsewhere");
        let inner = t.dir("elsewhere/inner");
        link(&inner, &base.join("link"));
        let g = probe(&base, &base, "./link/..");
        assert_eq!(g.real.as_deref(), Some(elsewhere.as_path()), "got {g:?}");
        assert_eq!(g.reach, Reach::Outside);
    }

    #[test]
    fn a_missing_tail_that_walks_up_is_unverifiable() {
        // Where `<missing>/..` lands depends on a directory that is not there.
        let t = Tmp::new("misstail");
        let base = t.dir("proj");
        let g = probe(&base, &base, "./not-created/..");
        assert_eq!(g.reach, Reach::Unverifiable, "got {g:?}");
    }

    #[test]
    fn a_path_that_does_not_exist_yet_is_placed_but_never_quiet() {
        // amux does no tilde or env expansion, so `"~/notes"` becomes
        // `<base>/~/notes`: a path that reads to a human as "my home directory"
        // and reaches nothing. Folding it into a terse "N inside" count is a
        // positive false statement about the entry most likely to be wrong.
        let t = Tmp::new("missing");
        let base = t.dir("proj");
        let g = probe(&base, &base, "~/notes");
        assert_eq!(g.reach, Reach::Inside);
        assert!(!g.exists);
        assert!(
            !g.is_quiet(),
            "a path that is not there must be named, not counted"
        );
        assert_eq!(g.tag(), "MISSING");
        assert!(g.note().contains("does not exist"));
    }

    #[test]
    fn the_readme_sibling_checkout_example_is_disclosed_not_refused() {
        // README:214 is `"add_dirs": ["../shared", "./specs"]`. A sibling
        // checkout is a documented, legitimate use; two previous attempts broke
        // this example to close the hole, which is how a control gets switched
        // off. It is shown, not refused.
        let t = Tmp::new("readme");
        let base = t.dir("proj");
        let shared = t.dir("shared");
        let specs = t.dir("proj/specs");
        let g_shared = probe(&base, &base, "../shared");
        let g_specs = probe(&base, &base, "./specs");
        assert_eq!(g_shared.reach, Reach::Outside);
        assert_eq!(g_shared.real.as_deref(), Some(shared.as_path()));
        assert_eq!(g_specs.reach, Reach::Inside);
        assert!(g_specs.is_quiet());
        // And the child still gets a usable path to the sibling: nothing here
        // refuses, drops, or rewrites it into something that is not that dir.
        assert_eq!(g_shared.given, shared);
        assert_eq!(g_specs.given, specs);
    }

    #[test]
    fn the_child_is_handed_the_path_that_was_disclosed() {
        // The banner and the launch must be ONE computation. Resolving a second
        // time in the spawn is how an acknowledged "inside" became a live grant
        // on a credential store when a link was flipped during the Enter window.
        let t = Tmp::new("handed");
        let base = t.dir("proj");
        let real = t.dir("proj/context");
        let g = probe(&base, &base, "./context");
        assert_eq!(g.given, real);
        assert_eq!(Some(g.given.clone()), g.real);
    }

    #[test]
    fn a_credential_store_is_named_even_when_it_is_inside_the_anchor() {
        // The anchor can legitimately be a directory that CONTAINS a credential
        // store (a fleet file kept in $HOME). "Inside" would then be quiet on
        // `./.ssh`, which is the one grant nobody should approve by accident.
        let t = Tmp::new("credin");
        let home = t.dir("home");
        let ssh = t.dir("home/.ssh");
        let stores = Stores::under(&home);
        let g = Grant::probe(&home, &home, Field::AddDir, "./.ssh", &stores);
        assert_eq!(g.reach, Reach::Inside);
        assert_eq!(g.real.as_deref(), Some(ssh.as_path()));
        assert!(
            !g.is_quiet(),
            "a credential store must never be counted quietly"
        );
        assert_eq!(g.tag(), "CREDENTIALS");
        assert!(g.note().contains("SSH private keys"), "{:?}", g.note());
    }

    #[test]
    fn granting_a_parent_of_a_credential_store_says_what_it_encloses() {
        // `--add-dir $HOME` hands over ~/.ssh exactly as surely as naming it,
        // and that is the case an operator is least likely to work out from the
        // path alone.
        let t = Tmp::new("credup");
        let home = t.dir("home");
        t.dir("home/.ssh");
        let stores = Stores::under(&home);
        let g = Grant::probe(&t.0, &t.0, Field::AddDir, "./home", &stores);
        assert!(!g.is_quiet());
        assert!(
            g.note().contains("encloses") && g.note().contains("SSH private keys"),
            "{:?}",
            g.note()
        );
    }

    #[test]
    fn a_credential_store_that_is_not_installed_is_never_cited() {
        // An earlier attempt refused `$HOME/.config` because it "encloses
        // .config/gcloud, which holds Google Cloud credentials" on a machine
        // with no gcloud installed. A warning the operator can see is wrong is a
        // warning they learn to skip.
        let t = Tmp::new("nostore");
        let home = t.dir("home");
        t.dir("home/.ssh");
        let cfg = t.dir("home/.config");
        let stores = Stores::under(&home);
        assert_eq!(stores.describe(Some(&cfg)), None);
    }

    #[test]
    fn the_anchor_is_the_repo_the_fleet_file_is_checked_into() {
        // Measured failure of anchoring on the fleet file's own directory: a
        // monorepo whose file lives in `tools/` marked every same-repo path
        // OUTSIDE — 18 loud lines for a 6-agent roster, scrolling the trust
        // posture off a 24-row terminal.
        let t = Tmp::new("anchorrepo");
        let repo = t.dir("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let tools = t.dir("repo/tools");
        let api = t.dir("repo/packages/api");
        let a = anchor_for(&tools, &tools, false, None);
        assert_eq!(a.kind, AnchorKind::Repo);
        assert_eq!(a.dir, repo);
        let g = probe(&a.dir, &tools, "../packages/api");
        assert_eq!(g.reach, Reach::Inside, "same-repo path must not be OUTSIDE");
        assert!(g.is_quiet());
        assert_eq!(g.given, api);
    }

    #[test]
    fn the_global_fleet_file_is_anchored_at_the_directory_you_ran_in() {
        // `~/.config/amux/fleet.json`'s own directory holds no project by
        // construction, so anchoring there makes 100% of paths OUTSIDE — a tag
        // on every line carries no information at all.
        let t = Tmp::new("anchorglobal");
        let cfgdir = t.dir("cfg/amux");
        let work = t.dir("work");
        let a = anchor_for(&cfgdir, &work, true, None);
        assert_eq!(a.kind, AnchorKind::InvokingDir);
        assert_eq!(a.dir, work);
    }

    #[test]
    fn the_anchor_never_swallows_the_home_directory() {
        // A home directory that happens to be a git repo would make "inside"
        // cover ~/.ssh, i.e. mean nothing.
        let t = Tmp::new("anchorhome");
        let home = t.dir("home");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        let proj = t.dir("home/proj");
        let a = anchor_for(&proj, &proj, false, Some(&home));
        assert_eq!(a.kind, AnchorKind::FleetDir);
        assert_eq!(a.dir, proj);
    }

    #[test]
    fn identical_grants_across_agents_are_disclosed_once() {
        // Measured: ten agents each carrying the documented
        // `["../shared","../protos"]` printed twenty near-identical loud lines
        // for two facts, wrapped on 80 columns, and pushed the posture and
        // can-spawn lines off the screen the Enter was asked on.
        let t = Tmp::new("dedupe");
        let base = t.dir("proj");
        t.dir("shared");
        t.dir("protos");
        let agents: Vec<Agent> = (0..10)
            .map(|i| Agent {
                name: format!("a{i}"),
                cmd: s(&["claude"]),
                add_dirs: s(&["../shared", "../protos"]),
                ..Default::default()
            })
            .collect();
        let fleet = Fleet {
            grid: None,
            identity: None,
            trust: None,
            allow_ctl: None,
            agents,
        };
        let anchor = Anchor {
            dir: base.clone(),
            kind: AnchorKind::FleetDir,
        };
        let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
        let lines = plan.banner_lines();
        let loud: Vec<&String> = lines.iter().filter(|l| l.contains("OUTSIDE")).collect();
        assert_eq!(
            loud.len(),
            2,
            "one line per directory, not per agent: {lines:#?}"
        );
        assert!(
            loud[0].contains("a0") && loud[0].contains("+7"),
            "{}",
            loud[0]
        );
        // And the whole banner stays inside a terminal's worth of rows.
        assert!(lines.len() <= 8, "banner too tall: {lines:#?}");
    }

    #[test]
    fn the_verdict_names_the_destinations_not_just_a_count() {
        // The banner scrolls; the line printed immediately before the blocking
        // Enter does not. A surviving summary that reports a bare count is the
        // one line an operator is guaranteed to read and the one line that tells
        // them nothing.
        let t = Tmp::new("verdict");
        let base = t.dir("proj");
        let shared = t.dir("shared");
        let fleet = one_agent_fleet("lead", "claude", None, &["../shared", "./in"]);
        t.dir("proj/in");
        let anchor = Anchor {
            dir: base.clone(),
            kind: AnchorKind::FleetDir,
        };
        let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
        let v = plan.verdict();
        assert!(v.contains("GRANTS 1"), "{v}");
        assert!(
            v.contains(shared.file_name().unwrap().to_str().unwrap()),
            "the verdict must name where the grant goes: {v}"
        );
    }

    #[test]
    fn a_fleet_with_nothing_to_disclose_says_so_plainly() {
        let t = Tmp::new("clean");
        let base = t.dir("proj");
        t.dir("proj/specs");
        let fleet = one_agent_fleet("lead", "claude", None, &["./specs"]);
        let anchor = Anchor {
            dir: base.clone(),
            kind: AnchorKind::FleetDir,
        };
        let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
        assert!(plan
            .verdict()
            .starts_with("every agent dir resolves inside"));
        assert!(!plan.banner_lines().iter().any(|l| l.contains("OUTSIDE")));
    }

    #[test]
    fn an_agent_that_does_not_run_an_agent_cli_is_disclosed_as_opaque() {
        // `["sh","-c","claude --add-dir ~/.ssh"]` grants read access to the SSH
        // keys through a channel amux does not parse and should not: the direct
        // `--add-dir` in `cmd` IS refused by vet_spawn_argv, which makes the
        // shell wrapper a trap rather than an obscure edge. amux cannot close
        // it, so it says so on the banner instead of implying completeness.
        let t = Tmp::new("opaque");
        let base = t.dir("proj");
        let mut fleet = one_agent_fleet("one", "sh", None, &[]);
        fleet.agents.push(Agent {
            name: "two".to_string(),
            cmd: s(&["claude"]),
            ..Default::default()
        });
        let anchor = Anchor {
            dir: base.clone(),
            kind: AnchorKind::FleetDir,
        };
        let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
        assert_eq!(plan.agents[0].opaque.as_deref(), Some("sh"));
        assert_eq!(plan.agents[1].opaque, None);
        assert!(plan
            .banner_lines()
            .iter()
            .any(|l| l.contains("do not run an agent CLI") && l.contains("one")));
    }

    #[test]
    fn sanitize_defangs_controls_and_bidi_but_leaves_a_windows_path_alone() {
        let out = sanitize("clear\x1b[2Jhere\x07\nand\u{202e}back\u{2028}x\u{7f}");
        assert!(!out.chars().any(|c| c.is_control()), "{out}");
        assert!(out.contains("\\u{1b}") && out.contains("\\u{7}") && out.contains("\\u{a}"));
        assert!(
            out.contains("\\u{202e}"),
            "a bidi override reorders a path: {out}"
        );
        assert!(out.contains("\\u{2028}"), "{out}");
        assert!(out.contains("\\u{7f}"), "{out}");
        // Backslashes are NOT doubled: that mangles every Windows path in the
        // banner, on the platform with the longest paths.
        assert_eq!(sanitize(r"C:\Users\dev\repo"), r"C:\Users\dev\repo");
        assert_eq!(sanitize(r"\\?\C:\proj"), r"\\?\C:\proj");
        assert_eq!(sanitize("ordinary-name"), "ordinary-name");
    }

    #[test]
    fn shorten_elides_the_middle_so_the_destination_survives() {
        // Cutting the tail hides where a grant goes, which is the one fact the
        // line exists to carry.
        let long = format!("/a/{}/secrets-here", "x".repeat(300));
        let out = shorten(&long, 40);
        assert!(out.chars().count() <= 40, "{out}");
        assert!(out.ends_with("secrets-here"), "{out}");
        assert!(out.starts_with("/a/"), "{out}");
        assert!(out.contains('…'), "an elision must be visible: {out}");
        assert_eq!(shorten("short", 40), "short");
    }

    #[test]
    fn no_file_supplied_string_can_inject_an_escape_into_the_banner() {
        // Every one of these arrives through the `json` parser, which decodes
        // `\u001b` into a real ESC.
        let t = Tmp::new("inject");
        let base = t.dir("proj");
        let text = r#"{"fleets":{"f":{"identity":"\u001b[31mid",
          "agents":[{"name":"ev\u001b[2J\u001b[Hil","cmd":["claude"],
          "add_dirs":["./\u001b[2Ktrick","\u202egnp.txt"],
          "identity":"\u001bbad"}]}}}"#;
        let fleets = parse(text).unwrap();
        let fleet = fleets.get("f").unwrap();
        assert!(
            fleet.agents[0].name.contains('\u{1b}'),
            "the parser really does decode an escape - otherwise this test proves nothing"
        );
        let anchor = Anchor {
            dir: base.clone(),
            kind: AnchorKind::FleetDir,
        };
        let plan = Plan::build(fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
        let mut all = plan.banner_lines();
        all.push(plan.verdict());
        all.push(plan.agents[0].label.clone());
        for line in &all {
            assert!(
                !line.chars().any(|c| c.is_control()),
                "raw control character reached the banner: {line:?}"
            );
            assert!(
                !line.contains('\u{202e}'),
                "bidi override reached the banner: {line:?}"
            );
        }
        assert!(all.iter().any(|l| l.contains("\\u{1b}")), "{all:#?}");
    }

    #[test]
    fn one_huge_path_cannot_scroll_the_rest_of_the_disclosure_away() {
        let t = Tmp::new("flood");
        let base = t.dir("proj");
        let huge = format!("./{}", "a".repeat(40_000));
        let fleet = one_agent_fleet("lead", "claude", None, &[&huge, "../out"]);
        let anchor = Anchor {
            dir: base.clone(),
            kind: AnchorKind::FleetDir,
        };
        let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
        for line in plan.banner_lines() {
            assert!(
                line.chars().count() < 600,
                "banner line runaway: {}",
                line.len()
            );
        }
    }

    #[test]
    fn a_credential_store_cannot_be_pushed_off_the_banner_by_noise() {
        // The banner is capped, so a roster could otherwise bury
        // `--add-dir ~/.ssh` behind a wall of innocuous outside directories and
        // push it past the cut - the same trick as burying a flag in `cmd` and
        // hoping the review skims.
        let t = Tmp::new("bury");
        let base = t.dir("proj");
        let home = t.dir("home");
        t.dir("home/.ssh");
        let stores = Stores::under(&home);
        let mut dirs: Vec<String> = (0..20)
            .map(|i| {
                t.dir(&format!("noise{i}"));
                format!("../noise{i}")
            })
            .collect();
        dirs.push("../home/.ssh".to_string());
        let refs: Vec<&str> = dirs.iter().map(String::as_str).collect();
        let fleet = one_agent_fleet("lead", "claude", None, &refs);
        let anchor = Anchor {
            dir: base.clone(),
            kind: AnchorKind::FleetDir,
        };
        let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &stores);
        let lines = plan.banner_lines();
        assert!(
            lines[1].contains("CREDENTIALS") && lines[1].contains("SSH private keys"),
            "the worst grant must be the first one printed: {lines:#?}"
        );
        assert!(
            plan.verdict().contains(".ssh"),
            "and it must be in the line the prompt sits under: {}",
            plan.verdict()
        );
    }

    #[test]
    fn a_sibling_whose_name_merely_starts_with_the_anchor_is_outside() {
        // `/repo_secrets` is not inside `/repo`. A string `starts_with` says it
        // is; `Path::starts_with` is component-wise and does not.
        let t = Tmp::new("prefix");
        let base = t.dir("proj");
        let sneaky = t.dir("proj_secrets");
        let g = probe(&base, &t.0, "./proj_secrets");
        assert_eq!(g.reach, Reach::Outside, "got {g:?}");
        assert_eq!(g.real.as_deref(), Some(sneaky.as_path()));
    }

    #[test]
    fn an_absolute_path_inside_the_anchor_is_still_inside() {
        // Including through a symlinked route: both sides are canonicalised, so
        // macOS's `/tmp -> /private/tmp` is not a false alarm.
        let t = Tmp::new("abs");
        let base = t.dir("proj");
        let inner = t.dir("proj/specs");
        let g = probe(&base, &base, inner.to_str().unwrap());
        assert_eq!(g.reach, Reach::Inside);
        assert!(g.is_quiet());
    }
}
