"""Every release job that runs a repo script must check the repo out.

soldr#2763: the `publish` job made inline `gh` API calls and needed no
checkout. Extracting those calls into `.github/scripts/release_publish.py`
(soldr#2469) introduced a filesystem dependency without adding a checkout
step, and the v0.9.3 release died there with

    python3: can't open file '.../.github/scripts/release_publish.py'

after every build lane had already succeeded. `publish-pypi` carried the
same latent defect. This asserts the property for the whole workflow rather
than for the two jobs that happened to be caught.
"""

from __future__ import annotations

import re
from pathlib import Path

import yaml

REPO_ROOT = Path(__file__).parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"

SCRIPT_INVOCATION = re.compile(
    r"(?:python3?\s+|uv\s+run\b[^\n]*?\s)\S*(?:\.github/scripts|ci)/\S+\.py"
)


def _runs_a_repo_script(job: dict) -> bool:
    return any(
        SCRIPT_INVOCATION.search(step["run"])
        for step in job.get("steps", [])
        if isinstance(step.get("run"), str)
    )


def _checks_out(job: dict) -> bool:
    return any(
        isinstance(step.get("uses"), str)
        and step["uses"].startswith("actions/checkout@")
        for step in job.get("steps", [])
    )


def test_every_job_running_a_repo_script_checks_the_repo_out() -> None:
    workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))

    offenders = sorted(
        name
        for name, job in workflow["jobs"].items()
        if _runs_a_repo_script(job) and not _checks_out(job)
    )

    assert not offenders, (
        "these release jobs invoke a script from the repo but never check it "
        f"out, so the file will not exist at runtime: {offenders}"
    )


def test_the_guard_can_actually_see_a_missing_checkout() -> None:
    """A passing assertion is worthless if the detector matches nothing."""
    workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))

    scripted = [
        name for name, job in workflow["jobs"].items() if _runs_a_repo_script(job)
    ]
    assert "publish" in scripted, "publish must still be recognised as script-running"

    stripped = dict(workflow["jobs"]["publish"])
    stripped["steps"] = [
        step
        for step in stripped["steps"]
        if not str(step.get("uses", "")).startswith("actions/checkout@")
    ]
    assert _runs_a_repo_script(stripped)
    assert not _checks_out(stripped)
