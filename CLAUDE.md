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
