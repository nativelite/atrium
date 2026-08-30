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
  work exactly as for a hand-split grid. (Saved rosters / `amux fleet up` are a
  later increment; this is mass-spawn only.)

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
- **Unix polish.** The loop is portable and CI runs the e2e suite on Linux, but
  passthrough pane-switch repaint relies on the app redrawing on SIGWINCH
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
python dev.py check   # dependency guard + cargo test (what CI runs)
```

CI note: needs the `ORG_READ_TOKEN` secret while the nativelite crates are
private.
