"""Release publication requires a completed exact-candidate full CI run."""

from __future__ import annotations

import importlib.util
from dataclasses import dataclass
from pathlib import Path
from types import SimpleNamespace

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".github" / "scripts" / "release_full_ci_gate.py"
spec = importlib.util.spec_from_file_location("release_full_ci_gate", SCRIPT)
assert spec and spec.loader
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
SHA = "a" * 40
REPO = "zackees/soldr"


@dataclass(frozen=True)
class Proof:
    """A CI run payload and its jobs payload, as the GitHub API returns them."""

    run: dict
    jobs: list[dict]


def proof() -> Proof:
    run = {
        "event": "workflow_dispatch",
        "path": ".github/workflows/ci.yml",
        "head_branch": "main",
        "repository": {"full_name": REPO},
        "display_title": f"CI full {SHA}",
        "status": "completed",
        "conclusion": "success",
    }
    jobs = [
        {"name": name, "status": "completed", "conclusion": "success"}
        for name in ("CI mode", "Full coverage")
    ]
    return Proof(run, jobs)


def test_exact_sha_full_run_succeeds() -> None:
    fixture = proof()
    run, jobs = fixture.run, fixture.jobs
    gate.validate(run, jobs, sha=SHA, repository=REPO)


@pytest.mark.parametrize(
    "mutation",
    ["wrong_sha", "pr", "feature_branch", "failed_run", "missing", "skipped", "failed"],
)
def test_nonmatching_or_incomplete_proof_is_rejected(mutation: str) -> None:
    fixture = proof()
    run, jobs = fixture.run, fixture.jobs
    if mutation == "wrong_sha":
        run["display_title"] = "CI full " + "b" * 40
    elif mutation == "pr":
        run["event"] = "pull_request"
    elif mutation == "feature_branch":
        run["head_branch"] = "feat/spoofed-full-coverage"
    elif mutation == "failed_run":
        run["conclusion"] = "failure"
    elif mutation == "missing":
        jobs.pop()
    else:
        jobs[-1]["conclusion"] = mutation
    with pytest.raises(gate.GateError):
        gate.validate(run, jobs, sha=SHA, repository=REPO)


def test_workflow_requires_gate_before_build_and_preserves_npm_recovery() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github" / "workflows" / "auto-release.yml").read_text()
    )
    assert "push" not in workflow.get("on", workflow.get(True, {}))
    jobs = workflow["jobs"]
    assert "full_ci_gate" in jobs["build"]["needs"]
    assert "needs.full_ci_gate.result == 'success'" in jobs["build"]["if"]
    assert "needs.prepare.result == 'skipped'" in jobs["publish-npm"]["if"]
    assert "inputs.npm_release_ref != ''" in jobs["publish-npm"]["if"]
    assert "validate_npm_release_recovery.py" in str(jobs["publish-npm"]["steps"])


def test_release_gate_scripts_use_pinned_uv_after_setup() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github" / "workflows" / "auto-release.yml").read_text()
    )
    for job_name in ("prepare", "full_ci_gate"):
        steps = workflow["jobs"][job_name]["steps"]
        setup_index = next(
            index
            for index, step in enumerate(steps)
            if str(step.get("uses", "")).startswith("astral-sh/setup-uv@")
        )
        gate_index = next(
            index
            for index, step in enumerate(steps)
            if "release_full_ci_gate.py" in step.get("run", "")
        )
        assert setup_index < gate_index
        assert steps[gate_index]["run"].startswith(
            "uv run --no-project --python 3.13 python .github/scripts/release_full_ci_gate.py "
        )


def test_npm_recovery_cannot_bypass_full_candidate_gate() -> None:
    jobs = yaml.safe_load((ROOT / ".github/workflows/auto-release.yml").read_text())[
        "jobs"
    ]
    assert "full_ci_gate" in jobs["publish-npm"]["needs"]
    assert jobs["publish-npm"]["if"].startswith(
        "always() && needs.full_ci_gate.result == 'success' &&"
    )
    gate_job = jobs["full_ci_gate"]
    assert "always()" in gate_job["if"]
    assert "inputs.npm_release_ref != ''" in gate_job["if"]
    verifier = next(
        step
        for step in gate_job["steps"]
        if step.get("name") == "Verify completed full CI run and required jobs"
    )
    assert "inputs.candidate_sha" in verifier["env"]["CANDIDATE_SHA"]
    recovery_step = next(
        step
        for step in jobs["publish-npm"]["steps"]
        if step.get("name") == "Verify npm recovery candidate source"
    )
    assert "--verify-candidate" in recovery_step["run"]
    assert recovery_step["env"]["CANDIDATE_SHA"] == "${{ inputs.candidate_sha }}"
    steps = jobs["publish-npm"]["steps"]
    assert steps.index(recovery_step) < next(
        i
        for i, step in enumerate(steps)
        if step.get("name") == "Publish package to npm"
    )


@pytest.mark.parametrize(
    "result", ["", "skipped", "failure", "cancelled", "neutral", "success"]
)
def test_actual_recovery_publish_condition_requires_success(result: str) -> None:
    jobs = yaml.safe_load((ROOT / ".github/workflows/auto-release.yml").read_text())[
        "jobs"
    ]
    expression = jobs["publish-npm"]["if"].replace("&&", " and ").replace("||", " or ")
    # Evaluate the actual workflow boolean expression with a recovery event;
    # the skipped normal release graph must not authorize publication.
    context = {
        "always": lambda: True,
        "inputs": SimpleNamespace(npm_release_ref="v1.2.3"),
        "needs": SimpleNamespace(
            full_ci_gate=SimpleNamespace(result=result),
            prepare=SimpleNamespace(result="skipped"),
        ),
    }
    # Expression is read only from this repository workflow, with no builtins.
    # pylint: disable=eval-used
    assert eval(expression, {"__builtins__": {}}, context) is (result == "success")
