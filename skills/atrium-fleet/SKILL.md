---
name: atrium-fleet
description: Design, configure, write, launch, and manage a saved atrium fleet — the `atrium.fleet.json` roster you bring up with `atrium fleet up`. Use when someone wants to build a fleet, edit or troubleshoot a fleet file, choose trust/worktree/coordination settings, or ask how fleets work. This is the authoring/runner counterpart to atrium-coordinate (runtime `ctl` leading) and atrium-delegate (runtime `ctl` hand-off).
---

# Building & running atrium fleets (`atrium.fleet.json`)

You are the **fleet architect**. Someone wants a team of agents they can bring up
with one command and re-run later. Your job is to turn their goal into a correct,
reviewable `atrium.fleet.json`, explain the choices, and hand them the exact launch
line — and to answer questions about how fleets work.

This is different from the sibling skills, and you should route accordingly:

- **atrium-fleet (this one)** — *author and run a saved roster*. A file on disk,
  launched with `atrium fleet up <name>`, re-runnable, diff-reviewable.
- **atrium-coordinate** — you are *already running* and told to lead a team; you
  spawn teammates live with `atrium ctl spawn`.
- **atrium-delegate** — you are *already running* and hand off one independent part
  live with `atrium ctl spawn`.

If the person wants a repeatable, reviewable team definition → stay here. If they
want you to fan work out *right now* from inside a running pane → the runtime skills.

## The workflow you run with them

1. **Understand the goal.** What repo/dir? How many agents, doing what? Do the
   agents need to talk to each other (coordination) or just run in parallel? Should
   each work in isolation (worktrees) or share one tree? Hands-off or supervised?
   Ask — don't assume. A fleet is worth getting right because it's re-run.
   Ask what the agents will **build or test**, and the machine's cores and RAM.
   That decides how much the fleet may compile at once (below), and it is not
   optional: design the roster to fit the machine.
2. **Propose a roster** in prose first: one line per agent (name, model, what it
   owns, whether it gets a worktree, **which build/test commands it may run**).
   Get a nod before writing JSON.
3. **Start from a template, then edit.** `atrium fleet init crew` (or `solo`,
   `pair`, or one of the user's own from the global `fleet.json` — `atrium fleet
   ls --templates` is the menu) writes `./atrium.fleet.json` with the roster,
   prompts and kickoffs that encode the short-session loop below; `--agents N`
   scales the builders. It never overwrites. Then write the instruction layers
   (schema below): the committed `CLAUDE.md` with the fleet's shared rules and
   resource budget, and edit the per-agent `prompt` and kickoff to the project.
   Prefer explicit `model`/`worktree` keys over stuffing flags into `cmd`.
4. **Explain the launch line and the settings you chose**, then let them run it.
5. **Iterate.** Fleet files are cheap to edit and re-run; tune and relaunch.

## Where the file lives (discovery)

atrium looks for the fleet file **cwd-first, then user-global** (fallback, not
merge):

1. `./atrium.fleet.json` in the directory you launch atrium from — this is the
   normal, project-local home (check it into the repo so the team is shared).
2. else the user-global file: `%APPDATA%\atrium\fleet.json` (Windows) or
   `~/.config/atrium/fleet.json` (elsewhere).

A cwd file **shadows** the global one — if both exist, only the cwd file is read.
So a per-project fleet must live in that project's `atrium.fleet.json`. `atrium
fleet ls` lists the fleets in whichever file was found. The global file is also
the user's **template library**: `atrium fleet init <name>` copies a fleet from
it into the project as written, and a fleet there named like a built-in wins
over the built-in. `ATRIUM_FLEET` names the global file's full path for anyone
who keeps it elsewhere.

Beside it lives the user-global **`config.json`** (`atrium config path` shows
where): `claude_aliases` (a second account's shim such as `claude2`, with the
`config_dir` it runs under, so its panes bind and recover), session-wide `deny`,
`ctl_allow`, `trust_allow`, `build_jobs`, `memory_mb`, and `fleet_defaults`
(`trust`, `identity`, `allow_ctl`, `grid` for fleets that leave them out).
Precedence, lowest to highest: config.json, the fleet's key, the `ATRIUM_*`
variable, a flag. If a fleet's command is not plain `claude`, name it there or
the pane gets none of atrium's launch rules.

## Schema

One file holds a `fleets` map of `name → fleet`. Unknown keys are ignored (forward-
compatible), and an empty `agents` list is rejected.

### Fleet-level keys

| key | type | meaning |
|-----|------|---------|
| `agents` | array (required) | the roster, one pane each, in order |
| `grid` | `"RxC"` | tile layout, e.g. `"2x3"`; omit → auto-balanced from agent count |
| `identity` | string | default credential for agents that don't name their own |
| `trust` | `"plan"`\|`"accept"`\|`"automode"`\|`"skip"` | fleet trust posture when the CLI doesn't pass `--trust`. A **request**, capped by the session ceiling |
| `allow_ctl` | bool | bring up the board/bus control plane (same as `--allow-ctl`). **Required for any fleet whose agents coordinate** — without it, kickoffs that publish to a bus silently do nothing |
| `topics` | array of strings | declare the canonical bus vocabulary → bus runs **strict** (a topic outside the list is rejected, so the team converges on one set). Omit → soft-gated |
| `worktrees` | bool | shorthand: every agent gets its **own** worktree named after itself (full fan-out) |
| `worktree_base` | string | where worktree dirs go; omit → sibling `../.atrium-worktrees/<fleet>/`. Set it to give two concurrent sessions on one repo distinct bases |
| `worktree_seed` | array of strings | untracked files/dirs to link into each fresh worktree (git only checks out tracked files, so a needed `.env` would be missing). Hardlinks files / junctions dirs; never seeds build caches |
| `build_jobs` | number | total compiler jobs **all** agents' cargo builds share (the session compile pool). Default one per core, bounded by RAM; `0` = off. See [Compiling is the scarce resource](#compiling-is-the-scarce-resource-required-for-any-fleet-that-builds) |
| `memory_mb` | number | fixed ceiling on the memory everything the agents run may use. Default dynamic; `0` = off. Hard on Windows and on Linux under `systemd-run --user --scope -p Delegate=yes`; soft otherwise |
| `deny` | array of strings | commands **no** claude agent in the session may run, including workers spawned later: claude rules (`"Bash(git push --force*)"`) or command prefixes (`"cargo test --workspace"`) |
| `context` | object | shared context store: `{"provider": "context-mode", "share": "knowledge"}` (`share`: `knowledge` = shared index, private session memory; `full`; `none`). Stored per fleet under `.atrium/ctx/<fleet>/`, and kept by a pane across `ctl respawn` |

### Agent-level keys

| key | type | meaning |
|-----|------|---------|
| `name` | string (required) | label / role handle |
| `cmd` | array (required) | the command + args, e.g. `["claude"]` or `["codex"]` |
| `kickoff` | string | trailing positional prompt → the agent's **first user message**, so it starts working the instant the fleet comes up. Omit → it idles |
| `model` | string | `--model <m>` (`opus`\|`sonnet`\|`haiku`\|…) |
| `effort` | string | `--effort <e>` |
| `worktree` | string | **group name** for tree isolation: a unique value = solo worktree+branch; a value **shared** with other agents = they co-develop one worktree; absent = stays in the main tree |
| `trust` | policy | per-agent trust override (see mixed-model note below), capped by the session ceiling |
| `cwd` | string | working dir (so its `CLAUDE.md` auto-loads); resolved relative to the fleet file |
| `add_dirs` | array | extra readable dirs → `--add-dir` |
| `prompt` | string | system-prompt suffix → `--append-system-prompt` (behavior, not permissions). The home for the agent's **role, owned files and limits** (see instruction layers). atrium folds it into one block with its own worktree/ctl text; **atrium 0.32.0 and earlier dropped it** whenever `allow_ctl` or a worktree was set, so on those versions put the role in the kickoff |
| `identity` | string | per-agent credential, overrides the fleet default |
| `can_spawn` | bool | may this agent create its own teammates via `atrium ctl spawn`? Defaults **false** for a fleet agent |
| `deny` | array of strings | commands **this** agent may not run, on top of the fleet's `deny`. The way to stop a worker running workspace-wide builds (claude agents only) |

Launch arg order per agent: `cmd… [--add-dir …] [--append-system-prompt prompt]
[--model M] [--effort E] "<kickoff>"`. You can equivalently inline `--model` in
`cmd`, but the `model` key reads cleaner in a diff.

### Minimal fleet

```json
{
  "fleets": {
    "review": {
      "grid": "1x2",
      "agents": [
        { "name": "security", "cmd": ["claude", "--model", "opus"],
          "kickoff": "Read PLAN.md. Audit src/auth/ for authz bugs and secret handling. Write findings to REVIEW-security.md. Do not edit source." },
        { "name": "perf", "cmd": ["claude", "--model", "sonnet"],
          "kickoff": "Read PLAN.md. Profile the hot path in src/render/. Write findings to REVIEW-perf.md. Do not edit source." }
      ]
    }
  }
}
```

### Coordinated, isolated build fleet (the powerful shape)

```json
{
  "fleets": {
    "build": {
      "grid": "2x3",
      "allow_ctl": true,
      "topics": ["build", "review"],
      "build_jobs": 8,
      "memory_mb": 32768,
      "deny": ["cargo build --release", "cargo bench"],
      "agents": [
        { "name": "lead", "cmd": ["claude", "--model", "opus"],
          "kickoff": "You lead. Read PLAN.md, then `atrium ctl board get lead`, `board list` and `bus feed`, and continue from there. Assign via board/bus, don't code. `atrium ctl bus sub build review`." },
        { "name": "api", "cmd": ["claude", "--model", "sonnet"], "worktree": "api",
          "deny": ["cargo test --workspace", "cargo build --workspace", "python dev.py check"],
          "prompt": "You own crates/api. Build and test only that crate: cargo test -p api.",
          "kickoff": "You own crates/api in your own worktree (already placed — don't cd, commit on your branch). Implement per PLAN.md, `cargo test -p api` green, commit, then `atrium ctl bus pub build --to lead msg=api done`." },
        { "name": "store", "cmd": ["claude", "--model", "sonnet"], "worktree": "store",
          "deny": ["cargo test --workspace", "cargo build --workspace", "python dev.py check"],
          "prompt": "You own crates/store. Build and test only that crate: cargo test -p store.",
          "kickoff": "You own crates/store in your own worktree. … `atrium ctl bus pub build --to lead msg=store done`." },
        { "name": "reviewer", "cmd": ["claude", "--model", "opus"],
          "kickoff": "Adversarial reviewer in the main tree. Verify each branch against real behavior; block the integrator on the bus until issues are fixed. Don't rubber-stamp." },
        { "name": "integrator", "cmd": ["claude", "--model", "sonnet"],
          "kickoff": "Main tree. Wait until api+store report done and reviewer is clear, then merge their branches one at a time, run the full gate (`python dev.py check`) green after each, commit. You are the only agent that runs the full gate. Don't reinstall." }
      ]
    }
  }
}
```

## Launch & manage

```
atrium fleet init <template> [--agents N]          # start ./atrium.fleet.json from solo | pair | crew | yours
atrium fleet ls --templates                         # the built-ins and the user's own templates
atrium fleet up <name> [--allow-ctl] [--trust <policy>] [--max-depth N]
atrium --trust automode [--allow-ctl] up <name>     # flags-first form (same thing)
atrium fleet ls                                     # list fleets in the found file
atrium fleet clean <name>                           # reclaim worktrees a run kept
```

**Flags go BEFORE `up` in the short form** (`atrium --trust automode up build`). If
you write `atrium up build --trust automode`, atrium parses `up` as a program and
the fleet never launches through the fleet path — the classic silent miss. When in
doubt use the explicit `atrium fleet up build --trust automode`. If the fleet file
already sets `trust`/`allow_ctl`, you don't need the flags at all.

## Trust & hands-off (read this before setting `automode`)

The trust posture is what makes a fleet run without a human clicking "allow":

- `plan` = read-only, `accept` = auto-accept **edits** + a safe command allowlist,
  `automode` = claude's own auto mode (edits **and** commands, no allowlist),
  `skip` = full bypass. Bare `--trust` = `accept`; `--skip-permissions` = `skip`.
- Every fleet/agent trust value is a **request capped by the session ceiling**. An
  agent can de-escalate (run `accept` under an `automode` session) but never
  escalate past what the human approved at launch.
- **`automode` is model-gated by Anthropic.** haiku reports *"this model does not
  have automode"* — so a fleet launched `--trust automode` leaves its haiku agents
  unable to run commands hands-off, and `automode` carries **no** command allowlist
  anyway. Fix for mixed-model fleets: set `"trust": "accept"` on the non-automode
  agents. `accept` gives `acceptEdits` + the allowlist, which **every** model
  honors. This is the main reason the per-agent `trust` key exists.
- The `accept` allowlist runs common dev commands hands-off (cargo/npm/pytest
  builds+tests, and git `add`/`commit`/`merge`/`worktree`/`status`/`log`/`diff`)
  but **deliberately still prompts** on `git push`/`reset`/`clean`. Don't try to
  widen it with raw claude flags in `cmd` — use `trust`.
- atrium also pre-accepts claude's separate **folder-trust** dialog per pane cwd,
  so a hands-off launch in a fresh dir doesn't stall on "trust this folder?".

Rule of thumb: all-opus/sonnet coordinating fleet → launch `--trust automode`. Any
haiku in the roster → give those agents `"trust": "accept"`.

## Worktrees (per-agent isolation)

Turn this on when agents would otherwise fight over one working tree — two agents
editing files, or one agent's half-done WIP failing another's green gate.

- Set `"worktree": "<name>"` per agent (unique = solo, shared = a squad on one
  concern), or `"worktrees": true` to fan every agent out into its own.
- Each worktree is a real `git worktree` on branch `atrium/<fleet>/<slug(name)>`,
  checked out at `<base>/<fleet>/<slug(name)>` (base default = sibling
  `../.atrium-worktrees`).
- **Single-owner-per-file is the invariant that keeps merges clean.** Give each
  worktree a disjoint set of files; tell each agent in its kickoff to touch only
  its files and to STOP + post on the bus if it needs another's. Then the final
  merge is conflict-free by construction.
- **Teardown is safe by default.** On fleet exit atrium reclaims worktrees that are
  **clean and merged**, and **keeps** any that are dirty or unmerged (so nothing is
  lost). `atrium fleet clean <name>` reclaims kept-but-now-merged ones later.
- Kickoff norms for worktree agents: *your cwd already IS the worktree — don't cd,
  run no `git worktree` commands, commit on your current branch.*
- **Pitfall — relative sibling path deps.** If the repo's build pulls sibling
  crates/packages by **relative** path (Rust `path = "../abus"`, or a JS monorepo
  with `../pkg` links), those don't resolve from inside a worktree (it sits at a
  different depth), so the build breaks there even though it's fine in the main
  tree. Either seed/junction the siblings beside the worktrees, or keep those
  agents in the main tree. Check the build's path deps before fanning out a
  multi-crate repo. (A standalone single-crate repo has no such issue.)

## Coordination wiring

A fleet whose agents talk needs three things, or it silently no-ops:

1. `"allow_ctl": true` — brings up the board/bus. Without it, every `atrium ctl
   bus/board` call in a kickoff fails quietly.
2. **Self-contained kickoffs.** Each agent starts **blank** — it can't see your
   plan or the others. The kickoff must carry the goal, exact file paths,
   constraints, the done-signal, and the bus/board commands to use. Write a
   paragraph, not "do the api part". Commit a `PLAN.md` if agents share context —
   but note **worktree** agents only see files committed at HEAD, so either commit
   the plan or inline it into each kickoff.
3. Optional `"topics": [...]` to lock the bus vocabulary so the team doesn't
   fragment into `review`/`reviews`/`review-gate` — and so `atrium ctl bus
   topics` tells every agent what to subscribe to.
4. **Subscriptions in every kickoff.** A publish is typed into the panes
   subscribed to its topic and the panes it names with `--to`, once each is
   idle; it reaches nobody else until they run `bus feed`. So each kickoff
   starts with `atrium ctl bus sub <topic>` for the topics that role must react
   to, and each "done" post goes `--to lead` (or whoever must act). Do not
   subscribe every agent to everything: every post then wakes every idle
   teammate, a turn each. Role names are unique while a pane lives.

The runtime coordination commands (what your kickoffs tell agents to run):
`atrium ctl board set/get/list`, `atrium ctl bus pub/sub/feed/resolve/topics`.
See the atrium-coordinate skill for the full board/bus playbook.

## Long runs: short sessions

A roster agent lives as long as the fleet, and so does everything in its context.
For a run with more than a few items, keep the roster to the roles that must
last, and let the lead spawn the rest one item at a time:

- **Roster:** the lead (with `"can_spawn": true`), plus an integrator if branches
  need merging. The lead spawns a fresh builder per item and a fresh reviewer per
  item, and reaps each one at its checkpoint. atrium-coordinate has the sizing
  table, the checkpoint and the review loop to put in the lead's instructions.
- **The lead's kickoff is its restart point.** It runs again whenever the lead
  is respawned, so write it to resume: read `PLAN.md`, `board get lead`,
  `board list`, `bus feed`, and continue. A kickoff that says "start the plan"
  starts the plan over.
- **Give the fleet a `context` block** so the lead can push long output through
  context-mode, and so its knowledge base survives a restart.
- **Commit `PLAN.md`** before `fleet up`. Every fresh session, and every worktree,
  starts from it.

## Compiling is the scarce resource (required for any fleet that builds)

**Design the fleet to fit the machine it runs on, not the size of the task.**
Agents are cheap: an idle agent CLI is a few hundred MB. Compiling is not: one
`rustc` job peaks at 1–3 GB, a link step more, and a toolchain starts one job
per core by default. So a fleet's real cost is **how much it compiles at the
same time**, and in any compiled-language fleet (Rust, C/C++, Go, Java/Kotlin,
Swift, large TypeScript type-checks) that is the number you design around. A
fleet that only reads, reviews, writes docs or runs Python scripts can skip
this section.

**What happened without it.** A 10-agent fleet ran on 16 cores and 64 GB RAM.
Seven agents, each in its own worktree, ran `cargo test --workspace` at the same
moment: seven cold build directories, each compiling with 16 jobs, plus a release
build. System memory (RAM plus pagefile) ran out, and `rustc`, a Claude Code hook
and atrium all failed allocations in the same second. The session and every
pane were lost, and other programs on the machine crashed with it.

### The rules

These are design rules for the roster, not suggestions for the agents:

1. **Never design a fleet where several agents compile the whole project at the
   same time.** At most **one** workspace-wide build or test runs across the
   entire fleet at once. Seven agents each running the full gate is the failure
   above, whatever the machine.
2. **Workers compile only what they own.** A worker that owns one crate or
   package builds and tests only that (`cargo test -p <crate>`,
   `go test ./pkg/...`). No `--workspace`, `--release` or benchmark builds unless
   the role exists for that.
3. **One agent owns the full gate.** Normally the integrator runs the
   workspace-wide build and tests, after merging, one branch at a time. If more
   than one agent genuinely needs a full build, they take turns through a board
   lock (below).
4. **Size from the machine.** Ask for cores and RAM before proposing a roster.
   Across the agents that can compile at once:
   - compiler jobs in total ≈ **core count**
   - peak memory per build × builds at once ≤ **about half of RAM** (assume
     2–4 GB per Rust build when unsure)

   If the plan needs more than that, change the plan: fewer builders, a smaller
   set of worktrees, or serialized phases. Don't just raise limits.
5. **Plan the work so compiling is serialized.** Order phases so builders
   finish and hand off rather than all building at the end: workers check their
   own crate, report done, and the integrator merges and gates in sequence.
6. **Fewer worktrees than agents.** Read-only reviewers and leads don't need
   one, and every worktree is another cold build directory to compile from
   scratch.

### What atrium enforces (atrium 0.33.0 and newer)

Rules the agents are merely told get ignored under pressure. atrium now backs
the rules above with limits the agents can't talk their way past. **Set these
keys explicitly in any fleet that compiles**, so the budget is in the reviewed
file and on the banner rather than left to defaults.

| key | what atrium does | limit to know |
|-----|------------------|---------------|
| `build_jobs` | One **shared compile pool** for the whole session. Every pane gets `CARGO_MAKEFLAGS` pointing at it, so all agents' cargo builds together run at most `build_jobs` compiler jobs (plus one per running cargo). An agent's `-j` or `CARGO_BUILD_JOBS` can't get past it. Default: one per core, bounded by RAM. `0` = off. | **cargo only.** `make`, `ninja`, `cmake`, `go`, `gradle` and `tsc` ignore it; cap those with environment variables (below). |
| `memory_mb` | A **memory guard**: a ceiling on the memory of everything the panes run, with atrium itself outside it. The default is dynamic and tracks the machine's free memory. When it's needed, it stops the largest build (never an agent), waits 30 s to see the effect, and posts it to the bus. **Hard** on Windows (a job memory limit) and on Linux when atrium is launched under `systemd-run --user --scope -p Delegate=yes` (cgroup `memory.max`, swap closed); **soft** on plain Linux terminals and macOS, where atrium watches and stops builds but can't block an allocation between checks. | The banner says `(soft)` when it's soft — on Linux fleets that compile, launch under `systemd-run` to get the hard cap. It is a backstop, not a budget: a fleet that routinely hits it is designed wrong. |
| `deny` (fleet and agent) | A **deny list** passed to claude as `--disallowedTools`. Fleet-level rules bind every claude pane in the session, including workers spawned later; agent-level rules bind that agent. Every claude pane also refuses commands naming `CARGO_MAKEFLAGS`, so no agent can strip the pool. | Claude agents only. It matches command **text**, so it catches an agent, not a determined workaround. |

**Use `deny` to enforce rules 1–3.** Give each worker the commands its role
must not run, and leave the integrator free to run the gate:

```json
{ "name": "api", "cmd": ["claude"], "worktree": "api",
  "deny": ["cargo test --workspace", "cargo build --workspace",
           "cargo build --release", "cargo bench", "python dev.py check"],
  "prompt": "You own crates/api. Build and test only that: cargo test -p api." },
{ "name": "integrator", "cmd": ["claude"],
  "prompt": "You run the full gate, after merging, one branch at a time." }
```

A denied command comes back to the agent as refused, so the agent learns the
rule the moment it tries to break it, instead of the machine finding out.

**For toolchains the pool doesn't cover**, cap parallelism in the environment
*before* `atrium fleet up` (every pane inherits atrium's environment), or in a
committed config:

- `MAKEFLAGS=-j2`, `CMAKE_BUILD_PARALLEL_LEVEL=2`, `GOFLAGS=-p=2`, Gradle
  `org.gradle.workers.max=2`.
- A Rust `.cargo/config.toml` `[build] jobs = 2` still works as a per-build cap
  under the pool. Commit it: worktrees live outside the repo and only see
  tracked files.

**Serialize full builds with a board lock** when more than one agent needs them:
`atrium ctl board claim build-lock` before, `atrium ctl board release build-lock`
after. A denied claim names the holder, so the agent waits and retries. The
lease lasts 5 minutes and re-claiming renews it, so a long build must re-claim.

**Check the banner before pressing Enter.** The posture line states the budget,
for the build fleet above: `… ctl on, 8 compile jobs shared, memory capped at
32.0 GiB, 4 deny rules`. The deny count is the session-wide rules (two built-in,
plus the fleet's `deny` and `ATRIUM_DENY`); per-agent rules aren't in it. `build pool
OFF` or `memory guard OFF` there on a fleet that compiles means the file needs
fixing first.

**Read the `PREFLIGHT` block (atrium 0.34.0 and newer).** Every warning is
gathered into one bold yellow block right before the verdict (plain
`atrium fleet: warning:` lines when piped). None of them stop the launch, so
fix the file rather than pressing Enter past them. The warnings cover:
- governed flags removed from a `cmd`
- `deny` rules that can't bind
- the build pool off
- ctl with no spawner
- worktrees outside a git repo
- a roster at or over the host pane cap

## Instruction layers: CLAUDE.md, `prompt`, kickoff

Each agent starts blank. Give it three layers, each with a distinct job. Don't
pile everything into a giant kickoff.

| layer | who sees it | put here |
|-------|-------------|----------|
| **Committed `CLAUDE.md`** (repo root) | every agent: main tree, and worktrees, which check out HEAD | the fleet's shared rules: resource budget and allowed commands, the gate command, file-ownership rule, bus/board protocol, forbidden actions, when to stop and ask |
| **Per-agent `prompt`** | that agent only, as part of its system prompt, for the whole session | its role, the exact files/dirs it owns, its own command limits (e.g. "you may run `cargo test -p rat-core` only"), who it reports to |
| **`kickoff`** | that agent's first user message | the first task: short, pointing at `PLAN.md` / `SPEC.md` sections rather than pasting them |

Optionally, a `CLAUDE.md` inside an owned subtree (e.g. `crates/core/CLAUDE.md`)
holds notes specific to that code. Claude Code loads it when the agent works in
that directory.

**Commit `CLAUDE.md` and any `PLAN.md` before `fleet up`.** Worktree agents only
see what is committed at HEAD. Uncommitted rules silently don't exist for them.

A fleet section for the root `CLAUDE.md`, to adapt:

```markdown
## Fleet rules (every agent)

- **Stay inside your files.** Edit only the files your role owns. If you need
  a change elsewhere, stop and post it on the bus; don't make it yourself.
- **Build budget.** Run only your own package's build and tests
  (`cargo test -p <your-crate>`). Never `--workspace`, `--release` or benches
  unless your role says so. All builds in this session share one compile pool
  sized to the machine; don't try to raise or bypass it (no `-j`, no
  `CARGO_BUILD_JOBS`, never touch `CARGO_MAKEFLAGS`). A refused command is a
  rule, not an obstacle to route around.
- **Full builds take the lock.** Before any workspace-wide build or test, run
  `atrium ctl board claim build-lock`. If it's denied, wait and retry; don't
  build anyway. Run `atrium ctl board release build-lock` when done.
- **No background build loops.** No watch mode or repeated rebuild loops, and
  never more than one build at a time.
- **Done means green.** Run `<gate command>` and see it pass, commit on your
  branch, then `atrium ctl bus pub <topic> --to lead msg=<what> done`.
- **When blocked, stop.** Post the blocker on the bus and wait; don't guess
  across another agent's files.
```

Write rules as concrete commands an agent can follow, not as goals.
"Use resources carefully" does nothing; "`cargo test -p <crate>` only, never
`--workspace`" is followed.

## Recipes

- **Parallel review** — N read-only agents (`plan` or just "don't edit"), each
  owning a dimension, writing to its own `REVIEW-*.md`. Often no worktrees needed
  (read-only doesn't collide). Grid `1xN`.
- **Build team** — lead (main tree, delegates) + workers (each a worktree, disjoint
  files) + adversarial reviewer (main tree) + integrator (main tree, merges +
  gates). `allow_ctl: true`, `--trust automode`. The shape shown above; for a
  handful of items.
- **Long build run** — roster of lead (`can_spawn: true`, restartable kickoff) +
  integrator, a `context` block, a committed `PLAN.md`. The lead sizes the items,
  then spawns a fresh builder and a fresh reviewer per item and a final reviewer
  at the end. See [Long runs: short sessions](#long-runs-short-sessions).
- **Tool-develops-itself (bootstrap loop)** — when the fleet edits the very tool
  it's running on, the running binary is **locked**: agents develop in worktrees →
  gate green → merge → the human reinstalls (`cargo install --path . --force` or
  equivalent) → restart. There is no hot-reload; changes go live only on reinstall.
  Don't have an agent try to reinstall the binary that's hosting it.

## Gotchas checklist

- Several agents that each run the full build or test at the same time → N
  worktrees × all cores of compilers → the machine runs out of memory and atrium
  dies with every pane. One full gate at a time; workers build only their own
  package, enforced with per-agent `deny` (see Compiling is the scarce resource).
- Compiled-language fleet without explicit `build_jobs` / `memory_mb` / `deny` →
  the budget is left to defaults instead of the reviewed file. Set them.
- Non-cargo builds (`make`, `ninja`, `go`, `gradle`, `tsc`) → the compile pool
  doesn't cover them; cap them in the environment before `fleet up`.
- Rust job cap in an *untracked* `.cargo/config.toml` → worktrees outside the repo
  never see it; commit it.
- `deny` on a codex agent → not enforced (claude only); the banner warns.
- `--dangerously-skip-permissions`, `--permission-mode` or `--allowedTools` in a
  `cmd` → removed at launch, and a preflight warning says so (on atrium ≤ 0.33.0
  the banner claimed this but the flag still reached the agent). Set the posture
  with `trust`.
- More agents than the host pane cap → a preflight warning, not a refusal; the
  cap still stops mid-run `ctl spawn` (raise it with `ATRIUM_MAX_PANES`).
- Shared rules in an uncommitted `CLAUDE.md` → worktree agents never see them.
- Role put only in `prompt` on atrium ≤ 0.32.0 with `allow_ctl` or worktrees →
  silently dropped; put it in the kickoff there.

- Coordinating fleet but no `allow_ctl` → bus/board silently dead.
- Lead kickoff written as "begin the plan" → a respawned lead starts over; write
  it to resume from `PLAN.md` and the board.
- Lead spawning per-item teammates without `"can_spawn": true` → every spawn is
  refused (a fleet agent defaults to false).
- `--trust automode` with a haiku agent → that agent can't run commands; add
  `"trust": "accept"` to it.
- Flags after `up` → fleet doesn't launch through the fleet path.
- Worktree agents can't see uncommitted/untracked files (they're checked out from
  HEAD) → commit shared context or inline it.
- Multi-crate repo with relative sibling path deps + worktrees → worktree builds
  break; seed the siblings or stay in the main tree.
- cwd `atrium.fleet.json` shadows the global file → edit the right one.
- Two overlapping-file agents without disjoint ownership → merge conflicts; enforce
  single-owner-per-file.

## Discovery

`atrium fleet --help` (up, init, ls, clean), `atrium fleet ls --templates`,
`atrium config --help`, and `atrium ctl --help` (or `atrium --help`) for the
authoritative, current command surface — every family answers `--help` from
anywhere. When unsure of a flag or key, check those rather than
guessing — the surface is the source of truth.
