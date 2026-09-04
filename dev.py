#!/usr/bin/env python3
"""Local dev runner for amux — stdlib Python driving cargo, no task runner.

The same `check` gate as every nativelite package, so the muscle memory is
identical across languages. Here `check` is the zero-dependency guard, then
`cargo fmt --check`, then `cargo test` (unit + integration + doctests). Invoke it
however your platform
spells Python — `python` is Windows-only, macOS/Linux ship `python3`, and the
shebang + exec bit make `./dev.py` work everywhere:

  ./dev.py check      (macOS/Linux)   |   python dev.py check      (Windows)
  ./dev.py test       # cargo test
  ./dev.py build      # cargo build --release
  ./dev.py fmt        # cargo fmt --check
  ./dev.py guard      # zero-dependency guard
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PY = sys.executable


def run(*args: str) -> int:
    print(f"$ {' '.join(args)}")
    return subprocess.call(args, cwd=str(ROOT))


def test() -> int:
    return run("cargo", "test", "--all-targets") or run("cargo", "test", "--doc")


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
