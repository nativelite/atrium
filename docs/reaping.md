# Orphaned panes and reaping

atrium makes sure a pane's whole process tree dies when the pane does, and
sweeps up any pane whose owning atrium is provably gone. This page explains the
mechanism, the safety conditions, and the commands and diagnostics involved.

## The problem: cleaning up a pane's process tree

A pane child is a session leader (it is started with `setsid`), so atrium tears
down whole process **groups** rather than a single process. A pipe-EOF watchdog
does the teardown again if atrium dies in a way no handler can catch.

This is the unix reconstruction of what Windows gets for free from a Job Object
(`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), which is kernel-enforced. The
reconstruction is deliberately weaker in two ways worth stating plainly:

- A process group is only advisory: a child can call `setsid()` to leave it.
- The watchdog can only kill what the crash registry names.

## The session marker

Every pane carries a marker, `ATRIUM_SESSION=<owner pid>:<owner start time>`,
injected whether or not the control channel is on. atrium also writes a small
stamp file to cover the case where a pane clears its own environment.

The marker records who owns the pane (the owning atrium's pid) and when that
owner started, so a dead owner can be told apart from a live one and from pid
reuse.

## The three conditions for collecting a pane

A pane is collected only when **all three** conditions hold. Any ambiguity means
skip.

1. It carries the marker.
2. It is its own process-group leader: a pane root that atrium created, not a
   descendant that wandered into the group. The group is what gets killed, so
   this guards against killing more than atrium owns.
3. Its owner is provably gone, not merely unreachable. `kill(pid, 0)` succeeds
   on a zombie, so liveness uses `pid_running`; the start time defeats pid
   reuse; and the identity check is the executable's real path plus `(dev, ino)`
   (via `proc_pidpath` on macOS, `/proc/<pid>/exe` on Linux), never `argv[0]`,
   which is forgeable.

## Why `ppid == 1` is the wrong test

The obvious cheap test, "an orphan is a process whose `ppid == 1`", is wrong,
and atrium does not use it.

A child is reparented (its `ppid` becomes 1) only when its parent **finishes**
exiting. An atrium stuck mid-exit never gets there, so its panes keep pointing
at a corpse and `ppid == 1` never becomes true. That stuck-mid-exit case is
exactly the population that strands ptys, so the cheap test misses precisely the
panes that need collecting.

## Commands

### `atrium reap`

`atrium reap` sweeps every dead owner and names each victim and why. Concurrent
sessions are safe by construction: a live session's panes fail condition 3
(their owner is still running), so one session's reap never touches another's
panes.

### `atrium --reap-orphans`

`atrium --reap-orphans` runs the same sweep before starting. It is opt-in for
now and always prints what it killed.

### Windows

On Windows none of this is compiled. The Job Object already gives the guarantee,
kernel-enforced, so there is nothing to reconstruct.

## Diagnostics

Two flags help when a pane or the terminal is misbehaving:

- `atrium --stdin-probe` prints the hex of whatever your terminal actually
  delivers for a few seconds. It is the fastest way to answer "what does this
  keyboard or terminal send?".
- `ATRIUM_DEBUG=1` adds stage markers to stderr: stdin arrivals, pane output,
  and shutdown steps.

## See also

- [atrium README](../README.md)
