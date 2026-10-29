"""soldr#2469 4.1: promotion publishes only a verified run's exact artifacts."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
GATE = load_script_module(
    ROOT / ".github" / "scripts" / "release_promotion_gate.py", "release_promotion_gate"
)
SHA = "c" * 40
BUILDS = ["Linux x64 (glibc)", "Windows x64 (Linux cross)"]


def source(**overrides: str):
    fields = {
        "path": ".github/workflows/auto-release.yml",
        "event": "workflow_dispatch",
        "repository": "zackees/soldr",
        "title": f"Release dry run {SHA}",
        "html_url": "https://example/run",
    }
    fields.update(overrides)
    return GATE.SourceRun(**fields)


def jobs(**conclusions: str):
    names = [
        *BUILDS,
        "Smoke Intel macOS release artifacts",
        "Smoke Windows release artifacts (x64)",
    ]
    return [GATE.SourceJob(name, conclusions.get(name, "success")) for name in names]


def check(run, job_list, sha: str = SHA) -> None:
    GATE.validate(
        run, job_list, sha=sha, repository="zackees/soldr", build_names=BUILDS
    )


def test_a_green_dry_run_for_the_candidate_is_promotable() -> None:
    check(source(), jobs())


@pytest.mark.parametrize(
    "run",
    [
        source(path=".github/workflows/ci.yml"),
        source(event="push"),
        source(repository="someone/fork"),
        source(title=f"Release dry run {'d' * 40}"),
    ],
)
def test_a_run_from_elsewhere_is_refused(run) -> None:
    with pytest.raises(GATE.PromotionError):
        check(run, jobs())


def test_a_failed_build_or_smoke_is_refused() -> None:
    with pytest.raises(GATE.PromotionError, match="Linux x64"):
        check(source(), jobs(**{"Linux x64 (glibc)": "failure"}))
    with pytest.raises(GATE.PromotionError, match="Smoke Windows"):
        check(source(), jobs(**{"Smoke Windows release artifacts (x64)": "skipped"}))


def test_a_run_without_smokes_is_refused() -> None:
    builds_only = [GATE.SourceJob(name, "success") for name in BUILDS]
    with pytest.raises(GATE.PromotionError, match="no release-artifact smoke"):
        check(source(), builds_only)


def test_the_workflow_wires_promotion_end_to_end() -> None:
    workflow = (ROOT / ".github" / "workflows" / "auto-release.yml").read_text(
        encoding="utf-8"
    )
    assert (
        "run-name: \"Release ${{ inputs.dry_run && 'dry run ' || '' }}${{ inputs.candidate_sha }}\""
        in workflow
    )
    assert "promote_run_id:" in workflow
    assert "&& inputs.promote_run_id == ''" in workflow  # build skipped
    assert workflow.count("run-id: ${{ inputs.promote_run_id || github.run_id }}") == 9
    assert "release_promotion_gate.py" in workflow


def test_a_skipped_post_publish_smoke_does_not_block_promotion() -> None:
    """Dry runs skip "Smoke published Dylint"; it is not an artifact smoke."""
    job_list = [
        *jobs(),
        GATE.SourceJob("Smoke published Dylint on Windows x64", "skipped"),
    ]
    check(source(), job_list)
