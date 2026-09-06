#!/usr/bin/env python3
"""nativelite zero-dependency guard, app variant (stdlib Python only).

Apps compose nativelite's own crates, so ``[dependencies]`` entries are
allowed **iff** they are org crates: either a ``package = "nativelite-*"``
registry dep (with a version: the crates.io publish form) or a git dep on
``github.com/nativelite/``. Anything else (a third-party crates.io name, a
foreign git URL) fails. Build- and dev-dependency tables must be empty.
"""
from __future__ import annotations

import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ORG_PREFIX = "https://github.com/nativelite/"


def check_manifest() -> list[str]:
    data = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    problems: list[str] = []
    for name, spec in (data.get("dependencies") or {}).items():
        if not isinstance(spec, dict):
            problems.append(
                f"[dependencies] {name!r} must be a nativelite org crate, "
                f"found: {spec!r}"
            )
            continue
        git = spec.get("git")
        pkg = spec.get("package") or ""
        org_git = bool(git) and git.startswith(ORG_PREFIX)
        org_pkg = pkg.startswith("nativelite-") and bool(spec.get("version"))
        if not (org_git or org_pkg):
            problems.append(
                f"[dependencies] {name!r} must be a nativelite crate "
                f"(package = 'nativelite-*' with a version, or an org git dep); "
                f"found: {spec!r}"
            )
    for table in ("build-dependencies", "dev-dependencies"):
        if data.get(table):
            problems.append(f"[{table}] must be empty, found: {sorted(data[table])}")
        for tname, target in (data.get("target") or {}).items():
            if target.get(table):
                problems.append(f"[target.{tname}.{table}] must be empty")
    return problems


def main() -> int:
    problems = check_manifest()
    if problems:
        print("Dependency guard FAILED:")
        for p in problems:
            print(f"  - {p}")
        return 1
    print("Dependency guard OK: nativelite crates + stdlib only.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
