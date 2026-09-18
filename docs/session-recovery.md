# Session save-state and recovery

atrium periodically writes a snapshot of the running session to disk. If the host
crashes or atrium exits uncleanly, `atrium recover` reads that snapshot and
reopens the session: same window-and-pane layout, same working directories, same
identities, same agent commands, the same **session policy** (deny rules, trust
postures, spawn capabilities, compile pool, memory ceiling) — and, for agents
that support `--resume`, the same transcript.

## Picking up where you left off

Start atrium the way you normally do — `atrium`, or `atrium fleet up <name>` — in
the project whose session crashed. It notices, shows what it would restore, and
asks once:

```
atrium: the last session in this project did not exit cleanly (saved 4m ago).
atrium recover: restoring 2 pane(s), trust automode (from the snapshot), ctl on (max depth 6), 5 session deny rule(s), build pool 10, memory cap 32768 MB
  [lead] claude --append-system-prompt <812 chars> --model opus <1403 chars> --resume ee8aef7c-…
      mode automode · may spawn teammates
  [builder] claude --append-system-prompt <1190 chars> --model opus <903 chars> --resume dee537d9-…
      cwd D:\projects\.atrium-worktrees\m1\piece · mode automode · cannot spawn · 3 own deny rule(s)
Resume it? [Y/n]
```

Enter resumes. `n` starts a fresh session and stops asking about that one — it is
kept, and `atrium recover` can still restore it. That one prompt is both the
resume and the approval, so it shows what the snapshot controls: the session's
posture and guards, and for every pane the command it will run (flags as written,
long prompts reduced to their length), its trust mode, whether it may spawn
teammates, and its own deny rules. A permission flag saved in a command
(`--dangerously-skip-permissions`, `--permission-mode`, `--allowedTools`) is
never replayed — posture comes only from the policy you confirm — and is named as
ignored. Every string from the file is stripped of control characters before it
is printed.

The offer is only made for a session that **did not end on purpose**. Quitting
atrium, or every pane exiting, marks the snapshot closed. A crash, a kill, a
closed terminal window, a lost SSH connection and a shutdown all leave it open —
on unix a termination signal runs atrium's teardown but is still not a decision to
end the session. The offer needs a terminal, is never made from inside an atrium
pane (the pane marker, or process ancestry where the platform supports it — not
yet Windows), and an answer of end-of-input is a no.

### `atrium recover`

The explicit form, for when you want to choose:

| Command | What it does |
| --- | --- |
| `atrium recover` | restore this project's newest session that is not still running — crashed *or* closed — after the same confirmation |
| `atrium recover --list` | list this project's saved sessions: state (`running` / `crashed` / `closed`), age, panes, roles, trust, file |
| `atrium recover --snapshot <path>` | restore a specific snapshot file — including one an older atrium left in the temp directory |

`--trust` and `--max-depth` override what the snapshot recorded for that one
setting; everything you leave out comes back as it was. `--allow-ctl` only turns
the control plane *on* — there is currently no flag that turns it off for a
session whose snapshot recorded it on.

Resuming needs a terminal and a person at it: `atrium recover` refuses to run
without one, before it asks or changes anything, and `ATRIUM_YES` does not answer
this confirmation. It also refuses to start a second copy of agents that are
already running — a session whose atrium is still alive, or one whose agent
transcripts another running session has already resumed. That second check is
made again after you answer, in case another terminal resumed it meanwhile.

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
  - **kickoff** — whether the last element of argv is a fleet agent's kickoff
    prompt. A pane that resumes its transcript is launched *without* it: the agent
    is mid-task, and receiving its opening instructions again sent resumed agents
    back to step one. A pane with nothing to resume starts fresh and keeps it.
  - **worktree instructions** — the norms atrium folds into a worktree agent's
    system prompt (*your cwd already is the worktree, do not `cd`, commit on your
    branch*). They are not in argv, so they are recorded separately and folded in
    again on resume.
  - **context store** — the `CONTEXT_MODE_DIR` / `CONTEXT_MODE_SESSION_SUFFIX`
    variables a fleet's `context` block gives each agent. These two names are the
    *only* environment a snapshot can record or restore; any other name in the
    file is dropped when it is read, because a pane's environment (`PATH`, a
    preload variable, a ctl token) is a stronger lever than its command line. A
    value containing a control character is dropped too: on Windows the
    environment is a NUL-separated block, so a NUL inside an allowed value would
    otherwise smuggle in a second variable. A NUL anywhere in a pane's command,
    instructions or paths refuses the whole snapshot — on Windows it would cut
    off everything after it, including the trust flags and deny list atrium
    appends.

These are the live fields tracked on each `Pane` throughout the session
(`src/main.rs`); the snapshot serialises them to disk.

### Session policy

Alongside the panes, the snapshot records the policy the session as a whole ran
under. None of it can be reconstructed from the pane roster: these are applied
once at `fleet up` and live in process globals, so replaying each pane's command
restores none of them.

- **deny** — the session-wide deny entries (`ATRIUM_DENY` plus the fleet's
  `deny`), re-installed before the first pane spawns.
- **claude\_aliases** — the session's claude aliases (`ATRIUM_CLAUDE_ALIASES`
  plus the fleet's `claude_aliases`), re-installed before the first pane
  spawns so a fleet built on a shim is still claude.
- **build\_jobs** — the compile pool size, rebuilt so recovered agents' builds
  are pooled rather than unbounded.
- **memory\_mb** — the session memory ceiling for the guard.
- **trust** — the session trust ceiling every pane is capped to.
- **allow\_ctl** / **max\_depth** — whether the control plane was bound, and the
  spawn-depth guard.
- **topics** — a fleet's declared bus topics, which put the bus in strict
  admission. Without them a resumed fleet accepted any topic its roster ruled out.
  An unreadable list fails the load, like `deny`.

Beside the snapshot, each session also keeps its **bus** and **board** — the
team's subscriptions, events and board entries, which used to live only in the
atrium process and died with it. A resume starts from the resumed session's
copies, so agents come back subscribed and with unread events waiting. They are
written off the run loop by a background writer, owner-only, at most once per
snapshot interval (5 s) and once more on exit, so a crash can lose up to the last
few seconds of bus activity; on load, the bus is held to its live ring and
message-size caps.

**A snapshot is an input with authority, and every agent atrium hosts can write
it.** Be precise about what it can and cannot do:

- A **pane's** recorded posture can only lower it: it is capped to the session
  ceiling, and a corrupt or unrecognised keyword fails closed to `default`
  (atrium relaxes nothing) rather than defaulting to the ceiling.
- A corrupt `deny` list, or a corrupt policy block, **fails the load** instead of
  reading as "this session had no guards".
- The **session ceiling itself** comes from the snapshot when you pass no
  `--trust`, so recovery can raise the posture above the flagless default. Inside
  another atrium session it is capped to that session's posture where process
  ancestry is available (not yet on Windows). Recovery therefore prints the
  posture, marked `(from the snapshot)` when it came from the file, *before* the
  `skip` confirmation, and `skip` still prompts. Read that line. If you do not
  trust the snapshot, pass `--trust` explicitly and it wins.

Note that a snapshot has always carried each pane's **argv**, so a hostile one
could already choose what gets launched; policy restoration widens what it
controls rather than opening the door.

### Who can write the file

The store is owner-only on unix, which keeps *other users* out. It does not keep
out your own agents: everything atrium hosts runs as you, and no file permission
separates a user from their own processes. The honest defenses are therefore
detection and consent, not integrity:

- **Out of the way.** The file lives in your per-user state directory, not in
  the project — the project root is exactly where a fleet's main-tree agents are
  meant to write.
- **Tamper tripwire, on its own file.** The warden digests every snapshot atrium
  writes and checks it on its cadence. A write atrium did not make — an edit, a
  deletion, a re-creation — is raised as a decision (audit log, bus, status bar),
  and atrium immediately rewrites the file from memory, which is authoritative
  while it runs. A write in the last check interval before atrium dies is not
  seen. On Windows every pane dies with atrium (the Job Object), so no hosted
  agent outlives it to write afterwards; on unix an orphaned one can.
- **No tripwire on the rest of the store — on purpose.** A forged snapshot does
  not need to touch atrium's own file: planting one beside it, or editing an old
  crashed session, is what would get it offered. A watch for that was built and
  removed. It is evaded by writing while no atrium runs, or by reusing the pid of
  one that did; and it fired whenever an atrium in the same project died within a
  few seconds of starting — an alert that is both easy to dodge and often wrong
  teaches the operator to ignore the warden. The defenses for a planted file are
  the ones below and the confirmation, which shows every pane's command.
- **Inconsistent files are never candidates.** A file whose recorded project is
  another one, whose recorded pid does not match its name, or whose heartbeat is
  in the future is listed by `atrium recover --list` as *suspect* and is never
  offered, restored by default, or counted toward the prune limit — a future
  timestamp would otherwise make a planted file the newest session and push real
  ones out.
- **Consent.** Nothing is resumed without the confirmation above, and the line
  it follows states the posture, the guards and whether the posture came from
  the file.

Real isolation is an OS boundary — a separate account per agent, or a
container — not something a file can provide.

## What recovery does not restore

- **Panes in other windows.** Only the first window is snapshotted, so a worker
  created by `ctl spawn` *without* `--here` — which opens its own window — is not
  recorded and does not come back.
- **A ctl worker's task prompt.** A fleet kickoff is known and left out on
  resume; a task passed to `ctl spawn -- claude "<task>"` is just the last
  argument of an arbitrary command, indistinguishable from any other, so a resumed
  worker receives it again.
- **Status for a pane resumed by hand.** A pane you launched yourself with
  `claude --resume <id>` has no session id atrium injected, so it shows no agent
  status and its snapshot records none. Recovery still replays its command, so it
  resumes; it just is not recognised as the same agent by the second-copy check.
- **A bus or board given explicitly.** With `ATRIUM_BUS` / `ATRIUM_BOARD` set,
  that file is used as before — written on every change — and is not copied
  into the store.
- **The last seconds of bus activity before a crash.** See above.

The bus and board files are as writable by hosted agents as the snapshot. What
they carry is messages, not permissions — but agents act on messages, and a
message's sender is just a field in the file. The confirmation says how many
events, open decisions and board entries a resume brings back; treat them the way
you would treat a bus the agents wrote while you were away, because that is what
it is.


## Where the snapshot lives

In a per-user state directory, one directory per project and one file per
session:

```
Windows  %LOCALAPPDATA%\atrium\sessions\<project>-<digest>\<pid>-<started ms>.json
unix     $XDG_STATE_HOME/atrium/sessions/<project>-<digest>/<pid>-<started ms>.json
         (~/.local/state/atrium/sessions/... when XDG_STATE_HOME is unset)
```

The start time is in the name so a reused pid never overwrites an older session.
An atrium running inside another atrium's pane keeps no snapshot at all: an agent
in a fleet's main tree shares the operator's working directory, and its session
would otherwise be filed, pruned against and offered as the operator's.

The project is the directory atrium was started in; `<project>` is its folder
name and `<digest>` tells apart two folders with the same name. Two atriums in one
project each keep their own file. Directories are `0700` and files `0600` on
unix. A project keeps its five newest sessions; older ones are pruned when a new
session starts, never one that is still running.

Each file also records the atrium that wrote it and a **heartbeat**: a running
atrium rewrites it every 15 seconds even when nothing changed. A session counts as
running only while its pid is alive *and* its heartbeat is fresh — so a reboot that
hands the old pid to another process does not make a crashed session look alive.

`ATRIUM_STATE_DIR` overrides the location. The test suite uses it (with
`ATRIUM_RESUME_OFFER=0`, which turns the launch-time offer off) so its sessions
never mix with yours.

atrium 0.35.1 and earlier kept snapshots in the temp directory as
`atrium-session-<pid>.json`. Those are not scanned — picking the newest file from
a directory every session and test run writes to is how the wrong session used to
get restored — but when a project has nothing saved, `atrium recover` names the
newest one whose atrium is no longer running, for `--snapshot`.

A session's bus and board sit beside its snapshot as `<pid>-<started ms>.bus.json`
and `.board.json`, and are pruned with it. An atrium nested in another atrium's
pane has no place in the store, so its bus and board stay in memory, as before.

The format is a single JSON line, versioned (`version: 2`); do not hard-code the
path in scripts.

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

1. **atrium exits** — uncleanly (crash, power loss, kill), leaving its snapshot
   marked open, or cleanly (a graceful quit marks it closed, so it is not offered
   on the next launch).
2. **Hosted agents are terminated (Windows) or orphaned (Unix).**
   On Windows, atrium assigns every pane process to a Job Object with
   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`src/reap.rs`). When atrium's handle
   closes — however it dies, including an uncatchable `TerminateProcess` — the
   kernel terminates every process in the job. There is nothing to reattach to;
   `atrium recover` reconstructs the session from the snapshot.
   On Unix, agents may continue running under `init` until they exit naturally.
   The orphan reaper can reclaim stale processes; see [reaping](reaping.md).
3. **Operator starts atrium in the project** — and confirms the resume offer,
   or runs `atrium recover`. The confirmed snapshot is marked closed so it is not
   offered again; the resumed session writes its own.
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
opens a new session from that project's newest snapshot rather than raising an
error.

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
