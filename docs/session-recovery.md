# Session save-state and recovery

atrium periodically writes a snapshot of the running session to disk. If the host
crashes or atrium exits uncleanly, `atrium recover` reads that snapshot and
reopens the session: same window-and-pane layout, same working directories, same
identities, same agent commands, the same **session policy** (deny rules, trust
postures, spawn capabilities, compile pool, memory ceiling) — and, for agents
that support `--resume`, the same transcript.

`atrium recover` takes no arguments. `--trust` and `--max-depth` override what
the snapshot recorded for that one setting; everything you leave out comes back
as it was. `--allow-ctl` only turns the control plane *on* — there is currently
no flag that turns it off for a session whose snapshot recorded it on.

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
  - **deny** — the pane's own deny entries (its fleet agent's `deny`). These are
    *not* recoverable from argv: `--disallowedTools` is assembled at spawn from
    the roster, so a recovery that replayed argv alone handed a pane back every
    command its agent entry had taken away.
  - **can\_spawn** — may this pane create teammates with `ctl spawn`? A fleet
    agent gets this only if its roster entry says so (default false), and the
    snapshot carries it so recovery does not fall back to the spawn path's
    human-pane default of `true`.
  - **depth** / **parent** — the pane's place in the ctl spawn tree, so the
    `--max-depth` recursion guard resumes where it left off. The parent is stored
    as a **pane id**, not an `AgentId`: agent ids are minted per process and are
    re-minted on recovery, so a stored agent id would name a different pane.
  - **mode** — the trust posture this pane actually ran at, which may sit below
    the session ceiling (a fleet agent's own `trust`, a de-escalated ctl worker).

These are the live fields tracked on each `Pane` throughout the session
(`src/main.rs`); the snapshot serialises them to disk.

### Session policy

Alongside the panes, the snapshot records the policy the session as a whole ran
under. None of it can be reconstructed from the pane roster: these are applied
once at `fleet up` and live in process globals, so replaying each pane's command
restores none of them.

- **deny** — the session-wide deny entries (`ATRIUM_DENY` plus the fleet's
  `deny`), re-installed before the first pane spawns.
- **build\_jobs** — the compile pool size, rebuilt so recovered agents' builds
  are pooled rather than unbounded.
- **memory\_mb** — the session memory ceiling for the guard.
- **trust** — the session trust ceiling every pane is capped to.
- **allow\_ctl** / **max\_depth** — whether the control plane was bound, and the
  spawn-depth guard.

**A snapshot is an input with authority, and it is a file in a shared temp
directory.** Be precise about what it can and cannot do:

- A **pane's** recorded posture can only lower it: it is capped to the session
  ceiling, and a corrupt or unrecognised keyword fails closed to `default`
  (atrium relaxes nothing) rather than defaulting to the ceiling.
- A corrupt `deny` list, or a corrupt policy block, **fails the load** instead of
  reading as "this session had no guards".
- The **session ceiling itself** comes from the snapshot when you pass no
  `--trust`, so recovery can raise the posture above the flagless default — up to
  whatever an enclosing atrium session caps it at. Recovery therefore prints the
  posture, marked `(from the snapshot)` when it came from the file, *before* the
  `skip` confirmation, and `skip` still prompts. Read that line. If you do not
  trust the snapshot, pass `--trust` explicitly and it wins.

Note that a snapshot has always carried each pane's **argv**, so a hostile one
could already choose what gets launched; policy restoration widens what it
controls rather than opening the door. Hardening the file itself (exclusive
create, `0600`, refusing a snapshot whose pid never belonged to a dead atrium) is
open work.

## What recovery does not restore

- **Worktree norms.** A fleet's worktree agents are launched with behavioural
  norms folded into their system prompt (*your cwd already is the worktree, do
  not `cd`, commit on your current branch*). Recovery restores the worktree and
  the cwd but not those norms.
- **Fleet context environment.** `CONTEXT_MODE_DIR` and the session suffix a
  fleet's `context` block injects are not re-injected.
- **Panes in other windows.** Only the first window is snapshotted, so a worker
  created by `ctl spawn` *without* `--here` — which opens its own window — is not
  recorded and does not come back.
- **The kickoff prompt is re-delivered.** A fleet agent's kickoff lives in its
  argv, so a recovered agent receives its opening instructions again on top of
  the resumed transcript.


## Where the snapshot lives

One file per session, in the same temp directory as the pane registry
(`reap::registry_dir()` — `%TEMP%` on Windows, `/tmp` on Unix), named for the
atrium process that wrote it:

```
<tmp>/atrium-session-<pid>.json
```

`atrium recover` with no arguments picks the **most recently modified** one, so
recover before starting another atrium session — a newer session writes a newer
snapshot and would be chosen instead. The format is a single JSON line, versioned
(`version: 2`); do not hard-code the path in scripts, it is an implementation
detail that a future daemon may move.

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

### Session policy on recovery

Before the first pane is re-spawned, recovery re-installs the session's deny
rules, memory ceiling and compile pool, in the same order `fleet up` does — the
deny list and ceiling are read at every spawn, and the pool must exist before the
first agent inherits it. Each pane is then re-spawned with its own deny entries,
its own trust posture (capped to the session ceiling), its `can_spawn`
capability and its spawn-tree depth.

Recovery prints a one-line account of what it restored before it takes the
terminal:

```
atrium recover: restoring 4 pane(s), trust automode, ctl on (max depth 6), 5 session deny rule(s), build pool 10, memory cap 32768 MB
```

A **v1 snapshot** — written by atrium 0.35.1 or earlier, before the policy block
existed — has no policy to restore. It still loads, and the panes still come back
with their transcripts, but the guards do not; recovery says so rather than
looking identical:

```
atrium recover: restoring 4 pane(s) — this snapshot predates the policy block, so the session's deny rules, compile pool and memory cap are NOT restored; pass them on the command line if the session had them
```

For that case the equivalents are `ATRIUM_DENY`, `ATRIUM_BUILD_JOBS` and
`ATRIUM_MEMORY_MB` in the environment, plus `--trust` / `--allow-ctl` as flags.

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
4. **Policy is re-installed** — the session's deny rules, compile pool and
   memory ceiling go back in place *before* any pane spawns.
5. **Layout is rebuilt** — the split tree is reconstructed and all panes are
   re-spawned with their own deny entries, trust posture, spawn capability and
   depth. For Claude panes, `--resume <session_id>` reconnects each agent to its
   prior transcript. For other panes, the same command is re-launched in the
   recorded cwd.
6. **ctl topology is restored** — roles are re-stamped and each worker is
   re-pointed at its parent's new agent id, so `ctl spawn`, `ctl send`, and `ctl
   status` see the same logical layout they did before the crash. A worker whose
   parent had already exited cannot be re-pointed; it keeps its recorded depth
   and stays a worker (the privilege gate requires *both* no parent and depth 0,
   so a lost link cannot promote it to the operator).

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
