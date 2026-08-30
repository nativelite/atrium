# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
