# Windows: what is held, and why

The ctl control channel (`src/ipc.rs`) was audited and rewritten. The unix half
landed. The Windows half is **held for a machine that can execute it**, so this
file is the record of what is deliberately still broken there.

Nothing here is a regression: the shipping Windows module is the one that was
already in `main`, plus one narrow fix. But the gap between the platforms is now
wide, and that is a decision, not an oversight.

## Where the held implementation lives

The full rewrite is commit **`6292ce3`** on branch `fix/ipc-unix` (`wip:
ipcfix-final.patch as received, unmodified`) — the patch verbatim, before the
Windows module was taken back out. A copy of the same patch is archived outside
the repo at `../amux-worktree-archive-20260905/`.

It **type-checks** under `cargo clippy --target x86_64-pc-windows-msvc
--all-targets -- -D warnings`. That is all it has ever been shown to do. No line
of it has been executed on Windows.

## Defect status, per platform

| # | Defect | unix | Windows |
|---|---|---|---|
| D1 | `respond()` truncates: `write_all` on a non-blocking stream | **fixed** — outbox, flushed per tick | n/a (different mechanism) |
| D2 | `FlushFileBuffers` blocks the whole event loop | n/a | **LIVE — CRITICAL** |
| D3 | `respond()` ignores `written` | n/a | **fixed** — truncation is now reported |
| D4 | `poll()` drops the head of any request over 8 KiB | n/a | **LIVE** |
| D5 | one silent client monopolises the channel (pre-auth DoS) | **fixed** — bounded pool, `MAX_SLOTS = 8` | **LIVE** |
| D6 | client accepts an unbounded reply | **fixed** — `MAX_REPLY` + total deadline | **LIVE** |
| D7 | a truncated reply is returned as `Ok` | **fixed** — EOF before newline is an error | **LIVE** |
| D8 | endpoint hijack → token theft | **detected** — `(dev, ino)` tripwire | **LIVE, and undetectable** |
| D9 | `MAX_REQUEST` unchecked on the newline branch | **fixed** | capped at the 8 KiB buffer; no error reply |

Windows keeps six of the nine.

## The trap in D2 — read this before "just deleting the flush"

The obvious fix is to delete the `FlushFileBuffers` call. Do not, on its own.

`respond()` writes, flushes, then calls `recycle()`, and `DisconnectNamedPipe`
**discards data the client has not read**. The blocking flush is what guarantees
the reply landed before the disconnect. Delete it alone and the hang becomes
silent reply loss — a worse defect, and a much quieter one.

Removing the flush requires deferring the disconnect: that is the
`Phase::Writing` / `Phase::Draining` state machine in the held commit. Both
halves, or neither. `windows_respond_may_not_drop_the_flush_without_deferring_the_disconnect`
guards this and is revert-checked against exactly that edit.

## What only a Windows box can answer

1. What a non-blocking `ReadFile` on a **server** handle returns while the client
   is still reading, versus after it has closed: `ERROR_NO_DATA`,
   `ERROR_BROKEN_PIPE`, or success-with-zero. `winmap::read_outcome` encodes an
   answer; it has never been checked against the OS.
2. Whether byte mode behaves as documented under `PIPE_NOWAIT`. The held commit
   drops message mode entirely on the strength of the documentation.
3. Whether removing `FlushFileBuffers` leaves buffered data intact until the
   client drains, given the deferred disconnect.

## The guards that will speak up when this work lands

Three tests in `src/ipc.rs` are pinned to the *held* state and will fail the day
the rewrite arrives, each naming what to restore:

- `windows_respond_may_not_drop_the_flush_without_deferring_the_disconnect`
- `the_transport_budgets_live_in_one_place` — its `("windows", ...)` row was
  removed; put it back
- `windows_replica_gives_up_at_once_on_a_reader_that_hung_up` — its anchor is
  inverted; restore the real one

`wire`, `chan` and `winmap` are already in the tree, platform-independent and
tested on any host. The held work is the Windows `sys` module that consumes them.
