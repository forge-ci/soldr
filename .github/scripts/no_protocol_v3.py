#!/usr/bin/env python3
"""Fail if a `protocol_v3` / `client_v3` broker module ever appears (soldr#2360/#2363).

The broker-fronted daemon design (soldr#2361 Phase 1, soldr#2363) is
deliberate about *not* introducing a new wire major: the running-process
broker protocol stays `protocol_v2` / `client_v2`, and breaking wire changes
are made in place on v2. Integrators are forced to upgrade via the
minimum-version floor (`ServiceDefinition.min_version`, enforced by
running-process's Hello handler) refusing below-floor peers at connect --
never via a parallel v3 module that would let old and new wires coexist.

This is a guard against reintroducing exactly the thing the design ruled
out: a `protocol_v3` or `client_v3` module in soldr would mean two wire majors live side
by side, which defeats the whole point of the floor -- so this check fails
loudly rather than let it land unnoticed in a large PR.

Usage:
    no_protocol_v3.py [--roots crates]
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

DEFAULT_ROOTS = ("crates",)

# Matches a module path, type, or identifier segment named exactly
# `protocol_v3` or `client_v3` (case-sensitive, word-bounded so
# `protocol_v30` or `my_client_v3x` don't false-positive).
PATTERN = re.compile(r"\b(protocol_v3|client_v3)\b")

# Own docstring/comments in this file and the tracking issues reference the
# banned names in prose; those don't count as a violation.
SELF = Path(__file__).resolve()


@dataclass(frozen=True)
class Finding:
    """One banned-name occurrence: repo-relative file, 1-based line, its text."""

    path: Path
    lineno: int
    line: str


def scan(roots: tuple[str, ...], repo_root: Path) -> list[Finding]:
    findings: list[Finding] = []
    for root_str in roots:
        root = repo_root / root_str
        if not root.is_dir():
            continue
        for path in sorted(root.rglob("*.rs")):
            try:
                text = path.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError):
                continue
            for lineno, line in enumerate(text.splitlines(), start=1):
                if PATTERN.search(line):
                    findings.append(
                        Finding(path.relative_to(repo_root), lineno, line.strip())
                    )
    return findings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--roots", nargs="+", default=list(DEFAULT_ROOTS))
    parser.add_argument("--repo-root", default=".")
    args = parser.parse_args(argv)

    repo_root = Path(args.repo_root).resolve()
    findings = scan(tuple(args.roots), repo_root)

    if not findings:
        print(f"no_protocol_v3: clean ({', '.join(args.roots)} checked).")
        return 0

    print("no_protocol_v3: FAIL", file=sys.stderr)
    for finding in findings:
        print(f"  - {finding.path}:{finding.lineno}: {finding.line}", file=sys.stderr)
    print(
        "\nThe broker wire stays protocol_v2 / client_v2, broken in place -- "
        "see soldr#2360 and soldr#2363. A protocol_v3/client_v3 module means "
        "two wire majors coexisting, which defeats the minimum-version floor. "
        "If this is a deliberate, reviewed policy change, update this script's "
        "PATTERN alongside the design docs rather than routing around it.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
