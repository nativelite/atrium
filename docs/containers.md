# Running atrium in a container

atrium makes no network calls of its own and keeps its control channel on a
same-user local socket, so one container holding one atrium session is a
natural unit: the agents inside reach whatever the container's network policy
allows, and everything that operates the session from outside does it through
that one socket. This page is the checklist for that deployment. Each item
points at the page that documents the mechanism.

## The shape

- **One container, one session.** Run atrium as a non-root user with a
  persistent home, so the agents' own credentials and transcripts survive a
  restart.
- **A fixed control socket**, on a volume a sidecar can share:
  `ATRIUM_CTL_SOCKET=/run/atrium/ctl.sock` (or `--ctl-socket`). The socket is
  `0600` and every peer's uid is checked, so a sidecar must run as the same
  uid. See [A stable endpoint](control-plane.md#a-stable-endpoint---ctl-socket).
  The variable names the endpoint; it does not switch the control plane on.
  That is still `--allow-ctl`, or `"allow_ctl": true` in the fleet.
- **Headless or attached.** A fleet that nobody watches starts with
  `ATRIUM_YES=1 atrium fleet up <name> --headless`; see the README's headless
  section. A person who wants to see the panes still attaches through a
  terminal the container provides (`kubectl exec -it … -- atrium …`, with or
  without an outer tmux). There is no `atrium attach` to a headless session
  yet.

## Starting it

```bash
# headless: an init process, a Job, a systemd unit
export ATRIUM_CTL_SOCKET=/run/atrium/ctl.sock
export ATRIUM_BOARD=/home/agent/.atrium/board.json
export ATRIUM_BUS=/home/agent/.atrium/bus.jsonl
ATRIUM_YES=1 atrium fleet up review-crew --headless --size 50x200
```

- Headless needs the control channel and `ATRIUM_YES=1`, and is refused at the
  `skip` posture: full bypass is confirmed by a person at a terminal. An
  unattended fleet tops out at `automode`.
- The session ends when its last pane exits or on `SIGTERM`, `SIGINT` or
  `SIGHUP`, through the same teardown as an attached session, and exits 0
  either way.
- Make atrium the container's main process, or start it under `setsid`.
  `nohup` alone does not protect it: atrium takes `SIGHUP` itself, to tear its
  panes down.

## Inside an outer tmux

Started inside a real tmux, atrium drops the inherited `TMUX` and `TMUX_PANE`
before any pane exists, so Claude Code's agent teams split an atrium pane and
never the outer tmux window. No `env -u TMUX` wrapper is needed.

## Probes and metrics

Both are token-free reads on the control socket. Outside a pane, with no
`ATRIUM_CTL` set, `atrium ctl` finds the session through its discovery file
when exactly one is running; in a sidecar, set `ATRIUM_CTL` to the socket path.

```yaml
livenessProbe:
  exec:
    command: ["atrium", "ctl", "ping", "--timeout", "2000"]
  periodSeconds: 10
```

- `atrium ctl ping` exits 0 only when the session's run loop answered, and 1
  with one line (`no endpoint`, `connection refused`, `timeout`) otherwise. It
  never waits past its timeout.
- `atrium ctl metrics` prints Prometheus text (`atrium_panes{state=…}`,
  `atrium_wakes_delivered_total`, `atrium_bus_depth`, `atrium_open_decisions`,
  …). atrium opens no port: a sidecar runs the command and serves the text.
- `atrium --version --json` gives the version and target for an image label.

Neither probe is written to the audit log.

## Operating it from outside: operator tokens

Without a credential, a caller outside a pane gets the reads only. A control
service that must `send`, `kill`, `spawn`, publish or resolve a decision
presents an operator token; see
[Operator tokens](control-plane.md#operator-tokens-a-caller-that-is-not-a-pane).

```bash
atrium --allow-ctl --operator-token-file /run/atrium/operators.json claude
atrium ctl --operator-token-file /run/atrium/op-token kill dev_2
```

- **The file must be owned by atrium's user and closed to group and others**
  (`chmod 600`), or atrium refuses to start. A Kubernetes Secret volume is
  usually root-owned and group-readable, so it is refused as mounted. Copy it
  in the entrypoint or an init step, as the user atrium runs as:
  `install -m 600 /secrets/operators.json /run/atrium/operators.json`.
- **Keep the file away from the panes if you can.** Panes run as the same
  user as atrium and can read any file that user can. atrium removes
  `ATRIUM_OPERATOR_TOKEN_FILE` from the environment panes inherit, which hides
  the path, not the file. The control service's own copy belongs in the
  sidecar's filesystem, not the agents'.
- **Grant the fewest verbs.** Nothing is granted by default. An operator's
  spawn is bounded by the session's launch trust and takes a bare command
  name only; its text is typed only into agent panes, never into a shell.
- Every operator request is recorded as `operator:<name>` in `atrium ctl
  audit`. Set `ATRIUM_CTL_AUDIT=<file>` to keep the log past the in-memory
  ring.

## Memory

On Linux the hard memory cap for panes needs a cgroup atrium owns: its own
cgroup delegated to it and holding no other process (what
`systemd-run --user --scope -p Delegate=yes` gives). A container normally does
not provide that, so inside one:

- atrium's hard cap is not taken; the soft guard still watches and stops the
  largest build near the ceiling (`ATRIUM_MEMORY_MB`, or `memory_mb` in the
  fleet).
- **The container's own memory limit is the hard limit.** Set it. When it is
  hit the kernel's OOM killer chooses the victim inside the container, and
  that can be atrium itself. Leave headroom above `ATRIUM_MEMORY_MB`.

## What to persist

| what | where | why |
|---|---|---|
| the board | `ATRIUM_BOARD=<file>` | survives a restart; in memory otherwise |
| the bus | `ATRIUM_BUS=<file>` | the same, and a sidecar can tail it |
| the audit log | `ATRIUM_CTL_AUDIT=<file>` | the in-memory ring holds 1024 entries |
| session snapshots | the per-user state directory (`ATRIUM_STATE_DIR` overrides it) | `atrium recover` after a crash; see [session recovery](session-recovery.md) |
| the agents' homes | `$HOME` | their credentials and transcripts |

## What a session does not do

- It does not talk to another session. The board and the bus live in one
  process; bridging the bus between sessions is planned as `atrium bridge`,
  speaking a line protocol to an adapter that owns the transport and TLS.
- It opens no network listener. Everything above is a local socket and files.
