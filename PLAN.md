# PLAN — 0.47.0, "runs in a container"

The release after 0.46.0. The November plan (0.43.0 through 0.46.0) is shipped;
this one comes from a deployment report: atrium as the agent workspace inside
containers, one session per container, attached to through an outer tmux, with
a sidecar and a control endpoint that need to reach the session from outside a
pane. 0.47.0 makes one session easy to host and to operate from outside.
0.48.0 connects sessions to each other.

Every item is a GitHub issue on the `0.47.0` milestone; the issue carries the
detail. This file is the order, the file fences and the dependencies.

Founder decisions that shaped it (2026-11-15):

- **Bus federation uses a sidecar adapter.** atrium speaks a documented
  line-JSON protocol on a local endpoint; NATS, Kafka, WebSocket and TLS live
  in an adapter outside atrium. No transport client enters the tree
  (`standards/zero-dependency.md`). It is 0.48.0.
- **Headless is in.** `atrium fleet up --headless` reverses the README's
  "detach is out of scope" for the run half; `atrium attach` follows in 0.48.0.

## Items

| # | item | size | files (the fence) | done-signal |
|---|---|---|---|---|
| K ([#22](https://github.com/nativelite/atrium/issues/22)) | An inherited outer `TMUX`/`TMUX_PANE` never reaches a pane; `atrium --version --json` | S | `src/main.rs` (startup, version), `src/tmux_shim.rs`, `src/help.rs` | a pane without the shim has no `TMUX`; the shim's own values still arrive; `--version --json` prints version, commit when known, target |
| L ([#23](https://github.com/nativelite/atrium/issues/23)) | A stable ctl endpoint: `--ctl-socket <path>` / `ATRIUM_CTL_SOCKET`, live-listener refusal, stale takeover, a discovery file | M | `../ipc` (main), `src/ipc.rs`, `src/main.rs` (`bind_ctl` only), `src/fleet_cli/up.rs` (the bind only), `docs/control-plane.md` | atrium binds the given path; a second atrium on the same path fails with a clear line; a stale file is taken over; panes get the path as `ATRIUM_CTL` |
| M ([#24](https://github.com/nativelite/atrium/issues/24)) | Operator tokens: a file-borne credential for callers that are not panes, a verb allow-list, `by: operator:<name>` in the audit log | M, **severe** | new `src/operator.rs`, `src/ctl/request.rs`, `src/ctl/client.rs`, `src/ctl_server/dispatch.rs`, `src/audit.rs`, `docs/control-plane.md` | with the file, `ctl kill` from outside a pane works and is audited under the operator's name; without it, refused as today; model-output verbs stay off unless enabled; adversarial review done |
| N ([#25](https://github.com/nativelite/atrium/issues/25)) | `ctl ping` and `ctl metrics` (Prometheus text), both token-free reads | S | `src/ctl/request.rs`, `src/ctl/client.rs`, `src/ctl/reply.rs`, `src/ctl_server/dispatch.rs` (new arms only), `docs/control-plane.md` | `ping` exits 0 only when the run loop answered; `metrics` lists panes by state, wakes, bus depth; golden tests pin both shapes |
| O ([#26](https://github.com/nativelite/atrium/issues/26)) | 0.46.0 hardening: the Claude team snapshot lives in the session, not in a process-wide static | S | `src/claude_teams.rs`, the session state that owns it (`RunState`), `src/overview.rs`, the call sites | no `static WATCH`; the overview tests still pass |
| P ([#27](https://github.com/nativelite/atrium/issues/27)) | Headless sessions: `atrium fleet up <name> --headless [--size RxC]` | L, **severe** | `../rawterm-rs` (main) or a seam in `src/run_loop/`, `src/fleet_cli/up.rs`, `src/fleet_cli/launch.rs`, `src/run_loop/setup.rs`, `src/run_loop/keys.rs`, `README.md` | a fleet starts with no terminal on stdin or stdout, `ctl list`/`status` answer, it outlives its starter, it ends on the last pane's exit or a termination signal with the teardown the attached path runs |

M and N both add arms to `src/ctl/request.rs` and
`src/ctl_server/dispatch.rs`; K, L and P each touch a different part of
`src/main.rs` or `src/fleet_cli/up.rs`. The integrator expects small textual
conflicts there and resolves them at merge.

## Integrator's own work

- Merge one branch at a time, `python dev.py check`, push `main`.
- The adversarial review for M and P before their merge.
- The container section in the docs once K, L, M, N and P have landed: a
  non-root user, the memory guard without cgroup delegation, the container's
  own memory limit, where `ATRIUM_BOARD` and `ATRIUM_BUS` persist, the liveness
  probe on `ctl ping`, the sidecar on the stable socket.
- Live probes left from 0.46.0: the codex hooks status against a live codex,
  and the team board against a captured Claude Code team.
- The cross-target `cargo check` for linux and macOS, and the WSL suite, before
  the release.

## Open, not in this release

- nativelite-vterm keeps at most 64 notices between reads and drops the rest
  without a count. A dropped-notice counter is a vterm change and a publish;
  it waits for the next vterm release.
- The guard group from `PLAN-mod.md` (R12 budgets, R10 approvals, R11 deny
  propagation). R10's routed approvals and M's operator approvals are designed
  together, after M has shown what an operator identity looks like in use.

## 0.48.0, sketched

- **`atrium bridge`**: mirrors chosen bus topics between the local session and
  a line-JSON stream on stdio or a local socket. atrium owns origin tagging
  (`<session>/<pane>`), the hop count that stops echo, resume from the last
  acknowledged `seq`, wakes for local subscribers, and a remote `--decision`
  whose resolution travels back. Remote events are data and never become ctl
  commands. Inbound injection authenticates as an operator (M).
- **A reference sidecar adapter** outside the atrium tree.
- **`atrium attach <session>`** for a headless session (P).
- A shared board namespace (`shared/…`) after the bus.

## Rules that still hold

- Zero third-party dependencies; org crates only.
- Local `python dev.py check` green before every push; there is no CI.
- Builders run only their module's tests; one full gate, the integrator's.
- Every crates.io publish and every repo visibility change is the founder's.
  L needs nativelite-ipc published before atrium 0.47.0; P needs
  nativelite-rawterm published if the seam lands there.
- The marketplace repo is public: skills and plugin syncs go with a release.

## Release chain

nativelite-ipc (L) → nativelite-rawterm (P, if touched) → atrium 0.47.0 →
marketplace plugin bump if a skill changed.
