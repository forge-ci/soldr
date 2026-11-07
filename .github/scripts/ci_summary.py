#!/usr/bin/env python3
"""Require the selected CI tier on one candidate before a PR may merge."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import runpy
import time
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, TypedDict

COVERAGE = runpy.run_path(str(Path(__file__).with_name("ci_full_coverage.py")))
SELECTOR = runpy.run_path(str(Path(__file__).with_name("ci_mode.py")))
SCHEMA = "fleet-ci-summary/v1"


@dataclass(frozen=True)
class JsonDocument:
    """A parsed JSON object whose schema the consumers validate field by field."""

    fields: dict

    def get(self, key: str, default: Any = None) -> Any:
        return self.fields.get(key, default)


class Selection(TypedDict):
    """The resolved mode and identity supplied by the workflow."""

    mode: str
    docs_only: bool
    event_name: str
    author_permission: str
    expected_sha: str
    selected_sha: str
    trusted: bool
    skip_lint: bool


def required_jobs(
    adapter: JsonDocument,
    full: JsonDocument,
    mode: str,
    docs_only: bool,
    skip_lint: bool = False,
) -> set[str]:
    """Adapters declare a meaningful minimal gate and nonempty test expansion.

    `skip_lint` is ci-mode's GATE-010 output: the head is trusted AND every gate
    mapped to the Lint job carries a stamp-valid `Ci-Attestation:` trailer. Only
    then does the workflow skip `lint` and run the `lint-docs` stand-in, so only
    then may the summary require `docs_jobs` instead of `minimal_jobs`. Using
    `trusted` here required `lint-docs` from PRs where Lint actually ran, which
    the workflow never starts (red summary on a green PR).
    """
    groups = {}
    for name in ("minimal_jobs", "docs_jobs", "test_jobs"):
        groups[name] = COVERAGE["required_jobs"](
            COVERAGE["JsonDocument"](
                {
                    "schema_version": adapter.get("schema_version"),
                    "required_jobs": adapter.get(name),
                    "targets": [],
                }
            )
        )
    policy_jobs = {"ci-mode", "path-selection", "full-coverage"}
    if any(group & policy_jobs for group in groups.values()):
        raise ValueError("tier jobs cannot name policy prerequisites")
    if not groups["test_jobs"] - groups["minimal_jobs"]:
        raise ValueError("test tier must add jobs beyond minimal")
    jobs = {"ci-mode", "path-selection"}
    if mode == "minimal":
        return jobs | groups["docs_jobs" if docs_only or skip_lint else "minimal_jobs"]
    if mode == "test":
        return jobs | groups["minimal_jobs"] | groups["test_jobs"]
    if mode == "full":
        return (
            jobs
            | groups["minimal_jobs"]
            | groups["test_jobs"]
            | COVERAGE["required_jobs"](COVERAGE["JsonDocument"](full.fields))
            | {"full-coverage"}
        )
    raise ValueError("missing or unknown CI mode")


def summary_failures(
    adapter: JsonDocument,
    full: JsonDocument,
    needs: JsonDocument,
    *,
    mode: str,
    docs_only: bool,
    event_name: str,
    author_permission: str,
    expected_sha: str,
    selected_sha: str,
    trusted: bool = False,
    skip_lint: bool = False,
) -> list[str]:
    failures = []
    if event_name not in (
        "pull_request",
        "push",
        "workflow_dispatch",
        "merge_group",
        "schedule",
    ):
        failures.append("missing or unsupported event")
    if (
        event_name == "pull_request"
        and author_permission not in ("write", "maintain", "admin")
        and mode != "full"
    ):
        failures.append("external or unresolved author requires full CI")
    if event_name == "workflow_dispatch" and mode != "full":
        failures.append("candidate dispatch requires full CI")
    if event_name == "schedule" and mode != "full":
        failures.append("nightly schedule requires full CI")
    if event_name == "push" and mode != "full":
        failures.append("push to main requires full CI")
    if event_name == "merge_group" and mode not in ("minimal", "full"):
        failures.append("merge queue requires the linux host lane or full CI")
    if trusted and (
        event_name != "pull_request"
        or mode != "minimal"
        or author_permission not in ("write", "maintain", "admin")
    ):
        failures.append("attested skip is limited to a writer's minimal PR")
    # This bit comes from reviewed ci-mode's attestation verifier. It is not
    # evidence furnished by a PR artifact, and never replaces test/full cells.
    jobs = required_jobs(adapter, full, mode, docs_only, skip_lint)
    failures += COVERAGE["coverage_failures"](
        COVERAGE["JsonDocument"](
            {"schema_version": 1, "required_jobs": sorted(jobs), "targets": []}
        ),
        COVERAGE["JsonDocument"](needs.fields),
        expected_sha=expected_sha,
        selected_sha=selected_sha,
    )
    if mode == "full":
        # Retain the canonical refusal of build-only/blocked execution cells.
        failures += COVERAGE["coverage_failures"](
            COVERAGE["JsonDocument"](full.fields),
            COVERAGE["JsonDocument"](needs.fields),
            expected_sha=expected_sha,
            selected_sha=selected_sha,
        )
    return sorted(set(failures))


# Jobs that legitimately run after (or alongside) the summary: the summary is
# their prerequisite, or they exist only to cancel an already-failing run.
_AUDIT_EXEMPT_PREFIXES = ("CI summary", "Cancel run (", "End-of-queue cache janitor")
# CI Pre must never be waited on (tests/test_ci_pre_workflow.py), so it may
# still be running; once it has finished, its conclusion is audited like any.
_AUDIT_UNORDERED_PREFIXES = ("CI Pre",)
_AUDIT_OK = ("success", "skipped")
# GitHub keeps one *pending* job per concurrency group: a newer run's
# `cache-janitor` sweep replaces this one (cancel-in-progress: false only
# protects a running sweep). It validates nothing, so supersession is allowed.
_AUDIT_SUPERSEDABLE = {"CI Pre / Cache janitor": "cancelled"}


def starved_nightly_probe(job: dict, event_name: str) -> bool:
    """Only an explicitly unassigned, unexecuted nightly probe is infrastructure.

    Missing API fields are not evidence. A started or failed macOS job must
    retain the ordinary failure verdict; PR and queue gates never relax.
    """
    return (
        event_name == "schedule"
        and job.get("name") == "reld live fetch (macos arm64)"
        and job.get("status") == "completed"
        and job.get("conclusion") == "cancelled"
        and "runner_name" in job
        and job["runner_name"] in (None, "")
        and job.get("runner_id") == 0
        and job.get("steps") == []
    )


def run_attempt_failures(jobs: list[dict]) -> list[str]:
    """Fail on any job of this run attempt that did not succeed (obs gate_jobs).

    ``needs`` only reflects declared prerequisites; this audit lists every job
    GitHub recorded for the attempt -- reusable-workflow children included --
    so a failed or cancelled job cannot hide behind one aggregating context.
    """
    if not jobs:
        return ["run attempt job list is empty"]
    failures = []
    for job in jobs:
        name = str(job.get("name", ""))
        if name.startswith(_AUDIT_EXEMPT_PREFIXES):
            continue
        if job.get("status") != "completed":
            if name.startswith(_AUDIT_UNORDERED_PREFIXES):
                continue
            failures.append(f"job {name!r} has not completed ({job.get('status')})")
        elif job.get("conclusion") not in _AUDIT_OK and (
            _AUDIT_SUPERSEDABLE.get(name) != job.get("conclusion")
        ):
            failures.append(f"job {name!r} concluded {job.get('conclusion')}")
    return failures


def list_run_attempt_jobs(repository: str, run_id: str, attempt: str, token: str):
    """Every job of one run attempt, following pagination."""
    jobs: list[dict] = []
    page = 1
    while True:
        request = urllib.request.Request(
            f"https://api.github.com/repos/{repository}/actions/runs/{run_id}"
            f"/attempts/{attempt}/jobs?per_page=100&page={page}",
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            body = json.loads(response.read().decode("utf-8"))
        batch = body.get("jobs", [])
        jobs += batch
        if len(jobs) >= int(body.get("total_count", 0)) or not batch:
            return jobs
        page += 1


def settled_run_attempt_jobs(repository: str, run_id: str, attempt: str, token: str):
    """Refresh stale job states before auditing; CI Pre is never waited on.

    Eight snapshots allow 70 seconds of consistency lag, within the summary's
    five-minute timeout. Exhaustion returns the last snapshot for the normal
    fail-closed audit; an unfinished job never becomes a successful job.
    """
    for number in range(1, 9):
        jobs = list_run_attempt_jobs(repository, run_id, attempt, token)
        pending = [
            str(job.get("name", ""))
            for job in jobs
            if job.get("status") != "completed"
            and not str(job.get("name", "")).startswith(
                _AUDIT_EXEMPT_PREFIXES + _AUDIT_UNORDERED_PREFIXES
            )
        ]
        print(
            f"Run attempt jobs: attempt {number}/8; unfinished jobs: {pending}",
            flush=True,
        )
        if not pending or number == 8:
            return jobs
        time.sleep(10)
    raise AssertionError("unreachable job listing attempt")


def current_selection_failures(
    event: dict | None,
    *,
    mode: str,
    selected_sha: str,
    permission: str,
    aliases: Any,
) -> list[str]:
    """A rerun may exceed today's requested tier, but cannot undercut it."""
    if event is None:
        return ["current PR metadata is unavailable"]
    try:
        selection = SELECTOR["select_mode"](
            "pull_request",
            event,
            "",
            author_permission=permission,
            label_aliases=aliases,
        )
    except (ValueError, TypeError, KeyError):
        return ["current PR selection is invalid"]
    current_mode, current_sha = selection.mode, selection.checkout_sha
    failures = []
    if selected_sha.lower() != current_sha:
        failures.append("selected candidate is not the current PR head")
    ranks = {"minimal": 0, "test": 1, "full": 2}
    if ranks.get(mode, -1) < ranks[current_mode]:
        failures.append(f"current PR requires {current_mode} CI; cached mode is {mode}")
    return failures


def force_cancel_run(repository: str, run_id: str, token: str) -> None:
    """Ask GitHub to cancel this run, including its `always()` jobs."""
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repository}/actions/runs/{run_id}/force-cancel",
        method="POST",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=30):
        pass


# How long to wait for GitHub to cancel this job before failing closed.
CANCEL_GRACE_SECONDS = 120


def conclude_cancelled_attempt() -> int:
    """Turn this job into `cancelled` for a cancelled (e.g. superseded) attempt.

    `always()` still runs the summary after the run is cancelled, and the jobs
    it judges -- ci-mode included -- are cancelled with empty outputs. Judging
    them would report FAILURE on the head for a change nobody found faulty and
    leave a red required context beside the newer, healthy attempt; succeeding
    would be a false pass. The honest verdict is the attempt's own: cancelled.
    If GitHub does not cancel this job in time, fail closed.
    """
    print("::warning::CI summary: run attempt was cancelled; concluding cancelled")
    try:
        force_cancel_run(
            os.environ.get("GITHUB_REPOSITORY", ""),
            os.environ.get("GITHUB_RUN_ID", ""),
            os.environ.get("GITHUB_TOKEN", ""),
        )
        time.sleep(CANCEL_GRACE_SECONDS)
    except (OSError, ValueError) as error:
        print(f"::error::CI summary failed: cannot cancel the attempt: {error}")
        return 1
    print("::error::CI summary failed: the cancelled attempt was not cancelled")
    return 1


def cancelled_attempt_result(needs: JsonDocument) -> int | None:
    """Recognize selector cancellation before validating its missing outputs.

    A cancellation flag alone must never mask a failing required job, including
    reusable-workflow children visible only in the attempt's complete API list.
    """
    selector = needs.get("ci-mode", {})
    cancelled_selector = (
        isinstance(selector, dict)
        and selector.get("result") == "cancelled"
        and selector.get("outputs") == {}
        and not os.environ.get("CI_MODE", "")
    )
    if not cancelled_selector and os.environ.get("CI_ATTEMPT_CANCELLED") != "true":
        return None
    failures = [
        f"job {name!r} concluded {value.get('result')}"
        for name, value in needs.fields.items()
        if value.get("result") not in (*_AUDIT_OK, "cancelled")
    ]
    try:
        jobs = list_run_attempt_jobs(
            os.environ.get("GITHUB_REPOSITORY", ""),
            os.environ.get("GITHUB_RUN_ID", ""),
            os.environ.get("GITHUB_RUN_ATTEMPT", ""),
            os.environ.get("GITHUB_TOKEN", ""),
        )
        # Only cancelled jobs are expected in a cancelled attempt. Preserve all
        # other audit failures, including empty lists and unfinished jobs.
        failures += run_attempt_failures(
            [
                (
                    {**job, "conclusion": "skipped"}
                    if job.get("conclusion") == "cancelled"
                    else job
                )
                for job in jobs
            ]
        )
    except (OSError, ValueError) as error:
        failures.append(f"cannot list run attempt jobs: {error}")
    if failures:
        for failure in failures:
            print(f"::error::CI summary failed: {failure}")
        return 1
    return conclude_cancelled_attempt()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--adapter", type=Path, required=True)
    parser.add_argument("--full-contract", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    adapter_raw = args.adapter.read_bytes()
    full_raw = args.full_contract.read_bytes()
    adapter = JsonDocument(json.loads(adapter_raw))
    full = JsonDocument(json.loads(full_raw))
    needs = JsonDocument(json.loads(os.environ["CI_NEEDS_JSON"]))
    cancelled = cancelled_attempt_result(needs)
    if cancelled is not None:
        return cancelled
    inputs: Selection = {
        "mode": os.environ.get("CI_MODE", ""),
        "docs_only": os.environ.get("DOCS_ONLY", "") == "true",
        "event_name": os.environ.get("GITHUB_EVENT_NAME", ""),
        "author_permission": os.environ.get("AUTHOR_PERMISSION", "unknown"),
        "expected_sha": os.environ.get("EXPECTED_SHA", ""),
        "selected_sha": os.environ.get("SELECTED_SHA", ""),
        "trusted": os.environ.get("CI_TRUSTED", "") == "true",
        "skip_lint": os.environ.get("CI_SKIP_LINT", "") == "true",
    }
    infrastructure_jobs = []
    selector_permission = inputs["author_permission"]
    current_event = None
    current_failures = []
    if inputs["event_name"] == "pull_request":
        # A failed-job rerun can reuse ci-mode's outputs from an earlier
        # attempt. The merge decision must use permission at summary time.
        try:
            event = json.loads(
                Path(os.environ.get("GITHUB_EVENT_PATH", "")).read_text(
                    encoding="utf-8"
                )
            )
            current_event = (
                SELECTOR["lookup_current_pull_request"](
                    event,
                    os.environ.get("GITHUB_REPOSITORY", ""),
                    os.environ.get("GITHUB_TOKEN", ""),
                )
                if isinstance(event, dict)
                else None
            )
            permission = (
                SELECTOR["lookup_author_permission"](
                    current_event,
                    os.environ.get("GITHUB_REPOSITORY", ""),
                    os.environ.get("GITHUB_TOKEN", ""),
                )
                if current_event is not None
                else None
            )
        except (OSError, ValueError):
            permission = None
        inputs["author_permission"] = permission or "unknown"
    try:
        if inputs["event_name"] == "pull_request":
            aliases = SELECTOR["label_aliases_from_adapter"](
                {
                    "schema_version": adapter.get("schema_version"),
                    "label_aliases": adapter.get("label_aliases", {}),
                }
            )
            current_failures = current_selection_failures(
                current_event,
                mode=inputs["mode"],
                selected_sha=inputs["selected_sha"],
                permission=inputs["author_permission"],
                aliases=aliases,
            )
        failures = summary_failures(adapter, full, needs, **inputs)
        failures += current_failures
        try:
            attempt_jobs = settled_run_attempt_jobs(
                os.environ.get("GITHUB_REPOSITORY", ""),
                os.environ.get("GITHUB_RUN_ID", ""),
                os.environ.get("GITHUB_RUN_ATTEMPT", ""),
                os.environ.get("GITHUB_TOKEN", ""),
            )
            infrastructure_jobs = [
                job["name"]
                for job in attempt_jobs
                if starved_nightly_probe(job, inputs["event_name"])
            ]
            failures += run_attempt_failures(
                [
                    (
                        {**job, "conclusion": "skipped"}
                        if starved_nightly_probe(job, inputs["event_name"])
                        else job
                    )
                    for job in attempt_jobs
                ]
            )
        except (OSError, ValueError) as error:
            # Fail closed: an unauditable run is not a green run.
            failures.append(f"cannot list run attempt jobs: {error}")
        jobs = sorted(
            required_jobs(
                adapter,
                full,
                inputs["mode"],
                inputs["docs_only"],
                inputs["skip_lint"],
            )
        )
    except (ValueError, TypeError, KeyError) as error:
        failures, jobs = [f"invalid CI contract or selection: {error}"], []
    report = {
        "schema": SCHEMA,
        **inputs,
        "selector_author_permission": selector_permission,
        "current_pr_head": (
            current_event["pull_request"].get("head", {}).get("sha")
            if current_event is not None
            else None
        ),
        "current_pr_labels": (
            [label["name"] for label in current_event["pull_request"]["labels"]]
            if current_event is not None
            else None
        ),
        "adapter_sha256": hashlib.sha256(adapter_raw).hexdigest(),
        "manifest_sha256": hashlib.sha256(full_raw).hexdigest(),
        "required_jobs": jobs,
        "outcomes": {
            job: (
                needs.fields[job].get("result", "missing")
                if isinstance(needs.get(job), dict)
                else "missing"
            )
            for job in jobs
        },
        "failures": failures,
        "infrastructure_jobs": infrastructure_jobs,
        "success": not failures and not infrastructure_jobs,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    for failure in failures:
        print(f"::error::CI summary failed: {failure}")
    if infrastructure_jobs and not failures:
        print(
            "::warning::Nightly infrastructure incomplete: macos-15 probe had "
            "no runner and executed no steps; coverage is unvalidated. "
            "Rerun failed jobs when runner capacity recovers."
        )
        return conclude_cancelled_attempt()
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
