# Read this first — you are the Windows half of a split job

You are working on `amux`, a terminal multiplexer. Nobody expects you to know
the rest of the codebase. This file is self-contained and tells you exactly what
to do.

You are on branch **`windows/ipc-rewrite`**.

## The one thing that matters

`src/ipc.rs` on this branch contains a rewrite of the Windows named-pipe control
channel. **Not one line of it has ever run on Windows.** It was written on a mac,
against Microsoft's documentation, and the only thing anyone has proved about it
is that `cargo clippy --target x86_64-pc-windows-msvc` is clean.

Type-checking is not evidence about how a named pipe behaves.

Your job is to find out whether it actually works. Treat every claim in the code
comments as a hypothesis written by someone who could not test it.

## Background, briefly

`amux ctl <cmd>` talks to a running amux over a local IPC channel — a unix
socket on macOS/Linux, a named pipe on Windows. amux's main loop is a single
thread on a 15 ms tick that drives every pane. **The control channel must never
block that thread.** If it does, every pane in every window freezes.

An audit found nine defects. The unix fixes landed on `main`. The Windows fixes
are this branch, held back because they could not be executed.

## Defect status on Windows

| # | Defect | On `main` today | On this branch |
|---|---|---|---|
| D2 | `FlushFileBuffers` blocks the whole event loop | **LIVE, CRITICAL** | claimed fixed |
| D3 | `respond()` ignores `written` | fixed on main | fixed |
| D4 | `poll()` drops the head of any request over 8 KiB | **LIVE** | claimed fixed |
| D5 | one silent client monopolises the channel (pre-auth DoS) | **LIVE** | claimed fixed |
| D6 | client accepts an unbounded reply | **LIVE** | claimed fixed |
| D7 | a truncated reply is returned as `Ok` | **LIVE** | claimed fixed |
| D8 | endpoint hijack → token theft | **LIVE, undetectable** | partially mitigated |

"Claimed" means: written, type-checks, never executed.

## Step 1 — does it even build and run

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Report the numbers. If tests hang, say which — a hang here is itself a finding,
because a blocked event loop is the defect class we are chasing.

## Step 2 — the three questions only your machine can answer

These are the reason this branch exists. Each one is a guess encoded in
`src/ipc.rs`; your job is to replace the guess with a measurement. Write each as
a small standalone probe (a `tests/` file or a `src/bin/` scratch binary is
fine) and **report the observed values**, not your reading of the docs.

### Q1 — what a non-blocking `ReadFile` returns on a *server* handle

`mod winmap` maps Win32 `(ok, n, GetLastError())` triples to decisions. It is a
pure decision table with tests that pass on a mac, which proves only that the
table is self-consistent.

Create a named pipe with `PIPE_NOWAIT`, connect a client, then call `ReadFile`
on the **server** handle in three states, and record `(ok, n, err)` for each:

1. client connected, has sent nothing
2. client connected, has sent a partial line (no `\n` yet)
3. client has closed its handle

Expected candidates are `ERROR_NO_DATA` (232), `ERROR_BROKEN_PIPE` (109), or
success-with-zero. Then check `winmap::read_outcome` maps each observed triple
the way the code assumes.

### Q2 — does byte mode behave as documented under `PIPE_NOWAIT`

This branch drops `PIPE_TYPE_MESSAGE`/`PIPE_READMODE_MESSAGE` for
`PIPE_TYPE_BYTE`/`PIPE_READMODE_BYTE`, purely on the strength of the docs. That
is the single largest untested change here.

Send a request larger than the 8 KiB read buffer — 20 KB is the realistic case
(`amux ctl send <20 KB payload>`). Confirm byte mode delivers it as a stream the
framer reassembles, with **no `ERROR_MORE_DATA` and no lost head**. On `main`'s
message mode the head is discarded and the run loop is handed a fragment that
begins mid-payload; that is D4.

### Q3 — is data safe after removing `FlushFileBuffers`

**This is the dangerous one. Read it carefully.**

On `main`, `respond()` writes, calls `FlushFileBuffers`, then calls `recycle()`,
which calls `DisconnectNamedPipe`. `DisconnectNamedPipe` **discards data the
client has not read**. The blocking flush is what guarantees the reply landed
before the pipe drops. That is why D2 could not simply be deleted on the mac
side — deleting the flush alone turns a hang into silent reply loss.

This branch removes the flush and instead defers the disconnect through a
`Phase::Writing` / `Phase::Draining` state machine.

Verify **both** halves:

1. The reply still arrives intact when the client reads slowly.
2. The event loop is no longer blocked: a client that writes a request and never
   reads the reply must not freeze anything. On `main` this hangs every pane.

If the reply is ever truncated or lost, this branch is worse than `main` and you
must say so plainly.

## Step 3 — demonstrate each defect

For every defect you confirm fixed, produce a test that **fails on `main` and
passes here**, and record the actual failure text from the `main` run. Do not
skip the `main` run. A test that passes in both places proves nothing, and this
repo has shipped several of those.

Collect them in `tests/win_ipc_verify.rs`.

## House rules — these are hard constraints

- **Zero third-party dependencies.** stdlib plus the `nativelite` crates only.
  Raw `extern "system"` / `extern "C"` declarations are the normal way here.
  `python3 tools/dep_guard.py` enforces this.
- **Never block the 15 ms event loop.** Not for a flush, not for a lock, not for
  a syscall that can wait on a peer.
- **Do not ship a hand-rolled MAC.** An earlier audit suggested keying
  `std::hash::DefaultHasher` to authenticate the endpoint. That is not a
  cryptographic MAC. If D8 needs authentication, that is a separate decision, not
  something to improvise.
- **Executed proof over reasoning.** Mark anything you did not actually run as
  unexecuted. Saying "I could not test this" is a good answer; saying "this
  should work" as though it were a result is not.

## What to send back

1. The gate output from Step 1.
2. The three observed answers from Step 2, as raw `(ok, n, err)` values and
   observed behaviour — not conclusions.
3. `tests/win_ipc_verify.rs`, with the `main` failure text for each test.
4. A plain list of anything in this branch you believe is wrong. It was written
   blind; finding it wrong is a success, not a problem.

## Context files in this repo

- `WINDOWS-HELD.md` — why this was split off and what the mac side shipped
- `CHANGELOG.md` — recent work on the control channel
- `README.md` — what amux is

Branch `fix/ipc-unix` is the unix half that landed. `main` is the pre-fix
baseline you will be comparing against.
