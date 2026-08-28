# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/nativelite/amux/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nativelite/amux/releases/tag/v0.1.0
