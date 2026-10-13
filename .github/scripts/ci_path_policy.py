#!/usr/bin/env python3
"""Identify documentation-only PRs for the cheap routine lint status."""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

# Dependency-free capture also works when this script is invoked by absolute path.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)


def normalized(path: str) -> str:
    path = str(PurePosixPath(path.replace("\\", "/")))
    return path[2:] if path.startswith("./") else path


def is_docs_only_path(path: str) -> bool:
    path = normalized(path).lower()
    name = PurePosixPath(path).name
    return (
        path.endswith(".md")
        or path.startswith("docs/")
        or path.startswith(".github/issue_template/")
        or path.startswith(".github/pull_request_template")
        or name.startswith("license")
    )


@dataclass(frozen=True)
class PathSelection:
    """Job-level selections, emitted as ``name=value`` output lines."""

    docs_only: bool

    def output_lines(self) -> tuple[str, ...]:
        return (f"docs_only={str(self.docs_only).lower()}",)


@dataclass(frozen=True)
class PullRequestRange:
    base_sha: str
    head_sha: str


def select(event_name: str, changed_paths: list[str]) -> PathSelection:
    """Return the selections for job-level GitHub expressions."""

    docs_only = (
        event_name == "pull_request"
        and bool(changed_paths)
        and all(is_docs_only_path(path) for path in changed_paths)
    )
    return PathSelection(docs_only=docs_only)


def parse_pull_request_range(event_text: str) -> PullRequestRange | None:
    """Read the base/head SHAs from a webhook event payload, if it has them."""

    if not event_text:
        return None
    event = json.loads(event_text)
    if not isinstance(event, dict):
        return None
    pr = event.get("pull_request")
    if not isinstance(pr, dict):
        return None
    base, head = pr.get("base"), pr.get("head")
    if not isinstance(base, dict) or not isinstance(head, dict):
        return None
    base_sha, head_sha = base.get("sha"), head.get("sha")
    if not isinstance(base_sha, str) or not isinstance(head_sha, str):
        return None
    return PullRequestRange(base_sha=base_sha, head_sha=head_sha)


def pull_request_paths(pull_request: PullRequestRange | None) -> list[str]:
    if pull_request is None:
        return []
    result = run_captured(
        [
            "git",
            "diff",
            "--name-only",
            "--diff-filter=ACDMRTUXB",
            f"{pull_request.base_sha}...{pull_request.head_sha}",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return [path for path in result.stdout.splitlines() if path]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-name", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    parser.add_argument("--event-path", default=os.environ.get("GITHUB_EVENT_PATH", ""))
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT", ""))
    args = parser.parse_args()
    event_text = (
        Path(args.event_path).read_text(encoding="utf-8") if args.event_path else ""
    )
    outputs = select(
        args.event_name, pull_request_paths(parse_pull_request_range(event_text))
    )
    if not args.github_output:
        raise SystemExit("--github-output (or GITHUB_OUTPUT) is required")
    with Path(args.github_output).open("a", encoding="utf-8") as destination:
        for line in outputs.output_lines():
            destination.write(f"{line}\n")
    print("CI path selection: " + ", ".join(outputs.output_lines()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
