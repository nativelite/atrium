# Architecture

atrium is a multi-agent terminal, "tmux for coding agents," that hosts your own
CLIs in switchable, splittable panes. It is small and faithful by design: a
passthrough first, an emulator only when it has to tile, built entirely on the
nativelite stack with zero third-party dependencies. See the
[project overview](../README.md) for usage and keys.

## The zero-dependency stack

atrium is assembled from five nativelite crates, each doing exactly one thing.
There are no third-party runtime dependencies, all the way down.

- **pty**: pseudo-terminals. Spawns and drives the child processes (your agent,
  your shell) behind each pane.
- **rawterm**: raw input and events. Reads keystrokes as raw bytes so they can
  flow back to the focused pane undecoded.
- **ansi**: VT parsing and screen diff. Parses the terminal byte stream and
  computes minimal screen updates.
- **vterm**: the terminal-emulator core. The screen model atrium falls back to
  when it must tile panes.
- **agsess**: read-only agent session status. Reports whether a pane's agent is
  working or waiting on you, without ever writing to the session.

## Passthrough first, emulator only when tiling

atrium is a passthrough first and an emulator only when it must tile.

In passthrough, the active pane's VT byte stream goes straight to your real
terminal, and keystrokes flow back undecoded via `rawterm`'s raw-bytes read.
Nothing sits between the pane and the screen.

On Windows, ConPTY maintains every pane's screen itself and repaints it in full
on resize. Because of that, switching to a passthrough pane is just a resize
nudge, and background panes cost nothing to track.

A pinned scroll region protects the bar row: it pins the bar, so panes believe
the terminal is one row shorter and cannot touch it.

## Two host-level subtleties

atrium is the host for the consoles it spawns, so it has to handle two host-level
details correctly. Each was found the hard way, by its own test suite.

### Terminating the win32-input-mode request

A pane's console asks *its host* to switch to win32-input-mode (`ESC[?9001h`).
atrium is that host, so the request is terminated at atrium. Tunneled through, it
would flip atrium's own input encoding and every hotkey would go dark.

### Running inside another ConPTY

Everything runs correctly even when atrium is itself hosted inside another
ConPTY. The end-to-end tests rely on this: they run atrium inside a `pty` and
drive it with keystrokes.

## Correctness

atrium is verified with pure unit tests over its logic and end-to-end tests over
the real thing.

### Pure units

- **Prefix scanner**: chunk-split safe, literal-prefix, and
  command/forward interleaving, including split and focus-move commands.
- **Bar builder**: marker priority, including the `?` waiting mark and the fleet
  count.
- **Tiled compositor**: box borders, title badges, and liveness plus
  agent-status tinting.
- **Binder**: pane-id by sessions into status, a pure mapping.
- **Mode-request filter**: verified at every possible chunk split.

### End-to-end

atrium runs inside a `pty`, driven with real keystrokes:

- Passthrough of a one-shot command and auto-exit.
- An interactive shell round-trip with the bar present and `Ctrl+A q` quitting
  cleanly.
- Literal-prefix delivery.
