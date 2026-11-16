//! `atrium mod`: the plugin of function hooks a claude pane loads, written out
//! of this binary ([`atrium::modfiles`]).

use crate::*;

/// `atrium mod install [--at <dir>]` — write the mod built into this binary to
/// the configured folder (or `<dir>`); `atrium mod status` — what is installed,
/// whether this machine's claude panes will load it, whether the circuit
/// breaker backed it out, and what a running session's mods reported;
/// `atrium mod retry` — forget a trip, so the next session loads the mod.
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
            // The same code the first-run offer runs on a yes: the files,
            // the marker, and a cleared breaker record.
            match modfiles::install_report(&dir, atrium::session_store::state_root().as_deref()) {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                    // Installing into the folder panes load is a yes to the
                    // mod, kept like the first-run offer's: it turns a
                    // recorded no around, and lets a launch write the folder
                    // again if it goes missing. Not for a folder the user
                    // manages (`ATRIUM_MOD`, `mod.path`, a git checkout).
                    let cfg = atrium::config::get();
                    if configured.as_deref() == Some(dir.as_path())
                        && install_is_a_yes(
                            modfiles::env_named(),
                            cfg.mod_managed,
                            modfiles::developer_folder(&dir, false),
                            modfiles::Said::of(cfg),
                        )
                    {
                        match atrium::config::record_mod_answer(true) {
                            Ok(kept) if cfg.mod_off => println!(
                                "the mod was off on this machine and is on again: your yes is \
                                 kept in {}",
                                kept.describe(true)
                            ),
                            Ok(kept) => println!(
                                "your yes is kept in {}: a launch writes this folder again if \
                                 it goes missing",
                                kept.describe(true)
                            ),
                            Err(why) => {
                                eprintln!("atrium mod install: the yes could not be kept ({why})")
                            }
                        }
                    }
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
            let enabled = modfiles::session_enabled();
            let said = modfiles::Said::of(atrium::config::get());
            let (line, _) =
                modfiles::describe(&modfiles::status(), enabled, modfiles::session_yes());
            println!("{line}");
            println!(
                "enabled: {}",
                if enabled {
                    "yes (a fleet may still say \"mod\": false for its own session)"
                } else {
                    "no: \"mod\": false in config.json, or a no to the first-run offer; \
                     nothing is loaded, checked or said"
                }
            );
            println!(
                "{}",
                offer_line(
                    atrium::config::mod_answer_kept().map(|(yes, kept)| (yes, kept.describe(yes))),
                    unasked_of(
                        modfiles::env_named(),
                        said == modfiles::Said::Managed,
                        modfiles::configured_dir()
                            .is_some_and(|d| modfiles::developer_folder(&d, false)),
                        modfiles::configured_dir()
                            .is_some_and(|d| modfiles::folder_at(&d) == modfiles::Folder::Ours),
                    ),
                )
            );
            // Which path a named teammate takes in a session started from
            // here with the control plane on (a fleet's own keys may differ).
            println!(
                "{}",
                modfiles::session_teammates_line(true, || claude_version("claude"))
            );
            println!(
                "built in: mod {} ({})",
                modfiles::embedded_version(),
                modfiles::embedded_hash()
            );
            // The breaker's memory, against the claude a pane would run: the
            // `claude` on PATH and every command the config or the
            // environment names as one (`claude_aliases`).
            let mut programs = vec!["claude".to_string()];
            for alias in atrium::bind::session_claude_aliases() {
                if !programs.contains(&alias) {
                    programs.push(alias);
                }
            }
            let versions: Vec<(String, Option<String>)> = programs
                .into_iter()
                .map(|p| {
                    let v = claude_version(&p);
                    (p, v)
                })
                .collect();
            for (program, version) in &versions {
                println!(
                    "claude: `{program}` is {}",
                    version
                        .as_deref()
                        .unwrap_or("not answering --version (unknown, which is not \"changed\")")
                );
            }
            match atrium::session_store::state_root() {
                Some(root) => {
                    let record = modfiles::read_record(&root);
                    let claude = version_for(&record, &versions);
                    for line in modfiles::breaker_lines(
                        &record,
                        claude.as_deref(),
                        atrium::session_store::now_ms(),
                    ) {
                        println!("{line}");
                    }
                }
                None => println!(
                    "breaker: no state directory on this machine; a trip lasts one session"
                ),
            }
            println!("{}", session_line());
            println!(
                "claude panes atrium spawns get {}=<that folder> and {}=<this atrium>; \
                 any claude loads it with --plugin-dir <that folder>",
                modfiles::ENV_PLUGIN_DIRS,
                modfiles::ENV_BIN
            );
            ExitCode::SUCCESS
        }
        Some("off") => {
            if let Some(extra) = args.get(1) {
                eprintln!("atrium mod off: unexpected argument {extra:?}");
                return ExitCode::FAILURE;
            }
            // A no, kept exactly as answering `n` to the first-run offer
            // keeps it: the config's one key, or the answer file.
            match atrium::config::record_mod_answer(false) {
                Ok(kept) => {
                    println!(
                        "the mod is off in atrium's claude panes from the next session: the no \
                         is kept in {}",
                        kept.describe(false)
                    );
                    println!(
                        "its folder is left as it is, and no launch writes it; `atrium mod \
                         install` turns the mod on again"
                    );
                    ExitCode::SUCCESS
                }
                Err(why) => {
                    eprintln!("atrium mod off: {why}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("retry") => {
            if let Some(extra) = args.get(1) {
                eprintln!("atrium mod retry: unexpected argument {extra:?}");
                return ExitCode::FAILURE;
            }
            let Some(root) = atrium::session_store::state_root() else {
                println!("mod retry: no state directory on this machine, so nothing is remembered");
                return ExitCode::SUCCESS;
            };
            match modfiles::read_record(&root) {
                modfiles::RecordRead::None => {
                    println!("mod retry: the breaker is closed; nothing to forget");
                }
                was => {
                    if !modfiles::clear_record(&root) {
                        eprintln!(
                            "atrium mod retry: cannot remove {}",
                            modfiles::record_path(&root).display()
                        );
                        return ExitCode::FAILURE;
                    }
                    match was {
                        modfiles::RecordRead::Tripped(r) => println!(
                            "mod retry: forgot the trip against Claude Code {} and mod {} ({})",
                            r.claude, r.mod_version, r.reason
                        ),
                        _ => println!("mod retry: removed an unreadable record"),
                    }
                    println!(
                        "the next session loads the mod into its claude panes again; a session \
                         already running keeps what it decided"
                    );
                }
            }
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

/// Does `atrium mod install` into the folder panes load count as a yes to
/// the mod, to be kept? Not when the user manages the folder (`ATRIUM_MOD`,
/// the config's `mod.path`, a git checkout), and not when a yes is already
/// kept. A recorded no is turned around: installing is changing one's mind.
pub(crate) fn install_is_a_yes(
    env_named: bool,
    path_named: bool,
    checkout: bool,
    said: atrium::modfiles::Said,
) -> bool {
    !env_named && !path_named && !checkout && said != atrium::modfiles::Said::Yes
}

/// Why the first-run offer is not made on this machine, or that it will be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unasked {
    /// `ATRIUM_MOD` names the folder.
    Env,
    /// The config's `"mod"` is an object.
    Managed,
    /// The folder is a git checkout or atrium's own source.
    Checkout,
    /// A mod with atrium's marker is installed.
    Installed,
    /// Nothing stands in the way: the next start with a person at it asks.
    NotYet,
}

/// [`Unasked`] from the facts, in the order [`atrium::modfiles::offer_of`]
/// looks at them.
pub(crate) fn unasked_of(env_named: bool, managed: bool, checkout: bool, marked: bool) -> Unasked {
    if env_named {
        Unasked::Env
    } else if managed {
        Unasked::Managed
    } else if marked {
        Unasked::Installed
    } else if checkout {
        Unasked::Checkout
    } else {
        Unasked::NotYet
    }
}

/// The `offer:` line of `atrium mod status`: the recorded answer to the
/// first-run offer and where it is kept, or why nobody was asked; and, for
/// a yes, whether a launch really writes the folder on the strength of it.
pub(crate) fn offer_line(kept: Option<(bool, String)>, state: Unasked) -> String {
    let yours = match state {
        Unasked::Env => Some(format!("{} names it", atrium::modfiles::ENV_MOD)),
        Unasked::Checkout => Some("it is a git checkout or atrium's own source".to_string()),
        _ => None,
    };
    match (kept, state) {
        (Some((true, at)), _) => match yours {
            Some(why) => format!(
                "offer: answered yes, kept in {at}; the folder panes load is yours to manage \
                 ({why}), so no launch writes it"
            ),
            None => format!(
                "offer: answered yes, kept in {at}; a launch writes the folder again if it \
                 goes missing (`atrium mod off` stops that)"
            ),
        },
        (Some((false, at)), _) => {
            format!("offer: answered no, kept in {at}; `atrium mod install` turns it around")
        }
        (None, Unasked::Env) => format!(
            "offer: not asked ({} names the folder, which is yours to manage)",
            atrium::modfiles::ENV_MOD
        ),
        (None, Unasked::Managed) => "offer: not asked (the config's \"mod\" is an object, \
                                     which is yours to manage)"
            .to_string(),
        (None, Unasked::Checkout) => "offer: not asked (the mod folder is a git checkout or \
                                      atrium's own source, which atrium never writes to)"
            .to_string(),
        (None, Unasked::Installed) => "offer: not asked (a mod with atrium's marker is \
                                       installed, and a launch keeps it current; `atrium mod \
                                       install` keeps a yes, `atrium mod off` a no)"
            .to_string(),
        (None, Unasked::NotYet) => "offer: not asked yet; the next start with a person at it \
                                    asks once, when claude is on PATH (the answer is kept as \
                                    \"mod\" in config.json, or in mod.answer beside it when \
                                    there is no config file); `atrium mod install` says yes \
                                    now, `atrium mod off` says no"
            .to_string(),
    }
}

/// The version to hold the record against, among the claudes a pane could
/// run: the one the record names if any of them is it (the mod stays off for
/// that claude), else the first that answered, else none (unknown).
pub(crate) fn version_for(
    record: &atrium::modfiles::RecordRead,
    versions: &[(String, Option<String>)],
) -> Option<String> {
    let known = || versions.iter().filter_map(|(_, v)| v.clone());
    match record {
        atrium::modfiles::RecordRead::Tripped(r) => {
            known().find(|v| r.matches(v)).or_else(|| known().next())
        }
        _ => known().next(),
    }
}

/// What a running session says of its mods, in one line: the failures they
/// reported and whether its breaker tripped, read from `ctl metrics` (counts
/// only, served without a token) at the session this shell is in, or the one
/// session running on this machine. Bounded at two seconds.
fn session_line() -> String {
    let address = match std::env::var(atrium::ctl::ENV_ADDRESS)
        .ok()
        .filter(|a| !a.is_empty())
        .map_or_else(atrium::ipc::discover_one, Ok)
    {
        Ok(a) => a,
        Err(_) => {
            return "session: none reachable from here (failure counts are a running session's)"
                .to_string()
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let asked = std::thread::Builder::new()
        .name("atrium-mod-status".to_string())
        .spawn(move || {
            let _ = tx.send(atrium::ipc::request(&address, r#"{"cmd":"metrics"}"#));
        });
    if asked.is_err() {
        return "session: could not be asked".to_string();
    }
    match rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Ok(reply)) => session_line_of(&reply),
        _ => "session: did not answer within 2 s".to_string(),
    }
}

/// [`session_line`] from a `metrics` reply. An atrium older than the counters
/// answers without them, and that is said rather than read as zero.
pub(crate) fn session_line_of(reply: &str) -> String {
    let v = json::parse(reply).ok();
    let num = |k: &str| {
        v.as_ref()
            .and_then(|v| v.get(k))
            .and_then(json::Value::as_i64)
    };
    match (num("mod_failures"), num("mod_tripped")) {
        (Some(failures), Some(tripped)) => {
            let late = num("mod_late_hellos").unwrap_or(0);
            let breaker = match (tripped != 0, late) {
                (false, _) => "closed".to_string(),
                (true, 0) => "tripped: new claude panes launch without the mod".to_string(),
                // Not un-tripped by itself; said, so it is not silently off.
                (true, n) => format!(
                    "tripped: new claude panes launch without the mod, though {n} pane{} \
                     launched with it {} said hello since (the mod may be fine: a new \
                     session loads it again)",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "has" } else { "have" }
                ),
            };
            format!(
                "session: {failures} failure{} reported by its mods; breaker {breaker} \
                 (`atrium ctl status <pane>` shows one pane's mod)",
                if failures == 1 { "" } else { "s" },
            )
        }
        _ => "session: reachable, and its atrium does not report mod health (older than 0.48)"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_is_held_against_the_claude_it_names_among_the_ones_a_pane_could_run() {
        use atrium::modfiles::{RecordRead, TripRecord};
        let record = RecordRead::Tripped(TripRecord::now("2.1.325", "why", 0));
        let v = |list: &[(&str, Option<&str>)]| -> Vec<(String, Option<String>)> {
            list.iter()
                .map(|(p, v)| (p.to_string(), v.map(str::to_string)))
                .collect()
        };
        // The alias is the claude that tripped: the mod stays off for it,
        // whatever the plain `claude` on PATH is.
        assert_eq!(
            version_for(
                &record,
                &v(&[("claude", Some("2.2.0")), ("claude2", Some("2.1.325"))])
            ),
            Some("2.1.325".to_string())
        );
        assert_eq!(
            version_for(&record, &v(&[("claude", Some("2.2.0"))])),
            Some("2.2.0".to_string())
        );
        // Nobody answered: unknown, not "changed".
        assert_eq!(
            version_for(&record, &v(&[("claude", None), ("claude2", None)])),
            None
        );
        assert_eq!(
            version_for(
                &RecordRead::None,
                &v(&[("claude", None), ("c2", Some("9"))])
            ),
            Some("9".to_string())
        );
    }

    /// `atrium mod install` into the folder panes load is a yes, kept like
    /// the offer's: it turns a recorded no around. Never for a folder the
    /// user manages, and not again once a yes is kept.
    #[test]
    fn installing_into_the_panes_folder_is_a_yes_unless_the_user_manages_it() {
        use atrium::modfiles::Said;
        assert!(install_is_a_yes(false, false, false, Said::Unset));
        assert!(install_is_a_yes(false, false, false, Said::No));
        assert!(!install_is_a_yes(false, false, false, Said::Yes));
        for said in [Said::Unset, Said::No, Said::Yes, Said::Managed] {
            assert!(!install_is_a_yes(true, false, false, said), "ATRIUM_MOD");
            assert!(!install_is_a_yes(false, true, false, said), "a mod object");
            assert!(!install_is_a_yes(false, false, true, said), "a checkout");
        }
    }

    #[test]
    fn status_shows_the_recorded_answer_and_where_it_is_kept() {
        let kept = |yes: bool| Some((yes, "/cfg/config.json (KEY)".to_string()));
        let yes = offer_line(kept(true), Unasked::NotYet);
        assert!(
            yes.starts_with("offer: answered yes, kept in /cfg/config.json (KEY)")
                && yes.contains("a launch writes the folder again if it goes missing")
                && yes.contains("`atrium mod off`"),
            "{yes}"
        );
        assert_eq!(offer_line(kept(true), Unasked::Installed), yes);
        // A yes over a folder the user manages writes nothing, and says so.
        for (state, why) in [
            (Unasked::Env, "ATRIUM_MOD names it"),
            (Unasked::Checkout, "a git checkout"),
        ] {
            let line = offer_line(kept(true), state);
            assert!(
                line.contains("answered yes")
                    && line.contains(why)
                    && line.contains("no launch writes it")
                    && !line.contains("writes the folder again"),
                "{line}"
            );
        }
        let no = offer_line(kept(false), Unasked::NotYet);
        assert!(
            no.starts_with("offer: answered no, kept in /cfg/config.json (KEY)")
                && no.contains("`atrium mod install`"),
            "{no}"
        );
        // Nothing recorded: only where the offer will be made does it say so.
        let unasked = offer_line(None, Unasked::NotYet);
        assert!(
            unasked.starts_with("offer: not asked yet") && unasked.contains("mod.answer"),
            "{unasked}"
        );
        for (state, why) in [
            (Unasked::Env, "ATRIUM_MOD names the folder"),
            (Unasked::Managed, "\"mod\" is an object"),
            (Unasked::Checkout, "git checkout"),
            (Unasked::Installed, "atrium's marker is installed"),
        ] {
            let line = offer_line(None, state);
            assert!(
                line.starts_with("offer: not asked (") && line.contains(why),
                "{line}"
            );
            assert!(!line.contains("asks once"), "{line}");
        }
    }

    #[test]
    fn why_nobody_is_asked_follows_the_order_the_offer_decides_in() {
        assert_eq!(unasked_of(true, true, true, true), Unasked::Env);
        assert_eq!(unasked_of(false, true, true, true), Unasked::Managed);
        assert_eq!(unasked_of(false, false, true, true), Unasked::Installed);
        assert_eq!(unasked_of(false, false, true, false), Unasked::Checkout);
        assert_eq!(unasked_of(false, false, false, false), Unasked::NotYet);
    }

    #[test]
    fn the_session_line_reads_the_counters_and_says_when_there_are_none() {
        let healthy = session_line_of(r#"{"ok":true,"mod_failures":0,"mod_tripped":0}"#);
        assert!(
            healthy.contains("0 failures") && healthy.contains("closed"),
            "{healthy}"
        );
        let tripped = session_line_of(r#"{"ok":true,"mod_failures":1,"mod_tripped":1}"#);
        assert!(
            tripped.contains("1 failure reported") && tripped.contains("tripped"),
            "{tripped}"
        );
        assert!(session_line_of(r#"{"ok":true,"uptime_ms":5}"#).contains("older than 0.48"));
        assert!(session_line_of("not json").contains("older than 0.48"));
    }
}
