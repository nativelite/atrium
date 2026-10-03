//! `atrium mod`: the plugin of function hooks a claude pane loads, written out
//! of this binary ([`atrium::modfiles`]).

use crate::*;

/// `atrium mod install [--at <dir>]` — write the mod built into this binary to
/// the configured folder (or `<dir>`); `atrium mod status` — what is installed
/// and whether this machine's claude panes will load it.
pub(crate) fn mod_cmd(args: &[String]) -> ExitCode {
    use atrium::modfiles;
    match args.first().map(String::as_str) {
        Some("install") => {
            let at = match args.get(1).map(String::as_str) {
                Some("--at") => match args.get(2) {
                    Some(p) => Some(std::path::PathBuf::from(p)),
                    None => {
                        eprintln!("atrium mod install: --at needs a directory");
                        return ExitCode::FAILURE;
                    }
                },
                Some(other) => {
                    eprintln!("atrium mod install: unexpected argument {other:?}");
                    return ExitCode::FAILURE;
                }
                None => None,
            };
            let configured = modfiles::configured_dir();
            let dir = match at.clone().or_else(|| configured.clone()) {
                Some(d) => d,
                None => {
                    eprintln!("atrium mod install: no platform config directory; pass --at <dir>");
                    return ExitCode::FAILURE;
                }
            };
            match modfiles::install_at(&dir) {
                Ok(done) => {
                    println!(
                        "mod {} at {}: {} written, {} unchanged",
                        modfiles::embedded_version(),
                        done.dir.display(),
                        done.written.len(),
                        done.unchanged.len()
                    );
                    if at.is_some() && configured.as_deref() != Some(dir.as_path()) {
                        println!(
                            "panes look in {}; to use this folder set \"mod\": {{ \"path\": {:?} }} \
                             in config.json or {}={}",
                            configured
                                .map(|c| c.display().to_string())
                                .unwrap_or_else(|| "(no platform config directory)".to_string()),
                            dir.display().to_string(),
                            modfiles::ENV_MOD,
                            dir.display()
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("atrium mod install: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("status") => {
            let (line, _) = modfiles::describe(&modfiles::status(), modfiles::session_enabled());
            println!("{line}");
            println!(
                "claude panes atrium spawns get {}=<that folder> and {}=<this atrium>; \
                 any claude loads it with --plugin-dir <that folder>",
                modfiles::ENV_PLUGIN_DIRS,
                modfiles::ENV_BIN
            );
            ExitCode::SUCCESS
        }
        Some(a) if atrium::help::wants(a) => {
            print!("{}", atrium::help::MOD);
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{}", atrium::help::MOD);
            ExitCode::FAILURE
        }
    }
}
