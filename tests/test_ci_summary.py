"""Always-running, fail-closed merge summary (setup-soldr#523)."""

import json
from pathlib import Path

import pytest
import yaml
from conftest import load_script_module

ROOT = Path(__file__).parents[1]
SUMMARY = load_script_module(ROOT / ".github/scripts/ci_summary.py", "ci_summary")
SHA = "a" * 40


def adapter():
    return {
        "schema_version": 1,
        "minimal_jobs": ["lint", "host"],
        "docs_jobs": ["docs-lint"],
        "test_jobs": ["extra-test"],
    }


def full_contract():
    return {
        "schema_version": 1,
        "required_jobs": ["lint", "host", "platform-smoke"],
        "targets": [
            {"triple": "board", "ci": {"build_job": "build", "run_job": "run"}}
        ],
    }


def doc(fields):
    return SUMMARY.JsonDocument(fields)


def check(needs, **overrides):
    inputs = {
        "mode": "full",
        "docs_only": False,
        "event_name": "pull_request",
        "author_permission": "read",
        "expected_sha": SHA,
        "selected_sha": SHA,
        **overrides,
    }
    return SUMMARY.summary_failures(
        doc(adapter()), doc(full_contract()), doc(needs), **inputs
    )


def passing():
    return {
        job: {"result": "success"}
        for job in SUMMARY.required_jobs(
            doc(adapter()), doc(full_contract()), "full", False
        )
    }


@pytest.mark.parametrize(
    "state", ["missing", "skipped", "cancelled", "neutral", "failure"]
)
def test_green_aggregate_cannot_hide_one_unsuccessful_platform(state):
    needs = passing()
    if state == "missing":
        del needs["run"]
    else:
        needs["run"] = {"result": state}
    assert f"run: {state}" in check(needs)


def test_full_pass_binds_candidate_and_includes_the_test_tier():
    assert check(passing()) == []
    assert check(passing(), selected_sha="b" * 40)
    needs = passing()
    del needs["extra-test"]
    assert "extra-test: missing" in check(needs)


@pytest.mark.parametrize("permission", ["read", "triage", "none", "unknown", ""])
def test_external_author_cannot_downgrade_even_with_all_minimal_jobs_green(permission):
    assert "external or unresolved author requires full CI" in check(
        passing(), mode="minimal", author_permission=permission
    )


def test_trusted_minimal_does_not_require_skipped_full_jobs():
    needs = {
        job: {"result": "success"}
        for job in ["ci-mode", "path-selection", "lint", "host"]
    }
    assert check(needs, mode="minimal", author_permission="write") == []
    assert check(needs, mode="test", author_permission="write") == [
        "extra-test: missing"
    ]
    assert check(
        needs, mode="minimal", author_permission="write", event_name="workflow_dispatch"
    )


def test_docs_gate_requires_successful_path_selection_and_docs_lint():
    needs = {
        job: {"result": "success"} for job in ["ci-mode", "path-selection", "docs-lint"]
    }
    assert check(needs, mode="minimal", docs_only=True, author_permission="admin") == []
    needs["path-selection"] = {"result": "failure"}
    assert check(needs, mode="minimal", docs_only=True, author_permission="admin")
    assert check(needs, docs_only=True)


def test_unknown_mode_fails_clearly():
    with pytest.raises(ValueError, match="unknown CI mode"):
        check(passing(), mode="")


def test_merge_queue_accepts_linux_lane_or_full():
    assert check(passing(), event_name="merge_group") == []
    assert "merge queue requires the linux host lane or full CI" in check(
        passing(), event_name="merge_group", mode="test"
    )


@pytest.mark.parametrize("event", ["push", "schedule"])
def test_post_merge_events_require_full(event):
    assert check(passing(), event_name=event) == []
    assert any(
        "requires full CI" in failure
        for failure in check(passing(), event_name=event, mode="minimal")
    )


def _job(name, conclusion="success", status="completed"):
    return {"name": name, "status": status, "conclusion": conclusion}


def test_run_attempt_audit_passes_success_and_skipped():
    jobs = [
        _job("Lint"),
        _job("Cook size gate", "skipped"),
        _job("CI summary", None, "in_progress"),
    ]
    assert SUMMARY.run_attempt_failures(jobs) == []


@pytest.mark.parametrize(
    "conclusion", ["failure", "cancelled", "timed_out", "action_required", "neutral"]
)
def test_run_attempt_audit_fails_any_unsuccessful_job(conclusion):
    failures = SUMMARY.run_attempt_failures(
        [_job("Lint"), _job("CI Pre / plan", conclusion)]
    )
    assert failures == [f"job 'CI Pre / plan' concluded {conclusion}"]


def test_run_attempt_audit_fails_unfinished_and_empty():
    assert SUMMARY.run_attempt_failures(
        [_job("reld dogfood proof (linux x64)", None, "in_progress")]
    )
    assert SUMMARY.run_attempt_failures([]) == ["run attempt job list is empty"]


def test_run_attempt_audit_exempts_only_post_summary_jobs():
    jobs = [
        _job("End-of-queue cache janitor", None, "queued"),
        _job("Cancel run (bootstrap soldr failed)", "skipped"),
        _job("CI Pre / janitor", None, "in_progress"),
    ]
    assert SUMMARY.run_attempt_failures(jobs) == []


def test_run_attempt_audit_still_fails_a_finished_unordered_job():
    assert SUMMARY.run_attempt_failures([_job("CI Pre / janitor", "failure")])


def test_summary_needs_every_job_it_can_wait_for():
    """The audit is only sound if every non-exempt job finishes first."""
    workflow = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())
    jobs = workflow["jobs"]
    needs = set(jobs["ci-summary"]["needs"])
    for job_id, job in jobs.items():
        name = str(job.get("name", job_id))
        if (
            job_id == "ci-summary"
            or name.startswith(SUMMARY._AUDIT_EXEMPT_PREFIXES)
            or name.startswith(SUMMARY._AUDIT_UNORDERED_PREFIXES)
        ):
            continue
        assert job_id in needs, job_id


def test_test_adapter_must_add_real_jobs_beyond_minimal():
    declaration = adapter()
    declaration["test_jobs"] = ["lint", "host"]
    with pytest.raises(ValueError, match="test tier must add"):
        SUMMARY.required_jobs(doc(declaration), doc(full_contract()), "test", False)


@pytest.mark.parametrize("group", ["minimal_jobs", "docs_jobs", "test_jobs"])
def test_policy_checks_cannot_masquerade_as_test_jobs(group):
    declaration = adapter()
    declaration[group] = ["ci-mode"]
    with pytest.raises(ValueError, match="policy prerequisites"):
        SUMMARY.required_jobs(doc(declaration), doc(full_contract()), "full", False)


def test_build_only_target_cannot_pass_summary():
    contract = full_contract()
    contract["targets"][0]["ci"]["run_job"] = None
    assert SUMMARY.summary_failures(
        doc(adapter()),
        doc(contract),
        doc(passing()),
        mode="full",
        docs_only=False,
        event_name="pull_request",
        author_permission="read",
        expected_sha=SHA,
        selected_sha=SHA,
    ) == ["board: no full-CI execution job"]


def cli_environment(tmp_path, monkeypatch):
    monkeypatch.setattr(
        SUMMARY,
        "list_run_attempt_jobs",
        lambda *_: [{"name": "Lint", "status": "completed", "conclusion": "success"}],
    )
    adapter_path = tmp_path / "adapter.json"
    manifest_path = tmp_path / "full.json"
    report_path = tmp_path / "summary.json"
    adapter_path.write_text(json.dumps(adapter()))
    manifest_path.write_text(json.dumps(full_contract()))
    for key, value in {
        "CI_NEEDS_JSON": json.dumps(passing()),
        "CI_MODE": "full",
        "DOCS_ONLY": "false",
        "AUTHOR_PERMISSION": "read",
        "GITHUB_EVENT_NAME": "pull_request",
        "EXPECTED_SHA": SHA,
        "SELECTED_SHA": "b" * 40,
    }.items():
        monkeypatch.setenv(key, value)
    monkeypatch.setattr(
        "sys.argv",
        [
            "ci_summary.py",
            "--adapter",
            str(adapter_path),
            "--full-contract",
            str(manifest_path),
            "--report",
            str(report_path),
        ],
    )
    return report_path


def test_cli_failure_report_preserves_candidate_identity(tmp_path, monkeypatch):
    report_path = cli_environment(tmp_path, monkeypatch)
    assert SUMMARY.main() == 1
    report = json.loads(report_path.read_text())
    assert report["schema"] == "fleet-ci-summary/v1"
    assert report["expected_sha"] == SHA
    assert not report["success"]
    assert report["failures"]


@pytest.mark.parametrize(
    "permission, exit_code", [("read", 1), (None, 1), ("admin", 0)]
)
def test_rerun_summary_rechecks_permission_instead_of_cached_write(
    tmp_path, monkeypatch, permission, exit_code
):
    report_path = cli_environment(tmp_path, monkeypatch)
    event_path = tmp_path / "event.json"
    event = {
        "pull_request": {
            "user": {"login": "author"},
            "head": {"sha": SHA},
            "labels": [],
        }
    }
    event_path.write_text(json.dumps(event))
    monkeypatch.setenv("GITHUB_EVENT_PATH", str(event_path))
    monkeypatch.setenv("GITHUB_REPOSITORY", "o/r")
    monkeypatch.setenv("GITHUB_TOKEN", "test-token")
    monkeypatch.setenv("CI_MODE", "minimal")
    monkeypatch.setenv("AUTHOR_PERMISSION", "write")
    monkeypatch.setenv("SELECTED_SHA", SHA)
    queries = []

    def lookup(payload, repository, token):
        queries.append((payload, repository, token))
        return permission

    monkeypatch.setitem(SUMMARY.SELECTOR, "lookup_author_permission", lookup)
    monkeypatch.setitem(
        SUMMARY.SELECTOR, "lookup_current_pull_request", lambda *_: event
    )
    assert SUMMARY.main() == exit_code
    assert queries == [(event, "o/r", "test-token")]
    report = json.loads(report_path.read_text())
    assert report["author_permission"] == (permission or "unknown")


@pytest.mark.parametrize("label", ["ci-test", "ci-full"])
def test_rerun_cannot_reuse_minimal_after_current_labels_require_more(label):
    event = {"pull_request": {"head": {"sha": SHA}, "labels": [{"name": label}]}}
    assert SUMMARY.current_selection_failures(
        event,
        mode="minimal",
        selected_sha=SHA,
        permission="admin",
        aliases=SUMMARY.SELECTOR["LabelAliases"](),
    )


def test_current_head_and_legacy_labels_are_authoritative():
    event = {"pull_request": {"head": {"sha": "b" * 40}, "labels": []}}
    assert SUMMARY.current_selection_failures(
        event,
        mode="full",
        selected_sha=SHA,
        permission="admin",
        aliases=SUMMARY.SELECTOR["LabelAliases"](),
    )
    event["pull_request"]["head"]["sha"] = SHA
    event["pull_request"]["labels"] = [{"name": "ci:full"}]
    assert SUMMARY.current_selection_failures(
        event,
        mode="test",
        selected_sha=SHA,
        permission="admin",
        aliases=SUMMARY.SELECTOR["LabelAliases"](
            (SUMMARY.SELECTOR["LabelAlias"]("ci:full", "ci-full"),)
        ),
    )


def test_missing_current_metadata_fails_even_when_cached_full_is_green():
    assert SUMMARY.current_selection_failures(
        None,
        mode="full",
        selected_sha=SHA,
        permission="admin",
        aliases=SUMMARY.SELECTOR["LabelAliases"](),
    )


def test_cli_rerun_uses_live_labels_instead_of_original_event(tmp_path, monkeypatch):
    report_path = cli_environment(tmp_path, monkeypatch)
    original = {"pull_request": {"head": {"sha": SHA}, "labels": []}}
    current = {"pull_request": {"head": {"sha": SHA}, "labels": [{"name": "ci-full"}]}}
    event_path = tmp_path / "event.json"
    event_path.write_text(json.dumps(original))
    monkeypatch.setenv("GITHUB_EVENT_PATH", str(event_path))
    monkeypatch.setenv("CI_MODE", "minimal")
    monkeypatch.setenv("SELECTED_SHA", SHA)
    monkeypatch.setitem(
        SUMMARY.SELECTOR, "lookup_current_pull_request", lambda *_: current
    )
    permission_inputs = []

    def permission(payload, *_):
        permission_inputs.append(payload)
        return "admin"

    monkeypatch.setitem(SUMMARY.SELECTOR, "lookup_author_permission", permission)
    assert SUMMARY.main() == 1
    assert permission_inputs == [current]
    assert (
        "current PR requires full CI; cached mode is minimal"
        in json.loads(report_path.read_text())["failures"]
    )


def test_merge_summary_runs_even_when_selector_or_required_cells_fail() -> None:
    """The summary is the stable merge context, so it must survive upstream failure.

    A skipped `Full coverage` job is not a merge summary: without an
    `always()` gate the context this change installs would simply not be
    reported on the runs where a decision is most needed.
    """
    jobs = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())["jobs"]
    summary = jobs.get("ci-summary")
    assert summary, "a skipped Full coverage job is not a merge summary"
    assert summary["name"] == "CI summary"
    assert summary["if"] == "${{ always() }}"
    assert set(jobs["full-coverage"]["needs"]) <= set(summary["needs"])
    assert {"lint-docs", "path-selection", "full-coverage"} <= set(summary["needs"])


def test_run_attempt_audit_allows_only_a_superseded_cache_janitor():
    assert (
        SUMMARY.run_attempt_failures([_job("CI Pre / Cache janitor", "cancelled")])
        == []
    )
    assert SUMMARY.run_attempt_failures([_job("CI Pre / Cache janitor", "failure")])
    assert SUMMARY.run_attempt_failures(
        [_job("CI Pre / Repository Actions cache budget", "cancelled")]
    )
