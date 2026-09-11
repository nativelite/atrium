# atrium-dev — round 1 plan

A fleet of atrium agents developing atrium itself. This file is visible to the
**main-tree** agents (lead, reviewer, integrator); the five workers get the same
information self-contained in their kickoffs because their worktrees are checked
out from HEAD and cannot see this untracked file.

## Roster (8 panes, grid 2x4)

| pane | model | tree | owns | task |
|------|-------|------|------|------|
| lead | opus | main | — | decompose, assign via board/bus, set merge order |
| trust | sonnet | worktree `trust` | `src/trust.rs` | #2 `cargo clean` on allowlist + #4 matcher format |
| norms | sonnet | worktree `norms` | `src/fleet_cli.rs` | #1 worktree-norms system-prompt injection |
| seed | sonnet | worktree `seed` | `src/worktree.rs` | #6 auto-junction sibling path-dep crates |
| scaffold | sonnet | worktree `scaffold` | `.claude/settings.json`, `CLAUDE.md` | #5 hands-off scaffold |
| docs | sonnet | worktree `docs` | `docs/` | document the dev workflow |
| reviewer | opus | main | — | adversarial verify, block integrator |
| integrator | sonnet | main | — | merge branches, gate merged tree, commit |

**Single-owner-per-file is the merge-safety invariant.** Every worker edits
exactly one file (or disjoint new files). No two workers share a file this round,
so the four-way (five-way) merge is clean by construction. If any worker finds it
must touch a file it does not own, it STOPS and posts on the bus; the lead
re-partitions — nobody edits across ownership lines silently.

## Backlog (this round)

1. **norms** — worktree-norms system-prompt injection (`fleet_cli.rs`). When a
   fleet member has a `WorktreePlan`, append a second `--append-system-prompt`
   block: "you're in worktree `<name>` on branch `<branch>`, don't cd, run no git
   worktree commands, commit on your current branch, announce tersely." Mirrors
   `main.rs:3590`'s `AGENT_CTL_DIRECTIVE` append but scoped to worktree members.
   Behavior/instructions only — grants no permissions.
2. **trust** — add `cargo clean` (consider `cargo run`) to `DEFAULT_ALLOW`
   (`trust.rs:38`). Keep the deliberate exclusions (no git push/reset/clean).
4. **trust** — verify the `--allowedTools` matcher format. `accept_edits_args`
   (`trust.rs:~118`) emits `Bash(P *)` (SPACE); Claude Code docs use `Bash(P:*)`
   (COLON). Determine which the installed Claude Code honors; align + pin a test.
   This is the likely cause of the "commands still prompt" edge cases.
5. **scaffold** — committed `.claude/settings.json` (`permissions.allow` mirroring
   the vetted `DEFAULT_ALLOW`, exclusions intact, correct matcher form per #4) +
   `CLAUDE.md` behavior norms. settings.json = enforcement (inherited by every
   worktree off HEAD, model-independent); CLAUDE.md = instructions only.
6. **seed** — the atrium-on-atrium build fix. A repo with relative sibling
   path-deps does not build inside a worktree (`../abus` resolves under the
   worktree base, not the repo parent). After `ensure`, junction each sibling
   path-dep crate into `<base>/<fleet>/` so `../<x>` resolves from every worktree.
   Idempotent; tested. **This removes the manual junction pre-step for round 2+.**

Deferred to **round 2** (after this round merges + reinstalls): backlog #3 —
`atrium ctl spawn --worktree <name>` and `respawn <pane> --worktree <name>`. It is
cross-file (`ctl.rs` + `main.rs` + `fleet_cli.rs`) and collides with `norms`'
ownership of `fleet_cli.rs`, so it waits until `norms` is merged. Respawn-in-place
(kill the pane's child, relaunch with new cwd) — you CANNOT chdir a running
process from outside.

## Gate

`python dev.py check` (guard → **fmt first** → test; folds sibling abus). fmt
reflows and fails the gate on a whitespace nit — run `cargo fmt` before every
re-run (the R6 trap). Every worker's worktree must be green before merge.

## Merge order (integrator)

`trust, seed, norms, scaffold, docs` — all disjoint, expect conflict-free merges.
Then `python dev.py check` on the merged main tree (builds normally there, no
junctions needed) must be green; commit. Do **not** reinstall or restart — the
founder owns that (the running exe is locked).

## Bootstrap loop (per round)

develop-in-worktrees → `python dev.py check` green in each → merge → gate merged
main tree → founder: `cargo install --path . --force` → restart atrium. Changes
go live only after reinstall+restart; there is no hot-reload.
