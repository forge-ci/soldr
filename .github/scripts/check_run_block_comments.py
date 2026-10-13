#!/usr/bin/env python3
"""Refuse `#` inside a folded (`>`) `run:` block in a workflow.

soldr#3018 found this the expensive way. `ci.yml`'s macOS queue watchdog
carried its rationale as `#` lines *inside* a `run: >-` scalar. In a folded
scalar `#` is not a comment -- it is literal text -- so YAML folded the whole
block onto one line and the shell then treated the first `#` as the start of a
shell comment, discarding everything after it.

What it discarded was `--grace-seconds 2700`. The watchdog silently ran at its
900s argparse default, so a recalibration the file documented in fifteen lines
of prose had never once taken effect. It failed intermittently for as long as
that was true, and every failure looked like the flaky lane it was watching.

A literal `|` block is safe -- newlines survive, so `#` starts a real shell
comment on its own line -- so only folded scalars are rejected. The fix is
always the same: move the prose to a YAML comment above the step.

Usage:
    python3 .github/scripts/check_run_block_comments.py [--workflows DIR]

Exit codes:
  0 - no folded run block contains `#`
  1 - at least one does
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
from dataclasses import dataclass

# `run: >` or `run: >-` (optionally with an indentation indicator), then the
# indented block that follows it. Both spellings occur in this repo: `run:` on
# its own line, and `- run:` as the first key of a list item.
FOLDED_RUN = re.compile(r"^(\s*(?:-\s+)?)run:\s*>-?\d*\s*$")


@dataclass(frozen=True)
class OffendingBlock:
    """The first `#` line hidden inside one folded `run: >` block."""

    line_number: int
    line: str


def offending_blocks(text: str) -> list[OffendingBlock]:
    """Return the first offending line of each folded run block."""
    found: list[OffendingBlock] = []
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        m = FOLDED_RUN.match(lines[i])
        if not m:
            i += 1
            continue
        indent = len(m.group(1))
        start = i + 1
        j = start
        while j < len(lines):
            line = lines[j]
            if line.strip() and (len(line) - len(line.lstrip())) <= indent:
                break
            if line.lstrip().startswith("#"):
                found.append(OffendingBlock(j + 1, line.strip()))
                break
            j += 1
        i = j if j > i else i + 1
    return found


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflows", default=".github/workflows")
    args = parser.parse_args(argv)

    problems: list[str] = []
    for path in sorted(pathlib.Path(args.workflows).glob("*.yml")):
        for block in offending_blocks(path.read_text(encoding="utf-8")):
            problems.append(
                f"{path}:{block.line_number}: `#` inside a folded `run: >` block.\n"
                f"     {block.line}\n"
                "     A folded scalar joins these lines into one, so the shell "
                "treats this `#` as a comment and DROPS every argument after "
                "it. Move the prose to a YAML comment above the step, or use a "
                "literal `run: |` block."
            )

    if problems:
        print("check_run_block_comments: found silently-truncated commands\n")
        for problem in problems:
            print(f"  - {problem}")
        return 1
    print("check_run_block_comments: no folded run block hides arguments behind `#`.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
