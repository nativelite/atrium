# atrium
**tmux for agents.** A multi-agent terminal that hosts your own CLIs — a
coding agent, a shell — in switchable, splittable panes with an **agent-aware**
status bar and borders. Built on the nativelite stack (`pty` + `rawterm` +
`ansi` + `vterm` + `agsess`): **zero third-party dependencies**, all the way
down.

```bash
atrium claude        # every pane runs `claude`
atrium               # every pane runs your shell (COMSPEC / $SHELL)
atrium claude --continue        # any command + args
atrium --identity work claude   # run the agent under the `work` credential
atrium -n 4 claude              # open four agent panes at once (a 2x2 grid)
atrium --grid 2x3 claude        # open a 2x3 grid (six panes)
```

```
┌ 1:claude ─────────────────┐┌ 2:claude ? ───────────────┐
│ (focused: full fidelity)  ││ (its agent is waiting on   │
│                           ││  you — border goes yellow) │
└───────────────────────────┘└───────────────────────────┘
 atrium | 1:claude* | 2:claude? | 3:cmd- | 1 waiting | ^A c:win …
```

**Start flags** (atrium's own, before the command): `--identity <name>` /
`-I <name>` runs the agent under a credential identity; `-n <N>` opens **N agent
panes at once** (N a positive multiple of 2) in a balanced grid, and
`--grid <R>x<C>` sets the grid shape explicitly. Both must come *before* the
hosted command, so a later `-n`/`-I` the hosted program takes is never eaten.

**Keys** — `Ctrl+A`, then: `c` new window (runs the launch command) · `!` new
shell pane · `:` **command prompt** (type any shell/program — `wsl`, `pwsh`,
`claude` — to open in a new pane) · `1`-`9` switch window · `n`/`p` cycle · `"`
split stacked · `%` split side-by-side · `h`/`j`/`k`/`l` (or arrows) move focus ·
`z` zoom the focused pane · `m` mouse capture (off by default; on = click-to-focus
+ wheel-scroll) · `b` board+bus dashboard · `x` kill the focused pane · `q` quit ·
`Ctrl+A Ctrl+A` sends a literal `Ctrl+A` through.

The bar marks each window: `*` active, `?` a bound agent is waiting on you,
`+` background activity, `-` idle, `!` exited. A pane whose process ends closes
itself; when the last one ends, atrium exits.

## Windows and splits

atrium has two levels. A **window** is the 0.1 "switchable full-screen" concept —
`Ctrl+A c` makes a new one, `1`-`9`/`n`/`p` move between them. Inside a window,
`Ctrl+A "` / `Ctrl+A %` **split** the focused pane into a tiled layout; `hjkl`
or the arrows move focus between tiles, and `Ctrl+A z` **zooms** the focused
pane to full-screen and back.

- A window with **one pane** (or a **zoomed** pane) renders in **passthrough**:
  the pane's raw VT byte stream goes straight to your real terminal — your
  terminal does the rendering, so colors, cursor art, and TUIs like Claude Code
  look exactly as they would bare. This is the 0.1 path, untouched.
- The moment a window holds **two or more visible panes** it renders **tiled**:
  each pane drives a `vterm` emulator sized to its rect, all panes are
  composited into one master screen, and only the changed cells are written.
  Every pane wears a one-cell box border with its `index:title` in the top edge.
  Zoom is the escape hatch back to perfect fidelity for a TUI the emulator can't
  render pixel-exact (wide/CJK glyphs, sixel, mouse).

## Agent-aware borders (0.3)

When atrium launches an agent pane (a `claude` command), it binds that pane to the
agent's own session and lets the agent's **real status** tint the pane's chrome —
so in a grid of agents you can see, at a glance, *which one is stuck waiting on
you.*

- **Border color** (unfocused, tiled panes): **bright yellow** = the agent is
  **waiting on your approval**; **grey** = waiting for a prompt, or idle;
  **default** = working. The focused pane keeps its normal bright-cyan border —
  a blocked agent in the pane you're already looking at needs no nagging.
- **Title badge**: the pane's top-edge label gains one marker — ` 2:claude ? `
  when waiting on you, ` 2:claude ~ ` while working, nothing when idle.
- **Status bar**: a window whose bound agent is waiting shows `?` next to its
  name, and the bar appends `| N waiting` — so a blocked agent in a
  *backgrounded* window still surfaces, even off-screen.

How the bind works: when atrium spawns an agent pane it mints a fresh session id
and passes `claude --session-id <uuid>` (unless you already supplied a
session-selecting flag like `--resume`/`--continue`/`--session-id`, in which case
the id is yours and atrium leaves it alone). Claude Code writes its transcript as
`<uuid>.jsonl`, so atrium knows the exact file to watch — a direct lookup, no
guessing, even with several agents in one directory. The status itself is
derived by the [`agsess`](https://github.com/nativelite/agsess) crate, which
reads those transcript files **read-only**.

### What atrium can and cannot see

atrium reads the *status* of Claude Code sessions **it launched** — nothing more.
Concretely:

- It reads only the **session transcript files** (`*.jsonl`) that Claude Code
  writes under your own `~/.claude/projects`, on your own disk, and only the ones
  atrium started via `--session-id`. It is **read-only, status only**: the border
  and bar render *whether* an agent is working / waiting / idle, and **never any
  conversation text** — no prompt, no reply, ever leaks from one pane into
  another pane's chrome. No credential file is ever opened; there is no network
  path at all.
- It does **not** see: an agent you started *inside* a shell pane (`$ claude` —
  that pane's title is your shell, so atrium never binds it; a **documented v1
  limitation**), agents running in other tools, sessions you run outside atrium
  entirely (that is what [`agtop`](https://github.com/nativelite/agtop) is for),
  or anything that would require credentials.
- Because a pane atrium did not launch as an agent is never bound, non-agent panes
  (shells, `vim`, `cmd`) never acquire agent chrome and never cost a scan.

The waiting-on-you status is an inference over the transcript tail, **gated by
Claude Code's `permissionMode`**. On a machine running mostly in `auto` mode the
loud yellow border correctly stays quiet — auto mode doesn't block on the human,
so there's nothing to surface; the yellow is reserved for the sessions where an
agent is genuinely blocked. (A future opt-in hook can replace the inference with
a certainty; it is not part of this release.)

## Agent identity (0.4)

`--identity <name>` (short `-I <name>`) launches an agent pane under a chosen
credential identity:

```bash
atrium --identity work claude      # a static key stored in your OS vault
atrium --identity wif:prod claude  # a Workload Identity Federation profile
```

atrium resolves the target's environment through
[`akey`](https://github.com/nativelite/akey) — a key name becomes
`ANTHROPIC_API_KEY`, a `wif:<name>` profile becomes the five documented
federation variables — and injects it into **that one child** via the pty
(`pty::Pty::spawn_with_env`). The credential is set for that agent alone; it
never touches atrium's siblings, and atrium makes no network calls.

- **The identity applies to the initial agent pane and is inherited by every
  split / new pane.** Identity is fixed for a pane's lifetime (a running
  process's env can't change), so to re-identify you respawn the pane — an
  explicit, visible action.
- **Only the *name* is shown.** A `·<name>` tag rides in the pane's top border
  (`2:claude ·work`) and next to the window's entry in the bar. A `wif:` identity
  reads apart from a static key at a glance (`·wif:prod` vs `·work`), which makes
  the credential-precedence footgun — a leftover static key shadowing a WIF
  profile — visible. The tag renders as **colored text** (the identity color as
  the name's foreground, matching the pane border) in both the border and the
  reverse-video bar — never a filled color chip — and the text is always drawn
  (color is a redundant a11y channel, never color-only).
- **Names only, never secrets — a hard guarantee.** The resolved environment is
  re-fetched on every spawn, handed straight to the child, and dropped; it is
  never cached on the pane, never logged, never persisted, and never appears in
  `ATRIUM_DEBUG` traces. A screen-shared grid leaks no key and no token — only the
  labels you typed.
- **Resolve failure is visible, not silent.** If the identity can't be resolved
  (no such target, vault locked) the reason lands in the bar and the agent spawns
  *without* the credential — you are told, rather than running unauthenticated by
  surprise.

Non-agent panes (shells, editors) and no-identity spawns are unaffected — they
use the plain spawn path and carry no tag. New dependency: the org crate `akey`.
Third-party dependencies remain zero.

## Mass-spawn (0.5)

`-n <N>` / `--grid <R>x<C>` open a whole grid of agent panes in **one** window,
instead of splitting N times by hand:

```bash
atrium -n 4 claude       # 2x2 — four agents in a square
atrium -n 6 claude       # 2x3
atrium -n 8 claude       # 2x4
atrium --grid 2x3 claude # the same 2x3, shape given explicitly
atrium --identity work -n 4 claude   # four panes, all under the `work` identity
```

- `-n <N>` takes a **positive multiple of 2** (2, 4, 6, 8, …); an odd or zero N
  is a clear startup error (`-n must be a positive multiple of 2`), never a silent
  fall back to one pane. The shape is balanced automatically — rows = the factor
  of N closest to `sqrt(N)`, cols = `N / rows` — so the grid is never taller than
  it is wide.
- `--grid <R>x<C>` sets the dimensions yourself (product ≥ 2).
- **Every tile runs the same command, each its own session** — each agent pane
  still gets its own `--session-id`, so the agent-aware chrome binds each one
  independently — and **all under the same `--identity`** if one was given.
- Everything after spawn is the ordinary tiled window: `hjkl`/arrows move focus,
  `z` zooms, `x` kills and re-tiles, the status/identity chrome and `q` quit all
  work exactly as for a hand-split grid. For a **saved roster** — named agents
  each with their own identity, working dir, and instructions — see
  `atrium fleet up` below.

## Saved rosters — `atrium fleet up` (0.6)

Mass-spawn opens N copies of *one* command; a **fleet** brings up a squad of
*different, named* agents, each already in-role — its own identity, working
directory, extra context dirs, and instructions — from one command:

```bash
atrium fleet up review-crew   # bring up the whole squad, each on its identity
atrium fleet ls               # list the fleet names in the file
```

A fleet lives in **`atrium.fleet.json`**, looked for in the current directory first
(check it into the repo so a team shares the fleet), then a user-global fallback
(`%APPDATA%\atrium\fleet.json` on Windows, `~/.config/atrium/fleet.json` elsewhere).
The file is read **read-only** — atrium never writes it.

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

- **`grid`** (optional) — an `RxC` layout that must fit the agent count; omit it
  and the window auto-grids from the number of agents.
- **`allow_ctl`** (optional) — bring the control plane up, as `--allow-ctl`
  does. A fleet whose agents coordinate is dead without it, and silently so: the
  panes come up and publish into a bus that is not there. If the file is the
  complete definition of a spin-up, this belongs in it.
- **`trust`** (optional) — the posture the whole fleet runs at (`plan`,
  `accept`, `automode`, `skip`). A fleet is launched to run hands-off, so it needs
  one; declaring it here keeps it with the roster it applies to and out of a flag
  you retype every launch. `--trust` on the command line wins when given. It is a
  **request, not an override**: if this atrium is itself running inside another
  atrium, the outer session's policy still caps it.
- **`identity`** (optional) — a default `akey` identity for every agent; a
  per-agent `identity` overrides it. Resolved via `akey` and injected per pane
  exactly as `--identity` does — the pane shows the `·<name>` tag (the **name**
  only, never a secret), and a resolve failure is flashed and the pane runs
  without it, never silently unauthenticated.
- Each agent needs a **`name`** and a **`cmd`** (command + args). Optional:
  **`cwd`** (spawned there, so its `CLAUDE.md` auto-loads — resolved relative to
  the fleet file's directory, absolute paths as-is), **`add_dirs`**
  (→ `claude --add-dir …`, so it can read other trees), **`prompt`**
  (→ `--append-system-prompt`), **`model`** (→ `--model`), **`effort`**
  (→ `--effort`), and **`can_spawn`** — may this agent create teammates with
  `ctl spawn`? **Defaults to `false`.** Creating teammates is a capability the
  roster grants, not something implied by having ctl access: a depth cap bounds
  how far a fan-out goes, but never says who may start one. Each agent still gets
  its own `--session-id`, so the
  agent-aware chrome binds each pane independently.
- **Errors spawn nothing.** No file (the message names both locations), malformed
  JSON, an unknown fleet name, a fleet with zero agents, a grid that does not fit,
  or a `cwd` that is not a directory — each is a clear startup error, never a
  partial fleet. Unknown fields are ignored, so the format can grow without
  breaking older files.
- **Every directory the roster grants is on the screen you approve.** `add_dirs`
  becomes `claude --add-dir`, i.e. read access, and a fleet file is often written
  by an agent and approved by a human *by running it* — so before `fleet up`
  blocks for your Enter it resolves every `cwd`/`add_dirs` entry **through
  symlinks** and names the ones that are not plain subdirectories of the anchor:
  where each really lands, whether it exists yet, whether atrium could not resolve
  it at all, and whether it is (or contains) a known credential store such as
  `~/.ssh`. The identity each agent comes up on is named too. Identical grants
  across agents are printed once, and the last line before the prompt names the
  destinations, so a tall roster cannot scroll the point away. The child is then
  spawned with exactly the resolved paths that were shown.
  - The **anchor** — what "outside" is measured against — is the repository the
    fleet file is checked into, else the file's own directory; for the
    user-global file it is the directory you ran `atrium` in, since
    `~/.config/atrium` holds no project.
  - **Nothing here refuses.** A sibling checkout (`"../shared"` above) is a
    legitimate, documented use; a gate that blocks ordinary work is one people
    route around invisibly. This makes the reach *visible*, and the Enter is the
    approval.
  - **What it does not cover**, plainly: a symlink *inside* a granted directory
    (`--add-dir d` grants `d`'s whole subtree, including links out of it);
    anything a `cmd` does for itself (`["sh","-c","claude --add-dir ~/.ssh"]` is a
    shell command atrium does not parse — an agent whose `cmd` is not an agent CLI
    is flagged as unreadable for exactly this reason); the `prompt`/`kickoff`
    text; and a directory component of a resolved path re-pointed between the ack
    and the spawn. atrium is not a sandbox — at the same uid an agent can read
    anything atrium can (see `warden`'s notes). This bounds and discloses what atrium
    *hands over*.

## The ctl control plane (0.8–0.11)

A fleet is a *saved* org chart; **`ctl`** builds one **live**, and lets an agent
(or you) *drive* it. It turns atrium from a viewer of agents into a **runtime** for
them: a pane can **spawn**, **send** to, **observe**, and **kill** other agent
panes — and because every one is a visible, killable pane with a status border,
the whole hierarchy stays **legible and controllable**, which opaque subagents
are not.

The shape it's built for — one human, a couple of coordinators, a wide bench of
workers:

```
  you (a shell/agent pane)
   └─ hand an initiative to →
      lead_a, lead_b          (coordinators)
        └─ each splits it across →
           dev_1, dev_2, …    (workers, each its own pane)
```

**Turn it on (opt-in, off by default):**

```bash
atrium --allow-ctl claude                 # bind the control channel
atrium --allow-ctl --trust claude         # …and launch every agent trusted + hands-off
atrium --allow-ctl --max-depth 3 claude   # cap spawn recursion (default 6; 0 = unlimited)
```

Without `--allow-ctl` the channel isn't bound and `atrium ctl` refuses — today's
atrium is unchanged for anyone who doesn't ask for this. When it's on, atrium opens a
per-instance control channel (a **named pipe** on Windows, a **unix socket**
elsewhere — zero-dep, non-blocking-drained in the run loop) and injects two env
vars into **every** pane it spawns:

- **`ATRIUM_CTL`** — the channel endpoint. `atrium ctl` connects here.
- **`ATRIUM_PANE`** — the caller pane's agent id, so the server attributes each
  request to its place in the spawn tree.

**Hands-off policy — `--trust <plan|accept|automode|skip>`.** For an agent-driven
fleet to run without you at the keyboard, atrium relaxes the permission posture of
every agent pane it launches. The `--trust` argument sets the **session policy**:
the mode spawned agents run in **and** the ceiling they are capped at. Ordered
low→high power: `plan < accept < automode < skip`.

- **`--trust plan`** — claude's read-only **plan mode**: agents analyze and
  propose, but make no edits and run no commands.
- **`--trust accept`** (or bare **`--trust`**) — auto-accept edits + a safe
  allowlist (the safe default, below).
- **`--trust automode`** — claude's **auto mode** (`--permission-mode auto`):
  hands-off edits *and* commands under claude's own guardrails. (claude only
  enters auto mode if your plan/model/org allow it, else it falls back to default.)
- **`--trust skip`** (alias **`--skip-permissions`**) — **full bypass** (below).

**Per-teammate mode.** By default a `ctl spawn` teammate inherits the session
policy. Add **`--mode plan|accept|automode|skip`** to a spawn to pick a different
mode for that teammate — but **only downward**. The session policy is a ceiling
that holds for every caller, with no exception: a spawn may match it or
*de*-escalate below it, and nothing can elevate past it. To run agents at a higher
posture, set it at launch with `--trust`, where it is one visible, deliberate
choice rather than something a pane can ask for mid-session.

> This used to carve out the root pane, on the reading that the root pane is you
> and you may direct your own session. Two things broke that: `-n N` and `--grid`
> give *every* pane no parent, so every pane in a mass-spawned session counted as
> the operator and could request `skip` — a full bypass — in a session set to
> `plan`; and a ceiling that a pane can exceed is not a ceiling. Since the human
> issues `ctl` from inside a pane, there was no way to tell "you" from "an agent
> running where you launched it".

atrium also strips raw claude permission flags (`--dangerously-skip-permissions`,
`--permission-mode`) from agent-supplied spawn argv, and refuses flags a teammate
may not choose at all (`--mcp-config`, `--plugin-dir`, `--settings` — each reaches
code execution outside the tool-permission system). Agents request a mode through
`--mode`, so atrium's policy stays the single source of truth. Anything atrium ignores,
caps or refuses is reported in the spawn reply's `note` (never silent).

- **`accept` — the safe default.** Agents launch in claude's **auto-accept-edits**
  mode plus an **allowlist of safe dev commands**, so the edit/build/test loop
  runs hands-off — but anything outside the allowlist (`curl`, `git push`, `rm`
  outside the working dir, a critical path) still surfaces as a **visible approval
  prompt** in its pane (which shows as a waiting-on-you `?` in the chrome). The
  built-in allowlist covers `python`/`pytest`, `cargo test`/`build`/`check`/
  `clippy`/`fmt`, `go test`/`build`/`vet`, `node`, `npm test` (read-only shell like
  `ls`/`cat`/`git status` is already auto-accepted by acceptEdits). Extend it with
  **`ATRIUM_TRUST_ALLOW="cmd one,cmd two"`** — each comma-separated prefix `P`
  becomes a `Bash(P *)` matcher, e.g. `ATRIUM_TRUST_ALLOW="just build,make test"`.

- **`skip` — full bypass, explicit.** Appends claude's
  `--dangerously-skip-permissions` (equivalent to `--permission-mode
  bypassPermissions`), so **every** command runs with no gate at all.
  Genuinely dangerous (an agent, and every teammate it spawns, can delete files,
  push to git, or hit the network unsupervised on your machine), so atrium prints a
  plain-English warning and asks you to **confirm at launch** before it starts.

Both modes also **pre-accept claude's separate *folder-trust* dialog** for each
pane's working directory — the "Do you trust the files in this folder?" gate,
stored per-directory in `~/.claude.json`, which neither permission flag covers.
atrium writes only that trust bit, only for the pane's own directory, atomically; a
config it can't parse is left untouched and the pane still launches. That write is
the one place atrium touches another tool's config — deliberate and opt-in. Default
(no flag) leaves permissions entirely to claude.

Any process in a pane — a shell command, or an agent via a tool/hook — issues
control by running **`atrium ctl <cmd>`**, which speaks one JSON request per line
and prints the one-line JSON reply.

**The command surface.** Targets are a pane **id** (its numeric agent id) or a
**role** label (`dev_1`); an ambiguous role is a clear error.

| Command | Does | Reply |
| --- | --- | --- |
| `atrium ctl spawn [--role R] [--identity X] [--here\|--window] -- <cmd…>` | Open a visible worker running `<cmd>` (a new window, or `--here` tiled beside the caller), tagged `R`, optionally under credential identity `X`. | `{"ok":true,"pane":3,"role":"dev_1","session":"…"}` |
| `atrium ctl list` | The live org chart: every pane with parent, role, depth, and `agsess` status. | `{"ok":true,"tree":[{"id":0,"parent":null,"role":null,"title":"claude","depth":0,"status":"idle"},…]}` |
| `atrium ctl send <target> <text…>` | Deliver `<text>` to a pane's agent as a submitted prompt. **Queued until the target is idle** (agsess-gated), so it never lands mid-turn. | `{"ok":true,"target":3,"queued":true}` |
| `atrium ctl status [<target>]` | One target's status, or (no target) a roll-up of the caller's subtree. Backed by `agsess` — `working` / `waiting-approval` / `waiting-prompt` / `idle` (or `null` if unbound). | `{"ok":true,"pane":3,"status":"working"}` |
| `atrium ctl kill <target>` | Terminate a worker **and its whole subtree** (a lead's kill reaps its ICs). The dead panes' windows re-tile / close on the next tick. | `{"ok":true,"killed":[3,4,5]}` |
| `atrium ctl audit [N]` | The control-request log (most recent `N`, or all), for reconstructing a run. | `{"ok":true,"audit":[{"seq":1,"caller":0,"action":"spawn","detail":"role=dev_1 argv=claude identity=-","ok":true,"note":"pane=1"},…]}` |
| `atrium ctl board set <key> <field=value…>` | Merge fields into the shared board (create if absent); an empty value clears a field. | `{"ok":true,"key":"auth","entry":{"by":"dev_1","ms":…,"fields":{"status":"DONE","owner":"Max"}}}` |
| `atrium ctl board get <key>` / `list` / `del <key>` | Read one entry, roll up the whole board, or remove an entry. | `{"ok":true,"board":[{"key":"auth","by":"dev_1","fields":{"status":"DONE"}},…]}` |
| `atrium ctl bus pub <topic> [--decision] <field=value…>` | Publish a structured event to `topic`. Default urgency `fyi`; `--decision` marks an escalation that needs a human answer. | `{"ok":true,"event":{"seq":7,"topic":"deploy","kind":"fyi","from":"dev_1","fields":{"msg":"merged"}}}` |
| `atrium ctl bus sub <topic…>` / `unsub [<topic…>]` | Subscribe (merged; `*` = firehose) or unsubscribe (empty ⇒ all) the caller. | `{"ok":true,"subscribed":["deploy","*"]}` |
| `atrium ctl bus feed [--since <seq>]` | Pull the caller's subscribed events with `seq > since` (no echo of your own); returns a `cursor` to resume from. | `{"ok":true,"feed":[{"seq":7,"topic":"deploy",…}],"cursor":7}` |
| `atrium ctl bus resolve <seq>` | Mark a `decision_needed` event answered (clears the bar/panel escalation). | `{"ok":true,"seq":7,"resolved":true}` |

### The board — shared source of truth

`send` is the ephemeral message stream ("I just shipped auth"); the **board** is
the durable current truth (`auth: DONE, owner: Max, url: …`). It's a schemaless
`key → fields` tracker living in the atrium daemon — and because atrium is the single
broker process every pane talks to, it's a plain map behind the pipe: **single
writer, no locking, no consensus.** A coordinating lead sets tasks and reads
`board list` for status/owner/blocker instead of re-scraping each teammate's
transcript; teammates update their own entry as they work. It's part of the ctl
surface, so it's gated by `--allow-ctl`; set **`ATRIUM_BOARD=<file>`** to persist it
across restarts (in-memory otherwise). Every write records who made it (in the
`audit` log and the entry's `by`), but the board is shared by the whole session —
no per-teammate walls. (This is the first half of atrium's coordination layer; the
`bus` below is the second. Both are slated to be extracted into an `abus` org
crate once the layer is clearly its own concern.)

### The bus — the team's event stream

If the board answers *"what is currently true?"*, the **bus** answers *"what just
happened, and does anyone need to act?"* — the active half of the coordination
layer. A teammate **publishes a structured event to a topic**
(`bus pub deploy msg=shipping url=…`); teammates **pull** the topics they
**subscribe** to (`bus sub deploy` then `bus feed`). State-only coordination means
polling the board; the bus lets a finished worker *notify* without everyone
re-checking.

Two urgency classes carry the visibility-vs-approval split: **`fyi`** (the
default — cheap, lands on the feed) and **`decision_needed`** (`--decision` — an
escalation that needs a human/lead answer). Open decisions surface **actively**:
the `Ctrl+A b` panel lists them first and the status bar shows
`N decisions need you`, so you don't have to be looking. `bus resolve <seq>` clears
one once answered.

**Backpressure is designed in, not bolted on** — because unread messages cost
attention: you never receive an echo of your own events; it's **pull, not push**
(you only pay for topics you subscribed to); each agent has a **publish rate cap**
(20 events / 10 s) so one looping worker can't storm the team; and the log is a
**bounded ring** (the oldest 512 events, then they fall off). Like the board it
lives in the single broker process (a plain in-memory log, no locking), is gated
by `--allow-ctl`, records who sent each event, and persists across restarts with
**`ATRIUM_BUS=<file>`**.

### The security model — the part that must be right

Giving a hosted agent the power to spawn processes is a real capability. The
safety rests on **everything being a visible pane you can kill**, plus hard
limits — each one unit-tested as a pure function:

1. **Opt-in.** No `--allow-ctl`, no channel; `atrium ctl` refuses (`ATRIUM_CTL`
   unset). Nothing changes for a session that didn't ask.
2. **Agents, not arbitrary shell.** `ctl spawn` accepts only commands on an
   **agent allowlist** (default `{claude}`), so a confused agent can't
   `ctl spawn -- rm -rf`. Extend it per-session — set by the human who launches
   atrium — with **`ATRIUM_CTL_ALLOW`** (comma-separated stems, e.g. for another
   agent CLI). It never weakens a session that didn't set it.
3. **No count cap; a depth guard.** A fleet designed for 32 runs 32 — width is
   unlimited. Only *recursion* is bounded, by **`--max-depth`** (default 6,
   `0` removes it): a spawn past the ceiling is refused, so a self-spawning agent
   can't fork-bomb the machine. You always see the live count and can kill any
   subtree.
4. **Subtree-scoped control.** By default an agent may `send` / `status` / `kill`
   / `audit` only panes **in its own subtree**; a lead steers its own team, never
   a sibling's. **You control everything** — a root pane (the one atrium opened, or
   any `ctl` run from outside a pane) is the operator and reaches every pane.
5. **Scoped credential delegation.** `ctl spawn --identity X` may only pass down
   an identity the caller **itself holds** — its own identity, or the session /
   fleet default atrium launched with — so a worker can't mint `wif:prod` its lead
   was never granted. You (the human root) are the trust root and may delegate
   any vault identity. Only the identity **name** is ever handled here; the secret
   is re-resolved per spawn and never stored or logged (the same discipline as
   `--identity`).
6. **Audit.** Every request is recorded — caller, action, a **secret-free**
   detail (`send` logs the text *length*, never the body; identities by name
   only), and outcome — to an in-memory ring, readable live via `ctl audit`
   (subtree-scoped like any read). Opt in to a persistent JSONL mirror with
   **`ATRIUM_CTL_AUDIT=<file>`**; a file it can't open is flashed once, and the log
   keeps running in memory.
7. **Visibility is the safety story.** Nothing a `ctl`-spawned agent does is
   hidden: it's a pane with a status border, in the org chart, killable. That is
   the whole reason to do this in atrium instead of as opaque subagents.

### A run: initiative → leads → ICs

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
atrium ctl status dev_1      # one IC (lead_b's team is out of scope — refused)

# When an IC is done, its lead reaps it (and anything under it):
atrium ctl kill dev_1

# You see all of it, any time — the org chart and the full trail:
atrium ctl list
atrium ctl audit
```

The multiplier: many workers, coordinated by a few of them, driven by one human —
and every level is a pane you can watch, redirect, zoom into (`Ctrl+A z`), or
kill. `ctl` is the small verb layer; the panes, tiling, identity injection, and
`agsess` status it stands on already existed.

### Delegation skills — `atrium-delegate` and `atrium-coordinate`

`ctl` is the mechanism; two Claude Code skills (in [`skills/`](skills/)) are the
instruction layer that makes an agent *use* it. Copy the directories into
`~/.claude/skills/` and any atrium-hosted claude picks them up. They exist because
the trigger is **who decides to delegate**:

- **`atrium-delegate`** (ambient / discretionary) — the agent *may* hand off an
  independent, substantial part when it genuinely pays, and does small or
  dependent work inline. On its own a capable agent usually judges it can do the
  work itself — which is correct — so this fires rarely by design. Its real value
  is discoverability: without it, an agent doesn't know `atrium ctl` exists at all.
- **`atrium-coordinate`** (directed) — for when you *want* fan-out. It casts the
  agent as a coordinator that splits the work, delegates **every** part, monitors,
  collects, and reaps — instead of implementing inline. This is the reliable
  lever: on a task where the ambient skill chose inline every time, coordinate
  mode fanned out every time.

Drop a spawned lead straight into coordinate mode by telling it to coordinate:

```bash
atrium ctl spawn --role lead -- claude
atrium ctl send lead "Coordinate this across a team, one teammate per part: <the initiative>"
```

or, from your own pane, just say "coordinate this across a team: …" and let the
skill surface.

## The architecture (why it's small and faithful)

atrium is a **passthrough** first, an emulator only when it must tile. In
passthrough the active pane's VT byte stream goes straight to your real terminal;
keystrokes flow back undecoded (`rawterm`'s raw-bytes read). On Windows, ConPTY
maintains every pane's screen itself and repaints in full on resize — so
switching to a passthrough pane is a resize nudge, and background panes cost
nothing to track. A scroll region pins the bar row; panes believe the terminal
is one row shorter and can't touch it.

Two host-level subtleties atrium handles (each found by its own test suite,
the hard way):

- A pane's console asks *its host* to switch to win32-input-mode
  (`ESC[?9001h`). atrium is that host, so the request is terminated at atrium
  — tunneled through, it would flip atrium's own input encoding and every
  hotkey would go dark.
- Everything runs even when atrium itself is hosted inside another ConPTY
  (the e2e tests run atrium inside a `pty` and drive it with keystrokes).

## What's deliberately out of scope (v1)

- **Detach/reattach, scrollback, config files.** Later concerns.
- **Vendor integration beyond read-only status.** atrium never reads a pane's
  credentials and never renders transcript *content* — it reads only the
  agent-status signal described above. Pair with `akey` for per-pane keys
  (`atrium akey run work -- claude`) and `agtop` for a full session table.
- **Agents launched *inside* a shell pane.** atrium's agent chrome tracks panes
  atrium itself launched as agents (see "What atrium can and cannot see"). Running
  `$ claude` inside a shell pane is a documented v1 limitation, not a silent bug.
- **Unix polish.** The loop is portable and the e2e suite passes on Linux (run
  it locally), but passthrough pane-switch repaint relies on the app redrawing
  on SIGWINCH
  (full-screen TUIs do; bare shells won't repaint their prompt). Windows —
  where ConPTY guarantees repaints — is the reference platform today.

## Orphaned panes — `atrium reap` and `--reap-orphans`

A pane child is a session leader (`setsid`), so atrium tears down whole process
**groups**, and a pipe-EOF watchdog does it again if atrium dies in a way no
handler can catch. That is the unix reconstruction of what Windows gets from a
Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, kernel-enforced). The
reconstruction is weaker in a way worth stating: a process group is advisory —
a child can `setsid()` out of one — and the watchdog can only kill what the
crash registry names.

So every pane also carries `ATRIUM_SESSION=<owner pid>:<owner start time>`,
injected whether or not the control channel is on, plus a small stamp file for
the case where a pane clears its own environment. A pane is collected only when
**all three** hold, and any ambiguity means *skip*:

1. it carries the marker;
2. it is its own process-group leader (a pane root atrium created, not a
   descendant that wandered into the group — the group is what gets killed);
3. its owner is provably gone — not merely unreachable. `kill(pid, 0)` succeeds
   on a zombie, so liveness uses `pid_running`; the start time defeats pid
   reuse; and the identity check is `proc_pidpath` + `(dev, ino)`, never
   `argv[0]`, which is forgeable on macOS.

The obvious cheap test — "orphan means `ppid == 1`" — is wrong, and atrium does
not use it. A child is reparented only when its parent *finishes* exiting; an
atrium stuck mid-exit never gets there, so its panes keep pointing at a corpse and
`ppid == 1` never becomes true. That is exactly the population that strands
ptys.

`atrium reap` sweeps every dead owner and names each victim and why. Concurrent
sessions are safe by construction: a live session's panes fail condition 3.
`atrium --reap-orphans` runs the same sweep before starting; it is opt-in for now
and always prints what it killed. On Windows none of this is compiled — the Job
Object already gives the guarantee, kernel-enforced.

## Diagnostics

`atrium --stdin-probe` prints the hex of whatever your terminal actually
delivers for a few seconds — the fastest way to answer "what does this
keyboard/terminal send?". Setting `ATRIUM_DEBUG=1` adds stage markers to
stderr (stdin arrivals, pane output, shutdown steps).

## Correctness

Pure units: the prefix scanner (chunk-split safe, literal-prefix,
command/forward interleaving, split and focus-move commands), the bar builder
(marker priority incl. the `?` waiting mark and the fleet count), the tiled
compositor (box borders, title badges, liveness + agent-status tinting), the
binder (pane-id × sessions → status, pure), and the mode-request filter (verified
at every possible chunk split). End-to-end: atrium runs **inside a `pty`**, driven
with real keystrokes — passthrough of a one-shot command and auto-exit, an
interactive shell round-trip with the bar present and `Ctrl+A q` quitting
cleanly, and literal-prefix delivery.

## Development

```bash
# macOS / Linux:
./dev.py check          # or: python3 dev.py check
# Windows:
python dev.py check     # dependency guard + cargo test (the pre-push gate)
```

(`python` is Windows-only; macOS/Linux ship `python3`. `dev.py` carries a
`#!/usr/bin/env python3` shebang and the executable bit, so `./dev.py check`
works everywhere.)

No CI runs right now (GitHub Actions are off, 2026-08-30); the local `dev.py
check` is the gate. A lean CI may return once the nativelite crates are public —
it would need an org-read token to fetch the private git deps.
