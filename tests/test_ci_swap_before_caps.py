"""soldr#3148 step 1: swap (rung 3) precedes every compile cap (rung 4).

CLAUDE.md's concurrency ladder puts "give the machine headroom" before "cap
concurrency". These three lanes carried `CARGO_BUILD_JOBS` / `SOLDR_JOBS` caps
with no swap at all, so a canary that lifted a cap had no backstop. This pins
the order: in each job, `setup_ci_swap.sh` runs before the first step that sets
a cap, whether through a step `env:` block or a `$GITHUB_ENV` write.
"""

from __future__ import annotations

from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
CAP_KEYS = ("CARGO_BUILD_JOBS", "SOLDR_JOBS")

JOBS = [
    ("cook-size-gate.yml", "cook-size-gate"),
    ("auto-release.yml", "build"),
]


def _steps(workflow: str, job: str) -> list[dict]:
    document = yaml.safe_load((WORKFLOWS / workflow).read_text(encoding="utf-8"))
    return document["jobs"][job]["steps"]


def _sets_cap(step: dict) -> bool:
    env = step.get("env") or {}
    if any(key in env for key in CAP_KEYS):
        return True
    run = step.get("run") or ""
    return any(f"{key}=" in run and "GITHUB_ENV" in run for key in CAP_KEYS)


def _enlarges_swap(step: dict) -> bool:
    return "setup_ci_swap.sh" in (step.get("run") or "")


@pytest.mark.parametrize(("workflow", "job"), JOBS)
def test_swap_is_enabled_before_the_first_compile_cap(workflow: str, job: str) -> None:
    steps = _steps(workflow, job)
    capped = [index for index, step in enumerate(steps) if _sets_cap(step)]
    swap = [index for index, step in enumerate(steps) if _enlarges_swap(step)]
    assert capped, f"{workflow}:{job} no longer sets a compile cap; update this test"
    assert swap, f"{workflow}:{job} caps compile parallelism without enlarging swap"
    assert swap[0] < capped[0], (
        f"{workflow}:{job} enlarges swap at step {swap[0]}, after its first cap at "
        f"step {capped[0]}"
    )


def test_perf_matrix_bench_keeps_swap_and_sets_no_cap() -> None:
    # soldr#3148: the sqlite-link fixture's GITHUB_ENV cap is gone; exclusive
    # admission serializes the sqlite3.c amalgamation on its own.
    steps = _steps("perf-matrix.yml", "bench")
    assert not any(_sets_cap(step) for step in steps)
    assert any(_enlarges_swap(step) for step in steps)


def test_the_cap_detector_sees_both_spellings() -> None:
    assert _sets_cap({"env": {"CARGO_BUILD_JOBS": "1"}})
    assert _sets_cap({"run": 'echo "SOLDR_JOBS=1" >> "$GITHUB_ENV"'})
    assert not _sets_cap({"run": "echo SOLDR_JOBS is documented here"})
    assert not _sets_cap({"env": {"RUSTFLAGS": "-C debuginfo=0"}})
