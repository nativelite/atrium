//! Claude Code's own agent teams, read from the files Claude Code keeps for
//! them, so the overview can show a lead's task list beside its teammates.
//!
//! Claude Code (2.1.289, read from its binary on 2026-11-09; no team was on
//! disk to copy) keeps a team under `<claude root>/teams/<team>/`:
//! `config.json` names the lead and the members
//! (`{name, createdAt, leadAgentId, leadSessionId, members: [{agentId, name,
//! agentType, joinedAt, tmuxPaneId, cwd, subscriptions, backendType}]}`) and
//! `inboxes/<agent>.json` holds each member's mailbox. The team's task list is
//! `<claude root>/tasks/<team>/<id>.json`, one file per task
//! (`{id, subject, description, activeForm?, owner?, status, blocks,
//! blockedBy, metadata?}`, `status` one of `pending`, `in_progress`,
//! `completed`), beside a `.highwatermark` file that is not a task. A lead's
//! team is named `session-` plus the first eight characters of its session id
//! unless it was handed a name; the team directory goes when the lead's
//! session ends and the tasks directory stays, so a team is live while its
//! `config.json` exists. The claude root is `$CLAUDE_CONFIG_DIR` when set, else
//! `~/.claude`.
//!
//! This module only reads. [`TeamWatch`] looks at most once a second: a stat
//! of `config.json` and a listing of the tasks directory, whose entries carry
//! the stamp (mtime and length) a listing already pays for on Windows, and it
//! re-reads only a file whose stamp changed. A directory mtime alone would miss
//! a task rewritten in place, which is how a status changes.
//!
//! The live snapshot is process-wide ([`refresh`] / [`snapshot`]): the run loop
//! refreshes it in its agent-refresh phase and the overview reads it, without
//! the renderer having to carry one more argument down to the panel.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// What Claude Code prefixes a session-named team with.
pub const TEAM_PREFIX: &str = "session-";

/// The claude config root: `$CLAUDE_CONFIG_DIR` when set and non-empty (claude
/// honors it), else `~/.claude` under `USERPROFILE` / `HOME`.
pub fn claude_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".claude"))
}

/// The team a lead with this session id runs: `session-` plus the first eight
/// characters of the id (Claude Code: `` `session-${sessionId.slice(0,8)}` ``).
pub fn team_name_for(session_id: &str) -> String {
    let short: String = session_id.chars().take(8).collect();
    format!("{TEAM_PREFIX}{short}")
}

/// Claude Code's directory name for a team: every character outside
/// `[A-Za-z0-9]` becomes `-`, then lowercased.
pub fn team_dir_name(team: &str) -> String {
    team.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Claude Code's directory name for a task list, and a task file's stem: every
/// character outside `[A-Za-z0-9_-]` becomes `-`.
pub fn task_key(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `<root>/teams/<team>`: where the team's `config.json` and inboxes live.
pub fn team_dir(root: &Path, team: &str) -> PathBuf {
    root.join("teams").join(team_dir_name(team))
}

/// `<root>/tasks/<team>`: the team's task list, one file per task.
pub fn tasks_dir(root: &Path, team: &str) -> PathBuf {
    root.join("tasks").join(task_key(team))
}

/// The value after `--team-name` in a teammate's argv, which Claude Code passes
/// through the tmux shim: the team the pane belongs to.
pub fn team_name_arg(argv: &[String]) -> Option<String> {
    argv.windows(2)
        .find(|w| w[0] == "--team-name")
        .map(|w| w[1].clone())
        .or_else(|| {
            argv.iter()
                .find_map(|a| a.strip_prefix("--team-name=").map(str::to_string))
        })
}

/// The live team directory of the lead whose session id this is: the derived
/// team's directory under the claude root, when its `config.json` exists.
pub fn locate(session_id: &str) -> Option<PathBuf> {
    locate_in(&claude_root()?, session_id)
}

/// [`locate`] under an explicit root.
pub fn locate_in(root: &Path, session_id: &str) -> Option<PathBuf> {
    let dir = team_dir(root, &team_name_for(session_id));
    dir.join("config.json").is_file().then_some(dir)
}

/// A task's state, as Claude Code's task files spell it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    /// `pending` / `in_progress` / `completed`; anything else is not a task.
    pub fn parse(s: &str) -> Option<TaskStatus> {
        match s {
            "pending" => Some(TaskStatus::Pending),
            "in_progress" => Some(TaskStatus::InProgress),
            "completed" => Some(TaskStatus::Completed),
            _ => None,
        }
    }

    /// The checkbox the overview draws: `[ ]`, `[~]`, `[x]`.
    pub fn mark(self) -> &'static str {
        match self {
            TaskStatus::Pending => "[ ]",
            TaskStatus::InProgress => "[~]",
            TaskStatus::Completed => "[x]",
        }
    }
}

/// One task of a team's list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub status: TaskStatus,
    /// The teammate's name this task is claimed by, if any.
    pub owner: Option<String>,
    /// Ids of the tasks that must complete before this one can start.
    pub blocked_by: Vec<String>,
    /// The present-tense form Claude Code shows while the task runs.
    pub active_form: Option<String>,
}

/// One member of the team: its name (the pane's role, from `--agent-name`) and
/// its agent id (`<name>@<team>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub agent_id: String,
}

/// A snapshot of one live team: who is in it and what its task list says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TeamState {
    pub name: String,
    pub lead_agent_id: Option<String>,
    pub members: Vec<Member>,
    /// In id order (numeric ids numerically, then the rest lexically).
    pub tasks: Vec<Task>,
}

impl TeamState {
    /// The team's `config.json`: its name, lead and members. `None` when the
    /// text is not a JSON object; a member without a name is skipped.
    pub fn parse_config(text: &str) -> Option<TeamState> {
        let v = json::parse(text).ok()?;
        v.as_object()?;
        let members = v
            .get("members")
            .and_then(json::Value::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(|m| {
                Some(Member {
                    name: m.get("name")?.as_str()?.to_string(),
                    agent_id: m
                        .get("agentId")
                        .and_then(json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect();
        Some(TeamState {
            name: v
                .get("name")
                .and_then(json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            lead_agent_id: v
                .get("leadAgentId")
                .and_then(json::Value::as_str)
                .map(str::to_string),
            members,
            tasks: Vec::new(),
        })
    }

    /// `owner`'s current task: the one in progress, else one it has claimed but
    /// not started. Its subject is the overview caption when the mod gave none.
    pub fn task_for(&self, owner: &str) -> Option<&Task> {
        let owned = |t: &&Task| t.owner.as_deref() == Some(owner);
        self.tasks
            .iter()
            .filter(owned)
            .find(|t| t.status == TaskStatus::InProgress)
            .or_else(|| {
                self.tasks
                    .iter()
                    .filter(owned)
                    .find(|t| t.status == TaskStatus::Pending)
            })
    }

    /// Is `task` waiting on another task of this list that is not completed? A
    /// dependency that is no longer in the list does not block.
    pub fn blocked(&self, task: &Task) -> bool {
        task.blocked_by.iter().any(|id| {
            self.tasks
                .iter()
                .any(|t| t.id == *id && t.status != TaskStatus::Completed)
        })
    }

    /// `(pending, in progress, completed)`.
    pub fn counts(&self) -> (usize, usize, usize) {
        self.tasks
            .iter()
            .fold((0, 0, 0), |(p, i, c), t| match t.status {
                TaskStatus::Pending => (p + 1, i, c),
                TaskStatus::InProgress => (p, i + 1, c),
                TaskStatus::Completed => (p, i, c + 1),
            })
    }
}

/// One task file. `None` unless it is a JSON object with a string `id` and
/// `subject` and a known `status`; the other keys are optional.
pub fn parse_task(text: &str) -> Option<Task> {
    let v = json::parse(text).ok()?;
    v.as_object()?;
    let strings = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(json::Value::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect()
    };
    Some(Task {
        id: v.get("id")?.as_str()?.to_string(),
        subject: v.get("subject")?.as_str()?.to_string(),
        status: TaskStatus::parse(v.get("status")?.as_str()?)?,
        owner: v
            .get("owner")
            .and_then(json::Value::as_str)
            .filter(|o| !o.is_empty())
            .map(str::to_string),
        blocked_by: strings("blockedBy"),
        active_form: v
            .get("activeForm")
            .and_then(json::Value::as_str)
            .map(str::to_string),
    })
}

/// Task id order: numeric ids by value, then the rest by text.
fn id_order(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

/// Is this directory entry a task file: a regular `*.json`, not a dotfile
/// (`.highwatermark` sits beside the tasks)?
fn is_task_file(name: &str) -> bool {
    !name.starts_with('.') && name.ends_with(".json")
}

/// Every task in `dir`, in id order. A missing directory is an empty list; a
/// file that is not a task (malformed, or not one of the shapes above) is
/// skipped, never fatal.
pub fn read_tasks(dir: &Path) -> Vec<Task> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut tasks: Vec<Task> = entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(is_task_file))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| parse_task(&text))
        .collect();
    tasks.sort_by(|a, b| id_order(&a.id, &b.id));
    tasks
}

/// The team under `team_dir` with the tasks under `tasks_dir`. `None` when the
/// team's `config.json` is missing or not a team (the team is over, or never
/// was); a missing tasks directory is a team with no tasks yet.
pub fn read_team(team_dir: &Path, tasks_dir: &Path) -> Option<TeamState> {
    let text = std::fs::read_to_string(team_dir.join("config.json")).ok()?;
    let mut team = TeamState::parse_config(&text)?;
    team.tasks = read_tasks(tasks_dir);
    Some(team)
}

/// What a look remembers about a file: enough to know it was rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Stamp {
        Stamp {
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }
}

/// One lead's team as the watch last saw it: the config's stamp and parse,
/// and each task file's stamp and parse (`None` for a file that is not a task).
#[derive(Clone, Debug)]
struct Entry {
    config_stamp: Stamp,
    config: TeamState,
    files: BTreeMap<String, (Stamp, Option<Task>)>,
}

impl Entry {
    fn state(&self) -> TeamState {
        let mut team = self.config.clone();
        team.tasks = self.files.values().filter_map(|(_, t)| t.clone()).collect();
        team.tasks.sort_by(|a, b| id_order(&a.id, &b.id));
        team
    }
}

/// The reader behind the overview: the live team of every lead it is told
/// about, re-read only when a file's stamp changed, looked at most once per
/// [`TeamWatch::EVERY`].
pub struct TeamWatch {
    root: Option<PathBuf>,
    /// By the lead's session id.
    entries: HashMap<String, Entry>,
    last_look: Option<Instant>,
}

impl Default for TeamWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl TeamWatch {
    /// How often [`TeamWatch::refresh`] looks at the disk, at most.
    pub const EVERY: Duration = Duration::from_secs(1);

    /// A watch over the claude root ([`claude_root`]).
    pub fn new() -> TeamWatch {
        Self::at(claude_root())
    }

    /// A watch over an explicit root (`None`: nothing is ever found).
    pub fn at(root: Option<PathBuf>) -> TeamWatch {
        TeamWatch {
            root,
            entries: HashMap::new(),
            last_look: None,
        }
    }

    /// [`TeamWatch::look`], at most once per [`TeamWatch::EVERY`]; `false`
    /// when it did not look.
    pub fn refresh(&mut self, leads: &[&str]) -> bool {
        if self
            .last_look
            .is_some_and(|last| last.elapsed() < Self::EVERY)
        {
            return false;
        }
        self.last_look = Some(Instant::now());
        self.look(leads)
    }

    /// Look now: for each lead session id, stat the derived team's
    /// `config.json` (gone: the team is over and is dropped) and list its tasks
    /// directory, re-reading only the files whose stamp changed. `true` when
    /// what [`TeamWatch::teams`] returns changed.
    pub fn look(&mut self, leads: &[&str]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|sid, _| leads.contains(&sid.as_str()));
        let mut changed = self.entries.len() != before;
        let Some(root) = self.root.clone() else {
            return changed;
        };
        for &sid in leads {
            let name = team_name_for(sid);
            let config_path = team_dir(&root, &name).join("config.json");
            let Ok(meta) = std::fs::metadata(&config_path) else {
                changed |= self.entries.remove(sid).is_some();
                continue;
            };
            let config_stamp = Stamp::of(&meta);
            let old = self.entries.get(sid);
            let config = match old {
                Some(e) if e.config_stamp == config_stamp => e.config.clone(),
                _ => {
                    let Some(team) = std::fs::read_to_string(&config_path)
                        .ok()
                        .and_then(|t| TeamState::parse_config(&t))
                    else {
                        // Not a team file (or mid-write): keep what we had, if
                        // anything, and try again next look.
                        continue;
                    };
                    team
                }
            };
            let files = list_tasks(&tasks_dir(&root, &name), old.map(|e| &e.files));
            let next = Entry {
                config_stamp,
                config,
                files,
            };
            let differs = old.map_or(true, |e| e.state() != next.state());
            self.entries.insert(sid.to_string(), next);
            changed |= differs;
        }
        changed
    }

    /// Every live team, by name.
    pub fn teams(&self) -> Vec<TeamState> {
        let mut teams: Vec<TeamState> = self.entries.values().map(Entry::state).collect();
        teams.sort_by(|a, b| a.name.cmp(&b.name));
        teams
    }

    /// The team the lead with this session id runs, if live.
    pub fn team_led_by(&self, session_id: &str) -> Option<TeamState> {
        self.entries.get(session_id).map(Entry::state)
    }
}

/// List `dir`'s task files with their stamps, parsing only those whose stamp
/// is not the one `known` remembers. A missing directory lists nothing.
fn list_tasks(
    dir: &Path,
    known: Option<&BTreeMap<String, (Stamp, Option<Task>)>>,
) -> BTreeMap<String, (Stamp, Option<Task>)> {
    let mut files = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for e in entries.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !is_task_file(&name) {
            continue;
        }
        let Ok(meta) = e.metadata() else {
            continue;
        };
        let stamp = Stamp::of(&meta);
        let task = match known.and_then(|k| k.get(&name)) {
            Some((old, task)) if *old == stamp => task.clone(),
            _ => std::fs::read_to_string(e.path())
                .ok()
                .and_then(|t| parse_task(&t)),
        };
        files.insert(name, (stamp, task));
    }
    files
}

static WATCH: OnceLock<Mutex<TeamWatch>> = OnceLock::new();

fn watch() -> &'static Mutex<TeamWatch> {
    WATCH.get_or_init(|| Mutex::new(TeamWatch::new()))
}

/// The run loop's hook: refresh the process-wide watch for these lead session
/// ids (gated to once a second). `true` when a team the overview shows changed.
pub fn refresh(leads: &[&str]) -> bool {
    watch()
        .lock()
        .map(|mut w| w.refresh(leads))
        .unwrap_or(false)
}

/// A copy of every live team the process-wide watch knows, by name.
pub fn snapshot() -> Vec<TeamState> {
    watch().lock().map(|w| w.teams()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A fresh claude root for one test, removed on drop.
    struct Root(PathBuf);

    impl Root {
        fn new() -> Root {
            let dir = std::env::temp_dir().join(format!(
                "atrium-claude-teams-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Root(dir)
        }

        fn write(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SESSION: &str = "9e251086-b14c-40a6-867a-9016c9cb8cc8";
    const TEAM: &str = "session-9e251086";

    // The shapes Claude Code 2.1.289 writes, per its binary: the team record
    // (`{name, createdAt, leadAgentId, leadSessionId, members: [...]}`) and the
    // task record (`{id, subject, description, activeForm?, owner?, status,
    // blocks, blockedBy, metadata?}`).
    const CONFIG: &str = r#"{"name":"session-9e251086","createdAt":1762700000000,"leadAgentId":"team-lead@session-9e251086","leadSessionId":"9e251086-b14c-40a6-867a-9016c9cb8cc8","members":[{"agentId":"team-lead@session-9e251086","name":"team-lead","agentType":"team-lead","joinedAt":1762700000000,"tmuxPaneId":"leader","cwd":"/home/u/proj","subscriptions":[],"backendType":"in-process"},{"agentId":"builder@session-9e251086","name":"builder","agentType":"general-purpose","joinedAt":1762700001000,"tmuxPaneId":"%7","cwd":"/home/u/proj","subscriptions":[],"backendType":"tmux"},{"agentId":"writer@session-9e251086","name":"writer","agentType":"general-purpose","joinedAt":1762700002000,"tmuxPaneId":"%8","cwd":"/home/u/proj","subscriptions":[],"backendType":"tmux"}]}"#;
    const TASK_1: &str = r#"{"id":"1","subject":"Parse the team config","description":"Read config.json into members.","activeForm":"Parsing the team config","owner":"builder","status":"in_progress","blocks":["2"],"blockedBy":[]}"#;
    const TASK_2: &str = r#"{"id":"2","subject":"Render the block","description":"One line per task under the lead.","status":"pending","blocks":[],"blockedBy":["1"]}"#;
    const TASK_3: &str = r#"{"id":"3","subject":"Write the docs","description":"A paragraph in control-plane.md.","owner":"writer","status":"completed","blocks":[],"blockedBy":[],"metadata":{"note":"done"}}"#;
    const TASK_10: &str = r#"{"id":"10","subject":"Cut the release","description":"Later.","status":"pending","blocks":[],"blockedBy":["2","3"]}"#;

    fn team_fixture(root: &Root) {
        root.write(&format!("teams/{TEAM}/config.json"), CONFIG);
        root.write(&format!("teams/{TEAM}/inboxes/builder.json"), "[]");
        root.write(&format!("tasks/{TEAM}/1.json"), TASK_1);
        root.write(&format!("tasks/{TEAM}/2.json"), TASK_2);
        root.write(&format!("tasks/{TEAM}/3.json"), TASK_3);
        root.write(&format!("tasks/{TEAM}/10.json"), TASK_10);
        root.write(&format!("tasks/{TEAM}/.highwatermark"), "10");
        root.write(&format!("tasks/{TEAM}/4.json"), r#"{"id":"4","subject":"#);
        root.write(&format!("tasks/{TEAM}/notes.txt"), "not a task");
    }

    #[test]
    fn team_name_is_session_dash_first_eight() {
        assert_eq!(team_name_for(SESSION), TEAM);
        assert_eq!(team_name_for("abc"), "session-abc");
        assert_eq!(team_dir_name("Session-9E25/x"), "session-9e25-x");
        assert_eq!(task_key("Session-9E25/x_y"), "Session-9E25-x_y");
        let root = Path::new("/r");
        assert_eq!(
            team_dir(root, TEAM),
            Path::new("/r").join("teams").join(TEAM)
        );
        assert_eq!(
            tasks_dir(root, TEAM),
            Path::new("/r").join("tasks").join(TEAM)
        );
    }

    #[test]
    fn team_name_arg_reads_both_spellings() {
        let argv = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(
            team_name_arg(&argv(
                "claude --agent-name builder --team-name session-9e251086"
            )),
            Some(TEAM.to_string())
        );
        assert_eq!(
            team_name_arg(&argv("claude --team-name=session-9e251086")),
            Some(TEAM.to_string())
        );
        assert_eq!(team_name_arg(&argv("claude --model opus")), None);
    }

    #[test]
    fn config_parses_name_lead_and_members() {
        let t = TeamState::parse_config(CONFIG).expect("a team");
        assert_eq!(t.name, TEAM);
        assert_eq!(
            t.lead_agent_id.as_deref(),
            Some("team-lead@session-9e251086")
        );
        let names: Vec<&str> = t.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["team-lead", "builder", "writer"]);
        assert_eq!(t.members[1].agent_id, "builder@session-9e251086");
        assert!(t.tasks.is_empty());
        // A nameless member is skipped; a non-object is not a team.
        let t = TeamState::parse_config(r#"{"members":[{"agentId":"x@t"},{"name":"b"}]}"#).unwrap();
        assert_eq!(t.members.len(), 1);
        assert_eq!(t.members[0].agent_id, "");
        assert!(TeamState::parse_config("[]").is_none());
        assert!(TeamState::parse_config("{").is_none());
    }

    #[test]
    fn task_parses_each_state_and_rejects_malformed() {
        let t = parse_task(TASK_1).expect("a task");
        assert_eq!(t.id, "1");
        assert_eq!(t.subject, "Parse the team config");
        assert_eq!(t.status, TaskStatus::InProgress);
        assert_eq!(t.owner.as_deref(), Some("builder"));
        assert_eq!(t.active_form.as_deref(), Some("Parsing the team config"));
        assert!(t.blocked_by.is_empty());
        let t = parse_task(TASK_2).unwrap();
        assert_eq!(t.status, TaskStatus::Pending);
        assert_eq!(t.owner, None);
        assert_eq!(t.blocked_by, ["1"]);
        let t = parse_task(TASK_3).unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert_eq!(t.owner.as_deref(), Some("writer"));
        assert!(parse_task(r#"{"id":"4","subject":"#).is_none(), "truncated");
        assert!(
            parse_task(r#"{"id":"4","subject":"x","status":"done"}"#).is_none(),
            "unknown status"
        );
        assert!(
            parse_task(r#"{"id":"4","status":"pending"}"#).is_none(),
            "no subject"
        );
        assert!(parse_task(r#""pending""#).is_none(), "not an object");
        assert_eq!(TaskStatus::Pending.mark(), "[ ]");
        assert_eq!(TaskStatus::InProgress.mark(), "[~]");
        assert_eq!(TaskStatus::Completed.mark(), "[x]");
    }

    #[test]
    fn read_team_skips_non_tasks_and_orders_by_id() {
        let root = Root::new();
        team_fixture(&root);
        let team = read_team(&team_dir(&root.0, TEAM), &tasks_dir(&root.0, TEAM)).expect("live");
        let ids: Vec<&str> = team.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["1", "2", "3", "10"], "numeric order, 4.json skipped");
        assert_eq!(team.members.len(), 3);
        assert_eq!(team.counts(), (2, 1, 1));
        assert_eq!(team.task_for("builder").map(|t| t.id.as_str()), Some("1"));
        assert_eq!(team.task_for("writer"), None, "completed is not current");
        assert_eq!(team.task_for("team-lead"), None);
        assert!(team.blocked(&team.tasks[1]), "2 waits on 1 (in progress)");
        assert!(team.blocked(&team.tasks[3]), "10 waits on 2");
        assert!(!team.blocked(&team.tasks[0]));
        let mut lone = team.tasks[1].clone();
        lone.blocked_by = vec!["99".into()];
        assert!(
            !team.blocked(&lone),
            "a dependency no longer listed does not block"
        );
    }

    #[test]
    fn read_team_without_tasks_dir_is_empty_and_without_config_is_none() {
        let root = Root::new();
        root.write(&format!("teams/{TEAM}/config.json"), CONFIG);
        let team = read_team(&team_dir(&root.0, TEAM), &tasks_dir(&root.0, TEAM)).unwrap();
        assert!(team.tasks.is_empty());
        assert!(read_team(
            &team_dir(&root.0, "session-other"),
            &tasks_dir(&root.0, TEAM)
        )
        .is_none());
        assert!(read_tasks(&root.0.join("nowhere")).is_empty());
    }

    #[test]
    fn locate_needs_the_config_file() {
        let root = Root::new();
        assert_eq!(locate_in(&root.0, SESSION), None);
        root.write(&format!("tasks/{TEAM}/1.json"), TASK_1);
        assert_eq!(
            locate_in(&root.0, SESSION),
            None,
            "tasks alone are a past team"
        );
        root.write(&format!("teams/{TEAM}/config.json"), CONFIG);
        assert_eq!(locate_in(&root.0, SESSION), Some(team_dir(&root.0, TEAM)));
    }

    #[test]
    fn watch_rereads_only_changed_files_and_drops_an_ended_team() {
        let root = Root::new();
        team_fixture(&root);
        let mut w = TeamWatch::at(Some(root.0.clone()));
        assert!(w.look(&[SESSION]), "first look finds the team");
        let teams = w.teams();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].name, TEAM);
        assert_eq!(teams[0].tasks.len(), 4);
        assert!(!w.look(&[SESSION]), "nothing changed");
        // A task rewritten in place (status flips, length changes).
        root.write(
            &format!("tasks/{TEAM}/1.json"),
            &TASK_1.replace("in_progress", "completed"),
        );
        assert!(w.look(&[SESSION]), "a rewritten task is a change");
        assert_eq!(w.teams()[0].tasks[0].status, TaskStatus::Completed);
        // A new task appears; a malformed one that becomes well-formed appears.
        root.write(
            &format!("tasks/{TEAM}/4.json"),
            &TASK_2.replace("\"2\"", "\"4\""),
        );
        assert!(w.look(&[SESSION]));
        assert_eq!(w.teams()[0].tasks.len(), 5);
        // A lead that runs no team is simply absent.
        assert!(!w.look(&[SESSION, "ffffffff-0000-0000-0000-000000000000"]));
        assert_eq!(w.teams().len(), 1);
        assert!(w.team_led_by(SESSION).is_some());
        // The session ends: Claude Code removes the team dir; the tasks stay.
        std::fs::remove_dir_all(team_dir(&root.0, TEAM)).unwrap();
        assert!(w.look(&[SESSION]), "an ended team is a change");
        assert!(w.teams().is_empty());
        assert!(!w.look(&[SESSION]));
        // A lead that left the list is dropped too.
        root.write(&format!("teams/{TEAM}/config.json"), CONFIG);
        assert!(w.look(&[SESSION]));
        assert!(w.look(&[]));
        assert!(w.teams().is_empty());
    }

    #[test]
    fn refresh_is_gated_to_once_a_second() {
        let root = Root::new();
        team_fixture(&root);
        let mut w = TeamWatch::at(Some(root.0.clone()));
        assert!(w.refresh(&[SESSION]));
        std::fs::remove_dir_all(team_dir(&root.0, TEAM)).unwrap();
        assert!(!w.refresh(&[SESSION]), "within a second: no look");
        assert_eq!(
            w.teams().len(),
            1,
            "the ended team is still shown until the next look"
        );
        w.last_look = Some(Instant::now() - TeamWatch::EVERY);
        assert!(
            w.refresh(&[SESSION]),
            "a second later: looked, and the team is gone"
        );
        assert!(w.teams().is_empty());
        let mut none = TeamWatch::at(None);
        assert!(!none.look(&[SESSION]));
        assert!(none.teams().is_empty());
    }
}
