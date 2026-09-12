# Fleets and mass-spawn

atrium can open a whole grid of agent panes from one command. Use **mass-spawn** to launch N copies of the *same* command in one window, or a **fleet** to bring up a squad of *different, named* agents, each already in-role. See the [project README](../README.md) for the wider tour.

## Mass-spawn

`-n <N>` and `--grid <R>x<C>` open a grid of agent panes in one window, instead of splitting N times by hand.

```bash
atrium -n 4 claude                   # 2x2: four agents in a square
atrium -n 6 claude                   # 2x3
atrium -n 8 claude                   # 2x4
atrium --grid 2x3 claude             # the same 2x3, shape given explicitly
atrium --identity work -n 4 claude   # four panes, all under the `work` identity
```

### `-n <N>`

`-n` takes a **positive multiple of 2** (2, 4, 6, 8, and so on). An odd or zero N is a clear startup error (`-n must be a positive multiple of 2`), never a silent fall back to one pane.

The shape is balanced automatically: rows are the factor of N closest to `sqrt(N)`, cols are `N / rows`. The grid is therefore never taller than it is wide.

### `--grid <R>x<C>`

`--grid` sets the dimensions yourself. The product must be at least 2.

### What every tile shares

- **Every tile runs the same command, each its own session.** Each agent pane gets its own `--session-id`, so the agent-aware chrome binds each one independently.
- **All under one identity.** If `--identity` is given, every pane runs under it. See [credential identity](identity.md).

Everything after spawn is the ordinary tiled window: `hjkl` and arrows move focus, `z` zooms, `x` kills and re-tiles, and the status/identity chrome plus `q` quit all work exactly as for a hand-split grid.

For a saved roster of named agents, each with its own identity, working dir, and instructions, use a fleet.

## Fleets

Mass-spawn opens N copies of *one* command. A **fleet** brings up a squad of *different, named* agents, each already in-role with its own identity, working directory, extra context dirs, and instructions, from one command.

```bash
atrium fleet up review-crew   # bring up the whole squad, each on its identity
atrium fleet ls               # list the fleet names in the file
```

### Where the fleet file lives

A fleet lives in **`atrium.fleet.json`**, looked for in this order:

1. The **current directory** first. Check it into the repo so a team shares the fleet.
2. A **user-global fallback**: `%APPDATA%\atrium\fleet.json` on Windows, `~/.config/atrium/fleet.json` elsewhere.

The file is read **read-only**. atrium never writes it.

## The JSON format

```json
{
  "fleets": {
    "review-crew": {
      "grid": "2x2",
      "identity": "work",
      "agents": [
        {
          "name": "reviewer",
          "cmd": ["claude"],
          "identity": "wif:prod",
          "cwd": "./review",
          "add_dirs": ["../shared", "./specs"],
          "prompt": "You review PRs for safety.",
          "model": "opus",
          "effort": "high"
        },
        { "name": "builder", "cmd": ["claude"], "cwd": "./app" }
      ]
    }
  }
}
```

### Fleet-level fields

| Field | Required | Meaning |
| --- | --- | --- |
| `grid` | optional | An `RxC` layout that must fit the agent count. Omit it and the window auto-grids from the number of agents. |
| `identity` | optional | A default `akey` identity for every agent. A per-agent `identity` overrides it. |
| `allow_ctl` | optional | Bring [the control plane](control-plane.md) up, as `--allow-ctl` does. |
| `trust` | optional | The trust posture the whole fleet runs at. |
| `context` | optional | Shared knowledge and memory backend for the fleet (see [Shared context](#shared-context)). |
| `topics` | optional | The fleet's canonical bus-topic vocabulary (an array of strings): the topics its agents coordinate on. See [the control plane](control-plane.md). |
| `worktrees` | optional | `true` gives **every** agent its own git worktree and branch (a full fan-out), so parallel workers never clobber one shared tree. See [worktrees](worktrees.md). |
| `worktree_base` | optional | Where those worktrees are created; defaults to a sibling of the repo so they never show up as untracked files inside it. See [worktrees](worktrees.md). |
| `worktree_seed` | optional | Untracked paths (an array of strings) linked from the main tree into each fresh worktree — config or build inputs git does not track. Best-effort (junction/symlink, copy fallback); never fails a launch. See [worktrees](worktrees.md). |

**`identity`** is resolved via `akey` and injected per pane exactly as `--identity` does. The pane shows the `·<name>` tag (the **name** only, never a secret), and a resolve failure is flashed and the pane runs without it, never silently unauthenticated. See [credential identity](identity.md).

**`allow_ctl`** matters because a fleet whose agents coordinate is dead without it, and silently so: the panes come up and publish into a bus that is not there. If the file is the complete definition of a spin-up, this belongs in it.

**`trust`** takes one of `plan`, `accept`, `automode`, or `skip`. A fleet is launched to run hands-off, so it needs one; declaring it here keeps it with the roster it applies to and out of a flag you retype every launch. `--trust` on the command line wins when given. It is a **request, not an override**: if this atrium is itself running inside another atrium, the outer session's policy still caps it. See [trust and security](trust-and-security.md).

### Per-agent fields

Each agent needs a **`name`** and a **`cmd`** (command plus args). Everything else is optional.

| Field | Maps to | Meaning |
| --- | --- | --- |
| `name` | required | The agent's name in the roster. |
| `cmd` | required | Command and args, for example `["claude"]`. |
| `identity` | `--identity` | Overrides the fleet-level `identity` for this agent. |
| `cwd` | spawn directory | The agent is spawned here, so its `CLAUDE.md` auto-loads. Resolved relative to the fleet file's directory; absolute paths are used as-is. |
| `add_dirs` | `claude --add-dir …` | Extra trees the agent may read. |
| `prompt` | `--append-system-prompt` | Extra system-prompt text. |
| `model` | `--model` | The model to run. |
| `effort` | `--effort` | The reasoning effort. |
| `can_spawn` | `ctl spawn` capability | May this agent create teammates? **Defaults to `false`.** |
| `trust` | `--trust` for this pane | Overrides the fleet-level `trust` for this one agent, capped at the fleet/session ceiling (a per-agent posture can de-escalate, never elevate). See [trust and security](trust-and-security.md). |
| `worktree` | worktree group | The worktree group this agent joins. Agents sharing a name co-develop one worktree and branch; distinct names are isolated. See [worktrees](worktrees.md). |
| `kickoff` | first prompt | An opening message submitted to the agent once it is up (distinct from `prompt`, which is appended to its system prompt) — the initiative that starts it working. |

**`can_spawn`** is a capability the roster grants, not something implied by having ctl access. A depth cap bounds how far a fan-out goes, but never says who may start one, so the right to create teammates is declared here.

If a fleet turns the control plane on (`allow_ctl`) and coordinates by spawning teammates, set `"can_spawn": true` on its **lead** — a coordinator without it comes up unable to create the teammates it manages, and the denial only surfaces on its first `ctl spawn`. atrium warns at launch when a ctl-enabled fleet has no spawn-capable agent, but the fix is in the roster.

Each agent still gets its own `--session-id`, so the agent-aware chrome binds each pane independently.

## Designate a lead

A fleet is not a row of agents each doing its own thing next to the others. That is how work gets dropped: no one owns the whole, no part gets reviewed, and a stuck worker just sits. The shape a fleet is *for* is **one lead that coordinates the team and drives it to completion**, a bench of workers, and usually an adversarial reviewer. Give one agent a lead persona and let it split the initiative, assign one part per teammate, watch progress, send each finished part to the reviewer, and mark a part done only once the reviewer clears it.

This works because of how `fleet up` wires the panes: every fleet agent comes up as an **operator** pane (it has no parent in the spawn tree), so any agent may drive any other over the control plane. The lead can `atrium ctl status <teammate>`, `atrium ctl send <teammate> ...`, read the whole `atrium ctl board list`, and ask the reviewer to check any part. atrium hands every fleet agent the *capability*; the `prompt` and `kickoff` decide **who uses it as the lead**. So the lead role is not a field, it is a brief: cast one agent as the coordinator and the others as workers.

Two things have to be in place or the coordination silently does nothing:

- **`allow_ctl: true`**, so the board and bus exist and every pane is told about `atrium ctl`. Without it the panes come up, the kickoffs tell them to publish to a bus that is not there, and nothing happens.
- **The coordination skills** installed, so a hosted claude reaches for `ctl` reliably instead of not knowing it exists. Install them once from the [nativelite marketplace](https://github.com/nativelite/marketplace): `/plugin marketplace add nativelite/marketplace` then `/plugin install atrium@nativelite`.

A leaderless fleet with the skills missing is exactly the setup that comes up looking busy and finishes nothing. See [the control plane](control-plane.md) for the `ctl` surface the lead drives, and a complete worked roster in [`examples/atrium.fleet.json`](https://github.com/nativelite/atrium/tree/main/examples).

## Shared context: indexing and sharing knowledge across the fleet

A fleet working together on a shared codebase or problem benefits from a **shared knowledge repository** — a searchable index of documentation, design decisions, relevant code, or conversation history that every agent can consult without re-reading or re-exploring the same ground.

The shared context model splits work into two roles:

- **Indexing (one agent per fleet)**: A **lead or designated indexer** (usually the lead or a dedicated recon agent) is responsible for populating and maintaining the knowledge base. This agent reads widely, summarizes decisions, and feeds the index.
- **Consuming (all other agents)**: Every other agent in the fleet has read access to the shared context and pulls from it to inform their work, without the cost or latency of exploring from scratch.

This avoids the loss of work and rediscovered decisions that plague leaderless fleets, and scales better than each agent re-exploring the same problem space.

### Configuring a shared context

A fleet declares its shared context backend at the fleet level (not per-agent):

```json
{
  "fleets": {
    "review-crew": {
      "allow_ctl": true,
      "trust": "accept",
      "context": {
        "provider": "context-mode",
        "share": "knowledge"
      },
      "agents": [
        {
          "name": "lead",
          "cmd": ["claude"],
          "can_spawn": true,
          "prompt": "You index shared knowledge…"
        },
        {
          "name": "worker",
          "cmd": ["claude"],
          "prompt": "You consult shared knowledge…"
        }
      ]
    }
  }
}
```

**Field meanings:**

- **`provider`**: The backend that hosts the shared knowledge. `"context-mode"` is the current provider, backed by Claude Code's context-mode MCP server. Reserved for future providers (e.g., a native knowledge system). **This is provider-neutral by design**.
- **`share`**: The sharing mode for this fleet's context. One of: `"knowledge"` (shared knowledge/content index + private per-agent session memory, the default), `"full"` (shared knowledge AND shared session memory across the fleet), or `"none"` (no context provisioning). Fleet isolation is automatic: each fleet gets its own isolated context store under `.atrium/ctx/<fleet-name>/`.

### The indexer role

Designate one agent — usually the **lead** — as the **indexer**. This agent's job is to:

1. **Ingest and summarize**: Read design docs, specs, existing code, and ongoing conversation. Extract the key facts and decisions.
2. **Keep the index current**: As new information arrives (a decision is made, a pattern emerges), feed it into the shared context.
3. **Make it discoverable**: Organize the knowledge so other agents can search and find what they need.

In the fleet file, the indexer is simply the agent who has the prompt and kickoff guiding this behavior:

```json
{
  "name": "lead",
  "cmd": ["claude"],
  "prompt": "You maintain the shared knowledge base. Read widely, summarize decisions and designs, and keep the index fresh. Feed new insights to the shared context with `/ctx-index` or equivalent.",
  "kickoff": "Start by ingesting the project spec and existing docs into the shared context. As we work, keep it current."
}
```

Other agents run with prompts that direct them to **read** the shared context:

```json
{
  "name": "implementer",
  "cmd": ["claude"],
  "prompt": "You read the shared knowledge base to understand the design before you code. Consult `/ctx` to search shared docs before asking questions."
}
```

### What the shared context is NOT

- **Not a replacement for the control plane.** The control plane (`atrium ctl`) is for coordination and task tracking; shared context is for knowledge. A fleet uses both.
- **Not automatic.** An agent does not magically absorb knowledge; the indexer must deliberately feed it.
- **Not mandatory.** A small fleet or a synchronous team that talks often may not need it. Add shared context when the fleet grows or when knowledge is getting lost.
- **Not a sandbox.** Every agent consults the same knowledge base; there is no per-agent filtering. Use agent prompts to guide who contributes.

### Implementation and versioning

The `context` block is part of the frozen v1 contract, backed by the context-mode MCP server. The `provider` and `share` fields are stable. Each fleet automatically gets its own isolated context store under `.atrium/ctx/<fleet-name>/`, managed by the backend — no manual namespace configuration needed.

## The grant disclosure

`add_dirs` becomes `claude --add-dir`, that is, read access. A fleet file is often written by an agent and approved by a human *by running it*. So before `fleet up` blocks for your Enter, atrium resolves every `cwd` and `add_dirs` entry **through symlinks** and names the ones that are not plain subdirectories of the anchor.

For each such entry the disclosure names:

- where it really lands,
- whether it exists yet,
- whether atrium could not resolve it at all,
- whether it is (or contains) a known credential store such as `~/.ssh`.

The identity each agent comes up on is named too. Identical grants across agents are printed once, and the last line before the prompt names the destinations, so a tall roster cannot scroll the point away. The child is then spawned with exactly the resolved paths that were shown.

### The anchor

The **anchor** is what "outside" is measured against. It is the repository the fleet file is checked into, else the file's own directory. For the user-global file the anchor is the directory you ran `atrium` in, since `~/.config/atrium` holds no project.

### Nothing here refuses

A sibling checkout (`"../shared"` above) is a legitimate, documented use. A gate that blocks ordinary work is one people route around invisibly. The disclosure makes the reach *visible*, and the Enter is the approval.

### What the disclosure does not cover

- A symlink **inside** a granted directory. `--add-dir d` grants `d`'s whole subtree, including links out of it.
- Anything a `cmd` does for itself. `["sh","-c","claude --add-dir ~/.ssh"]` is a shell command atrium does not parse; an agent whose `cmd` is not an agent CLI is flagged as unreadable for exactly this reason.
- The `prompt` or `kickoff` text.
- A directory component of a resolved path re-pointed between the ack and the spawn (a TOCTOU repoint).

**atrium is not a sandbox.** At the same uid an agent can read anything atrium can. This mechanism bounds and discloses what atrium *hands over*; it does not confine what a running agent then does. See [trust and security](trust-and-security.md).

## Error handling

**Errors spawn nothing.** There is no partial fleet: if the launch cannot proceed, no pane comes up. The error cases are:

- no file (the message names both locations),
- malformed JSON,
- an unknown fleet name,
- a fleet with zero agents,
- a grid that does not fit the agent count,
- a `cwd` that is not a directory.

Each is a clear startup error. **Unknown fields are ignored**, so the format can grow without breaking older files.
