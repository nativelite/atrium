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
            match modfiles::install_at(&dir) {
                Ok(done) => {
                    println!(
                        "mod {} at {}: {} written, {} unchanged",
                        modfiles::embedded_version(),
                        done.dir.display(),
                        done.written.len(),
                        done.unchanged.len()
                    );
                    // A fresh install is a fresh start: whatever the breaker
                    // remembered was about what was there before.
                    if let Some(root) = atrium::session_store::state_root() {
                        if !matches!(modfiles::read_record(&root), modfiles::RecordRead::None)
                            && modfiles::clear_record(&root)
                        {
                            println!(
                                "the breaker's record was cleared: the next session loads the \
                                 mod again"
                            );
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
            let (line, _) = modfiles::describe(&modfiles::status(), enabled);
            println!("{line}");
            println!(
                "enabled: {}",
                if enabled {
                    "yes (a fleet may still say \"mod\": false for its own session)"
                } else {
                    "no: \"mod\": false in config.json; nothing is loaded, checked or said"
                }
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
