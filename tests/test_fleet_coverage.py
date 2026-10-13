"""Portable exact-SHA coverage interface for soldr#3345."""

import hashlib
import json
from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).parents[1]
COVERAGE = load_script_module(
    ROOT / ".github/scripts/ci_full_coverage.py", "fleet_coverage"
)
SHA = "a" * 40


def contract():
    return {
        "schema_version": 1,
        "required_jobs": ["lint", "board-suite"],
        "targets": [
            {
                "triple": "embedded-board",
                "ci": {"build_job": "board-build", "run_job": "board-run"},
            }
        ],
    }


def states():
    return {
        job: {"result": "success"}
        for job in ["lint", "board-suite", "board-build", "board-run"]
    }


def test_foreign_adapter_does_not_inherit_soldr_jobs():
    assert COVERAGE.required_jobs(COVERAGE.JsonDocument(contract())) == set(states())
    assert (
        COVERAGE.coverage_failures(
            COVERAGE.JsonDocument(contract()),
            COVERAGE.JsonDocument(states()),
            expected_sha=SHA,
            selected_sha=SHA,
        )
        == []
    )


@pytest.mark.parametrize(
    "status", ["skipped", "cancelled", "neutral", "failure", "", None]
)
def test_each_unsuccessful_cell_refuses_full_coverage(status):
    needs = states()
    needs["board-run"]["result"] = status
    assert COVERAGE.coverage_failures(
        COVERAGE.JsonDocument(contract()),
        COVERAGE.JsonDocument(needs),
        expected_sha=SHA,
        selected_sha=SHA,
    )


def test_missing_cell_and_wrong_sha_refuse_full_coverage():
    needs = states()
    del needs["board-run"]
    assert COVERAGE.coverage_failures(
        COVERAGE.JsonDocument(contract()),
        COVERAGE.JsonDocument(needs),
        expected_sha=SHA,
        selected_sha=SHA,
    ) == ["board-run: missing"]
    assert COVERAGE.coverage_failures(
        COVERAGE.JsonDocument(contract()),
        COVERAGE.JsonDocument(states()),
        expected_sha=SHA,
        selected_sha="b" * 40,
    )


@pytest.mark.parametrize("jobs", [[], [""], ["lint", "lint"], "lint", [None]])
def test_invalid_adapter_required_job_list_is_rejected(jobs):
    manifest = contract()
    manifest["required_jobs"] = jobs
    with pytest.raises(ValueError):
        COVERAGE.required_jobs(COVERAGE.JsonDocument(manifest))


@pytest.mark.parametrize("selected_sha,passed", [(SHA, True), ("b" * 40, False)])
def test_cli_writes_manifest_and_candidate_bound_report(
    tmp_path, monkeypatch, selected_sha, passed
):
    manifest = tmp_path / "adapter.json"
    manifest.write_text(json.dumps(contract()))
    report = tmp_path / "reports" / "coverage.json"
    monkeypatch.setenv("CI_NEEDS_JSON", json.dumps(states()))
    monkeypatch.setattr(
        "sys.argv",
        [
            "coverage",
            "--contract",
            str(manifest),
            "--expected-sha",
            SHA,
            "--selected-sha",
            selected_sha,
            "--report",
            str(report),
        ],
    )
    assert COVERAGE.main() == (0 if passed else 1)
    result = json.loads(report.read_text())
    assert result["schema"] == "fleet-ci-coverage/v1"
    assert (
        result["manifest_sha256"] == hashlib.sha256(manifest.read_bytes()).hexdigest()
    )
    assert result["candidate_sha"] == SHA
    assert result["selected_sha"] == selected_sha
    assert result["required_jobs"] == sorted(states())
    assert result["success"] is passed
    assert bool(result["failures"]) is (not passed)


def test_cli_cannot_report_green_without_candidate_identity(tmp_path, monkeypatch):
    manifest = tmp_path / "adapter.json"
    manifest.write_text(json.dumps(contract()))
    report = tmp_path / "coverage.json"
    monkeypatch.setenv("CI_NEEDS_JSON", json.dumps(states()))
    monkeypatch.setattr(
        "sys.argv", ["coverage", "--contract", str(manifest), "--report", str(report)]
    )
    with pytest.raises(ValueError, match="both candidate SHAs"):
        COVERAGE.main()
    assert not report.exists()


def test_normalized_uppercase_candidate_is_the_same_identity():
    assert (
        COVERAGE.coverage_failures(
            COVERAGE.JsonDocument(contract()),
            COVERAGE.JsonDocument(states()),
            expected_sha=SHA.upper(),
            selected_sha=SHA,
        )
        == []
    )


def test_soldr_wires_both_candidate_inputs_and_uploads_each_attempt():
    workflow = (ROOT / ".github/workflows/ci.yml").read_text()
    job = workflow.split("  full-coverage:\n", 1)[1].split(
        "  cancel-on-bootstrap-failure:", 1
    )[0]
    assert (
        "EXPECTED_SHA: ${{ inputs.candidate_sha || github.event.pull_request.head.sha || github.sha }}"
        in job
    )
    assert "SELECTED_SHA: ${{ needs.ci-mode.outputs.checkout_sha }}" in job
    assert '--expected-sha "$EXPECTED_SHA" --selected-sha "$SELECTED_SHA"' in job
    assert "full-coverage.json" in job
    assert "name: ci-full-coverage-${{ github.run_attempt }}" in job
    assert "if: always()" in job


@pytest.mark.parametrize(
    "field,value",
    [
        ("build_job", ""),
        ("build_job", None),
        ("run_job", "with space"),
        ("run_job", []),
    ],
)
def test_malformed_target_job_ids_are_rejected(field, value):
    manifest = contract()
    manifest["targets"][0]["ci"][field] = value
    with pytest.raises(ValueError, match="valid job ID"):
        COVERAGE.required_jobs(COVERAGE.JsonDocument(manifest))


def test_duplicate_portable_target_identities_are_rejected():
    manifest = contract()
    manifest["targets"].append(manifest["targets"][0].copy())
    with pytest.raises(ValueError, match="unique"):
        COVERAGE.required_jobs(COVERAGE.JsonDocument(manifest))


@pytest.mark.parametrize("version", [1.0, True, "1", None, 2])
def test_portable_schema_version_requires_integer_one(version):
    manifest = contract()
    manifest["schema_version"] = version
    with pytest.raises(ValueError, match="schema_version 1"):
        COVERAGE.required_jobs(COVERAGE.JsonDocument(manifest))


def test_default_contract_report_requires_candidate_identity(tmp_path, monkeypatch):
    report = tmp_path / "coverage.json"
    monkeypatch.setenv("CI_NEEDS_JSON", "{}")
    monkeypatch.setattr("sys.argv", ["coverage", "--report", str(report)])
    with pytest.raises(ValueError, match="both candidate SHAs"):
        COVERAGE.main()
    assert not report.exists()
