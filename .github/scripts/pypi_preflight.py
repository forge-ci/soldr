#!/usr/bin/env python3
"""Preflight the wheel set against PyPI's storage limits before upload (soldr#3343).

PyPI rejects a file over its per-file limit and any upload past the project's
total storage limit, and a rejection mid-way through a publish leaves a
partial release. This check runs first and fails before any byte is sent:

* every wheel must be within the per-file limit (PyPI default 100 MB);
* the project's current total plus the wheels not already on PyPI must be
  within the project limit (PyPI default 10 GB).

PyPI's JSON API reports file sizes but not a project's configured limits, so
the defaults apply unless overridden (``--file-limit-bytes``,
``--project-limit-bytes``) when PyPI has raised them for the project. If the
current total cannot be fetched, the check fails rather than assume headroom.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path

PROJECT = "soldr"
FILE_LIMIT_BYTES = 100 * 1000 * 1000
PROJECT_LIMIT_BYTES = 10 * 1000 * 1000 * 1000
WARN_FRACTION = 0.9


@dataclass(frozen=True)
class Wheel:
    name: str
    size: int


@dataclass(frozen=True)
class PublishedFile:
    name: str
    size: int


@dataclass(frozen=True)
class Verdict:
    failures: tuple[str, ...]
    warnings: tuple[str, ...]
    new_bytes: int
    projected_bytes: int


def local_wheels(dist: Path) -> list[Wheel]:
    return sorted(
        (Wheel(path.name, path.stat().st_size) for path in dist.glob("*.whl")),
        key=lambda wheel: wheel.name,
    )


def published_files(document: object) -> list[PublishedFile]:
    """Every file of every release in a PyPI project JSON document."""
    if not isinstance(document, dict):
        raise ValueError("PyPI project JSON is not an object")
    releases = document.get("releases")
    if not isinstance(releases, dict):
        raise ValueError("PyPI project JSON has no releases map")
    files = []
    for entries in releases.values():
        for entry in entries if isinstance(entries, list) else []:
            if isinstance(entry, dict):
                files.append(PublishedFile(str(entry["filename"]), int(entry["size"])))
    return files


def evaluate(
    wheels: list[Wheel],
    published: list[PublishedFile],
    *,
    file_limit: int,
    project_limit: int,
) -> Verdict:
    failures = []
    warnings = []
    if not wheels:
        failures.append("no wheels to publish in the dist directory")
    for wheel in wheels:
        if wheel.size > file_limit:
            failures.append(
                f"{wheel.name} is {wheel.size} bytes, over the {file_limit}-byte "
                "PyPI per-file limit"
            )
    already = {item.name for item in published}
    new_bytes = sum(wheel.size for wheel in wheels if wheel.name not in already)
    current = sum(item.size for item in published)
    projected = current + new_bytes
    if projected > project_limit:
        failures.append(
            f"project total would reach {projected} bytes, over the "
            f"{project_limit}-byte PyPI project limit (currently {current}); "
            "delete old releases on PyPI or request a higher limit first"
        )
    elif projected > WARN_FRACTION * project_limit:
        warnings.append(
            f"project total will reach {projected} of {project_limit} bytes "
            f"({projected / project_limit:.0%}); request a higher limit soon"
        )
    return Verdict(tuple(failures), tuple(warnings), new_bytes, projected)


def fetch_project(project: str) -> object:
    with urllib.request.urlopen(
        f"https://pypi.org/pypi/{project}/json", timeout=30
    ) as response:
        return json.loads(response.read().decode("utf-8"))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", default="dist")
    parser.add_argument("--project", default=PROJECT)
    parser.add_argument("--file-limit-bytes", type=int, default=FILE_LIMIT_BYTES)
    parser.add_argument("--project-limit-bytes", type=int, default=PROJECT_LIMIT_BYTES)
    args = parser.parse_args(argv)
    wheels = local_wheels(Path(args.dist))
    for wheel in wheels:
        print(f"{wheel.size:>12}  {wheel.name}")
    try:
        published = published_files(fetch_project(args.project))
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(
            f"::error::cannot establish the PyPI project's current size ({error}); "
            "refusing to publish without known headroom",
            file=sys.stderr,
        )
        return 1
    verdict = evaluate(
        wheels,
        published,
        file_limit=args.file_limit_bytes,
        project_limit=args.project_limit_bytes,
    )
    for warning in verdict.warnings:
        print(f"::warning::{warning}")
    for failure in verdict.failures:
        print(f"::error::{failure}", file=sys.stderr)
    if verdict.failures:
        return 1
    print(
        f"PyPI preflight ok: {len(wheels)} wheels, {verdict.new_bytes} new bytes, "
        f"project total {verdict.projected_bytes} bytes"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
