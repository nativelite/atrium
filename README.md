# amux
**tmux for agents.** A multi-agent terminal that hosts your own CLIs — a
coding agent, a shell — in switchable full-screen panes with an agent-aware
status bar. Built on the nativelite stack (`pty` + `rawterm` + `ansi`):
**zero third-party dependencies**, all the way down.

```bash
amux claude        # every pane runs `claude`
amux               # every pane runs your shell (COMSPEC / $SHELL)
```

```
┌────────────────────────────────────────────────────────────┐
│  (the active pane: Claude Code, full screen, full fidelity)│
│                                                            │
│ amux | 1:claude* | 2:claude+ | 3:cmd- | ^A c:new n/p:cycle │
└────────────────────────────────────────────────────────────┘
```

**Keys** — `Ctrl+A`, then: `c` new pane · `1`-`9` switch · `n`/`p` cycle ·
`x` kill pane · `q` quit · `Ctrl+A` sends a literal Ctrl+A through.
The bar marks panes: `*` active, `+` background activity, `-` idle,
`!` exited. A pane whose process ends closes itself; when the last one
ends, amux exits.

## The architecture (why it's small and faithful)

amux is a **passthrough**, not an emulator. The active pane's VT byte
stream goes straight to your real terminal — your terminal does the
rendering, so colors, cursor art, and TUIs like Claude Code look exactly
as they would bare. Keystrokes flow back undecoded (`rawterm`'s raw-bytes
read). On Windows, ConPTY maintains every pane's screen itself and
repaints in full on resize — so switching panes is a resize nudge, and
background panes cost nothing to track. A scroll region pins the bar row;
panes believe the terminal is one row shorter and can't touch it.

Two host-level subtleties amux handles (each found by its own test suite,
the hard way):

- A pane's console asks *its host* to switch to win32-input-mode
  (`ESC[?9001h`). amux is that host, so the request is terminated at amux
  — tunneled through, it would flip amux's own input encoding and every
  hotkey would go dark.
- Everything runs even when amux itself is hosted inside another ConPTY
  (the e2e tests run amux inside a `pty` and drive it with keystrokes).

## What's deliberately out of scope (v1)

- **Splits.** Panes are full-screen with a bar, tmux-window-style; tiling
  needs server-side screen state and comes later if wanted.
- **Detach/reattach, scrollback, config files.** Later concerns.
- **Any vendor integration.** amux never reads a pane's credentials or
  transcripts; it hosts programs. Pair with `akey` for per-pane keys
  (`amux akey run work -- claude`) and `agtop` to observe sessions.
- **Unix polish.** The loop is portable and CI runs the e2e suite on
  Linux, but pane-switch repaint relies on the app redrawing on SIGWINCH
  (full-screen TUIs do; bare shells won't repaint their prompt), and a
  pane app that sets its own scroll region can disturb the bar. Windows —
  where ConPTY guarantees repaints — is the reference platform today.

## Diagnostics

`amux --stdin-probe` prints the hex of whatever your terminal actually
delivers for a few seconds — the fastest way to answer "what does this
keyboard/terminal send?". Setting `AMUX_DEBUG=1` adds stage markers to
stderr (stdin arrivals, pane output, shutdown steps).

## Correctness

Pure units: the prefix scanner (chunk-split safe, literal-prefix,
command/forward interleaving), the bar builder (markers,
truncate-and-pad), and the mode-request filter (verified at every possible
chunk split). End-to-end: amux runs **inside a `pty`**, driven with real
keystrokes — passthrough of a one-shot command and auto-exit, an
interactive shell round-trip with the bar present and `Ctrl+A q` quitting
cleanly, and literal-prefix delivery. 14 tests; release binary 192 KB (measured).

## Development

```bash
python dev.py check   # dependency guard + cargo test (what CI runs)
```

CI note: needs the `ORG_READ_TOKEN` secret while the nativelite crates are
private.
