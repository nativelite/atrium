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
}
