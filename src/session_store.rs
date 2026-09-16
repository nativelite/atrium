//! Where session snapshots live, and which one a launch should offer to resume.
//!
//! A snapshot is not just layout: it carries the session's trust posture, deny
//! rules and spawn capabilities, so it decides how much authority a recovered
//! session gets. That shapes every choice here.
//!
//! * **Per-user state directory, not the project and not the temp dir.** The
//!   project root is the one place a fleet's main-tree agents are *supposed* to
//!   write, which makes it the worst home for a file that grants authority. The
//!   shared temp dir let any session — including every e2e test run — become "the
//!   newest snapshot" and hijack a recovery. `%LOCALAPPDATA%\atrium\sessions` on
//!   Windows, `$XDG_STATE_HOME/atrium/sessions` (else `~/.local/state/…`) on unix.
//! * **One directory per project, one file per session.** `atrium recover` in a
//!   project finds that project's sessions without guessing, and two atriums in
//!   the same project never overwrite each other's file.
//! * **Owner-only on unix** (`0700` directories, `0600` files).
//!
//! What this does **not** do is make the file trustworthy. Every agent atrium hosts
//! runs as the same user, and no file permission separates a user from their own
//! processes. The defenses that exist for that live elsewhere and are tripwires
//! and consent, not integrity: the warden notices a snapshot atrium did not write,
//! and a resume is always shown to the operator and confirmed before it applies.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::session::{SessionMeta, Snapshot};

/// Overrides the state directory. The test suite points it at a scratch
/// directory (`.cargo/config.toml`) so e2e sessions never land among real ones.
pub const ENV_STATE_DIR: &str = "ATRIUM_STATE_DIR";

/// `0` turns off the launch-time resume offer. `atrium recover` is unaffected.
/// The test suite sets it: an e2e session killed at the end of a test is, to the
/// store, indistinguishable from a crash, and must not prompt the next test.
pub const ENV_RESUME_OFFER: &str = "ATRIUM_RESUME_OFFER";

/// How often a running atrium rewrites its snapshot even when nothing changed.
pub const HEARTBEAT_MS: u64 = 15_000;

/// A snapshot whose writer is still running but which has not been rewritten for
/// this long is treated as abandoned. Four heartbeats: a busy tick cannot trip it,
/// and a hung atrium still becomes resumable.
pub const STALE_AFTER_MS: u64 = 4 * HEARTBEAT_MS;

/// Sessions kept per project. Older ones are pruned when a session starts.
pub const KEEP_PER_PROJECT: usize = 5;

/// The state directory for this user, if one can be determined.
pub fn state_root() -> Option<PathBuf> {
    root_from(|k| std::env::var_os(k))
}

/// [`state_root`] against an injected environment, so the platform rules are
/// testable without mutating the real one.
pub fn root_from(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(dir) = set(ENV_STATE_DIR) {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        set("LOCALAPPDATA").map(|d| PathBuf::from(d).join("atrium").join("sessions"))
    }
    #[cfg(not(windows))]
    {
        // The XDG spec says a relative XDG_STATE_HOME is invalid and must be
        // ignored — and a relative one would resolve against whatever directory
        // atrium happened to start in.
        set("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| set("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
            .map(|d| d.join("atrium").join("sessions"))
    }
}

/// The identity of a project directory: its canonical path, lower-cased on
/// Windows where paths are case-insensitive. Two spellings of one directory must
/// map to one project, or a resume offered in one would be missed in the other.
pub fn project_id(dir: &Path) -> String {
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let id = canon.to_string_lossy().into_owned();
    // `canonicalize` on Windows returns the verbatim form (`\\?\D:\...`). Drop that
    // prefix for an ordinary drive path so the id reads as the path people type;
    // it is dropped every time, so keying stays consistent. A verbatim UNC path
    // (`\\?\UNC\...`) keeps it — stripping that would change what it names.
    let id = match id.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with(r"UNC\") => rest.to_string(),
        _ => id,
    };
    if cfg!(windows) {
        id.to_lowercase()
    } else {
        id
    }
}

/// The directory name for a project: a readable prefix (the folder's own name)
/// plus a digest of the full identity, so `projects/app` and `work/app` differ
/// while `atrium recover --list` still reads as something a human recognises.
pub fn project_key(project_id: &str) -> String {
    let name: String = project_id
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("root")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let name = if name.is_empty() || name.chars().all(|c| c == '.') {
        "root".to_string()
    } else {
        name
    };
    format!(
        "{name}-{:016x}",
        crate::warden::digest(project_id.as_bytes())
    )
}

/// The directory holding one project's sessions.
pub fn project_dir(root: &Path, project_id: &str) -> PathBuf {
    root.join(project_key(project_id))
}

/// The snapshot file for the atrium with `pid` that started at `started_ms`, in
/// its project's directory. The start time is in the name because Windows reuses
/// pids quickly: named by pid alone, a new session would silently overwrite an
/// older *crashed* session's file whenever the resume offer was not shown.
pub fn session_file(root: &Path, project_id: &str, pid: u32, started_ms: u64) -> PathBuf {
    project_dir(root, project_id).join(format!("{pid}-{started_ms}.json"))
}

/// The writer pid a store file name claims (`<pid>-<ms>.json`), if it has one.
pub fn pid_from_name(path: &Path) -> Option<u32> {
    path.file_stem()?.to_str()?.split('-').next()?.parse().ok()
}

/// How far in the future a heartbeat may be before the file is treated as
/// suspect rather than as the newest session. Generous, so a clock adjustment
/// does not hide a real crash.
pub const FUTURE_SKEW_MS: u64 = 5 * 60_000;

/// Create `dir` (and its parents) and make it owner-only on unix. Only the
/// directories atrium owns are tightened — the state root and the project
/// directory — never a parent such as `~/.local/state` that other tools share.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `body` to `path` atomically (sibling `.tmp`, then rename) and owner-only
/// on unix. The mode is set on the open *and* re-applied, because a `.tmp` left
/// by a crash keeps whatever mode it was created with.
pub fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        let mut f = opts.open(&tmp)?;
        f.write_all(body.as_bytes())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// Milliseconds since the epoch, on the wall clock (what the heartbeat is stamped
/// with, and what survives a reboot to be compared against).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What became of the session a snapshot describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Its atrium is running and still writing the heartbeat. Never resumed.
    Running,
    /// It stopped without a clean exit: a crash, a kill, a reboot — or its atrium
    /// is alive but hung. Offered on launch.
    Crashed,
    /// It ended on purpose, or was already resumed or dismissed. Not offered, but
    /// `atrium recover` can still restore it.
    Closed,
}

/// Decide a session's [`Liveness`]. `running` is the pid probe
/// (`reap::pid_running` in the app), injected so the decision is testable.
///
/// Both halves of "running" are required. A pid alone is not enough — after a
/// reboot that pid may belong to an unrelated process, which would make a
/// crashed session look alive and never be offered. A fresh heartbeat alone is
/// not enough either — a session that died a second ago has one.
pub fn liveness(meta: &SessionMeta, now_ms: u64, running: impl Fn(u32) -> bool) -> Liveness {
    if meta.clean {
        return Liveness::Closed;
    }
    let fresh = meta
        .saved_at_ms
        .is_some_and(|t| now_ms.saturating_sub(t) < STALE_AFTER_MS);
    if fresh && meta.pid.is_some_and(running) {
        Liveness::Running
    } else {
        Liveness::Crashed
    }
}

/// One saved session in a project directory.
#[derive(Debug, Clone)]
pub struct Stored {
    pub path: PathBuf,
    pub snapshot: Snapshot,
}

/// A project directory, read. Only `sessions` are ever offered, restored by a
/// bare `atrium recover`, or counted toward the prune limit; the rest are shown
/// by `--list` and otherwise ignored.
#[derive(Debug, Default)]
pub struct Listing {
    /// Loadable, consistent sessions, newest first.
    pub sessions: Vec<Stored>,
    /// Loadable files that are inconsistent with where they sit, with the reason:
    /// a file whose recorded project is another one, whose recorded pid does not
    /// match its name, or whose heartbeat is in the future. atrium never writes
    /// such a file, and every agent it hosts can write into this directory, so
    /// treating one as a candidate would let a planted file be offered first — a
    /// far-future timestamp would also make it outrank, and prune away, the real
    /// sessions.
    pub suspect: Vec<(PathBuf, String)>,
    /// Files that could not be read, with the error. Reported, never silently
    /// skipped: an unreadable snapshot may be a tampered one.
    pub unreadable: Vec<(PathBuf, String)>,
}

/// Why a loaded file is not a candidate for this project, or `None` if it is.
pub fn suspicion(path: &Path, snap: &Snapshot, project_id: &str, now_ms: u64) -> Option<String> {
    let meta = &snap.meta;
    if let Some(project) = &meta.project {
        if project != project_id {
            return Some(format!("recorded for another project ({project})"));
        }
    }
    if let (Some(named), Some(recorded)) = (pid_from_name(path), meta.pid) {
        if named != recorded {
            return Some(format!(
                "named for pid {named} but written by pid {recorded}"
            ));
        }
    }
    if let Some(t) = meta.saved_at_ms {
        if t > now_ms.saturating_add(FUTURE_SKEW_MS) {
            return Some("heartbeat is in the future".to_string());
        }
    }
    None
}

/// Read every snapshot in one project's directory.
pub fn list(dir: &Path, project_id: &str, now_ms: u64) -> Listing {
    let mut out = Listing::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match crate::session::load(&path) {
            Ok(snapshot) => match suspicion(&path, &snapshot, project_id, now_ms) {
                Some(why) => out.suspect.push((path, why)),
                None => out.sessions.push(Stored { path, snapshot }),
            },
            Err(e) => out.unreadable.push((path, e)),
        }
    }
    out.sessions
        .sort_by_key(|s| std::cmp::Reverse(s.snapshot.meta.saved_at_ms.unwrap_or(0)));
    out
}

/// Is `candidate` already running under another session? True when a
/// [`Liveness::Running`] session shares one of its agent transcripts.
///
/// A resumed session writes its own file and the old one is only marked closed,
/// so without this a bare `atrium recover` in a second terminal would restore the
/// old file and start a second copy of the same agents on the same transcripts.
/// Transcript ids are the identity that matters: two processes resuming one
/// transcript is the actual harm, whatever the files are named.
pub fn superseded(
    stored: &[Stored],
    candidate: &Stored,
    now_ms: u64,
    running: impl Fn(u32) -> bool,
) -> bool {
    let ids: Vec<&str> = candidate
        .snapshot
        .panes
        .iter()
        .filter_map(|p| p.session_id.as_deref())
        .collect();
    if ids.is_empty() {
        return false;
    }
    stored.iter().any(|other| {
        other.path != candidate.path
            && liveness(&other.snapshot.meta, now_ms, &running) == Liveness::Running
            && other
                .snapshot
                .panes
                .iter()
                .filter_map(|p| p.session_id.as_deref())
                .any(|id| ids.contains(&id))
    })
}

/// The session a launch should offer to resume: the newest [`Liveness::Crashed`]
/// one. Only a crash is offered — a deliberate quit is not a reason to ask.
pub fn offer<'a>(
    stored: &'a [Stored],
    now_ms: u64,
    running: impl Fn(u32) -> bool,
) -> Option<&'a Stored> {
    stored.iter().find(|s| {
        liveness(&s.snapshot.meta, now_ms, &running) == Liveness::Crashed
            && !superseded(stored, s, now_ms, &running)
    })
}

/// The session a bare `atrium recover` restores: the newest one that is not
/// still running, whether it crashed or closed cleanly.
pub fn recoverable<'a>(
    stored: &'a [Stored],
    now_ms: u64,
    running: impl Fn(u32) -> bool,
) -> Option<&'a Stored> {
    stored.iter().find(|s| {
        liveness(&s.snapshot.meta, now_ms, &running) != Liveness::Running
            && !superseded(stored, s, now_ms, &running)
    })
}

/// The files to delete so a project keeps at most `keep` sessions. Never a
/// running session, whatever its age. Pure: the caller does the deleting.
pub fn prune_plan(
    stored: &[Stored],
    keep: usize,
    now_ms: u64,
    running: impl Fn(u32) -> bool,
) -> Vec<PathBuf> {
    stored
        .iter()
        .skip(keep)
        .filter(|s| liveness(&s.snapshot.meta, now_ms, &running) != Liveness::Running)
        .map(|s| s.path.clone())
        .collect()
}

/// Mark a stored session closed, so it is not offered again: after it has been
/// resumed (the resumed session writes its own file) or dismissed at the prompt.
/// The file is kept, so an explicit `atrium recover --snapshot` still works.
pub fn mark_closed(path: &Path) -> Result<(), String> {
    let mut snap = crate::session::load(path)?;
    snap.meta.clean = true;
    crate::session::save(path, &snap).map_err(|e| format!("cannot update {}: {e}", path.display()))
}

/// Is `path` inside `root`? Only files in the store are ever rewritten by
/// [`mark_closed`] — a snapshot passed from elsewhere (a v1 file in the temp
/// directory) is read, never modified.
pub fn is_in_store(path: &Path, root: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(path).starts_with(canon(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{capture, LayoutRecord};

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| {
            owned
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    fn meta(pid: u32, saved: u64, clean: bool) -> SessionMeta {
        SessionMeta {
            project: Some("p".to_string()),
            pid: Some(pid),
            saved_at_ms: Some(saved),
            clean,
        }
    }

    fn stored_with(name: &str, m: SessionMeta, ids: &[&str]) -> Stored {
        let mut s = stored(name, m);
        s.snapshot.panes = ids
            .iter()
            .enumerate()
            .map(|(i, id)| crate::session::PaneRecord {
                id: i,
                role: None,
                argv: vec!["claude".to_string()],
                cwd: None,
                identity: None,
                session_id: Some(id.to_string()),
                worktree: None,
                deny: Vec::new(),
                can_spawn: false,
                depth: 0,
                parent_pane: None,
                mode: None,
            })
            .collect();
        s
    }

    fn stored(name: &str, m: SessionMeta) -> Stored {
        let mut snapshot = capture(vec![0], 0, Vec::new(), None);
        snapshot.layout = LayoutRecord {
            ids: vec![0],
            focus: 0,
        };
        snapshot.meta = m;
        Stored {
            path: PathBuf::from(name),
            snapshot,
        }
    }

    #[test]
    fn the_override_wins_and_an_empty_one_is_ignored() {
        assert_eq!(
            root_from(env_of(&[(ENV_STATE_DIR, "/scratch/state")])),
            Some(PathBuf::from("/scratch/state"))
        );
        // An empty override must not become "the current directory".
        assert_ne!(
            root_from(env_of(&[(ENV_STATE_DIR, "")])),
            Some(PathBuf::from(""))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_uses_local_app_data() {
        assert_eq!(
            root_from(env_of(&[("LOCALAPPDATA", r"C:\Users\u\AppData\Local")])),
            Some(PathBuf::from(r"C:\Users\u\AppData\Local\atrium\sessions"))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_prefers_an_absolute_xdg_state_home_and_falls_back_to_home() {
        assert_eq!(
            root_from(env_of(&[
                ("XDG_STATE_HOME", "/x/state"),
                ("HOME", "/home/u")
            ])),
            Some(PathBuf::from("/x/state/atrium/sessions"))
        );
        assert_eq!(
            root_from(env_of(&[
                ("XDG_STATE_HOME", "rel/state"),
                ("HOME", "/home/u")
            ])),
            Some(PathBuf::from("/home/u/.local/state/atrium/sessions")),
            "a relative XDG_STATE_HOME is invalid per the spec"
        );
    }

    /// The id is what keys a project's sessions, so it must be stable across the
    /// ways a directory can be spelled — and on Windows it must not carry the
    /// verbatim prefix `canonicalize` adds, or `--list` shows a path nobody types.
    #[cfg(windows)]
    #[test]
    fn a_windows_project_id_is_case_folded_and_drops_the_verbatim_prefix() {
        let dir = std::env::temp_dir();
        let id = project_id(&dir);
        assert!(!id.starts_with(r"\\?\"), "verbatim prefix leaked: {id}");
        let upper = PathBuf::from(dir.to_string_lossy().to_uppercase());
        assert_eq!(
            project_id(&upper),
            id,
            "case must not split one project into two"
        );
    }

    /// Two projects with the same folder name must not share a directory, and
    /// the key must stay readable enough to recognise in `--list`.
    #[test]
    fn project_keys_are_readable_and_distinct() {
        let a = project_key("/home/u/projects/app");
        let b = project_key("/home/u/work/app");
        assert!(a.starts_with("app-") && b.starts_with("app-"));
        assert_ne!(a, b);
        assert_eq!(a, project_key("/home/u/projects/app"), "stable");
        assert!(project_key("/").starts_with("root-"));
        assert!(
            !project_key("/x/we ird:name").contains([' ', ':']),
            "no path-hostile characters in a directory name"
        );
    }

    /// The truth table the resume offer rests on — including the reboot case,
    /// where the recorded pid now belongs to something else.
    #[test]
    fn liveness_needs_both_a_running_pid_and_a_fresh_heartbeat() {
        let now = 10_000_000;
        let alive = |_: u32| true;
        let dead = |_: u32| false;
        assert_eq!(
            liveness(&meta(1, now - 1_000, false), now, alive),
            Liveness::Running
        );
        assert_eq!(
            liveness(&meta(1, now - 1_000, false), now, dead),
            Liveness::Crashed,
            "died a moment ago: the heartbeat is fresh but the pid is gone"
        );
        assert_eq!(
            liveness(&meta(1, now - STALE_AFTER_MS - 1, false), now, alive),
            Liveness::Crashed,
            "pid reused after a reboot, or a hung atrium: the heartbeat stopped"
        );
        assert_eq!(liveness(&meta(1, now, true), now, alive), Liveness::Closed);
        assert_eq!(
            liveness(&SessionMeta::default(), now, alive),
            Liveness::Crashed,
            "no meta cannot prove it is running"
        );
    }

    #[test]
    fn a_launch_offers_only_a_crash_and_recover_takes_anything_not_running() {
        let now = 10_000_000;
        let running = |pid: u32| pid == 1;
        let sessions = vec![
            stored("live", meta(1, now - 1_000, false)),
            stored("quit", meta(2, now - 2_000, true)),
            stored("crash", meta(3, now - 3_000, false)),
        ];
        assert_eq!(
            offer(&sessions, now, running).map(|s| s.path.clone()),
            Some(PathBuf::from("crash")),
            "neither the running session nor the clean quit is offered"
        );
        assert_eq!(
            recoverable(&sessions, now, running).map(|s| s.path.clone()),
            Some(PathBuf::from("quit")),
            "an explicit recover takes the newest session that is not running"
        );
    }

    #[test]
    fn pruning_keeps_the_newest_and_never_touches_a_running_session() {
        let now = 10_000_000;
        let running = |pid: u32| pid == 9;
        let sessions = vec![
            stored("a", meta(1, now - 1, false)),
            stored("b", meta(2, now - 2, true)),
            stored("c", meta(9, now - 3, false)),
            stored("d", meta(4, now - 4, true)),
        ];
        assert_eq!(
            prune_plan(&sessions, 2, now, running),
            vec![PathBuf::from("d")],
            "c is past the limit but still running"
        );
    }

    #[test]
    fn list_reports_unreadable_files_and_sorts_newest_first() {
        let dir = std::env::temp_dir().join(format!("atrium_store_list_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_private_dir(&dir).unwrap();
        for (name, saved) in [("1-1.json", 100), ("1-2.json", 300), ("1-3.json", 200)] {
            let mut s = capture(vec![0], 0, Vec::new(), None);
            s.meta = meta(1, saved, false);
            crate::session::save(&dir.join(name), &s).unwrap();
        }
        std::fs::write(dir.join("4-4.json"), b"{not json").unwrap();
        std::fs::write(dir.join("notes.txt"), b"ignored").unwrap();
        let listing = list(&dir, "p", 1_000);
        let order: Vec<u64> = listing
            .sessions
            .iter()
            .map(|s| s.snapshot.meta.saved_at_ms.unwrap())
            .collect();
        assert_eq!(order, vec![300, 200, 100]);
        assert_eq!(
            listing.unreadable.len(),
            1,
            "a corrupt snapshot is reported, not hidden"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every agent atrium hosts can write into the store. A file atrium would
    /// never have written must not become a candidate — above all one dated in
    /// the future, which would be offered first and push real sessions out of
    /// the prune window.
    #[test]
    fn a_file_inconsistent_with_its_place_is_suspect_not_a_session() {
        let now = 10_000_000;
        let ok = stored("7-1.json", meta(7, now, false));
        assert_eq!(suspicion(&ok.path, &ok.snapshot, "p", now), None);

        let future = stored("7-1.json", meta(7, now + FUTURE_SKEW_MS + 1, false));
        assert!(suspicion(&future.path, &future.snapshot, "p", now).is_some());

        let renamed = stored("8-1.json", meta(7, now, false));
        assert!(
            suspicion(&renamed.path, &renamed.snapshot, "p", now).is_some(),
            "a file named for another pid was not written by that atrium"
        );

        let elsewhere = stored("7-1.json", meta(7, now, false));
        assert!(
            suspicion(&elsewhere.path, &elsewhere.snapshot, "another", now).is_some(),
            "a snapshot copied in from another project is not this project's"
        );
    }

    /// Once a session has been resumed, the old file must not be restorable a
    /// second time while the resumed one runs — that is two copies of the same
    /// agents on the same transcripts.
    #[test]
    fn a_session_whose_transcripts_are_running_elsewhere_is_superseded() {
        let now = 10_000_000;
        let running = |pid: u32| pid == 2;
        let sessions = vec![
            stored_with("resumed", meta(2, now - 1_000, false), &["t-lead", "t-b"]),
            stored_with("original", meta(1, now - 9_000, true), &["t-lead", "t-b"]),
            stored_with("unrelated", meta(3, now - 20_000, true), &["t-other"]),
        ];
        assert!(superseded(&sessions, &sessions[1], now, running));
        assert!(!superseded(&sessions, &sessions[2], now, running));
        assert_eq!(
            recoverable(&sessions, now, running).map(|s| s.path.clone()),
            Some(PathBuf::from("unrelated")),
            "the original is skipped: its agents are already running"
        );
    }

    #[test]
    fn a_store_file_name_carries_its_writer_pid() {
        assert_eq!(
            pid_from_name(Path::new("/s/p/4242-1757990000000.json")),
            Some(4242)
        );
        assert_eq!(pid_from_name(Path::new("/s/p/notes.json")), None);
        let a = session_file(Path::new("/s"), "p", 7, 1);
        let b = session_file(Path::new("/s"), "p", 7, 2);
        assert_ne!(
            a, b,
            "a reused pid must not overwrite an older session's file"
        );
    }

    #[test]
    fn marking_closed_keeps_the_file_and_stops_the_offer() {
        let dir = std::env::temp_dir().join(format!("atrium_store_close_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_private_dir(&dir).unwrap();
        let path = dir.join("7-1.json");
        let mut s = capture(vec![0], 0, Vec::new(), None);
        s.meta = meta(7, 1, false);
        crate::session::save(&path, &s).unwrap();
        mark_closed(&path).unwrap();
        let ok = list(&dir, "p", 2).sessions;
        assert!(ok[0].snapshot.meta.clean);
        assert!(offer(&ok, 2, |_| false).is_none());
        assert!(
            recoverable(&ok, 2, |_| false).is_some(),
            "still explicitly recoverable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn store_files_and_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("atrium_store_mode_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_private_dir(&dir).unwrap();
        let path = dir.join("1.json");
        write_private(&path, "{}").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_files_inside_the_store_count_as_in_it() {
        let root = std::env::temp_dir().join(format!("atrium_store_in_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        ensure_private_dir(&root.join("p")).unwrap();
        let inside = root.join("p").join("1.json");
        std::fs::write(&inside, b"{}").unwrap();
        assert!(is_in_store(&inside, &root));
        assert!(!is_in_store(
            &std::env::temp_dir().join("elsewhere.json"),
            &root
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
