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
//! upgrade of the mod, and a hand edit is restored. `atrium mod install`
//! creates it and leaves a marker in it ([`MARKER`]); after that a launch
//! keeps it current by itself ([`heal_at`]), saying so in one line when it
//! rewrote something. A folder without the marker (a developer's checkout
//! named by `ATRIUM_MOD`, an install from before 0.48) is loaded as it is
//! and never written to.
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
    ("hooks/health.ts", include_str!("../mod/hooks/health.ts")),
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
/// is written, whole ([`write_whole`]). The folder is regenerated from the
/// binary, never merged. Last, the [`MARKER`] is written: it is what makes
/// the folder one a later launch may keep current ([`heal_at`]).
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
        write_whole(&path, text)?;
        done.written.push(rel);
    }
    write_whole(&dir.join(MARKER), &marker_text())?;
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

// ---- The breaker's memory, and the files healing (PLAN 0.48.0, S2) ----
//
// When the session's circuit breaker trips ([`crate::modstate`]), atrium stops
// loading the mod into new claude panes and writes one small record under the
// per-user state root, keyed by what it tripped against: the Claude Code
// version and the mod (its version and a hash of its embedded files). The
// next launch reads it. Same Claude Code and same mod: the mod stays off, one
// line says so and how to retry. Either changed: the mod is tried again, and
// the record is cleared by the first pane whose mod says hello.
//
// Nothing a pane wrote is in the record: the reason is atrium's own sentence
// and the versions are the ones atrium probed and embedded. A pane cannot
// reach the file through the control plane at all. The record never touches
// the config: `"mod": false` is the user's switch and means off and silent.

/// The record's file name under the state root.
pub const RECORD_FILE: &str = "mod-breaker.json";

/// The most of the record file that is read; a real one is under 400 bytes.
const RECORD_MAX_BYTES: u64 = 4096;

/// The most of a version or a reason a record may hold.
const RECORD_TEXT_MAX: usize = 200;

/// A hash of the mod built into this binary: FNV-1a (64 bits, 16 hex digits)
/// over every file's path and text, in [`FILES`] order, with line endings
/// folded so a checkout's `\r\n` and a release's `\n` hash alike. It tells
/// one build's mod from another's when the version number did not move; it is
/// change detection over atrium's own files, not a security boundary.
pub fn embedded_hash() -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for (rel, text) in FILES {
        eat(rel.as_bytes());
        eat(&[0]);
        for b in text.bytes().filter(|b| *b != b'\r') {
            eat(&[b]);
        }
        eat(&[0]);
    }
    format!("{h:016x}")
}

/// What a tripped breaker left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TripRecord {
    /// What `claude --version` printed when it tripped.
    pub claude: String,
    /// The mod's version then ([`embedded_version`]).
    pub mod_version: String,
    /// The mod's hash then ([`embedded_hash`]).
    pub mod_hash: String,
    /// When (wall-clock ms).
    pub at_ms: u64,
    /// Why, in atrium's own words.
    pub reason: String,
}

impl TripRecord {
    /// A record for a trip now, against this binary's mod.
    pub fn now(claude: &str, reason: &str, at_ms: u64) -> TripRecord {
        TripRecord {
            claude: record_text(claude),
            mod_version: record_text(&embedded_version()),
            mod_hash: embedded_hash(),
            at_ms,
            reason: record_text(reason),
        }
    }

    /// Does it describe this Claude Code and this binary's mod?
    pub fn matches(&self, claude: &str) -> bool {
        self.claude == record_text(claude) && self.mod_matches()
    }

    /// Does it describe this binary's mod (its version and its hash)?
    pub fn mod_matches(&self) -> bool {
        self.mod_version == record_text(&embedded_version()) && self.mod_hash == embedded_hash()
    }

    fn to_json(&self) -> String {
        let s = |v: &str| json::Value::String(v.to_string());
        let pairs: Vec<(String, json::Value)> = vec![
            ("v".to_string(), json::Value::Number(json::Number::Int(1))),
            ("claude".to_string(), s(&self.claude)),
            ("mod".to_string(), s(&self.mod_version)),
            ("mod_hash".to_string(), s(&self.mod_hash)),
            (
                "at_ms".to_string(),
                json::Value::Number(json::Number::Int(
                    i64::try_from(self.at_ms).unwrap_or(i64::MAX),
                )),
            ),
            ("reason".to_string(), s(&self.reason)),
        ];
        json::Value::Object(pairs.into_iter().collect()).to_string()
    }
}

/// A record's text field: scrubbed of control characters, trimmed, cut.
fn record_text(s: &str) -> String {
    crate::modstate::text_safe(s)
        .trim()
        .chars()
        .take(RECORD_TEXT_MAX)
        .collect()
}

/// What the state root holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordRead {
    /// No record: the breaker never tripped, or was cleared.
    None,
    /// A file is there and is not a record this atrium wrote (cut short,
    /// hand-edited into nonsense, another version's): ignored, as if absent.
    Unreadable,
    Tripped(TripRecord),
}

/// Where the record lives under `root`.
pub fn record_path(root: &Path) -> PathBuf {
    root.join(RECORD_FILE)
}

/// Read the record under `root`. Strict: version 1, every field present and
/// of its type, a hash of sixteen hex digits, nothing over its bound, the
/// whole file under [`RECORD_MAX_BYTES`]. Anything else is
/// [`RecordRead::Unreadable`], which every caller treats as no record, so a
/// damaged file can neither keep the mod off nor crash a launch.
pub fn read_record(root: &Path) -> RecordRead {
    use std::io::Read;
    let path = record_path(root);
    let Ok(file) = std::fs::File::open(&path) else {
        return RecordRead::None;
    };
    let mut text = String::new();
    if file
        .take(RECORD_MAX_BYTES + 1)
        .read_to_string(&mut text)
        .is_err()
        || text.len() as u64 > RECORD_MAX_BYTES
    {
        return RecordRead::Unreadable;
    }
    parse_record(&text).map_or(RecordRead::Unreadable, RecordRead::Tripped)
}

fn parse_record(text: &str) -> Option<TripRecord> {
    let v = json::parse(text).ok()?;
    if v.get("v")?.as_i64()? != 1 {
        return None;
    }
    let field = |k: &str| -> Option<String> {
        let s = v.get(k)?.as_str()?;
        (s.chars().count() <= RECORD_TEXT_MAX && s == record_text(s)).then(|| s.to_string())
    };
    let mod_hash = field("mod_hash")?;
    if mod_hash.len() != 16 || !mod_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let claude = field("claude").filter(|c| !c.is_empty())?;
    Some(TripRecord {
        claude,
        mod_version: field("mod")?,
        mod_hash,
        at_ms: u64::try_from(v.get("at_ms")?.as_i64()?).ok()?,
        reason: field("reason")?,
    })
}

/// Write the record under `root` (created if need be), whole: to a sibling
/// file first, then renamed over, so a reader never sees half of one.
pub fn write_record(root: &Path, record: &TripRecord) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let path = record_path(root);
    let tmp = root.join(format!("{RECORD_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, record.to_json())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write {}: {e}", path.display())
    })
}

/// Remove the record under `root`. `true` when one was there.
pub fn clear_record(root: &Path) -> bool {
    std::fs::remove_file(record_path(root)).is_ok()
}

/// What a launch found, for the one line it says and for what panes get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// The session does not use the mod (`"mod": false`, a fleet's): nothing
    /// was read, nothing healed, nothing said.
    Off,
    /// No record: the mod loads as usual.
    Clean,
    /// A record for this very Claude Code and mod: the mod stays off.
    Tripped(TripRecord),
    /// A record, and `claude --version` gave no answer in time: the version
    /// is unknown, which is not the same as changed. The mod stays off.
    TrippedUnknown(TripRecord),
    /// A record for another Claude Code or another mod, or for files this
    /// launch has just rewritten: the mod is tried again, and the record
    /// goes when a pane's mod says hello.
    Retrying(TripRecord),
}

/// Decide a launch from the record, the Claude Code version (`None`:
/// `claude --version` gave none in time) and whether this launch's heal
/// rewrote files of the installed mod (then the mod on disk is not the one
/// that tripped, whatever the versions say). Pure.
pub fn launch_of(enabled: bool, record: RecordRead, claude: Option<&str>, healed: bool) -> Launch {
    if !enabled {
        return Launch::Off;
    }
    match record {
        RecordRead::None | RecordRead::Unreadable => Launch::Clean,
        RecordRead::Tripped(r) if healed => Launch::Retrying(r),
        RecordRead::Tripped(r) => match claude {
            Some(c) if r.matches(c) => Launch::Tripped(r),
            Some(_) => Launch::Retrying(r),
            // Unknown is not "changed": a probe that timed out must not
            // bring a broken mod back.
            None if r.mod_matches() => Launch::TrippedUnknown(r),
            None => Launch::Retrying(r),
        },
    }
}

/// The one line a launch says of the breaker, if it has one to say.
pub fn launch_line(launch: &Launch, now_ms: u64) -> Option<String> {
    let ago = |r: &TripRecord| crate::ctl::humanize_idle(now_ms.saturating_sub(r.at_ms));
    match launch {
        Launch::Off | Launch::Clean => None,
        Launch::Tripped(r) => Some(format!(
            "mod: off since {} ago ({}); Claude Code {} and mod {} are unchanged, so it stays \
             off: `atrium mod retry` tries it again, `atrium mod status` says more",
            ago(r),
            r.reason,
            r.claude,
            r.mod_version,
        )),
        Launch::TrippedUnknown(r) => Some(format!(
            "mod: off since {} ago ({}); the mod is unchanged and claude did not say its \
             version, so it stays off: `atrium mod retry` tries it again",
            ago(r),
            r.reason,
        )),
        Launch::Retrying(r) => Some(format!(
            "mod: backed out {} ago against Claude Code {} and mod {}; that has changed, so \
             it is tried again now",
            ago(r),
            r.claude,
            r.mod_version,
        )),
    }
}

/// The marker `atrium mod install` leaves in the folder it wrote: the one
/// thing that says "this folder is atrium's to rewrite".
pub const MARKER: &str = ".atrium-mod";

/// The marker's text for this binary's mod.
fn marker_text() -> String {
    format!(
        "{{\"mod\":\"{}\",\"hash\":\"{}\"}}\n",
        embedded_version(),
        embedded_hash()
    )
}

/// What kind of folder `dir` is to a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Folder {
    /// No plugin manifest: not a mod. Nothing is written, nothing injected.
    Absent,
    /// A plugin manifest and no marker: a mod atrium did not install here
    /// with this scheme. An install from before 0.48, or a developer's own
    /// checkout named by `ATRIUM_MOD`. Loaded as it is, never rewritten.
    Unmarked,
    /// A manifest and the marker `atrium mod install` wrote: atrium's own
    /// folder, kept current by the launch.
    Ours,
}

/// [`Folder`] of `dir`.
pub fn folder_at(dir: &Path) -> Folder {
    if !dir.join(MANIFEST).is_file() {
        Folder::Absent
    } else if dir.join(MARKER).is_file() {
        Folder::Ours
    } else {
        Folder::Unmarked
    }
}

/// Is `path`, or the folder it sits in under `dir`, a symbolic link or a
/// junction? The heal writes only into atrium's own folder, and a link could
/// point it at someone else's file.
fn through_a_link(dir: &Path, rel: &str) -> bool {
    let is_link = |p: &Path| {
        std::fs::symlink_metadata(p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    };
    let path = dir.join(rel);
    is_link(&path)
        || path
            .parent()
            .is_some_and(|parent| parent != dir && is_link(parent))
}

/// Write `text` at `path` whole: to a sibling temporary name, then renamed
/// over, so a reader (a claude loading the plugin) never sees half a file and
/// a write that fails leaves the old file as it was.
fn write_whole(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.atrium-tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write {}: {e}", path.display())
    })
}

/// What the launch heal did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heal {
    /// Not atrium's folder to write ([`Folder::Absent`]): nothing done.
    NotInstalled,
    /// A mod without the marker ([`Folder::Unmarked`]): left exactly as it is.
    Unmarked,
    /// Every file already matched.
    Current,
    /// These files were missing or differed, and were rewritten.
    Rewrote(Vec<&'static str>),
    /// Some files could not be written: the folder now holds a mix of this
    /// binary's files and older ones. `failed` names each with why.
    Broken {
        written: Vec<&'static str>,
        failed: Vec<(&'static str, String)>,
    },
}

/// The launch heal: in a folder `atrium mod install` wrote ([`Folder::Ours`]),
/// rewrite every file that is missing or differs from the copy built into
/// this binary. It goes on past a file it cannot write, so the result names
/// every one; each file is replaced whole or not at all ([`write_whole`]); a
/// file reached through a symbolic link is refused, not followed. Every path
/// written is `dir` joined with one of [`FILES`]' own names. Any other
/// folder is left untouched.
pub fn heal_at(dir: &Path) -> Heal {
    match folder_at(dir) {
        Folder::Absent => return Heal::NotInstalled,
        Folder::Unmarked => return Heal::Unmarked,
        Folder::Ours => {}
    }
    let mut written = Vec::new();
    let mut failed = Vec::new();
    for (rel, text) in FILES {
        let path = dir.join(rel);
        if through_a_link(dir, rel) {
            failed.push((
                *rel,
                "it is a symbolic link, which is not written through".to_string(),
            ));
            continue;
        }
        if std::fs::read_to_string(&path).ok().as_deref() == Some(*text) {
            continue;
        }
        match write_whole(&path, text) {
            Ok(()) => written.push(*rel),
            Err(e) => failed.push((*rel, e)),
        }
    }
    if !failed.is_empty() {
        return Heal::Broken { written, failed };
    }
    // The marker follows the files: it names what the folder now holds.
    let marker = dir.join(MARKER);
    if std::fs::read_to_string(&marker).ok().as_deref() != Some(marker_text().as_str()) {
        let _ = write_whole(&marker, &marker_text());
    }
    if written.is_empty() {
        Heal::Current
    } else {
        Heal::Rewrote(written)
    }
}

/// The line a heal says, if it has one, and whether the folder may still be
/// loaded into panes this session.
pub fn heal_line(heal: &Heal, dir: &Path) -> (Option<String>, bool) {
    match heal {
        Heal::NotInstalled | Heal::Current => (None, true),
        Heal::Unmarked => (
            Some(format!(
                "mod: the mod at {} was not installed by this atrium (no {MARKER} marker); it \
                 is loaded as it is. Run `atrium mod install` to have atrium keep it current",
                dir.display()
            )),
            true,
        ),
        Heal::Rewrote(written) => (
            Some(format!(
                "mod: {} file{} at {} refreshed from this atrium ({})",
                written.len(),
                if written.len() == 1 { "" } else { "s" },
                dir.display(),
                embedded_version()
            )),
            true,
        ),
        Heal::Broken { failed, .. } => (
            Some(format!(
                "mod: {} file{} at {} could not be refreshed ({}: {}); the folder may hold a \
                 mix of versions, so claude panes launch without the mod this session. Fix the \
                 file and run `atrium mod install`",
                failed.len(),
                if failed.len() == 1 { "" } else { "s" },
                dir.display(),
                failed[0].0,
                failed[0].1
            )),
            false,
        ),
    }
}

/// Set once this session's first claude pane ran [`launch_once`].
static LAUNCHED: OnceLock<()> = OnceLock::new();
/// Why the mod is off for the rest of this session; `None` while it is on.
static TRIPPED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// A record for other versions was found: the first healthy hello clears it.
static RETRYING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The launch heal left the folder inconsistent: it is not loaded this session.
static UNLOADABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Why the mod is off for this session, if the breaker tripped (now, or on a
/// record from an earlier one).
pub fn session_tripped() -> Option<String> {
    TRIPPED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Did this launch's heal leave the installed folder inconsistent?
pub fn session_unloadable() -> bool {
    UNLOADABLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// The folder a claude pane is told to load: `installed` when the session
/// uses the mod and nothing holds it back (the breaker tripped, or the
/// launch heal left the folder inconsistent), else none. Pure.
pub fn inject_dir(enabled: bool, held_back: bool, installed: Option<&Path>) -> Option<&Path> {
    installed.filter(|_| enabled && !held_back)
}

/// Once per session, before the first claude pane gets the mod: heal the
/// installed files and read the breaker's record. `claude` answers what
/// `claude --version` prints, and is asked only when there is a record to
/// hold it against (the probe starts a process; a session with no record
/// never pays for it). Returns the lines to say, at most two. A session that
/// does not use the mod reads nothing, writes nothing and says nothing.
pub fn launch_once(claude: impl FnOnce() -> Option<String>) -> Vec<String> {
    let mut first = false;
    LAUNCHED.get_or_init(|| first = true);
    if !first || !session_enabled() {
        return Vec::new();
    }
    let (lines, state) = launch_at(
        configured_dir().as_deref(),
        crate::session_store::state_root().as_deref(),
        claude,
        crate::session_store::now_ms(),
    );
    if !state.loadable {
        UNLOADABLE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    match state.launch {
        Launch::Tripped(r) | Launch::TrippedUnknown(r) => {
            *TRIPPED.lock().unwrap_or_else(|e| e.into_inner()) = Some(r.reason);
        }
        Launch::Retrying(_) => RETRYING.store(true, std::sync::atomic::Ordering::Relaxed),
        Launch::Off | Launch::Clean => {}
    }
    lines
}

/// What [`launch_at`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launched {
    pub launch: Launch,
    /// May the folder be loaded into panes this session?
    pub loadable: bool,
}

/// [`launch_once`] over a given mod folder and state root: the heal, then
/// the record. Touches nothing else.
pub fn launch_at(
    dir: Option<&Path>,
    root: Option<&Path>,
    claude: impl FnOnce() -> Option<String>,
    now_ms: u64,
) -> (Vec<String>, Launched) {
    let mut lines = Vec::new();
    let heal = dir.map_or(Heal::NotInstalled, heal_at);
    let (line, loadable) = dir.map_or((None, true), |d| heal_line(&heal, d));
    lines.extend(line);
    let Some(root) = root else {
        return (
            lines,
            Launched {
                launch: Launch::Clean,
                loadable,
            },
        );
    };
    let record = read_record(root);
    let version = match record {
        RecordRead::Tripped(_) => claude(),
        RecordRead::None | RecordRead::Unreadable => None,
    };
    let healed = matches!(heal, Heal::Rewrote(_));
    let launch = launch_of(true, record, version.as_deref(), healed);
    lines.extend(launch_line(&launch, now_ms));
    (lines, Launched { launch, loadable })
}

/// The breaker tripped in this session: no claude pane gets the mod from
/// here on. With `remember` (the trip rests on what atrium saw itself) it is
/// also written down against `claude` (what `claude --version` prints) and
/// this binary's mod. Returns whether it was written down: not without
/// `remember`, not when the version is unknown (a record keyed by an unknown
/// version would match nothing, or everything), not without a state root.
/// In a session that does not use the mod this does nothing at all.
pub fn trip(reason: &str, claude: Option<&str>, remember: bool) -> bool {
    if !session_enabled() {
        return false;
    }
    *TRIPPED.lock().unwrap_or_else(|e| e.into_inner()) = Some(record_text(reason));
    RETRYING.store(false, std::sync::atomic::Ordering::Relaxed);
    if !remember {
        return false;
    }
    match crate::session_store::state_root() {
        Some(root) => remember_trip(&root, reason, claude, crate::session_store::now_ms()),
        None => false,
    }
}

/// Write the record of a trip under `root`, when `claude`'s version is known.
pub fn remember_trip(root: &Path, reason: &str, claude: Option<&str>, now_ms: u64) -> bool {
    match claude.map(record_text).filter(|c| !c.is_empty()) {
        Some(claude) => write_record(root, &TripRecord::now(&claude, reason, now_ms)).is_ok(),
        None => false,
    }
}

/// A pane atrium launched with the mod said hello and was granted something.
/// When this session was retrying after an earlier trip, the record is
/// cleared and the line to say comes back, once.
pub fn recovered() -> Option<String> {
    if !RETRYING.swap(false, std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    if session_tripped().is_some() {
        return None;
    }
    recover_at(&crate::session_store::state_root()?)
}

/// Clear the record under `root` and say the mod is back.
pub fn recover_at(root: &Path) -> Option<String> {
    clear_record(root).then(|| {
        format!(
            "mod: back on; mod {} works with this Claude Code",
            embedded_version()
        )
    })
}

/// The lines `atrium mod status` prints about the breaker. `claude` is what
/// the claude a pane would run says of its version, `None` when it gave no
/// answer.
pub fn breaker_lines(record: &RecordRead, claude: Option<&str>, now_ms: u64) -> Vec<String> {
    match record {
        RecordRead::None => vec!["breaker: closed (the mod has not been backed out)".to_string()],
        RecordRead::Unreadable => vec![
            "breaker: closed (a record is there and is not readable; it is ignored and will be \
             overwritten)"
                .to_string(),
        ],
        RecordRead::Tripped(r) => {
            let first = format!(
                "breaker: tripped {} ago against Claude Code {} and mod {} ({}): {}",
                crate::ctl::humanize_idle(now_ms.saturating_sub(r.at_ms)),
                r.claude,
                r.mod_version,
                r.mod_hash,
                r.reason
            );
            let second = match claude {
                Some(c) if r.matches(c) => {
                    "         both are unchanged: claude panes launch without the mod until one \
                     changes, or `atrium mod retry`"
                        .to_string()
                }
                None if r.mod_matches() => {
                    "         the mod is unchanged and claude did not say its version: claude \
                     panes launch without the mod until that is known to have changed, or \
                     `atrium mod retry`"
                        .to_string()
                }
                _ => format!(
                    "         now Claude Code {} and mod {} ({}): the next session tries the \
                     mod again",
                    claude.unwrap_or("(no version)"),
                    embedded_version(),
                    embedded_hash()
                ),
            };
            vec![first, second]
        }
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

    #[test]
    fn the_embedded_hash_is_stable_and_folds_line_endings() {
        let h = embedded_hash();
        assert_eq!(h.len(), 16);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(h, embedded_hash());
        // Every file the hooks module imports is embedded: a file left out of
        // FILES would install a mod that cannot load.
        let register = FILES
            .iter()
            .find(|(p, _)| *p == "hooks/register.ts")
            .map(|(_, t)| *t)
            .expect("register.ts is embedded");
        for line in register.lines().filter(|l| l.contains("from './")) {
            let name = line
                .split("from './")
                .nth(1)
                .and_then(|r| r.split('\'').next())
                .expect("an import path");
            assert!(
                FILES.iter().any(|(p, _)| *p == format!("hooks/{name}.ts")),
                "hooks/{name}.ts is imported by register.ts and not embedded"
            );
        }
    }

    #[test]
    fn a_trip_record_round_trips_and_matches_only_its_own_versions() {
        let root = temp("record");
        assert_eq!(read_record(&root), RecordRead::None);
        let r = TripRecord::now(
            "2.1.325 (Claude Code)",
            "2 claude panes never said hello",
            1_000,
        );
        write_record(&root, &r).expect("write");
        assert_eq!(read_record(&root), RecordRead::Tripped(r.clone()));
        assert!(r.matches("2.1.325 (Claude Code)"));
        assert!(
            r.matches("  2.1.325 (Claude Code)\n"),
            "as the probe prints it"
        );
        assert!(!r.matches("2.1.326 (Claude Code)"), "a new Claude Code");
        let other_mod = TripRecord {
            mod_hash: "0000000000000000".to_string(),
            ..r.clone()
        };
        assert!(
            !other_mod.matches("2.1.325 (Claude Code)"),
            "a changed mod, same version number"
        );
        let other_version = TripRecord {
            mod_version: "0.0.1".to_string(),
            ..r.clone()
        };
        assert!(!other_version.matches("2.1.325 (Claude Code)"));
        assert!(clear_record(&root));
        assert_eq!(read_record(&root), RecordRead::None);
        assert!(!clear_record(&root), "nothing left to clear");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_or_hand_edited_record_is_ignored() {
        let root = temp("badrecord");
        std::fs::create_dir_all(&root).expect("dir");
        let good = TripRecord::now("2.1.325", "why", 5).to_json();
        let bad: Vec<String> = vec![
            String::new(),
            "not json".to_string(),
            "[]".to_string(),
            "{}".to_string(),
            good.replace("\"v\":1", "\"v\":2"),
            good.replace("\"v\":1", "\"v\":\"1\""),
            good.replace(&embedded_hash(), "xyz"),
            good.replace(&embedded_hash(), "zzzzzzzzzzzzzzzz"),
            good.replace("\"at_ms\":5", "\"at_ms\":-5"),
            good.replace("\"at_ms\":5", "\"at_ms\":\"soon\""),
            good.replace("2.1.325", ""),
            good.replace("2.1.325", "2.1\\u001b[31m"),
            good.replace("why", &"y".repeat(RECORD_TEXT_MAX + 1)),
            good[..good.len() / 2].to_string(),
            format!("{good}{}", " ".repeat(RECORD_MAX_BYTES as usize)),
        ];
        for text in bad {
            std::fs::write(record_path(&root), &text).expect("write");
            assert_eq!(read_record(&root), RecordRead::Unreadable, "{text:.80}");
            assert_eq!(
                launch_of(true, read_record(&root), Some("2.1.325"), false),
                Launch::Clean
            );
        }
        // And the good one, to prove the list above fails for its edits alone.
        std::fs::write(record_path(&root), &good).expect("write");
        assert!(matches!(read_record(&root), RecordRead::Tripped(_)));
        // A directory where the file should be is unreadable, not a crash.
        let _ = std::fs::remove_file(record_path(&root));
        std::fs::create_dir_all(record_path(&root)).expect("dir");
        assert!(matches!(
            read_record(&root),
            RecordRead::Unreadable | RecordRead::None
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_launch_stays_tripped_on_the_same_versions_and_retries_when_either_changed() {
        let r = TripRecord::now("2.1.325 (Claude Code)", "why", 0);
        let rec = || RecordRead::Tripped(r.clone());
        assert_eq!(
            launch_of(true, rec(), Some("2.1.325 (Claude Code)"), false),
            Launch::Tripped(r.clone())
        );
        assert_eq!(
            launch_of(true, rec(), Some("2.1.400 (Claude Code)"), false),
            Launch::Retrying(r.clone())
        );
        // No version could be read: unknown is not "changed". The mod is the
        // same one, so it stays off.
        assert_eq!(
            launch_of(true, rec(), None, false),
            Launch::TrippedUnknown(r.clone())
        );
        // The launch has just rewritten the installed files: what tripped is
        // not what is on disk any more, whatever the versions say.
        assert_eq!(
            launch_of(true, rec(), Some("2.1.325 (Claude Code)"), true),
            Launch::Retrying(r.clone())
        );
        let old_mod = TripRecord {
            mod_hash: "00000000deadbeef".to_string(),
            ..r.clone()
        };
        assert_eq!(
            launch_of(
                true,
                RecordRead::Tripped(old_mod.clone()),
                Some("2.1.325 (Claude Code)"),
                false
            ),
            Launch::Retrying(old_mod.clone())
        );
        assert_eq!(
            launch_of(true, RecordRead::Tripped(old_mod.clone()), None, false),
            Launch::Retrying(old_mod),
            "a changed mod is a change, version read or not"
        );
        assert_eq!(
            launch_of(true, RecordRead::None, Some("x"), false),
            Launch::Clean
        );
        assert_eq!(
            launch_of(true, RecordRead::Unreadable, Some("x"), false),
            Launch::Clean
        );
        // `"mod": false` wins over everything, record or not, and says nothing.
        assert_eq!(
            launch_of(false, rec(), Some("2.1.325 (Claude Code)"), true),
            Launch::Off
        );
        assert_eq!(launch_line(&Launch::Off, 0), None);
        assert_eq!(launch_line(&Launch::Clean, 0), None);
        let stays = launch_line(&Launch::Tripped(r.clone()), 3_600_000).expect("a line");
        assert!(
            stays.contains("atrium mod retry") && stays.contains("2.1.325"),
            "{stays}"
        );
        let unknown = launch_line(&Launch::TrippedUnknown(r.clone()), 3_600_000).expect("a line");
        assert!(unknown.contains("did not say its version"), "{unknown}");
        let again = launch_line(&Launch::Retrying(r), 3_600_000).expect("a line");
        assert!(again.contains("tried again"), "{again}");
    }

    #[test]
    fn a_trip_is_written_only_against_a_known_version_and_a_hello_clears_it() {
        let root = temp("remember");
        // No version: a record keyed by nothing would match nothing, or everything.
        assert!(!remember_trip(&root, "why", None, 5));
        assert!(!remember_trip(&root, "why", Some("  "), 5));
        assert_eq!(read_record(&root), RecordRead::None);
        assert!(remember_trip(
            &root,
            "why",
            Some("2.1.325 (Claude Code)"),
            5
        ));
        assert!(matches!(read_record(&root), RecordRead::Tripped(_)));
        // The mod came back: the record goes, and it is said once.
        let line = recover_at(&root).expect("a line");
        assert!(line.contains("back on"), "{line}");
        assert_eq!(read_record(&root), RecordRead::None);
        assert_eq!(
            recover_at(&root),
            None,
            "nothing left to clear, nothing to say"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_a_folder_atrium_installed_into_is_healed() {
        // Not there: nothing is created.
        let never = temp("heal-never");
        assert_eq!(folder_at(&never), Folder::Absent);
        assert_eq!(heal_at(&never), Heal::NotInstalled);
        assert!(!never.exists());
        // A folder that merely holds one of the mod's file names is not a mod.
        std::fs::create_dir_all(&never).expect("dir");
        std::fs::write(never.join("README.md"), "mine").expect("write");
        assert_eq!(heal_at(&never), Heal::NotInstalled);
        assert_eq!(
            std::fs::read_to_string(never.join("README.md"))
                .ok()
                .as_deref(),
            Some("mine")
        );
        assert_eq!(std::fs::read_dir(&never).expect("read").count(), 1);
        let _ = std::fs::remove_dir_all(&never);

        // A mod with no marker: a developer's checkout named by ATRIUM_MOD,
        // or an install from before the marker. Loaded as it is, never
        // rewritten, edits and all.
        let checkout = temp("heal-checkout");
        for (rel, text) in FILES {
            let path = checkout.join(rel);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("dir");
            std::fs::write(&path, text).expect("write");
        }
        std::fs::write(checkout.join("hooks/register.ts"), "// my work in progress").expect("edit");
        assert_eq!(folder_at(&checkout), Folder::Unmarked);
        assert_eq!(heal_at(&checkout), Heal::Unmarked);
        assert_eq!(
            std::fs::read_to_string(checkout.join("hooks/register.ts"))
                .ok()
                .as_deref(),
            Some("// my work in progress")
        );
        assert!(
            !checkout.join(MARKER).exists(),
            "no marker appears by itself"
        );
        let (line, loadable) = heal_line(&Heal::Unmarked, &checkout);
        assert!(line.expect("a line").contains("atrium mod install"));
        assert!(loadable, "it is loaded as it is");
        // The repo's own mod/ is such a folder.
        let repo_mod = Path::new(env!("CARGO_MANIFEST_DIR")).join("mod");
        assert_eq!(folder_at(&repo_mod), Folder::Unmarked);
        // `atrium mod install` is what makes it atrium's.
        install_at(&checkout).expect("install");
        assert_eq!(folder_at(&checkout), Folder::Ours);
        assert!(status_at(&checkout).current);
        let _ = std::fs::remove_dir_all(&checkout);
    }

    #[test]
    fn the_launch_heal_rewrites_what_changed_in_atriums_own_folder() {
        let dir = temp("heal");
        install_at(&dir).expect("install");
        assert_eq!(folder_at(&dir), Folder::Ours);
        // Current: the heal writes nothing.
        assert_eq!(heal_at(&dir), Heal::Current);
        // An edited file and a missing one are both put back.
        std::fs::write(dir.join("hooks/register.ts"), "throw new Error('broken')").expect("edit");
        std::fs::remove_file(dir.join("hooks/health.ts")).expect("remove");
        assert_eq!(
            heal_at(&dir),
            Heal::Rewrote(vec!["hooks/register.ts", "hooks/health.ts"])
        );
        assert!(status_at(&dir).current);
        // A file of the user's own in the folder is not touched, and no
        // temporary file is left behind.
        std::fs::write(dir.join("notes.txt"), "mine").expect("write");
        assert_eq!(heal_at(&dir), Heal::Current);
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.txt"))
                .ok()
                .as_deref(),
            Some("mine")
        );
        let stray: Vec<String> = std::fs::read_dir(dir.join("hooks"))
            .expect("read")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".atrium-tmp"))
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
        // Without its manifest the folder is no longer a mod: left alone.
        std::fs::remove_file(dir.join(MANIFEST)).expect("remove");
        assert_eq!(heal_at(&dir), Heal::NotInstalled);
        assert!(!dir.join(MANIFEST).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_heal_that_cannot_write_every_file_names_each_and_the_folder_is_not_loaded() {
        let dir = temp("heal-broken");
        install_at(&dir).expect("install");
        // Three files differ; two of them cannot be replaced (a non-empty
        // directory sits where each file should be, on every platform).
        for rel in ["hooks/status.ts", "hooks/ctl.ts"] {
            std::fs::remove_file(dir.join(rel)).expect("remove");
            std::fs::create_dir_all(dir.join(rel).join("in-the-way")).expect("dir");
        }
        std::fs::write(dir.join("hooks/guard.ts"), "old").expect("edit");
        let heal = heal_at(&dir);
        let Heal::Broken { written, failed } = &heal else {
            panic!("expected a broken heal, got {heal:?}");
        };
        // It went on past the first failure: the one it could write is
        // written, and both it could not are named.
        assert_eq!(written, &vec!["hooks/guard.ts"]);
        assert_eq!(
            failed.iter().map(|(rel, _)| *rel).collect::<Vec<_>>(),
            vec!["hooks/status.ts", "hooks/ctl.ts"]
        );
        let (line, loadable) = heal_line(&heal, &dir);
        let line = line.expect("a line");
        assert!(
            line.contains("2 files")
                && line.contains("hooks/status.ts")
                && line.contains("without the mod"),
            "{line}"
        );
        assert!(!loadable, "a mix of versions is not loaded");
        // So the launch says so, loads nothing, and a pane gets no folder.
        let state = temp("heal-broken-state");
        let (lines, launched) = launch_at(Some(&dir), Some(&state), || None, 0);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(!launched.loadable);
        assert_eq!(inject_dir(true, !launched.loadable, Some(&dir)), None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_launch_reads_the_record_once_the_files_are_healed() {
        let dir = temp("launch");
        let state = temp("launch-state");
        install_at(&dir).expect("install");
        // No record: nothing to say, and claude is never asked its version.
        let (lines, launched) = launch_at(Some(&dir), Some(&state), || panic!("not asked"), 0);
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(launched.launch, Launch::Clean);
        assert!(launched.loadable);
        // A record for these very versions: off, one line.
        let v = "2.1.325 (Claude Code)";
        assert!(remember_trip(&state, "why", Some(v), 0));
        let (lines, launched) = launch_at(Some(&dir), Some(&state), || Some(v.to_string()), 60_000);
        assert!(matches!(launched.launch, Launch::Tripped(_)));
        assert_eq!(lines.len(), 1, "{lines:?}");
        // The probe timed out: still off.
        let (_, launched) = launch_at(Some(&dir), Some(&state), || None, 60_000);
        assert!(matches!(launched.launch, Launch::TrippedUnknown(_)));
        // The installed files were damaged and this launch put them back:
        // what tripped may have been the damage. Tried again, two lines.
        std::fs::write(dir.join("hooks/register.ts"), "broken").expect("edit");
        let (lines, launched) = launch_at(Some(&dir), Some(&state), || Some(v.to_string()), 60_000);
        assert!(
            matches!(launched.launch, Launch::Retrying(_)),
            "{launched:?}"
        );
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("refreshed") && lines[1].contains("tried again"),
            "{lines:?}"
        );
        // The record is still there until a pane's mod says hello.
        assert!(matches!(read_record(&state), RecordRead::Tripped(_)));
        // No mod folder at all: the record is still read.
        let (_, launched) = launch_at(None, Some(&state), || Some(v.to_string()), 60_000);
        assert!(matches!(launched.launch, Launch::Tripped(_)));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[cfg(unix)]
    #[test]
    fn the_heal_does_not_write_through_a_symbolic_link() {
        let dir = temp("heal-link");
        install_at(&dir).expect("install");
        let outside = temp("heal-link-target");
        std::fs::create_dir_all(&outside).expect("dir");
        let target = outside.join("victim.txt");
        std::fs::write(&target, "untouched").expect("write");
        std::fs::remove_file(dir.join("hooks/status.ts")).expect("remove");
        std::os::unix::fs::symlink(&target, dir.join("hooks/status.ts")).expect("link");
        assert!(matches!(heal_at(&dir), Heal::Broken { .. }));
        assert_eq!(
            std::fs::read_to_string(&target).ok().as_deref(),
            Some("untouched")
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A Windows junction is a link to a folder that needs no privilege to
    /// make: the heal must not write the mod's files through one.
    #[cfg(windows)]
    #[test]
    fn the_heal_does_not_write_through_a_junction() {
        let dir = temp("heal-junction");
        install_at(&dir).expect("install");
        let outside = temp("heal-junction-target");
        std::fs::create_dir_all(&outside).expect("dir");
        std::fs::write(outside.join("status.ts"), "untouched").expect("write");
        std::fs::remove_dir_all(dir.join("hooks")).expect("remove");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dir.join("hooks"))
            .arg(&outside)
            .output()
            .expect("cmd runs");
        assert!(
            made.status.success(),
            "mklink /J: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        let heal = heal_at(&dir);
        assert!(matches!(heal, Heal::Broken { .. }), "{heal:?}");
        assert_eq!(
            std::fs::read_to_string(outside.join("status.ts"))
                .ok()
                .as_deref(),
            Some("untouched")
        );
        assert!(
            !outside.join("register.ts").exists(),
            "nothing was written on the far side of the junction"
        );
        // Remove the junction itself, not what it points at.
        let _ = std::fs::remove_dir(dir.join("hooks"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_pane_gets_the_mod_only_while_the_session_uses_it_and_nothing_holds_it_back() {
        let dir = Path::new("/cfg/mod");
        assert_eq!(inject_dir(true, false, Some(dir)), Some(dir));
        assert_eq!(
            inject_dir(true, true, Some(dir)),
            None,
            "tripped, or unloadable"
        );
        assert_eq!(inject_dir(false, false, Some(dir)), None, "\"mod\": false");
        assert_eq!(inject_dir(false, true, Some(dir)), None);
        assert_eq!(inject_dir(true, false, None), None, "not installed");
        // And so no plugin folder reaches the pane's environment.
        let exe = Path::new("/bin/atrium");
        assert!(pane_env(true, inject_dir(true, true, Some(dir)), None, Some(exe)).is_empty());
        assert!(!pane_env(true, inject_dir(true, false, Some(dir)), None, Some(exe)).is_empty());
    }

    #[test]
    fn the_breaker_lines_say_closed_tripped_unknown_or_about_to_retry() {
        let closed = breaker_lines(&RecordRead::None, Some("2.1.325"), 0);
        assert!(closed[0].contains("closed"), "{closed:?}");
        let ignored = breaker_lines(&RecordRead::Unreadable, Some("2.1.325"), 0);
        assert!(ignored[0].contains("ignored"), "{ignored:?}");
        let r = TripRecord::now(
            "2.1.325",
            "2 claude panes loaded it and never said hello",
            0,
        );
        let same = breaker_lines(&RecordRead::Tripped(r.clone()), Some("2.1.325"), 60_000);
        assert!(
            same[0].contains("tripped") && same[0].contains("never said hello"),
            "{same:?}"
        );
        assert!(same[1].contains("atrium mod retry"), "{same:?}");
        let unknown = breaker_lines(&RecordRead::Tripped(r.clone()), None, 60_000);
        assert!(
            unknown[1].contains("did not say its version") && !unknown[1].contains("tries the mod"),
            "{unknown:?}"
        );
        let changed = breaker_lines(&RecordRead::Tripped(r), Some("2.2.0"), 60_000);
        assert!(changed[1].contains("tries the mod again"), "{changed:?}");
    }
}
