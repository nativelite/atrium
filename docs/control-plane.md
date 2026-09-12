# The control plane (ctl)

A [fleet](fleets.md) is a *saved* org chart; **`ctl`** builds one **live**, and lets an agent (or you) *drive* it. With `--allow-ctl`, a pane can **spawn**, **send** to, **observe**, and **kill** other agent panes, turning atrium from a viewer of agents into a **runtime** for them.

## What ctl is

The point of doing this inside atrium is legibility. Every agent ctl creates is a visible, killable pane with a status border, listed in the org chart. Because the whole hierarchy is on screen and controllable, atrium coordination stays legible in a way opaque subagents are not.

The shape it is built for is one human, a couple of coordinators, and a wide bench of workers:

```
  you (a shell/agent pane)
   └─ hand an initiative to →
      lead_a, lead_b          (coordinators)
        └─ each splits it across →
           dev_1, dev_2, …    (workers, each its own pane)
```

Trust posture (how hands-off spawned agents run, and the ceiling they are capped at) is set with `--trust` and is covered separately; see [trust and security](trust-and-security.md).

## Turning it on

The control plane is **opt-in and off by default**. Without `--allow-ctl` the channel is not bound and `atrium ctl` refuses; a session that did not ask for it is unchanged.

```bash
atrium --allow-ctl claude                 # bind the control channel
atrium --allow-ctl --trust claude         # …and launch every agent trusted + hands-off
atrium --allow-ctl --max-depth 3 claude   # cap spawn recursion (default 6; 0 = unlimited)
```

`--max-depth <N>` bounds spawn recursion so a self-spawning agent cannot fork-bomb the machine. The default is `6`; `0` removes the cap. Width is never capped, only recursion depth.

When ctl is on, atrium opens a **per-instance control channel** (a **named pipe** on Windows, a **unix socket** elsewhere; zero-dep, non-blocking-drained in the run loop) and injects two env vars into **every** pane it spawns:

- **`ATRIUM_CTL`**: the channel endpoint. `atrium ctl` connects here.
- **`ATRIUM_PANE`**: the caller pane's agent id, so the server attributes each request to its place in the spawn tree.

Any process in a pane (a shell command, or an agent via a tool or hook) issues control by running **`atrium ctl <cmd>`**, which speaks one JSON request per line and prints the one-line JSON reply.

## The command surface

Targets are a pane **id** (its numeric agent id) or a **role** label (`dev_1`); an ambiguous role is a clear error.

| Command | Does | Reply |
| --- | --- | --- |
| `atrium ctl spawn [--role R] [--identity X] [--here\|--window] [--mode <policy>] -- <cmd…>` | Open a visible worker running `<cmd>` (a new window, or `--here` tiled beside the caller), tagged `R`, optionally under credential identity `X`. `--mode <plan\|accept\|automode\|skip>` sets that worker's trust posture, capped at the session ceiling (see [trust](trust-and-security.md)). | `{"ok":true,"pane":3,"role":"dev_1","session":"…"}` |
| `atrium ctl list` | The live org chart: every pane with parent, role, depth, and `agsess` status. | `{"ok":true,"tree":[{"id":0,"parent":null,"role":null,"title":"claude","depth":0,"status":"idle"},…]}` |
| `atrium ctl send <target> <text…>` | Deliver `<text>` to a pane's agent as a submitted prompt. **Queued until the target is idle** (agsess-gated), so it never lands mid-turn. | `{"ok":true,"target":3,"queued":true}` |
| `atrium ctl status [<target>]` | One target's status, or (no target) a roll-up of the caller's subtree. Backed by `agsess`: `working` / `waiting-approval` / `waiting-prompt` / `idle` (or `null` if unbound). | `{"ok":true,"pane":3,"status":"working"}` |
| `atrium ctl kill <target>` | Terminate a worker **and its whole subtree** (a lead's kill reaps its ICs). The dead panes' windows re-tile or close on the next tick. | `{"ok":true,"killed":[3,4,5]}` |
| `atrium ctl respawn <target> [--worktree <name>]` | Relaunch a pane's process **in place**: kill the old child and swap in a fresh one, keeping the pane's role, parent, and depth. With `--worktree <name>` the new process starts in that (idempotently created) git worktree; otherwise it inherits atrium's own cwd. | `{"ok":true,"pane":3,"role":"dev_1","session":"…"}` |
| `atrium ctl audit [N]` | The control-request log (most recent `N`, or all), for reconstructing a run. | `{"ok":true,"audit":[{"seq":1,"caller":0,"action":"spawn","detail":"role=dev_1 argv=claude identity=-","ok":true,"note":"pane=1"},…]}` |
| `atrium ctl board set <key> <field=value…>` | Merge fields into the shared board (create if absent); an empty value clears a field. | `{"ok":true,"key":"auth","entry":{"by":"dev_1","ms":…,"fields":{"status":"DONE","owner":"Max"}}}` |
| `atrium ctl board get <key>` / `list` / `del <key>` | Read one entry, roll up the whole board, or remove an entry. | `{"ok":true,"board":[{"key":"auth","by":"dev_1","fields":{"status":"DONE"}},…]}` |
| `atrium ctl board claim <key> [--ttl <secs>]` / `release <key>` | Atomically claim a key as a **lease** (owner is the caller, derived server-side; default TTL unless `--ttl`), or release it. A claim is granted only if the key is free or its lease expired; otherwise the reply names the current holder and how long its lease runs. | `{"ok":true,"key":"auth","granted":true,"holder":"dev_1","lease_ms":…,"entry":{…}}` |
| `atrium ctl bus pub <topic> [--decision] <field=value…>` | Publish a structured event to `topic`. Default urgency `fyi`; `--decision` marks an escalation that needs a human answer. | `{"ok":true,"event":{"seq":7,"topic":"deploy","kind":"fyi","from":"dev_1","fields":{"msg":"merged"}}}` |
| `atrium ctl bus sub <topic…>` / `unsub [<topic…>]` | Subscribe (merged; `*` = firehose) or unsubscribe (empty ⇒ all) the caller. | `{"ok":true,"subscribed":["deploy","*"]}` |
| `atrium ctl bus feed [--since <seq>]` | Pull the caller's subscribed events with `seq > since` (no echo of your own); returns a `cursor` to resume from. | `{"ok":true,"feed":[{"seq":7,"topic":"deploy",…}],"cursor":7}` |
| `atrium ctl bus resolve <seq>` | Mark a `decision_needed` event answered (clears the bar/panel escalation). | `{"ok":true,"seq":7,"resolved":true}` |
| `atrium ctl bus topics` | List the active topics with how many panes subscribe to each — discoverability before you `sub`. | `{"ok":true,"topics":[{"topic":"deploy","subs":2},…]}` |

## The board: shared source of truth

`send` is the ephemeral message stream ("I just shipped auth"); the **board** is the durable current truth (`auth: DONE, owner: Max, url: …`). It answers *"what is currently true?"*.

It is a schemaless **`key → fields`** tracker living in the atrium daemon. Because atrium is the single broker process every pane talks to, it is a plain map behind the pipe: **single writer, no locking, no consensus.** A coordinating lead sets tasks and reads `board list` for status/owner/blocker instead of re-scraping each teammate's transcript; teammates update their own entry as they work.

The board is part of the ctl surface, so it is gated by `--allow-ctl`. Set **`ATRIUM_BOARD=<file>`** to persist it across restarts (in-memory otherwise). Every write records who made it (in the `audit` log and the entry's `by`), but the board is shared by the whole session; there are no per-teammate walls.

On top of the plain map, `board claim` / `release` give the team a **lease**: a coordinator (or a worker grabbing a unit of work) claims a key, and a claim succeeds only when the key is free or its previous lease has expired — so two panes cannot both believe they own the same task. The lease has a default TTL (overridable with `--ttl`), so a claimant that dies never holds the key forever.

## The bus: the team's event stream

If the board answers *"what is currently true?"*, the **bus** answers *"what just happened, and does anyone need to act?"*. A teammate **publishes a structured event to a topic** (`bus pub deploy msg=shipping url=…`); teammates **pull** the topics they **subscribe** to (`bus sub deploy` then `bus feed`). State-only coordination means polling the board; the bus lets a finished worker *notify* without everyone re-checking.

Two urgency classes carry the visibility-vs-approval split:

- **`fyi`**: the default; cheap, lands on the feed.
- **`decision_needed`**: set with `--decision`; an escalation that needs a human or lead answer.

Open decisions surface **actively**: the `Ctrl+A b` panel lists them first and the status bar shows `N decisions need you`, so you do not have to be looking. `bus resolve <seq>` clears one once answered.

**Backpressure is designed in, not bolted on**, because unread messages cost attention:

- **No echo** of your own events.
- **Pull, not push**: you only pay for topics you subscribed to.
- **Per-agent rate cap** of 20 events / 10 s, so one looping worker cannot storm the team.
- **Bounded ring** of the oldest 512 events, then they fall off.

Like the board, the bus lives in the single broker process (a plain in-memory log, no locking), is gated by `--allow-ctl`, records who sent each event, and persists across restarts with **`ATRIUM_BUS=<file>`**.

## A worked run: initiative to leads to ICs

```bash
# You launch the control plane, trusted so the fleet runs hands-off. With
# --trust, every ctl-spawned agent also comes up trusted (no per-action prompts),
# so a lead can drive its ICs without you clicking through dialogs.
atrium --allow-ctl --trust claude

# From your pane, hand the initiative to two coordinators:
atrium ctl spawn --role lead_a -- claude
atrium ctl spawn --role lead_b -- claude
atrium ctl send lead_a "Own the parser rewrite. Split it across two ICs, TDD."
atrium ctl send lead_b "Own the docs refresh. One IC is enough."

# lead_a, from its own pane, builds its team beside it (--here) and tasks them:
atrium ctl spawn --here --role dev_1 -- claude
atrium ctl spawn --here --role dev_2 -- claude
atrium ctl send dev_1 "Rewrite the tokenizer, tests first."
atrium ctl send dev_2 "Rewrite the AST builder, tests first."

# lead_a coordinates by status, not by scraping terminals:
atrium ctl status            # roll-up of lead_a's own subtree
atrium ctl status dev_1      # one IC (lead_b's team is out of scope, refused)

# When an IC is done, its lead reaps it (and anything under it):
atrium ctl kill dev_1

# You see all of it, any time: the org chart and the full trail:
atrium ctl list
atrium ctl audit
```

The multiplier: many workers, coordinated by a few of them, driven by one human, and every level is a pane you can watch, redirect, zoom into (`Ctrl+A z`), or kill. `ctl` is the small verb layer; the panes, tiling, identity injection, and `agsess` status it stands on already existed.

## Delegation skills: atrium-delegate and atrium-coordinate

`ctl` is the mechanism; two Claude Code skills (in `skills/`) are the instruction layer that makes an agent *use* it. Copy the directories into `~/.claude/skills/` and any atrium-hosted claude picks them up. They exist because the trigger is **who decides to delegate**:

- **`atrium-delegate`** (ambient / discretionary): the agent *may* hand off an independent, substantial part when it genuinely pays, and does small or dependent work inline. On its own a capable agent usually judges it can do the work itself (which is correct), so this fires rarely by design. Its real value is discoverability: without it, an agent does not know `atrium ctl` exists at all.
- **`atrium-coordinate`** (directed): for when you *want* fan-out. It casts the agent as a coordinator that splits the work, delegates **every** part, monitors, collects, and reaps, instead of implementing inline. This is the reliable lever: on a task where the ambient skill chose inline every time, coordinate mode fanned out every time.

Drop a spawned lead straight into coordinate mode by telling it to coordinate:

```bash
atrium ctl spawn --role lead -- claude
atrium ctl send lead "Coordinate this across a team, one teammate per part: <the initiative>"
```

or, from your own pane, just say "coordinate this across a team: …" and let the skill surface.

## See also

- [README](../README.md): atrium overview and the full feature set.
- [trust and security](trust-and-security.md): the `--trust` ladder and the ctl security model.
- [fleets](fleets.md): saved rosters (`atrium fleet up`), the persisted org chart.
- [credential identity](identity.md): per-pane credentials via `--identity` and `akey`.
