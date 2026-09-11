# Per-agent worktrees

> **Status: shipped.** Opt-in per-agent git worktrees, the structural fix for
> coordination finding #5 (shared-checkout gating). Configured in
> `atrium.fleet.json`; dormant unless a fleet asks for it.

## The problem

A fleet's agents all edit **one working tree**, and `dev.py check` gates the
**whole tree**. Two failure modes follow, both seen in the ctl-fixes run:

- **Gate contamination.** One agent's half-finished, unformatted WIP fails
  `cargo fmt --check` (which runs *first*), so a *different* agent's clean,
  review-passed work **can't get a green gate to commit**. (bus #105: *"your
  ctl.rs pure seams FAIL cargo fmt --check … is NOT dev.py-green"*.)
- **Same-file interleave.** Two agents editing the same files in one tree can't
  cleanly separate their commits; it takes manual "commit-sequencing" and
  "commit-split" gymnastics. (bus #106/#111: *"W1 and W2 both edit the SAME
  files (ctl.rs + main.rs)"*.)

Today's only mitigation is discipline — *keep-tree-green* + *path-scoped
commits*. Fragile and human.

## Goals / non-goals

**Goals**

- Let each agent (or group of agents) work in an **isolated working tree** on its
  own branch, so one agent's WIP can never fail another's gate.
- Keep the mechanism **opt-in**, **language-agnostic**, and **general-purpose** —
  a non-coding fleet (research, ops, anything) is completely unaffected.
- **Never destroy unmerged work** on teardown.
- Make the resulting branches **easy to merge** by reusing the coordination
  primitives atrium already has (the board and the bus), not by building a new
  merge engine.

**Non-goals (for v1)**

- An automatic merge engine / conflict resolver. v1 isolates; integration is
  surfaced and assisted, not automated.
- Any build-system or language knowledge baked into atrium.

## Design overview

Isolation is switched on **per agent** via a single config key, `worktree`. The
value is a **group name**:

- **distinct value →** that agent gets its **own** worktree + branch (solo
  isolation).
- **shared value →** every agent with that value **co-develops one** worktree +
  branch (a squad working the same concern together).
- **absent →** the agent uses the **main tree** — exactly today's behavior, fully
  backward-compatible.

One key therefore expresses solo work, parallel squads, and the legacy
single-tree mode. Nothing engages unless the key is present, so this is purely
additive.

```jsonc
{
  "fleets": {
    "ctl-fixes": {
      "allow_ctl": true,
      "agents": [
        { "name": "lead",         "cmd": ["claude"] },                    // main tree
        { "name": "flake",        "cmd": ["claude"], "worktree": "flake" },// solo tree+branch
        { "name": "controlplane", "cmd": ["claude"], "worktree": "cp" },   // solo
        { "name": "cp-helper",    "cmd": ["claude"], "worktree": "cp" }    // shares cp's tree+branch
      ]
    }
  }
}
```

A fleet-level shorthand `"worktrees": true` means "every agent gets its own
worktree, named after itself" — the common "full fan-out" case without repeating
the key.

### Language-agnostic: atrium invents no build logic

atrium hosts panes running arbitrary commands; it does not know or care whether
an agent compiles Rust, runs Python, or does research. So **build caching is not
atrium's concern** — it is a per-language choice the fleet expresses in config:

- **Interpreted languages** (Python, JS, Ruby): no compile, no cache — isolation
  just works.
- **Compiled languages** (Rust, Go, C++): each worktree builds independently by
  default. A fleet that wants a shared compiler cache sets the env var itself on
  its agents; atrium passes env through, it does not manage it.

This keeps every **build topology** a config choice, never a hardcode:

| Topology | How | When |
| --- | --- | --- |
| Isolated builds | default (separate worktrees) | correctness first; disk/CPU cheap |
| Shared cache | set `CARGO_TARGET_DIR` (etc.) on agents | faster, accept some `cargo` contention |
| Syntax/review per agent, **compile last** | agents lint/review in their trees; one integration step compiles the merged result | the "divvy it back, compile once" flow |
| Dedicated build agent | one agent owns compilation; others only edit/review | heavy builds, one source of truth |

## Lifecycle

**On `fleet up`** (only when at least one agent names a `worktree`, and the cwd is
a git repo):

1. For each **distinct** worktree name, run `git worktree add <dir> -b <branch>`
   off the current `HEAD`.
2. Each pane's working directory is set to its worktree `<dir>`. Agents with no
   key stay in the main tree.

**Naming (deterministic, greppable):**

- Branch: `atrium/<fleet>/<worktree>`
- Directory: `../.atrium-worktrees/<fleet>/<worktree>` — a **sibling** of the
  repo by default, so the worktrees never show up as untracked files inside it.
  Override the base with the fleet-level `"worktree_base"` key (resolved against
  the fleet dir; absolute used as-is) — set it to give two concurrent sessions
  on the same repo **distinct** bases so they don't collide on the same
  directories and branches.

The worktree/branch name is slugged to a safe single path/ref component
(anything outside `[A-Za-z0-9._-]` → `-`), so an agent name carrying a slash or
a control char can't produce an invalid branch or escape the base dir. Two
distinct names that slug alike would collide — a documented v1 sharp edge; keep
worktree names simple.

**On teardown — never destroy unmerged work:**

- `git worktree remove <dir>` **only if** the tree is clean **and** its branch has
  no commits that aren't already on the base.
- Otherwise **leave it** and report the path + branch in the exit banner, so you
  can inspect or merge it deliberately.
- `git worktree prune` clears orphaned entries on the next launch (an agent that
  crashed, a directory deleted by hand).

Once you have merged (or discarded) the work in a kept worktree, reclaim it with
`atrium fleet clean <name>`, run from the directory the fleet launched in. It
re-applies the same clean+merged test and removes the ones that now pass, still
refusing to destroy anything dirty or unmerged. It is implemented as a `fleet`
subcommand, not `ctl cleanup`: the cleanup happens *after* the fleet has exited,
so there is no running instance for ctl's IPC to reach.

## Seeding untracked shared state

`git worktree add` checks out only the files git **tracks**. Anything untracked —
`.env`, local config, secrets — **does not appear** in a fresh worktree, so an
agent can land in a tree that's missing the `.env` it needs to run.

The fix is an optional, configurable **seed list** that atrium **links** (not
copies — copies go stale and clutter) into each worktree, with a Windows-friendly,
no-privilege strategy:

```jsonc
"worktree_seed": [".env", "config.local.json"]
```

- **Files** → **hardlink** (same inode; edits propagate; no admin needed on
  Windows).
- **Directories** → **junction** (no admin needed on Windows).
- **Copy** only as a fallback if linking fails.
- **Build caches are never seeded** — those belong to the env-var path above.
- Default is **empty / opt-in**: atrium seeds nothing unless told, so behavior is
  never surprising.

## Keeping merges easy (reuse, don't rebuild)

A git merge is conflict-free when no two branches touch the same file. atrium
already has the two primitives to arrange that — so "easy merges" is a
**convention over existing tools**, not a new subsystem:

- **Disjoint ownership via `board claim`.** An agent claims the files/modules it
  will edit before touching them; two worktrees cannot both claim `ctl.rs`. This
  turns the *single-owner-per-file* working agreement into something *enforced*,
  and directly prevents the #111 "both edit the SAME files" conflict.
- **Conflict escalation via `bus pub --to <owner>`.** When a real conflict does
  arise at integration, it is routed to the owning agent over the bus (the
  delivered-wake `--to` path), instead of silently blocking a commit.
- **Merge-train (future, out of v1 scope).** Because atrium knows every agent's
  branch, a later helper can merge each green/clean branch into the base in turn,
  rebasing the rest, and route any conflict to its owner. v1 stops at isolation +
  ownership + escalation.

## Configurable and general-purpose

Everything git/merge-related is **opt-in**. With no `worktree` key, atrium behaves
exactly as it does today. A fleet used for **research, ops, or anything
non-coding** never touches worktrees or git — it keeps using the board and bus as
general coordination primitives. "Coding mode" is a layer you switch on, not a
change to what atrium fundamentally is.

## Ad-hoc worktrees — `ctl spawn --worktree`

A `fleet.json` entry is the right home for a standing team. But a quick ad-hoc
teammate does not need a fleet definition. Pass `--worktree <name>` directly to a
live `ctl spawn` and atrium handles the rest:

```
atrium ctl spawn --role scout --worktree probe -- claude
atrium ctl send scout "explore the auth module and report findings"
```

What happens:

- atrium looks up (or creates) a git worktree named `<name>` under a fixed
  **`adhoc`** slot — branch `atrium/adhoc/<name>`, directory
  `<worktree_base>/adhoc/<name>`. The name is slugged the same way fleet names
  are (anything outside `[A-Za-z0-9._-]` → `-`), so `feat/login` becomes
  `atrium/adhoc/feat-login`.
- The spawned pane's working directory is set to that worktree, so its
  `dev.py check` and `git commit` run isolated from every other tree.
- The **worktree behavioral norms** — "you are in worktree `<name>` on branch
  `<branch>`, do not cd, do not create new branches, commit on your current
  branch, announce tersely on the bus" — are injected as a system-prompt append,
  exactly as fleet members get them. No fleet file required.

If the worktree `<name>` already exists (an earlier teammate in the session
created it), the spawn **reuses it**. Two teammates with `--worktree squad`
co-develop one tree and one branch — the same group-share semantics as the
fleet `"worktree"` config key.

The worktree can be reclaimed after the session with `atrium fleet clean`, which
applies the same clean-and-merged safety test it applies to fleet-created
worktrees.

## Respawning a pane into a worktree — `ctl respawn`

Once a pane's child process is running you cannot `chdir` it from outside: the
process owns its working directory and there is no cross-process chdir primitive
on any supported platform. The only way to move a live pane into a different
directory is **respawn-in-place**: kill the child, then relaunch it with the new
cwd.

```
atrium ctl respawn scout --worktree probe
```

What `respawn` does:

1. Sends the pane's current child a graceful stop and waits for it to exit.
2. Creates the named worktree if it does not yet exist (same naming rules as
   `spawn --worktree`).
3. Relaunches the same command — same argv, role, and identity — with its working
   directory set to the worktree.
4. Re-injects the worktree behavioral norms into the fresh session.

The pane id and role are preserved. Teammates that target `scout` by role keep
reaching it after the respawn. Board state is yours to carry forward or reset —
a new session does not automatically inherit the previous session's conversation
context.

**When to use each:**

| Goal | Command |
| --- | --- |
| New teammate, isolated tree from the start | `ctl spawn --worktree <name> -- <cmd>` |
| Move an existing pane into a worktree mid-session | `ctl respawn <pane> --worktree <name>` |

## Implementation plan

Single-owner-per-file, pure seam + regression test per change, as usual.

1. **Config (`fleet.rs`).** Add `worktree: Option<String>` to `Agent` and
   `worktrees: Option<bool>` + `worktree_seed: Option<Vec<String>>` to `Fleet`;
   parse with clear errors; tests for parse/absent/shorthand. (Mirrors the
   `topics` work just landed.)
2. **Pure naming seam (new `worktree.rs`).** `plan_worktrees(fleet, cwd) ->
   Vec<WorktreePlan { name, dir, branch, agents }>` — no side effects, fully
   unit-tested (distinct vs shared names, legacy passthrough, branch/dir
   strings, the `worktrees: true` shorthand).
3. **Effectful git ops (behind the seam).** `git worktree add/remove/prune`,
   dirtiness/unmerged checks, seed linking (hardlink/junction/copy fallback). Thin
   wrapper over the plan; the decision logic stays in the pure seam.
4. **Spawn wiring (`fleet_cli.rs`).** Create worktrees before spawning panes; set
   each pane's cwd to its worktree dir; thread the plan into teardown.
5. **Teardown safety.** Clean+merged → remove; else keep + report. Prune orphans
   on launch.
6. **Docs.** Flip this banner; add `worktrees` to `docs/build.py` `PAGES`.

## Bootstrap note

The **first** implementation cannot run *inside* worktrees (they don't exist yet
— chicken and egg), but it can still use the fleet's board/bus coordination. Once
landed, atrium can dogfood its own worktrees for subsequent work.

## Resolved decisions

The three open questions from the design phase, as shipped:

- **Worktree dir location** — sibling `../.atrium-worktrees/<fleet>/` by
  default, **configurable** via `"worktree_base"` so two concurrent sessions on
  one repo don't collide.
- **Explicit cleanup** — yes: `atrium fleet clean <name>` reclaims kept
  worktrees after a manual merge; `git worktree prune` still runs automatically
  on every launch for orphans.
- **`worktrees: true` shorthand** — kept; it expresses the common full-fan-out
  case without repeating the key, and an explicit per-agent `worktree` still
  overrides it so a squad can group.
