# atrium

**tmux for coding agents.** A multi-agent terminal that runs your own CLIs, a
coding agent, a shell, in switchable, splittable panes with an **agent-aware**
status bar and borders, so a whole grid of agents stays legible at a glance.

Built entirely on the nativelite stack (`pty` + `rawterm` + `ansi` + `vterm` +
`agsess`): **zero third-party dependencies**, all the way down.

```
┌ 1:claude ─────────────────┐┌ 2:claude ? ───────────────┐
│ (focused: full fidelity)  ││ (its agent is waiting on   │
│                           ││  you, its border is yellow)│
└───────────────────────────┘└───────────────────────────┘
 atrium | 1:claude* | 2:claude? | 3:cmd- | 1 waiting | ^A c:win …
```

---

## Contents

- [Why atrium](#why-atrium)
- [Install](#install)
- [Quickstart](#quickstart)
- [Concepts: windows, panes, and the status chrome](#concepts)
- [Running many agents at once](#running-many-agents-at-once)
- [Coordinating agents: the control plane](#coordinating-agents-the-control-plane)
- [Reference](#reference): keys, commands, environment, config
- [Design and scope](#design-and-scope)
- [Development](#development)

For the full breakdown covering the complete `ctl` reference, the security model,
the architecture, and orphan reaping, see the [`docs/`](docs/) guide.

---

## Why atrium

Run more than one coding agent and the terminal falls apart: you lose track of
which agent is working, which is blocked waiting on you, and which finished ten
minutes ago. Tabs don't help; a backgrounded agent is invisible.

atrium gives every agent a **pane** and reads each one's **real status** (working
/ waiting on you / idle) straight from the agent's own session, so the chrome
tells you where to look. Then it lets one human, through a couple of coordinator
agents, drive a whole bench of workers, every one a visible, killable pane.

It does this with **no third-party code**: the pseudo-terminals, the VT parser,
the screen diffing, and the status reader are all nativelite crates.

## Install

```bash
# From source (works today):
git clone https://github.com/nativelite/atrium && cd atrium
cargo install --path .        # installs the `atrium` command

# Once published to crates.io:
cargo install atrium
```

Requires a recent stable Rust toolchain. Windows is the reference platform
(ConPTY); macOS and Linux run too (see [Design and scope](#design-and-scope)).

## Quickstart

```bash
atrium claude                   # every pane runs `claude`
atrium                          # every pane runs your shell (COMSPEC / $SHELL)
atrium claude --continue        # any command + args is hosted verbatim
atrium --identity work claude   # run the agent under the `work` credential
atrium -n 4 claude              # open four agent panes at once (a 2x2 grid)
```

Then drive it with the **`Ctrl+A`** prefix (press `Ctrl+A`, release, then a key):

| Key | Action |
| --- | --- |
| `c` | new window (runs the launch command) |
| `!` | new shell pane |
| `:` | command prompt: type any program (`wsl`, `pwsh`, `claude`) to open it in a new pane |
| `1`–`9` / `n` / `p` | switch / cycle windows |
| `"` / `%` | split the focused pane stacked / side-by-side |
| `h` `j` `k` `l` / arrows | move focus between tiles |
| `z` | zoom the focused pane to full-screen and back |
| `b` | the board + bus dashboard |
| `m` | toggle mouse capture (off by default, so text selection works) |
| `x` / `q` | kill the focused pane / quit atrium |
| `Ctrl+A` | send a literal `Ctrl+A` through to the pane |

A pane whose process exits closes itself; when the last one exits, atrium exits.

> **Start flags** are atrium's own and go *before* the hosted command, so a flag
> meant for the agent is never eaten: `--identity <name>` / `-I` (a credential
> identity), `-n <N>` (N agent panes, N a positive multiple of 2), `--grid <R>x<C>`
> (explicit grid), `--allow-ctl` (the [control plane](#coordinating-agents-the-control-plane)),
> `--trust <policy>` (hands-off posture).

## Concepts

### Windows and panes

atrium has two levels:

- A **window** is a full-screen workspace. `Ctrl+A c` makes one; `1`–`9` / `n` / `p`
  switch between them.
- Inside a window, `Ctrl+A "` / `%` **split** the focused pane into tiles; `hjkl`
  or arrows move focus; `Ctrl+A z` **zooms** a tile to full-screen and back.

A single (or zoomed) pane renders in **passthrough**: its raw terminal output
goes straight to your real terminal, so a TUI like Claude Code looks pixel-exact.
The moment a window holds two or more panes it renders **tiled**: each pane drives
its own `vterm` emulator and they're composited into one screen, each with a
one-cell border showing `index:title`. Zoom is the escape hatch back to perfect
fidelity.

### Agent-aware status

When atrium launches an agent pane, it binds that pane to the agent's own session
and lets the agent's **real status** tint the pane's chrome, so in a grid you can
see *which agent is stuck waiting on you*:

- **Border** (unfocused tiles): **yellow** = waiting on your approval · **grey** =
  waiting for a prompt or idle · **default** = working.
- **Title badge**: `2:claude ?` waiting on you · `2:claude ~` working.
- **Status bar**: a waiting agent adds `?` by its window name and `| N waiting`,
  so a blocked agent in a *backgrounded* window still surfaces.

This is **read-only, status only**. atrium reads the status of the Claude Code
sessions *it launched* (via the [`agsess`](https://github.com/nativelite/agsess)
crate, over the transcript files Claude Code writes on your own disk) and renders
*whether* an agent is working / waiting / idle, **never any conversation text**,
no prompts, no replies, no credentials, no network. Full detail:
[docs/agent-status.md](docs/agent-status.md).

### Credential identity

`--identity <name>` (short `-I`) launches an agent under a chosen credential,
resolved through [`akey`](https://github.com/nativelite/akey) and injected into
that one child:

```bash
atrium --identity work claude       # a static key from your OS vault
atrium --identity wif:prod claude   # a Workload Identity Federation profile
```

Only the **name** is ever shown (a `·work` tag in the border and bar), **never
the secret**, which is re-resolved per spawn and never cached, logged, or
persisted. A resolve failure is flashed and the agent starts without the
credential, never silently. Full detail: [docs/identity.md](docs/identity.md).

## Running many agents at once

**Mass-spawn** opens a grid of identical agents in one window:

```bash
atrium -n 4 claude        # 2x2, four agents, each its own session
atrium --grid 2x3 claude  # an explicit 2x3 (six panes)
```

`-n <N>` takes a positive multiple of 2 and auto-balances the grid; every tile
runs the same command, each with its own session so the status chrome binds
independently.

**Fleets** bring up a squad of *different, named* agents, each already in-role
with its own identity, working directory, and instructions, from one command:

```bash
atrium fleet up review-crew   # bring up the whole squad
atrium fleet ls               # list the fleets in the file
```

A fleet is defined in **`atrium.fleet.json`** (checked into the repo, or a
user-global fallback). Each agent gets a `name`, a `cmd`, and optional `cwd`,
`add_dirs`, `prompt`, `model`, `effort`, `identity`, and `can_spawn`. Before it
launches, atrium **discloses every directory the roster grants** (resolved through
symlinks) and waits for your Enter. Errors spawn nothing, never a partial fleet.
Full spec and the JSON schema: [docs/fleets.md](docs/fleets.md).

## Coordinating agents: the control plane

A fleet is a *saved* org chart; **`ctl`** builds one **live**. With `--allow-ctl`,
a pane can **spawn**, **send to**, **observe**, and **kill** other agent panes,
turning atrium from a viewer of agents into a **runtime** for them. Because every
agent is a visible, killable pane with a status border, the whole hierarchy stays
legible and controllable, which opaque subagents are not.

```bash
atrium --allow-ctl --trust claude          # bind the channel; run agents hands-off

# From your pane, hand initiatives to coordinators, who build their own teams:
atrium ctl spawn --role lead -- claude
atrium ctl send lead "Own the parser rewrite. Split it across two ICs, TDD."
atrium ctl status            # roll-up of your subtree
atrium ctl list              # the live org chart
```

It stands on three pieces:

- **`ctl`**: the verb layer: `spawn` / `list` / `send` / `status` / `kill` /
  `audit`.
- **The board**: a shared `key → fields` source of truth ("what is currently
  true?"): `atrium ctl board set/get/list/del`.
- **The bus**: a topic pub/sub event stream ("what just happened, who must
  act?"), with `fyi` and `decision_needed` urgencies: `atrium ctl bus
  pub/sub/feed/resolve`.

**Trust** governs how hands-off it runs. `--trust <plan|accept|automode|skip>`
sets the posture for spawned agents *and* the ceiling they're capped at. A
teammate's `--mode` can de-escalate but never elevate. `accept` (the safe default)
auto-accepts edits plus a dev-command allowlist while still prompting for anything
risky.

This is the deepest part of atrium. The full command reference, the board/bus
semantics, the trust ladder, and the **7-point security model** live in
[docs/control-plane.md](docs/control-plane.md) and
[docs/trust-and-security.md](docs/trust-and-security.md). Two Claude Code skills,
[`atrium-delegate`](skills/atrium-delegate) and
[`atrium-coordinate`](skills/atrium-coordinate), teach an agent to actually use
`ctl`.

## Reference

### Environment variables

| Variable | Purpose |
| --- | --- |
| `ATRIUM_CTL` | control-channel endpoint (set by atrium in every pane when `--allow-ctl`) |
| `ATRIUM_PANE` | the caller pane's agent id (set by atrium) |
| `ATRIUM_BOARD` | file to persist the board across restarts (in-memory otherwise) |
| `ATRIUM_BUS` | file to persist the bus across restarts |
| `ATRIUM_CTL_ALLOW` | extra command stems `ctl spawn` may launch (comma-separated) |
| `ATRIUM_CTL_AUDIT` | mirror the ctl audit log to a JSONL file |
| `ATRIUM_TRUST_ALLOW` | extra safe-command prefixes for `--trust accept` |
| `ATRIUM_SESSION` | owner marker atrium injects for orphan reaping |
| `ATRIUM_DEBUG` | `=1` adds stage markers on stderr |

### Subcommands

| Command | Purpose |
| --- | --- |
| `atrium [flags] [cmd...]` | start a session hosting `cmd` (default: your shell) |
| `atrium fleet up <name>` / `ls` | bring up a saved roster / list rosters |
| `atrium ctl <cmd>` | drive the control plane from inside a pane ([reference](docs/control-plane.md)) |
| `atrium reap` / `--reap-orphans` | clean up orphaned pane groups ([reaping](docs/reaping.md)) |
| `atrium --stdin-probe` | print the hex of what your terminal sends (diagnostics) |
| `atrium --version` | print the version |

### Config

Fleets are read from `atrium.fleet.json` (current directory first, then
`%APPDATA%\atrium\fleet.json` on Windows / `~/.config/atrium/fleet.json`
elsewhere). atrium reads it **read-only** and never writes it. The one file atrium
*does* write is Claude Code's per-directory folder-trust bit in `~/.claude.json`,
and only under `--trust`. See [docs/trust-and-security.md](docs/trust-and-security.md).

## Design and scope

atrium is a **passthrough first**, an emulator only when it must tile. In
passthrough the active pane's byte stream goes straight to your terminal and
keystrokes flow back undecoded. On Windows, ConPTY repaints each pane on resize,
so switching panes is a resize nudge and background panes cost nothing to track. A
pinned scroll region protects the status bar row. Deeper notes, including the
win32-input-mode containment and running atrium nested inside another ConPTY,
are in [docs/architecture.md](docs/architecture.md).

**Deliberately out of scope in v1:** detach/reattach, scrollback, and config files
(later concerns); vendor integration beyond read-only status (pair with `akey`
for keys and [`agtop`](https://github.com/nativelite/agtop) for a full session
table); and agents launched *inside* a shell pane (`$ claude`), which atrium
doesn't bind: a documented limitation, not a silent bug. Windows is the reference
platform; the loop is portable and the e2e suite passes on Linux, but passthrough
pane-switch repaint relies on the app redrawing on `SIGWINCH`.

## Development

```bash
python dev.py check      # dependency guard + cargo fmt --check + tests (the gate)
```

`dev.py` is stdlib-only Python (use `./dev.py check` on macOS/Linux). There is no
CI right now; the local `dev.py check` is the pre-push gate.

## License

MIT. nativelite ships its packages permissively; see [LICENSE](LICENSE).
