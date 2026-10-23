//! `atrium fleet init`: write `./atrium.fleet.json` from a template.

use super::ls::user_templates;
use crate::*;

/// `atrium fleet init <name> [--agents N]`: write `./atrium.fleet.json` holding
/// the named template — the user's own fleet of that name from the global
/// `fleet.json` (which wins over a built-in, and says so), else a built-in.
/// Copies, never links: the project file is self-contained and reviewable.
/// Refuses to overwrite an existing file.
/// The program a generated fleet's agents run when `--cmd` did not say:
/// codex from inside a codex pane (codex marks its commands' environment
/// with `CODEX_THREAD_ID`), claude from inside claude (`CLAUDECODE`) and from
/// anywhere else. A fleet written where it will run is of the writer's kind.
pub(crate) fn own_agent() -> String {
    own_agent_from(|k| std::env::var_os(k).is_some())
}

/// [`own_agent`] over an environment given as a predicate, for tests.
pub(crate) fn own_agent_from(has: impl Fn(&str) -> bool) -> String {
    if has("CODEX_THREAD_ID") && !has("CLAUDECODE") {
        "codex".to_string()
    } else {
        "claude".to_string()
    }
}

pub(crate) fn fleet_init(name: &str, builders: usize, cmd: Option<&str>) -> ExitCode {
    let agent = cmd.map(str::to_string).unwrap_or_else(own_agent);
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
        Ok(_) => match atrium::templates::builtin(name, builders, &agent) {
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

#[cfg(test)]
mod tests {
    use super::own_agent_from;

    /// Inside codex (its commands carry `CODEX_THREAD_ID`) a generated fleet is
    /// codex; inside claude (`CLAUDECODE`), or in a plain shell, claude. A shell
    /// that somehow carries both markers is read as claude: the safer default.
    #[test]
    fn a_fleet_written_inside_codex_is_codex_and_claude_otherwise() {
        let env = |vars: &'static [&'static str]| move |k: &str| vars.contains(&k);
        assert_eq!(own_agent_from(env(&["CODEX_THREAD_ID"])), "codex");
        assert_eq!(own_agent_from(env(&["CLAUDECODE"])), "claude");
        assert_eq!(own_agent_from(env(&[])), "claude");
        assert_eq!(
            own_agent_from(env(&["CODEX_THREAD_ID", "CLAUDECODE"])),
            "claude"
        );
    }
}
