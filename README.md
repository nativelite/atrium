# amux
**tmux for agents.** A multi-agent terminal that hosts your own CLIs — a
coding agent, a shell — in switchable, splittable panes with an **agent-aware**
status bar and borders. Built on the nativelite stack (`pty` + `rawterm` +
`ansi` + `vterm` + `agsess`): **zero third-party dependencies**, all the way
down.

```bash
amux claude        # every pane runs `claude`
amux               # every pane runs your shell (COMSPEC / $SHELL)
amux claude --continue        # any command + args
amux --identity work claude   # run the agent under the `work` credential
amux -n 4 claude              # open four agent panes at once (a 2x2 grid)
amux --grid 2x3 claude        # open a 2x3 grid (six panes)
```

```
┌ 1:claude ─────────────────┐┌ 2:claude ? ───────────────┐
│ (focused: full fidelity)  ││ (its agent is waiting on   │
│                           ││  you — border goes yellow) │
└───────────────────────────┘└───────────────────────────┘
 amux | 1:claude* | 2:claude? | 3:cmd- | 1 waiting | ^A c:win …
```

**Start flags** (amux's own, before the command): `--identity <name>` /
`-I <name>` runs the agent under a credential identity; `-n <N>` opens **N agent
panes at once** (N a positive multiple of 2) in a balanced grid, and
`--grid <R>x<C>` sets the grid shape explicitly. Both must come *before* the
hosted command, so a later `-n`/`-I` the hosted program takes is never eaten.

**Keys** — `Ctrl+A`, then: `c` new window · `1`-`9` switch window · `n`/`p`
cycle · `"` split stacked · `%` split side-by-side · `h`/`j`/`k`/`l` (or arrows)
move focus · `z` zoom the focused pane · `x` kill the focused pane · `q` quit ·
`Ctrl+A Ctrl+A` sends a literal `Ctrl+A` through.

The bar marks each window: `*` active, `?` a bound agent is waiting on you,
`+` background activity, `-` idle, `!` exited. A pane whose process ends closes
itself; when the last one ends, amux exits.

## Windows and splits

amux has two levels. A **window** is the 0.1 "switchable full-screen" concept —
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

When amux launches an agent pane (a `claude` command), it binds that pane to the
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

How the bind works: when amux spawns an agent pane it mints a fresh session id
and passes `claude --session-id <uuid>` (unless you already supplied a
session-selecting flag like `--resume`/`--continue`/`--session-id`, in which case
the id is yours and amux leaves it alone). Claude Code writes its transcript as
`<uuid>.jsonl`, so amux knows the exact file to watch — a direct lookup, no
guessing, even with several agents in one directory. The status itself is
derived by the [`agsess`](https://github.com/nativelite/agsess) crate, which
reads those transcript files **read-only**.

### What amux can and cannot see

amux reads the *status* of Claude Code sessions **it launched** — nothing more.
Concretely:

- It reads only the **session transcript files** (`*.jsonl`) that Claude Code
  writes under your own `~/.claude/projects`, on your own disk, and only the ones
  amux started via `--session-id`. It is **read-only, status only**: the border
  and bar render *whether* an agent is working / waiting / idle, and **never any
  conversation text** — no prompt, no reply, ever leaks from one pane into
  another pane's chrome. No credential file is ever opened; there is no network
  path at all.
- It does **not** see: an agent you started *inside* a shell pane (`$ claude` —
  that pane's title is your shell, so amux never binds it; a **documented v1
  limitation**), agents running in other tools, sessions you run outside amux
  entirely (that is what [`agtop`](https://github.com/nativelite/agtop) is for),
  or anything that would require credentials.
- Because a pane amux did not launch as an agent is never bound, non-agent panes
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
amux --identity work claude      # a static key stored in your OS vault
amux --identity wif:prod claude  # a Workload Identity Federation profile
```

amux resolves the target's environment through
[`akey`](https://github.com/nativelite/akey) — a key name becomes
`ANTHROPIC_API_KEY`, a `wif:<name>` profile becomes the five documented
federation variables — and injects it into **that one child** via the pty
(`pty::Pty::spawn_with_env`). The credential is set for that agent alone; it
never touches amux's siblings, and amux makes no network calls.

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
  `AMUX_DEBUG` traces. A screen-shared grid leaks no key and no token — only the
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
amux -n 4 claude       # 2x2 — four agents in a square
amux -n 6 claude       # 2x3
amux -n 8 claude       # 2x4
amux --grid 2x3 claude # the same 2x3, shape given explicitly
amux --identity work -n 4 claude   # four panes, all under the `work` identity
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
  `amux fleet up` below.

## Saved rosters — `amux fleet up` (0.6)

Mass-spawn opens N copies of *one* command; a **fleet** brings up a squad of
*different, named* agents, each already in-role — its own identity, working
directory, extra context dirs, and instructions — from one command:

```bash
amux fleet up review-crew   # bring up the whole squad, each on its identity
amux fleet ls               # list the fleet names in the file
```

A fleet lives in **`amux.fleet.json`**, looked for in the current directory first
(check it into the repo so a team shares the fleet), then a user-global fallback
(`%APPDATA%\amux\fleet.json` on Windows, `~/.config/amux/fleet.json` elsewhere).
The file is read **read-only** — amux never writes it.

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
  (→ `--effort`). Each agent still gets its own `--session-id`, so the
  agent-aware chrome binds each pane independently.
- **Errors spawn nothing.** No file (the message names both locations), malformed
  JSON, an unknown fleet name, a fleet with zero agents, a grid that does not fit,
  or a `cwd` that does not exist — each is a clear startup error, never a partial
  fleet. Unknown fields are ignored, so the format can grow without breaking
  older files.

## The ctl control plane (0.8–0.11)

A fleet is a *saved* org chart; **`ctl`** builds one **live**, and lets an agent
(or you) *drive* it. It turns amux from a viewer of agents into a **runtime** for
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
amux --allow-ctl claude                 # bind the control channel
amux --allow-ctl --trust claude         # …and launch every agent trusted + hands-off
amux --allow-ctl --max-depth 3 claude   # cap spawn recursion (default 6; 0 = unlimited)
```

Without `--allow-ctl` the channel isn't bound and `amux ctl` refuses — today's
amux is unchanged for anyone who doesn't ask for this. When it's on, amux opens a
per-instance control channel (a **named pipe** on Windows, a **unix socket**
elsewhere — zero-dep, non-blocking-drained in the run loop) and injects two env
vars into **every** pane it spawns:

- **`AMUX_CTL`** — the channel endpoint. `amux ctl` connects here.
- **`AMUX_PANE`** — the caller pane's agent id, so the server attributes each
  request to its place in the spawn tree.

**Hands-off modes — `--trust` (safe) and `--skip-permissions` (dangerous).**
For an agent-driven fleet to run without you at the keyboard, amux can relax the
permission posture of every agent pane it launches. Two levels, deliberately
separate so the safe one is the easy one:

- **`--trust` — the safe default.** Agents launch in claude's **auto-accept-edits**
  mode plus an **allowlist of safe dev commands**, so the edit/build/test loop
  runs hands-off — but anything outside the allowlist (`curl`, `git push`, `rm`
  outside the working dir, a critical path) still surfaces as a **visible approval
  prompt** in its pane (which shows as a waiting-on-you `?` in the chrome). The
  built-in allowlist covers `python`/`pytest`, `cargo test`/`build`/`check`/
  `clippy`/`fmt`, `go test`/`build`/`vet`, `node`, `npm test` (read-only shell like
  `ls`/`cat`/`git status` is already auto-accepted by acceptEdits). Extend it with
  **`AMUX_TRUST_ALLOW="cmd one,cmd two"`** — each comma-separated prefix `P`
  becomes a `Bash(P *)` matcher, e.g. `AMUX_TRUST_ALLOW="just build,make test"`.

- **`--skip-permissions` — full bypass, explicit.** Appends claude's
  `--dangerously-skip-permissions`, so **every** command runs with no gate at all.
  Genuinely dangerous (an agent, and every teammate it spawns, can delete files,
  push to git, or hit the network unsupervised on your machine), so amux prints a
  plain-English warning and asks you to **confirm at launch** before it starts.
  The two flags are mutually exclusive.

Both modes also **pre-accept claude's separate *folder-trust* dialog** for each
pane's working directory — the "Do you trust the files in this folder?" gate,
stored per-directory in `~/.claude.json`, which neither permission flag covers.
amux writes only that trust bit, only for the pane's own directory, atomically; a
config it can't parse is left untouched and the pane still launches. That write is
the one place amux touches another tool's config — deliberate and opt-in. Default
(no flag) leaves permissions entirely to claude.

Any process in a pane — a shell command, or an agent via a tool/hook — issues
control by running **`amux ctl <cmd>`**, which speaks one JSON request per line
and prints the one-line JSON reply.

**The command surface.** Targets are a pane **id** (its numeric agent id) or a
**role** label (`dev_1`); an ambiguous role is a clear error.

| Command | Does | Reply |
| --- | --- | --- |
| `amux ctl spawn [--role R] [--identity X] [--here\|--window] -- <cmd…>` | Open a visible worker running `<cmd>` (a new window, or `--here` tiled beside the caller), tagged `R`, optionally under credential identity `X`. | `{"ok":true,"pane":3,"role":"dev_1","session":"…"}` |
| `amux ctl list` | The live org chart: every pane with parent, role, depth, and `agsess` status. | `{"ok":true,"tree":[{"id":0,"parent":null,"role":null,"title":"claude","depth":0,"status":"idle"},…]}` |
| `amux ctl send <target> <text…>` | Deliver `<text>` to a pane's agent as a submitted prompt. **Queued until the target is idle** (agsess-gated), so it never lands mid-turn. | `{"ok":true,"target":3,"queued":true}` |
| `amux ctl status [<target>]` | One target's status, or (no target) a roll-up of the caller's subtree. Backed by `agsess` — `working` / `waiting-approval` / `waiting-prompt` / `idle` (or `null` if unbound). | `{"ok":true,"pane":3,"status":"working"}` |
| `amux ctl kill <target>` | Terminate a worker **and its whole subtree** (a lead's kill reaps its ICs). The dead panes' windows re-tile / close on the next tick. | `{"ok":true,"killed":[3,4,5]}` |
| `amux ctl audit [N]` | The control-request log (most recent `N`, or all), for reconstructing a run. | `{"ok":true,"audit":[{"seq":1,"caller":0,"action":"spawn","detail":"role=dev_1 argv=claude identity=-","ok":true,"note":"pane=1"},…]}` |

### The security model — the part that must be right

Giving a hosted agent the power to spawn processes is a real capability. The
safety rests on **everything being a visible pane you can kill**, plus hard
limits — each one unit-tested as a pure function:

1. **Opt-in.** No `--allow-ctl`, no channel; `amux ctl` refuses (`AMUX_CTL`
   unset). Nothing changes for a session that didn't ask.
2. **Agents, not arbitrary shell.** `ctl spawn` accepts only commands on an
   **agent allowlist** (default `{claude}`), so a confused agent can't
   `ctl spawn -- rm -rf`. Extend it per-session — set by the human who launches
   amux — with **`AMUX_CTL_ALLOW`** (comma-separated stems, e.g. for another
   agent CLI). It never weakens a session that didn't set it.
3. **No count cap; a depth guard.** A fleet designed for 32 runs 32 — width is
   unlimited. Only *recursion* is bounded, by **`--max-depth`** (default 6,
   `0` removes it): a spawn past the ceiling is refused, so a self-spawning agent
   can't fork-bomb the machine. You always see the live count and can kill any
   subtree.
4. **Subtree-scoped control.** By default an agent may `send` / `status` / `kill`
   / `audit` only panes **in its own subtree**; a lead steers its own team, never
   a sibling's. **You control everything** — a root pane (the one amux opened, or
   any `ctl` run from outside a pane) is the operator and reaches every pane.
5. **Scoped credential delegation.** `ctl spawn --identity X` may only pass down
   an identity the caller **itself holds** — its own identity, or the session /
   fleet default amux launched with — so a worker can't mint `wif:prod` its lead
   was never granted. You (the human root) are the trust root and may delegate
   any vault identity. Only the identity **name** is ever handled here; the secret
   is re-resolved per spawn and never stored or logged (the same discipline as
   `--identity`).
6. **Audit.** Every request is recorded — caller, action, a **secret-free**
   detail (`send` logs the text *length*, never the body; identities by name
   only), and outcome — to an in-memory ring, readable live via `ctl audit`
   (subtree-scoped like any read). Opt in to a persistent JSONL mirror with
   **`AMUX_CTL_AUDIT=<file>`**; a file it can't open is flashed once, and the log
   keeps running in memory.
7. **Visibility is the safety story.** Nothing a `ctl`-spawned agent does is
   hidden: it's a pane with a status border, in the org chart, killable. That is
   the whole reason to do this in amux instead of as opaque subagents.

### A run: initiative → leads → ICs

```bash
# You launch the control plane, trusted so the fleet runs hands-off. With
# --trust, every ctl-spawned agent also comes up trusted (no per-action prompts),
# so a lead can drive its ICs without you clicking through dialogs.
amux --allow-ctl --trust claude

# From your pane, hand the initiative to two coordinators:
amux ctl spawn --role lead_a -- claude
amux ctl spawn --role lead_b -- claude
amux ctl send lead_a "Own the parser rewrite. Split it across two ICs, TDD."
amux ctl send lead_b "Own the docs refresh. One IC is enough."

# lead_a, from its own pane, builds its team beside it (--here) and tasks them:
amux ctl spawn --here --role dev_1 -- claude
amux ctl spawn --here --role dev_2 -- claude
amux ctl send dev_1 "Rewrite the tokenizer, tests first."
amux ctl send dev_2 "Rewrite the AST builder, tests first."

# lead_a coordinates by status, not by scraping terminals:
amux ctl status            # roll-up of lead_a's own subtree
amux ctl status dev_1      # one IC (lead_b's team is out of scope — refused)

# When an IC is done, its lead reaps it (and anything under it):
amux ctl kill dev_1

# You see all of it, any time — the org chart and the full trail:
amux ctl list
amux ctl audit
```

The multiplier: many workers, coordinated by a few of them, driven by one human —
and every level is a pane you can watch, redirect, zoom into (`Ctrl+A z`), or
kill. `ctl` is the small verb layer; the panes, tiling, identity injection, and
`agsess` status it stands on already existed.

### Delegation skills — `amux-delegate` and `amux-coordinate`

`ctl` is the mechanism; two Claude Code skills (in [`skills/`](skills/)) are the
instruction layer that makes an agent *use* it. Copy the directories into
`~/.claude/skills/` and any amux-hosted claude picks them up. They exist because
the trigger is **who decides to delegate**:

- **`amux-delegate`** (ambient / discretionary) — the agent *may* hand off an
  independent, substantial part when it genuinely pays, and does small or
  dependent work inline. On its own a capable agent usually judges it can do the
  work itself — which is correct — so this fires rarely by design. Its real value
  is discoverability: without it, an agent doesn't know `amux ctl` exists at all.
- **`amux-coordinate`** (directed) — for when you *want* fan-out. It casts the
  agent as a coordinator that splits the work, delegates **every** part, monitors,
  collects, and reaps — instead of implementing inline. This is the reliable
  lever: on a task where the ambient skill chose inline every time, coordinate
  mode fanned out every time.

Drop a spawned lead straight into coordinate mode by telling it to coordinate:

```bash
amux ctl spawn --role lead -- claude
amux ctl send lead "Coordinate this across a team, one teammate per part: <the initiative>"
```

or, from your own pane, just say "coordinate this across a team: …" and let the
skill surface.

## The architecture (why it's small and faithful)

amux is a **passthrough** first, an emulator only when it must tile. In
passthrough the active pane's VT byte stream goes straight to your real terminal;
keystrokes flow back undecoded (`rawterm`'s raw-bytes read). On Windows, ConPTY
maintains every pane's screen itself and repaints in full on resize — so
switching to a passthrough pane is a resize nudge, and background panes cost
nothing to track. A scroll region pins the bar row; panes believe the terminal
is one row shorter and can't touch it.

Two host-level subtleties amux handles (each found by its own test suite,
the hard way):

- A pane's console asks *its host* to switch to win32-input-mode
  (`ESC[?9001h`). amux is that host, so the request is terminated at amux
  — tunneled through, it would flip amux's own input encoding and every
  hotkey would go dark.
- Everything runs even when amux itself is hosted inside another ConPTY
  (the e2e tests run amux inside a `pty` and drive it with keystrokes).

## What's deliberately out of scope (v1)

- **Detach/reattach, scrollback, config files.** Later concerns.
- **Vendor integration beyond read-only status.** amux never reads a pane's
  credentials and never renders transcript *content* — it reads only the
  agent-status signal described above. Pair with `akey` for per-pane keys
  (`amux akey run work -- claude`) and `agtop` for a full session table.
- **Agents launched *inside* a shell pane.** amux's agent chrome tracks panes
  amux itself launched as agents (see "What amux can and cannot see"). Running
  `$ claude` inside a shell pane is a documented v1 limitation, not a silent bug.
- **Unix polish.** The loop is portable and the e2e suite passes on Linux (run
  it locally), but passthrough pane-switch repaint relies on the app redrawing
  on SIGWINCH
  (full-screen TUIs do; bare shells won't repaint their prompt). Windows —
  where ConPTY guarantees repaints — is the reference platform today.

## Diagnostics

`amux --stdin-probe` prints the hex of whatever your terminal actually
delivers for a few seconds — the fastest way to answer "what does this
keyboard/terminal send?". Setting `AMUX_DEBUG=1` adds stage markers to
stderr (stdin arrivals, pane output, shutdown steps).

## Correctness

Pure units: the prefix scanner (chunk-split safe, literal-prefix,
command/forward interleaving, split and focus-move commands), the bar builder
(marker priority incl. the `?` waiting mark and the fleet count), the tiled
compositor (box borders, title badges, liveness + agent-status tinting), the
binder (pane-id × sessions → status, pure), and the mode-request filter (verified
at every possible chunk split). End-to-end: amux runs **inside a `pty`**, driven
with real keystrokes — passthrough of a one-shot command and auto-exit, an
interactive shell round-trip with the bar present and `Ctrl+A q` quitting
cleanly, and literal-prefix delivery.

## Development

```bash
python dev.py check   # dependency guard + cargo test (the pre-push gate)
```

No CI runs right now (GitHub Actions are off, 2026-08-30); the local `dev.py
check` is the gate. A lean CI may return once the nativelite crates are public —
it would need an org-read token to fetch the private git deps.
