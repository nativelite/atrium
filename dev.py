#!/usr/bin/env python3
"""Local dev runner for atrium — stdlib Python driving cargo, no task runner.

The same `check` gate as every nativelite package, so the muscle memory is
identical across languages. Here `check` is the zero-dependency guard, then the
drift check (generated docs and the marketplace's copy of the skills), then
`cargo fmt --check`, then clippy and the MSRV build, then `cargo test` (unit +
integration + doctests). Invoke it
however your platform
spells Python — `python` is Windows-only, macOS/Linux ship `python3`, and the
shebang + exec bit make `./dev.py` work everywhere:

  ./dev.py check      (macOS/Linux)   |   python dev.py check      (Windows)
  ./dev.py test       # cargo test, under a wall-clock budget
  ./dev.py build      # cargo build --release
  ./dev.py fmt        # cargo fmt --check
  ./dev.py lint       # clippy -D warnings (host + unix targets), MSRV build
  ./dev.py guard      # zero-dependency guard
  ./dev.py drift      # docs/*.html match docs/*.md; skills match the plugin's copy

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

#: The sibling crate atrium depends on by path (the bus/board coordination core).
#: `cargo test`/`cargo fmt` in this package do NOT descend into a path dependency,
#: so without folding it into the gate here an abus regression — the bus size cap
#: or the topic-admission policy — would sail through untested and unformatted.
#: Skipped gracefully if the sibling isn't present (e.g. a vendored checkout that
#: only ships atrium).
ABUS_MANIFEST = ROOT.parent / "abus" / "Cargo.toml"

#: The Claude Code skills that teach a hosted agent to use `ctl` and fleets.
#: `skills/` here is the **source of truth**; the marketplace plugin ships a
#: copy, and nothing but the gate compares them. They diverged once already:
#: `atrium-fleet` lived only in the marketplace for five plugin versions while
#: this repo's README promised a hand-copy path, so anyone following the docs
#: ran fleets with a skill half the size of the real one.
SKILLS = ROOT / "skills"
MARKETPLACE_SKILLS = ROOT.parent / "marketplace" / "atrium" / "skills"

#: Wall-clock budget for one `cargo test` invocation. Generous on purpose: this
#: is a backstop against a deadlock, not a performance assertion (the suite runs
#: in ~15 s), so raising it on a slow or cold machine costs nothing.
TEST_TIMEOUT = int(os.environ.get("ATRIUM_TEST_TIMEOUT", "300"))

#: The exit code `timeout(1)` uses, so a caller can tell a hang from a failure.
TIMED_OUT = 124

#: How many tests run at once. Each end-to-end test is a whole atrium — a pty, a
#: shell, sometimes a fleet of them — so libtest's default of one thread per core
#: oversubscribes the machine several times over, and a test that waits 15 s for
#: a pane to answer can miss it. Measured on a 16-core machine: at the default,
#: about one run in five failed, each time on a *different* test (the shape of a
#: starved machine, not of a bad test); at half the cores, eight runs in a row
#: passed, for about 4 s more. Not a performance knob — a correctness one.
TEST_THREADS = os.environ.get("ATRIUM_TEST_THREADS") or str(max(2, (os.cpu_count() or 4) // 2))


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
    # Drop any libtest args the caller already passed (`--test-threads`), so the
    # serial re-run is the only thing after the `--`.
    cargo_args = list(args)
    if "--" in cargo_args:
        cargo_args = cargo_args[: cargo_args.index("--")]
    serial = cargo_args + ["--", "--test-threads=1"]
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
    print(f"# {TEST_THREADS} tests at once (ATRIUM_TEST_THREADS)")
    # Only the binary targets need the cap (see TEST_THREADS); doctests are cheap.
    threads = ("--", f"--test-threads={TEST_THREADS}")
    invocations = [
        ("cargo", "test", "--all-targets", *threads),
        ("cargo", "test", "--doc"),
    ]
    # Fold in the sibling abus crate so its coordination tests are part of the
    # gate, not a manual afterthought (see ABUS_MANIFEST).
    if ABUS_MANIFEST.exists():
        mp = ("--manifest-path", str(ABUS_MANIFEST))
        invocations += [
            ("cargo", "test", *mp, "--all-targets", *threads),
            ("cargo", "test", *mp, "--doc"),
        ]
    for args in invocations:
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
    code = run("cargo", "fmt", "--check")
    if code or not ABUS_MANIFEST.exists():
        return code
    # The sibling crate is formatted by the same gate; a drift in abus is a gate
    # failure here, not something only a hand-run would catch.
    return run("cargo", "fmt", "--check", "--manifest-path", str(ABUS_MANIFEST))


#: Cross targets clippy also runs for, when installed. The gate runs on Windows,
#: where every `cfg(unix)` line is compiled out: unix code was never linted, and
#: the only unix check was a hand-run `cargo check --target`.
LINT_TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-apple-darwin")


def _installed_targets() -> set[str]:
    try:
        out = subprocess.run(
            ["rustup", "target", "list", "--installed"],
            capture_output=True, text=True, check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError):
        return set()
    return set(out.split())


def _msrv() -> str | None:
    import re
    m = re.search(r'^rust-version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.M)
    return m.group(1) if m else None


def lint() -> int:
    """clippy with warnings as errors, then a build on the declared MSRV.

    Neither ran anywhere before: 17 clippy warnings built up unseen, and a
    `div_ceil` (Rust 1.73) shipped in releases that declared `rust-version =
    1.70` - `cargo install` failed on Windows on 1.70-1.72 for every release
    from 0.33.0 to 0.37.2.
    A missing target or toolchain is skipped out loud, never silently passed.
    """
    code = run("cargo", "clippy", "--all-targets", "--", "-D", "warnings")
    if code:
        return code
    installed = _installed_targets()
    for target in LINT_TARGETS:
        if target not in installed:
            print(f"lint: target {target} not installed, skipping its clippy")
            continue
        code = run("cargo", "clippy", "--all-targets", "--target", target, "--", "-D", "warnings")
        if code:
            return code
    msrv = _msrv()
    if msrv is None:
        return 0
    try:
        have = subprocess.run(
            ["rustup", "run", msrv, "rustc", "--version"], capture_output=True
        ).returncode == 0
    except OSError:
        have = False
    if not have:
        print(f"lint: toolchain {msrv} not installed (rustup toolchain install {msrv}), skipping the MSRV build")
        return 0
    return run("cargo", f"+{msrv}", "check", "--all-targets", "--locked")


def guard() -> int:
    return run(PY, "tools/dep_guard.py")


def _skills_drift() -> int:
    """Check that the marketplace's copy of the skills matches this repo's.

    `skills/` here is the source of truth; the marketplace plugin is a
    distribution copy (`nativelite/playbooks/release.md`). Skipped when the
    sibling checkout isn't there, exactly like the abus fold-in above.
    """
    if not MARKETPLACE_SKILLS.is_dir():
        print(f"skills: no marketplace checkout at {MARKETPLACE_SKILLS}, skipping")
        return 0

    def tree(root: Path) -> dict[str, str]:
        # Text compared with newlines normalized: the two repos are checked out
        # separately and git may hand one of them CRLF.
        return {
            str(p.relative_to(root)).replace("\\", "/"): p.read_text(
                encoding="utf-8"
            ).replace("\r\n", "\n")
            for p in sorted(root.rglob("*"))
            if p.is_file()
        }

    ours, theirs = tree(SKILLS), tree(MARKETPLACE_SKILLS)
    missing = sorted(set(ours) - set(theirs))
    extra = sorted(set(theirs) - set(ours))
    changed = sorted(f for f in set(ours) & set(theirs) if ours[f] != theirs[f])
    if not (missing or extra or changed):
        print(f"skills OK: {len(ours)} files match the marketplace copy.")
        return 0
    print("SKILLS DRIFT between skills/ and the marketplace plugin:")
    for f in missing:
        print(f"  only here:        {f}")
    for f in extra:
        print(f"  only marketplace: {f}")
    for f in changed:
        print(f"  differs:          {f}")
    print(f"  fix: copy skills/ over {MARKETPLACE_SKILLS} and bump plugin.json")
    return 1


def drift() -> int:
    """The two hand-synced copies nothing else checks.

    Both drifted unnoticed: `atrium-fleet` shipped only in the marketplace
    while the README promised a hand-copy path from this repo, and
    `fleets.html` sat a whole release behind `fleets.md`. Neither is code, so
    no test would ever have caught them.
    """
    return run(PY, "docs/build.py", "--check") or _skills_drift()


def check() -> int:
    # guard (cheap) → drift (cheap) → fmt (cheap, fail fast on style drift) →
    # lint (a compile, no run) → test (expensive).
    return guard() or drift() or fmt() or lint() or test()


COMMANDS = {
    "test": test,
    "build": build,
    "fmt": fmt,
    "lint": lint,
    "guard": guard,
    "drift": drift,
    "check": check,
}


def main(argv: list[str]) -> int:
    cmd = argv[1] if len(argv) > 1 else "check"
    fn = COMMANDS.get(cmd)
    if fn is None:
        print(f"unknown command {cmd!r}; choose from: {', '.join(COMMANDS)}")
        return 2
    return fn()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
