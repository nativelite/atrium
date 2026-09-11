# Developing atrium with atrium: the dev-fleet pattern

> **This page documents what this very fleet embodies.** The atrium-dev fleet
> runs atrium to develop atrium — each worker in its own per-agent worktree,
> coordinating over the board and bus, gating every push with `python dev.py
> check`. The pattern is repeatable for any multi-concern Rust (or other
> compiled) repo.

## The bootstrap moment

The first time you run a worktree fleet, atrium does not yet know about worktrees
(or the feature you are adding). You use the *currently installed* binary —
the old atrium — to launch a fleet that produces the new atrium. Once the work
is merged and the new binary is built and installed, the next fleet launch uses
the upgraded atrium to develop the next feature. This is the bootstrap cycle:

```
develop-in-worktrees
  → python dev.py check  (the sole gate; there is no CI)
  → git add -A && git commit
  → integrator merges all branches to main
  → cargo install --path . --force   ← must stop atrium first
  → restart atrium with the new binary
```

**Why stop first.** On Windows the running `.exe` is locked — you cannot
overwrite it in place. `cargo install --force` will fail unless the old process
has exited. The pattern is: finish the fleet run, `cargo install`, then launch
the next fleet with the new binary.

## The fleet config

A dev-fleet for atrium looks like this:

```jsonc
{
  "fleets": {
    "atrium-dev": {
      "allow_ctl": true,
      "trust": "automode",
      "worktrees": true,
      "worktree_seed": [".env"],
      "agents": [
        { "name": "lead",       "cmd": ["claude"],  "can_spawn": true },
        { "name": "trust",      "cmd": ["claude"] },
        { "name": "norms",      "cmd": ["claude"] },
        { "name": "seed",       "cmd": ["claude"] },
        { "name": "scaffold",   "cmd": ["claude"] },
        { "name": "docs",       "cmd": ["claude"] },
        { "name": "reviewer",   "cmd": ["claude"] },
        { "name": "integrator", "cmd": ["claude"] }
      ]
    }
  }
}
```

`"worktrees": true` is the full fan-out shorthand: every agent gets its own
branch and directory, named after itself. The lead and integrator can stay in
the main tree by giving them an explicit `"worktree": ""` — or they can have
their own trees too and commit only to coordinate; that is a team call. The
key principle is **one concern per worktree** so one agent's WIP can never fail
another agent's `dev.py check` gate.

## Per-agent isolation in a Rust repo

Without worktrees, a fleet's agents all write to **one** working tree, and
`cargo fmt --check` — which runs first in `dev.py check` — fails the
**whole** tree the moment any agent leaves unformatted code. A different agent's
clean, review-passed work cannot commit until the offender is fixed. This is
"gate contamination" (bus #105 from the ctl-fixes fleet: *"your ctl.rs pure
seams FAIL cargo fmt --check … is NOT dev.py-green"*).

With `"worktrees": true`, each agent checks out its own copy of `HEAD`:

```
.atrium-worktrees/
  atrium-dev/
    trust/      ← branch: atrium/atrium-dev/trust
    norms/      ← branch: atrium/atrium-dev/norms
    seed/       ← branch: atrium/atrium-dev/seed
    scaffold/   ← branch: atrium/atrium-dev/scaffold
    docs/       ← branch: atrium/atrium-dev/docs
    reviewer/
    integrator/
```

Each of those directories is a full git worktree. An agent that leaves WIP
unformatted in its tree fails only **its own** gate; every other agent's
`dev.py check` runs in isolation and stays green.

The worktrees are created **before** any pane spawns, so atrium fails fast if
git is unavailable or the repo is not a git repository. They are placed as a
**sibling** of the repo (`../.atrium-worktrees/<fleet>/`) so they never show
up as untracked files inside the repo itself.

## The sibling relative-path-dep pitfall

When a Cargo project references a sibling repo via a relative path — for example
a dev-dependency declared as `path = "../atrium-skill-kit"` — that path is
**resolved from the worktree's directory**, not from the main repo root. A
worktree at `../.atrium-worktrees/atrium-dev/seed/` would look for
`../atrium-skill-kit` one level up from itself, landing at
`../.atrium-worktrees/atrium-dev/atrium-skill-kit/`, which does not exist.
`cargo build` in that worktree fails immediately with a `No such file or
directory` error.

**The fix: `worktree_seed` with a junction.** Add the sibling to the seed list:

```jsonc
"worktree_seed": ["../atrium-skill-kit"]
```

The seed function resolves the source relative to the main repo root (`cwd`):
`cwd.join("../atrium-skill-kit")` → the real sibling. The destination resolves
relative to the worktree directory (`dir`): `dir.join("../atrium-skill-kit")`
→ the slot beside the worktree where `cargo` expects it. On Windows this link is
created as an **NTFS junction** (no admin rights needed), on Unix as a
**symlink**. A hardlink falls back to a copy only when both junction and symlink
fail. The result: every worktree can `cargo build` immediately, and edits to the
linked sibling propagate to all trees that share it.

Build caches are explicitly **not** seeded — they can be shared via the
`CARGO_TARGET_DIR` env var set on agents that want it, or left isolated so each
tree builds independently. Both are valid and neither is a default.

## Worktree behavior norms

A fleet running under `"trust": "automode"` lets agents run autonomously — but
`automode` is model-gated: Claude Haiku reports "this model does not have
automode." A mixed fleet (Sonnet lead + Haiku workers) therefore sets a
**per-agent trust override** on the Haiku agents:

```jsonc
{ "name": "docs", "cmd": ["claude", "--model", "haiku"], "trust": "accept" }
```

The `"trust"` field is a **request**, capped to the session ceiling. An agent
can de-escalate (run at `accept` under an `automode` session), but never
escalate past what the human approved at launch. atrium prints any effective
difference in the pre-flight banner so the operator always knows what posture
each agent actually runs under.

Under `"trust": "accept"`, agents land in `acceptEdits` mode with an
allow-list of safe commands. The allow-list was extended to include the
**git coordination loop** a worktree fleet runs on, so agents do not prompt on
every commit:

| Allowed (hands-off) | NOT allowed (still prompts) |
| --- | --- |
| `git status` | `git push` (network) |
| `git log` | `git reset` (destructive) |
| `git diff` | `git clean` (destructive) |
| `git branch` | `git push --force` |
| `git show` | |
| `git add` | |
| `git commit` | |
| `git merge` | |
| `git worktree` | |

The scoping is deliberate: inspect, stage, commit, merge, and manage worktrees
are all local and reversible; network and destructive git operations stay a
visible prompt even under the most permissive session policy.

## The `.claude` scaffold

Each worktree receives a committed `.claude/` directory that seeds it with the
context the agent needs without carrying session-level settings:

**`.claude/settings.json`** — a project-level settings file committed alongside
the code. It carries the Claude Code tool allowlist for this worktree (matching
what the trust layer allows via the fleet's `accept` posture), any MCP plugins
the agent needs, and the model/effort defaults appropriate for this role. Being
committed to the branch, it travels with `git worktree add` and is present the
instant the agent's pane opens.

**`CLAUDE.md`** — the agent's standing instructions for this worktree. It
declares the agent's scope ("you own `src/worktree.rs`"), links the one-file
ownership discipline, and names the files the agent must NOT touch (those
claimed by teammates on the board). A minimal CLAUDE.md entry looks like:

```markdown
# seed — worktree.rs owner

You own `src/worktree.rs` only. Do not edit files owned by other agents (check
`atrium ctl board list`). Gate every push: `python dev.py check` must be green.
```

Together, settings.json and CLAUDE.md mean the agent never has to be told its
role via the kickoff prompt alone — the files are always there and always
correct for the branch it is on.

## Claim before you build

A worktree stops gate contamination, but two worktrees can still produce a
merge conflict if both edit the same file. The protocol is simple:

1. **Claim your file** on the board before touching it:
   `atrium ctl board claim src/worktree.rs`
2. **Check the board** before opening any file you did not claim:
   `atrium ctl board list`
3. **Post a blocker** if you need a file owned by someone else:
   `atrium ctl bus pub atrium-dev --decision --to <owner> q="need to touch X"`

Board claims are leased and auto-renewed while you work; if a claim is denied,
it names who holds it. The board + bus are the same primitives the fleet uses for
everything else — worktree isolation does not add new coordination overhead, it
just uses what is already there.

## The development loop end-to-end

```
1.  atrium fleet atrium-dev     # launches fleet; creates worktrees off HEAD
2.  [each agent] claim your file on the board
3.  [each agent] write failing test → make it pass → cargo fmt
4.  [each agent] python dev.py check  (green → commit on your branch)
5.  [reviewer]   inspect diff; post pass or blockers on the bus
6.  [integrator] merge all green branches to main; resolve any conflicts
7.  [all]        fleet run ends; clean worktrees are auto-removed
8.  cargo install --path . --force   ← after fleet exits (Windows: exe unlocked)
9.  Launch next fleet with the new binary
```

Step 4 is the **sole gate** — there is no CI. Running the local check before
every commit is what protects `main`, and every package ships a `dev.py` so the
check runs in seconds. The check order is:

```
cargo fmt --check  →  cargo clippy  →  cargo test
```

Formatting runs first because it is the fastest filter and the most common
cause of gate contamination in a multi-agent tree. Fix it before running tests.

## When the fleet is the product

The atrium-dev fleet is particularly self-referential: it uses atrium's board
and bus to coordinate agents that are adding new atrium features, running inside
worktrees that the new atrium code will create. The running atrium binary is the
old one. The new one lives in the worktrees until it is merged, installed, and
restarted.

This means the fleet cannot dogfood the very features it is landing until the
next restart. It can, however, verify them: `cargo test` in any worktree runs
the unit tests (pure seam + effectful git tests) for the new code, giving
confidence before the binary is ever installed.
