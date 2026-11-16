# PLAN — 0.49.0

The release after 0.48.0 (2026-11-16: the tmux shim for Claude Code 2.1.325,
the mod failing open with a health check and a circuit breaker, the mod
offered at first start). 0.49.0 finishes the mod's recovery story with an
agent-assisted repair, settles what 0.48.0 left untested on the shim, and
starts connecting sessions. An issue on the `0.49.0` milestone carries each
item's detail once it is opened; this file is the order, the fences and what
each item owes before it merges.

Founder decisions that still shape it (2026-11-15 and 2026-11-16):

- **An agent may repair a broken mod locally.** `atrium mod doctor` gathers
  evidence and can have an agent write a patch, shown as a diff and applied
  on one keypress; an issue reaches the maintainer; the next release
  overwrites the patch. Hands-off application is a config key, off by default.
- **Bus federation uses a sidecar adapter.** atrium speaks line-JSON on a
  local endpoint; NATS, Kafka, WebSocket and TLS live outside the tree.
- The mod is offered, never installed silently; a broken mod recovers by
  itself; nothing restarts a working pane.

## Items

| # | item | size | notes |
|---|---|---|---|
| W1 | Shim live tests owed from 0.48.0: does a custom agent definition's frontmatter outrank the `--permission-mode` atrium passes (then pass or keep dropping `--agent-type`); can a pane token approve a teammate's permission prompts through the team mailbox under a caller-chosen verify key; a second teammate; the mod on, confirming the mod path takes over | S, live | no code until the answers are in |
| W2 | `atrium mod doctor`, evidence half: versions, the failing hook, the error class and scrubbed text, whether the mod loaded, the breaker record; a scrubbed, prefilled issue (browser, or `gh` when signed in) with a fingerprint (Claude Code version + failing hook) in its title, asked each time | M | no model involved |
| W3 | `atrium mod doctor`, repair half: a mod-less claude pane in a scratch copy of the mod's source with the evidence; the patch as a diff; one keypress to apply; a local override in its own folder, marked "locally patched" in the preflight, judged by the health check, discarded if it fails, replaced when a newer release ships a newer mod; `mod_autofix` off by default | L, **severe** | the launch heal must not fight the override: give it its own location and marker |
| W4 | `atrium bridge`: chosen bus topics mirrored to a line-JSON stream on stdio or a local socket; origin tagging (`<session>/<pane>`), a hop count, resume from the last acknowledged seq, wakes for local subscribers, a remote `--decision` resolved at its origin; inbound injection as an operator; a reference sidecar adapter outside the tree | L, **severe** | two-session acceptance test, relay-cut test |
| W5 | `atrium attach <session>` for a headless session | L | a third `Host` variant in `src/run_loop/host.rs` |
| W6 | Follow-ups: #28 (attached fleet drops startup flashes); #29 (headless and ctl-socket items, closed-pipe panics, e2e timeouts tight under load, the build-commit cache oddity); a mistyped fleet name is asked the mod question before it fails; `atrium recover` and a fleet without ctl print no teams line | S each | any time |

W2 and W3 touch `src/mod_cli.rs`, `src/modfiles.rs` and `mod/`. W4 and W5
touch the bus, the operator path and the run loop's host seam; they do not
overlap with the mod work and can run beside it.

## Proof each item owes before it merges

- W1: the live runs themselves, written up in `docs/control-plane.md`.
- W2: the report produced from a deliberately broken mod, read for anything
  that should have been scrubbed.
- W3: a deliberately broken mod repaired end to end on this machine, and a
  patch that fails the health check thrown out.
- W4: two sessions bridged through one relay; a publish in one wakes a
  subscriber in the other and is audited as remote; the relay cut and
  restored.
- W5: a headless fleet attached to, typed into, detached from, still running.

## Open, carried

- macOS has not been built or run since 0.47.0; it is compile-checked only.
- The mod's retry on a busy unix listener has never been seen live (Claude
  Code under WSL is the Windows install and does not reach its prompt).
- A delivery the mod read and then failed to submit is lost: the wire has no
  acknowledgement.
- Windows: a multi-line send to an agent pane without the mod is flattened.
  With the mod offered at first start most claude panes take the inbox path;
  still worth measuring whether a raw line break submits under ConPTY.
- The 0.46.0 team board has a real captured team directory to be checked
  against; the codex hooks status has not run against a live codex.
- The guard group from `PLAN-mod.md` (budgets, routed approvals, deny
  propagation), designed together with operator approvals.

## Rules that still hold

- Zero third-party dependencies; org crates only.
- Local `python dev.py check` green before every push; there is no CI. Run
  the Windows gate and the WSL suite one after the other, not together: three
  ctl end-to-end tests time out under that load.
- The gate compiles against Rust 1.81; builders check with `cargo +1.81`.
- Builders run only their module's tests; one full gate, the integrator's.
- Severe items get a scope statement, a self-review and an adversarial review
  before the merge, and a live run where only a live run can prove it.
- Every crates.io publish and every repo visibility change is the founder's.
- The marketplace repo is public: skills and plugin syncs go with a release.
