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

Feed cost no longer grows with the grid. The flood figure is the range over
five runs on a busy machine; `cat` measured 85-160 MB/s over the same runs, so
treat the remaining gap as roughly 3x, not a precise ratio. Key echo (p50 0.30-0.44
ms, p99 0.59-0.88 ms) and idle CPU (0.6-0.8%) are unchanged.
