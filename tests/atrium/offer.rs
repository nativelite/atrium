//! The first-run offer of the mod, through a pseudo terminal: a person is
//! asked once, Enter means yes, the answer is kept, and nobody is asked where
//! nobody is there (PLAN 0.48.0, S3).
//!
//! Every test redirects the platform config directory, the state directory
//! and `PATH`, and clears `ATRIUM_MOD` (which `.cargo/config.toml` sets for
//! cargo runs and which would end the offer before it began), so nothing here
//! reads or writes the developer's own config or mod folder.
//!
//! A pty child's environment is this process's plus overrides, and a
//! variable cannot be taken out of it that way. `ATRIUM_YES` and
//! `ATRIUM_PANE` in the environment the suite runs in (a gate run from
//! inside an atrium pane has both) would each mean "nobody is there". So
//! every launch here goes through a small script that unsets both and then
//! runs atrium: the tests always run, whatever the gate's own environment.

use crate::support::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

const QUESTION: &[u8] = b"Install the atrium mod? [Y/n]";
const CONFIG_QUESTION: &[u8] = b"Global config file location";

/// A machine of its own: a platform config directory, a state directory and
/// a `PATH` that holds a `claude` (or none).
struct Lab {
    root: PathBuf,
}

/// How a launch differs from "a person at a terminal".
#[derive(Default)]
struct How<'a> {
    /// `ATRIUM_YES=1`.
    yes: bool,
    /// `ATRIUM_PANE=3`: this atrium runs inside an atrium pane.
    pane: bool,
    /// stderr goes to this file instead of the terminal.
    stderr: Option<&'a Path>,
}

impl Lab {
    fn new(name: &str, claude: bool) -> Lab {
        let root =
            std::env::temp_dir().join(format!("atrium-offer-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        if claude {
            let fake = root.join("bin").join(if cfg!(windows) {
                "claude.cmd"
            } else {
                "claude"
            });
            let text = if cfg!(windows) {
                "@echo fake\r\n"
            } else {
                "#!/bin/sh\necho fake\n"
            };
            std::fs::write(&fake, text).unwrap();
            make_executable(&fake);
        }
        Lab { root }
    }

    /// The platform config directory's `atrium` folder.
    fn atrium(&self) -> PathBuf {
        self.root.join("cfg").join("atrium")
    }

    fn mod_dir(&self) -> PathBuf {
        self.atrium().join("mod")
    }

    fn config(&self) -> PathBuf {
        self.atrium().join("config.json")
    }

    fn answer_file(&self) -> PathBuf {
        self.atrium().join("mod.answer")
    }

    /// `PATH`: the lab's own `bin`, then only what the system itself needs,
    /// so a claude installed on this machine is not found.
    fn path(&self) -> String {
        let bin = self.root.join("bin").display().to_string();
        if cfg!(windows) {
            let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
            format!("{bin};{system}\\System32;{system}")
        } else {
            format!("{bin}:/usr/bin:/bin")
        }
    }

    fn env(&self) -> Vec<(String, String)> {
        let text = |p: PathBuf| p.display().to_string();
        vec![
            ("APPDATA".to_string(), text(self.root.join("cfg"))),
            ("XDG_CONFIG_HOME".to_string(), text(self.root.join("cfg"))),
            (
                "ATRIUM_STATE_DIR".to_string(),
                text(self.root.join("state").join("sessions")),
            ),
            ("PATH".to_string(), self.path()),
            // Empty counts as unset for each of these.
            ("ATRIUM_MOD".to_string(), String::new()),
            ("ATRIUM_CONFIG".to_string(), String::new()),
            ("ATRIUM_CLAUDE_ALIASES".to_string(), String::new()),
            ("ATRIUM_RESUME_OFFER".to_string(), "0".to_string()),
        ]
    }

    /// The script a pty launch runs: it takes `ATRIUM_YES` and `ATRIUM_PANE`
    /// out of the environment (or sets them, when `how` says so), then runs
    /// atrium with its own arguments.
    fn launcher(&self, how: &How) -> PathBuf {
        let exe = env!("CARGO_BIN_EXE_atrium");
        let name = format!(
            "run-{}{}{}",
            u8::from(how.yes),
            u8::from(how.pane),
            u8::from(how.stderr.is_some())
        );
        let (path, text) = if cfg!(windows) {
            let mut text = "@echo off\r\nset ATRIUM_YES=\r\nset ATRIUM_PANE=\r\n".to_string();
            if how.yes {
                text.push_str("set ATRIUM_YES=1\r\n");
            }
            if how.pane {
                text.push_str("set ATRIUM_PANE=3\r\n");
            }
            text.push_str(&format!("\"{exe}\" %*"));
            if let Some(file) = how.stderr {
                text.push_str(&format!(" 2> \"{}\"", file.display()));
            }
            text.push_str("\r\n");
            (self.root.join(format!("{name}.cmd")), text)
        } else {
            let mut text = "#!/bin/sh\nunset ATRIUM_YES ATRIUM_PANE\n".to_string();
            if how.yes {
                text.push_str("export ATRIUM_YES=1\n");
            }
            if how.pane {
                text.push_str("export ATRIUM_PANE=3\n");
            }
            text.push_str(&format!("exec \"{exe}\" \"$@\""));
            if let Some(file) = how.stderr {
                text.push_str(&format!(" 2> \"{}\"", file.display()));
            }
            text.push('\n');
            (self.root.join(format!("{name}.sh")), text)
        };
        std::fs::write(&path, text).unwrap();
        make_executable(&path);
        path
    }

    /// A launch a person is at: `atrium <args>` in a pty.
    fn pty(&self, args: &[&str]) -> pty::Pty {
        self.pty_how(args, &How::default())
    }

    fn pty_how(&self, args: &[&str], how: &How) -> pty::Pty {
        let script = self.launcher(how).display().to_string();
        pty::Pty::spawn_with_env(&script, args, 30, 120, &self.env()).unwrap()
    }

    /// A launch nobody is at: no terminal on stdin. Its stdout and stderr.
    fn piped(&self, args: &[&str], extra: &[(&str, &str)]) -> (String, String) {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_atrium"));
        cmd.args(args)
            .envs(self.env())
            .env_remove("ATRIUM_YES")
            .env_remove("ATRIUM_PANE")
            .stdin(std::process::Stdio::null());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Every file under the platform config directory, relative to it.
    fn written(&self) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    out.push(
                        path.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root.join("cfg"), &self.root.join("cfg"), &mut out);
        out.sort();
        out
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn shown(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&strip_csi_bytes(bytes)).into_owned()
}

/// Read to the end of the launch (`fleet up` of a fleet that does not exist
/// ends right after the first-run step) and return everything it showed.
fn to_exit(p: &mut pty::Pty, mut seen: Vec<u8>) -> String {
    seen.extend(read_until(p, b"\x00never\x00", Duration::from_secs(6)));
    let _ = wait_exit(p, 15);
    shown(&seen)
}

/// A fleet launch that ends right after the first-run step: no such fleet.
const LAUNCH: &[&str] = &["fleet", "up", "no-such-fleet-for-the-offer"];

/// A new user: no config, claude on PATH. One launch asks both questions;
/// Enter to each writes the config at the default place and the mod with its
/// marker, and keeps the yes in the config just written.
#[test]
fn a_first_launch_answers_both_questions_with_enter_and_the_mod_is_installed() {
    let lab = Lab::new("enter", true);
    let mut p = lab.pty(LAUNCH);
    let mut seen = read_until(&mut p, CONFIG_QUESTION, Duration::from_secs(15));
    assert!(contains(&seen, CONFIG_QUESTION), "{}", shown(&seen));
    assert!(
        !contains(&seen, QUESTION),
        "the config question comes first"
    );
    p.write(b"\r").unwrap();
    seen.extend(read_until(&mut p, QUESTION, Duration::from_secs(15)));
    let text = shown(&seen);
    assert!(contains(&seen, QUESTION), "{text}");
    // The wording, before the question: what it is, what it changes, and
    // where the answer will be kept (the config the first question wrote).
    for piece in [
        "a Claude Code plugin",
        "status for certain",
        "takes over Claude Code's Agent tool",
        "lets Claude Code carry on",
        "atrium mod off",
        "Nothing of the mod is written without a yes",
        "Your answer, yes or no, is kept in",
        "config.json (its \"mod\" key",
    ] {
        assert!(
            contains_shown(&seen, piece.as_bytes()),
            "{piece:?} in: {text}"
        );
    }
    p.write(b"\r").unwrap();
    let text = to_exit(&mut p, seen);

    let mod_dir = lab.mod_dir();
    assert!(
        mod_dir.join(".claude-plugin/plugin.json").is_file(),
        "{text}"
    );
    assert!(mod_dir.join("hooks/hooks.json").is_file());
    let marker = std::fs::read_to_string(mod_dir.join(".atrium-mod")).unwrap();
    assert!(
        marker.contains("\"mod\"") && marker.contains("\"hash\""),
        "{marker}"
    );
    let config = std::fs::read_to_string(lab.config()).unwrap();
    assert!(
        config.contains("\"mod\": true") && config.contains("\"claude_aliases\""),
        "the yes is in the config the first question wrote: {config}"
    );
    assert!(!lab.answer_file().exists(), "one place holds the answer");
    assert!(
        text.contains("written, 0 unchanged"),
        "what it wrote is printed: {text}"
    );
    assert!(
        contains_shown(text.as_bytes(), b"atrium does not ask again"),
        "{text}"
    );

    // The next launch asks nothing.
    let mut p = lab.pty(LAUNCH);
    let text = to_exit(&mut p, Vec::new());
    assert!(!text.contains("Install the atrium mod"), "{text}");
    assert!(!text.contains("Global config file location"), "{text}");
}

/// `n` records the no, writes no mod, and a second launch asks nothing. The
/// no is `"mod": false`: off and silent.
#[test]
fn a_no_is_recorded_and_the_second_launch_asks_nothing() {
    let lab = Lab::new("no", true);
    let mut p = lab.pty(LAUNCH);
    let mut seen = read_until(&mut p, CONFIG_QUESTION, Duration::from_secs(15));
    p.write(b"\r").unwrap();
    seen.extend(read_until(&mut p, QUESTION, Duration::from_secs(15)));
    assert!(contains(&seen, QUESTION), "{}", shown(&seen));
    p.write(b"n\r").unwrap();
    let text = to_exit(&mut p, seen);
    assert!(
        !lab.mod_dir().exists(),
        "nothing of the mod is written without a yes: {text}"
    );
    let config = std::fs::read_to_string(lab.config()).unwrap();
    assert!(config.contains("\"mod\": false"), "{config}");

    let mut p = lab.pty(LAUNCH);
    let text = to_exit(&mut p, Vec::new());
    assert!(!text.contains("Install the atrium mod"), "{text}");
    assert!(!lab.mod_dir().exists());
    // And where nobody is there, a claude launch says nothing of the mod.
    let (_, err) = lab.piped(&["--allow-ctl", "claude"], &[]);
    assert!(
        !err.contains("mod: not installed"),
        "silent after a no: {err}"
    );
}

/// No terminal on stdin, or `ATRIUM_YES`: nothing is asked, nothing is
/// installed, nothing is recorded. A launch that hosts claude with the
/// control plane on says, once, that the mod is not installed.
#[test]
fn with_nobody_there_nothing_is_asked_and_nothing_is_written() {
    let lab = Lab::new("nobody", true);
    let (_, err) = lab.piped(LAUNCH, &[]);
    assert!(!err.contains("Install the atrium mod"), "{err}");
    assert!(!err.contains("Global config file location"), "{err}");
    assert_eq!(lab.written(), Vec::<String>::new(), "{err}");

    // A plain launch of claude: the terminal is never taken (there is none),
    // and the lines before it are there. Once, not per pane.
    let (_, err) = lab.piped(&["--allow-ctl", "claude"], &[]);
    assert!(!err.contains("Install the atrium mod"), "{err}");
    assert_eq!(err.matches("mod: not installed at").count(), 1, "{err}");
    assert!(err.contains("`atrium mod install` adds it"), "{err}");
    assert!(err.contains("must be a terminal"), "{err}");
    // The path a named teammate would take, told as it is: no mod, the shim,
    // and a teams variable atrium cannot see.
    assert!(
        err.contains("atrium: teams: the mod is off; Claude Code's own teammates land as panes through the tmux shim")
            && err.contains("atrium cannot see it in its environment"),
        "{err}"
    );
    let (_, err) = lab.piped(
        &["--allow-ctl", "claude"],
        &[("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1")],
    );
    assert!(!err.contains("atrium cannot see it"), "{err}");
    // Not for a command that is not claude, and not without the control plane.
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    for args in [&["--allow-ctl", shell][..], &["claude"][..]] {
        let (_, err) = lab.piped(args, &[]);
        assert!(
            !err.contains("mod:") && !err.contains("teams:"),
            "{args:?}: {err}"
        );
    }
    assert_eq!(lab.written(), Vec::<String>::new());

    // A terminal, and `ATRIUM_YES`: the same nothing.
    let mut p = lab.pty_how(
        LAUNCH,
        &How {
            yes: true,
            ..How::default()
        },
    );
    let text = to_exit(&mut p, Vec::new());
    assert!(text.contains("no fleet named"), "{text}");
    assert!(!text.contains("Install the atrium mod"), "{text}");
    assert_eq!(lab.written(), Vec::<String>::new(), "{text}");
}

/// The question is written to stderr. With stderr redirected nobody would
/// see it, atrium would wait on a blank screen, and the Enter pressed at an
/// apparent hang would install the mod: so with stdin a terminal and stderr
/// a file nothing is asked and nothing is written.
#[test]
fn with_stderr_redirected_nothing_is_asked_and_nothing_is_written() {
    let lab = Lab::new("stderr", true);
    let log = lab.root.join("err.log");
    let how = How {
        stderr: Some(&log),
        ..How::default()
    };
    // No config: neither question may wait for a line nobody was shown.
    let mut p = lab.pty_how(LAUNCH, &how);
    let text = to_exit(&mut p, Vec::new());
    let err = std::fs::read_to_string(&log).unwrap();
    assert!(
        err.contains("no fleet named"),
        "the launch ran to its end: {err}"
    );
    for (what, said) in [("the terminal", &text), ("the file", &err)] {
        assert!(!said.contains("Install the atrium mod"), "{what}: {said}");
        assert!(!said.contains("Global config file"), "{what}: {said}");
    }
    assert_eq!(lab.written(), Vec::<String>::new());
    // With a config there (so only the mod question is in play): the same.
    std::fs::create_dir_all(lab.atrium()).unwrap();
    std::fs::write(lab.config(), "{}").unwrap();
    let mut p = lab.pty_how(LAUNCH, &how);
    let text = to_exit(&mut p, Vec::new());
    let err = std::fs::read_to_string(&log).unwrap();
    assert!(err.contains("no fleet named"), "{err}");
    assert!(!err.contains("Install the atrium mod"), "{err}");
    assert!(!text.contains("Install the atrium mod"), "{text}");
    assert_eq!(lab.written(), ["atrium/config.json"]);
    assert_eq!(std::fs::read_to_string(lab.config()).unwrap(), "{}");
}

/// Keys pressed before the question is on screen are not its answer. A path
/// pasted at the config question with extra Enters behind it answers the
/// config question only: the mod question still waits, and a real `n` is
/// then taken.
#[test]
fn what_is_typed_ahead_does_not_answer_the_question() {
    let lab = Lab::new("ahead", true);
    let mut p = lab.pty(LAUNCH);
    let mut seen = read_until(&mut p, CONFIG_QUESTION, Duration::from_secs(15));
    assert!(contains(&seen, CONFIG_QUESTION), "{}", shown(&seen));
    // One Enter for the config question, and two more typed ahead.
    p.write(b"\r\r\r").unwrap();
    seen.extend(read_until(&mut p, QUESTION, Duration::from_secs(15)));
    assert!(contains(&seen, QUESTION), "{}", shown(&seen));
    // The question is still waiting: nothing was taken as its answer.
    let after = read_until(&mut p, b"does not ask again", Duration::from_secs(3));
    let text = shown(&after);
    assert!(
        !text.contains("written,") && !text.contains("does not ask again"),
        "an Enter typed ahead answered the question: {text}"
    );
    assert!(p.try_wait().unwrap().is_none(), "atrium is still asking");
    assert!(!lab.mod_dir().exists(), "{text}");
    assert!(lab.config().is_file(), "the config question was answered");
    // A real answer is taken.
    p.write(b"n\r").unwrap();
    seen.extend(after);
    let text = to_exit(&mut p, seen);
    let config = std::fs::read_to_string(lab.config()).unwrap();
    assert!(config.contains("\"mod\": false"), "{config}\n{text}");
    assert!(!lab.mod_dir().exists());
}

/// An atrium started inside an atrium pane is not the first start, and what
/// types into a pane may be an agent: nothing is asked there, though both
/// its streams are terminals.
#[test]
fn an_atrium_inside_a_pane_is_not_asked() {
    let lab = Lab::new("pane", true);
    std::fs::create_dir_all(lab.atrium()).unwrap();
    std::fs::write(lab.config(), "{}").unwrap();
    let mut p = lab.pty_how(
        LAUNCH,
        &How {
            pane: true,
            ..How::default()
        },
    );
    let text = to_exit(&mut p, Vec::new());
    assert!(text.contains("no fleet named"), "{text}");
    assert!(!text.contains("Install the atrium mod"), "{text}");
    assert_eq!(lab.written(), ["atrium/config.json"]);
    assert_eq!(std::fs::read_to_string(lab.config()).unwrap(), "{}");
    // The same launch outside a pane is asked: the pane is what decided.
    let mut p = lab.pty(LAUNCH);
    let seen = read_until(&mut p, QUESTION, Duration::from_secs(15));
    assert!(contains(&seen, QUESTION), "{}", shown(&seen));
    p.write(b"n\r").unwrap();
    let _ = to_exit(&mut p, seen);
}

/// An existing user: a config they formatted by hand, no answer in it. The
/// next launch with a person at it asks the one question, once; the yes goes
/// into their file as one key and every other byte stays.
#[test]
fn an_existing_config_with_no_answer_is_asked_once_and_edited_by_one_key() {
    let lab = Lab::new("existing", true);
    let hand = "{\n\t\"deny\" : [ \"Bash(rm -rf *)\" ],\n\n    \"notify\":\"bell\"\n}\n";
    std::fs::create_dir_all(lab.atrium()).unwrap();
    std::fs::write(lab.config(), hand).unwrap();

    let mut p = lab.pty(LAUNCH);
    let seen = read_until(&mut p, QUESTION, Duration::from_secs(15));
    assert!(contains(&seen, QUESTION), "{}", shown(&seen));
    assert!(
        !contains(&seen, CONFIG_QUESTION),
        "a config is there: {}",
        shown(&seen)
    );
    p.write(b"y\r").unwrap();
    let text = to_exit(&mut p, seen);
    assert_eq!(
        std::fs::read_to_string(lab.config()).unwrap(),
        hand.replacen("{\n\t", "{\n\t\"mod\": true,\n\t", 1),
        "{text}"
    );
    assert!(lab.mod_dir().join(".atrium-mod").is_file(), "{text}");

    let mut p = lab.pty(LAUNCH);
    let text = to_exit(&mut p, Vec::new());
    assert!(!text.contains("Install the atrium mod"), "{text}");
}

/// Without Claude Code on PATH there is nothing to offer a plugin to:
/// nothing is asked and nothing recorded, so the question comes when claude
/// appears.
#[test]
fn without_claude_on_path_nothing_is_asked_or_recorded() {
    let lab = Lab::new("noclaude", false);
    std::fs::create_dir_all(lab.atrium()).unwrap();
    std::fs::write(lab.config(), "{}").unwrap();
    let mut p = lab.pty(LAUNCH);
    let text = to_exit(&mut p, Vec::new());
    assert!(text.contains("no fleet named"), "{text}");
    assert!(!text.contains("Install the atrium mod"), "{text}");
    assert_eq!(lab.written(), ["atrium/config.json"]);
    assert_eq!(std::fs::read_to_string(lab.config()).unwrap(), "{}");
}

/// A plain launch asks too, before the terminal is taken, and with no config
/// file to keep the answer in it goes to the answer file. The session then
/// starts as usual.
#[test]
fn a_plain_launch_asks_before_the_terminal_is_taken_and_keeps_the_answer_beside_the_config() {
    let lab = Lab::new("plain", true);
    let (shell, bar): (&[&str], &[u8]) = if cfg!(windows) {
        (&["cmd", "/Q"], b"1:cmd")
    } else {
        (&["sh", "-i"], b"1:sh")
    };
    let mut p = lab.pty(shell);
    let seen = read_until(&mut p, QUESTION, Duration::from_secs(15));
    assert!(contains(&seen, QUESTION), "{}", shown(&seen));
    assert!(!contains(&seen, bar), "asked before the session is drawn");
    assert!(
        !contains(&seen, CONFIG_QUESTION),
        "a plain launch asks for no config"
    );
    assert!(
        contains_shown(&seen, b"mod.answer (one word)"),
        "where the answer goes is said first: {}",
        shown(&seen)
    );
    p.write(b"n\r").unwrap();
    let after = read_until(&mut p, bar, Duration::from_secs(15));
    assert!(
        contains(&after, bar),
        "the session starts: {}",
        shown(&after)
    );
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
    assert_eq!(
        std::fs::read_to_string(lab.answer_file()).unwrap().trim(),
        "no"
    );
    assert!(
        !lab.config().exists(),
        "no config is made to hold an answer"
    );
    assert!(!lab.mod_dir().exists());

    // Asked once: the next plain launch goes straight to the session.
    let mut p = lab.pty(shell);
    let seen = read_until(&mut p, bar, Duration::from_secs(15));
    assert!(contains(&seen, bar), "{}", shown(&seen));
    assert!(!contains(&seen, QUESTION), "{}", shown(&seen));
    p.write(b"\x01q").unwrap();
    let _ = wait_exit(&mut p, 15);
}

/// `atrium mod off` is a no without hand-editing: kept exactly as answering
/// `n` keeps it, shown by `atrium mod status`, and turned around by `atrium
/// mod install`.
#[test]
fn mod_off_records_a_no_and_mod_install_turns_it_around() {
    let lab = Lab::new("off", true);
    // No config file: the answer file.
    let (out, err) = lab.piped(&["mod", "off"], &[]);
    assert!(
        out.contains("the no is kept in") && out.contains("mod.answer (no)"),
        "{out}{err}"
    );
    assert_eq!(lab.written(), ["atrium/mod.answer"]);
    let (status, _) = lab.piped(&["mod", "status"], &[]);
    assert!(status.contains("offer: answered no, kept in"), "{status}");
    assert!(status.contains("enabled: no"), "{status}");
    // A person at the next start is not asked: the answer is there.
    let mut p = lab.pty(LAUNCH);
    let seen = read_until(&mut p, CONFIG_QUESTION, Duration::from_secs(15));
    p.write(b"\r").unwrap();
    let text = to_exit(&mut p, seen);
    assert!(!text.contains("Install the atrium mod"), "{text}");
    assert!(!lab.mod_dir().exists());

    // `mod install` says yes again, and the yes is kept where the config
    // the launch above wrote can hold it: its one key.
    let (out, err) = lab.piped(&["mod", "install"], &[]);
    assert!(
        out.contains("the mod was off on this machine and is on again"),
        "{out}{err}"
    );
    assert!(lab.mod_dir().join(".atrium-mod").is_file());
    let (status, _) = lab.piped(&["mod", "status"], &[]);
    assert!(
        status.contains("offer: answered yes, kept in") && status.contains("`atrium mod off`"),
        "{status}"
    );
    assert!(status.contains("enabled: yes"), "{status}");

    // And off again, with a config there: its one key.
    let before = std::fs::read_to_string(lab.config()).unwrap();
    let (out, err) = lab.piped(&["mod", "off"], &[]);
    assert!(out.contains("config.json (\"mod\": false)"), "{out}{err}");
    assert_eq!(
        std::fs::read_to_string(lab.config()).unwrap(),
        before.replace("\"mod\": true", "\"mod\": false")
    );
    assert!(
        lab.mod_dir().join(".atrium-mod").is_file(),
        "the folder is left as it is"
    );
    let (_, err) = lab.piped(&["mod", "off", "now"], &[]);
    assert!(err.contains("unexpected argument"), "{err}");
}
