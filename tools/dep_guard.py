#!/usr/bin/env python3
"""nativelite zero-dependency guard, app variant (stdlib Python only).

Apps compose nativelite's own crates, so ``[dependencies]`` entries are
allowed **iff** they are git dependencies on ``github.com/nativelite/``
repos. Anything else — a crates.io name, a foreign git URL — fails. Build-
and dev-dependency tables must be empty, as for library packages.
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
        git = spec.get("git") if isinstance(spec, dict) else None
        if not git or not git.startswith(ORG_PREFIX):
            problems.append(
                f"[dependencies] {name!r} must be a git dependency on "
                f"{ORG_PREFIX}*, found: {spec!r}"
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
