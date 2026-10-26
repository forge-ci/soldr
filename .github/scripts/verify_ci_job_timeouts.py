"""Verify that every job which allocates a runner has a bounded runtime.

soldr#1978 item 8. This used to check `ci.yml` alone, which left the two jobs
that run on *every* PR -- `_build-and-test.yml` and `_bootstrap-e2e.yml` --
sitting on GitHub's 360-minute default until soldr#1977 added timeouts by hand.

The per-job rule was already right: a job with `runs-on` needs a timeout, and a
reusable-workflow *caller* (`uses:` with no `runs-on`) cannot have one. What was
missing is that the reusable workflow *files themselves* contain jobs with
`runs-on`, and nothing ever opened them. Walking the whole directory makes the
regression class impossible rather than fixed-once.
"""

from __future__ import annotations

import re
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

JOB_HEADER = re.compile(r"^  ([A-Za-z0-9_-]+):\s*(?:#.*)?$")
TOP_LEVEL_KEY = re.compile(r"^    ([A-Za-z0-9_-]+):(?:\s*(.*))?$")
INTEGER = re.compile(r"^[0-9]+$")
# soldr#2615: a reusable workflow may size its budget by a boolean input
# (`run_pep517_smoke` grows the target-run job only when the wheel smoke
# rides along). Additional boolean gates may select other static integer
# budgets (#2701), so every outcome remains fully verifiable while arbitrary
# expressions stay rejected.
# Each gate is either a boolean input (`inputs.flag && 65`) or an input compared
# against a quoted literal (`inputs.mode == 'x86_64-recovery' && 30`, soldr#3076);
# both are static and every branch is still an integer that gets range-checked.
CONDITIONAL = re.compile(
    r"^\$\{\{\s*(?:inputs\.[A-Za-z0-9_-]+(?:\s*==\s*'[^']*')?\s*&&\s*[0-9]+\s*\|\|\s*)+"
    + r"[0-9]+\s*\}\}$"
)


def _timeout_value_is_valid(value: str) -> bool:
    """A literal 1..360 integer, or boolean-input gates over static values."""

    if INTEGER.fullmatch(value):
        return 1 <= int(value) <= 360
    if CONDITIONAL.fullmatch(value) is None:
        return False
    branches = re.findall(r"(?:&&|\|\|)\s*([0-9]+)", value)
    return bool(branches) and all(1 <= int(branch) <= 360 for branch in branches)


@dataclass(frozen=True)
class JobBlock:
    job_id: str
    lines: tuple[str, ...]


def _job_blocks(workflow: str) -> list[JobBlock]:
    """Return top-level job blocks from the workflow's ``jobs`` section."""

    in_jobs = False
    current_id: str | None = None
    current_lines: list[str] = []
    blocks: list[JobBlock] = []

    for line in workflow.splitlines():
        if not in_jobs:
            if line == "jobs:":
                in_jobs = True
            continue

        # A non-indented line starts a new YAML top-level section.
        if line and not line.startswith(" "):
            break

        match = JOB_HEADER.match(line)
        if match:
            if current_id is not None:
                blocks.append(JobBlock(current_id, tuple(current_lines)))
            current_id = match.group(1)
            current_lines = []
        elif current_id is not None:
            current_lines.append(line)

    if current_id is not None:
        blocks.append(JobBlock(current_id, tuple(current_lines)))
    return blocks


def _timeout_values(lines: Sequence[str]) -> list[str]:
    """Read only four-space job keys, excluding nested step keys."""

    values: list[str] = []
    for line in lines:
        match = TOP_LEVEL_KEY.match(line)
        if match and match.group(1) == "timeout-minutes":
            values.append((match.group(2) or "").split("#", 1)[0].strip())
    return values


def find_timeout_violations(workflow: str) -> list[str]:
    """Return actionable errors for direct jobs without valid job timeouts."""

    violations: list[str] = []
    for block in _job_blocks(workflow):
        job_id, lines = block.job_id, block.lines
        keys = {
            match.group(1) for line in lines if (match := TOP_LEVEL_KEY.match(line))
        }
        if "uses" in keys and "runs-on" not in keys:
            # Reusable-workflow callers cannot use jobs.<id>.timeout-minutes.
            continue
        if "runs-on" not in keys:
            continue

        values = _timeout_values(lines)
        if not values:
            violations.append(f"{job_id}: missing job-level timeout-minutes")
            continue
        if len(values) != 1:
            violations.append(
                f"{job_id}: expected exactly one job-level timeout-minutes, found {len(values)}"
            )
            continue
        value = values[0]
        if not _timeout_value_is_valid(value):
            violations.append(
                f"{job_id}: timeout-minutes must be an integer from 1 to 360 (got {value!r})"
            )
    return violations


# Workflows that predate this check and still have unbounded jobs.
#
# soldr#1978 item 8: extending the walk to every workflow surfaced a set of
# jobs sitting on GitHub's 360-minute default. They are recorded rather than
# silently skipped, and rather than blocked -- the same trade `loc_ratchet`
# makes for the files already over the line ceiling. The live count is printed
# by `main()` on every run, so it is deliberately not repeated here: a number
# in a comment goes stale the first time someone burns an entry down, which is
# exactly what happened after the first three.
#
# Burned down so far: benchmark-stats (#2057), parent-cache-bench (#2058),
# cache-delta-experiment (#2067), auto-release + vcpkg-windows-refresh (#2077).
#
# The point of the extension is that a *new* workflow cannot join this list.
# Removing an entry is a burn-down PR; picking the right bound for a perf or
# release job needs someone who knows how long it legitimately runs, and a
# too-low timeout that kills a healthy long build is worse than the default.
# `test_grandfathered_entries_still_need_the_exemption` fails if every job in a
# listed file has since been bounded, so a spent entry cannot linger.
#
# The list is now empty. The last two entries were held for *evidence*, not
# effort: bounds for the burned-down files came from measured job history, and
# an earlier pass found none for these two. Widening the sample to 20 runs
# changed that for `perf-matrix` -- real cells have since run, not just the
# `gate` job:
#
#   build-soldr  max  6.0 min      bench  max  4.1 min
#   gate / setup / evaluate  ~0.1 min
#
# so those are bounded from history with 7x or better headroom. Two caveats are
# deliberately reflected in the numbers rather than papered over:
#
#   * Only the `medium` and `sqlite-link` fixtures have ever run. A heavier
#     fixture is legitimately slower, so `bench` is set to 120 -- ~29x the
#     observed max -- rather than something snug.
#   * `perf-cold-warm` still has zero completed runs. Its jobs are bounded by
#     analogy (same ubuntu-24.04 runner, same build work as perf-matrix) and
#     loosely: 90 for the two build jobs.
#
# The original caution stands and is worth restating for whoever edits these
# next: a too-low timeout that kills a healthy long build is worse than the
# 360-minute default. These bounds exist to convert an unbounded hang into a
# bounded failure, not to police perf runtimes. If a legitimate sweep ever
# approaches one of them, raise it -- that is not a regression.
GRANDFATHERED: frozenset[str] = frozenset()


def workflow_paths(root: Path) -> list[Path]:
    """Every workflow file, including reusable ones.

    Sorted so failure output is stable across runs and platforms.
    """

    return sorted(
        path for pattern in ("*.yml", "*.yaml") for path in root.glob(pattern)
    )


def main(workflow_path: Path | None = None) -> int:
    if workflow_path is not None:
        paths = [workflow_path]
    else:
        workflows = Path(__file__).resolve().parents[2] / ".github" / "workflows"
        paths = workflow_paths(workflows)
        if not paths:
            print(f"no workflows found under {workflows}", file=sys.stderr)
            return 1

    failed = False
    skipped = 0
    for path in paths:
        violations = find_timeout_violations(path.read_text(encoding="utf-8"))
        if violations and path.name in GRANDFATHERED:
            skipped += 1
            print(
                f"note: {path.name} has {len(violations)} unbounded job(s), "
                "grandfathered by soldr#1978 item 8"
            )
            continue
        if violations:
            failed = True
            print(f"workflow timeout policy failed: {path}", file=sys.stderr)
            for violation in violations:
                print(f"- {violation}", file=sys.stderr)
    if failed:
        return 1

    print(
        f"workflow timeout policy passed: {len(paths)} workflow(s)"
        + (f", {skipped} grandfathered" if skipped else "")
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
