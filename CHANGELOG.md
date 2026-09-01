# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.23.0] - 2026-09-01

### Fixed
- **`board set` / `bus pub` accept unquoted spaced values.** The shell splits
  `msg=merged the PR` into three argv tokens; the parser now rejoins continuation
  words into the current field's value, so `amux ctl bus pub deploy msg=merged the
  PR url=…` works without quoting (a token with `=` starts a new field; a token
  without `=` appends to the current one).
- **Text selection works by default.** Mouse capture is now **off** at startup, so
  dragging selects/copies text as in any terminal; `Ctrl+A m` turns capture on
  (click-to-focus, wheel-scrolls-the-hovered-tile) when you want it.
- **Pane labels are 1-based**, matching the status bar. The `board`/`bus` "by/from"
  attribution for an unnamed pane now reads `pane 1`, `pane 2` (was `pane 0`), so
  it lines up with the bar's `1:`, `2:`. (Roles remain the stable identity.)
- **amux no longer freezes at startup on a machine with many/large agent
  transcripts.** The agent-status monitor (`agsess`) discovered stale sessions at
  cold start but left them un-tailed, so the first status poll `read_to_end`'d
  every historical transcript — on a box with a real fleet (hundreds of sessions,
  several hundred MB each) that was >1 GB of reads in one loop tick, freezing the
  event loop for tens of seconds (queued `ctl send`s never delivered, nothing
  repainted). Fixed in `agsess`: a stale session is marked *caught up* (length
  only, no read) so later polls tail only new bytes, and a single tail is now
  capped at 16 MiB (reads the recent tail, resyncs at a line boundary). amux picks
  this up via its `agsess` dependency.

### Changed
- **The coordination layer (board + bus) moved into the new [`abus`] org crate**
  (the one-concern rule; amux stays "multi-agent terminal + broker"). No behavior
  or API change — `amux::board` / `amux::bus` are re-exported from `abus`, so the
  `ctl` surface and everything else are byte-for-byte the same. The 16 board/bus
  unit tests moved with the code.

[`abus`]: https://github.com/nativelite/abus

### Added
- **`--identity` accepts multiple keys — one agent, several credentials.** Give a
  comma-separated list (`amux --identity work,hf claude`) and each resolves to its
  own environment variable(s) and they're all injected into that pane. Pairs with
  the new `akey` per-key env-var support: `akey set hf --for huggingface` stores a
  Hugging Face token as `HF_TOKEN`, `akey set X --env VAR` for anything custom — so
  an agent can hold, say, `ANTHROPIC_API_KEY` + `HF_TOKEN` at once. A name that
  fails to resolve is surfaced in the bar without sinking the others.
- **Command prompt — `Ctrl+A :` — open any shell/program in a new pane.** Type a
  command line (e.g. `wsl`, `wsl -d Ubuntu`, `pwsh -NoLogo`, `claude`, a quoted
  path with spaces) and Enter opens it in a new window, mid-session — no longer
  limited to the command amux was launched with. The prompt shows on the bar row
  with the cursor; Esc or Ctrl+C cancels, Backspace edits. Quoted arguments are
  honored so a path with spaces stays one argument.
- **The bus — topic-routed pub/sub for a coordinating team** (coordination layer
  part 2, the active complement to the board's durable state). A teammate
  publishes a **structured event** to a **topic**; teammates **pull** the topics
  they subscribe to. Command surface (all `--allow-ctl` gated, like the board):
  - `amux ctl bus pub <topic> [--decision] <field=value…>` — publish. Default
    urgency is **`fyi`** (cheap, informational); `--decision` (or
    `--kind decision_needed`) marks an **escalation** that needs a human/lead
    answer.
  - `amux ctl bus sub <topic…>` — subscribe (merged); `*` is the firehose.
    `amux ctl bus unsub [topic…]` drops interest (empty ⇒ all).
  - `amux ctl bus feed [--since <seq>]` — pull your subscribed events, resumable
    via the returned `cursor`. Rendered as a colored feed (amber `!` decisions,
    dim `·` FYI, clickable URLs); `--json` for the raw form.
  - `amux ctl bus resolve <seq>` — mark a decision answered.
  - **Backpressure is built in:** no echo of your own events, pull-not-push (you
    only pay for topics you own), a per-agent publish **rate cap** (20 events /
    10 s), and a **bounded ring** (512 events, oldest fall off). The server
    derives *who* from the caller's role/pane, so a worker can't publish or
    subscribe as someone else.
  - **`AMUX_BUS=<file>`** persists the feed + subscriptions across a restart
    (atomic snapshot), mirroring `AMUX_BOARD`.
- **`Ctrl+A b` is now a board **+** bus dashboard.** The overlay shows the board on
  top and the bus feed below a divider — open `decision_needed` escalations first
  (amber), then recent events. Its title shows `N decisions awaiting you`.
- **Open decisions escalate on the status bar.** When the bus has unresolved
  `decision_needed` events and no flash note is up, the bar shows
  `N decisions need you · Ctrl+A b` — the bus's push channel to the human.

## [0.22.1] - 2026-08-31

### Fixed
- **Resizing during an agent's boot/reconnect no longer leaves it drawn in a
  corner.** A single pty resize can be missed by an app mid-boot (claude while
  `/rc` is connecting), so it kept rendering at the stale size in the top-left of
  the enlarged terminal. On a resize amux now forces a full redraw of the focused
  passthrough pane with a second resize (size-1 → size) — which fires even during
  boot, unlike the previous nudge that only ran once the pane had painted. Size is
  also polled faster (150 ms, was 400 ms) so a resize settles sooner.

## [0.22.0] - 2026-08-31

### Added
- **Live board panel — `Ctrl+A b`.** A full-screen dashboard of the shared board:
  a status glyph + colored `status` (● done green, ○ blocked red, ◐ in-progress),
  the key bold, remaining fields dim with **clickable URLs**, and the writer in
  parens. It updates **live** — any `ctl board` write repaints it — while the panes
  keep running underneath (drained and emulated, just not painted); keystrokes are
  swallowed so they don't reach the hidden panes, and `Ctrl+A b` again drops you
  back with the panes repainted and the cursor restored. This is the visual payoff
  of the board (and the future home for pub/sub bus events). Status colors and the
  clickable-link rendering are now shared between the panel and the CLI board view.

## [0.21.3] - 2026-08-31

### Fixed
- **The status bar no longer disappears on initial load** (until the agent's UI
  settled / remote-control connected). A passthrough app emits a full-screen erase
  (`ESC[2J`) at startup and again as it settles; `2J` ignores the scroll region and
  wipes the bar row, and amux only repainted the bar on a change or a 500 ms
  heartbeat. amux now repaints the bar immediately after the splash→app handoff and
  after any full-screen clear, so the bar is present from the first frame.
- **A newly-switched passthrough pane no longer scrolls into the bar row** (seen as
  the `Ctrl+A !` shell "shrinking" a row after the first command). `switch_window`
  and the repaint nudge now re-assert the bar-protecting scroll region (`rows-1`)
  before clearing — a passthrough app can reset the real terminal's scroll region
  while it runs, and without restoring it the next pane could scroll over the bar.

## [0.21.2] - 2026-08-31

### Added
- **`Ctrl+A !` opens a plain shell in a new window.** Every other pane runs your
  launch command (e.g. claude); this gives you a shell to drive `amux ctl` from —
  so you can run `amux ctl board list` and actually *see* the rendered, colored,
  clickable board view (agent tool-output panes render it plainly). The shell gets
  the ctl env injected like any pane, but no trust posture (it isn't an agent).
  Uses `$SHELL` / `%COMSPEC%`.

## [0.21.1] - 2026-08-31

### Added
- **Rendered board view with clickable links.** `amux ctl board list` / `get` /
  `set` now print a human table instead of raw JSON: one line per entry, the
  `status` field **colored** (green done/shipped, red blocked, cyan in-progress,
  amber waiting), any `http(s)` value rendered as an **OSC-8 clickable hyperlink**,
  and the writer shown dim (`by …`). Field names stay freeform — the coloring keys
  off whatever you put in `status`, and links off anything URL-shaped. Pass
  `--json` for the raw machine reply (scripting). `del` prints a one-line
  confirmation.

## [0.21.0] - 2026-08-31

### Added
- **The shared board — a source-of-truth tracker for a coordinating team.** New
  `amux ctl board set <key> <field=value…> | get <key> | list | del <key>`. Where
  `send` is the ephemeral message stream, the board is the durable current truth
  (`auth: DONE, owner: Max, url: …`) — a schemaless `key → fields` map living in
  the amux daemon. Because amux is the single broker process, it's a plain map
  behind the pipe: single writer, no locking, no consensus. A lead reads
  `board list` for status/owner/blocker instead of re-scraping teammate
  transcripts (the readback gap from real use); teammates update their own entry
  as they work. Shared by the whole session (no per-teammate walls); every write
  records `by` and is audited. Gated by `--allow-ctl`; set `AMUX_BOARD=<file>` to
  persist across restarts (in-memory otherwise). The injected agent directive and
  the amux-coordinate skill now teach teammates to keep the board current.

  This is the first increment of amux's coordination layer; a topic-routed pub/sub
  bus (structured `fyi` vs `decision_needed` events, backpressure) is the planned
  second half, at which point the board + bus extract into an `abus` org crate.
  New `amux::board` module (`Board`, `Entry`, snapshot persistence).

## [0.20.1] - 2026-08-31

### Fixed
- **Redraw fragments at the bottom of a passthrough pane** (e.g. claude's `/`
  command menu leaving overlapping text). The status bar painted itself with
  `ESC 7`/`ESC 8` (DECSC/DECRC), whose single cursor-save slot is **shared** with
  the hosted app — claude parks its cursor there to draw a popup and restores it
  later, so a bar repaint landing in between corrupted claude's saved cursor and
  its next draw landed in the wrong place. amux now tracks the cursor itself (the
  tiled master carries it; a passthrough pane is fed to its emulator) and
  repositions with an explicit CUP after the bar instead of DECSC/DECRC — no
  shared state, no fragments. Tiled mode was already immune (the app's cursor
  saves never reach the real terminal).

## [0.20.0] - 2026-08-31

### Added
- **Scroll the tile your mouse is over.** The wheel now scrolls whichever tile the
  cursor is hovering — not just the focused one. amux routes a wheel notch to the
  pane under the cursor, translated into that pane's inner coordinates, and
  forwards it as a mouse-wheel event to the app. It only forwards to a pane whose
  app actually enabled mouse tracking (sniffed per-pane from its output), so
  hovering a claude tile scrolls it while a bare shell never receives stray bytes.
- **Mouse capture is now ON by default** (was opt-in via `Ctrl+A m`): a click
  focuses the pane under the cursor and the wheel scrolls the hovered tile out of
  the box. Native text selection while captured is **Shift-drag**; `Ctrl+A m`
  still toggles capture off for the terminal's own mouse. Falls back to off if the
  terminal refuses capture.

## [0.19.0] - 2026-08-31

### Added
- **A themed color identity (`amux::theme`).** One shared, vivid truecolor palette
  drives both the status bar and the tile borders, so the chrome reads as a
  designed system and states are distinguishable at a glance:
  - **Status bar** is now a dark themed statusline (was a raw reverse-video strip):
    a cyan **`amux` signature chip**, and each window entry colored by its state in
    the *same* language as the tile borders — focused = bright cyan, waiting =
    amber, exited = red, background-activity = green, idle = dim grey. The visible
    text (and every column) is unchanged; only color was added.
  - **Tile borders** moved from muted 16-color indices to the shared vivid palette,
    so a focused pane (bright cyan) clearly jumps out from idle (dim grey) — the
    contrast complaint.
  - **Startup splash** wordmark now sweeps a cool per-letter gradient (cyan → azure
    → indigo → violet) instead of flat cyan.

  All colors live in one place (`src/theme.rs`) for easy tuning.

## [0.18.1] - 2026-08-31

### Fixed
- **`automode` and `skip` are now distinct policies (0.18.0 conflated them).**
  0.18.0 mapped `automode` straight to `--dangerously-skip-permissions`, but those
  are two different things. Per Claude Code v2.1.195 the permission modes are
  `default` / `acceptEdits` / `plan` / **`auto`** / `dontAsk` / `bypassPermissions`,
  and `--dangerously-skip-permissions` == `--permission-mode bypassPermissions`.
  So now: **`automode` → `--permission-mode auto`** (claude's "auto mode on":
  hands-off edits + commands with claude's guardrails) and a separate
  **`skip` → `--dangerously-skip-permissions`** (full bypass, the nuclear option,
  still confirmed at launch). Rank order is `plan < accept < automode < skip`;
  `--skip-permissions` is now the alias for `--trust skip`. New `TrustMode::Auto`.

## [0.18.0] - 2026-08-31

### Added
- **Session trust policy + per-teammate modes.** `--trust` now takes a policy
  argument — **`--trust plan|accept|automode`** — that sets the mode spawned agents
  run in *and* the ceiling they're capped at. Bare `--trust` = `accept` (today's
  behavior); `--skip-permissions` is an alias for `--trust automode`; new **`plan`**
  = claude's read-only plan mode (`--permission-mode plan`).
- **`ctl spawn --mode plan|accept|automode`** picks a teammate's mode instead of
  inheriting the policy. Governance: **the operator (root pane) may set any mode**
  (elevating a teammate is the human directing the session); **a spawned worker is
  capped at the policy** — it may match or de-escalate but never elevate itself.
  Anything capped or stripped is reported in the spawn reply's `note`, never
  silently. This resolves the tension from 0.17.0's flat "strip everything": the
  human can now say "spin these up in automode / plan" and have it take effect,
  while a rogue sub-agent still can't escalate on its own. New `TrustMode::Plan`,
  `TrustMode::rank`, `TrustMode::from_policy_keyword` / `policy_label`.

## [0.17.1] - 2026-08-31

### Fixed
- **`--here` grid: no thin stub row for non-rectangular counts.** 0.17.0 filled
  rows to `ceil(√n)`, leaving the remainder as a short last row (13 → 4/4/4/**1**,
  a ~1-tall sliver). The grid now uses `rows = floor(√n)` and spreads panes
  **evenly** across those rows (sizes differ by at most one), so every count tiles
  as a real grid with comparable row heights: 13 → three rows of 5/4/4, 3 → a
  single 1×3 row, 7 → 4/3. Perfect counts are unchanged (12 → 3×4, 9 → 3×3).

## [0.17.0] - 2026-08-31

### Fixed
- **`ctl spawn --here` now re-tiles into a balanced grid.** Each `--here` worker
  used to split the caller pane side-by-side, so N of them stacked into a
  degenerate `1×N` strip (12 tiles → twelve unusably-thin columns). The window is
  now re-gridded over all its panes on every `--here` spawn into a near-square
  layout (`cols = ceil(√n)`), so 2→1×2, 4→2×2, 6→2×3, 12→3×4, … stay balanced.
  Existing panes keep their ids; focus lands on the fresh worker. New
  `layout::Tree::grid_from_ids`.

### Security
- **amux governs agent permissions — a spawned agent can no longer escalate its
  teammates.** A hosted agent could append `--dangerously-skip-permissions` (or
  `--permission-mode` / `--allowedTools "Bash(*)"`) to its `ctl spawn -- claude …`
  and hand teammates full bypass even when the human launched in safe `--trust`
  mode. `ctl spawn` now strips those amux-governed permission flags from
  agent-supplied argv and reports what it removed in the spawn reply's `note`
  (visible, never silent); amux's launch trust mode is the single source of
  truth. The injected delegation directive also now tells agents to spawn plain
  `claude` with no permission flags. New `ctl::sanitize_spawn_argv`.

## [0.16.3] - 2026-08-31

### Fixed
- **Startup splash: no more white-bar flash, no spinner pause.** Two initial-load
  glitches, same root cause — the splash was on a *separate* write path from the
  status bar. (1) The splash `2J`-cleared the whole screen ~8×/sec on its own
  atomic frame, wiping the bar a beat before the bar's separate frame repainted
  it, which read as the white bar flashing. The splash is now composited **into
  the tick's single synchronized frame** with the bar, so the whole screen
  repaints atomically and the bar never disappears. (2) At ~1 s in, the agent pane
  is still unbound, so agsess discovery (a full `read_dir` over the projects root)
  fired and blocked the loop for the better part of a second, freezing the
  spinner. Discovery is now **deferred while the sole pane is on the splash** —
  nothing is bound yet, so there is nothing to show — and resumes the moment the
  agent paints.

## [0.16.2] - 2026-08-31

### Fixed
- **Startup splash: hide the cursor.** The cursor blinked next to the spinner and
  flashed at handoff. The splash now hides the cursor (`?25l`) while it is up and
  restores it (`?25h`) when the agent takes over — clean spinner, no stray cursor.

## [0.16.1] - 2026-08-31

### Fixed
- **Startup splash never actually showed.** 0.16.0 marked a pane "painted" on its
  first byte, but an agent emits terminal-setup bytes within milliseconds (before
  any visible content), so the splash was wiped instantly. "Painted" now means the
  emulator has *visible* content; while still blank, the agent's pre-frame bytes
  are suppressed so they don't scribble under the splash, and on the agent's first
  frame the splash is wiped and the screen is painted straight from the emulator.
  So the `a m u x` splash + spinner now stays up for the whole boot.

## [0.16.0] - 2026-08-31

### Added
- **Animated startup splash for the initial (passthrough) load.** A single/zoomed
  pane that has not painted yet now shows a centered `a m u x` wordmark + spinner
  instead of a ~3-second blank screen while the agent boots — so the initial load
  reads as *loading*, not broken. It wipes itself the instant the agent produces
  output. (Complements the tiled per-pane loading spinner from 0.15.0.)
- **`AMUX_SPAWN_LOG=<file>`** — opt-in diagnostic: appends the exact command amux
  launches for each pane (post trust-flags, post shim), so "why isn't this pane
  in the mode I expected" is answered by data. Confirmed with it that `--trust`'s
  `acceptEdits` + allowlist reach ctl-spawned teammates identically to the
  initial pane.

## [0.15.0] - 2026-08-31

### Added
- **Per-pane loading animation.** A tiled pane whose agent has not painted
  anything yet (a freshly-spawned teammate still booting) now shows an animated
  `⣾ starting <title>…` spinner centered in its box, instead of a dead blank
  rect — so an initializing pane reads as *loading*, not *broken*. The spinner
  advances ~8 fps while blank and stops driving repaints the moment the pane
  paints; the blank-check short-circuits, so a painted pane costs nothing.
  (Tiled mode only; a single passthrough pane shows claude's own startup.)

## [0.14.3] - 2026-08-31

### Fixed
- **Shudder during heavy data bursts.** The active window's panes were drained
  with an 8-read cap per tick (~64 KiB), so a big burst — a large `ctl send`, a
  wall of tool output — was composited half-drained; if it straddled a `?2026`
  block, the outer terminal stalled (the shudder). Active panes now drain fully
  before compositing (they already break the instant there is no more data, so
  the higher cap only bites on a genuine flood and never adds idle latency),
  matching what background panes already did.

## [0.14.2] - 2026-08-31

### Added
- **Teammate placement guidance.** The injected ctl directive and the
  `amux-coordinate` skill now tell the agent about `--here` (tile the teammate
  beside you as a pane, for a one-view org chart) vs `--window` (a separate
  window/tab, the default — better for many teammates), and to follow the
  human's stated layout preference. Both were always available on `ctl spawn`;
  now the agent knows to use them, so you can just say "tile them" or "give each
  its own window". Also pulls vterm 0.3.0 (ECH + SU/SD scroll fidelity).

## [0.14.1] - 2026-08-31

### Fixed
- **Agent panes died on launch under `--allow-ctl` on Windows.** The ctl
  directive added in 0.14.0 contained shell-special characters (`"`, `<`, `>`,
  backticks) that broke the `cmd /C claude.cmd …` shim's argument quoting, so
  `amux --allow-ctl --trust claude` opened and quit immediately (the pane's
  claude got a garbled command line and exited, taking amux with it). The
  directive is now plain prose with no shell-special characters, so it survives
  the shim intact. (A loud comment on the constant guards against reintroducing
  a special character.)

## [0.14.0] - 2026-08-31

### Fixed
- **Buttery tiled rendering — synchronized output on emit.** amux now wraps each
  composited frame it writes to the real terminal in DEC mode 2026
  (`?2026h … ?2026l`) and coalesces the tiled composite + the bar into a single
  write per tick, so the outer terminal paints each frame atomically instead of
  showing it half-drawn. This is the emit side of the same mode `vterm` already
  honors on input; it removes the tiled "shutter" when several agent panes
  animate at once. Terminals without mode 2026 ignore the markers (degrades
  cleanly), and an idle tick (empty frame) emits nothing.

### Added
- **Reliable ctl delegation — amux teaches its agents about `ctl`.** When the
  control channel is live (`--allow-ctl`), amux appends a short directive to
  every agent pane it launches (via `--append-system-prompt`): delegate through
  `amux ctl spawn`/`send`/`status`/`kill` (visible panes), **not** the agent's
  own invisible Task/background-agents tool. This is always in the agent's
  context, so reliable coordination no longer depends on a skill happening to
  auto-surface. The `amux-coordinate` / `amux-delegate` skills are hardened with
  the same "use `amux ctl`, never background agents" rule.

## [0.13.0] - 2026-08-31

### Changed
- **`--trust` is now safe by default.** It previously meant full bypass
  (`--dangerously-skip-permissions`); it now launches agents in claude's
  **auto-accept-edits** mode plus an **allowlist of safe dev commands**, so the
  edit/build/test loop runs hands-off while anything outside the allowlist
  (`curl`, `git push`, `rm` outside the working dir, critical paths) still
  surfaces as a **visible approval prompt** in its pane. The built-in allowlist
  covers `python`/`pytest`, `cargo test`/`build`/`check`/`clippy`/`fmt`,
  `go test`/`build`/`vet`, `node`, `npm test`; extend it with
  `AMUX_TRUST_ALLOW="cmd one,cmd two"` (each prefix → a `Bash(P *)` matcher).

### Added
- **`--skip-permissions`** — the previous full-bypass behavior
  (`--dangerously-skip-permissions`, every command runs ungated), now behind an
  explicit plain-English risk warning + a **launch confirmation**. Mutually
  exclusive with `--trust`. Full bypass is a conscious, confirmed choice, not the
  default trusted mode.
- `AMUX_TRUST_ALLOW` — configure the `--trust` safe-command allowlist.

Both trusted modes still pre-accept claude's per-directory folder-trust dialog
(unchanged from 0.12.0). Verified: `dev.py check` green (145 lib incl. new
flag-parsing + allowlist tests, 54 integ), clippy clean.

## [0.12.0] - 2026-08-30

### Fixed
- **`--trust` is now actually hands-off.** It appended claude's
  `--dangerously-skip-permissions` (no per-action prompts) but claude still
  blocked on its separate *"Do you trust the files in this folder?"* dialog —
  a per-directory gate stored in `~/.claude.json`, which that flag does **not**
  cover. `--trust` now also pre-accepts folder trust for each agent pane's
  working directory, so a fleet launched in a fresh folder no longer stops for a
  human to click through the dialog.

### Added
- **`amux::trust`** — pre-accepts claude's folder-trust dialog by setting
  `projects["<dir>"].hasTrustDialogAccepted = true` in `~/.claude.json` (the same
  state claude writes when you accept). Only under `--trust`, only the trust bit,
  only for the pane's own directory; an atomic temp-then-rename write that
  preserves the rest of the file byte-for-byte, and **leaves a config it can't
  parse untouched** (never clobbers what it didn't understand). Honors
  `CLAUDE_CONFIG_DIR`. This is the one place amux writes another tool's config —
  deliberate and opt-in; see the `--trust` note in the README.

## [0.11.0] - 2026-08-30

### Added
- **ctl C3 — `kill`, credential delegation, and an audit log.** The control
  plane gains the last milestone (design §5), so a hosted agent hierarchy is
  fully steerable *and* governed:
  - **`amux ctl kill <target>`** tears down a worker **and its whole subtree**
    (a lead's `kill` reaps its ICs too). Subtree-scoped like `send`/`status`: a
    worker may only kill inside its own subtree; the operator kills anything.
    The reply lists the torn-down pane ids. Teardown reuses the interactive-kill
    reap path (collapse split tree, drop panes, remove any emptied window).
  - **`amux ctl spawn --identity X`** delegates a credential identity to the
    worker — **scoped**: a worker may only pass down an identity it itself holds
    (its own, or the session/fleet default amux launched with), so an IC can't
    mint `wif:prod` its lead was never granted. The operator (human root) is the
    trust root and may delegate any vault identity. Only the identity *name* is
    ever handled here; resolved secrets are re-resolved per spawn and never
    stored or logged (unchanged from identity path B).
  - **Audit log** — every ctl request is recorded (caller, action, a secret-free
    detail, outcome) to an in-memory ring, readable live via **`amux ctl audit
    [N]`** (subtree-scoped: a worker sees only its own subtree's entries).
    Opt-in on-disk JSONL mirror via `AMUX_CTL_AUDIT=<file>`. `send` logs the
    text *length*, never the body; identity *names* only, never secrets.

### Fixed
- **Synchronized-output fidelity (via vterm 0.2.0).** Tiled panes hosting Claude
  Code no longer show stray leftover / overlapping text: vterm now honors DEC
  private mode 2026 (`?2026h`/`?2026l`), double-buffering across a synchronized
  update, so amux's compositor never samples a pane mid-redraw. No amux code
  change — the compositor already reads `term.screen()` each tick; this bumps
  the vterm git dependency to the fix.

## [0.10.0] - 2026-08-30

### Added
- **`--trust` — hands-off agent fleets.** `amux --trust …` launches every agent
  pane amux spawns (the initial one, splits, grid tiles, and **ctl-spawned
  workers**) with claude's `--dangerously-skip-permissions`, so a spawned worker
  comes up **trusted and in auto mode** — no workspace-trust dialog, no
  per-action prompts. This is what makes an agent-driven hierarchy (a lead
  `ctl spawn`-ing and tasking workers) run without a human clicking through a
  trust prompt for every new agent. Opt-in: with it on, agents run tools
  unsupervised. Only agent panes get the flag; a shell pane never does.

## [0.9.0] - 2026-08-30

### Added
- **Mouse mode — `Ctrl+A m` — click a pane to focus it.** Off by default (so the
  terminal's native drag-to-select / copy keeps working); toggle it on and a
  left-click focuses the tiled pane under the cursor. The bar hint shows `m:mouse`
  and a note reports the mode when toggled. While on, hold **Shift** and drag to
  select text (the terminal's override). The scanner parses SGR mouse reports
  (`ESC [ < Cb ; Cx ; Cy M`) only while mouse mode is on — a lone ESC keypress is
  never intercepted or delayed for a TUI in the pane when it's off, and non-mouse
  escapes (arrow keys) always pass through to the pane. Wheel and drag events are
  ignored (only a plain left click focuses). Built on `rawterm::Terminal::set_mouse`.

## [0.8.2] - 2026-08-30

### Fixed
- **You can select and copy text again.** amux (via `rawterm`) was capturing the
  mouse and clearing the console's quick-edit mode, which disabled native
  drag-to-select — so copying out of a pane didn't work. amux now leaves the
  mouse alone by default (picks up `rawterm` 0.2.1), so text selection/copy works
  in every mode. (Click-to-focus a pane will return as an explicit, opt-in mouse
  mode.)

## [0.8.1] - 2026-08-30

### Fixed
- **A `ctl`-spawned worker now paints full-size immediately.** A `ctl spawn`
  (new window) and `ctl spawn --here` (split) sized the worker's pty to a rough
  spawn estimate but never resized the window to its true rects, so the agent
  rendered short — content bunched at the top with a blank area below — until a
  manual terminal resize forced a recompute. Both paths now call `resize_window`
  right after spawning (as the interactive split handlers already did), so the
  worker fills its pane at once.

## [0.8.0] - 2026-08-30

### Added
- **`ctl` control plane (C2): task and observe the hierarchy.** Building on C1's
  channel + `spawn`/`list`:
  - `amux ctl send <target> <text>` — deliver a task to a worker as a submitted
    prompt. **Queue-until-idle** (agsess-gated): if the target is mid-turn the
    text waits and is delivered (text, then Enter) once it goes idle, so a send
    never lands in the middle of a turn. `<target>` is a pane id or role label.
  - `amux ctl status [<target>]` — a target's live `agsess` status, or (no
    target) the caller's subtree roll-up.
  - `amux ctl spawn --here` — tile the worker *beside* the pane that spawned it
    (same window), so a lead and its ICs sit in one view; default `spawn` still
    opens a new window.
- **Subtree-scoped control (Decision 3).** `send`/`status` on a specific target
  are scoped to the caller's own subtree; a **root/operator** pane controls
  everything. A worker cannot steer a sibling's team — refused with a clear JSON
  error. (Pure `in_subtree` guard, unit-tested.)

## [0.7.0] - 2026-08-30

### Added
- **`ctl` control plane (C1) — opt-in.** amux can now host a *controllable*
  hierarchy of agents. Launch with `amux --allow-ctl [--max-depth <N>] …` and
  amux binds a per-process control channel (a Windows **named pipe** / unix
  **socket**, zero third-party deps), injecting its address (`AMUX_CTL`) and each
  pane's id (`AMUX_PANE`) into every pane it spawns. From inside a pane:
  - `amux ctl spawn [--role R] -- <cmd>` opens a **visible** new worker pane
    (returns its agent id + session id as JSON);
  - `amux ctl list` returns the spawn tree (id, parent, role, depth, live
    `agsess` status) as JSON.
  The channel is drained non-blocking from the run loop (no thread). **Off by
  default:** without `--allow-ctl` there is no pipe, `amux ctl` refuses, and
  behavior is identical to before.
- **Spawn-tree safety guards.** `ctl spawn` accepts only commands on the agent
  allowlist (`{claude}`, extensible per-session via `AMUX_CTL_ALLOW`), and a
  `--max-depth` ceiling (default 6, `0` = unlimited) bounds *recursion* — never
  fleet width — as a fork-bomb circuit-breaker. Every worker is a normal pane:
  visible in the bar, killable, in the org chart.

_Not yet: `ctl send` / `status` / `kill`, identity delegation, `--here` splits
(they arrive in C2–C3)._

## [0.6.1] - 2026-08-29

### Fixed
- **Non-power-of-2 grids now tile evenly.** The layout engine split every node
  50/50 by space, but an N-wide row is a left-leaning binary tree
  (`split(a, split(b, c))`), so a 3-column line came out **50% / 25% / 25%**
  (`-n 6`, `8`, `12` were all lopsided; only powers of two happened to halve
  evenly). Each split now divides its span **proportionally to the leaf count**
  of its two children, so `split(a, split(b, c))` gives `a` one-third and its
  two-leaf sibling two-thirds → even thirds. Concretely, an 80-column 3-wide row
  was `40 / 20 / 20` and is now `26 / 27 / 27`. A plain two-pane split is 1 leaf
  vs 1 leaf, so it stays 50/50 — manual splits are unchanged.
- **Tiled view no longer garbles on terminal resize.** On a size change the run
  loop now clears the screen (`\x1b[2J\x1b[H`, matching the window-switch and
  passthrough-repaint paths) before recomputing the grid and resizing each pane's
  emulator and pty to its new inner rect, then forces a full recompose. Without
  the clear, cells from the previous (larger) frame lingered outside the new,
  smaller master. Passthrough (single-pane / zoomed) resize is unchanged.

## [0.6.0] - 2026-08-29

### Added
- **Saved rosters — `amux fleet up <name>`.** Bring up a whole squad of agents
  already in-role — identity, working directory, context dirs, and instructions —
  from one command. A fleet is defined in a project-local **`amux.fleet.json`**
  (checked into the repo so a team shares it), with a user-global fallback
  (`%APPDATA%\amux\fleet.json` on Windows / `~/.config/amux/fleet.json`
  elsewhere). `amux fleet up review-crew` reads the file (read-only), builds one
  tiled window with a pane per agent — laid out by the fleet's `"grid"` or an
  auto balanced grid — and spawns each agent's `cmd` in its `cwd` (so its
  `CLAUDE.md` auto-loads), under its identity (per-agent, else a fleet default,
  resolved via `akey` and injected per-pane), with `--add-dir` (`add_dirs`),
  `--append-system-prompt` (`prompt`), `--model`, and `--effort` when set, plus
  the usual `--session-id` so status binding works per pane. Each pane wears its
  identity `·<name>` tag exactly as `--identity` panes do — the **name** only,
  never a secret. `amux fleet ls` lists the fleet names. Any error before
  spawning — no file (the message names both locations), malformed JSON, unknown
  fleet, a fleet with zero agents, a grid that does not fit the agent count, or a
  `cwd` that does not exist — is reported and **nothing is spawned** (never a
  partial fleet). Unknown JSON fields are ignored, so the schema can grow without
  breaking older files. Reuses the existing per-pane spawn machinery (identity /
  `--session-id` / effective-command / cwd), so amux adds no secret handling.
  Depends on the `json` crate (org, zero-dep) and pty 0.3.0's `spawn_full` (cwd).

## [0.5.0] - 2026-08-29

### Added
- **Mass-spawn — open N agent panes at once (`-n <N>` / `--grid <R>x<C>`).** One
  command now opens a whole balanced grid of agent tiles in a single window
  instead of N interactive splits: `amux -n 4 claude` gives a 2×2 square,
  `amux -n 6 claude` a 2×3, `amux -n 8 claude` a 2×4. `-n <N>` takes a positive
  **multiple of 2** (an odd or zero N is a clear startup error:
  `-n must be a positive multiple of 2`); `--grid <R>x<C>` sets the shape
  explicitly (`--grid 2x3`, product ≥ 2). The flags are amux's own, stripped off
  the front after `--identity`, before the hosted command — a later `-n` that
  belongs to the hosted program is never eaten. Every tile runs the same command,
  **each its own session** (its own `--session-id` via the existing bind path),
  all under the same `--identity` if one was given. The grid is built with
  `layout::Tree::grid`, so focus (`hjkl`/arrows), zoom, kill-retile, and all the
  agent/identity chrome behave exactly as for a hand-split grid.

### Fixed
- **Identity `·<name>` tag now reads as colored *text*, not a filled chip.** The
  status bar is reverse-video, so setting a foreground color on the still-reversed
  tag cell swapped to a filled background block (an accidental color chip). The
  tag now drops `reverse` so its identity color lands on the *text* — the same SGR
  the tiled border already uses — on the bar's normal background, then restores the
  base reverse-video style so nothing bleeds. The `·<name>` **text is always
  present** (color is a redundant a11y channel).

## [0.4.1] - 2026-08-29

### Fixed
- **Terminal restored cleanly on exit (amux owns the alt screen).** A hosted app
  that toggles the **alternate screen buffer** (`ESC[?1049h/l`, or the legacy
  `?1047`/`?47`) in passthrough was fighting amux's own alt screen, so on quit the
  agent's last frame (e.g. Claude's "custom API key detected" prompt) stayed on
  screen instead of the user's pre-amux shell. amux now **owns** the alt screen
  tmux-style: the passthrough filter strips a pane's alt-screen enter/leave
  (alongside the win32-input-mode toggles it already stripped), so panes render
  into amux's buffer and never touch the real terminal's. `cleanup_screen` is now
  a full sanitize — reset SGR, disable mouse reporting and bracketed paste, show
  the cursor, reset the scroll region, then leave the alt screen — run on every
  exit path so a hosted app that left modes on can't corrupt the terminal.
- **Identity `·<name>` tag is now colored in the status bar.** The per-identity
  palette color (the same one the tiled border uses) was only applied on the
  tiled border, so a single-pane (passthrough) user never saw it. The bar now
  colors just the `·<name>` tag segment and restores its reverse-video style
  afterward so the color doesn't bleed into the rest of the line. The `·<name>`
  **text is always present** (color is a redundant a11y channel).

## [0.4.0] - 2026-08-29

### Added
- **Per-pane agent identity — `--identity <name>` (short `-I <name>`).** amux
  now launches an agent pane under a chosen credential identity: it resolves the
  target's environment via [`akey`](https://github.com/nativelite/akey)
  (`akey::resolve` — a key name → `ANTHROPIC_API_KEY`; `wif:<name>` → the five
  federation vars) and injects it into that one child via the new
  `pty::Pty::spawn_with_env`. This is path B of the amux ⨯ akey design (native
  flag, not the wrapper). Usage: `amux --identity work claude`.
- **Inherit-on-split.** The identity applies to the initial agent pane and is
  inherited by every split / new pane. It is re-resolved on **each** spawn; only
  the identity **name** is stored on the pane — never the resolved secret.
- **Identity tag in the chrome (name only).** A `·<name>` tag renders after the
  `index:title` in a pane's top border (e.g. `2:claude ·work`, `2:claude
  ·wif:prod`) and next to the window's entry in the status bar. A `wif:` identity
  reads apart from a static key at a glance. The tag carries a deterministic
  per-identity color from a small, tunable palette (blue/magenta) chosen to avoid
  the status hues; the `·<name>` **text is always rendered** (never color-only).
- **Visible resolve failure.** If `akey::resolve` errors (no such target, vault
  locked) the reason is surfaced in the pane bar and the agent spawns *without*
  the credential — never silently, never unauthenticated-without-saying-so.

amux renders identity **names** only: no key, no token, ever. The resolved env
lives only for the spawn call and is never logged, printed, or persisted (the
`AMUX_DEBUG` traces never include it). New dependency: the org crate `akey`;
`pty` bumped to 0.2.0 for `spawn_with_env`. Third-party dependencies remain zero.

## [0.3.0] - 2026-08-29

### Added
- **Agent-aware pane borders.** When amux launches an agent pane (`claude`) it
  mints a fresh session id and passes `claude --session-id <uuid>` (unless the
  user already supplied `--session-id`/`--resume`/`--continue`), so the pane's
  transcript path is known exactly. The [`agsess`](https://github.com/nativelite/agsess)
  crate derives that session's attention `Status` from the transcript files
  (read-only, status only), and the binder maps pane → status by id.
- Attention chrome on **unfocused** bound agent panes: bright-yellow border +
  `?` title badge when the agent is **waiting on your approval**; grey when
  waiting for a prompt / idle; default with a `~` badge while working. The
  focused pane is never escalated; a dead child (red) and focus (cyan) still
  outrank an agent mark.
- Status bar: a `?` marker on any window whose bound agent is waiting, plus a
  `| N waiting` fleet note so a blocked agent in a backgrounded window surfaces
  off-screen. Border and bar render **status only** — never any transcript text.
- Tiered agent-state poll on the loop's existing `Instant`-throttle (no
  threads): bound transcripts tail ~1 s, discovery ~5 s (accelerated to ~1 s
  while any agent pane is still unbound). The first refresh uses
  `agsess::World::refresh_since(process_start_ms)` so the cold history scan can't
  freeze keystrokes at startup.

Agents launched *inside* a shell pane (`$ claude`) are not bound — a documented
v1 limitation. New dependency: the org crate `agsess`. Third-party dependencies
remain zero. M5 of the amux 0.3 agent-aware feature.

## [0.2.1] - 2026-08-28

### Added
- **Boxed pane borders + liveness color.** Every tiled pane is drawn as a full
  four-sided box with its `index:title` embedded in the top edge, and the border
  is tinted by the pane's liveness: focused = bold bright cyan, exited = red,
  idle = grey, background-active = default. Checked in strict priority order.

## [0.2.0] - 2026-08-28

### Added
- **Tiled splits.** `Ctrl+A "` (stacked) and `Ctrl+A %` (side-by-side) split the
  focused pane; `h`/`j`/`k`/`l` or the arrow keys move focus between tiles;
  `Ctrl+A z` zooms the focused pane to full-screen passthrough and back. A window
  with one pane (or a zoomed pane) stays in the 0.1 passthrough path; a window
  with two or more visible panes renders tiled — each pane driving a `vterm`
  emulator, all composited into one master screen written by diff. Windows
  (`Ctrl+A c`, `1`-`9`, `n`/`p`) and splits coexist.

## [0.1.0] - 2026-08-28

### Added
- Multi-pane terminal hosting arbitrary CLIs on real PTYs (`pty` crate):
  full-screen active pane with byte-level passthrough in both directions
  (`rawterm::read_bytes`), tmux-window-style switching, and a reserved
  status bar (scroll-region pinned, reverse video via `ansi`) with
  active/activity/idle/exited markers.
- `Ctrl+A` prefix keys: `c` new pane, `1`-`9` switch, `n`/`p` cycle, `x`
  kill, `q` quit, doubled prefix for a literal `Ctrl+A`. Panes close on
  child exit; amux exits with the last pane.
- Pane-switch repaint via ConPTY's resize-repaint contract (resize nudge);
  resize propagation to all panes; per-pane background-activity flagging.
- Host-level mode-negotiation filtering: a pane's `ESC[?9001h/l`
  (win32-input-mode) request terminates at amux instead of tunneling to
  the outer terminal and silently re-encoding amux's own stdin — the bug
  the first e2e run shipped, kept fixed by a split-safe filter test.
- Diagnostics: `--stdin-probe` (hex of delivered input) and `AMUX_DEBUG`
  stage markers.
- 14 tests: pure scanner/bar/filter units plus end-to-end suites running
  amux itself inside a `pty` with real keystrokes (passthrough +
  auto-exit, interactive round-trip + bar + quit, literal prefix).

The nativelite **agent terminal** suite flagship (see
`roadmap/agent-terminal-suite.md` in `nativelite/ops`).

[Unreleased]: https://github.com/nativelite/amux/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/nativelite/amux/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/nativelite/amux/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/nativelite/amux/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/nativelite/amux/releases/tag/v0.1.0
