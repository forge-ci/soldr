#!/usr/bin/env python3
"""Perf/benchmark workflow gate — two modes, one check on recent main activity.

The perf-matrix and benchmark-stats workflows are expensive (build + bench +
publish, ~30-60 min a piece). Firing them on every push to main during
active development produces per-commit noise that swamps the between-day
trend they're meant to track. Never firing them on quiet-only days means we
miss the trend entirely. This gate reconciles the two.

## Modes

* `push` (default): skip when the last non-bot commit on main is
  YOUNGER than `--hours` (default 24). Rationale: a busy day is
  already producing benchmark noise; suppress the per-merge triggers
  and let the nightly cron capture the day's cumulative trend.
* `schedule`: run when there WAS at least one non-bot commit on main
  in the last `--hours`. Rationale: on a day that had activity there
  is something new to benchmark, so the nightly cron fires; on a
  quiet day, skip cleanly.

The two modes read the same signal (age of the newest non-bot commit)
and simply invert its interpretation. Both write `should_run=<bool>` and
`reason=<text>` to `$GITHUB_OUTPUT`.

## Bot classifiers

An author email counts as bot-authored when it matches any of:
    * `github-actions[bot]@...` — the default GHA identity for
      `${{ secrets.GITHUB_TOKEN }}` pushes.
    * `actions@github.com` — legacy GHA push identity.
    * `noreply@github.com` — used by some Squash/Merge web flows and by
      programmatic pushes via the GitHub API.
    * `renovate[bot]` / `dependabot[bot]` — dependency-update bots.
    * `soldr-release-bot` — hypothetical future release automation
      identity.

`noreply@anthropic.com` (Claude's co-author trailer) is NOT classified as
bot: it appears only in the co-author trailer of commits whose primary
author is a real user, and `git log --format=%ae` reports the AUTHOR
email, not co-author trailers.

## Usage

    python3 .github/scripts/perf_gate.py --mode push     --hours 24
    python3 .github/scripts/perf_gate.py --mode schedule --hours 24

Always exits 0.

## Bypass rules baked in the caller

The workflow YAML is responsible for the trivial bypasses:
    * `workflow_dispatch` runs bypass the gate entirely (explicit
      human request).
    * On perf-matrix, `perf/**` and `evaluate/**` branch pushes also
      bypass (feature-branch iteration).

The script only handles the `push:main` and `schedule` code paths.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

BOT_EMAIL_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"^github-actions\[bot\]@", re.IGNORECASE),
    re.compile(r"^actions@github\.com$", re.IGNORECASE),
    re.compile(r"^noreply@github\.com$", re.IGNORECASE),
    re.compile(r"renovate\[bot\]", re.IGNORECASE),
    re.compile(r"dependabot\[bot\]", re.IGNORECASE),
    re.compile(r"soldr-release-bot", re.IGNORECASE),
)


def is_bot(email: str) -> bool:
    email = email.strip()
    return any(p.search(email) for p in BOT_EMAIL_PATTERNS)


@dataclass(frozen=True)
class Commit:
    """One commit's unix timestamp and author email."""

    unix_ts: int
    author_email: str


def walk_main_commits(limit: int, ref: str) -> Iterable[Commit]:
    """Yield a `Commit` for the most recent `limit` commits on `ref`."""
    proc = run_captured(
        ["git", "log", f"-n{limit}", "--format=%at|%ae", ref],
        capture_output=True,
        text=True,
        check=True,
    )
    for line in proc.stdout.splitlines():
        ts_str, sep, email = line.partition("|")
        if not sep:
            continue
        try:
            yield Commit(int(ts_str), email)
        except ValueError:
            continue


def emit_output(should_run: bool, reason: str) -> None:
    print(reason)
    out_path = os.environ.get("GITHUB_OUTPUT")
    if not out_path:
        return
    with open(out_path, "a", encoding="utf-8") as fh:
        fh.write(f"should_run={'true' if should_run else 'false'}\n")
        # Reason is a single line by construction.
        fh.write(f"reason={reason}\n")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    ap.add_argument(
        "--mode",
        choices=("push", "schedule"),
        default="push",
        help="push (default): skip when there was recent activity. "
        "schedule: run when there was recent activity.",
    )
    ap.add_argument(
        "--hours",
        type=int,
        default=24,
        help="Size of the recent-activity window.",
    )
    ap.add_argument(
        "--limit",
        type=int,
        default=200,
        help="Number of recent commits to walk (bounds the git log call).",
    )
    ap.add_argument(
        "--ref",
        default="HEAD",
        help="Git ref to walk. Defaults to HEAD (which is `main` when the "
        "workflow triggered on push:main or schedule with a checkout of main).",
    )
    args = ap.parse_args()

    now = int(time.time())
    cutoff_ts = now - args.hours * 3600

    latest_human_ts: int | None = None
    latest_human_email: str | None = None
    for commit in walk_main_commits(args.limit, args.ref):
        if is_bot(commit.author_email):
            continue
        latest_human_ts = commit.unix_ts
        latest_human_email = commit.author_email
        break

    if latest_human_ts is None:
        # No non-bot commit in the walk window — no signal either way.
        # Push mode: extremely quiet, safe to run.
        # Schedule mode: nothing new to benchmark, skip.
        if args.mode == "schedule":
            emit_output(
                False,
                f"No non-bot commit in the last {args.limit} main commits — "
                "nothing new to benchmark; skipping nightly perf run.",
            )
        else:
            emit_output(
                True,
                f"No non-bot commit in the last {args.limit} main commits — "
                "quiet enough to proceed with perf run.",
            )
        return 0

    hours_ago = (now - latest_human_ts) // 3600
    had_recent_activity = latest_human_ts >= cutoff_ts

    if args.mode == "schedule":
        if had_recent_activity:
            emit_output(
                True,
                f"Nightly cron: last non-bot commit was {hours_ago}h ago "
                f"({latest_human_email}); < {args.hours}h. Day had activity, "
                "proceeding with perf run.",
            )
        else:
            emit_output(
                False,
                f"Nightly cron: last non-bot commit was {hours_ago}h ago "
                f"({latest_human_email}); >= {args.hours}h. Quiet day, "
                "nothing new to benchmark; skipping.",
            )
        return 0

    # mode == "push"
    if had_recent_activity:
        emit_output(
            False,
            f"Push:main: last non-bot commit was {hours_ago}h ago "
            f"({latest_human_email}); < {args.hours}h. Active burst — "
            "skipping perf run; the nightly cron will capture today's trend.",
        )
    else:
        emit_output(
            True,
            f"Push:main: last non-bot commit was {hours_ago}h ago "
            f"({latest_human_email}); >= {args.hours}h. Post-quiet window, "
            "proceeding with perf run.",
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
