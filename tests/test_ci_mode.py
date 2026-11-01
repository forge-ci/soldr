"""The routine CI budget and explicit full-validation contract (soldr#3344)."""

from __future__ import annotations

import io
import json
import re
from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
MODE = load_script_module(ROOT / ".github" / "scripts" / "ci_mode.py", "ci_mode")
COVERAGE = load_script_module(
    ROOT / ".github" / "scripts" / "ci_full_coverage.py", "ci_full_coverage"
)
SHA = "a" * 40


def _job(name: str) -> str:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    match = re.search(rf"^  {re.escape(name)}:\s*$", workflow, re.MULTILINE)
    assert match, name
    end = re.search(r"^  [\w-]+:\s*$", workflow[match.end() :], re.MULTILINE)
    return workflow[match.start() : match.end() + end.start() if end else None]


def test_mode_is_decided_once_and_dispatch_requires_candidate_sha() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "format('CI full {0}', inputs.candidate_sha)" in workflow
    assert "candidate_sha:" in workflow
    assert (
        "required: true"
        in workflow.split("candidate_sha:", 1)[1].split("permissions:", 1)[0]
    )
    assert "ci_mode.py" in _job("ci-mode")
    assert "mode: ${{ steps.mode.outputs.mode }}" in _job("ci-mode")
    assert "checkout_sha: ${{ steps.mode.outputs.checkout_sha }}" in _job("ci-mode")


def test_docs_only_pr_can_request_full_ci_without_duplicate_lint_status() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    pull_request_trigger = workflow.split("  pull_request:\n", 1)[1].split(
        "  workflow_dispatch:\n", 1
    )[0]
    assert "paths-ignore:" not in pull_request_trigger
    assert not (ROOT / ".github" / "workflows" / "lint-docs-shim.yml").exists()


def test_minimal_jobs_and_full_jobs_have_explicit_dependencies() -> None:
    for name in ("lint", "build-linux-x64"):
        assert "ci-mode" in _job(name)
        assert "|| needs.path-selection.outputs.docs_only != 'true'" in _job(name)
    full_jobs = (
        "pep517-daemon-smoke",
        "windows-e2e-policy",
        "e2e-cross-bootstrap-soldr",
        "e2e-linux-x64-gnu-build",
        "e2e-linux-arm64-build",
        "e2e-linux-arm64",
        "e2e-linux-x64-musl-build",
        "e2e-linux-x64-musl",
        "e2e-linux-arm64-musl-build",
        "e2e-linux-arm64-musl",
        "e2e-macos-x64-build",
        "e2e-macos-x64",
        "e2e-macos-arm64-build",
        "e2e-macos-arm64",
        "e2e-windows-x64-build",
        "e2e-windows-x64",
        "e2e-windows-x64-gnu-build",
        "e2e-windows-x64-gnu",
        "e2e-windows-arm64-build",
        "e2e-windows-arm64",
        "wheel-cross-policy",
        "wheel-cross-verify",
    )
    for name in full_jobs:
        job = _job(name)
        assert "ci-mode" in job, name
        expected = (
            "!= 'minimal'"
            if name
            in {"e2e-cross-bootstrap-soldr", "e2e-macos-arm64-build", "e2e-macos-arm64"}
            else "== 'full'"
        )
        assert f"needs.ci-mode.outputs.mode {expected}" in job, name
    assert "needs.ci-mode.outputs.mode != 'minimal'" in _job("e2e-linux-x64")
    scheduled = set(
        re.findall(
            r"^  ([\w-]+):\s*$",
            WORKFLOW.read_text().split("jobs:\n", 1)[1],
            re.MULTILINE,
        )
    )
    assert scheduled == set(full_jobs) | {
        "ci-mode",
        "lint",
        "build-linux-x64",
        "e2e-linux-x64",
        "full-coverage",
        "ci-summary",
        "cancel-on-bootstrap-failure",
        "cancel-on-e2e-linux-x64-failure",
        # Path- or event-scoped side jobs from the consolidated PR entry
        # point (#3349); each carries its own gate independent of CI mode.
        "path-selection",
        "lint-docs",
        # zackees/ci.yml#6: the cache janitor + budget verdict (ci-pre.yml).
        "ci-pre",
        # GEN-022 (forge-ci/ci.yml#405/#409): merge-queue housekeeping,
        # event-scoped and independent of CI mode.
        "cancel-superseded",
        "queue-cache-janitor",
        "setup-soldr-action",
        "cook-size-gate",
        "macos-recovery-replay",
        # soldr#3276 §6: reld dogfooding evidence jobs. Informational only
        # (not in `full-coverage`'s `needs:`), so they carry their own gate.
        "reld-dogfood-proof-linux",
        "reld-live-fetch-windows-x64",
        "reld-live-fetch-macos-arm64",
    }


def test_manual_run_passes_candidate_sha_to_every_checkout_surface() -> None:
    assert "ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job("lint")
    assert "source_ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job(
        "build-linux-x64"
    )
    assert "source_ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job(
        "e2e-linux-x64"
    )
    assert "ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job(
        "e2e-cross-bootstrap-soldr"
    )
    assert "ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job(
        "wheel-cross-verify"
    )
    target_run = (ROOT / ".github" / "workflows" / "_ci-target-run.yml").read_text()
    assert "source_ref:" in target_run
    assert (
        "ref: ${{ inputs.source_ref != '' && inputs.source_ref || github.sha }}"
        in target_run
    )
    for name in (
        "e2e-linux-arm64",
        "e2e-linux-x64-musl",
        "e2e-linux-arm64-musl",
        "e2e-windows-x64",
        "e2e-windows-x64-gnu",
        "e2e-windows-arm64",
        "e2e-macos-x64",
        "e2e-macos-arm64",
    ):
        job = _job(name)
        assert "uses: ./.github/workflows/_ci-target-run.yml" in job
        assert "source_ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in job


def test_full_mode_overrides_old_fast_build_policy() -> None:
    assert "--full" in _job("windows-e2e-policy")
    assert "--full" in _job("wheel-cross-policy")
    assert "contains(github.event.pull_request.labels" not in _job(
        "pep517-daemon-smoke"
    )


def test_label_changes_recompute_mode_on_same_head_sha() -> None:
    labels = [{"name": "fast-build"}]
    event = {"pull_request": {"head": {"sha": SHA}, "labels": labels}}
    assert MODE.select_mode(
        "pull_request", event, "", author_permission="write"
    ) == MODE.ModeSelection("minimal", SHA)
    labels.append({"name": "ci-test"})
    assert MODE.select_mode(
        "pull_request", event, "", author_permission="write"
    ) == MODE.ModeSelection("test", SHA)
    labels.append({"name": "ci-full"})
    assert MODE.select_mode(
        "pull_request", event, "", author_permission="write"
    ) == MODE.ModeSelection("full", SHA)
    labels.pop()
    assert MODE.select_mode(
        "pull_request", event, "", author_permission="write"
    ) == MODE.ModeSelection("test", SHA)
    labels.pop()
    assert MODE.select_mode(
        "pull_request", event, "", author_permission="write"
    ) == MODE.ModeSelection("minimal", SHA)


def test_ci_test_adds_the_linux_x64_e2e_cell_without_the_full_matrix() -> None:
    assert "needs.ci-mode.outputs.mode != 'minimal'" in _job("e2e-linux-x64")
    assert "needs.ci-mode.outputs.mode == 'full'" in _job("e2e-linux-arm64-build")


def test_dispatch_requires_full_sha_and_push_is_full() -> None:
    assert MODE.select_mode("workflow_dispatch", {}, SHA) == MODE.ModeSelection(
        "full", SHA
    )
    assert MODE.select_mode("push", {"after": SHA}, "") == MODE.ModeSelection(
        "full", SHA
    )
    assert MODE.select_mode("schedule", {}, SHA) == MODE.ModeSelection("full", SHA)
    with pytest.raises(ValueError, match="schedule requires"):
        MODE.select_mode("schedule", {}, "")
    with pytest.raises(ValueError, match="40-character"):
        MODE.select_mode("workflow_dispatch", {}, "main")
    MODE.verify_checkout(SHA, SHA.upper())
    with pytest.raises(ValueError, match="checked out"):
        MODE.verify_checkout(SHA, "b" * 40)


def test_full_sentinel_rejects_skipped_and_missing_canonical_jobs() -> None:
    contract = {"targets": [{"ci": {"build_job": "build-a", "run_job": "run-a"}}]}
    needs = {
        job: {"result": "success"}
        for job in COVERAGE.required_jobs(COVERAGE.JsonDocument(contract))
    }
    assert (
        COVERAGE.coverage_failures(
            COVERAGE.JsonDocument(contract), COVERAGE.JsonDocument(needs)
        )
        == []
    )
    needs["run-a"]["result"] = "skipped"
    assert COVERAGE.coverage_failures(
        COVERAGE.JsonDocument(contract), COVERAGE.JsonDocument(needs)
    ) == ["run-a: skipped"]
    del needs["build-a"]
    assert COVERAGE.coverage_failures(
        COVERAGE.JsonDocument(contract), COVERAGE.JsonDocument(needs)
    ) == [
        "build-a: missing",
        "run-a: skipped",
    ]


def test_full_sentinel_rejects_build_only_supported_platforms() -> None:
    contract = {
        "targets": [
            {
                "triple": "aarch64-apple-darwin",
                "ci": {
                    "build_job": "build-mac-arm",
                    "run_job": None,
                    "execution_exception": {"issue": 3071},
                },
            }
        ]
    }
    needs = {
        job: {"result": "success"}
        for job in COVERAGE.required_jobs(COVERAGE.JsonDocument(contract))
    }
    assert COVERAGE.coverage_failures(
        COVERAGE.JsonDocument(contract), COVERAGE.JsonDocument(needs)
    ) == ["aarch64-apple-darwin: no full-CI execution job (soldr#3071)"]


def test_current_full_contract_requires_both_macos_execution_jobs() -> None:
    contract = json.loads((ROOT / "ci" / "canonical-targets.json").read_text())
    needs = {
        job: {"result": "success"}
        for job in COVERAGE.required_jobs(COVERAGE.JsonDocument(contract))
    }
    assert (
        COVERAGE.coverage_failures(
            COVERAGE.JsonDocument(contract), COVERAGE.JsonDocument(needs)
        )
        == []
    )
    needs["e2e-macos-arm64"]["result"] = "skipped"
    assert COVERAGE.coverage_failures(
        COVERAGE.JsonDocument(contract), COVERAGE.JsonDocument(needs)
    ) == ["e2e-macos-arm64: skipped"]


def test_native_arm_replay_provisions_rust_objcopy_runtime_before_tests() -> None:
    target_run = (ROOT / ".github" / "workflows" / "_ci-target-run.yml").read_text()
    provision = target_run.index("- name: Provision native ARM rust-objcopy runtime")
    replay = target_run.index("- name: Run owned pre-built native tests")
    assert provision < replay
    step = target_run[provision:replay]
    assert "inputs.target == 'aarch64-apple-darwin'" in step
    assert "python .github/scripts/provision_macos_objcopy.py" in step
    assert '--soldr "$SOLDR_BIN"' in step
    assert '--channel "$RUSTUP_TOOLCHAIN"' in step
    assert '--rustc "$RUSTC"' in step


def test_macos_runner_modes_are_opt_in_and_architecture_matched() -> None:
    contract = json.loads((ROOT / "ci" / "canonical-targets.json").read_text())
    mac_targets = {
        target["triple"]: target["ci"]
        for target in contract["targets"]
        if "apple-darwin" in target["triple"]
    }
    assert mac_targets["x86_64-apple-darwin"]["runner"] == "macos-15-intel"
    assert mac_targets["aarch64-apple-darwin"]["runner"] == "macos-15"
    for ci in mac_targets.values():
        build = _job(ci["build_job"])
        run = _job(ci["run_job"])
        assert "upload_test_archive: true" in build
        assert "uses: ./.github/workflows/_ci-target-run.yml" in run
        assert f"runs_on: {ci['runner']}" in run
        assert "source_ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in run
        assert (
            "needs.ci-mode.outputs.mode != 'minimal'" in run
            or "needs.ci-mode.outputs.mode == 'full'" in run
        )
    assert "needs.ci-mode.outputs.mode == 'full'" in _job("e2e-macos-x64")
    assert "needs.ci-mode.outputs.mode != 'minimal'" in _job("e2e-macos-arm64")


def test_full_sentinel_has_all_required_dependencies() -> None:
    contract = json.loads((ROOT / "ci" / "canonical-targets.json").read_text())
    job = _job("full-coverage")
    assert "always()" in job
    assert "needs.ci-mode.outputs.mode == 'full'" in job
    needs_line = next(
        line for line in job.splitlines() if line.startswith("    needs: ")
    )
    dependencies = {
        name.strip() for name in needs_line.split("[", 1)[1].split("]", 1)[0].split(",")
    }
    for required in COVERAGE.required_jobs(COVERAGE.JsonDocument(contract)):
        assert required in dependencies, required


def test_full_overrides_platform_and_wheel_path_policies() -> None:
    windows = load_script_module(
        ROOT / ".github" / "scripts" / "windows_e2e_policy.py", "windows_e2e_full"
    )
    wheel = load_script_module(
        ROOT / ".github" / "scripts" / "wheel_lane_policy.py", "wheel_full"
    )
    assert windows.decide_windows_e2e(
        event_name="pull_request",
        labels=["fast-build"],
        changed_paths=["README.md"],
        full=True,
    ).run
    assert wheel.decide_wheel_lane(
        event_name="pull_request", changed_paths=["README.md"], full=True
    ).run


def test_expensive_smokes_run_only_in_full_mode_on_candidate_sha() -> None:
    for name in ("setup-soldr-action", "cook-size-gate"):
        job = _job(name)
        assert "needs: [ci-mode, build-linux-x64]" in job
        assert "needs.ci-mode.outputs.mode == 'full'" in job
        assert "source_ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in job
        workflow = (ROOT / ".github" / "workflows" / f"{name}.yml").read_text()
        triggers = workflow.split("on:\n", 1)[1].split("permissions:", 1)[0]
        assert "  push:" not in triggers
        assert "  workflow_call:" in triggers
        assert "source_ref:" in triggers
        assert "ref: ${{ inputs.source_ref || github.sha }}" in workflow
        assert name in COVERAGE.EXTRA_REQUIRED
        assert name in _job("full-coverage").split("needs: ", 1)[1].split("\n", 1)[0]


def test_full_mode_overrides_documentation_only_skip() -> None:
    assert "needs.ci-mode.outputs.mode != 'full'" in _job("lint-docs")
    for name in ("lint", "build-linux-x64"):
        assert "needs.ci-mode.outputs.mode == 'full'" in _job(name)


@pytest.mark.parametrize("permission", ["read", "triage", "none", None, "unknown"])
@pytest.mark.parametrize("association", ["CONTRIBUTOR", "MEMBER", "OWNER"])
def test_external_author_is_full_without_labels(permission, association) -> None:
    event = {
        "pull_request": {
            "head": {"sha": SHA},
            "labels": [],
            "author_association": association,
        }
    }
    assert MODE.select_mode(
        "pull_request", event, "", author_permission=permission
    ) == MODE.ModeSelection("full", SHA)


@pytest.mark.parametrize("permission", ["write", "maintain", "admin"])
def test_effective_writer_keeps_minimal_even_from_a_fork(permission) -> None:
    event = {
        "pull_request": {
            "head": {"sha": SHA, "repo": {"fork": True}},
            "labels": [],
            "author_association": "NONE",
        }
    }
    assert MODE.select_mode(
        "pull_request", event, "", author_permission=permission
    ) == MODE.ModeSelection("minimal", SHA)


def test_external_label_removal_and_revoked_permission_cannot_downgrade() -> None:
    labels = [{"name": "ci-full"}]
    event = {"pull_request": {"head": {"sha": SHA}, "labels": labels}}
    assert MODE.select_mode(
        "pull_request", event, "", author_permission="read"
    ) == MODE.ModeSelection("full", SHA)
    labels.clear()
    assert MODE.select_mode(
        "pull_request", event, "", author_permission="read"
    ) == MODE.ModeSelection("full", SHA)
    assert MODE.select_mode(
        "pull_request", event, "", author_permission="write"
    ) == MODE.ModeSelection("minimal", SHA)
    assert MODE.select_mode(
        "pull_request", event, "", author_permission=None
    ) == MODE.ModeSelection("full", SHA)


@pytest.mark.parametrize("age", [61, 3600, -60])
def test_stale_or_future_permission_response_is_unknown(age) -> None:
    data = {"permission": "admin", "user": {"login": "writer"}}
    assert (
        MODE.permission_from_response(
            data,
            login="writer",
            response_date="Thu, 01 Oct 2026 17:00:00 GMT",
            now=1790874000 + age,
        )
        is None
    )


@pytest.mark.parametrize("permission", ["admin", "write", "read", "none"])
def test_fresh_matching_permission_response_is_accepted(permission) -> None:
    assert (
        MODE.permission_from_response(
            {"permission": permission, "user": {"login": "Writer"}},
            login="writer",
            response_date="Thu, 01 Oct 2026 17:00:00 GMT",
            now=1790874005,
        )
        == permission
    )


@pytest.mark.parametrize(
    "data",
    [
        {"permission": "admin", "user": {"login": "different"}},
        {"permission": ["admin"], "user": {"login": "writer"}},
        {"permission": "custom-admin", "user": {"login": "writer"}},
        {"permission": "admin"},
    ],
)
def test_mismatched_or_malformed_permission_response_is_unknown(data) -> None:
    assert (
        MODE.permission_from_response(
            data,
            login="writer",
            response_date="Thu, 01 Oct 2026 17:00:00 GMT",
            now=1790874005,
        )
        is None
    )


def test_selector_executes_trusted_base_source_with_read_only_token() -> None:
    job = _job("ci-mode")
    assert "ref: ${{ github.event.pull_request.base.sha || github.sha }}" in job
    assert "python3 .ci-selector/.github/scripts/ci_mode.py" in job
    assert "GITHUB_TOKEN: ${{ github.token }}" in job
    assert "permissions:\n  contents: read" in WORKFLOW.read_text()
    assert "pull_request_target:" not in WORKFLOW.read_text()


def test_live_lookup_is_bound_to_base_repo_and_requests_fresh_data(monkeypatch) -> None:
    event = {
        "pull_request": {
            "base": {"repo": {"full_name": "o/r"}},
            "user": {"login": "writer"},
        }
    }
    requests = []

    def respond(request, timeout):
        requests.append((request, timeout))
        response = io.BytesIO(
            json.dumps({"permission": "write", "user": {"login": "writer"}}).encode()
        )
        response.headers = {"Date": "Thu, 01 Oct 2026 17:00:00 GMT"}
        return response

    monkeypatch.setattr(MODE.urllib.request, "urlopen", respond)
    monkeypatch.setattr(MODE.time, "time", lambda: 1790874005)
    assert MODE.lookup_author_permission(event, "o/r", "test-token") == "write"
    assert MODE.lookup_author_permission(event, "o/r", "test-token") == "write"
    assert len(requests) == 2  # reruns never reuse a prior decision
    request, timeout = requests[0]
    assert (
        request.full_url
        == "https://api.github.com/repos/o/r/collaborators/writer/permission"
    )
    assert request.get_header("Cache-control") == "no-cache"
    assert timeout == 20
    assert MODE.lookup_author_permission(event, "different/repo", "test-token") is None
    assert len(requests) == 2


def test_permission_api_failure_is_unknown_and_selects_full(monkeypatch) -> None:
    event = {
        "pull_request": {
            "head": {"sha": SHA},
            "labels": [],
            "base": {"repo": {"full_name": "o/r"}},
            "user": {"login": "dependabot[bot]", "type": "Bot"},
        }
    }

    def unavailable(*_args, **_kwargs):
        raise MODE.urllib.error.URLError("permission unavailable")

    monkeypatch.setattr(MODE.urllib.request, "urlopen", unavailable)
    permission = MODE.lookup_author_permission(event, "o/r", "test-token")
    assert permission is None
    assert MODE.select_mode(
        "pull_request", event, "", author_permission=permission
    ) == MODE.ModeSelection("full", SHA)


@pytest.mark.parametrize(
    "repository",
    [
        "zackees/soldr",
        "zackees/zccache",
        "FastLED/fbuild",
        "zackees/clud",
        "zackees/mimalloc-pprof",
        "zackees/bosn",
        "zackees/kernal-api",
        "FastLED/cli",
        "FastLED/FastLED",
    ],
)
def test_one_selector_is_portable_to_every_fleet_identity(
    repository, monkeypatch
) -> None:
    event = {
        "pull_request": {
            "head": {"sha": SHA},
            "labels": [],
            "base": {"repo": {"full_name": repository}},
            "user": {"login": "contributor"},
        }
    }
    requested = []

    def respond(request, timeout):
        requested.append(request.full_url)
        response = io.BytesIO(
            json.dumps(
                {"permission": "read", "user": {"login": "contributor"}}
            ).encode()
        )
        response.headers = {"Date": "Thu, 01 Oct 2026 17:00:00 GMT"}
        return response

    monkeypatch.setattr(MODE.urllib.request, "urlopen", respond)
    monkeypatch.setattr(MODE.time, "time", lambda: 1790874005)
    permission = MODE.lookup_author_permission(event, repository, "test-token")
    assert requested == [
        f"https://api.github.com/repos/{repository}/collaborators/contributor/permission"
    ]
    assert MODE.SELECTOR_SCHEMA == "fleet-ci-mode/v1"
    assert MODE.select_mode(
        "pull_request", event, "", author_permission=permission
    ) == MODE.ModeSelection("full", SHA)
    assert MODE.select_mode(
        "pull_request", event, "", author_permission="write"
    ) == MODE.ModeSelection("minimal", SHA)


@pytest.mark.parametrize("job", ["setup-soldr-action", "cook-size-gate"])
def test_full_smokes_receive_no_repository_secrets(job) -> None:
    """Full validation runs untrusted candidates with only the read-only token."""
    assert "secrets:" not in _job(job)


def test_merge_queue_runs_linux_lane_on_exact_group_identity():
    event = {"merge_group": {"head_sha": SHA}}
    assert MODE.select_mode("merge_group", event, "") == MODE.ModeSelection(
        "minimal", SHA
    )
    with pytest.raises(ValueError, match="merge_group"):
        MODE.select_mode("merge_group", {"merge_group": {"head_sha": "main"}}, "")


@pytest.mark.parametrize(
    "alias, canonical, mode",
    [("ci:full", "ci-full", "full"), ("ci-windows", "ci-test", "test")],
)
def test_reviewed_adapter_aliases_use_common_selection(alias, canonical, mode):
    event = {"pull_request": {"head": {"sha": SHA}, "labels": [{"name": alias}]}}
    assert MODE.select_mode(
        "pull_request",
        event,
        "",
        author_permission="write",
        label_aliases=MODE.LabelAliases((MODE.LabelAlias(alias, canonical),)),
    ) == MODE.ModeSelection(mode, SHA)
    assert MODE.select_mode(
        "pull_request",
        event,
        "",
        author_permission="read",
        label_aliases=MODE.LabelAliases((MODE.LabelAlias(alias, canonical),)),
    ) == MODE.ModeSelection("full", SHA)


@pytest.mark.parametrize(
    "adapter",
    [
        {},
        {"schema_version": True, "label_aliases": {}},
        {"schema_version": 1.0, "label_aliases": {}},
        {"schema_version": 1, "label_aliases": {"ci-full": "ci-test"}},
        {"schema_version": 1, "label_aliases": {"legacy": "minimal"}},
        {"schema_version": 1, "label_aliases": {"": "ci-full"}},
    ],
)
def test_invalid_selector_adapter_cannot_override_literal_contract(adapter):
    with pytest.raises(ValueError):
        MODE.label_aliases_from_adapter(adapter)


@pytest.mark.parametrize(
    "mutation", ["valid", "stale", "wrong-base", "wrong-number", "bad-labels", "closed"]
)
def test_current_pr_lookup_binds_fresh_metadata_to_original_scope(
    monkeypatch, mutation
):
    original = {"number": 12, "pull_request": {"base": {"repo": {"full_name": "o/r"}}}}
    data = {
        "number": 12,
        "state": "open",
        "head": {"sha": SHA},
        "base": {"repo": {"full_name": "o/r"}},
        "labels": [{"name": "ci-full"}],
    }
    if mutation == "wrong-base":
        data["base"]["repo"]["full_name"] = "other/repo"
    elif mutation == "wrong-number":
        data["number"] = 13
    elif mutation == "bad-labels":
        data["labels"] = [{}]
    elif mutation == "closed":
        data["state"] = "closed"
    requests = []

    def respond(request, timeout):
        requests.append(request.full_url)
        response = io.BytesIO(json.dumps(data).encode())
        response.headers = {"Date": "Thu, 01 Oct 2026 17:00:00 GMT"}
        return response

    monkeypatch.setattr(MODE.urllib.request, "urlopen", respond)
    monkeypatch.setattr(
        MODE.time, "time", lambda: 1790874000 + (61 if mutation == "stale" else 5)
    )
    result = MODE.lookup_current_pull_request(original, "o/r", "test-token")
    assert requests == ["https://api.github.com/repos/o/r/pulls/12"]
    assert result == (
        {"number": 12, "pull_request": data} if mutation == "valid" else None
    )
