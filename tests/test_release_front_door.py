"""soldr#3343: ci/release.py starts and resumes one exact-SHA release."""

from __future__ import annotations

import json
import subprocess
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
REL = load_script_module(ROOT / "ci" / "release.py", "soldr_release_front_door")
SHA = "a" * 40
OTHER = "b" * 40
CARGO = '[workspace.package]\nversion = "1.2.3"\n'


@dataclass(frozen=True)
class GhRun:
    """One `gh run list --json` row (field names are gh's)."""

    run_id: int
    title: str
    status: str
    conclusion: str

    def to_json(self) -> dict:
        return {
            "databaseId": self.run_id,
            "displayTitle": self.title,
            "status": self.status,
            "conclusion": self.conclusion,
        }


@dataclass
class FakeGit:
    """Answers git/gh calls from a scripted repository state."""

    ancestor: bool = True
    tag_sha: str | None = None
    ci_runs: list[GhRun] = field(default_factory=list)
    open_attempt: int | None = None
    calls: list[list[str]] = field(default_factory=list)

    def __call__(self, argv: Sequence[str]) -> subprocess.CompletedProcess:
        args = list(argv)
        self.calls.append(args)
        out, code = "", 0
        if args[:3] == ["git", "merge-base", "--is-ancestor"]:
            code = 0 if self.ancestor else 1
        elif args[:2] == ["git", "show"]:
            out = CARGO
        elif args[:2] == ["git", "ls-remote"]:
            out = f"{self.tag_sha}\trefs/tags/v1.2.3\n" if self.tag_sha else ""
        elif args[:3] == ["gh", "run", "list"]:
            out = json.dumps([row.to_json() for row in self.ci_runs])
        elif args[:3] == ["gh", "issue", "list"]:
            title = f"release attempt v1.2.3 @ {SHA[:12]}"
            rows = [{"number": self.open_attempt, "title": title}]
            out = json.dumps(rows if self.open_attempt else [])
        elif args[:3] == ["gh", "issue", "create"]:
            out = "https://github.com/zackees/soldr/issues/4242\n"
        return subprocess.CompletedProcess(args, code, out, "")

    def dispatched(self) -> list[list[str]]:
        return [c for c in self.calls if c[:3] == ["gh", "workflow", "run"]]


def ci_run(run_id: int, status: str, conclusion: str = "", sha: str = SHA) -> GhRun:
    return GhRun(run_id, f"CI full {sha}", status, conclusion)


def test_a_fresh_candidate_dispatches_full_ci() -> None:
    git = FakeGit()
    assert REL.main(["start", "--sha", SHA], run=git) == 0
    (call,) = git.dispatched()
    assert call[3] == "ci.yml" and f"candidate_sha={SHA}" in call


def test_a_running_ci_waits_without_dispatching() -> None:
    git = FakeGit(ci_runs=[ci_run(7, "in_progress")])
    assert REL.main(["start", "--sha", SHA], run=git) == 0
    assert not git.dispatched()


def test_a_green_ci_dispatches_the_release_with_its_run_id() -> None:
    git = FakeGit(
        ci_runs=[ci_run(9, "completed", "failure"), ci_run(8, "completed", "success")]
    )
    assert REL.main(["start", "--sha", SHA], run=git) == 0
    (call,) = git.dispatched()
    assert call[3] == "auto-release.yml"
    assert f"candidate_sha={SHA}" in call and "full_ci_run_id=8" in call


def test_runs_for_another_sha_are_ignored() -> None:
    git = FakeGit(ci_runs=[ci_run(8, "completed", "success", sha=OTHER)])
    REL.main(["start", "--sha", SHA], run=git)
    (call,) = git.dispatched()
    assert call[3] == "ci.yml"


def test_resume_after_the_tag_exists_at_the_same_sha() -> None:
    git = FakeGit(tag_sha=SHA, ci_runs=[ci_run(8, "completed", "success")])
    assert REL.main(["start", "--sha", SHA], run=git) == 0
    assert git.dispatched()[0][3] == "auto-release.yml"


@pytest.mark.parametrize(
    "git",
    [
        FakeGit(tag_sha=OTHER),
        FakeGit(ancestor=False),
    ],
)
def test_refusals_dispatch_nothing(git) -> None:
    assert REL.main(["start", "--sha", SHA], run=git) == 1
    assert not git.dispatched()


def test_a_short_sha_is_refused() -> None:
    git = FakeGit()
    assert REL.main(["start", "--sha", "abc123"], run=git) == 1
    assert not git.calls


def test_status_reports_tag_and_runs(capsys) -> None:
    git = FakeGit(tag_sha=SHA, ci_runs=[ci_run(8, "completed", "success")])
    assert REL.main(["status", "--sha", SHA], run=git) == 0
    out = capsys.readouterr().out
    assert "v1.2.3" in out and "full CI run 8: completed success" in out


def test_start_opens_one_attempt_issue_with_the_frozen_directive() -> None:
    git = FakeGit()
    assert REL.main(["start", "--sha", SHA], run=git) == 0
    (create,) = [c for c in git.calls if c[:3] == ["gh", "issue", "create"]]
    body = create[create.index("--body") + 1]
    assert f"candidate_sha: {SHA}" in body and "tag: v1.2.3" in body
    assert "x86_64-unknown-linux-gnu" in body
    (comment,) = [c for c in git.calls if c[:3] == ["gh", "issue", "comment"]]
    assert comment[3] == "4242" and "dispatched full CI" in comment[-1]


def test_resume_appends_to_the_existing_attempt() -> None:
    git = FakeGit(open_attempt=77, ci_runs=[ci_run(8, "completed", "success")])
    assert REL.main(["start", "--sha", SHA], run=git) == 0
    assert not [c for c in git.calls if c[:3] == ["gh", "issue", "create"]]
    (comment,) = [c for c in git.calls if c[:3] == ["gh", "issue", "comment"]]
    assert comment[3] == "77" and "auto-release.yml" in comment[-1]


def test_a_refusal_opens_no_attempt() -> None:
    git = FakeGit(tag_sha=OTHER)
    assert REL.main(["start", "--sha", SHA], run=git) == 1
    assert not [c for c in git.calls if c[:2] == ["gh", "issue"]]
