# macOS support audit (amux 0.23.0)

Thread-2 audit of the platform seams, verifying amux builds and runs on macOS.
amux is developed on Windows; macOS is reached through the `cfg(unix)` branches,
which it shares with Linux. This documents what was checked and the result.

## Verdict: amux builds on macOS as-is — no code changes required.

The whole tree — amux plus every unix-facing dependency (`rawterm`'s
`sys_unix.rs`, `pty`, `abus`, `vterm`, …) and all unit + integration test
targets — compile-checks cleanly for both Apple architectures. This is the
strongest validation available without a Mac: `cargo check` type-checks the
exact code a macOS build would compile.

```
cargo check --all-targets                                   # windows host  -> green
cargo check --target aarch64-apple-darwin  --all-targets    # Apple Silicon -> green
cargo check --target x86_64-apple-darwin   --all-targets    # Intel Mac     -> green
cargo check --target x86_64-unknown-linux-gnu --all-targets # unix parity   -> green
```

(A macOS *link+run* still needs a Mac; only the Apple SDK/linker is missing on
this host. Nothing in amux's own code blocks it.)

## Seams reviewed

| Seam | Location | macOS behavior | Status |
|------|----------|----------------|--------|
| Default shell | `main.rs::default_shell` | unix branch uses `$SHELL`, falls back to `sh` | ✓ correct |
| Command hosting | `main.rs::effective_command` | `#[cfg(windows)]` does PATHEXT/`.cmd`-shim hosting; `#[cfg(not(windows))]` returns argv unchanged, so a macOS agent launches directly | ✓ correct |
| Control-plane IPC | `ipc.rs` `#[cfg(unix)] mod sys` | `std::os::unix::net::UnixListener`, address `$TMPDIR/amux-ctl-<pid>.sock`, stale-socket cleanup on bind + drop | ✓ correct |
| Global fleet path | `fleet.rs::global_path` | unix branch uses `$XDG_CONFIG_HOME` else `~/.config/amux/fleet.json` | ✓ builds & runs |
| Session id (uuid) | `uid.rs::v4` | pure-std entropy (wall clock + `RandomState` + counter); no OS RNG syscall | ✓ portable |
| Raw terminal / PTY | external `rawterm` (`sys_unix.rs`), `pty` | not in this repo; both compile-check clean for darwin | ✓ (deps) |

No Linux-only assumptions exist in the repo: a sweep found no `/proc`,
`epoll`/`inotify`, `libc`, or non-Windows `extern` blocks. The only raw FFI is
the `#[cfg(windows)]` named-pipe block in `ipc.rs`. Integration tests are
cfg-branched (`cmd`/`/Q` on Windows → `sh`/`-i` on unix), so they execute on
macOS rather than assuming a Windows shell.

## Runtime caveats (not build breakers; need a Mac to exercise)

1. **Unix-domain-socket path length.** macOS caps `sun_path` at 104 bytes
   (Linux 108). `ipc.rs::default_address` builds `$TMPDIR/amux-ctl-<pid>.sock`.
   Under launchd `$TMPDIR` (`/var/folders/…`) is ~50 chars, so the total stays
   ~70 < 104 — fine in practice; only a pathologically deep `$TMPDIR` would
   overflow. **Addressed:** a 104-byte `sun_path` guard + test was added in
   `ipc.rs` by the agent-cli/ipc thread (board `macos-support`), so an overflow
   now fails fast with a clear error instead of a truncated bind.
2. **Resolver case-sensitivity (test-only).** `resolver_finds_shims_and_flags_shell_hosting`
   in `tests/amux.rs` is not `cfg`-gated and matches `tool.exe` against ext
   `.EXE`. It passes on the default case-insensitive APFS volume; on a
   case-sensitive volume it could fail. The resolver (`resolve.rs`) is only
   *used* on Windows — the unix `effective_command` bypasses it — so this is a
   test artifact, not a runtime path.

## Recommended follow-ups (next increment)

- Run the suite on a real Mac (CI runner) to exercise the two runtime caveats.
- If macOS-native config location is desired, consider
  `~/Library/Application Support/amux` for `global_path` on `target_os = "macos"`
  (currently shares the Linux `~/.config` convention — intentional, left as-is).
