# atrium benchmarks

```sh
cargo bench --bench terminal
```

This is a release build. Under `cargo test` the bench target only builds, so the
local gate stays fast. Knobs:
- `ATRIUM_BENCH_SAMPLES` (default 100)
- `ATRIUM_BENCH_IDLE_S` (default 10)
- `ATRIUM_BENCH_FLOOD_MB` (default 16)
- `ATRIUM_BENCH_KEY_GAP_MS` (default 30)
- `ATRIUM_BENCH_ONLY` (`latency`, `idle`, `feed` or `flood`)

On Windows the key gap decides what the latency numbers measure. Every write to
the console host opens a ~16 ms frame window, and a key that echoes inside one
waits for it to close. At the default 30 ms — faster than anyone types — a
terminal is never clear of its own last frame, so the p90 is the console's
cadence, not atrium's. Use 120 ms for what a person feels.

## What `terminal` measures

- **Setup:** atrium, or the bare shell it hosts, runs on a real pty. The harness
  acts as the terminal and timestamps output **where it arrives**, on a reader
  thread. A harness that polls with sleeps inflates latency: an earlier one-off
  read a bare shell at 1.6 ms that was really 0.1 ms.
- **Key echo:** time from writing a key to that key echoing back.
- **Idle CPU:** the process's own CPU time over a quiet window, as % of one core.
- **Emulator feed:** the same log fed straight into one `vterm::Term`, in
  process, with no pty or rendering. It isolates the emulator's share of the
  flood cost, and it's where the flood bottleneck turned out to live.
- **Flood:** a pane prints a deterministic, coloured, agent-style log. Time from
  launch until every pane exits. The pane blocks when atrium falls behind, so on
  Linux/macOS this is atrium's end-to-end consumption rate.
  - **Windows caveat:** the console host behind each pane turns output into
    screen updates and skips frames, so atrium never sees most of the bytes. The
    Windows flood numbers mostly measure the console host; use Linux for
    atrium's own throughput.

## Baseline: atrium 0.34.0 (2026-09-15)

Machine: the founder's desktop, 16 logical cores, 64 GB RAM, Windows 11. The
Linux numbers are WSL Ubuntu on the same machine. Each run is 100 key echoes per
scenario, a 10 s idle window, and 16 MB per pane, run once. Treat single-digit
percentage differences as noise until repeated.

### Windows

| scenario | echo p50 ms | p90 ms | p99 ms | max ms | idle CPU % | flood MB/s |
| --- | --- | --- | --- | --- | --- | --- |
| bare cmd | 0.10 | 0.16 | 0.24 | 0.30 | 0.00 | 347.1 |
| atrium, 1 pane | 0.32 | 0.53 | 16.05 | 16.08 | 0.94 | 102.1 |
| atrium, 8 tiled panes | 0.53 | 15.96 | 16.17 | 16.48 | 1.56 | 315.6 |

### Linux (WSL)

| scenario | echo p50 ms | p90 ms | p99 ms | max ms | idle CPU % | flood MB/s |
| --- | --- | --- | --- | --- | --- | --- |
| bare sh / cat | 0.12 | 0.15 | 0.20 | 0.43 | 0.00 | 99.5 |
| atrium, 1 pane | 0.30 | 0.49 | 0.59 | 0.61 | 0.70 | 10.6 |
| atrium, 8 tiled panes | 0.68 | 0.81 | 0.89 | 0.90 | 0.80 | 12.9 (total) |

What the baseline shows:
- **Windows ~16 ms outliers** (p99 with 1 pane, p90 with 8) don't occur on
  Linux. Cause found and fixed — see below.
- **Flood throughput is atrium's bottleneck.** On Linux a pane's output goes
  through atrium about 9x slower than `cat` alone (10.6 vs 99.5 MB/s). With 8
  panes that's about 1.6 MB/s per pane. Fixed since — see below.

## Since the baseline: the emulator (0.35.0)

The flood bottleneck was the emulator's scrolling, not atrium's loop. Two
changes in `nativelite-ansi` — a block move for a region scroll, then a row
ring so scrolling the whole screen only advances an origin — moved the Linux
numbers (WSL, same machine):

| measurement | 0.34.0 | block move | row ring |
| --- | --- | --- | --- |
| feed, 24x80 | 16.1 | 53.5 | 70.2 MB/s |
| feed, 40x160 | 11.0 | 27.4 | 66.0 MB/s |
| feed, 60x240 | 5.3 | 18.8 | 63.8 MB/s |
| flood, 1 pane | 10.6 | — | 39-51 MB/s |
| flood, 8 panes | 12.9 | — | 47.0 MB/s total |

### The Windows ~16 ms echo stalls (0.35.0)

They were atrium's own writes. The console host turns each write into a ~16 ms
frame, and a key echoing inside that frame waits for it. A probe (a pty inside a
pty, no atrium — a relay that only copies bytes) pinned it down:

| relay behaviour | echo p50 | p99 | max | keys ≥ 8 ms |
| --- | --- | --- | --- | --- |
| silent unless echoing | 0.06 | 0.17 | 0.29 | 0 / 150 |
| repaints a bar every 100 ms | 0.09 | 8.66 | 13.12 | 3 / 150 |
| repaints a bar every 20 ms | 12.12 | 14.14 | 14.93 | 136 / 150 |

The hop count made no difference; a *periodic write with nothing to say* made all
of it. atrium was repainting an unchanged bar twice a second. Fixed by writing
nothing while idle, repairing the bar only after a pane printed and went quiet,
and coalescing frames queued together into one write. Windows, keys 120 ms apart:

| scenario | p50 ms | p90 ms | p99 ms | max ms |
| --- | --- | --- | --- | --- |
| bare cmd | 0.12 | 0.25 | 0.56 | 0.74 |
| atrium, 1 pane | 0.34 | 0.54 | 1.58 | 1.75 |
| atrium, 8 tiled panes | 0.80 | 1.74 | 2.38 | 4.10 |

At the default 30 ms gap the stalls still show, for both atrium and a bare
relay: below about one frame per key, the console host's cadence is the floor.

### Where the flood gap went (0.35.1)

A second emulator change — blitting runs of plain ASCII instead of writing cell
by cell (`nativelite-vterm` 0.5.2) — took the feed to **173 MB/s** at
40x160 and atrium's flood to **73.7-76.6 MB/s** against 135-139 MB/s for `cat`:
a 1.8x gap, from 9x at the 0.34.0 baseline.

That remaining gap is now explained, and it is not overhead. atrium does
everything `cat` does *and* runs the bytes through the emulator, one after the
other: 1/173 + 1/137 predicts 76 MB/s, which is what it measures. Going faster
means making the emulator cheaper or overlapping the emulator with the write
instead of doing them in series. (When this was written, parsing was 81% of
the emulator's cost, with a `String` per text run and two `Vec`s per CSI;
`nativelite-ansi` 0.4 since moved to borrowed events with no allocation per
token, and the feed figures below reflect that.) Nothing in the pipeline is
being wasted today.

Feed cost no longer grows with the grid. The flood figure is the range over
five runs on a busy machine; `cat` measured 85-160 MB/s over the same runs, so
treat the remaining gap as roughly 3x, not a precise ratio. Key echo (p50 0.30-0.44
ms, p99 0.59-0.88 ms) and idle CPU (0.6-0.8%) are unchanged.

## 0.42.0 (2026-11-08), Windows

Measured on the release tree at `810eaa6` after ctl requests stopped forcing a
full repaint (8011dd9). Same machine class as the baseline; one run, so treat
the figures as a snapshot, not a trend.

| scenario | echo p50 ms | p90 ms | p99 ms | max ms | idle CPU % | flood MB/s |
| --- | --- | --- | --- | --- | --- | --- |
| bare cmd | 0.17 | 0.28 | 0.62 | 0.62 | 0.00 | 386 |
| atrium, 1 pane | 0.36 | 1.10 | 4.02 | 15.87 | 0.93 | 94 |
| atrium, 8 tiled panes | 1.31 | 15.67 | 16.13 | 16.33 | 1.71 | 483 (total) |

Emulator feed, in process: 569 MB/s at 24x80, 807 MB/s at 40x160, 803 MB/s at
60x240 (was 173 MB/s at 40x160 in 0.35.1).

What this does and does not show:

- The 8-pane p90 sits on the Windows console host's ~16 ms frame, as the
  0.35.0 notes describe; the Windows flood number mostly measures that host.
  Use the Linux rows for atrium's own throughput.
- The repaint fix was not exercised by this run; the ctl-polls scenario below
  was added for it.

## The ctl-polls scenario (0.43.0, 2026-11-08), Windows

`atrium, 8 tiled panes + ctl polls` is the eight-pane scenario under
`--allow-ctl` with a poller issuing a token-free `ctl list` five times a
second for the whole measurement: what eight modded panes do with their
1.5 s `inbox` polls. The poller finds the endpoint by the atrium's pid
(`atrium::ipc::address_for`), as a host terminal would. Until 8011dd9 every
such request reset the renderer and forced a full `2J` repaint; this row is
the before/after of that change, same machine, same harness, one run each.
`ATRIUM_BENCH_BIN=<path>` points the harness at another atrium, which is how
the "before" binary (a release build of 50717aa) was measured.

| atrium | scenario | echo p50 ms | p90 ms | p99 ms | idle CPU % |
| --- | --- | --- | --- | --- | --- |
| 50717aa (before) | 8 tiled panes | 0.62 | 15.87 | 16.47 | 1.25 |
| 50717aa (before) | 8 tiled panes + ctl polls | 1.53 | 15.97 | 16.18 | 1.40 |
| 0.43.0 (after) | 8 tiled panes | 0.74 | 15.93 | 16.32 | 1.25 |
| 0.43.0 (after) | 8 tiled panes + ctl polls | 1.16 | 15.81 | 16.47 | 0.94 |

What it shows, and what it cannot:

- Under polls, the median key echo fell from 1.53 to 1.16 ms and idle CPU
  from 1.40 to 0.94 % of a core; without polls the two builds are within
  noise of each other, as they should be.
- The p90 and p99 sit on the Windows console host's ~16 ms frame in every
  row, so the frame the fix saves per poll does not show in those columns;
  what a user saw before the fix was a visible flash of every tiled pane
  several times a second, which no latency column measures. A Linux run has
  no console frame and would show the saved work directly; not run here.
- One run each on a busy machine: treat the deltas as direction, not size.

## Soak: the held inbox and the waking channel (unreleased, 2026-11-09)

Not a bench but an ignored e2e, `tests/atrium/soak.rs`, kept beside the
numbers it produces:

```sh
ATRIUM_SOAK_SECS=600 cargo test --test atrium soak -- --ignored --nocapture
python dev.py soak      # the same, ATRIUM_SOAK_SECS default 60
```

atrium `--allow-ctl -n 8 <shell>` runs in a pty. Every pane registers as a
modded pane (`hello mod=0.1.0 caps=status,inbox`) and holds an `inbox --wait`
open from the test process with that pane's token (`ATRIUM_TOKEN`, which each
pane's shell writes to a file as it starts), so eight held connections sit in
the run loop's `serve_held_inboxes` for the whole run; the lead (pane 0) sends
a numbered marker to a random pane twice a second, and a token-free host asks
`list` five times a second by pid (`atrium::ipc::address_for`), the way the
ctl-polls scenario does. Every 5 s the atrium process's working set and
handle count (open fds on Linux) are sampled. The run fails on RSS or handle
growth over 20 % from the minute-1 sample to the end, a marker not received
within 10 s of its send (or a refused send), a host request over 5 s or
refused, or a final `list` slower than 1 s.

Both runs below are 120 s on the founder's desktop (16 cores, 64 GB; the
Linux row is WSL Ubuntu on the same machine), one run each, with the tree at
the commit that added the harness. "Baseline" is the 60 s sample. Delivery
is the time from the lead's `send` request to the marker arriving in the
waiter's held reply.

| metric | Windows | Linux (WSL) |
| --- | --- | --- |
| rss MB start / baseline / end | 11.4 / 12.0 / 12.3 (+2.5 %) | 7.8 / 8.1 / 8.3 (+2.6 %) |
| handles (fds) start / baseline / end | 167 / 175 / 168 (-4.0 %) | 32 / 33 / 25 (-24.2 %) |
| host `list` requests (refused or failed) | 591 (0) | 584 (0) |
| host p50 / p99 / max ms | 2.54 / 10.43 / 39.19 | 2.14 / 142.65 / 329.51 |
| sends / received / late or lost / refused | 239 / 239 / 0 / 0 | 238 / 238 / 0 / 0 |
| delivery p50 / p99 / max ms | 1.5 / 10.6 / 24.5 | 1.3 / 32.5 / 313.9 |
| waiter replies / expired holds / errors | 247 / 0 / 0 | 246 / 0 / 0 |
| final `list` ms | 2.32 | 2.30 |

What it shows, and what it cannot:

- No leak or stall at this length: every marker reached its waiting pane,
  within the run loop's tick on the median, and the working set at the end
  is within noise of the minute-1 sample on both platforms. The eight held
  connections are in the start and baseline handle counts; the end sample is
  taken after the lead's `soak-stop` wakes have released the waiters, which
  is the drop of about eight (168 from 175; 25 from 33), so a hold costs one
  handle or fd and gives it back. The waiter replies are the markers plus
  those eight wakes; no hold reached `HOLD_DEADLINE`.
- Two minutes is a smoke of the harness, not the soak it is for; the number
  that matters is the 600 s run, which `ATRIUM_SOAK_SECS=600` gives. A growth
  under 20 % over two minutes says little about a slow leak.
- The Windows host max (39 ms) is the console host's frame, not the channel:
  the same ~16 ms stalls the latency rows above sit on, twice over. The
  Linux tail is wider: a host p99 of 143 ms and a worst delivery of 314 ms
  against medians of 2 and 1.3 ms, under WSL with eight interactive `sh`
  panes repainting. Well inside the 5 s and 10 s budgets, and one run, so
  noted rather than chased; a 600 s run on native Linux would say whether it
  is the channel or the host.
