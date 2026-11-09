# PLAN — 0.43.0

The release after 0.42.0. 0.42.0 is what sits on `main` unreleased on
2026-11-08: the codex startup-screen fix (50717aa), ctl requests that repaint
only what they changed (8011dd9), Claude Code's own agent teams landing as
atrium panes through the tmux-compatible shim (25a0ded, 29bdb45, 4efe488),
and the `teams` setting (962398f). It ships once the live probe holds: a real
Claude Code lead in `atrium --allow-ctl claude` spawns a split-pane teammate
and it appears as an atrium pane with the lead as its parent.

Every 0.43.0 item is a GitHub issue on the `0.43.0` milestone; the issue is
the unit of work and carries the detail. This file is the order and the
dependencies. Founder decisions that shaped it (2026-11-08): both tmux shims
share one std-only MIT protocol crate, after 0.42.0; `src/ipc` becomes
`nativelite-ipc` after 0.42.0 and foldwave builds no transport of its own
before then; the ctl doorbell and the held inbox connection are one ipc item
with an adversarial review.

## Items

| # | item | size | depends on | done-signal |
|---|---|---|---|---|
| A ([#13](https://github.com/nativelite/atrium/issues/13)) | Shared tmux protocol crate; atrium's shim depends on it | M | new org repo; crates.io publish (founder) | `src/tmux_shim.rs` keeps only `Broker`/`CtlBroker`/env/ensure; the 12 shim tests pass against the crate; registry build of atrium compiles |
| B ([#14](https://github.com/nativelite/atrium/issues/14)) | `nativelite-ipc`: extract `src/ipc` with its exploit replays | L | A published first, or lands in the same release train | atrium depends on the crate; replay tests green in the crate; adversarial review done |
| C ([#15](https://github.com/nativelite/atrium/issues/15)) | ctl listener on the doorbell + held `inbox --wait` connection | L | B (do inside the crate, not during its move) | a ctl request is served without waiting a tick; the mod's 1.5 s poll is gone; `ctl wait`/`ask` hold a connection |
| D ([#16](https://github.com/nativelite/atrium/issues/16)) | Hook-injected certainty status for unmodded claude panes | M | — | yellow border within one tick of a permission prompt on a claude pane without the mod; user-passed `--settings` never overridden |
| E ([#17](https://github.com/nativelite/atrium/issues/17)) | Teams follow-ups: `--teammate-mode auto` on argv, e2e with the fake claude, first-split size | M | 0.42.0 probe result | e2e green; no settings.json edit needed for a fresh user |
| F ([#18](https://github.com/nativelite/atrium/issues/18)) | agsess refresh O(bound sessions), snapshot on change, indexed status | M | — | refresh cost measured before/after; idle CPU and key echo re-benchmarked |
| G ([#19](https://github.com/nativelite/atrium/issues/19)) | Title and needs-you notifications: build or retire | M or 0 | founder decision | `src/attention.rs` + the old M1–M5 plan, or the plan deleted |
| H ([#20](https://github.com/nativelite/atrium/issues/20)) | Token-free ctl reads documented as a stable host-read surface | S | — | docs/control-plane.md states the promise; wire-golden tests pin the reply shapes |
| I ([#21](https://github.com/nativelite/atrium/issues/21)) | Housekeeping: 15 ms tick docs, benches README, re-bench after 8011dd9, overlays without `2J` | S | — | numbers recorded in benches/README.md |

## Order of attack

1. **H, I, D, F** are independent of the crate moves and can start at once,
   each in its own session or fleet item. D and F touch `src/pane_spawn.rs` and
   `src/vendors.rs` respectively; nothing else overlaps.
2. **A** next: a move, not a rewrite. The publish is the gate.
3. **B then C**: the ipc work is one train. C is the latency win users feel
   (the polls go away); B is what foldwave is waiting on.
4. **E** after the 0.42.0 probe says what the shim got wrong in practice.
5. **G** whenever the founder decides.

## Rules that still hold

- Zero third-party dependencies; org crates only (`standards/zero-dependency.md`).
- Local `python dev.py check` green before every push; there is no CI.
- Severe/critical (B, C) get a scope statement, a self-review and an
  adversarial pass before the push, and the commit message says so.
- Every crates.io publish and every repo visibility change is the founder's.
- The marketplace repo is public: skills and plugin syncs go with a release.

## Release chain for 0.43.0

A's crate → (B's crate) → atrium 0.43.0 → marketplace plugin bump. If A or B
slips, atrium 0.43.0 ships without them: D, E, F, H and I are self-contained.
