#!/usr/bin/env python3
"""Release front door: start, resume and report one exact-SHA release (soldr#3343).

A soldr release is two dispatches on one merged commit (RELEASE.md): full CI
with ``candidate_sha``, then ``auto-release.yml`` with the same SHA and that
CI run's id. Doing them by hand meant copying run ids between pages and, after
a failure, working out which step to repeat. This script owns the sequence and
is idempotent, so **re-running ``start`` is the resume path**. It never bumps a
version and never touches a tag: the workflow mints the tag.

    uv run --no-project python ci/release.py status --sha <40-hex>
    uv run --no-project python ci/release.py start  --sha <40-hex>

``start`` is a state machine, one step per invocation:

1. refuse a SHA that is not a full 40-hex commit reachable from ``origin/main``;
2. refuse when ``v<version>`` (from ``Cargo.toml`` at that SHA) already tags a
   *different* commit -- a conflicting identity is never moved;
3. with a successful ``CI full <sha>`` run: dispatch ``auto-release.yml`` with
   ``candidate_sha`` and ``full_ci_run_id``;
4. with one still running: report it and stop (run ``start`` again later);
5. otherwise: dispatch full CI for the SHA and stop.

``auto-release.yml`` itself skips surfaces that are already complete, so a
second release dispatch for the same SHA only retries what is missing.

Every ``start`` also keeps the attempt's live control record: one open issue
per (version, SHA), titled ``release attempt v<version> @ <sha12>``, whose body
is the frozen directive (SHA, tag, mode, targets, destinations) and whose
comments are the timestamped state transitions.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tomllib
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import run_captured  # noqa: E402 -- source-relative bootstrap

REPO = "zackees/soldr"
CI_WORKFLOW = "ci.yml"
RELEASE_WORKFLOW = "auto-release.yml"
SHA = re.compile(r"[0-9a-f]{40}")

Runner = Callable[[Sequence[str]], subprocess.CompletedProcess]


class ReleaseError(Exception):
    """A refusal: nothing was dispatched."""


@dataclass(frozen=True)
class CiRun:
    run_id: int
    status: str
    conclusion: str


@dataclass(frozen=True)
class Plan:
    """What `start` decided, and the command it ran (if any)."""

    version: str
    action: str
    detail: str


def default_runner(argv: Sequence[str]) -> subprocess.CompletedProcess:
    return run_captured(list(argv), capture_output=True, text=True, check=False)


def _ok(run: Runner, argv: Sequence[str]) -> str:
    completed = run(argv)
    if completed.returncode != 0:
        raise ReleaseError(
            f"`{' '.join(argv[:4])}` failed: {(completed.stderr or completed.stdout).strip()}"
        )
    return completed.stdout


def validate_sha(sha: str, run: Runner) -> None:
    if not SHA.fullmatch(sha):
        raise ReleaseError("candidate SHA must be a full 40-character lowercase commit")
    if run(["git", "merge-base", "--is-ancestor", sha, "origin/main"]).returncode != 0:
        raise ReleaseError(f"{sha} is not reachable from origin/main (fetch first?)")


def version_at(sha: str, run: Runner) -> str:
    manifest = tomllib.loads(_ok(run, ["git", "show", f"{sha}:Cargo.toml"]))
    version = manifest.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(version, str):
        raise ReleaseError(
            "Cargo.toml at the candidate has no [workspace.package].version"
        )
    return version


def tagged_sha(tag: str, run: Runner) -> str | None:
    """The commit `tag` points at on origin, or None when it does not exist."""
    out = _ok(
        run,
        [
            "git",
            "ls-remote",
            "--tags",
            "origin",
            f"refs/tags/{tag}^{{}}",
            f"refs/tags/{tag}",
        ],
    )
    lines = [line.split() for line in out.splitlines() if line.strip()]
    peeled = [sha for sha, ref in lines if ref.endswith("^{}")]
    direct = [sha for sha, ref in lines if not ref.endswith("^{}")]
    found = peeled or direct
    return found[0] if found else None


def ci_full_runs(sha: str, run: Runner) -> list[CiRun]:
    """`CI full <sha>` workflow_dispatch runs of ci.yml, newest first."""
    out = _ok(
        run,
        [
            "gh",
            "run",
            "list",
            "--repo",
            REPO,
            "--workflow",
            CI_WORKFLOW,
            "--event",
            "workflow_dispatch",
            "--limit",
            "50",
            "--json",
            "databaseId,displayTitle,status,conclusion",
        ],
    )
    return [
        CiRun(int(item["databaseId"]), item["status"], item.get("conclusion") or "")
        for item in json.loads(out)
        if item.get("displayTitle") == f"CI full {sha}"
    ]


def plan_start(sha: str, run: Runner) -> Plan:
    validate_sha(sha, run)
    version = version_at(sha, run)
    tag = f"v{version}"
    existing = tagged_sha(tag, run)
    if existing is not None and existing != sha:
        raise ReleaseError(
            f"{tag} already tags {existing}, not {sha}; a released version is never "
            "moved -- bump the version for a new candidate"
        )
    runs = ci_full_runs(sha, run)
    passed = next(
        (r for r in runs if r.status == "completed" and r.conclusion == "success"), None
    )
    if passed is not None:
        return Plan(version, "dispatch-release", str(passed.run_id))
    running = next((r for r in runs if r.status != "completed"), None)
    if running is not None:
        return Plan(version, "wait-for-ci", str(running.run_id))
    return Plan(version, "dispatch-ci", "")


def execute(plan: Plan, sha: str, run: Runner) -> str:
    if plan.action == "dispatch-release":
        _ok(
            run,
            [
                "gh",
                "workflow",
                "run",
                RELEASE_WORKFLOW,
                "--repo",
                REPO,
                "--ref",
                "main",
                "-f",
                f"candidate_sha={sha}",
                "-f",
                f"full_ci_run_id={plan.detail}",
            ],
        )
        return f"dispatched {RELEASE_WORKFLOW} for v{plan.version} with full CI run {plan.detail}"
    if plan.action == "wait-for-ci":
        return (
            f"full CI run {plan.detail} for {sha} is still running; "
            "run `start` again once it succeeds"
        )
    _ok(
        run,
        [
            "gh",
            "workflow",
            "run",
            CI_WORKFLOW,
            "--repo",
            REPO,
            "--ref",
            "main",
            "-f",
            f"candidate_sha={sha}",
        ],
    )
    return f"dispatched full CI for v{plan.version} at {sha}; run `start` again once it succeeds"


def attempt_title(version: str, sha: str) -> str:
    return f"release attempt v{version} @ {sha[:12]}"


def attempt_directive(version: str, sha: str, targets: list[str]) -> str:
    """The attempt issue's body: the live control record (soldr#3343).

    It is written once, when the attempt starts, and never edited: the
    scope (SHA, tag, mode, targets, destinations) is frozen for the life of
    the attempt. State transitions are appended as comments.
    """
    target_lines = "\n".join(f"  - {triple}" for triple in targets)
    return (
        f"Release attempt for `v{version}` on exact SHA `{sha}`, driven by "
        "`ci/release.py` (soldr#3343). Do not edit the directive; the script "
        "appends state transitions as comments.\n\n"
        "```yaml\n"
        f"candidate_sha: {sha}\n"
        f"tag: v{version}\n"
        "mode: full\n"
        "destinations: [github-release, pypi, npm]\n"
        "crates_io: skipped while every workspace crate is publish = false\n"
        "targets:\n"
        f"{target_lines}\n"
        "```\n"
    )


def find_or_open_attempt(version: str, sha: str, run: Runner) -> int:
    """The open attempt issue for (version, SHA), opened when there is none."""
    title = attempt_title(version, sha)
    out = _ok(
        run,
        [
            "gh",
            "issue",
            "list",
            "--repo",
            REPO,
            "--state",
            "open",
            "--search",
            f'"{title}" in:title',
            "--json",
            "number,title",
        ],
    )
    for item in json.loads(out):
        if item.get("title") == title:
            return int(item["number"])
    url = _ok(
        run,
        [
            "gh",
            "issue",
            "create",
            "--repo",
            REPO,
            "--title",
            title,
            "--body",
            attempt_directive(version, sha, release_targets()),
        ],
    ).strip()
    return int(url.rsplit("/", 1)[-1])


def release_targets() -> list[str]:
    """The canonical release targets (ci/canonical-targets.json)."""
    contract = json.loads(
        (ROOT / "ci" / "canonical-targets.json").read_text(encoding="utf-8")
    )
    return [
        entry["triple"]
        for entry in contract["targets"]
        if entry["release"]["status"] == "included"
    ]


def record_transition(issue: int, message: str, run: Runner) -> None:
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    _ok(
        run,
        [
            "gh",
            "issue",
            "comment",
            str(issue),
            "--repo",
            REPO,
            "--body",
            f"`{stamp}` {message}",
        ],
    )


def status(sha: str, run: Runner) -> str:
    validate_sha(sha, run)
    version = version_at(sha, run)
    tag = f"v{version}"
    existing = tagged_sha(tag, run)
    runs = ci_full_runs(sha, run)
    lines = [f"candidate {sha} -> {tag}"]
    lines.append(f"tag: {'absent' if existing is None else existing}")
    if not runs:
        lines.append("full CI: none dispatched")
    for ci in runs:
        lines.append(f"full CI run {ci.run_id}: {ci.status} {ci.conclusion}".rstrip())
    return "\n".join(lines)


def main(argv: list[str] | None = None, run: Runner = default_runner) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("command", choices=["status", "start"])
    parser.add_argument("--sha", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "status":
            print(status(args.sha, run))
        else:
            plan = plan_start(args.sha, run)
            issue = find_or_open_attempt(plan.version, args.sha, run)
            message = execute(plan, args.sha, run)
            record_transition(issue, message, run)
            print(f"{message} (attempt #{issue})")
    except ReleaseError as error:
        print(f"release: refused: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
