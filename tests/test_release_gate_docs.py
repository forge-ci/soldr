"""soldr#2469 step 1.2: the release docs must describe the live gates.

The 0.9.0 incident was survivable partly because nobody could tell from the
docs what was actually enforced. RELEASE.md, SECURITY.md and
docs/RELEASE_VERIFICATION.md described branch protection and release-time
re-validation that do not exist. Those three were corrected; this pins the
correction so it cannot rot back, and fails when a doc claims a control the
workflow does not implement.

The checks are deliberately narrow. They assert the *shape* of a claim against
the workflow, not prose style, so a rewrite that stays truthful still passes.
"""

from __future__ import annotations

import re
from pathlib import Path

import yaml

REPO_ROOT = Path(__file__).parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"
VERIFICATION = REPO_ROOT / "docs" / "RELEASE_VERIFICATION.md"
RELEASE_MD = REPO_ROOT / "RELEASE.md"
SECURITY_MD = REPO_ROOT / "SECURITY.md"

# Jobs that actually publish something users can consume.
PUBLISHING_JOBS = ("publish", "publish-pypi", "publish-npm")


def flowed(path: Path) -> str:
    """Doc text with runs of whitespace collapsed.

    These claims are prose and get re-wrapped by editors, so a phrase like
    "not currently branch-protected" is routinely split across a line
    break. Matching the raw text would fail on a purely cosmetic rewrap.
    """
    return re.sub(r"\s+", " ", path.read_text(encoding="utf-8"))


def workflow_jobs() -> dict:
    return yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))["jobs"]


def jobs_declaring_an_environment() -> set[str]:
    return {name for name, job in workflow_jobs().items() if job.get("environment")}


def test_only_npm_publication_declares_an_environment() -> None:
    """The fact the verification doc now states.

    When soldr#2469 Phase 4 puts the other publications behind an environment,
    this fails -- and the doc claiming a gate must be updated in the same
    change. That coupling is the point: the previous wording ("final
    publication happens in the `release` environment") was true of exactly one
    of three surfaces, which read as though all of them were gated.
    """
    assert jobs_declaring_an_environment() == {"publish-npm"}


def test_the_verification_doc_does_not_imply_a_blanket_environment_gate() -> None:
    text = flowed(VERIFICATION)
    assert "final publication happens in the `release` environment" not in text
    # It must name which surface is involved rather than generalizing.
    assert "only the **npm** publication runs in the `release` environment" in text


def test_docs_do_not_claim_branch_protection() -> None:
    """`main` has no protection and no rulesets (verified 2026-08-11/21).

    Each doc must say so where it describes the merge step, because the
    reviewed-merge *is* the authorization step in the current model -- a reader
    who believes it is mechanically gated has the threat model wrong.
    """
    for path in (RELEASE_MD, SECURITY_MD, VERIFICATION):
        text = flowed(path)
        assert "not currently branch-protected" in text or "not protected" in text, (
            f"{path.name} must state that `main` is not branch-protected "
            "while that remains true (soldr#2469)"
        )
        assert "soldr#2469" in text, f"{path.name} should point at the target state"


def test_docs_do_not_claim_release_time_revalidation() -> None:
    """No lint/test/e2e job exists in the release workflow."""
    jobs = set(workflow_jobs())
    for suspect in ("lint", "test", "e2e", "clippy", "fmt"):
        assert suspect not in jobs, (
            f"auto-release.yml gained a {suspect!r} job; the docs saying it does "
            "NOT re-validate before publishing are now wrong (soldr#2469 Phase 1)"
        )
    for path in (SECURITY_MD, VERIFICATION):
        text = flowed(path)
        assert "does **not** currently re-run lint" in text, (
            f"{path.name} must state that release-time re-validation does not "
            "happen while that remains true"
        )


def test_every_publishing_job_still_exists() -> None:
    """Guards the two tests above from silently passing on a renamed job."""
    jobs = set(workflow_jobs())
    for name in PUBLISHING_JOBS:
        assert name in jobs, f"publishing job {name!r} disappeared from the workflow"


def test_release_md_owner_setup_does_not_present_protection_as_configured() -> None:
    """RELEASE.md must not contradict itself about branch protection.

    Its intro already says `main` is not branch-protected, but the "Owner
    Setup" section listed protection among "the one-time controls that make
    the unattended flow work" and then stated the workflow "assumes that any
    version bump reaching `main` has already passed the required validation
    gate". Nothing enforces that gate, so the sentence described a guarantee
    the repository does not provide -- in the one section an owner reads to
    find out what is configured.
    """
    text = flowed(RELEASE_MD)
    assert "has already passed the required validation gate" not in text
    assert "### 1. Protect `main` — not configured today" in text


def test_future_agent_instructions_expect_the_unprotected_state() -> None:
    """The audit checklist must not presuppose the control it is auditing.

    "Confirm `main` still requires pull requests and the expected validation
    checks" sends a reader looking for something that is not there, and the
    section ends by telling them to stop and report drift -- turning the
    known, tracked absence into a false alarm every time.
    """
    text = flowed(RELEASE_MD)
    assert "Confirm `main` still requires pull requests" not in text
    assert "It is **not** today" in text
