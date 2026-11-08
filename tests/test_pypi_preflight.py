"""soldr#3343: the PyPI size/quota preflight fails before any upload."""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
PRE = load_script_module(
    ROOT / ".github" / "scripts" / "pypi_preflight.py", "pypi_preflight"
)
MB = 1000 * 1000


def wheel(name: str, size: int):
    return PRE.Wheel(name, size)


def test_an_oversized_wheel_fails() -> None:
    verdict = PRE.evaluate(
        [wheel("a.whl", 101 * MB)], [], file_limit=100 * MB, project_limit=10_000 * MB
    )
    assert any("per-file limit" in failure for failure in verdict.failures)


def test_projected_quota_exhaustion_fails() -> None:
    published = [PRE.PublishedFile("old.whl", 9_990 * MB)]
    verdict = PRE.evaluate(
        [wheel("new.whl", 20 * MB)],
        published,
        file_limit=100 * MB,
        project_limit=10_000 * MB,
    )
    assert any("project limit" in failure for failure in verdict.failures)


def test_files_already_on_pypi_are_not_counted_twice() -> None:
    published = [PRE.PublishedFile("same.whl", 9_990 * MB)]
    verdict = PRE.evaluate(
        [wheel("same.whl", 20 * MB)],
        published,
        file_limit=100 * MB,
        project_limit=10_000 * MB,
    )
    assert not verdict.failures and verdict.new_bytes == 0


def test_near_the_limit_warns_without_failing() -> None:
    published = [PRE.PublishedFile("old.whl", 9_100 * MB)]
    verdict = PRE.evaluate(
        [wheel("new.whl", 10 * MB)],
        published,
        file_limit=100 * MB,
        project_limit=10_000 * MB,
    )
    assert not verdict.failures and verdict.warnings


def test_an_empty_dist_fails() -> None:
    verdict = PRE.evaluate([], [], file_limit=100 * MB, project_limit=10_000 * MB)
    assert verdict.failures


def test_published_files_reads_every_release() -> None:
    document = {
        "releases": {
            "0.1": [{"filename": "a.whl", "size": 3}],
            "0.2": [{"filename": "b.whl", "size": 4}],
        }
    }
    assert [f.size for f in PRE.published_files(document)] == [3, 4]


def test_the_publish_job_preflights_before_uploading() -> None:
    workflow = (ROOT / ".github" / "workflows" / "auto-release.yml").read_text(
        encoding="utf-8"
    )
    job = workflow[workflow.index("  publish-pypi:") :]
    assert job.index("pypi_preflight.py") < job.index("gh-action-pypi-publish")
