"""soldr#2469 step 2.2: the tag + draft-release mutations, out of YAML.

These two steps create the release tag and the draft GitHub Release. They were
inline bash, so their idempotency and their soldr#1252 recovery guidance could
only be exercised by attempting a real release. A fake runner makes both
testable.
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "release_publish.py"

REPO = "zackees/soldr"
TAG = "v1.2.3"
SHA = "0123456789abcdef"
RUN_ID = "42"


def load_module():
    spec = importlib.util.spec_from_file_location("release_publish", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["release_publish"] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


@dataclass(frozen=True)
class Reply:
    """One scripted gh result."""

    code: int
    stderr: str = ""


@dataclass
class FakeGh:
    """A runner replaying scripted gh results and recording every call."""

    replies: list[Reply]
    calls: list[list[str]] = field(default_factory=list)

    def __call__(self, args: Sequence[str]) -> subprocess.CompletedProcess:
        self.calls.append(list(args))
        reply = self.replies.pop(0) if self.replies else Reply(0)
        return subprocess.CompletedProcess(list(args), reply.code, "", reply.stderr)


def fake_runner(codes: list[int]) -> FakeGh:
    """A runner replaying scripted exit codes with empty output."""
    return FakeGh([Reply(code) for code in codes])


def make_dist(tmp_path: Path, names=("a.tar.zst", "b.whl")) -> Path:
    dist = tmp_path / "dist"
    dist.mkdir()
    for name in names:
        (dist / name).write_text("x", encoding="utf-8")
    return dist


# --- asset globbing -------------------------------------------------------


def test_assets_are_sorted_for_a_deterministic_command_line(tmp_path: Path) -> None:
    dist = make_dist(tmp_path, ("z.whl", "a.tar.zst", "m.txt"))
    assets = MODULE.release_assets(dist)
    assert [Path(p).name for p in assets] == ["a.tar.zst", "m.txt", "z.whl"]


def test_an_empty_dist_is_a_named_error_not_a_literal_glob(tmp_path: Path) -> None:
    """The inline `dist/*` degraded to a literal argument and confused gh."""
    dist = tmp_path / "dist"
    dist.mkdir()
    try:
        MODULE.release_assets(dist)
    except FileNotFoundError as error:
        assert "no release assets found" in str(error)
    else:  # pragma: no cover - the assertion above is the contract
        raise AssertionError("an empty dist must raise")


def test_a_missing_dist_is_a_named_error(tmp_path: Path) -> None:
    try:
        MODULE.release_assets(tmp_path / "nope")
    except FileNotFoundError as error:
        assert "does not exist" in str(error)
    else:  # pragma: no cover
        raise AssertionError("a missing dist must raise")


def test_directories_inside_dist_are_not_uploaded(tmp_path: Path) -> None:
    dist = make_dist(tmp_path)
    (dist / "subdir").mkdir()
    assert [Path(p).name for p in MODULE.release_assets(dist)] == ["a.tar.zst", "b.whl"]


# --- ensure-tag -----------------------------------------------------------


def test_an_existing_tag_is_a_skip_not_a_failure() -> None:
    run = fake_runner([0])  # `gh api refs/tags/<tag>` succeeds
    calls = run.calls
    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 0
    assert len(calls) == 1, "an existing tag must not attempt a create"


def test_a_missing_tag_is_created_at_the_requested_sha() -> None:
    run = fake_runner([1, 0])  # lookup fails, create succeeds
    calls = run.calls
    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 0
    create = calls[1]
    assert f"ref=refs/tags/{TAG}" in create
    assert f"sha={SHA}" in create
    assert "POST" in create


def test_a_tag_create_failure_exits_one(monkeypatch, tmp_path: Path) -> None:
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    run = fake_runner([1, 1])
    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 1
    text = summary.read_text(encoding="utf-8")
    assert "Manual release recovery needed (tag)" in text
    # The recovery block must be runnable as-is (soldr#1252).
    assert f"gh api repos/{REPO}/git/refs -X POST -f ref=refs/tags/{TAG}" in text
    assert f"sha={SHA}" in text


# --- create-draft ---------------------------------------------------------


def test_a_new_release_is_created_as_a_draft(monkeypatch, tmp_path: Path) -> None:
    output = tmp_path / "out.txt"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    dist = make_dist(tmp_path)
    run = fake_runner([1, 0])  # view fails (absent), create succeeds
    calls = run.calls

    assert (
        MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
        == 0
    )
    create = calls[1]
    # Draft is the whole point: soldr#2469 step 4.2 verifies the asset set
    # before publication, which is impossible once a release is immutable.
    assert "--draft" in create
    assert "--generate-notes" in create
    assert create[create.index("--target") + 1] == SHA
    assert output.read_text(encoding="utf-8").strip() == "created=true"


def test_an_existing_release_re_uploads_with_clobber(
    monkeypatch, tmp_path: Path
) -> None:
    """Idempotency: a rerun after a partial success must not fail."""
    output = tmp_path / "out.txt"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    dist = make_dist(tmp_path)
    run = fake_runner([0, 0])  # view succeeds (present), upload succeeds
    calls = run.calls

    assert (
        MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
        == 0
    )
    upload = calls[1]
    assert upload[:3] == ["gh", "release", "upload"]
    assert "--clobber" in upload
    assert output.read_text(encoding="utf-8").strip() == "created=true"


def test_a_failed_re_upload_does_not_report_success(
    monkeypatch, tmp_path: Path
) -> None:
    """A partial asset set that reports success is the 0.9.0 wound."""
    output = tmp_path / "out.txt"
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    dist = make_dist(tmp_path)
    run = fake_runner([0, 1])  # view succeeds, upload fails

    assert (
        MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
        == 1
    )
    assert output.read_text(encoding="utf-8").strip() == "created=false"
    assert "Manual release recovery needed" in summary.read_text(encoding="utf-8")


def test_a_failed_create_sets_created_false_and_emits_recovery(
    monkeypatch, tmp_path: Path
) -> None:
    output = tmp_path / "out.txt"
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    dist = make_dist(tmp_path)
    run = fake_runner([1, 1])  # view fails, create fails

    assert (
        MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
        == 1
    )
    assert output.read_text(encoding="utf-8").strip() == "created=false"
    text = summary.read_text(encoding="utf-8")
    assert f"gh run download {RUN_ID} --repo {REPO}" in text
    assert "--pattern 'release-soldr-*'" in text


def test_every_asset_is_passed_to_gh(monkeypatch, tmp_path: Path) -> None:
    """The shell used to expand `dist/*`; the script must pass them all."""
    monkeypatch.setenv("GITHUB_OUTPUT", str(tmp_path / "out.txt"))
    dist = make_dist(tmp_path, ("one.whl", "two.whl", "three.tar.zst"))
    run = fake_runner([1, 0])
    calls = run.calls
    MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
    create = calls[1]
    for name in ("one.whl", "two.whl", "three.tar.zst"):
        assert any(arg.endswith(name) for arg in create), name


# --- workflow wiring ------------------------------------------------------


def test_the_workflow_invokes_the_script_for_both_mutations() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "auto-release.yml").read_text(
        encoding="utf-8"
    )
    assert "release_publish.py ensure-tag" in workflow
    assert "release_publish.py create-draft" in workflow
    # The inline implementations must be gone, not merely bypassed. Match on
    # strings unique to those blocks rather than on the command names: a
    # comment explaining why `gh release create --target <sha>` creates the
    # tag implicitly is worth keeping, and should not fail this test.
    assert "already exists; uploading/replacing assets" not in workflow
    assert 'echo "created=true" >> "$GITHUB_OUTPUT"' not in workflow
    assert "Manual release recovery needed" not in workflow


def failing_gh(marker: str, fail_from: int) -> FakeGh:
    """Fake gh: calls before `fail_from` exit 1 quietly (the "exists?" probe
    misses); every later call exits 1 writing `marker` to stderr."""
    return FakeGh(
        [Reply(1)] * (fail_from - 1) + [Reply(1, f"HTTP 403: {marker}\n")] * 4
    )


def test_ensure_tag_failure_prints_gh_stderr(tmp_path, monkeypatch, capsys):
    """soldr#3385: the ::error:: line and summary carry gh's own stderr."""
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    run = failing_gh("MARKER_TAG_3385", fail_from=2)
    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 1
    out = capsys.readouterr()
    error_line = next(ln for ln in out.out.splitlines() if ln.startswith("::error::"))
    assert "MARKER_TAG_3385" in error_line
    assert "MARKER_TAG_3385" in summary.read_text(encoding="utf-8")
    assert "MARKER_TAG_3385" in out.err


def test_create_draft_release_failure_prints_gh_stderr(tmp_path, monkeypatch, capsys):
    """soldr#3385: release create failure surfaces gh's stderr."""
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    monkeypatch.delenv("GITHUB_OUTPUT", raising=False)
    run = failing_gh("MARKER_RELEASE_3385", fail_from=2)
    rc = MODULE.create_draft_release(
        REPO, TAG, SHA, run_id=RUN_ID, dist=make_dist(tmp_path), run=run
    )
    assert rc == 1
    out = capsys.readouterr()
    error_line = next(ln for ln in out.out.splitlines() if ln.startswith("::error::"))
    assert "MARKER_RELEASE_3385" in error_line
    assert "MARKER_RELEASE_3385" in summary.read_text(encoding="utf-8")


def test_gh_stderr_forwarded_on_success(capsys):
    """soldr#3389 contract: gh stderr is forwarded even when gh succeeds."""

    def run(args: Sequence[str]) -> subprocess.CompletedProcess:
        return subprocess.CompletedProcess(list(args), 0, "", "MARKER_OK_3389\n")

    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 0
    assert "gh api repos/zackees/soldr/git/refs/tags/v1.2.3: MARKER_OK_3389" in (
        capsys.readouterr().err
    )


# --- soldr#3343: transient retry -----------------------------------------


def test_a_transient_failure_is_retried_until_success() -> None:
    run = FakeGh([Reply(1, "HTTP 502: Bad Gateway"), Reply(1, "HTTP 503"), Reply(0)])
    calls = run.calls
    sleeps: list[float] = []
    result = MODULE.retrying(run, sleep=sleeps.append)(["gh", "release", "upload"])
    assert result.returncode == 0
    assert len(calls) == 3 and len(sleeps) == 2
    assert all(0 <= s <= 60 for s in sleeps)


def test_transient_retries_stop_after_ten_attempts() -> None:
    run = FakeGh([Reply(1, "connection reset by peer")] * 20)
    calls = run.calls
    result = MODULE.retrying(run, sleep=lambda _s: None)(["gh", "api", "x"])
    assert result.returncode == 1
    assert len(calls) == MODULE.RETRY_ATTEMPTS == 10


def test_permanent_failures_are_not_retried() -> None:
    for stderr in ("HTTP 403: Resource not accessible", "HTTP 422: immutable", ""):
        run = FakeGh([Reply(1, stderr), Reply(0)])
        calls = run.calls
        result = MODULE.retrying(run, sleep=lambda _s: None)(["gh", "api", "x"])
        assert result.returncode == 1 and len(calls) == 1, stderr


def test_ensure_tag_retries_a_transient_create(monkeypatch) -> None:
    monkeypatch.setattr(MODULE.time, "sleep", lambda _s: None)
    run = FakeGh([Reply(1, "HTTP 404"), Reply(1, "HTTP 500"), Reply(0)])
    calls = run.calls
    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 0
    assert len(calls) == 3


# --- soldr#3343: release-level info.json ----------------------------------


def test_release_info_records_every_file_with_size_and_hash(tmp_path: Path) -> None:
    dist = make_dist(tmp_path, names=("soldr-v1.2.3-x.tar.zst", "soldr-1.2.3.whl"))
    contract = tmp_path / "contract.json"
    contract.write_text("{}", encoding="utf-8")
    info = MODULE.release_info(
        dist,
        version=TAG,
        sha=SHA,
        contract=contract,
        triples=["x86_64-unknown-linux-gnu"],
        run_id=RUN_ID,
    )
    assert info.tag == TAG and info.commit_sha == SHA
    assert info.targets == ["x86_64-unknown-linux-gnu"]
    assert [f.name for f in info.files] == ["soldr-1.2.3.whl", "soldr-v1.2.3-x.tar.zst"]
    assert all(f.bytes == 1 and len(f.sha256) == 64 for f in info.files)
    assert MODULE.info_name(TAG) == "soldr-v1.2.3-info.json"


def test_info_json_is_an_expected_release_asset_and_written_by_the_workflow() -> None:
    assert (
        "soldr-v1.2.3-info.json"
        in MODULE.release_completeness.expected_github_assets(TAG, [])
    )
    workflow = (REPO_ROOT / ".github" / "workflows" / "auto-release.yml").read_text(
        encoding="utf-8"
    )
    write = workflow.index("release_publish.py write-info")
    assert workflow.index("SHA256SUMS.txt") < write < workflow.index("ensure-tag")


def complete_draft(tag: str, _repo: str) -> dict:
    gate = MODULE.release_completeness
    names = gate.expected_github_assets(tag, gate.included_triples())
    return {"isDraft": True, "assets": [{"name": n, "size": 1} for n in names]}


def test_publish_makes_a_complete_draft_public() -> None:
    run = fake_runner([0])
    assert MODULE.publish_release(REPO, TAG, run, complete_draft) == 0
    assert run.calls == [
        ["gh", "release", "edit", TAG, "--repo", REPO, "--draft=false"]
    ]


def test_a_failed_publish_exits_one() -> None:
    assert MODULE.publish_release(REPO, TAG, fake_runner([1]), complete_draft) == 1


def test_an_incomplete_draft_is_never_published() -> None:
    run = fake_runner([0])

    def empty(_tag: str, _repo: str) -> dict:
        return {"isDraft": True, "assets": []}

    assert MODULE.publish_release(REPO, TAG, run, empty) == 1
    assert not run.calls
