#!/usr/bin/env python3
"""Enforce the per-file line ceiling in CI, as a ratchet (soldr#1966).

The 1,500-line ceiling was enforced only by a PostToolUse hook. A hook fires
when an agent edits a file; it does not fire for a human edit, a squash merge,
or anything CI does. So files drifted past the ceiling by every other route,
and the cost landed on whichever agent next had to touch one -- as a hard
block demanding a refactor before an unrelated fix could proceed.

That refactor is a *rename*, and renames conflict destructively: soldr#1962
became a modify/delete conflict against the soldr#1960 split, where resolving
it by taking the delete compiles, passes CI, and silently ships the fix
missing. The biggest files are the popular ones, so the blast radius is
largest exactly where splitting is most dangerous.

A plain threshold in CI cannot be adopted here -- 13 files are already over,
and blocking every PR that touches them would be worse than the status quo.
So this is a **ratchet** rather than a threshold:

* a file at or under the ceiling must stay at or under it;
* a file already over may not get *bigger*;
* shrinking is always allowed.
* Rust `mod.rs` files are exempt because they are module aggregation surfaces
  whose size does not reliably describe a monolithic implementation.

That needs no hand-maintained grandfather list -- the baseline is simply the
file's size at the merge base, which is always correct and never goes stale.
It also puts the cost on the change that causes it, while the diff is small,
instead of on an unrelated change months later.

Usage:
    loc_ratchet.py --base-ref origin/main [--ceiling 1500] [--paths crates]
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

DEFAULT_CEILING = 1500
# Only source we own; generated files are not something a PR author can
# reasonably split.
DEFAULT_ROOTS = ("crates",)
SUFFIX = ".rs"
EXEMPT_NAMES = {"mod.rs"}


@dataclass(frozen=True)
class Violation:
    path: str
    lines: int
    baseline: int | None  # None => the file is new on this branch

    def describe(self, ceiling: int) -> str:
        if self.baseline is None:
            return (
                f"{self.path}: new file is {self.lines} lines (ceiling {ceiling}). "
                f"Split it now, while it has no history to conflict with."
            )
        return (
            f"{self.path}: grew {self.baseline} -> {self.lines} lines "
            f"(ceiling {ceiling}). A file already over the ceiling may not get "
            f"bigger; move the addition into a new module instead."
        )


class NoMergeBase(Exception):
    """The base commit is unreachable — usually a shallow CI checkout."""


def _run(args: list[str]) -> str:
    return run_captured(args, check=True, capture_output=True, text=True).stdout


def resolve_base(base_ref: str, base_sha: str | None) -> str:
    """The commit to compare against.

    An explicit `base_sha` is used verbatim. That matters in CI: on a
    `pull_request` event the checkout is a shallow merge ref with no common
    history, so `git merge-base` fails and the whole check silently skips --
    which is exactly what the first version of this script did on its own PR
    (soldr#1966). `git diff A B` compares two trees and needs no ancestry
    between them, so given the base SHA the comparison works at depth 1.

    Without one, fall back to a merge base, which is the right answer locally.
    """
    if base_sha:
        return base_sha
    return _merge_base(base_ref)


def _merge_base(base_ref: str) -> str:
    """Merge base of `base_ref` and HEAD, or raise [`NoMergeBase`].

    CI checkouts are shallow by default, so the base commit may simply not be
    present. That must **skip**, not fail: a check that reports a violation it
    cannot substantiate would fail honest PRs, and the first fix anyone reached
    for would be deleting the check.
    """
    try:
        return _run(["git", "merge-base", base_ref, "HEAD"]).strip()
    except subprocess.CalledProcessError as exc:
        raise NoMergeBase(base_ref) from exc


@dataclass(frozen=True, order=True)
class ChangedFile:
    path: str
    baseline_path: str


def changed_files(base: str, roots: tuple[str, ...]) -> list[ChangedFile]:
    """(path, baseline_path) pairs added, modified or renamed since the merge base.

    Deletions are excluded: a path that no longer exists cannot violate a size
    ceiling, and reporting it would block the very splits this check exists to
    encourage. Renames are included and baselined against their pre-rename
    path, so a move cannot reset the ratchet (soldr#3652).
    """
    raw = _run(
        ["git", "diff", "--name-status", "-M", "--diff-filter=AMR", base, "HEAD"]
    )
    out: list[ChangedFile] = []
    for line in raw.splitlines():
        fields = line.rstrip("\n").split("\t")
        if len(fields) < 2 or not fields[0]:
            continue
        status = fields[0][0]
        if status == "R" and len(fields) >= 3:
            baseline_path, path = fields[1], fields[2]
        elif status in ("A", "M"):
            path = baseline_path = fields[1]
        else:
            continue
        if not path.endswith(SUFFIX):
            continue
        if path.rsplit("/", 1)[-1] in EXEMPT_NAMES:
            continue
        if not any(path == r or path.startswith(f"{r}/") for r in roots):
            continue
        out.append(ChangedFile(path=path, baseline_path=baseline_path))
    return sorted(out)


def line_count_at(ref: str, path: str) -> int | None:
    """Line count of `path` at `ref`, or None when it did not exist there."""
    try:
        blob = run_captured(
            ["git", "show", f"{ref}:{path}"],
            check=True,
            capture_output=True,
        ).stdout
    except subprocess.CalledProcessError:
        return None
    if not blob:
        return 0
    return blob.count(b"\n") + (0 if blob.endswith(b"\n") else 1)


def line_count_worktree(path: str) -> int:
    with open(path, "rb") as fh:
        blob = fh.read()
    if not blob:
        return 0
    return blob.count(b"\n") + (0 if blob.endswith(b"\n") else 1)


@dataclass(frozen=True)
class Evaluation:
    violations: tuple[Violation, ...]
    checked: int


def evaluate(
    base_ref: str,
    roots: tuple[str, ...],
    ceiling: int,
    base_sha: str | None = None,
) -> Evaluation:
    base = resolve_base(base_ref, base_sha)
    violations: list[Violation] = []
    checked = 0
    for changed in changed_files(base, roots):
        path, baseline_path = changed.path, changed.baseline_path
        try:
            lines = line_count_worktree(path)
        except OSError:
            # Raced or unreadable: never fail a PR for a file we cannot read.
            continue
        checked += 1
        if lines <= ceiling:
            continue
        baseline = line_count_at(base, baseline_path)
        if baseline is None or lines > baseline:
            violations.append(Violation(path=path, lines=lines, baseline=baseline))
    return Evaluation(violations=tuple(violations), checked=checked)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument(
        "--base-sha",
        default=None,
        help="exact base commit; skips merge-base, which a shallow CI checkout cannot compute",
    )
    parser.add_argument("--ceiling", type=int, default=DEFAULT_CEILING)
    parser.add_argument(
        "--paths",
        nargs="*",
        default=list(DEFAULT_ROOTS),
        help="directory roots to check",
    )
    args = parser.parse_args(argv)

    try:
        evaluation = evaluate(
            args.base_ref, tuple(args.paths), args.ceiling, args.base_sha
        )
    except NoMergeBase:
        print(
            f"loc_ratchet: skipped — no merge base with {args.base_ref} "
            "(shallow checkout?). Not failing the build on a comparison that "
            "could not be made.",
            file=sys.stderr,
        )
        return 0

    if not evaluation.violations:
        print(
            f"loc_ratchet: {evaluation.checked} changed file(s) checked, no violations."
        )
        return 0

    print("loc_ratchet: FAIL", file=sys.stderr)
    for violation in evaluation.violations:
        print(f"  - {violation.describe(args.ceiling)}", file=sys.stderr)
    print(
        "\nThe ceiling is a ratchet: files already over it are allowed to stay, "
        "but not to grow. If this addition genuinely belongs in that file, the "
        "split it is asking for is the change that was already overdue.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
