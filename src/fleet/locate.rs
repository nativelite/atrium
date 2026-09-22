//! Finding the fleet file (the project's own first, then the user-global one)
//! and resolving the directories its agents name.

#[cfg(doc)]
use super::anchor_for;
use std::path::{Path, PathBuf};

/// The fleet-file name looked for in the current directory.
pub const FILE_NAME: &str = "atrium.fleet.json";

/// A located fleet file: the path we read and the directory that agent `cwd` /
/// `add_dirs` are resolved against (the file's own directory).
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub path: PathBuf,
    pub dir: PathBuf,
    /// True when this is the user-global file rather than a project-local one.
    ///
    /// Its directory is `~/.config/atrium`, which holds no project by
    /// construction, so it is the wrong thing to measure "inside" against — see
    /// [`anchor_for`], which uses the invoking directory instead.
    pub global: bool,
}

/// The text of the object under `"fleets"."<name>"` in a fleet file, exactly
/// as written (key order, spacing, comments-in-strings intact), or `None` when
/// the file does not hold that fleet. `fleet init` copies a user's template
/// with this rather than re-serializing the parsed value, so the project file
/// reads as the template was written.
pub fn fleet_object_text(text: &str, name: &str) -> Option<String> {
    let root = json::parse(text).ok()?;
    root.get("fleets")?.get(name)?;
    // Walk to the value: find the key inside the "fleets" object at nesting
    // depth 2, then take the balanced braces after it. A JSON scanner that
    // respects strings, since a prompt may contain braces and quotes.
    let bytes = text.as_bytes();
    let key = format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""));
    let mut depth = 0usize;
    let mut i = 0;
    let mut in_str = false;
    let mut fleets_depth: Option<usize> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                // A key or a string value. At depth 1, "fleets" opens the map; at
                // the map's depth, our name opens the object.
                let end = str_end(bytes, i)?;
                let lit = &text[i..=end];
                let rest = text[end + 1..].trim_start();
                if depth == 1 && lit == "\"fleets\"" && rest.starts_with(':') {
                    fleets_depth = Some(2);
                } else if fleets_depth == Some(depth) && lit == key && rest.starts_with(':') {
                    let colon = end + 1 + (text[end + 1..].len() - rest.len());
                    let open = text[colon + 1..].find('{')? + colon + 1;
                    let close = balanced_close(bytes, open)?;
                    return Some(text[open..=close].to_string());
                }
                i = end + 1;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    None
}

/// The index of the closing quote of the string literal opening at `start`.
fn str_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// The index of the `}` balancing the `{` at `open`, string-aware.
fn balanced_close(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = str_end(bytes, i)?,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Environment knob: the user-global fleet file's path, in full, when someone
/// keeps it somewhere else (a synced dotfiles folder, say). Unset ⇒ the
/// platform default under [`global_dir`].
pub const ENV_FLEET: &str = "ATRIUM_FLEET";

/// The user-global fleet-file path: [`ENV_FLEET`] when set, else
/// `%APPDATA%\atrium\fleet.json` on Windows / `~/.config/atrium/fleet.json`
/// elsewhere, or `None` if neither the variable nor the base dir is set.
pub fn global_path() -> Option<PathBuf> {
    global_file(ENV_FLEET, "fleet.json")
}

/// The platform's user-config directory for atrium: `%APPDATA%\atrium` on
/// Windows, `$XDG_CONFIG_HOME/atrium` or `~/.config/atrium` elsewhere. Both
/// user-global files (`fleet.json`, `config.json`) live here unless their own
/// variable moves them.
pub fn global_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.map(|b| b.join("atrium"))
}

/// A user-global file: the full path in `var` when set (empty counts as unset),
/// else `name` under [`global_dir`]. Each file has its own variable, so moving
/// one never moves the other.
pub fn global_file(var: &str, name: &str) -> Option<PathBuf> {
    match std::env::var_os(var).filter(|v| !v.is_empty()) {
        Some(p) => Some(PathBuf::from(p)),
        None => global_dir().map(|d| d.join(name)),
    }
}

/// A human-readable rendering of the global path for error messages, even when
/// the base dir is unset (so the message can still name *where* it would look).
fn global_path_display() -> String {
    match global_path() {
        Some(p) => p.display().to_string(),
        None => {
            if cfg!(windows) {
                "%APPDATA%\\atrium\\fleet.json".to_string()
            } else {
                "~/.config/atrium/fleet.json".to_string()
            }
        }
    }
}

/// Locate the fleet file: `./atrium.fleet.json` first, then the user-global
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
//   (whose directory is `~/.config/atrium`, so *every* useful path is outside it)
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
// atrium itself grants an agent through `cwd`/`add_dirs` is resolved through
// symlinks, classified against the anchor, and named on the screen the operator
// acknowledges — and the child is handed the same resolved path that was
// shown.** What it does not and cannot cover is listed on `Plan::build`.

#[cfg(test)]
mod tests;
