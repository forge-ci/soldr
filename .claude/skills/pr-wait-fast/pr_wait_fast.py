#!/usr/bin/env python3
"""Wait on a pull request's checks, returning on the FIRST failure or when all pass.

`gh pr checks --watch` waits for every check to finish before it returns,
so a lane that fails in minute two still costs the full wall-clock of the
slowest lane. This script polls `gh pr checks --json` and exits as soon as
the outcome is decided:

* exit 1 -- at least one check is in the `fail` bucket (names + links printed)
* exit 0 -- no check is pending and none failed (pass / skipping only)
* exit 2 -- the deadline passed with checks still pending
* exit 3 -- `gh` itself failed (auth, unknown PR, network)

With `--cancel`, the first failure also cancels every still-running workflow
run for the PR's head commit, so the whole CI fan-out fails fast instead of
burning runner minutes on a result that is already red.

Usage:
    pr_wait_fast.py <pr-number-or-url> [--repo owner/name] [--interval 20]
                    [--timeout 5400] [--cancel]
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (
    run_captured,
)  # noqa: E402 -- source-relative bootstrap precedes this import


@dataclass(frozen=True)
class Check:
    """The named check fields used by the waiter."""

    name: str
    bucket: str
    link: str


FAIL_BUCKETS = {"fail"}
DONE_BUCKETS = {"pass", "skipping"}


def run_gh(args: list[str]) -> subprocess.CompletedProcess[str]:
    return run_captured(["gh", *args], text=True, capture_output=True, check=False)


def fetch_checks(pr: str, repo: str | None) -> list[Check]:
    cmd = ["pr", "checks", pr, "--json", "name,state,bucket,link,workflow"]
    if repo:
        cmd += ["--repo", repo]
    proc = run_gh(cmd)
    if proc.returncode != 0 and not proc.stdout.strip():
        # Right after a push gh prints "no checks reported on the '<branch>'
        # branch" with no JSON: that is "pending", bounded by --grace.
        if "no checks reported" in proc.stderr:
            return []
        sys.stderr.write(proc.stderr)
        raise SystemExit(3)
    # `gh pr checks` exits 8 when checks are pending and 1 when any failed,
    # but still prints the JSON payload; only an empty payload is an error.
    checks = [
        Check(name=row["name"], bucket=row["bucket"], link=row["link"])
        for row in json.loads(proc.stdout or "[]")
    ]
    return checks


def head_sha(pr: str, repo: str | None) -> str:
    cmd = ["pr", "view", pr, "--json", "headRefOid", "-q", ".headRefOid"]
    if repo:
        cmd += ["--repo", repo]
    proc = run_gh(cmd)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit(3)
    return proc.stdout.strip()


def cancel_inflight_runs(sha: str, repo: str | None) -> list[str]:
    cmd = [
        "run",
        "list",
        "--commit",
        sha,
        "--json",
        "databaseId,status,name",
        "--limit",
        "100",
    ]
    if repo:
        cmd += ["--repo", repo]
    proc = run_gh(cmd)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        return []
    cancelled: list[str] = []
    for run in json.loads(proc.stdout or "[]"):
        if run["status"] in {"queued", "in_progress", "waiting", "pending"}:
            cancel = ["run", "cancel", str(run["databaseId"])]
            if repo:
                cancel += ["--repo", repo]
            if run_gh(cancel).returncode == 0:
                cancelled.append(f"{run['name']} (#{run['databaseId']})")
    return cancelled


def summarize(checks: list[Check]) -> str:
    counts: dict[str, int] = {}
    for check in checks:
        counts[check.bucket] = counts.get(check.bucket, 0) + 1
    return ", ".join(f"{k}={v}" for k, v in sorted(counts.items())) or "no checks yet"


def main() -> int:  # noqa: C901
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("pr", help="PR number, branch, or URL (as accepted by gh)")
    parser.add_argument("--repo", help="owner/name; defaults to the current repo")
    parser.add_argument("--interval", type=float, default=20.0, help="poll seconds")
    parser.add_argument(
        "--timeout", type=float, default=5400.0, help="deadline seconds"
    )
    parser.add_argument(
        "--cancel",
        action="store_true",
        help="on first failure, cancel every still-running run for the head commit",
    )
    parser.add_argument(
        "--grace",
        type=float,
        default=90.0,
        help="seconds to keep polling when no checks are reported yet (fresh push)",
    )
    args = parser.parse_args()

    started = time.monotonic()
    last_summary = None
    while True:
        checks = fetch_checks(args.pr, args.repo)
        summary = summarize(checks)
        if summary != last_summary:
            print(f"[{int(time.monotonic() - started):>5}s] {summary}", flush=True)
            last_summary = summary

        failed = [c for c in checks if c.bucket in FAIL_BUCKETS]
        if failed:
            print("\nFAILED:")
            for check in failed:
                print(f"  - {check.name}  {check.link}")
            if args.cancel:
                cancelled = cancel_inflight_runs(
                    head_sha(args.pr, args.repo), args.repo
                )
                if cancelled:
                    print("\nCancelled in-flight runs:")
                    for name in cancelled:
                        print(f"  - {name}")
            return 1

        elapsed = time.monotonic() - started
        if checks and all(c.bucket in DONE_BUCKETS for c in checks):
            print("\nALL PASSED")
            return 0
        if not checks and elapsed > args.grace:
            print("\nno checks reported for this PR head", file=sys.stderr)
            return 2
        if elapsed > args.timeout:
            pending = [c.name for c in checks if c.bucket not in DONE_BUCKETS]
            print(f"\nTIMEOUT with pending: {', '.join(pending)}", file=sys.stderr)
            return 2
        time.sleep(args.interval)


if __name__ == "__main__":
    raise SystemExit(main())
