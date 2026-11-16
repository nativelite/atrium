# PLAN — 0.48.0

The release after 0.47.0 ("runs in a container", released 2026-11-15). Two
threads: the mod becomes the default way a claude pane runs in atrium, with
the recovery that makes a default safe; and sessions start talking to each
other. This file is the order, the file fences and the dependencies; an issue
on the `0.48.0` milestone carries each item's detail once it is opened.

Founder decisions that shaped it (2026-11-15 and 2026-11-16):

- **The mod is offered at first start, not installed silently.** After the
  logo, with the config offer, atrium asks once whether to install the mod;
  Enter means yes; the answer is kept in the config; a yes covers refreshing
  it on upgrades. Nobody is asked when nobody is there (headless,
  `ATRIUM_YES`, no terminal).
- **A broken mod recovers by itself.** Hooks fail open; atrium checks each
  modded pane's health; a circuit breaker backs the mod out and brings it back
  when Claude Code or the mod changes.
- **An agent may repair it locally.** `atrium mod doctor` gathers evidence and
  can have an agent write a patch, shown as a diff and applied on one
  keypress; an issue reaches the maintainer; the next release overwrites the
  patch. Hands-off application is a config key, off by default.
- **Bus federation uses a sidecar adapter** (carried over): atrium speaks
  line-JSON on a local endpoint; NATS, Kafka, WebSocket and TLS live outside.
- A teammate launched through the tmux shim inherits what atrium gave its
  lead; other variables pass only through a `teams_env` allowlist.

## What the live probes of 2026-11-16 established

- With the mod installed, a claude pane never reaches the tmux shim: the mod
  takes over the `Agent` tool and makes an atrium pane itself. The shim serves
  claude panes without the mod.
- Claude Code 2.1.325 has no team-create tool: a named `Agent` call is the
  team launch, and teams need `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`.
- Inside atrium a lead delegates with `atrium ctl spawn` by default, because
  atrium's directive tells it to.
- The fixed shim launched a real 2.1.325 teammate (Windows, mod off).

## Items

| # | item | size | state |
|---|---|---|---|
| R | The tmux shim serves Claude Code 2.1.325: a placeholder pane, then `respawn-pane` with the teammate built and governed by atrium; `teams_env`. Needs nativelite-tmuxproto 0.4.0 | L, **severe** | built, live probe passed, review fixes in progress |
| S1 | The mod's hooks fail open: every hook catches its own errors, is bounded in time, and lets the tool call proceed | M | next |
| S2 | Mod health and a circuit breaker: hello within a window, a pane that fails runs unmodded, the breaker stops injecting for the session and is remembered against the Claude Code and mod versions, a pane that never started is relaunched once without the mod, a working pane is flagged and left alone, the mod comes back when either version changes; the installed files are checked against the binary's copy | L, **severe** | next, with S1 |
| S3 | First-run offer after the logo, the answer kept in the config, launch refresh after a yes, one line when nobody is there; the preflight says which path handles a named teammate (mod, shim, neither) and stops announcing teams it cannot deliver | M | after S2 |
| S4 | `atrium mod doctor`: evidence (versions, failing hook, error text), an agent-written patch in a mod-less pane, the diff and one keypress, a local override marked in the preflight and expired by a newer release, a scrubbed prefilled issue with a fingerprint in its title; `mod_autofix` off by default | L, **severe** | after S3, may ride a release later |
| T | `atrium bridge`: chosen bus topics mirrored to a line-JSON stream, origin tagging, hop count, resume from the last acknowledged seq, wakes, remote decisions resolved at their origin; inbound injection as an operator; a reference sidecar adapter outside the tree | L, **severe** | after S3 |
| U | `atrium attach <session>` for a headless session | L | with or after T |
| V | Follow-ups: #28 (attached fleet drops startup flashes), #29 (headless and ctl-socket items) | S | any time |

S1 and S2 touch `mod/`, `src/modstate.rs`, `src/modfiles.rs`, the hello and
report handlers and the spawn path's injection decision. R touches the spawn
and respawn handlers too; the integrator merges R first and expects small
conflicts in `src/pane_spawn.rs` and `src/ctl_server/dispatch.rs`.

## Proof each item owes before it merges

- R: done-signal met (a real lead launched a teammate). Still owed before the
  release: live runs for `--agent-type` frontmatter and for mailbox approvals
  under a caller-chosen verify key; a second teammate; Linux.
- S1/S2: tests that break the mod on purpose (a hook that throws, one that
  hangs, a mod that does not load), and one live run of each against the
  installed Claude Code.
- S3: the offer exercised through a pty (yes, no, Enter, no terminal).
- S4: a deliberately broken mod repaired end to end on this machine.

## Open, carried

- Windows: a multi-line send to an agent pane without the mod is flattened.
  The mod's inbox keeps line breaks; with S3 most claude panes have the mod.
  Still worth measuring whether a raw line break submits under ConPTY.
- The 0.46.0 team board has now a real captured team directory to be checked
  against.
- The codex hooks status has not run against a live codex.
- macOS is compile-checked only.
- The guard group from `PLAN-mod.md` (budgets, routed approvals, deny
  propagation), designed together with operator approvals.

## Rules that still hold

- Zero third-party dependencies; org crates only.
- Local `python dev.py check` green before every push; there is no CI.
- Builders run only their module's tests; one full gate, the integrator's.
- Severe items get a scope statement, a self-review and an adversarial review
  before the merge; a live run where only a live run can prove it.
- Every crates.io publish and every repo visibility change is the founder's.
- The marketplace repo is public: skills and plugin syncs go with a release.
