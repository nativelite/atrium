#!/usr/bin/env python3
"""Local dev runner for atrium — stdlib Python driving cargo, no task runner.

The same `check` gate as every nativelite package, so the muscle memory is
identical across languages. Here `check` is the zero-dependency guard, then
`cargo fmt --check`, then `cargo test` (unit + integration + doctests). Invoke it
however your platform
spells Python — `python` is Windows-only, macOS/Linux ship `python3`, and the
shebang + exec bit make `./dev.py` work everywhere:

  ./dev.py check      (macOS/Linux)   |   python dev.py check      (Windows)
  ./dev.py test       # cargo test, under a wall-clock budget
  ./dev.py build      # cargo build --release
  ./dev.py fmt        # cargo fmt --check
  ./dev.py guard      # zero-dependency guard

Test runs are bounded. `cargo test` has no per-test timeout and this project has
no CI to kill a stuck job, so a test that deadlocks used to wedge the machine
that ran it — silently, since a hang produces no failing line. Every test
invocation now gets `ATRIUM_TEST_TIMEOUT` seconds (default 300); when it expires
the whole process tree is killed and the run is repeated serially to name the
test that hung:

  ATRIUM_TEST_TIMEOUT=600 ./dev.py check    # a slow machine, or a cold build
"""
from __future__ import annotations

import os
import signal
import subprocess
import sys
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PY = sys.executable
WINDOWS = os.name == "nt"

#: Wall-clock budget for one `cargo test` invocation. Generous on purpose: this
#: is a backstop against a deadlock, not a performance assertion (the suite runs
#: in ~15 s), so raising it on a slow or cold machine costs nothing.
TEST_TIMEOUT = int(os.environ.get("ATRIUM_TEST_TIMEOUT", "300"))

#: The exit code `timeout(1)` uses, so a caller can tell a hang from a failure.
TIMED_OUT = 124


def _spawn(args: list[str], **kw) -> subprocess.Popen:
    """Start `args` in its own process group, so the whole tree can be killed.

    A hung test is usually hung *on a child* — a pane, a pty, an `atrium ctl`
    waiting on a reply that never comes. Killing cargo alone would leave those
    parked, which is the failure this runner exists to end.
    """
    if WINDOWS:
        kw["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kw["start_new_session"] = True
    return subprocess.Popen(args, cwd=str(ROOT), **kw)


def _kill_tree(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    if WINDOWS:
        subprocess.run(
            ["taskkill", "/T", "/F", "/PID", str(proc.pid)], capture_output=True
        )
    else:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            proc.kill()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        # A process stuck mid-exit cannot be reaped. Report it rather than
        # block: waiting forever here would reproduce the very hang we killed.
        print("!! the runner did not exit after SIGKILL; a child is stuck mid-exit")


def run(*args: str, timeout: int | None = None) -> int:
    print(f"$ {' '.join(args)}")
    proc = _spawn(list(args))
    try:
        return proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        _kill_tree(proc)
        print(f"\n!! TIMED OUT after {timeout}s: {' '.join(args)}")
        return TIMED_OUT
    except KeyboardInterrupt:
        # The child is in its own group, so the terminal's Ctrl-C never reached
        # it. Pass it on — to the whole tree, which is more than Ctrl-C did.
        _kill_tree(proc)
        raise


def _name_the_hang(args: tuple[str, ...], timeout: int) -> None:
    """Re-run serially to find out *which* test hung.

    In parallel mode libtest prints a test's name only once it finishes, so a
    hang leaves no trace of the culprit — which is why the last one cost an
    afternoon. With `--test-threads=1` the name is printed *before* the test
    runs, so the last unterminated line is the test that never came back.
    """
    serial = list(args) + ["--", "--test-threads=1"]
    print(f"\n$ {' '.join(serial)}   # locating the hang")
    proc = _spawn(serial, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    seen = bytearray()

    def pump() -> None:
        while True:
            # `read1`, not `readline`: the line that matters is the one with no
            # newline yet — `test foo ... ` with the result still pending.
            chunk = proc.stdout.read1(4096)
            if not chunk:
                return
            seen.extend(chunk)
            sys.stdout.write(chunk.decode("utf-8", "replace"))
            sys.stdout.flush()

    threading.Thread(target=pump, daemon=True).start()
    try:
        proc.wait(timeout=timeout)
        print(
            "\n!! the serial re-run did NOT hang — the hang is intermittent, or "
            "it only appears under parallelism (a shared file, port or pty)"
        )
        return
    except subprocess.TimeoutExpired:
        _kill_tree(proc)
    lines = seen.decode("utf-8", "replace").splitlines()
    last = lines[-1].strip() if lines else ""
    if last.startswith("test ") and last.endswith("..."):
        print(f"\n!! THE HANG IS: {last[len('test '):].rsplit(' ...', 1)[0]}")
    else:
        print(f"\n!! timed out again; last line was: {last!r}")


def test() -> int:
    print(f"# test budget: {TEST_TIMEOUT}s per invocation (ATRIUM_TEST_TIMEOUT)")
    for args in (("cargo", "test", "--all-targets"), ("cargo", "test", "--doc")):
        code = run(*args, timeout=TEST_TIMEOUT)
        if code == TIMED_OUT:
            _name_the_hang(args, TEST_TIMEOUT)
            return TIMED_OUT
        if code:
            return code
    return 0


def build() -> int:
    return run("cargo", "build", "--release")


def fmt() -> int:
    return run("cargo", "fmt", "--check")


def guard() -> int:
    return run(PY, "tools/dep_guard.py")


def check() -> int:
    # guard (cheap) → fmt (cheap, fail fast on style drift) → test (expensive).
    return guard() or fmt() or test()


COMMANDS = {"test": test, "build": build, "fmt": fmt, "guard": guard, "check": check}


def main(argv: list[str]) -> int:
    cmd = argv[1] if len(argv) > 1 else "check"
    fn = COMMANDS.get(cmd)
    if fn is None:
        print(f"unknown command {cmd!r}; choose from: {', '.join(COMMANDS)}")
        return 2
    return fn()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
