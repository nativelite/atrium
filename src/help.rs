//! The help text, in one place and one shape: `usage:` lines, then a block per
//! group with one command per line — its flags, then what it does — and a
//! closing pointer to the next level down. Every family answers `--help` and
//! `-h` the same way, on stdout, exit 0, and needs no session to do so.
//!
//! Each string lists what the parser in front of it accepts, nothing more
//! (`argv.rs`, `identity.rs`, `orphan.rs`, `spawn.rs`, `ctl.rs`, `fleet_cli.rs`,
//! `main.rs`). A flag that is not in the parser is not in the help.

/// `atrium --help`.
pub fn top() -> String {
    format!(
        "\
atrium {v} — one terminal, many agents

usage: atrium [session flags] [command [args...]]   host a command in a pane (default: your shell)
       atrium [session flags] up <fleet>            launch a saved fleet (session flags go first)
       atrium <family> ...                          fleet | ctl | config | recover | reap

session flags:
  --trust [<policy>]        the mode spawned agents run in, and their ceiling: plan (read-only) |
                            accept (auto-accept edits + a safe dev-command list; bare --trust) |
                            automode (claude's auto mode) | skip (full bypass, confirmed at launch)
  --skip-permissions        the same as --trust skip
  --allow-ctl               switch the control plane on (atrium ctl in every pane)
  --max-depth <N>           how deep spawned teammates may spawn (default 6; 0 = unlimited)
  --identity <name>, -I     run every pane under a vault identity (name only; see docs/identity.md)
  -n <N> | --grid <R>x<C>   open N copies of the command in a grid, or an exact R×C grid
  --reap-orphans            first kill pane groups whose atrium is gone (as `atrium reap`)

  --version, -V             print the version         --help, -h   this text
  --stdin-probe             show the raw bytes the terminal sends (a diagnostic)

families:  atrium fleet --help     saved rosters: up, init, ls, clean
           atrium ctl --help       the control plane, from inside a pane
           atrium config --help    the user-global config.json
           atrium recover --help   restore this project's last session
           atrium reap             clean up orphaned pane groups, then exit

The trust policy is a ceiling for every caller: `ctl spawn --mode` may match it or
de-escalate, never elevate. Extend the accept allowlist with ATRIUM_TRUST_ALLOW=a,b.
Mouse capture is off so text selection works; Ctrl+A m turns it on.
",
        v = env!("CARGO_PKG_VERSION")
    )
}

/// `atrium ctl --help`. Shown on stdout for `--help`/`-h`, and on stderr after
/// a usage error.
pub const CTL: &str = "\
usage: atrium ctl <command> [args]    the control plane, run from inside a pane
       atrium ctl --json ...          print the raw JSON reply    --no-color   plain text

teammates:
  spawn [--role R] [--identity X] [--here | --window] [--mode M] [--worktree W] -- <cmd...>
                                  create a teammate: beside you (--here) or in its own window
                                  (--window, default); --mode plan|accept|automode|skip, capped at
                                  the session policy; --worktree names a git worktree to run in
  send <target> <text...>         type a task into a teammate once it is idle (never mid-turn)
  status [<target>]               what each teammate is doing (idle, working, waiting on you)
  list                            the spawn tree: every pane, its role, parent and depth
  kill <target>                   stop a teammate and drop its pane
  respawn <target> [--worktree W] restart a teammate's agent as a fresh session, in place
  audit [<N>]                     the last N control-plane requests and their outcomes

board — durable team state (the source of truth):
  board set <key> <field=value...>   record current truth (status, owner, blocker, url)
  board get <key> | list | del <key>
  board claim <key> [--ttl <secs>] | release <key>   take a task under a short lease

bus — the event stream (what just happened):
  bus pub <topic> [--decision] [--to <role|id>[,...]] [--new] <field=value...>
                                  publish; it is typed into every pane subscribed to the topic and
                                  every pane named with --to, once each is idle; --decision needs
                                  an answer; --new opens a topic nobody has used
  bus sub <topic...> | unsub [<topic...>]   subscribe (`*` = everything) or unsubscribe (none = all)
  bus feed [--since <seq>]        pull what your topics carried since a cursor (not your own)
  bus resolve <seq>               mark a decision answered
  bus topics                      the active topics and how many panes follow each

<target> is a role name or a pane id from `list`. A worker reaches only the panes it
spawned; a bus wake also reaches the panes above it and the topic's subscribers.
";

/// `atrium fleet --help`.
pub const FLEET: &str = "\
usage: atrium fleet <command> [args]    saved rosters of agents (atrium.fleet.json)

  up <name> [--allow-ctl] [--trust [<policy>]] [--skip-permissions] [--max-depth <N>]
                                    bring the fleet up; the flags are the session flags
  init <template> [--agents <N>]    write ./atrium.fleet.json from a built-in (solo, pair, crew)
                                    or one of your own fleets in the user-global fleet.json;
                                    --agents scales the builders; never overwrites
  ls                                the fleets in the file that would be used
  ls --templates                    the built-ins and your own templates
  clean <name>                      reclaim a fleet's clean, merged worktrees

The fleet file is looked for in the current directory and its parents, then in the
user-global fleet.json (atrium config path shows where). See docs/fleets.md.
";

/// `atrium config --help`.
pub const CONFIG: &str = "\
usage: atrium config <command>    the user-global config.json

  path                  where it is read from, whether it exists, and what named it
  init [--at <path>]    write a starter there (asks for the location when a human is present)
                        and remember it, so a config kept elsewhere is found on every launch

The file holds claude_aliases, deny, ctl_allow, trust_allow, build_jobs, memory_mb and
fleet_defaults. ATRIUM_CONFIG names its full path outright. See the README's Config section.
";

/// `atrium recover --help`.
pub const RECOVER: &str = "\
usage: atrium recover [--list | --snapshot <path>] [session flags]   restore the last session

  (no flags)            restore the newest session that is not still running, after showing
                        what it would restore and asking once
  --list                the saved sessions for this project: state, age, panes, roles, trust
  --snapshot <path>     restore a specific snapshot file

session flags (--trust, --skip-permissions, --allow-ctl, --max-depth) override what the
snapshot recorded. A launch in a project whose last session crashed offers this itself.
See docs/session-recovery.md.
";

/// `atrium reap --help`.
pub const REAP: &str = "\
usage: atrium reap    kill pane process groups whose atrium is gone, print each, and exit

Off by default at launch; `atrium --reap-orphans ...` does the same before starting a
session. See docs/reaping.md.
";

/// Is `arg` a request for help? Both spellings, everywhere.
pub fn wants(arg: &str) -> bool {
    arg == "--help" || arg == "-h"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every family's help names every command its parser accepts, and only
    /// flags the parser accepts — the list here is the inventory, kept beside
    /// the text so a new subcommand is added to both or the test says so.
    #[test]
    fn each_help_names_every_command_and_flag_its_parser_accepts() {
        let cases: &[(&str, &[&str])] = &[
            (
                CTL,
                &[
                    "spawn",
                    "--role",
                    "--identity",
                    "--here",
                    "--window",
                    "--mode",
                    "--worktree",
                    "send",
                    "status",
                    "list",
                    "kill",
                    "respawn",
                    "audit",
                    "board set",
                    "board get",
                    "board claim",
                    "--ttl",
                    "release",
                    "bus pub",
                    "--decision",
                    "--to",
                    "--new",
                    "bus sub",
                    "unsub",
                    "bus feed",
                    "--since",
                    "bus resolve",
                    "bus topics",
                    "--json",
                    "--no-color",
                ],
            ),
            (
                FLEET,
                &[
                    "up",
                    "--allow-ctl",
                    "--trust",
                    "--skip-permissions",
                    "--max-depth",
                    "init",
                    "--agents",
                    "ls",
                    "--templates",
                    "clean",
                ],
            ),
            (CONFIG, &["path", "init", "--at"]),
            (
                RECOVER,
                &[
                    "--list",
                    "--snapshot",
                    "--trust",
                    "--allow-ctl",
                    "--max-depth",
                ],
            ),
        ];
        for (text, names) in cases {
            for n in *names {
                assert!(text.contains(n), "{n:?} missing from:\n{text}");
            }
        }
        let top = top();
        for n in [
            "--trust",
            "--skip-permissions",
            "--allow-ctl",
            "--max-depth",
            "--identity",
            "-I",
            "-n",
            "--grid",
            "--reap-orphans",
            "--version",
            "-V",
            "--help",
            "-h",
            "--stdin-probe",
            "up <fleet>",
            "fleet",
            "ctl",
            "config",
            "recover",
            "reap",
        ] {
            assert!(top.contains(n), "{n:?} missing from the top-level help");
        }
        assert!(top.contains(env!("CARGO_PKG_VERSION")));
    }

    /// One shape: every text opens with `usage:` and stays inside 100 columns,
    /// so it reads in a terminal without wrapping.
    #[test]
    fn every_help_starts_with_usage_and_fits_a_terminal() {
        for text in [
            top(),
            CTL.to_string(),
            FLEET.to_string(),
            CONFIG.to_string(),
            RECOVER.to_string(),
            REAP.to_string(),
        ] {
            assert!(
                text.starts_with("usage:") || text.starts_with("atrium "),
                "{text}"
            );
            for line in text.lines() {
                assert!(line.chars().count() <= 100, "over 100 columns: {line:?}");
            }
        }
    }

    #[test]
    fn help_is_asked_for_with_either_spelling() {
        assert!(wants("--help") && wants("-h"));
        assert!(!wants("help") && !wants("--h") && !wants("-help"));
    }
}
