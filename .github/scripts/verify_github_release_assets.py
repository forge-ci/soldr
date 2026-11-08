#!/usr/bin/env python3
"""Verify a published GitHub Release has every contract-required asset.

This is the release-side GitHub asset gate extracted from auto-release.yml
(soldr#2469 step 2.2). The data validation is deliberately pure so the
workflow's release-state decisions can be exercised from fixtures.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any, cast

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)


def load_release_completeness() -> Any:
    """Load the sibling helper in direct-exec and file-loaded test modes."""
    path = Path(__file__).with_name("release_completeness.py")
    spec = importlib.util.spec_from_file_location("release_completeness", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load release completeness helper: {path}")
    module = importlib.util.module_from_spec(spec)
    # Registered before exec so the helper's @dataclass classes can resolve
    # their own module namespace when this script runs as a file.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return cast(Any, module)


RELEASE_COMPLETENESS = load_release_completeness()


def verify_release_assets(tag: str, release: dict, *, draft: bool = False) -> list[str]:
    """Return named failures for an incomplete GitHub Release.

    By default the release must be published. With `draft`, it must still be
    a draft: the publish job checks the full asset set there, before it makes
    the release public (soldr#2469 4.2), because a published immutable release
    can no longer gain assets.
    """
    failures: list[str] = []
    if draft and release.get("isDraft") is not True:
        failures.append(f"GitHub release {tag} is not a draft")
    if not draft and release.get("isDraft") is not False:
        failures.append(f"GitHub release {tag} is still a draft")

    assets = {
        asset.get("name"): asset.get("size")
        for asset in release.get("assets", [])
        if isinstance(asset, dict)
    }
    for name in RELEASE_COMPLETENESS.expected_github_assets(
        tag, RELEASE_COMPLETENESS.included_triples()
    ):
        size = assets.get(name)
        if size is None:
            failures.append(f"GitHub release {tag} is missing expected asset: {name}")
        elif not isinstance(size, int) or size <= 0:
            failures.append(f"GitHub release asset {name} has invalid size {size!r}")
    return failures


def fetch_release(tag: str, repo: str) -> dict:
    """Read release metadata through the runner's authenticated gh CLI."""
    result = run_captured(
        ["gh", "release", "view", tag, "--repo", repo, "--json", "assets,isDraft"],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="release tag, e.g. v0.9.1")
    parser.add_argument(
        "--repo",
        default=os.environ.get("GITHUB_REPOSITORY", "zackees/soldr"),
        help="GitHub owner/repository (defaults to GITHUB_REPOSITORY)",
    )
    parser.add_argument(
        "--draft",
        action="store_true",
        help="verify the still-draft release before it is published (soldr#2469 4.2)",
    )
    args = parser.parse_args(argv)
    tag = args.version if args.version.startswith("v") else f"v{args.version}"
    try:
        failures = verify_release_assets(
            tag, fetch_release(tag, args.repo), draft=args.draft
        )
    except (subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"could not inspect GitHub release {tag}: {error}", file=sys.stderr)
        return 1
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(
        f"GitHub release {tag} has "
        f"{len(RELEASE_COMPLETENESS.expected_github_assets(tag, RELEASE_COMPLETENESS.included_triples()))} expected assets."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
