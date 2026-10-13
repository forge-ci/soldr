#!/usr/bin/env python3
"""Decide whether a pull request may skip platform E2E validation.

A pull request whose every changed path is documentation or repository
metadata that cannot affect a shipped binary skips the platform fan-out
outright.  For everything else the ``fast-build`` label is the request
channel, honored only when the diff is otherwise unclassifiable as safe.
Pushes, empty diffs, and any unclassified path fail safe by running
everything.

The docs-only rule complements canonical CI's cheap docs path: its protected
``Lint`` context still reports, while the compiler/platform fan-out stays off.

Emits two outputs from one classification (soldr#3018):

``run_windows_e2e``
    The original gate, for the three Windows MSVC pairs.

``run_platform_e2e``
    The same decision, extended to the macOS pairs and the non-x64 Linux
    lanes.  A docs-only change used to pay for eight platform fan-outs; on
    macOS each of those also re-pays a 12-23 minute queue, which soldr#2996
    measured as the largest single source of CI wall-clock variance.

``e2e-linux-x64`` is deliberately NOT gated.  ``ci.yml`` calls the Linux
glibc lane the proof of life, and a proof of life that a heuristic can skip
is not one.  It is also the cheapest lane, so gating it saves least and
risks most.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path, PurePosixPath
from typing import NamedTuple, Sequence

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

FAST_BUILD_LABEL = "fast-build"


class Decision(NamedTuple):
    run: bool
    reason: str


def _normalized(path: str) -> str:
    normalized = str(PurePosixPath(path.replace("\\", "/")))
    return normalized[2:] if normalized.startswith("./") else normalized


def is_low_risk_path(path: str) -> bool:
    """Return whether *path* is safe to omit Windows binary validation for."""

    normalized = _normalized(path)
    lowered = normalized.lower()
    name = PurePosixPath(lowered).name
    return (
        lowered.endswith(".md")
        or lowered.startswith("docs/")
        or lowered.startswith(".github/issue_template/")
        or lowered.startswith(".github/pull_request_template")
        or name.startswith("license")
    )


def decide_windows_e2e(
    *,
    event_name: str,
    labels: Sequence[str],
    changed_paths: Sequence[str],
    full: bool = False,
) -> Decision:
    if full:
        return Decision(True, "ci-full requires every platform")
    if event_name != "pull_request":
        return Decision(True, f"{event_name or 'non-PR'} events always run Windows E2E")
    if not changed_paths:
        return Decision(True, "empty diff cannot be classified safely")

    # A documentation/metadata-only PR skips the platform fan-out without
    # needing the fast-build label. Canonical CI's path-selection job also
    # skips its host build for this case; this second guard preserves that
    # result if the policy is reused independently. The label remains the
    # request channel for everything else.
    sensitive = [path for path in changed_paths if not is_low_risk_path(path)]
    if not sensitive:
        return Decision(False, "documentation/metadata-only change skips platform E2E")
    if FAST_BUILD_LABEL not in labels:
        return Decision(True, "fast-build label is absent")
    return Decision(True, f"platform-sensitive path changed: {sensitive[0]}")


def _pull_request_paths(event: dict) -> list[str]:
    pull_request = event.get("pull_request")
    if not isinstance(pull_request, dict):
        return []
    base = pull_request.get("base")
    head = pull_request.get("head")
    if not isinstance(base, dict) or not isinstance(head, dict):
        return []
    base_sha = base.get("sha")
    head_sha = head.get("sha")
    if not isinstance(base_sha, str) or not isinstance(head_sha, str):
        return []

    result = run_captured(
        [
            "git",
            "diff",
            "--name-only",
            "--diff-filter=ACDMRTUXB",
            f"{base_sha}...{head_sha}",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return [line for line in result.stdout.splitlines() if line]


def _labels(event: dict) -> list[str]:
    pull_request = event.get("pull_request")
    if not isinstance(pull_request, dict):
        return []
    raw_labels = pull_request.get("labels")
    if not isinstance(raw_labels, list):
        return []
    return [
        name
        for label in raw_labels
        if isinstance(label, dict) and isinstance((name := label.get("name")), str)
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-name", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    parser.add_argument("--event-path", default=os.environ.get("GITHUB_EVENT_PATH", ""))
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT", ""))
    parser.add_argument("--full", action="store_true")
    args = parser.parse_args()

    event: dict = {}
    if args.event_path:
        event = json.loads(Path(args.event_path).read_text(encoding="utf-8"))
    paths = (
        _pull_request_paths(event)
        if args.event_name == "pull_request" and not args.full
        else []
    )
    decision = decide_windows_e2e(
        event_name=args.event_name,
        labels=_labels(event),
        changed_paths=paths,
        full=args.full,
    )
    verdict = "run" if decision.run else "skip"
    print(
        f"Platform E2E (Windows + macOS + non-x64 Linux): {verdict} ({decision.reason})"
    )
    if not args.github_output:
        raise SystemExit("--github-output (or GITHUB_OUTPUT) is required")
    with Path(args.github_output).open("a", encoding="utf-8") as output:
        value = "true" if decision.run else "false"
        output.write(f"run_windows_e2e={value}\n")
        # soldr#3018: same classification, broader fan-out. Kept as a distinct
        # output so a future change can narrow one without silently moving the
        # other.
        output.write(f"run_platform_e2e={value}\n")
        output.write(f"reason={decision.reason}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
