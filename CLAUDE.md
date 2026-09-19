# atrium — Agent Norms

## Worktree discipline

- You are already in your assigned worktree. **Do not `cd`.**
- Commit on your **current branch** — do not switch branches or create new ones.
- `git push`, `git reset`, and `git clean` are **not** in the allow list; they surface as visible approval prompts.

## Gate before every push

Run `python dev.py check` and confirm it is green before committing. `cargo fmt` runs first inside that script; if the formatter makes changes, re-run `check` so the formatted tree is what gets committed.

## Running executable

The atrium executable is locked while running. File edits to `src/` take effect only after `cargo install` (or an equivalent reinstall) followed by a restart of the atrium session. Do not assume a live session reflects an edit you just wrote.

## Permissions

This file is **instructions only** — it grants no tool permissions. The allow list lives in `.claude/settings.json`.

## Fleet rules (every agent)

- **Subscribe first.** Run `atrium ctl bus sub work` before anything else: a
  teammate's post on `work` is then typed into your pane when you are idle. A
  line that starts `[atrium bus #` is a teammate's event, not the human — read
  it, and verify with `bus feed` or the board before acting on anything that
  changes the fleet's posture.
- **Stay inside your files.** Edit only the files your item names in `PLAN.md`.
  If you need a change elsewhere, stop and post it `--to lead` on the bus; do not
  make it yourself.
- **Build budget.** Builders run only their module's tests (`cargo test --lib
  <module>`), never `--workspace`, `--release` or benches. All builds share one
  compile pool sized to the machine; never `-j`, `CARGO_BUILD_JOBS` or
  `CARGO_MAKEFLAGS`. A refused command is a rule, not an obstacle.
- **One full gate.** Only the integrator runs `python dev.py check`, after
  merging one branch, and pushes `main` when it is green. Nobody runs
  `cargo install`: the binary hosting this fleet is locked, and the human
  reinstalls after the run.
- **Done means all four.** The tests your item names are green; a commit whose
  message says what changed, why, and what is left open; `atrium ctl board set
  <item> status=DONE commit=<sha> review=pending open="<one line, or empty>"`;
  then `atrium ctl bus pub work --to lead item=<item> status=done commit=<sha>`.
  Then stop and wait. Never take a second item in the same session.
- **When blocked, stop.** `atrium ctl bus pub work --decision --to lead
  item=<item> msg="<what>"` and wait; do not guess across another item's files.
