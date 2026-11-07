"""Fail-closed release execution contract for soldr#3071."""

from __future__ import annotations

import json
from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).parents[1]
CONTRACT = ROOT / "ci" / "canonical-targets.json"
WORKFLOW = ROOT / ".github" / "workflows" / "release-auto.yml"
MODULE = load_script_module(
    ROOT / ".github" / "scripts" / "release_completeness.py",
    "release_execution_contract",
)


def _contract(tmp_path: Path, run_job: str | None) -> Path:
    path = tmp_path / "canonical-targets.json"
    path.write_text(
        json.dumps(
            {
                "targets": [
                    {
                        "triple": "aarch64-apple-darwin",
                        "ci": {
                            "build_job": "e2e-macos-arm64-build",
                            "run_job": run_job,
                        },
                        "release": {
                            "status": "included",
                            "execution_gate": {"required": True, "issue": 3071},
                            "artifact_provenance": {
                                "build": {
                                    "status": "cross-built",
                                    "runner": "ubuntu-24.04",
                                },
                                "execution": {
                                    "archive": {
                                        "status": (
                                            "required-before-publication"
                                            if run_job
                                            else "not-executed"
                                        )
                                    },
                                    "wheel": {
                                        "status": (
                                            "required-before-publication"
                                            if run_job
                                            else "not-executed"
                                        )
                                    },
                                },
                            },
                        },
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    return path


def _workflow(tmp_path: Path, body: str) -> Path:
    path = tmp_path / "ci.yml"
    path.write_text(f"jobs:\n{body}", encoding="utf-8")
    return path


def _release_workflow(tmp_path: Path, gate_job: str = "smoke_macos_arm64") -> Path:
    path = tmp_path / "release-auto.yml"
    path.write_text(
        "jobs:\n"
        f"  {gate_job}:\n"
        "    runs-on: [self-hosted, macOS, ARM64]\n"
        "    steps:\n"
        "      - uses: actions/download-artifact@pinned\n"
        "        with:\n"
        "          name: release-soldr-aarch64-apple-darwin\n"
        "      - uses: actions/download-artifact@pinned\n"
        "        with:\n"
        "          name: pypi-soldr-aarch64-apple-darwin\n"
        "      - run: >-\n"
        "          python ci/smoke_release_artifacts.py\n"
        "          --target aarch64-apple-darwin\n"
        "          --require-wheel-import --require-daemon-cache-smoke\n"
        + "".join(
            f"  {publisher}:\n"
            f"    needs: [{gate_job}]\n"
            f"    if: needs.{gate_job}.result == 'success'\n"
            "    runs-on: ubuntu-24.04\n"
            for publisher in ("publish", "publish-pypi", "publish-npm")
        ),
        encoding="utf-8",
    )
    return path


def test_required_execution_without_run_job_fails_closed(tmp_path: Path) -> None:
    failures = MODULE.release_execution_failures(_contract(tmp_path, None))
    assert failures == [
        "aarch64-apple-darwin: release execution required by soldr#3071 "
        "but ci.run_job is missing"
    ]


def test_required_execution_with_run_job_passes(tmp_path: Path) -> None:
    contract = _contract(tmp_path, "e2e-macos-arm64")
    payload = json.loads(contract.read_text(encoding="utf-8"))
    for record in payload["targets"][0]["release"]["artifact_provenance"][
        "execution"
    ].values():
        record["gate_job"] = "smoke_macos_arm64"
    contract.write_text(json.dumps(payload), encoding="utf-8")
    workflow = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    needs: e2e-macos-arm64-build\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: aarch64-apple-darwin\n",
    )
    assert (
        MODULE.release_execution_failures(
            contract, workflow, _release_workflow(tmp_path)
        )
        == []
    )


def test_nonexistent_or_detached_run_job_does_not_clear_gate(tmp_path: Path) -> None:
    contract = _contract(tmp_path, "e2e-macos-arm64")
    missing = _workflow(tmp_path, "  another-job:\n    runs-on: ubuntu-24.04\n")
    assert "does not exist" in MODULE.release_execution_failures(contract, missing)[0]

    detached = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: x86_64-apple-darwin\n",
    )
    assert (
        "not a target-matched replay"
        in MODULE.release_execution_failures(
            contract, detached, _release_workflow(tmp_path)
        )[0]
    )

    comment_only = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    # e2e-macos-arm64-build is not a dependency here\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: aarch64-apple-darwin\n",
    )
    assert (
        "not a target-matched replay"
        in MODULE.release_execution_failures(
            contract, comment_only, _release_workflow(tmp_path)
        )[0]
    )


def test_execution_claim_cannot_outpace_the_real_run_job(tmp_path: Path) -> None:
    contract = json.loads(_contract(tmp_path, None).read_text(encoding="utf-8"))
    contract["targets"][0]["release"]["artifact_provenance"]["execution"]["archive"] = {
        "status": "required-before-publication",
        "gate_job": "imaginary",
    }
    path = tmp_path / "lying-contract.json"
    path.write_text(json.dumps(contract), encoding="utf-8")
    failures = MODULE.release_execution_failures(path)
    assert any("manifest provenance claims archive" in failure for failure in failures)


def test_run_job_cannot_leave_manifest_provenance_at_not_executed(
    tmp_path: Path,
) -> None:
    path = _contract(tmp_path, "e2e-macos-arm64")
    contract = json.loads(path.read_text(encoding="utf-8"))
    contract["targets"][0]["release"]["artifact_provenance"]["execution"]["wheel"] = {
        "status": "not-executed",
        "issue": 3071,
    }
    path.write_text(json.dumps(contract), encoding="utf-8")
    workflow = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    needs: e2e-macos-arm64-build\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: aarch64-apple-darwin\n",
    )
    failures = MODULE.release_execution_failures(
        path, workflow, _release_workflow(tmp_path)
    )
    assert any("wheel as 'not-executed'" in failure for failure in failures)


def test_ci_replay_alone_cannot_clear_the_shipped_artifact_gate(
    tmp_path: Path,
) -> None:
    contract = _contract(tmp_path, "e2e-macos-arm64")
    workflow = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    needs: e2e-macos-arm64-build\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: aarch64-apple-darwin\n",
    )
    release = tmp_path / "release-without-arm-smoke.yml"
    release.write_text(
        "jobs:\n  publish:\n    runs-on: ubuntu-24.04\n", encoding="utf-8"
    )
    failures = MODULE.release_execution_failures(contract, workflow, release)
    assert any("no real release gate for the shipped archive" in f for f in failures)
    assert any("no real release gate for the shipped wheel" in f for f in failures)


def test_echo_only_release_job_cannot_claim_shipped_artifact_execution(
    tmp_path: Path,
) -> None:
    contract = _contract(tmp_path, "e2e-macos-arm64")
    payload = json.loads(contract.read_text(encoding="utf-8"))
    for record in payload["targets"][0]["release"]["artifact_provenance"][
        "execution"
    ].values():
        record["gate_job"] = "smoke_macos_arm64"
    contract.write_text(json.dumps(payload), encoding="utf-8")
    workflow = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    needs: e2e-macos-arm64-build\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: aarch64-apple-darwin\n",
    )
    release = tmp_path / "echo-release.yml"
    release.write_text(
        "jobs:\n"
        "  smoke_macos_arm64:\n"
        "    runs-on: self-hosted\n"
        "    steps:\n"
        "      - run: echo aarch64-apple-darwin\n"
        "  publish:\n"
        "    needs: [smoke_macos_arm64]\n",
        encoding="utf-8",
    )
    failures = MODULE.release_execution_failures(contract, workflow, release)
    assert any("does not download the shipped archive" in f for f in failures)
    assert any("missing ci/smoke_release_artifacts.py" in f for f in failures)


def test_every_publisher_must_depend_on_the_runtime_gate(tmp_path: Path) -> None:
    contract = _contract(tmp_path, "e2e-macos-arm64")
    payload = json.loads(contract.read_text(encoding="utf-8"))
    for record in payload["targets"][0]["release"]["artifact_provenance"][
        "execution"
    ].values():
        record["gate_job"] = "smoke_macos_arm64"
    contract.write_text(json.dumps(payload), encoding="utf-8")
    workflow = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    needs: e2e-macos-arm64-build\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: aarch64-apple-darwin\n",
    )
    release = _release_workflow(tmp_path)
    text = release.read_text(encoding="utf-8").replace(
        "  publish-pypi:\n    needs: [smoke_macos_arm64]\n",
        "  publish-pypi:\n    needs: [build]\n",
    )
    release.write_text(text, encoding="utf-8")
    failures = MODULE.release_execution_failures(contract, workflow, release)
    assert any("not a publish-pypi dependency" in f for f in failures)


def test_always_publisher_must_require_runtime_gate_success(tmp_path: Path) -> None:
    contract = _contract(tmp_path, "e2e-macos-arm64")
    payload = json.loads(contract.read_text(encoding="utf-8"))
    for record in payload["targets"][0]["release"]["artifact_provenance"][
        "execution"
    ].values():
        record["gate_job"] = "smoke_macos_arm64"
    contract.write_text(json.dumps(payload), encoding="utf-8")
    workflow = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    needs: e2e-macos-arm64-build\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: aarch64-apple-darwin\n",
    )
    release = _release_workflow(tmp_path)
    guarded_publisher = (
        "  publish-npm:\n"
        + "    needs: [smoke_macos_arm64]\n"
        + "    if: needs.smoke_macos_arm64.result == 'success'\n"
    )
    unguarded_publisher = (
        "  publish-npm:\n    needs: [smoke_macos_arm64]\n    if: always()\n"
    )
    text = release.read_text(encoding="utf-8").replace(
        guarded_publisher,
        unguarded_publisher,
    )
    release.write_text(text, encoding="utf-8")
    failures = MODULE.release_execution_failures(contract, workflow, release)
    expected = "publish-npm does not require successful completion"
    assert expected in "\n".join(failures)


def test_arm64_contract_requires_native_release_archive_and_wheel_smoke() -> None:
    payload = json.loads(CONTRACT.read_text(encoding="utf-8"))
    arm64 = next(
        row for row in payload["targets"] if row["triple"] == "aarch64-apple-darwin"
    )
    assert arm64["release"]["execution_gate"] == {"required": True, "issue": 3071}
    assert arm64["release"]["artifact_provenance"] == {
        "build": {"status": "cross-built", "runner": "ubuntu-24.04"},
        "execution": {
            "archive": {
                "status": "required-before-publication",
                "gate_job": "smoke_macos_arm64",
            },
            "wheel": {
                "status": "required-before-publication",
                "gate_job": "smoke_macos_arm64",
            },
        },
    }


def test_checked_in_contract_allows_release_after_required_smokes(capsys) -> None:
    assert MODULE.main(["--verify-execution-contract"]) == 0
    assert "release execution contract complete" in capsys.readouterr().out


def test_publish_requires_execution_contract_gate() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    gate = workflow.split("  release_execution_contract:\n", 1)[1].split(
        "\n  publish:", 1
    )[0]
    publish = workflow.split("\n  publish:\n", 1)[1].split(
        "\n  verify_github_release:", 1
    )[0]
    assert "--verify-execution-contract" in gate
    assert "release_execution_contract" in publish.split("    if:", 1)[0]
    assert "needs.release_execution_contract.result == 'success'" in publish

    for name, following in (
        ("publish-pypi", "smoke-published-dylint"),
        ("publish-npm", "release-completeness"),
    ):
        job = workflow.split(f"\n  {name}:\n", 1)[1].split(f"\n  {following}:", 1)[0]
        assert "release_execution_contract" in job.split("    if:", 1)[0]
        assert "needs.release_execution_contract.result == 'success'" in job

    assert "should_publish_pypi == 'true'" in gate
    assert "should_publish_npm == 'true'" in gate
    # npm-only recovery verifies an already immutable release in its own job;
    # prepare and release artifact smoke jobs intentionally skip on that path.
    assert "inputs.npm_release_ref != ''" not in gate
    npm = workflow.split("\n  publish-npm:\n", 1)[1].split(
        "\n  release-completeness:", 1
    )[0]
    assert "needs.prepare.result == 'skipped'" in npm
    assert "validate_npm_release_recovery.py" in npm
