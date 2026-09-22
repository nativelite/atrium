//! `atrium fleet init`: write `./atrium.fleet.json` from a template.

use super::ls::user_templates;
use crate::*;

/// `atrium fleet init <name> [--agents N]`: write `./atrium.fleet.json` holding
/// the named template — the user's own fleet of that name from the global
/// `fleet.json` (which wins over a built-in, and says so), else a built-in.
/// Copies, never links: the project file is self-contained and reviewable.
/// Refuses to overwrite an existing file.
pub(crate) fn fleet_init(name: &str, builders: usize) -> ExitCode {
    let target = std::path::Path::new("atrium.fleet.json");
    if target.exists() {
        eprintln!(
            "atrium fleet init: ./atrium.fleet.json already exists; edit it, or move it aside first"
        );
        return ExitCode::FAILURE;
    }
    let (text, source) = match user_templates() {
        Err(e) => {
            eprintln!("atrium fleet: {e}");
            return ExitCode::FAILURE;
        }
        Ok(Some((path, fleets))) if fleets.get(name).is_some() => {
            // The user's fleet, copied as the text they wrote it in: the file's
            // object for that name, re-serialized from the parsed value would
            // lose their key order and formatting.
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            match atrium::fleet::fleet_object_text(&raw, name) {
                Some(object) => {
                    if builders != 1 {
                        eprintln!(
                            "atrium fleet init: --agents applies to the built-ins; {name:?} is yours \
                             and is copied as written"
                        );
                    }
                    let shadow = if atrium::templates::NAMES.contains(&name) {
                        format!(" (yours, shadowing the built-in {name:?})")
                    } else {
                        String::new()
                    };
                    (
                        atrium::templates::fleet_file(name, &object),
                        format!("{}{shadow}", path.display()),
                    )
                }
                None => {
                    eprintln!(
                        "atrium fleet init: could not extract {name:?} from {}",
                        path.display()
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
        Ok(_) => match atrium::templates::builtin(name, builders) {
            Some(object) => (
                atrium::templates::fleet_file(name, &object),
                format!("built-in {name:?}"),
            ),
            None => {
                eprintln!(
                    "atrium fleet init: no template named {name:?} (try `atrium fleet ls --templates`)"
                );
                return ExitCode::FAILURE;
            }
        },
    };
    // Whatever the source, what lands must be a roster `fleet up` accepts.
    if let Err(e) = atrium::fleet::parse(&text) {
        eprintln!("atrium fleet init: the template does not parse as a fleet: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::write(target, &text) {
        eprintln!("atrium fleet init: write {}: {e}", target.display());
        return ExitCode::FAILURE;
    }
    println!("wrote ./atrium.fleet.json from {source}");
    println!("next: review it, then `atrium fleet up {name}`");
    ExitCode::SUCCESS
}
