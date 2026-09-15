# atrium benchmarks

```sh
cargo bench --bench terminal
```

This is a release build. Under `cargo test` the bench target only builds, so the
local gate stays fast. Knobs:
- `ATRIUM_BENCH_SAMPLES` (default 100)
- `ATRIUM_BENCH_IDLE_S` (default 10)
- `ATRIUM_BENCH_FLOOD_MB` (default 16)
- `ATRIUM_BENCH_ONLY` (`latency`, `idle` or `flood`)

## What `terminal` measures

- **Setup:** atrium, or the bare shell it hosts, runs on a real pty. The harness
  acts as the terminal and timestamps output **where it arrives**, on a reader
  thread. A harness that polls with sleeps inflates latency: an earlier one-off
  read a bare shell at 1.6 ms that was really 0.1 ms.
- **Key echo:** time from writing a key to that key echoing back.
- **Idle CPU:** the process's own CPU time over a quiet window, as % of one core.
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
  Linux. They come from the Windows console path, not from atrium's loop; under
  investigation.
- **Flood throughput is atrium's bottleneck.** On Linux a pane's output goes
  through atrium about 9x slower than `cat` alone (10.6 vs 99.5 MB/s). With 8
  panes that's about 1.6 MB/s per pane.
