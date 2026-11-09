//! atrium answering as `tmux`, for Claude Code's agent teams.
//!
//! Claude Code's split-pane teams do not draw panes. The lead shells out to a
//! program named `tmux` and asks it to split the window and run the teammate
//! (`split-window -d -t @0 -h -l 70% -P -F '#{pane_id}' -- claude --agent-name …`),
//! then reads the new pane's id back. Its backend registry knows two pane
//! managers, tmux and iTerm2, and it picks tmux whenever `TMUX` is set in the
//! lead's environment. There is no setting that names another program.
//!
//! So atrium answers to the name. Under `--allow-ctl`, a claude pane is born
//! with `TMUX`, `TMUX_PANE` and a `PATH` that starts with a directory holding
//! `tmux` (`tmux.exe` on Windows: Claude Code is a native executable there and
//! `CreateProcess` resolves executables only, never `.cmd` shims). That file is
//! atrium's own binary under a second name; [`invoked_as_shim`] notices the
//! name and [`main`] serves the verbs. The protocol half, parsing the argv,
//! expanding `#{…}` formats, splitting the shell-quoted command and running a
//! verb against a pane backend, is the shared [`tmuxproto`] crate, which
//! fwterm uses too; this module is atrium's backend ([`CtlBroker`]: each verb
//! is one `atrium ctl` request with the caller's own token), the pane
//! environment, and the shim file's placement.
//!
//! A teammate therefore becomes an ordinary atrium pane: a child of the lead
//! in the spawn tree, under its trust ceiling, with the status border, the
//! overview row, the board and the bus.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub use tmuxproto::{
    agent_name, expand, needs_broker, parse, parse_target, run, sh_words, Broker, Host, Node,
    Outcome, PaneView, Target, Verb, View, WINDOW_ID,
};

/// Who answers: `#{session_name}` is `atrium`, `tmux -V` says `tmux 3.5a-atrium`.
pub const HOST: Host = Host {
    session_name: "atrium",
    version_suffix: "atrium",
};

pub const ENV_TMUX: &str = "TMUX";
pub const ENV_TMUX_PANE: &str = "TMUX_PANE";
/// Set beside `TMUX` so Claude Code keeps truecolor: with `TMUX` alone it
/// drops to 256 colours, assuming a real tmux in between.
pub const ENV_CLAUDE_TRUECOLOR: &str = "CLAUDE_CODE_TMUX_TRUECOLOR";
/// The hidden flag `atrium --tmux-shim <tmux args…>` that reaches [`main`]
/// without renaming the binary (tests, and a hand probe).
pub const FLAG: &str = "--tmux-shim";

/// Claude Code's own flag for how teammates are shown. atrium passes it as
/// `auto` (split panes when inside tmux, which an atrium claude pane now is;
/// in-process otherwise) so nobody edits `settings.json` to see teammates.
pub const TEAMMATE_MODE_FLAG: &str = "--teammate-mode";

/// The first Claude Code that took `--teammate-mode` on the command line
/// (it is hidden from `--help`, so the version is the probe).
pub const TEAMMATE_MODE_SINCE: (u64, u64, u64) = (2, 1, 50);

/// Is the first `major.minor.patch` in `version_output` at least `min`?
/// `claude --version` prints `2.1.289 (Claude Code)`.
pub fn version_at_least(version_output: &str, min: (u64, u64, u64)) -> bool {
    let mut nums = [0u64; 3];
    let mut i = 0;
    let mut cur: Option<u64> = None;
    for c in version_output.chars() {
        match c {
            '0'..='9' => cur = Some(cur.unwrap_or(0) * 10 + (c as u64 - '0' as u64)),
            '.' if cur.is_some() && i < 2 => {
                nums[i] = cur.take().unwrap_or(0);
                i += 1;
            }
            _ if cur.is_some() => break,
            _ => {}
        }
    }
    if let Some(n) = cur {
        if i < 3 {
            nums[i] = n;
            i += 1;
        }
    }
    i == 3 && (nums[0], nums[1], nums[2]) >= min
}

/// Should atrium add `--teammate-mode auto` to this argv? Not when the user
/// already passes the flag: theirs wins.
pub fn wants_teammate_mode(argv: &[String]) -> bool {
    !argv
        .iter()
        .any(|a| a == TEAMMATE_MODE_FLAG || a.starts_with("--teammate-mode="))
}

/// The real broker: `atrium ctl` requests over the session's channel, with
/// the caller's token from the environment (inherited from the claude pane).
pub struct CtlBroker {
    address: String,
    caller: Option<usize>,
}

impl CtlBroker {
    pub fn from_env() -> Result<CtlBroker, String> {
        let address = std::env::var(crate::ctl::ENV_ADDRESS)
            .ok()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                format!(
                    "not inside a ctl-enabled atrium pane ({} unset)",
                    crate::ctl::ENV_ADDRESS
                )
            })?;
        let caller = std::env::var(crate::ctl::ENV_PANE)
            .ok()
            .and_then(|p| p.parse().ok());
        Ok(CtlBroker { address, caller })
    }

    fn call(&self, args: &[&str]) -> Result<json::Value, String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let line =
            crate::ctl::build_request(&owned, self.caller).map_err(|e| e.message().to_string())?;
        let reply = crate::ipc::request(&self.address, &line).map_err(|e| e.to_string())?;
        let v = json::parse(&reply).map_err(|_| "unreadable reply from atrium".to_string())?;
        if v.get("ok").and_then(json::Value::as_bool) != Some(true) {
            let msg = v
                .get("err")
                .and_then(json::Value::as_str)
                .unwrap_or("request refused");
            return Err(msg.to_string());
        }
        Ok(v)
    }
}

impl Broker for CtlBroker {
    fn caller(&self) -> Option<usize> {
        self.caller
    }
    fn list(&mut self) -> Result<Vec<Node>, String> {
        let v = self.call(&["list"])?;
        let tree = v.get("tree").and_then(json::Value::as_array).unwrap_or(&[]);
        Ok(tree
            .iter()
            .filter_map(|n| {
                let id = n.get("id").and_then(json::Value::as_i64)?;
                let parent = n
                    .get("parent")
                    .and_then(json::Value::as_i64)
                    .and_then(|p| usize::try_from(p).ok());
                let role = n
                    .get("role")
                    .and_then(json::Value::as_str)
                    .map(str::to_string);
                let title = n
                    .get("title")
                    .and_then(json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                Some((usize::try_from(id).ok()?, parent, role, title))
            })
            .collect())
    }
    fn spawn(&mut self, here: bool, role: Option<&str>, argv: &[String]) -> Result<usize, String> {
        let mut args: Vec<&str> = vec!["spawn"];
        args.push(if here { "--here" } else { "--window" });
        if let Some(r) = role {
            args.push("--role");
            args.push(r);
        }
        args.push("--");
        args.extend(argv.iter().map(String::as_str));
        let v = self.call(&args)?;
        v.get("pane")
            .and_then(json::Value::as_i64)
            .and_then(|p| usize::try_from(p).ok())
            .ok_or_else(|| "spawn reply named no pane".to_string())
    }
    fn kill(&mut self, id: usize) -> Result<(), String> {
        let t = id.to_string();
        self.call(&["kill", &t]).map(|_| ())
    }
    fn send(&mut self, id: usize, text: &str) -> Result<(), String> {
        let t = id.to_string();
        self.call(&["send", &t, text]).map(|_| ())
    }
}

/// True when this process was started under the name `tmux`: the copy of
/// atrium's binary that [`ensure`] placed on the panes' `PATH`.
pub fn invoked_as_shim() -> bool {
    let by_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_os_string()))
        .is_some_and(|s| s.eq_ignore_ascii_case("tmux"));
    let by_argv0 = std::env::args_os().next().is_some_and(|a| {
        Path::new(&a)
            .file_stem()
            .is_some_and(|s| s.eq_ignore_ascii_case("tmux"))
    });
    by_exe || by_argv0
}

/// Entry point for the shim: parse, serve, print, exit as tmux would.
pub fn main(args: &[String]) -> ExitCode {
    let verb = parse(args);
    let outcome = if needs_broker(&verb) {
        match CtlBroker::from_env() {
            Ok(mut b) => run(verb, &mut b, HOST),
            Err(e) => Outcome::err(format!("tmux (atrium): {e}")),
        }
    } else {
        run(verb, &mut tmuxproto::NoBroker, HOST)
    };
    if !outcome.stdout.is_empty() {
        print!("{}", outcome.stdout);
    }
    if !outcome.stderr.is_empty() {
        eprintln!("{}", outcome.stderr.trim_end());
    }
    ExitCode::from(outcome.code)
}

/// Where the shim lives: beside the sessions store, under atrium's per-user
/// state directory (`%LOCALAPPDATA%\atrium\shim`, `~/.local/state/atrium/shim`).
/// `root` is [`crate::session_store::state_root`], the sessions directory
/// itself, so the shim is its sibling and never a stray entry in the store.
pub fn shim_dir(root: &Path) -> PathBuf {
    root.parent().unwrap_or(root).join("shim")
}

/// The shim's file name on this platform.
pub fn shim_file_name() -> &'static str {
    if cfg!(windows) {
        "tmux.exe"
    } else {
        "tmux"
    }
}

/// Make `<state>/shim/tmux[.exe]` a current copy of `exe` (atrium itself). A
/// hard link when the filesystem allows, a copy otherwise; refreshed when the
/// installed atrium changes (size or mtime), kept when it is already current.
/// A shim that cannot be replaced because it is running is left as it is.
pub fn ensure(exe: &Path, root: &Path) -> Result<PathBuf, String> {
    let dir = shim_dir(root);
    crate::session_store::ensure_private_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let shim = dir.join(shim_file_name());
    let want = std::fs::metadata(exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    if let Ok(have) = std::fs::metadata(&shim) {
        let same = have.len() == want.len() && have.modified().ok() == want.modified().ok();
        if same {
            return Ok(shim);
        }
        if std::fs::remove_file(&shim).is_err() {
            // In use (Windows locks a running image): serve the old one.
            return Ok(shim);
        }
    }
    if std::fs::hard_link(exe, &shim).is_ok() {
        return Ok(shim);
    }
    std::fs::copy(exe, &shim)
        .map(|_| shim.clone())
        .map_err(|e| format!("{}: {e}", shim.display()))
}

/// The environment a claude pane gets so Claude Code's teams backend finds
/// atrium: `TMUX=<ctl address>,<atrium pid>,0` (the first field is what Claude
/// Code reads as the socket), `TMUX_PANE=%<pane>`, truecolor kept, and `PATH`
/// led by the shim's directory. Empty when there is no shim to lead with: a
/// `TMUX` that points at nothing would send Claude Code looking for a tmux
/// that is not there, and dim its colours on the way.
pub fn pane_env(
    address: &str,
    pane: &str,
    shim: Option<&Path>,
    path: Option<&OsStr>,
) -> Vec<(String, String)> {
    let Some(shim) = shim else {
        return Vec::new();
    };
    let Some(dir) = shim.parent() else {
        return Vec::new();
    };
    let mut new_path = dir.to_string_lossy().into_owned();
    if let Some(old) = path.map(|p| p.to_string_lossy()) {
        if !old.is_empty() {
            new_path.push(if cfg!(windows) { ';' } else { ':' });
            new_path.push_str(&old);
        }
    }
    vec![
        (
            ENV_TMUX.to_string(),
            format!("{address},{},0", std::process::id()),
        ),
        (ENV_TMUX_PANE.to_string(), format!("%{pane}")),
        (ENV_CLAUDE_TRUECOLOR.to_string(), "1".to_string()),
        ("PATH".to_string(), new_path),
    ]
}

/// The `teams` key, in `config.json` and per fleet: `"show"` (or `true`, or
/// absent) catches a Claude Code lead's teammates as atrium panes; `"off"` (or
/// `false`) sets no `TMUX` and prepends nothing, so Claude Code's teams run
/// in-process as they did before. `Ok(None)` is "not said"; `Err` names the
/// accepted spellings for the caller's error.
pub fn parse_setting(v: Option<&json::Value>) -> Result<Option<bool>, &'static str> {
    match v {
        None | Some(json::Value::Null) => Ok(None),
        Some(json::Value::Bool(b)) => Ok(Some(*b)),
        Some(json::Value::String(s)) if s == "show" => Ok(Some(true)),
        Some(json::Value::String(s)) if s == "off" => Ok(Some(false)),
        Some(_) => Err("\"teams\" must be \"show\" or \"off\""),
    }
}

static SESSION_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Record whether this session catches Claude Code's teammates (a fleet's
/// `teams: off` turns it off). First call wins, like the mod's switch.
pub fn set_session_enabled(on: bool) {
    let _ = SESSION_ENABLED.set(on);
}

/// Does this session put the shim into claude panes? The fleet's say if it
/// spoke, else the config's `teams` key, else yes.
pub fn session_enabled() -> bool {
    match SESSION_ENABLED.get() {
        Some(on) => *on,
        None => !crate::config::get().teams_off,
    }
}

/// One line for the fleet banner and preflight.
pub fn describe(on: bool) -> &'static str {
    if on {
        "teams: Claude Code's teammates land as panes (teammateMode auto or tmux)"
    } else {
        "teams: off (Claude Code's teammates stay in-process)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_host_answers_as_atrium() {
        let out = run(parse(&a("-V")), &mut tmuxproto::NoBroker, HOST);
        assert_eq!(out.stdout, "tmux 3.5a-atrium\n");
        let view = View {
            host: HOST,
            caller: Some(3),
            panes: vec![PaneView {
                id: 3,
                title: "claude".into(),
                role: None,
            }],
        };
        assert_eq!(
            expand(
                "#{session_name}:#{window_id}.#{pane_id}",
                view.pane(3),
                &view
            ),
            "atrium:@0.%3"
        );
    }

    #[test]
    fn pane_env_leads_path_with_the_shim_or_sets_nothing() {
        let shim = Path::new(if cfg!(windows) {
            r"C:\s\shim\tmux.exe"
        } else {
            "/s/shim/tmux"
        });
        let env = pane_env("addr", "7", Some(shim), Some(OsStr::new("old")));
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert!(get("TMUX").unwrap().starts_with("addr,"));
        assert_eq!(get("TMUX_PANE").as_deref(), Some("%7"));
        assert_eq!(get("CLAUDE_CODE_TMUX_TRUECOLOR").as_deref(), Some("1"));
        let path = get("PATH").unwrap();
        assert!(
            path.starts_with(&shim.parent().unwrap().to_string_lossy().into_owned()),
            "{path}"
        );
        assert!(path.ends_with("old"));
        assert!(pane_env("addr", "7", None, None).is_empty());
    }

    #[test]
    fn ensure_links_or_copies_the_binary_and_keeps_a_current_one() {
        let root = std::env::temp_dir().join(format!("atrium-shim-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join("atrium-fake.exe");
        std::fs::write(&exe, b"binary").unwrap();
        let sessions = root.join("sessions");
        let shim = ensure(&exe, &sessions).unwrap();
        assert_eq!(shim.parent().unwrap(), root.join("shim"));
        assert_eq!(
            shim.file_name().unwrap().to_str().unwrap(),
            shim_file_name()
        );
        assert_eq!(std::fs::read(&shim).unwrap(), b"binary");
        let again = ensure(&exe, &sessions).unwrap();
        assert_eq!(again, shim);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_teams_setting_is_show_off_a_bool_or_an_error() {
        let p = |t: &str| parse_setting(json::parse(t).ok().as_ref());
        assert_eq!(p("null"), Ok(None));
        assert_eq!(p("\"show\""), Ok(Some(true)));
        assert_eq!(p("true"), Ok(Some(true)));
        assert_eq!(p("\"off\""), Ok(Some(false)));
        assert_eq!(p("false"), Ok(Some(false)));
        assert!(p("\"yes\"").is_err());
        assert!(p("1").is_err());
        assert_eq!(parse_setting(None), Ok(None));
    }

    #[test]
    fn the_version_floor_reads_claude_version() {
        assert!(version_at_least(
            "2.1.289 (Claude Code)",
            TEAMMATE_MODE_SINCE
        ));
        assert!(version_at_least("2.1.50", (2, 1, 50)));
        assert!(!version_at_least("2.1.49 (Claude Code)", (2, 1, 50)));
        assert!(!version_at_least("1.0.128", (2, 1, 50)));
        assert!(!version_at_least("garbage", (2, 1, 50)));
        assert!(version_at_least("v3.0.0", (2, 1, 50)));
    }

    #[test]
    fn a_users_teammate_mode_flag_wins() {
        assert!(wants_teammate_mode(&a("claude --session-id x")));
        assert!(!wants_teammate_mode(&a(
            "claude --teammate-mode in-process"
        )));
        assert!(!wants_teammate_mode(&a("claude --teammate-mode=tmux")));
    }
}
