//! `atrium config`: where the config is read from, and writing the starter.

use crate::*;

/// `atrium config path` — where the config is read from and why; `atrium config
/// init [--at <path>]` — write the starter there (asking, when no path is given
/// and a human is present) and point the platform place at it.
pub(crate) fn config_cmd(args: &[String]) -> ExitCode {
    use atrium::config;
    match args.first().map(String::as_str) {
        Some("path") => {
            let r = config::resolve();
            let (path, state) = match &r.path {
                Some(p) => (
                    p.display().to_string(),
                    if p.is_file() { "present" } else { "absent" },
                ),
                None => ("(no platform config directory)".to_string(), "absent"),
            };
            let by = match r.source {
                config::Source::Env => format!("named by {}", config::ENV_CONFIG),
                config::Source::Pointer => format!(
                    "named by {}",
                    config::pointer_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                config::Source::Default => "the platform default".to_string(),
            };
            println!("config: {path} ({state}; {by})");
            let fleet = atrium::fleet::global_path()
                .map(|p| {
                    let state = if p.is_file() { "present" } else { "absent" };
                    format!("{} ({state})", p.display())
                })
                .unwrap_or_else(|| "(no platform config directory)".to_string());
            println!("global fleet file: {fleet}");
            ExitCode::SUCCESS
        }
        Some("init") => {
            let at = match args.get(1).map(String::as_str) {
                Some("--at") => match args.get(2) {
                    Some(p) => Some(std::path::PathBuf::from(p)),
                    None => {
                        eprintln!("atrium config init: --at needs a path");
                        return ExitCode::FAILURE;
                    }
                },
                Some(other) => {
                    eprintln!("atrium config init: unexpected argument {other:?}");
                    return ExitCode::FAILURE;
                }
                None => None,
            };
            let path = match at {
                Some(p) => p,
                None => {
                    use std::io::IsTerminal;
                    let Some(default) = config::default_path() else {
                        eprintln!(
                            "atrium config init: no platform config directory; pass --at <path>"
                        );
                        return ExitCode::FAILURE;
                    };
                    if std::env::var_os("ATRIUM_YES").is_some() || !std::io::stdin().is_terminal() {
                        default
                    } else {
                        eprint!(
                            "Global config file location (Enter for default: {}): ",
                            default.display()
                        );
                        let _ = std::io::Write::flush(&mut std::io::stderr());
                        let mut line = String::new();
                        let _ = std::io::stdin().read_line(&mut line);
                        let home = std::env::var_os("HOME")
                            .or_else(|| std::env::var_os("USERPROFILE"))
                            .map(std::path::PathBuf::from);
                        config::answer_to_path(&line, &default, home.as_deref())
                    }
                }
            };
            match config::init_at(&path) {
                Ok(done) => {
                    println!("wrote {}", done.config.display());
                    if let Some(p) = done.pointer {
                        println!("remembered in {}", p.display());
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("atrium config init: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(a) if atrium::help::wants(a) => {
            print!("{}", atrium::help::CONFIG);
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{}", atrium::help::CONFIG);
            ExitCode::FAILURE
        }
    }
}
