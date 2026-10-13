#!/usr/bin/env python3
"""Report whether `soldr cook` actually did anything, and say so loudly.

soldr#3008: cook failing is not a lane failure. The `setup-soldr/cook`
action degrades to "continuing without cooked deps", so the build still
succeeds -- it just does so with no cache. That fallback is the right
behaviour, but it made cook *broken and silent*: it crashed on every run of
the only enabled lane for an unknown period, and the only symptom was
`gh cache list` showing zero cook entries, visible only to someone who
looked.

Under soldr#2996's directive cook is the only durable cache, so "never
saved" and "working" must stop looking identical from CI.

This reads the action's own outputs rather than grepping its logs, so it
follows the documented contract:

    cache-hit   true when the cache restored without running soldr cook
    ran         true when this step ran soldr cook
    save-layer  base | delta | none

Neither ran nor hit means cook produced nothing. That is worth a warning
annotation, which is visible on the run summary without turning the lane
red -- promoting it to a failure is a separate decision (see soldr#3008),
and ci.yml:70-88 records what happens when a gate goes red before it is
trusted.

Exits 0 always. A reporter that can fail the build it is reporting on is
worse than no reporter.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import sys
from dataclasses import dataclass


def truthy(value: str | None) -> bool:
    return (value or "").strip().lower() == "true"


@dataclass(frozen=True)
class Outcome:
    """An annotation `level` ('notice' or 'warning') and its message."""

    level: str
    message: str


def classify(ran: bool, hit: bool, layer: str) -> Outcome:
    """Return the annotation level and message for this cook outcome."""
    if hit:
        return Outcome("notice", "cook cache hit; soldr cook did not need to run")
    if ran:
        return Outcome(
            "notice", f"soldr cook ran and will save (layer={layer or 'unknown'})"
        )
    return Outcome(
        "warning",
        "soldr cook neither hit the cache nor ran successfully -- no cook "
        "cache will be written for this lane. This does not fail the build "
        "(the action continues without cooked deps), but it means the lane "
        "has no durable cache. See soldr#3008.",
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    args = parser.parse_args(argv)

    ran = truthy(os.environ.get("COOK_RAN"))
    hit = truthy(os.environ.get("COOK_HIT"))
    layer = (os.environ.get("COOK_LAYER") or "").strip()

    outcome = classify(ran, hit, layer)
    level, message = outcome.level, outcome.message
    line = f"cook[{args.target}]: {message}"

    # A GitHub annotation makes it visible on the run summary page, not just
    # buried in a step log nobody expands.
    print(f"::{level} title=cook {args.target}::{message}")
    print(line)

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        try:
            with pathlib.Path(summary).open("a", encoding="utf-8") as handle:
                mark = "⚠️" if level == "warning" else "✅"
                handle.write(f"- {mark} `cook` **{args.target}**: {message}\n")
        except OSError as error:
            print(f"report_cook_outcome: summary unwritable: {error}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
