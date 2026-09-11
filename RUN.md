# atrium-dev — founder launch checklist

The `atrium-dev` fleet (defined in `atrium.fleet.json`) uses atrium to develop
atrium. Read `PLAN.md` for the roster and backlog. This file is the human
pre-flight — do these steps in order from a **real terminal** (agents can't drive
the TUI, and the running `atrium.exe` is locked, so you host the fleet).

## 0. One-time: hide the scaffolding from git (optional but tidy)

`atrium.fleet.json` is a tracked fixture — adding `atrium-dev` shows as a modified
file. `PLAN.md` / `RUN.md` are new untracked files. None of it reaches the workers
(their worktrees are checked out from HEAD), and the integrator only `git merge`s,
so nothing here gets committed by the fleet. To keep `git status` clean during the
run, exclude them locally (does not modify the tracked `.gitignore`):

```
printf '\nPLAN.md\nRUN.md\n' >> .git/info/exclude
```

When you're done, revert the fixture: `git checkout atrium.fleet.json`.

## 1. Pre-create the sibling-crate junctions (round 1 only)

atrium's `Cargo.toml` pins its sibling crates with **relative** path deps
(`abus = ../abus`, plus `pty-rs`, `akey`, `rawterm-rs`, `ansi-rs`, `vterm-rs`,
`agsess`, `json-rs`). Inside a worktree at `..\.atrium-worktrees\atrium-dev\<name>\`,
`../abus` resolves to `..\atrium-dev\abus` — which doesn't exist — so `dev.py
check` can't build. Create the junctions once, beside where the worktrees will
live, so every worktree resolves its deps. (The `seed` worker automates this; from
round 2 on you can skip this step.)

```powershell
$base  = "D:\projects\nativelite\repos\.atrium-worktrees\atrium-dev"
$repos = "D:\projects\nativelite\repos"
New-Item -ItemType Directory -Force $base | Out-Null
foreach ($s in "abus","pty-rs","akey","rawterm-rs","ansi-rs","vterm-rs","agsess","json-rs") {
  $link = Join-Path $base $s
  if (-not (Test-Path $link)) {
    New-Item -ItemType Junction -Path $link -Target (Join-Path $repos $s) | Out-Null
  }
}
```

Verify: `Get-ChildItem $base` should list eight junctions pointing at the real crates.

## 2. Launch

From the atrium repo root (`D:\projects\nativelite\repos\atrium`), with a
**fresh** installed atrium (not the one you're editing — that exe is locked):

```
atrium --trust automode up atrium-dev
```

`--trust automode` puts every pane in hands-off mode. All eight panes are
opus/sonnet, which support automode, so no per-agent `trust` overrides are needed
(if you ever add a haiku worker, give it `"trust": "accept"` in its agent entry —
haiku has no automode). `allow_ctl` is already set in the fleet, so the board/bus
control plane is live. atrium discloses the worktree plan in the launch banner and
materializes the five worktrees after you ack.

## 3. Let it run

The lead assigns via board/bus; the five workers build in their own worktrees and
each drives `python dev.py check` to green before committing on its own branch; the
reviewer verifies adversarially and can block; the integrator merges
(`trust, seed, norms, scaffold, docs`), gates the merged main tree, and commits.
Watch progress from the TUI, or tail the bus.

## 4. Ship the round (founder-only — the fleet stops here)

Once the integrator reports the merged main tree green and committed:

```
cargo install --path . --force   # requires no atrium instance holding the exe
```

Close this atrium instance first if it complains "Access is denied (os error 5)" —
the running exe locks itself. Then restart atrium; the round's changes are now live
(worktree norms injected, `cargo clean` hands-off, matcher format aligned,
`.claude/settings.json` present, sibling junctions auto-seeded).

Reclaim any worktrees teardown kept: `atrium fleet clean atrium-dev`.

## 5. Round 2

With `seed` shipped, step 1 (manual junctions) is no longer needed. The next
backlog item is `#3` — `atrium ctl spawn --worktree <name>` and
`respawn <pane> --worktree <name>` (cross-file: `ctl.rs` + `main.rs` +
`fleet_cli.rs`; respawn-in-place, since you can't chdir a running process). Draft a
round-2 roster the same way once round 1 is live.
