"""soldr#2469 step 2.2: the tag + draft-release mutations, out of YAML.

These two steps create the release tag and the draft GitHub Release. They were
inline bash, so their idempotency and their soldr#1252 recovery guidance could
only be exercised by attempting a real release. A fake runner makes both
testable.
"""

from __future__ import annotations

import importlib.util
import hashlib
import json

import pytest
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "release_publish.py"

REPO = "zackees/soldr"
TAG = "v1.2.3"
SHA = "0123456789abcdef" * 2 + "01234567"
RUN_ID = "42"


def load_module():
    spec = importlib.util.spec_from_file_location("release_publish", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["release_publish"] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


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


class FakeGitHub:
    """REST-shaped offline API and mutations; unexpected calls fail the fixture."""

    def __init__(self, *, tag_sha=SHA, assets=None, release=True):
        self.tag_sha = tag_sha
        self.release = release
        self.draft = True
        self.assets = list(assets or [])
        self.calls = []
        self.annotated = {}
        self.lookup_error = None
        self.mutation_error = None
        self.after_mutation_sha = None
        self.success_stderr = ""

    def run(self, args):
        args = list(args)
        self.calls.append(args)
        body = None
        code = 0
        stderr = self.success_stderr
        if args[:2] == ["gh", "api"]:
            endpoint = args[2]
            if args[3:5] == ["-X", "POST"]:
                if self.mutation_error:
                    code, stderr = 1, self.mutation_error
                else:
                    self.tag_sha = self.after_mutation_sha or args[-1].removeprefix(
                        "sha="
                    )
                    body = self.ref()
            elif endpoint == f"repos/{REPO}/git/ref/tags/{TAG}":
                if self.lookup_error:
                    code, stderr = 1, self.lookup_error
                elif self.tag_sha is None:
                    code, stderr = 1, "gh: Not Found (HTTP 404)"
                else:
                    body = self.ref()
            elif endpoint.startswith(f"repos/{REPO}/git/tags/"):
                body = self.annotated[endpoint.rsplit("/", 1)[1]]
            elif endpoint == f"repos/{REPO}/releases/tags/{TAG}":
                if self.release:
                    body = {
                        "id": 17,
                        "tag_name": TAG,
                        "target_commitish": "main",
                        "draft": self.draft,
                    }
                else:
                    code, stderr = 1, "gh: Not Found (HTTP 404)"
            elif endpoint == f"repos/{REPO}/releases/17/assets?per_page=100":
                assert args[3:] == ["--paginate", "--slurp"]
                body = [
                    self.assets[i : i + 100] for i in range(0, len(self.assets), 100)
                ] or [[]]
            else:
                raise AssertionError(args)
        elif args[:3] in (["gh", "release", "create"], ["gh", "release", "upload"]):
            assert "--clobber" not in args
            if self.mutation_error:
                code, stderr = 1, self.mutation_error
            else:
                if args[2] == "create":
                    assert "--verify-tag" in args and "--draft" in args
                    self.release = True
                for path in args[4 : args.index("--repo")]:
                    self.assets.append(asset(Path(path), len(self.assets) + 1))
                if self.after_mutation_sha:
                    self.tag_sha = self.after_mutation_sha
                body = "https://github.com/zackees/soldr/releases/tag/v1.2.3"
        else:
            raise AssertionError(args)
        return subprocess.CompletedProcess(args, code, json.dumps(body), stderr)

    def ref(self):
        return {
            "ref": f"refs/tags/{TAG}",
            "object": {
                "type": "tag" if self.tag_sha in self.annotated else "commit",
                "sha": self.tag_sha,
            },
        }

    def mutations(self):
        return [
            call
            for call in self.calls
            if "POST" in call or call[:2] == ["gh", "release"]
        ]


def asset(path, identity=1):
    data = path.read_bytes()
    return {
        "id": identity,
        "name": path.name,
        "state": "uploaded",
        "size": len(data),
        "digest": "sha256:" + hashlib.sha256(data).hexdigest(),
    }


def publish(api, dist):
    return MODULE.create_draft_release(
        REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=api.run
    )


def test_existing_tag_at_another_candidate_is_refused_without_mutation():
    api = FakeGitHub(tag_sha="f" * 40)
    assert MODULE.ensure_tag(REPO, TAG, SHA, api.run) == 1
    assert not api.mutations()


@pytest.mark.parametrize("sha", [SHA, "b" * 64])
def test_matching_lightweight_tag_is_proven_without_mutation(sha):
    api = FakeGitHub(tag_sha=sha)
    assert MODULE.ensure_tag(REPO, TAG, sha, api.run) == 0
    assert not api.mutations()


@pytest.mark.parametrize("sha", ["0123456789abcdef", "main", "", "A" * 40, "a" * 41])
def test_abbreviated_or_noncommit_candidate_refuses_before_api(sha):
    api = FakeGitHub()
    assert MODULE.ensure_tag(REPO, TAG, sha, api.run) == 1
    assert not api.calls


def test_annotated_tag_peels_actual_object_identity():
    oid = "c" * 40
    api = FakeGitHub(tag_sha=oid)
    api.annotated[oid] = {
        "sha": oid,
        "tag": TAG,
        "object": {"type": "commit", "sha": SHA},
    }
    assert MODULE.ensure_tag(REPO, TAG, SHA, api.run) == 0
    assert any(f"/git/tags/{oid}" in call[2] for call in api.calls)
    api.annotated[oid]["object"]["sha"] = "f" * 40
    assert MODULE.ensure_tag(REPO, TAG, SHA, api.run) == 1
    assert not api.mutations()


def test_annotated_cycle_or_wrong_object_proof_refuses():
    oid = "c" * 40
    api = FakeGitHub(tag_sha=oid)
    api.annotated[oid] = {"sha": oid, "object": {"type": "tag", "sha": oid}}
    assert MODULE.ensure_tag(REPO, TAG, SHA, api.run) == 1
    api.annotated[oid] = {"sha": "d" * 40, "object": {"type": "commit", "sha": SHA}}
    assert MODULE.ensure_tag(REPO, TAG, SHA, api.run) == 1
    assert not api.mutations()


@pytest.mark.parametrize("body", ["", "null", "{}", "[]"])
def test_success_status_without_real_tag_json_is_not_absence(body):
    calls = []

    def run(args):
        calls.append(args)
        return subprocess.CompletedProcess(args, 0, body, "")

    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 1
    assert len(calls) == 1


@pytest.mark.parametrize(
    "error", ["HTTP 403: denied", "HTTP 429: quota", "HTTP 500: unavailable", ""]
)
def test_lookup_failure_does_not_create_tag(error):
    calls = []

    def run(args):
        calls.append(args)
        return subprocess.CompletedProcess(args, 1, "", error)

    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 1
    assert len(calls) == 1


def test_explicit_404_creates_exact_tag_and_reobserves_target():
    api = FakeGitHub(tag_sha=None)
    assert MODULE.ensure_tag(REPO, TAG, SHA, api.run) == 0
    assert len(api.calls) == 3
    assert f"sha={SHA}" in api.mutations()[0]
    assert api.calls[-1][2].endswith(f"/git/ref/tags/{TAG}")


def test_created_tag_wrong_target_is_not_success():
    api = FakeGitHub(tag_sha=None)
    api.after_mutation_sha = "f" * 40
    assert MODULE.ensure_tag(REPO, TAG, SHA, api.run) == 1


def test_tag_failure_preserves_real_diagnostic_and_recovery(
    tmp_path, monkeypatch, capsys
):
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    api = FakeGitHub(tag_sha=None)
    api.mutation_error = "HTTP 403: MARKER_TAG_3385"
    assert MODULE.ensure_tag(REPO, TAG, SHA, api.run) == 1
    output = capsys.readouterr()
    assert "MARKER_TAG_3385" in output.out and "MARKER_TAG_3385" in output.err
    assert (
        f"sha={SHA}" in summary.read_text() and "MARKER_TAG_3385" in summary.read_text()
    )


def test_release_requires_preexisting_matching_tag(tmp_path):
    api = FakeGitHub(tag_sha=None, release=False)
    assert publish(api, make_dist(tmp_path)) == 1
    assert not api.mutations()


def test_new_draft_uses_verify_tag_and_validates_uploaded_assets(tmp_path, monkeypatch):
    out = tmp_path / "output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))
    api = FakeGitHub(release=False)
    dist = make_dist(tmp_path)
    assert publish(api, dist) == 0
    create = api.mutations()[0]
    assert "--draft" in create and "--verify-tag" in create
    assert create[create.index("--target") + 1] == SHA
    assert {asset["name"] for asset in api.assets} == {"a.tar.zst", "b.whl"}
    assert out.read_text().splitlines() == ["created=false", "created=true"]


def test_matching_existing_assets_are_skipped_and_only_missing_uploaded(tmp_path):
    dist = make_dist(tmp_path)
    api = FakeGitHub(assets=[asset(dist / "a.tar.zst")])
    assert publish(api, dist) == 0
    upload = api.mutations()[0]
    assert upload[:3] == ["gh", "release", "upload"]
    assert str(dist / "b.whl") in upload and str(dist / "a.tar.zst") not in upload
    assert "--clobber" not in upload
    api.calls.clear()
    assert publish(api, dist) == 0
    assert not api.mutations(), "frozen retry must leave matching bytes intact"


@pytest.mark.parametrize(
    "field,value",
    [
        ("digest", "sha256:" + "f" * 64),
        ("size", 2),
        ("digest", None),
        ("state", "starter"),
        ("name", "foreign.whl"),
    ],
)
def test_conflicting_or_unproven_asset_never_uploads(tmp_path, field, value):
    dist = make_dist(tmp_path)
    existing = asset(dist / "a.tar.zst")
    existing[field] = value
    api = FakeGitHub(assets=[existing])
    assert publish(api, dist) == 1
    assert not api.mutations()


def test_duplicate_assets_refuse_even_with_identical_bytes(tmp_path):
    dist = make_dist(tmp_path)
    existing = asset(dist / "a.tar.zst")
    api = FakeGitHub(assets=[existing, dict(existing, id=2)])
    assert publish(api, dist) == 1
    assert not api.mutations()


def test_paginated_conflict_on_second_page_is_not_hidden(tmp_path):
    names = tuple(f"asset-{i:03}.whl" for i in range(101))
    dist = make_dist(tmp_path, names)
    existing = [asset(path, i + 1) for i, path in enumerate(sorted(dist.iterdir()))]
    existing[-1]["digest"] = "sha256:" + "f" * 64
    api = FakeGitHub(assets=existing)
    assert publish(api, dist) == 1
    assert not api.mutations()
    existing[-1] = asset(dist / names[-1], 101)
    api = FakeGitHub(assets=existing)
    assert publish(api, dist) == 0
    assert not api.mutations()


def test_candidate_change_during_upload_fails_frozen_retry(tmp_path):
    dist = make_dist(tmp_path)
    api = FakeGitHub()
    api.after_mutation_sha = "f" * 40
    assert publish(api, dist) == 1
    assert len(api.mutations()) == 1
    assert "--clobber" not in api.mutations()[0]


def test_mutation_failure_never_reports_success_and_retains_diagnostics(
    tmp_path, monkeypatch, capsys
):
    output = tmp_path / "out"
    summary = tmp_path / "summary"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    api = FakeGitHub(release=False)
    api.mutation_error = "HTTP 403: MARKER_RELEASE_3385"
    assert publish(api, make_dist(tmp_path)) == 1
    captured = capsys.readouterr()
    assert (
        "MARKER_RELEASE_3385" in captured.out
        and "MARKER_RELEASE_3385" in summary.read_text()
    )
    assert output.read_text().strip() == "created=false"
    assert "--verify-tag" in summary.read_text()


def test_gh_stderr_forwarded_on_success(capsys):
    api = FakeGitHub()
    api.success_stderr = "MARKER_OK_3389"
    assert MODULE.ensure_tag(REPO, TAG, SHA, api.run) == 0
    assert "MARKER_OK_3389" in capsys.readouterr().err


# --- workflow wiring ------------------------------------------------------


def test_the_workflow_invokes_the_script_for_both_mutations() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "release-auto.yml").read_text(
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


def test_changed_local_bytes_cannot_reuse_existing_asset(tmp_path):
    dist = make_dist(tmp_path)
    api = FakeGitHub(
        assets=[asset(path, i + 1) for i, path in enumerate(sorted(dist.iterdir()))]
    )
    (dist / "b.whl").write_bytes(b"different frozen bytes")
    assert publish(api, dist) == 1
    assert not api.mutations()


def test_new_artifact_during_upload_prevents_success(tmp_path):
    dist = make_dist(tmp_path)
    api = FakeGitHub()

    def run(args):
        result = api.run(args)
        if args[:3] == ["gh", "release", "upload"]:
            (dist / "unexpected.whl").write_bytes(b"unexpected")
        return result

    assert (
        MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
        == 1
    )


def test_release_lookup_permission_error_never_creates_draft(tmp_path):
    api = FakeGitHub(release=False)

    def run(args):
        if args[:3] == ["gh", "api", f"repos/{REPO}/releases/tags/{TAG}"]:
            return subprocess.CompletedProcess(args, 1, "", "HTTP 403: denied")
        return api.run(args)

    assert (
        MODULE.create_draft_release(
            REPO, TAG, SHA, run_id=RUN_ID, dist=make_dist(tmp_path), run=run
        )
        == 1
    )
    assert not api.mutations()


def test_candidate_drift_before_new_draft_creates_nothing(tmp_path):
    api = FakeGitHub(release=False)

    def run(args):
        result = api.run(args)
        if args[:3] == ["gh", "api", f"repos/{REPO}/releases/tags/{TAG}"]:
            api.tag_sha = "f" * 40
        return result

    assert (
        MODULE.create_draft_release(
            REPO, TAG, SHA, run_id=RUN_ID, dist=make_dist(tmp_path), run=run
        )
        == 1
    )
    assert not api.mutations()


def test_published_partial_release_refuses_missing_upload(tmp_path):
    dist = make_dist(tmp_path)
    api = FakeGitHub(assets=[asset(dist / "a.tar.zst")])
    api.draft = False
    assert publish(api, dist) == 1
    assert not api.mutations()


def test_published_complete_release_is_an_immutable_noop_retry(tmp_path):
    dist = make_dist(tmp_path)
    api = FakeGitHub(
        assets=[asset(path, i + 1) for i, path in enumerate(sorted(dist.iterdir()))]
    )
    api.draft = False
    assert publish(api, dist) == 0
    assert not api.mutations()
