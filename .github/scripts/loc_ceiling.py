#!/usr/bin/env python3
"""Hard per-file line ceiling for production Rust sources (issue #2493).

At issue completion no production Rust source file anywhere in the
workspace may exceed 1,000 physical lines. Unlike `loc_ratchet.py` (a
diff ratchet against the merge base), this is an absolute whole-tree
check: any violating file fails it, changed or not.

Scope: `crates/*/src/**/*.rs` — including `mod.rs` (index files are not
dumping grounds) and the soldr-platform index/facade files. Excluded:
generated sources (none live under src/), non-production test targets
(`crates/*/tests/**`), and unit-test-only modules (`tests.rs`,
`*_tests.rs`) which are non-production test code.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path

CEILING = 1000
ROOTS = ("crates",)
SUFFIX = ".rs"
EXCLUDED_NAMES = {"tests.rs"}


def production_sources() -> list[Path]:
    files: list[Path] = []
    for root in ROOTS:
        for path in Path(root).rglob("*" + SUFFIX):
            if not path.is_file():
                continue
            if "/src/" not in path.as_posix():
                continue
            if "/tests/" in path.as_posix():
                continue
            if path.name in EXCLUDED_NAMES or path.name.endswith("_tests.rs"):
                continue
            files.append(path)
    files.sort()
    return files


@dataclass(frozen=True)
class OverCeiling:
    """A production source file and its physical line count."""

    path: Path
    lines: int


def violations() -> list[OverCeiling]:
    over: list[OverCeiling] = []
    for path in production_sources():
        try:
            count = sum(1 for _ in path.read_text(encoding="utf-8").splitlines())
        except OSError:
            continue
        if count > CEILING:
            over.append(OverCeiling(path, count))
    over.sort(key=lambda item: (-item.lines, item.path.as_posix()))
    return over


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ceiling", type=int, default=CEILING)
    args = parser.parse_args()

    over = [item for item in violations() if item.lines > args.ceiling]
    if over:
        print(f"loc ceiling ({args.ceiling}) exceeded by {len(over)} file(s):")
        for item in sorted(over, key=lambda item: -item.lines):
            print(f"  {item.lines:5d} {item.path}")
        return 1
    print(f"loc ceiling ({args.ceiling}): all production files are within budget")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
