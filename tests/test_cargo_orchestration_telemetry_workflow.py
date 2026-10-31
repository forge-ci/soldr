"""Contract guards for the manual soldr#2878 evidence workflow."""

from __future__ import annotations

import fnmatch
from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "cargo-orchestration-telemetry.yml"
DRIVER = REPO_ROOT / "ci" / "run_cargo_orchestration_telemetry_matrix.py"
TELEMETRY = REPO_ROOT / "ci" / "cargo_orchestration_telemetry.py"
matrix = load_script_module(DRIVER, "cargo_orchestration_telemetry_matrix")


def test_driver_fixes_the_requested_one_two_eight_matrix(tmp_path: Path) -> None:
    source_soldr = tmp_path / "soldr"
    source_soldr.write_text("source binary", encoding="utf-8")
    evidence = tmp_path / "evidence"

    arguments = matrix.matrix_arguments(source_soldr, evidence)

    assert matrix.BASELINE_JOBS == "1,2,auto"
    assert matrix.RAISED_JOBS == 8
    assert arguments[:5] == [
        "--jobs",
        "1,2,auto",
        "--raised-jobs",
        "8",
        "--allow-raised-count",
    ]
    assert arguments[arguments.index("--") + 1 :] == [
        str(source_soldr),
        "cargo",
        "check",
        "-p",
        "soldr-cli",
    ]
    assert str(evidence / "telemetry.json") in arguments
    assert str(evidence / "cases") in arguments


def test_upload_glob_also_covers_the_jobs_auto_case_directory() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    upload = workflow[
        workflow.index("- name: Upload Cargo orchestration telemetry evidence") :
    ]
    assert "cases/jobs-*/command.log" in upload
    assert fnmatch.fnmatch("cases/jobs-auto/command.log", "cases/jobs-*/command.log")


def test_workflow_is_dispatch_only_and_is_not_a_perf_matrix_hook() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")

    assert "workflow_dispatch:" in workflow
    assert "push:" not in workflow
    assert "pull_request:" not in workflow
    assert "schedule:" not in workflow
    assert "perf-matrix" not in workflow
    assert "perf/" not in workflow
    assert "timeout-minutes: 120" in workflow


def test_prepared_toolchain_and_source_binary_precede_telemetry() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")

    python = workflow.index("- name: Set up telemetry Python")
    prepare = workflow.index("- name: Prepare pinned Rust and Cargo toolchain")
    source = workflow.index("- name: Build the source Soldr telemetry driver")
    cleanup = workflow.index(
        "- name: Stop setup-soldr builder cache before source telemetry"
    )
    retire = workflow.index(
        "- name: Retire the bootstrap broker before source telemetry"
    )
    telemetry = workflow.index("- name: Run fixed cold Cargo orchestration matrix")
    assert python < prepare < source < cleanup < retire < telemetry
    assert 'python-version: "3.13"' in workflow
    assert "toolchain: 1.98.1" in workflow
    assert "soldr cargo build -p soldr-cli --bin soldr" in workflow
    assert (
        "zackees/setup-soldr/cleanup@9337570234850b50322e696037127fa3cf8ac703"
        in workflow
    )
    assert "run: soldr broker remove" in workflow
    assert "CARGO_TARGET_DIR: ${{ runner.temp }}/soldr-2878-source-target" in workflow
    assert (
        "SOLDR_RUSTC_WRAPPER: ${{ runner.temp }}/soldr-2878-source-target/debug/soldr"
        in workflow
    )
    assert (
        '--source-soldr "${{ runner.temp }}/soldr-2878-source-target/debug/soldr"'
        in workflow
    )


def test_workflow_retains_json_and_per_case_command_logs_after_failure() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    driver = DRIVER.read_text(encoding="utf-8")
    telemetry = TELEMETRY.read_text(encoding="utf-8")

    assert "Upload Cargo orchestration telemetry evidence" in workflow
    assert "if: ${{ always() }}" in workflow
    assert "retention-days: 14" in workflow
    upload = workflow[
        workflow.index("- name: Upload Cargo orchestration telemetry evidence") :
    ]
    assert "${{ runner.temp }}/soldr-2878-telemetry/telemetry.json" in upload
    assert "${{ runner.temp }}/soldr-2878-telemetry/cases/jobs-*/command.log" in upload
    assert "soldr-2878-telemetry/cases/jobs-*/target" not in upload
    assert "soldr-2878-telemetry/cases/jobs-*/soldr-cache" not in upload
    assert "path: ${{ runner.temp }}/soldr-2878-telemetry\n" not in upload
    assert '"telemetry.json"' in driver
    assert '"cases"' in driver
    assert '"command.log"' in telemetry
