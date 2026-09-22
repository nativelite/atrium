//! `atrium fleet ls` (the fleet names) and `fleet ls --templates` (built-ins and the
//! user's own).

use super::fsan;
use crate::*;

/// The user's own templates: every fleet in the global `fleet.json`, by name,
/// with the text it was defined in. `None` when there is no global file; an
/// unreadable one is an error, since a typo there must not silently hide a
/// template behind a built-in of the same name.
pub(super) fn user_templates() -> Result<Option<(std::path::PathBuf, atrium::fleet::Fleets)>, String>
{
    let Some(path) = atrium::fleet::global_path() else {
        return Ok(None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let fleets = atrium::fleet::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some((path, fleets)))
}

/// `atrium fleet ls --templates`: the built-ins and the user's own, each
/// source labelled, so `init <name>` has a menu.
pub(crate) fn fleet_ls_templates() -> ExitCode {
    println!("built-in:");
    for (name, summary) in atrium::templates::NAMES
        .iter()
        .zip(atrium::templates::SUMMARIES)
    {
        println!("  {name:<12} {summary}");
    }
    match user_templates() {
        Ok(Some((path, fleets))) => {
            println!("yours ({}):", path.display());
            let names = fleets.names();
            if names.is_empty() {
                println!("  (none)");
            }
            for name in names {
                let f = fleets.get(name).unwrap();
                let roster: Vec<&str> = f.agents.iter().map(|a| a.name.as_str()).collect();
                let shadows = if atrium::templates::NAMES.contains(&name) {
                    "  (shadows the built-in)"
                } else {
                    ""
                };
                println!("  {name:<12} {}{shadows}", roster.join(", "));
            }
        }
        Ok(None) => {
            let where_ = atrium::fleet::global_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "the user-global fleet.json".to_string());
            println!("yours: none — fleets in {where_} are templates too");
        }
        Err(e) => {
            eprintln!("atrium fleet: {e}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// List the fleet names in the discovered fleet file, in file order. A missing
/// file or a malformed one is a clear error on stderr (non-zero exit).
pub(crate) fn fleet_ls() -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let located = match atrium::fleet::discover(&cwd) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("atrium fleet: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = match std::fs::read_to_string(&located.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("atrium fleet: cannot read {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleets = match atrium::fleet::parse(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("atrium fleet: {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let names = fleets.names();
    if names.is_empty() {
        println!("(no fleets defined in {})", located.path.display());
    } else {
        // Defanged: the fleet file supplies these and the `json` parser decodes
        // `\u001b`, so a fleet name can carry a real ESC and clear the terminal
        // this is being read on. `fsan` escapes control characters and nothing
        // else - no truncation, no backslash doubling - so an ordinary name
        // still round-trips through `atrium fleet ls | xargs atrium fleet up`.
        for name in names {
            println!("{}", fsan(name));
        }
    }
    ExitCode::SUCCESS
}
