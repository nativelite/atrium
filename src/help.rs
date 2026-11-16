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
       atrium <family> ...                          fleet | ctl | config | mod | recover | reap

session flags:
  --trust [<policy>]        the mode spawned agents run in, and their ceiling: plan (read-only) |
                            accept (auto-accept edits + a safe dev-command list; bare --trust) |
                            automode (claude's auto mode) | skip (full bypass, confirmed at launch)
  --skip-permissions        the same as --trust skip
  --allow-ctl               switch the control plane on (atrium ctl in every pane)
  --max-depth <N>           how deep spawned teammates may spawn (default 6; 0 = unlimited)
  --operator-token-file <path>
                            named tokens for control-plane callers that are not panes; it is
                            the first argument, or ATRIUM_OPERATOR_TOKEN_FILE holds the path
  --ctl-socket <path>       where the control plane listens, instead of a per-process address
                            (or ATRIUM_CTL_SOCKET; refused when another session is live on it)
  --identity <name>, -I     run every pane under a vault identity (name only; see docs/identity.md)
  -n <N> | --grid <R>x<C>   open N copies of the command in a grid, or an exact R×C grid
  --reap-orphans            first kill pane groups whose atrium is gone (as `atrium reap`)

  --version, -V [--json]    print the version; --json: one object (version, commit, target)
  --help, -h                this text
  --stdin-probe             show the raw bytes the terminal sends (a diagnostic)

families:  atrium fleet --help     saved rosters: up, init, ls, clean
           atrium ctl --help       the control plane, from inside a pane
           atrium config --help    the user-global config.json
           atrium mod --help       the plugin a claude pane loads: install, status
           atrium recover --help   restore this project's last session
           atrium reap             clean up orphaned pane groups, then exit

The trust policy is a ceiling for every caller: `ctl spawn --mode` may match it or
de-escalate, never elevate. Extend the accept allowlist with ATRIUM_TRUST_ALLOW=a,b.
Mouse capture is off so text selection works; Ctrl+A m turns it on.
",
        v = env!("CARGO_PKG_VERSION")
    )
}

/// The target triple this binary was built for: the `TARGET` rustc was given,
/// handed over by `build.rs`. Should a build ever run without the script, it
/// is put together from the compiler's `cfg` values instead, which matches
/// the rustc triple on the targets atrium ships for.
pub fn target_triple() -> String {
    if let Some(target) = option_env!("ATRIUM_BUILD_TARGET").filter(|t| !t.is_empty()) {
        return target.to_string();
    }
    let vendor = if cfg!(target_vendor = "apple") {
        "apple"
    } else if cfg!(target_vendor = "pc") {
        "pc"
    } else {
        "unknown"
    };
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let env = if cfg!(target_env = "msvc") {
        "-msvc"
    } else if cfg!(target_env = "gnu") {
        "-gnu"
    } else if cfg!(target_env = "musl") {
        "-musl"
    } else {
        ""
    };
    format!("{}-{vendor}-{os}{env}", std::env::consts::ARCH)
}

/// The commit this binary was built from, when the build knew it: twelve hex
/// characters of `HEAD` when `build.rs` ran in a git checkout of atrium, or
/// whatever `ATRIUM_BUILD_COMMIT` a packager set at compile time. A crates.io
/// install has neither. It is the commit the tree was at; local changes on
/// top of it are not reported.
pub fn build_commit() -> Option<&'static str> {
    option_env!("ATRIUM_BUILD_COMMIT").filter(|c| !c.is_empty())
}

/// `atrium --version --json`: one object on one line, keys in this order.
/// `commit` is always present and is `null` when the build did not know it.
pub fn version_json() -> String {
    version_value(env!("CARGO_PKG_VERSION"), build_commit(), &target_triple()).to_string()
}

fn version_value(version: &str, commit: Option<&str>, target: &str) -> json::Value {
    let s = |t: &str| json::Value::String(t.to_string());
    json::Value::Object(vec![
        ("version".to_string(), s(version)),
        ("commit".to_string(), commit.map_or(json::Value::Null, s)),
        ("target".to_string(), s(target)),
    ])
}

/// `atrium ctl --help`. Shown on stdout for `--help`/`-h`, and on stderr after
/// a usage error.
pub const CTL: &str = "\
usage: atrium ctl <command> [args]    the control plane, run from inside a pane
       atrium ctl --json ...          print the raw JSON reply    --no-color   plain text
       atrium ctl --quiet ...         print no reply (for a hook); the exit code still answers
       atrium ctl --operator-token-file <path> [--operator <name>] <command> [args]
                                      from outside a pane, as a named operator: the verbs its
                                      entry in the token file allows, on any pane, audited as
                                      operator:<name> (ATRIUM_CTL names the endpoint)

teammates:
  spawn [--role R] [--identity X] [--here | --window] [--mode M] [--worktree W] -- <cmd...>
                                  create a teammate: beside you (--here) or in its own window
        --no-focus                keep the keyboard where it is (the teammate opens unfocused)
        --beside <pane>           with --here: tile next to that pane (an id in your window)
        --size <N%|N>             with --here: the new pane takes N% of the anchor, or N cells
                                  (--window, default); --mode plan|accept|automode|skip, capped at
                                  the session policy; --worktree names a git worktree to run in
  send <target> <text...>         type a task into a teammate once it is idle (never mid-turn);
                                  it arrives marked with who sent it
  command <target> /<name>        run one slash command in a teammate, when the human listed
                                  it under `commands` (config or fleet); no arguments
  status [<target>]               what each teammate is doing (idle, working, waiting on you)
  list                            the spawn tree: every pane, its role, parent and depth
  kill <target>                   stop a teammate and drop its pane
  respawn <target> [--worktree W] restart a teammate's agent as a fresh session, in place
  audit [<N>]                     the last N control-plane requests and their outcomes
  ping [--timeout <ms>]           is the session answering: exit 0 with its version and uptime, or
                                  exit 1 with one line (no endpoint, refused, timeout; 1000 ms)
  metrics                         the session's counters as Prometheus text (--json: the raw reply)
  answer <target>                a pane's last answer and status, as its mod reported them
  wait <target> [--for answer|idle|exit] [--timeout S] [--after SEQ]
                                  poll until the pane answers (seq past SEQ), is idle, or is gone
  ask <target> [--timeout S] <question...>
                                  ask a pane's mod a question, answered from a fork of its own
                                  context without interrupting it; waits for the reply (120 s)
  asked <target> <id>             read the reply to an earlier ask
  who <path>                      which panes edited a file (their mods report every edit)
  hello | report | whoami | inbox what a pane's mod speaks (see mod/README.md)

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
            [--operator-token-file <path>]
                                    bring the fleet up; the flags are the session flags
  up <name> --headless [--size <rows>x<cols>] [session flags]
                                    the same with no terminal: nothing is read from stdin or
                                    drawn on stdout, the panes run at --size (default 50x200),
                                    and the session is reached through `atrium ctl` alone. Needs
                                    the control channel (--allow-ctl or the fleet's allow_ctl)
                                    and ATRIUM_YES=1, since there is nobody to ask. Ends when
                                    its last pane exits or on a termination signal
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

/// `atrium mod --help`.
pub const MOD: &str = "\
usage: atrium mod <command>    the plugin of function hooks a claude pane loads

  install [--at <dir>]  write the mod built into this atrium to its folder (the config's
                        mod.path, else mod/ under the platform config directory, else <dir>);
                        an upgrade of atrium is an upgrade of the mod, a hand edit is restored
  status                what is installed there, and whether claude panes will load it

Once installed, every claude pane atrium spawns gets CLAUDE_CODE_PLUGIN_DIRS naming the
folder and reports its status to atrium for certain instead of by inference. ATRIUM_MOD
names the folder outright; \"mod\": false in config.json or a fleet turns injection off.
See PLAN-mod.md and mod/README.md.
";

/// `atrium recover --help`.
pub const RECOVER: &str = "\
usage: atrium recover [--list | --snapshot <path>] [session flags]   restore the last session

  (no flags)            restore the newest session that is not still running, after showing
                        what it would restore and asking once
  --list                the saved sessions for this project: state, age, panes, roles, trust
  --snapshot <path>     restore a specific snapshot file

session flags (--trust, --skip-permissions, --allow-ctl, --max-depth) override what the
snapshot recorded; --operator-token-file <path> is read anew, never from the snapshot.
A launch in a project whose last session crashed offers this itself.
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

    #[test]
    fn version_json_is_one_object_of_version_commit_and_target() {
        let text = version_json();
        assert!(!text.contains('\n'), "one line: {text}");
        let v = json::parse(&text).expect("valid json");
        let keys: Vec<&str> = v
            .as_object()
            .expect("an object")
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["version", "commit", "target"]);
        assert_eq!(
            v.get("version").and_then(json::Value::as_str),
            Some(env!("CARGO_PKG_VERSION"))
        );
        // The commit is a string when the build was told, else null: never absent.
        match (v.get("commit"), build_commit()) {
            (Some(json::Value::Null), None) => {}
            (Some(json::Value::String(c)), Some(want)) => assert_eq!(c, want),
            other => panic!("commit: {other:?}"),
        }
        let target = v
            .get("target")
            .and_then(json::Value::as_str)
            .expect("target");
        assert_eq!(target, target_triple());
        assert!(target.starts_with(std::env::consts::ARCH), "{target}");
        assert!(target.split('-').count() >= 3, "{target}");
        #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
        assert_eq!(target, "x86_64-pc-windows-msvc");
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        assert_eq!(target, "x86_64-unknown-linux-gnu");
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(target, "aarch64-apple-darwin");
    }

    #[test]
    fn version_json_carries_a_known_commit_as_a_string() {
        assert_eq!(
            version_value("1.2.3", Some("68af6fe"), "x86_64-unknown-linux-gnu").to_string(),
            r#"{"version":"1.2.3","commit":"68af6fe","target":"x86_64-unknown-linux-gnu"}"#
        );
        assert_eq!(
            version_value("1.2.3", None, "t").to_string(),
            r#"{"version":"1.2.3","commit":null,"target":"t"}"#
        );
    }

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
                    "command",
                    "status",
                    "list",
                    "kill",
                    "respawn",
                    "audit",
                    "ping",
                    "--timeout",
                    "metrics",
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
                    "--quiet",
                    "--no-color",
                    "--operator-token-file",
                    "--operator <name>",
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
                    "--operator-token-file",
                    "--headless",
                    "--size",
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
                    "--operator-token-file",
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
            "--operator-token-file",
            "ATRIUM_OPERATOR_TOKEN_FILE",
            "--identity",
            "-I",
            "-n",
            "--grid",
            "--reap-orphans",
            "--version",
            "-V",
            "--json",
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
