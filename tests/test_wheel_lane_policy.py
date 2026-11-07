"""The `soldr wheel` verification lane must be gated, and gated correctly.

Two failure modes, opposite in cost:

* the lane runs on every PR -- an extra ubuntu-24.04 cross compile per push,
  which is exactly the spend soldr#1978 is cutting;
* the lane never runs -- soldr#2139's acceptance criterion goes back to being
  unverified, which is how it got here.

These tests pin both ends, plus the workflow wiring that makes an unselected
matrix allocate no runner at all.
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

import pytest
import yaml

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "wheel_lane_policy.py"
CI_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "ci.yml"


def _load_policy():
    spec = importlib.util.spec_from_file_location("wheel_lane_policy", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


policy = _load_policy()


@pytest.mark.parametrize(
    "event_name",
    ["push", "workflow_dispatch", "schedule", ""],
)
def test_non_pull_request_events_always_run(event_name: str) -> None:
    decision = policy.decide_wheel_lane(event_name=event_name, changed_paths=[])
    assert decision.run, decision.reason


@pytest.mark.parametrize(
    "path",
    [
        "crates/soldr-cli/src/wheel_cmd.rs",
        "pyproject.toml",
        ".github/workflows/ci.yml",
        ".github/scripts/build_release_wheel.py",
        ".github/scripts/verify_wheel_glibc.py",
        ".github/scripts/wheel_lane_policy.py",
    ],
)
def test_wheel_relevant_paths_select_the_lane(path: str) -> None:
    decision = policy.decide_wheel_lane(
        event_name="pull_request", changed_paths=["README.md", path]
    )
    assert decision.run, decision.reason
    assert path in decision.reason


def test_unrelated_pull_request_skips_the_lane() -> None:
    decision = policy.decide_wheel_lane(
        event_name="pull_request",
        changed_paths=[
            "README.md",
            "crates/soldr-cache/src/lib.rs",
        ],
    )
    assert not decision.run, decision.reason


def test_empty_diff_fails_open() -> None:
    # A gate that silently stops gating is worse than one runner-minute.
    decision = policy.decide_wheel_lane(event_name="pull_request", changed_paths=[])
    assert decision.run, decision.reason


def test_windows_style_paths_are_normalized() -> None:
    assert policy.is_wheel_relevant_path("crates\\soldr-cli\\src\\wheel_cmd.rs")
    assert policy.is_wheel_relevant_path("./pyproject.toml")


def test_matrix_cells_cover_cross_and_host_target_builds() -> None:
    # soldr#2139's criterion is a CROSS wheel from an x86_64 Linux host;
    # soldr#3432's is the HOST-target wheel on that same host, which must get
    # the catalogue 2.17 sysroot too rather than the runner's glibc.
    cells = {cell["target"]: cell for cell in policy.WHEEL_MATRIX}
    assert set(cells) == {"aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"}
    for cell in cells.values():
        assert cell["runner"] == "ubuntu-24.04"
        assert cell["expected_tag"] == "manylinux_2_17"
        assert cell["max_glibc"] == "2.17"
        assert cell["jobs"] == "1"
    assert len({cell["name"] for cell in policy.WHEEL_MATRIX}) == len(
        policy.WHEEL_MATRIX
    )


def _ci_jobs() -> dict:
    return yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))["jobs"]


def test_ci_lane_is_selected_by_the_policy_matrix() -> None:
    jobs = _ci_jobs()
    assert "wheel-cross-policy" in jobs
    lane = jobs["wheel-cross-verify"]

    # Matrix selection, not a step-level `if:` -- an unselected cell must not
    # allocate a runner.
    include = lane["strategy"]["matrix"]["include"]
    assert "fromJSON" in include
    assert "needs.wheel-cross-policy.outputs.matrix" in include
    assert "wheel-cross-policy" in lane["needs"]

    # And it consumes the shared bootstrap soldr rather than building a
    # fourth copy.
    assert "e2e-cross-bootstrap-soldr" in lane["needs"]
    names = [step.get("name", "") for step in lane["steps"]]
    runs = "\n".join(str(step.get("run", "")) for step in lane["steps"])
    assert any("bootstrap soldr artifact" in name for name in names)
    assert "soldr wheel" in "\n".join(names) or "wheel --release" in runs


def test_ci_lane_runs_the_verb_and_both_checks() -> None:
    lane = _ci_jobs()["wheel-cross-verify"]
    steps = lane["steps"]
    runs = "\n".join(str(step.get("run", "")) for step in steps)

    # The verb under test, in the grammar it now has.
    assert "prepare_release_wheel.py" in runs
    assert '--driver-dir "$RUNNER_TEMP/soldr-bin"' in runs
    assert '--expected-version "${version#soldr }"' in runs
    # The bytes check, not just the name check.
    assert "verify_wheel_glibc.py" in runs
    assert "--max-glibc" in runs
    # The name check lives in a `shell: python` step.
    tag_step = next(
        step for step in steps if "filename" in step.get("name", "").lower()
    )
    assert tag_step.get("shell") == "python"
    assert "EXPECTED_TAG" in tag_step["env"]
    smoke = next(
        step for step in steps if "Smoke the release helper" in step.get("name", "")
    )
    assert smoke["if"] == "matrix.target == 'x86_64-unknown-linux-gnu'"
    assert "smoke_release_wheel.py" in smoke["run"]


@pytest.mark.parametrize(
    "path",
    [".github/workflows/release-auto.yml", ".github/scripts/prepare_release_wheel.py"],
)
def test_release_wheel_changes_select_canary(path: str) -> None:
    """#3436: a change to the shipping path must select its wheel canary."""
    assert policy.decide_wheel_lane(event_name="pull_request", changed_paths=[path]).run


def test_policy_script_emits_a_json_matrix(tmp_path: Path) -> None:
    # The workflow feeds this straight into fromJSON; a non-JSON value would
    # fail at schedule time, not at test time.
    out = tmp_path / "out.txt"
    out.touch()

    run_captured(
        [
            sys.executable,
            str(SCRIPT),
            "--event-name",
            "push",
            "--github-output",
            str(out),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    lines = dict(
        line.split("=", 1) for line in out.read_text(encoding="utf-8").splitlines()
    )
    assert json.loads(lines["matrix"]) == policy.WHEEL_MATRIX
    assert lines["run_wheel_lane"] == "true"
