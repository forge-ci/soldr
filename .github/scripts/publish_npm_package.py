#!/usr/bin/env python3
"""Idempotently publish the npm package from a validated release checkout."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

# Dependency-free capture also works when this script is invoked by absolute path.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)


def run_npm(
    arguments: Sequence[str], source_dir: Path
) -> subprocess.CompletedProcess[str]:
    return run_captured(
        ["npm", *arguments],
        cwd=source_dir,
        check=False,
        capture_output=True,
        text=True,
    )


OIDC_EXCHANGE_MARKERS = ("OIDC token exchange error", "ENEEDAUTH")


def trusted_publisher_identity(package_name: str, environment: str) -> dict[str, str]:
    """Claims this job's GitHub OIDC token presents to npm's exchange."""
    repository = os.environ.get("GITHUB_REPOSITORY", "<unset GITHUB_REPOSITORY>")
    workflow_ref = os.environ.get("GITHUB_WORKFLOW_REF", "")
    workflow_path = workflow_ref.split("@", 1)[0].split("/", 2)[-1]
    workflow_file = Path(workflow_path).name or "<unset GITHUB_WORKFLOW_REF>"
    owner, _, repo = repository.partition("/")
    return {
        "package": package_name,
        "organization": owner,
        "repository": repository,
        "repository name": repo,
        "workflow filename": workflow_file,
        "environment": environment,
    }


def mismatch_summary(identity: dict[str, str]) -> str:
    return (
        f"npm trusted publisher for {identity['package']} does not match "
        f"{identity['repository']} {identity['workflow filename']} "
        f"(environment: {identity['environment']})"
    )


def print_identity(identity: dict[str, str]) -> None:
    print("npm trusted publisher settings this job requires (npmjs.com ->")
    print(
        f"{identity['package']} -> Settings -> Trusted publishing -> GitHub Actions):"
    )
    for key in (
        "package",
        "organization",
        "repository name",
        "workflow filename",
        "environment",
    ):
        print(f"  {key}: {identity[key]}")


def write_step_summary(text: str) -> None:
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(text + "\n")


def publish(source_dir: Path, environment: str = "release") -> bool:
    package = json.loads((source_dir / "package.json").read_text(encoding="utf-8"))
    package_spec = f"{package['name']}@{package['version']}"
    lookup = run_npm(["view", package_spec, "version", "--json"], source_dir)
    if lookup.returncode == 0:
        try:
            published_version = json.loads(lookup.stdout)
        except json.JSONDecodeError:
            published_version = None
        if published_version == package["version"]:
            print(f"{package_spec} is already published; skipping npm publish.")
            return False

    identity = trusted_publisher_identity(package["name"], environment)
    print_identity(identity)
    result = run_npm(["publish"], source_dir)
    if result.stdout:
        print(result.stdout, end="")
    if result.returncode != 0:
        detail = result.stderr.strip() or "npm publish failed without an error message"
        if any(marker in detail for marker in OIDC_EXCHANGE_MARKERS):
            summary = mismatch_summary(identity)
            write_step_summary(f"### npm publish\n\n{summary}\n")
            print_identity(identity)
            detail = f"{detail}\n{summary}"
        raise RuntimeError(detail)
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path, default=Path.cwd())
    parser.add_argument(
        "--environment",
        default="release",
        help="GitHub environment of the publishing job (npm trusted-publisher claim)",
    )
    args = parser.parse_args()
    publish(args.source_dir.resolve(), args.environment)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, KeyError, TypeError, RuntimeError, json.JSONDecodeError) as exc:
        raise SystemExit(f"npm publication failed: {exc}") from exc
