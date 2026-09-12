# Session save-state and recovery

atrium periodically writes a snapshot of the running session to disk. If the host
crashes or atrium exits uncleanly, `atrium recover` reads that snapshot and
reopens the session: same window-and-pane layout, same working directories, same
identities, same agent commands — and, for agents that support `--resume`, the
same transcript.

## What is captured

Each snapshot is a point-in-time image of the live session. It records:

- **Window layout** — the split tree describing how panes are arranged and which
  pane is focused. atrium's layout `Tree` stores pane boundaries as a recursive
  left/right split so the exact tile geometry can be reconstructed.

- Per pane:
  - **argv** — the command the pane is running (the hosted command and its
    arguments, exactly as passed to atrium at launch or via `ctl spawn`).
  - **cwd** — the working directory the agent was started in.
  - **identity** — the credential identity name passed via `--identity`, if any.
  - **session\_id** — the UUID atrium injected via `--session-id` when the pane
    was spawned as a Claude agent. `None` for shells and agents atrium did not
    bind. See [agent status](agent-status.md) for how session ids are used.
  - **role** — the ctl role label the pane was spawned under (e.g. `dev_1`), if
    any. Used by the ctl spawn tree to route messages; restored so `ctl send`
    works again after recovery.
  - **worktree** — the git worktree directory the pane is working in, if the
    session was started with per-agent worktrees. See [worktrees](worktrees.md).

These are the live fields tracked on each `Pane` throughout the session
(`src/main.rs`); the snapshot serialises them to disk.

## Where the snapshot lives

The snapshot path is determined by the snap implementation. The snap worker will
publish the concrete location on the `atrium-dev-r4` bus when the feature lands;
this section will be updated to name it. Until then, treat the location as
implementation-defined — do not hard-code it in scripts.

## How agents resume

Recovery distinguishes between agents that can reconnect to a prior transcript and
those that cannot.

### Claude (supports `--resume`)

For a Claude pane, atrium captured the `session_id` UUID it originally injected
via `--session-id`. On recovery, instead of minting a fresh UUID, atrium launches
the pane with `--resume <session_id>`, which reopens the previous transcript
exactly where it left off.

atrium's binder (`src/bind.rs`) already recognises `--resume` / `-r` as a
user-owned session argument and suppresses its own `--session-id` injection in
that case; the same suppression applies to the recovery-generated `--resume`.

### Agents without resume support

For shells and any agent that does not understand `--resume`, atrium launches the
pane fresh: the same argv in the same cwd, with the same identity resolved. The
working directory and role are restored so the pane slots back into the ctl spawn
tree correctly. The prior transcript is not recoverable — those panes come back
at a clean prompt.

## Recovery flow after a host crash

1. **atrium exits** — uncleanly (crash, power loss, kill) or cleanly (graceful
   quit writes a final snapshot).
2. **Hosted agents are terminated (Windows) or orphaned (Unix).**
   On Windows, atrium assigns every pane process to a Job Object with
   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`src/reap.rs`). When atrium's handle
   closes — however it dies, including an uncatchable `TerminateProcess` — the
   kernel terminates every process in the job. There is nothing to reattach to;
   `atrium recover` reconstructs the session from the snapshot.
   On Unix, agents may continue running under `init` until they exit naturally.
   The orphan reaper can reclaim stale processes; see [reaping](reaping.md).
3. **Operator runs `atrium recover`** — no arguments required. atrium locates
   the most recent snapshot for the current session, reads it, and re-opens the
   session window.
4. **Layout is rebuilt** — the split tree is reconstructed and all panes are
   re-spawned. For Claude panes, `--resume <session_id>` reconnects each agent
   to its prior transcript. For other panes, the same command is re-launched in
   the recorded cwd.
5. **ctl topology is restored** — roles are re-stamped so `ctl spawn`, `ctl
   send`, and `ctl status` see the same logical layout they did before the crash.

`atrium recover` is safe to run even if the previous session exited cleanly: it
opens a new session from the last snapshot rather than raising an error.

## Relationship to `--session-id`

atrium injects `--session-id <uuid>` at spawn for every Claude agent pane it
starts, binding the pane to a freshly-minted session id
([agent status](agent-status.md)). The snapshot preserves that id so recovery
can pass it back as `--resume <session_id>` instead of creating a second,
disconnected session.

A pane that already carries `--session-id`, `--resume`, `-r`, `--continue`, or
`-c` in its argv is treated as user-owned; atrium does not inject a second id at
original spawn or at recovery.

See also the [atrium README](../README.md).
