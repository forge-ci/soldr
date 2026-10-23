#!/usr/bin/env python3
"""Run `soldr cargo audit` with soldr#3785's blocking policy.

A RustSec advisory is published by a third party at an arbitrary moment. With
an unconditional `cargo audit` gate, that moment turns every open PR red,
including PRs that touch no dependency (2026-10-15). The report is still
wanted everywhere; only *blocking* is scoped:

- **Blocks** on any event that is not a pull request (a push to `main`, a
  release-candidate dispatch, a schedule), so a new advisory surfaces on
  `main` promptly.
- **Blocks** on a pull request (or a local run) whose diff against the merge
  base changes `Cargo.lock` or any `Cargo.toml`: that PR owns its dependency
  graph.
- **Reports without blocking** on any other pull request, with a loud warning
  naming the policy.

Fails closed: if the merge base or the diff cannot be computed, a finding
blocks.

Usage:
    python .github/scripts/audit_policy.py --base-ref origin/main
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from collections.abc import Callable, Sequence
from pathlib import Path

# File-backed capture (PY-003): no capture-then-wait through a pipe.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

AUDIT_COMMAND = ("soldr", "cargo", "audit")


def is_dependency_manifest(path: str) -> bool:
    name = path.replace("\\", "/").rsplit("/", 1)[-1]
    return name in ("Cargo.lock", "Cargo.toml")


def changed_paths(base_ref: str) -> list[str] | None:
    """Paths changed since the merge base with `base_ref`, or None on error."""

    try:
        merge_base = run_captured(
            ["git", "merge-base", base_ref, "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        diff = run_captured(
            ["git", "diff", "--name-only", merge_base, "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as exc:
        print(f"audit policy: cannot diff against {base_ref}: {exc}", file=sys.stderr)
        return None
    return [line for line in diff.splitlines() if line.strip()]


def finding_blocks(event: str | None, changed: Sequence[str] | None) -> bool:
    """Whether a failing audit fails this run (see module docstring)."""

    if event and event != "pull_request":
        return True
    if changed is None:
        return True
    return any(is_dependency_manifest(path) for path in changed)


def run_audit() -> int:
    return subprocess.run(AUDIT_COMMAND, check=False).returncode


def main(
    argv: list[str] | None = None,
    *,
    audit: Callable[[], int] = run_audit,
    diff: Callable[[str], list[str] | None] = changed_paths,
) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", default="origin/main")
    args = parser.parse_args(argv)

    code = audit()
    if code == 0:
        return 0
    event = os.environ.get("GITHUB_EVENT_NAME") or None
    changed = None if event and event != "pull_request" else diff(args.base_ref)
    if finding_blocks(event, changed):
        return code
    print(
        "::warning::cargo audit reported advisories, but this change touches no "
        "Cargo.lock/Cargo.toml, so it does not block (soldr#3785). It blocks on "
        "main, on release candidates, and on any PR that changes dependencies.",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
