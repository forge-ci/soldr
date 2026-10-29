#!/usr/bin/env python3
"""Verify the run a release promotes its artifacts from (soldr#2469 4.1).

A promotion publishes the exact artifacts an earlier ``auto-release.yml`` run
built and smoked, instead of compiling them again. That is only sound when
the source run is provably that candidate's: this gate requires it to be a
``auto-release.yml`` ``workflow_dispatch`` run in this repository whose
run-name names the same ``candidate_sha``, with every production build and
release-artifact smoke job successful. Anything else refuses the promotion;
nothing is published from an unverified run.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.request
from dataclasses import dataclass

WORKFLOW_PATH = ".github/workflows/auto-release.yml"
SMOKE_PREFIX = "Smoke "
# Only the release-artifact smokes: post-publish checks such as "Smoke
# published Dylint" are skipped on a dry run, which is the run promoted.
SMOKE_MARKER = "release artifacts"
SHA = re.compile(r"[0-9a-f]{40}")


class PromotionError(Exception):
    """The source run cannot be promoted."""


@dataclass(frozen=True)
class SourceRun:
    path: str
    event: str
    repository: str
    title: str
    html_url: str


@dataclass(frozen=True)
class SourceJob:
    name: str
    conclusion: str


def required_jobs(jobs: list[SourceJob], build_names: list[str]) -> list[str]:
    """Names of the jobs that must have succeeded: every build and every smoke."""
    smokes = [
        job.name
        for job in jobs
        if job.name.startswith(SMOKE_PREFIX) and SMOKE_MARKER in job.name
    ]
    return build_names + smokes


def validate(
    run: SourceRun,
    jobs: list[SourceJob],
    *,
    sha: str,
    repository: str,
    build_names: list[str],
) -> None:
    if not SHA.fullmatch(sha):
        raise PromotionError("candidate_sha must be a full lowercase commit SHA")
    if run.path != WORKFLOW_PATH or run.event != "workflow_dispatch":
        raise PromotionError(
            f"source run is not a {WORKFLOW_PATH} dispatch: {run.html_url}"
        )
    if run.repository != repository:
        raise PromotionError(
            f"source run belongs to {run.repository}, not {repository}"
        )
    if sha not in run.title.split():
        raise PromotionError(
            f"source run '{run.title}' was not for candidate {sha}: {run.html_url}"
        )
    by_name = {job.name: job.conclusion for job in jobs}
    smokes = [
        job.name
        for job in jobs
        if job.name.startswith(SMOKE_PREFIX) and SMOKE_MARKER in job.name
    ]
    if not smokes:
        raise PromotionError(
            f"source run ran no release-artifact smoke: {run.html_url}"
        )
    failed = [
        name
        for name in required_jobs(jobs, build_names)
        if by_name.get(name) != "success"
    ]
    if failed:
        raise PromotionError(
            f"source run {run.html_url} did not pass: " + ", ".join(sorted(failed))
        )


def get_json(url: str, token: str) -> object:
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.loads(response.read().decode("utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--candidate-sha", required=True)
    parser.add_argument(
        "--repository", default=os.environ.get("GITHUB_REPOSITORY", "zackees/soldr")
    )
    parser.add_argument(
        "--build-matrix",
        required=True,
        help="the release build matrix JSON (release_completeness.py --build-matrix)",
    )
    args = parser.parse_args()
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN") or ""
    api = f"https://api.github.com/repos/{args.repository}/actions/runs/{args.run_id}"
    raw_run = get_json(api, token)
    raw_jobs = get_json(f"{api}/jobs?per_page=100", token)
    if not isinstance(raw_run, dict) or not isinstance(raw_jobs, dict):
        raise SystemExit("unexpected GitHub API response")
    run = SourceRun(
        path=str(raw_run.get("path", "")),
        event=str(raw_run.get("event", "")),
        repository=str(raw_run.get("repository", {}).get("full_name", "")),
        title=str(raw_run.get("display_title", "")),
        html_url=str(raw_run.get("html_url", "")),
    )
    jobs = [
        SourceJob(str(job.get("name", "")), str(job.get("conclusion") or ""))
        for job in raw_jobs.get("jobs", [])
    ]
    build_names = [str(row["name"]) for row in json.loads(args.build_matrix)]
    try:
        validate(
            run,
            jobs,
            sha=args.candidate_sha,
            repository=args.repository,
            build_names=build_names,
        )
    except PromotionError as error:
        print(f"::error::promotion refused: {error}", file=sys.stderr)
        return 1
    print(f"promotion source verified for {args.candidate_sha}: {run.html_url}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
