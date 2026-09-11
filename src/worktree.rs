//! Per-agent git worktrees — the pure planning seam (coordination finding #5).
//!
//! A fleet's agents all share **one** working tree, and `dev.py check` gates the
//! whole tree, so one agent's half-finished WIP can fail a *different* agent's
//! green gate, and two agents editing the same files fight over one index. The
//! fix is to let an agent (or a squad of agents) work in its own `git worktree`
//! on its own branch. See `docs/worktrees.md` for the full design.
//!
//! This module owns only the **pure** half: given a parsed [`Fleet`] and the repo
//! directory, [`plan_worktrees`] decides *which* worktrees exist, what they're
//! named, which branch and directory each gets, and which agents share it — with
//! no side effects, so every rule is unit-testable without touching git or the
//! filesystem. The effectful half (`git worktree add/remove/prune`, seeding) is a
//! thin wrapper over this plan and lives behind it.
//!
//! # Grouping rule
//!
//! Each agent's **effective** worktree name is:
//! * its explicit [`Agent::worktree`] value, if set; else
//! * its own `name`, if the fleet set [`Fleet::worktrees`] `= true` (the
//!   full-fan-out shorthand); else
//! * nothing — the agent stays in the main tree (today's behavior).
//!
//! Agents that share an effective name co-develop one worktree + branch; distinct
//! names are isolated. Nothing is planned unless at least one agent resolves to a
//! name, so a fleet that names no worktree (and no shorthand) plans **none** and
//! the feature is entirely dormant.

use crate::fleet::{resolve_dir, Fleet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default base-directory name, created as a **sibling** of the repo so the
/// worktrees never show up as untracked files inside it. Overridable per fleet
/// via [`Fleet::worktree_base`] (e.g. so two concurrent sessions on the same repo
/// get distinct bases and don't collide on directories/branches).
pub const DEFAULT_BASE_NAME: &str = ".atrium-worktrees";

/// One planned worktree: a group name, the branch and directory it maps to, and
/// the agents (in file order) that will work in it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreePlan {
    /// The group name (an explicit `worktree` value, or an agent's own name under
    /// the `worktrees: true` shorthand). The raw key, before slugging.
    pub name: String,
    /// The branch created off `HEAD`: `atrium/<fleet>/<slug(name)>`.
    pub branch: String,
    /// The directory `git worktree add` checks out into:
    /// `<base>/<fleet>/<slug(name)>`.
    pub dir: PathBuf,
    /// The agents sharing this worktree, in fleet-file order.
    pub agents: Vec<String>,
}

/// Make a config-supplied name safe for use as a single branch/path component.
///
/// A worktree name can come from an agent's `name` (arbitrary text, and under the
/// `worktrees: true` shorthand it becomes a branch and a directory) — so anything
/// outside `[A-Za-z0-9._-]` is folded to `-`. This also neutralizes the `/`, `..`,
/// control-char, and whitespace cases that would otherwise produce an invalid
/// branch ref or escape the base directory. A name that slugs to empty becomes
/// `wt`. Two distinct names can slug to the same component; for v1 that is an
/// accepted, documented sharp edge — keep worktree names simple.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    // A leading '.' (or a name that is all dots) makes an invalid git ref and a
    // hidden directory; fold the whole thing to a safe stem in that case.
    let trimmed = out.trim_matches('.');
    if trimmed.is_empty() {
        "wt".to_string()
    } else {
        out
    }
}

/// Resolve the base directory the worktrees live under.
///
/// `Some(base)` in the fleet is resolved relative to the repo dir (absolute used
/// as-is, via the shared [`resolve_dir`] path algebra). Absent → the sibling
/// `../.atrium-worktrees` next to the repo, falling back to a child of the repo
/// only when the repo has no parent (a filesystem root).
pub fn base_dir(fleet: &Fleet, cwd: &Path) -> PathBuf {
    match &fleet.worktree_base {
        Some(b) => resolve_dir(cwd, b),
        None => cwd
            .parent()
            .map(|p| p.join(DEFAULT_BASE_NAME))
            .unwrap_or_else(|| cwd.join(DEFAULT_BASE_NAME)),
    }
}

/// The worktrees a fleet needs, fully resolved but with **no side effects**.
///
/// Empty when no agent resolves to a worktree name (the legacy single-tree case).
/// Otherwise one entry per distinct effective name, in first-appearance order,
/// each carrying its branch, directory, and member agents.
pub fn plan_worktrees(fleet: &Fleet, fleet_name: &str, cwd: &Path) -> Vec<WorktreePlan> {
    let base = base_dir(fleet, cwd);
    let shorthand = fleet.worktrees.unwrap_or(false);

    let mut plans: Vec<WorktreePlan> = Vec::new();
    for agent in &fleet.agents {
        // Effective group name: explicit key wins; else the agent's own name under
        // the shorthand; else the agent stays in the main tree.
        let name = match (&agent.worktree, shorthand) {
            (Some(w), _) => w.clone(),
            (None, true) => agent.name.clone(),
            (None, false) => continue,
        };
        match plans.iter_mut().find(|p| p.name == name) {
            Some(p) => p.agents.push(agent.name.clone()),
            None => {
                let component = slug(&name);
                plans.push(WorktreePlan {
                    branch: format!("atrium/{fleet_name}/{component}"),
                    dir: base.join(fleet_name).join(&component),
                    name,
                    agents: vec![agent.name.clone()],
                });
            }
        }
    }
    plans
}

// ---------------------------------------------------------------------------
// Effectful git ops — a thin, testable wrapper over the plan above
// ---------------------------------------------------------------------------
//
// Every decision (which worktrees, what names, where) is already made in the
// pure seam; this layer only *enacts* a `WorktreePlan` against git and the
// filesystem. Keeping it this thin is deliberate: the logic worth testing lives
// above, and the part that can only be exercised against a real repo stays
// small enough to cover with a handful of temp-repo tests.

/// Run `git -C <dir> <args…>`, returning trimmed stdout on success or the
/// trimmed stderr as an error. atrium shells out to the user's `git` rather than
/// linking a git library: worktrees, prune, and rev-list are exactly the plumbing
/// the CLI is stable at, and the operator's git is the one that understands their
/// repo's config, hooks, and credentials.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git (is it on PATH?): {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Is `cwd` inside a git work tree? Worktrees only engage when it is — a
/// non-repo fleet (research, ops) never touches git.
pub fn is_git_repo(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|s| s == "true")
}

/// The ref new branches fork from and "merged?" is measured against: the current
/// branch name, or the HEAD commit when detached. Captured once at `fleet up`.
pub fn base_ref(cwd: &Path) -> Result<String, String> {
    if let Ok(b) = git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        if !b.is_empty() {
            return Ok(b);
        }
    }
    git(cwd, &["rev-parse", "HEAD"])
}

/// Create the worktree + branch off `HEAD`, **idempotently**.
///
/// `Ok(true)` on a fresh create, `Ok(false)` when the directory is already there
/// (a prior launch) and is left untouched. A branch left over from a previous run
/// whose directory was removed is reused rather than treated as an error.
pub fn ensure(cwd: &Path, plan: &WorktreePlan) -> Result<bool, String> {
    if plan.dir.exists() {
        return Ok(false);
    }
    if let Some(parent) = plan.dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create worktree base {}: {e}", parent.display()))?;
    }
    let dir = plan.dir.to_string_lossy().into_owned();
    // Preferred path: a new branch off HEAD.
    if git(cwd, &["worktree", "add", &dir, "-b", &plan.branch, "HEAD"]).is_ok() {
        return Ok(true);
    }
    // The branch already exists (its dir was removed but the ref kept). Re-attach
    // a worktree to it instead of failing the whole launch.
    git(cwd, &["worktree", "add", &dir, &plan.branch])
        .map(|_| true)
        .map_err(|e| format!("git worktree add for {:?} failed: {e}", plan.name))
}

/// Clear git's records of worktrees whose directories have vanished (a crashed
/// agent, a hand-deleted dir). Safe to call on every launch.
pub fn prune(cwd: &Path) -> Result<(), String> {
    git(cwd, &["worktree", "prune"]).map(|_| ())
}

/// What teardown did with one worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Teardown {
    /// Clean and fully merged → directory removed.
    Removed,
    /// Uncommitted changes → kept, so nothing is lost.
    KeptDirty,
    /// Commits not yet on the base branch → kept, so nothing is lost.
    KeptUnmerged,
    /// Already gone (removed by hand, or never created).
    Missing,
}

/// True when the worktree has no uncommitted changes.
pub fn is_clean(dir: &Path) -> Result<bool, String> {
    Ok(git(dir, &["status", "--porcelain"])?.is_empty())
}

/// How many commits are on `branch` but not reachable from `base` — i.e. work
/// that removing the worktree/branch would strand.
pub fn unmerged_count(cwd: &Path, branch: &str, base: &str) -> Result<usize, String> {
    let spec = format!("{base}..{branch}");
    let n = git(cwd, &["rev-list", "--count", &spec])?;
    n.parse::<usize>()
        .map_err(|e| format!("could not read rev-list count {n:?}: {e}"))
}

/// Remove a worktree's directory **only** if it is clean and fully merged;
/// otherwise keep it and report why, so teardown can never destroy unmerged work.
/// The branch ref is always kept (removing a branch is a heavier statement than
/// removing a scratch checkout, and the operator may still want it).
pub fn remove_if_safe(cwd: &Path, plan: &WorktreePlan, base: &str) -> Result<Teardown, String> {
    if !plan.dir.exists() {
        return Ok(Teardown::Missing);
    }
    if !is_clean(&plan.dir)? {
        return Ok(Teardown::KeptDirty);
    }
    if unmerged_count(cwd, &plan.branch, base)? > 0 {
        return Ok(Teardown::KeptUnmerged);
    }
    let dir = plan.dir.to_string_lossy().into_owned();
    git(cwd, &["worktree", "remove", &dir])?;
    Ok(Teardown::Removed)
}

/// Link the configured untracked seed entries from the main tree into a fresh
/// worktree. Returns a list of human-readable warnings (a missing source, a link
/// that fell back to a copy) — seeding is best-effort and never fails a launch.
///
/// Files are **hardlinked** (same inode, edits propagate, no admin on Windows);
/// directories are **junctioned** on Windows / symlinked on Unix (also no admin);
/// a copy is the last resort. An entry already present in the worktree is left
/// alone. Nothing is seeded unless the fleet lists it, so behavior is never
/// surprising.
pub fn seed(cwd: &Path, dir: &Path, seeds: &[String]) -> Vec<String> {
    let mut warns = Vec::new();
    for entry in seeds {
        let src = cwd.join(entry);
        let dst = dir.join(entry);
        if !src.exists() {
            warns.push(format!(
                "seed {entry:?}: not found in the main tree, skipped"
            ));
            continue;
        }
        if dst.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warns.push(format!(
                    "seed {entry:?}: cannot create {}: {e}",
                    parent.display()
                ));
                continue;
            }
        }
        if let Err(e) = link_one(&src, &dst) {
            warns.push(format!("seed {entry:?}: {e}"));
        }
    }
    warns
}

/// Link one source into the worktree, by the file/dir strategy above, falling
/// back to a copy. Returns a note when a fallback copy was used (it can go stale),
/// or an error only when even the copy failed.
fn link_one(src: &Path, dst: &Path) -> Result<(), String> {
    if src.is_dir() {
        if link_dir(src, dst).is_ok() {
            return Ok(());
        }
        copy_dir(src, dst).map_err(|e| format!("link and copy both failed: {e}"))?;
        Err("linked as a copy (edits will not propagate)".to_string())
    } else {
        if std::fs::hard_link(src, dst).is_ok() {
            return Ok(());
        }
        std::fs::copy(src, dst).map_err(|e| format!("hardlink and copy both failed: {e}"))?;
        Err("copied (hardlink unavailable; edits will not propagate)".to_string())
    }
}

/// Create a no-privilege directory link: a junction on Windows, a symlink on Unix.
fn link_dir(src: &Path, dst: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        // `mklink /J` makes a junction, which needs no admin rights (unlike a dir
        // symlink). It is a cmd builtin, so it must run through `cmd /C`.
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dst)
            .arg(src)
            .status()
            .map_err(|e| format!("mklink: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err("mklink /J failed".to_string())
        }
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(src, dst).map_err(|e| format!("symlink: {e}"))
    }
}

/// Recursively copy a directory tree — the fallback when a junction/symlink can't
/// be made.
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::Agent;

    /// Build a fleet from (agent-name, worktree-key) pairs plus fleet-level knobs.
    fn fleet(
        agents: &[(&str, Option<&str>)],
        worktrees: Option<bool>,
        base: Option<&str>,
    ) -> Fleet {
        Fleet {
            grid: None,
            identity: None,
            trust: None,
            allow_ctl: None,
            context: None,
            topics: None,
            worktrees,
            worktree_base: base.map(str::to_string),
            worktree_seed: None,
            agents: agents
                .iter()
                .map(|(n, w)| Agent {
                    name: n.to_string(),
                    cmd: vec!["claude".to_string()],
                    worktree: w.map(str::to_string),
                    ..Default::default()
                })
                .collect(),
        }
    }

    #[test]
    fn no_key_and_no_shorthand_plans_nothing() {
        let f = fleet(&[("lead", None), ("worker", None)], None, None);
        assert!(
            plan_worktrees(&f, "crew", Path::new("/repo")).is_empty(),
            "the legacy single-tree case must plan zero worktrees"
        );
    }

    #[test]
    fn distinct_names_isolate_and_a_shared_name_groups() {
        let f = fleet(
            &[
                ("lead", None),            // main tree — no plan
                ("flake", Some("flake")),  // solo
                ("cp", Some("cp")),        // squad lead
                ("cp-helper", Some("cp")), // joins cp
            ],
            None,
            None,
        );
        let plans = plan_worktrees(&f, "ctl-fixes", Path::new("/work/repo"));
        assert_eq!(plans.len(), 2, "lead stays on main; flake + cp are planned");

        assert_eq!(plans[0].name, "flake");
        assert_eq!(plans[0].agents, vec!["flake"]);
        assert_eq!(plans[0].branch, "atrium/ctl-fixes/flake");
        // Default base is a SIBLING of the repo, not inside it.
        assert_eq!(
            plans[0].dir,
            Path::new("/work/.atrium-worktrees/ctl-fixes/flake")
        );

        assert_eq!(plans[1].name, "cp");
        assert_eq!(
            plans[1].agents,
            vec!["cp", "cp-helper"],
            "agents sharing a name co-develop one worktree, in file order"
        );
        assert_eq!(plans[1].branch, "atrium/ctl-fixes/cp");
    }

    #[test]
    fn the_shorthand_gives_every_agent_its_own_worktree() {
        let f = fleet(&[("a", None), ("b", None)], Some(true), None);
        let plans = plan_worktrees(&f, "crew", Path::new("/r/repo"));
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].name, "a");
        assert_eq!(plans[0].agents, vec!["a"]);
        assert_eq!(plans[1].name, "b");
        assert_eq!(plans[1].agents, vec!["b"]);
    }

    #[test]
    fn an_explicit_key_overrides_the_shorthand_so_a_squad_can_still_group() {
        // worktrees: true, but two agents pin the same explicit name → they share.
        let f = fleet(
            &[("a", None), ("b", Some("squad")), ("c", Some("squad"))],
            Some(true),
            None,
        );
        let plans = plan_worktrees(&f, "crew", Path::new("/r/repo"));
        assert_eq!(plans.len(), 2, "a fans out; b+c share 'squad'");
        assert_eq!(plans[0].name, "a");
        assert_eq!(plans[1].name, "squad");
        assert_eq!(plans[1].agents, vec!["b", "c"]);
    }

    #[test]
    fn worktree_base_overrides_the_default_so_two_sessions_do_not_collide() {
        let f = fleet(&[("x", Some("x"))], None, Some("../wt-session-2"));
        let plans = plan_worktrees(&f, "crew", Path::new("/home/dev/repo"));
        // Pure path algebra: a relative base is joined onto the repo dir (the `..`
        // is collapsed later, by canonicalize, in the effectful layer) — so build
        // the expectation the same way, keeping the assertion platform-correct.
        let want = Path::new("/home/dev/repo")
            .join("../wt-session-2")
            .join("crew")
            .join("x");
        assert_eq!(
            plans[0].dir, want,
            "a relative base resolves against the repo dir"
        );
    }

    #[test]
    fn an_absolute_worktree_base_is_used_as_is() {
        let f = fleet(&[("x", Some("x"))], None, Some("/tmp/atrium-wt"));
        let plans = plan_worktrees(&f, "crew", Path::new("/home/dev/repo"));
        assert_eq!(plans[0].dir, Path::new("/tmp/atrium-wt/crew/x"));
    }

    #[test]
    fn slug_makes_a_hostile_name_a_safe_branch_and_path_component() {
        assert_eq!(slug("feature/login"), "feature-login", "no path separators");
        assert_eq!(slug(".."), "wt", "cannot escape the base dir");
        assert_eq!(slug(".hidden"), ".hidden", "a single interior dot is fine");
        assert_eq!(slug(""), "wt");
        assert_eq!(
            slug("ok.name_1-2"),
            "ok.name_1-2",
            "safe chars pass through"
        );
        // A name carrying a terminal escape cannot smuggle control bytes into a
        // branch ref or a directory name.
        assert_eq!(slug("lead\u{1b}[2J"), "lead--2J");
    }

    #[test]
    fn a_hostile_shorthand_name_is_slugged_in_branch_and_dir() {
        let f = fleet(&[("feat/x", None)], Some(true), None);
        let plans = plan_worktrees(&f, "crew", Path::new("/r/repo"));
        assert_eq!(
            plans[0].name, "feat/x",
            "the group identity keeps the raw key"
        );
        assert_eq!(plans[0].branch, "atrium/crew/feat-x");
        assert_eq!(plans[0].dir, Path::new("/r/.atrium-worktrees/crew/feat-x"));
    }

    // --- effectful ops, against a real throwaway git repo --------------------

    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// A disposable git repo under the temp dir, with a worktree base beside it.
    /// Removed on drop. Gives every effectful test a clean, isolated repository.
    struct Repo {
        root: PathBuf,
        repo: PathBuf,
    }

    impl Repo {
        fn new(tag: &str) -> Repo {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("atrium-wt-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let repo = root.join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            let run = |args: &[&str]| {
                let ok = Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .status()
                    .unwrap()
                    .success();
                assert!(ok, "setup: git {args:?} failed");
            };
            run(&["init", "-q"]);
            run(&["config", "user.email", "t@example.com"]);
            run(&["config", "user.name", "tester"]);
            run(&["config", "commit.gpgsign", "false"]);
            std::fs::write(repo.join("README.md"), "seed\n").unwrap();
            run(&["add", "."]);
            run(&["commit", "-q", "-m", "init"]);
            Repo { root, repo }
        }

        /// A single-worktree plan whose base is inside this repo's root (so the
        /// whole thing is one cleanable tree, not a scatter of siblings).
        fn plan(&self, name: &str) -> WorktreePlan {
            let base = self.root.join("wt");
            let f = fleet(&[(name, Some(name))], None, Some(base.to_str().unwrap()));
            plan_worktrees(&f, "crew", &self.repo).remove(0)
        }

        fn commit_in(&self, dir: &Path, file: &str) {
            std::fs::write(dir.join(file), "x\n").unwrap();
            for args in [
                vec!["-C", dir.to_str().unwrap(), "add", "."],
                vec!["-C", dir.to_str().unwrap(), "commit", "-q", "-m", "wip"],
            ] {
                assert!(Command::new("git").args(&args).status().unwrap().success());
            }
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn is_git_repo_tells_a_repo_from_a_plain_directory() {
        let r = Repo::new("isrepo");
        assert!(is_git_repo(&r.repo), "the initialized repo is a work tree");
        assert!(
            !is_git_repo(&r.root),
            "its parent holds no repo, so worktrees stay dormant there"
        );
    }

    #[test]
    fn ensure_creates_the_worktree_then_is_idempotent() {
        let r = Repo::new("ensure");
        let plan = r.plan("flake");
        assert_eq!(ensure(&r.repo, &plan), Ok(true), "fresh create");
        assert!(plan.dir.join("README.md").exists(), "HEAD is checked out");
        assert_eq!(
            ensure(&r.repo, &plan),
            Ok(false),
            "a second launch leaves the existing worktree alone"
        );
    }

    #[test]
    fn teardown_removes_a_clean_merged_worktree() {
        let r = Repo::new("rm-clean");
        let plan = r.plan("flake");
        ensure(&r.repo, &plan).unwrap();
        let base = base_ref(&r.repo).unwrap();
        assert_eq!(remove_if_safe(&r.repo, &plan, &base), Ok(Teardown::Removed));
        assert!(!plan.dir.exists(), "a clean, merged worktree is cleaned up");
    }

    #[test]
    fn teardown_keeps_a_worktree_with_uncommitted_changes() {
        let r = Repo::new("rm-dirty");
        let plan = r.plan("flake");
        ensure(&r.repo, &plan).unwrap();
        std::fs::write(plan.dir.join("scratch.txt"), "wip\n").unwrap();
        let base = base_ref(&r.repo).unwrap();
        assert_eq!(
            remove_if_safe(&r.repo, &plan, &base),
            Ok(Teardown::KeptDirty)
        );
        assert!(plan.dir.exists(), "uncommitted work is never destroyed");
    }

    #[test]
    fn teardown_keeps_a_worktree_with_unmerged_commits() {
        let r = Repo::new("rm-unmerged");
        let plan = r.plan("flake");
        ensure(&r.repo, &plan).unwrap();
        r.commit_in(&plan.dir, "feature.rs"); // a commit not on the base branch
        let base = base_ref(&r.repo).unwrap();
        assert_eq!(
            remove_if_safe(&r.repo, &plan, &base),
            Ok(Teardown::KeptUnmerged)
        );
        assert!(plan.dir.exists(), "unmerged commits are never stranded");
    }

    #[test]
    fn seed_links_an_untracked_file_into_a_fresh_worktree() {
        let r = Repo::new("seed");
        // An untracked .env that `git worktree add` would NOT bring along.
        std::fs::write(r.repo.join(".env"), "SECRET=1\n").unwrap();
        let plan = r.plan("flake");
        ensure(&r.repo, &plan).unwrap();
        assert!(
            !plan.dir.join(".env").exists(),
            "a fresh worktree is missing the untracked .env — the gap seed fills"
        );
        let warns = seed(&r.repo, &plan.dir, &[".env".to_string()]);
        assert!(warns.is_empty(), "hardlink should succeed: {warns:?}");
        assert_eq!(
            std::fs::read_to_string(plan.dir.join(".env")).unwrap(),
            "SECRET=1\n"
        );
    }

    #[test]
    fn seed_warns_and_skips_a_source_that_is_not_there() {
        let r = Repo::new("seed-miss");
        let plan = r.plan("flake");
        ensure(&r.repo, &plan).unwrap();
        let warns = seed(&r.repo, &plan.dir, &["nope.env".to_string()]);
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("not found"), "got: {warns:?}");
    }
}
