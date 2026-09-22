//! The `atrium fleet …` command family: the dispatcher here, one file per
//! subcommand below it, and the preflight and launch code `fleet up` uses.

use crate::*;
use clean::fleet_clean;
use init::fleet_init;
use ls::{fleet_ls, fleet_ls_templates};

/// The `atrium fleet …` command family. `fleet up <name>` brings up a saved
/// roster; `fleet ls` lists the fleet names; anything else prints usage. Kept
/// separate from the hosted-program path — a fleet is atrium's own command, not a
/// child to run.
pub(crate) fn fleet_cmd(args: &[String]) -> ExitCode {
    if args.first().is_some_and(|a| atrium::help::wants(a)) {
        print!("{}", atrium::help::FLEET);
        return ExitCode::SUCCESS;
    }
    match args.first().map(String::as_str) {
        Some("up") => match args.get(1) {
            Some(name) if !name.starts_with('-') => {
                // Anything after the name is atrium's own meta-flags — `--allow-ctl`
                // (so the fleet can coordinate over the control plane), `--trust
                // <policy>`, `--max-depth`. Parsed with the shared parser.
                match atrium::ctl::parse_flags(&args[2..]) {
                    Ok((allow_ctl, max_depth, trust, rest)) if rest.is_empty() => {
                        fleet_up(name, allow_ctl, max_depth, trust)
                    }
                    Ok((_, _, _, rest)) => {
                        eprintln!("atrium fleet up: unexpected argument {:?}", rest[0]);
                        ExitCode::FAILURE
                    }
                    Err(e) => {
                        eprintln!("atrium fleet up: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            _ => {
                eprintln!("atrium fleet up <name>: needs a fleet name (try `atrium fleet ls`)");
                ExitCode::FAILURE
            }
        },
        Some("ls") if args.get(1).map(String::as_str) == Some("--templates") => {
            fleet_ls_templates()
        }
        Some("ls") => fleet_ls(),
        Some("init") => match args.get(1) {
            Some(name) if !name.starts_with('-') => {
                let mut builders = 1usize;
                let mut i = 2;
                while i < args.len() {
                    match args[i].as_str() {
                        "--agents" => match args.get(i + 1).and_then(|n| n.parse::<usize>().ok()) {
                            Some(n) if n >= 1 => {
                                builders = n;
                                i += 2;
                            }
                            _ => {
                                eprintln!("atrium fleet init: --agents needs a whole number of at least 1");
                                return ExitCode::FAILURE;
                            }
                        },
                        other => {
                            eprintln!("atrium fleet init: unexpected argument {other:?}");
                            return ExitCode::FAILURE;
                        }
                    }
                }
                fleet_init(name, builders)
            }
            _ => {
                eprintln!(
                    "atrium fleet init <name> [--agents N]: needs a template name \
                     (try `atrium fleet ls --templates`)"
                );
                ExitCode::FAILURE
            }
        },
        Some("clean") => match args.get(1) {
            Some(name) if !name.starts_with('-') => fleet_clean(name),
            _ => {
                eprintln!("atrium fleet clean <name>: needs a fleet name (try `atrium fleet ls`)");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprint!("{}", atrium::help::FLEET);
            ExitCode::FAILURE
        }
    }
}

/// Defang a fleet-file string before it reaches the terminal.
///
/// Every string in `atrium.fleet.json` is attacker-shaped in the workflow this
/// feature is built for - an agent writes the roster, a human reads the banner
/// and presses Enter - and the `json` parser decodes `\u001b`, so an agent name
/// or a path can carry a real ESC. Unfiltered it can clear the screen and
/// repaint a forged "every agent dir resolves inside" line over the disclosure
/// the human is about to approve.
pub(crate) fn fsan(s: &str) -> String {
    atrium::fleet::sanitize(s)
}

mod clean;
mod init;
mod launch;
mod ls;
mod preflight;
mod up;

#[cfg(test)]
pub(crate) use launch::fleet_launch;
pub(crate) use up::{fleet_up, up_alias};
