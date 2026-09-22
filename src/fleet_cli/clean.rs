//! `atrium fleet clean`: reclaim a fleet's per-agent worktrees, and the teardown
//! `fleet up` also runs when the session ends.

use super::fsan;
use crate::*;

/// `atrium fleet clean <name>` — reclaim the per-agent worktrees a fleet's run
/// left behind, now that they are merged.
///
/// Teardown at the end of `fleet up` already removes every worktree that is clean
/// and merged; the ones it *keeps* are exactly those with uncommitted or unmerged
/// work, reported so nothing is silently destroyed. After you merge (or discard)
/// that work, this command re-applies the same clean+merged test and removes the
/// ones that now pass — the deliberate second pass the teardown intentionally
/// refuses to make for you. Run it from the **same directory** the fleet was
/// launched in, so the worktree paths resolve identically. Never destroys a tree
/// that is still dirty or unmerged; those are reported and kept, as on exit.
pub(crate) fn fleet_clean(name: &str) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let located = match atrium::fleet::discover(&cwd) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("atrium fleet: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = match std::fs::read_to_string(&located.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("atrium fleet: cannot read {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleets = match atrium::fleet::parse(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("atrium fleet: {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleet = match fleets.get(name) {
        Some(f) => f,
        None => {
            eprintln!("atrium fleet: no fleet named \"{}\"", fsan(name));
            return ExitCode::FAILURE;
        }
    };

    let plans = atrium::worktree::plan_worktrees(fleet, name, &cwd);
    if plans.is_empty() {
        println!(
            "atrium fleet: fleet \"{}\" declares no worktrees",
            fsan(name)
        );
        return ExitCode::SUCCESS;
    }
    if !atrium::worktree::is_git_repo(&cwd) {
        eprintln!(
            "atrium fleet: {} is not a git repo — nothing to clean",
            cwd.display()
        );
        return ExitCode::FAILURE;
    }
    let _ = atrium::worktree::prune(&cwd);
    let base = match atrium::worktree::base_ref(&cwd) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("atrium fleet: cannot read the repo's HEAD: {e}");
            return ExitCode::FAILURE;
        }
    };

    use atrium::worktree::Teardown;
    let mut kept = 0usize;
    for p in &plans {
        match atrium::worktree::remove_if_safe(&cwd, p, &base) {
            Ok(Teardown::Removed) => {
                println!("atrium fleet: removed worktree \"{}\"", fsan(&p.name))
            }
            Ok(Teardown::Missing) => {}
            Ok(Teardown::KeptDirty) => {
                kept += 1;
                eprintln!(
                    "atrium fleet: kept \"{}\" at {} (branch {}) — still has uncommitted changes",
                    fsan(&p.name),
                    atrium::fleet::show_path(&p.dir),
                    fsan(&p.branch)
                );
            }
            Ok(Teardown::KeptUnmerged) => {
                kept += 1;
                eprintln!(
                    "atrium fleet: kept \"{}\" at {} (branch {}) — still has unmerged commits",
                    fsan(&p.name),
                    atrium::fleet::show_path(&p.dir),
                    fsan(&p.branch)
                );
            }
            Err(e) => eprintln!("atrium fleet: worktree \"{}\": {e}", fsan(&p.name)),
        }
    }
    let _ = atrium::worktree::prune(&cwd);
    if kept > 0 {
        println!(
            "atrium fleet: {kept} worktree(s) kept — merge or discard their work, then re-run"
        );
    }
    ExitCode::SUCCESS
}

/// Tear down a fleet's worktrees, printing a line for each that is kept (with a
/// reason) so the operator knows exactly what is left to merge or remove.
pub(super) fn report_worktree_teardown(
    cwd: &std::path::Path,
    plans: &[atrium::worktree::WorktreePlan],
    base: &str,
) {
    use atrium::worktree::Teardown;
    for p in plans {
        match atrium::worktree::remove_if_safe(cwd, p, base) {
            Ok(Teardown::Removed) | Ok(Teardown::Missing) => {}
            Ok(Teardown::KeptDirty) => eprintln!(
                "atrium fleet: kept worktree \"{}\" at {} (branch {}) — it has uncommitted changes",
                fsan(&p.name),
                atrium::fleet::show_path(&p.dir),
                fsan(&p.branch)
            ),
            Ok(Teardown::KeptUnmerged) => eprintln!(
                "atrium fleet: kept worktree \"{}\" at {} (branch {}) — it has unmerged commits; \
                 merge or remove it deliberately",
                fsan(&p.name),
                atrium::fleet::show_path(&p.dir),
                fsan(&p.branch)
            ),
            Err(e) => eprintln!("atrium fleet: worktree \"{}\" teardown: {e}", fsan(&p.name)),
        }
    }
}
