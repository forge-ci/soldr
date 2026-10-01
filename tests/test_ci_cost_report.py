"""Offline contract tests for the GitHub Actions cost report."""

import importlib.util
import unittest
from pathlib import Path
from types import SimpleNamespace

SCRIPT = Path(__file__).resolve().parents[1] / ".github/scripts/ci_cost_report.py"


def load_report():
    spec = importlib.util.spec_from_file_location("ci_cost_report", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(
    run_id,
    sha,
    *,
    event="push",
    attempts=1,
    workflow=10,
    created_at="2026-01-01T00:00:00Z",
    display_title=None,
    parent_run_id=None,
):
    return {
        "id": run_id,
        "head_sha": sha,
        "event": event,
        "workflow_id": workflow,
        "name": "CI",
        "path": ".github/workflows/ci.yml",
        "run_attempt": attempts,
        "status": "completed",
        "conclusion": "success",
        "html_url": f"https://example.test/runs/{run_id}",
        "created_at": created_at,
        "display_title": display_title
        or (
            f"ci-cost-parent-v1={parent_run_id}"
            if parent_run_id
            else f"CI full {sha}" if event == "workflow_dispatch" else "CI"
        ),
    }


def job(job_id, start, end, conclusion="success", labels=None):
    return {
        "id": job_id,
        "name": "build",
        "started_at": start,
        "completed_at": end,
        "conclusion": conclusion,
        "labels": labels or ["ubuntu-latest"],
    }


class FakeAPI:  # pylint: disable=too-few-public-methods
    def __init__(self, runs, jobs):
        self.runs, self.jobs, self.calls = runs, jobs, []

    def get(self, endpoint, params=None):
        self.calls.append((endpoint, params))
        page = int((params or {}).get("page", 1))
        if endpoint.endswith("/actions/runs"):
            return {
                "workflow_runs": self.runs.get(page, []),
                "total_count": sum(len(batch) for batch in self.runs.values()),
            }
        if "/attempts/" not in endpoint:
            run_id = int(endpoint.rsplit("/", 1)[1])
            return next(
                item
                for batch in self.runs.values()
                for item in batch
                if item["id"] == run_id
            )
        run_id = int(endpoint.split("/runs/")[1].split("/")[0])
        attempt = int(endpoint.split("/attempts/")[1].split("/")[0])
        return {
            "jobs": self.jobs.get((run_id, attempt, page), []),
            "total_count": sum(
                len(batch)
                for (rid, att, _), batch in self.jobs.items()
                if rid == run_id and att == attempt
            ),
        }


class CostReportTests(unittest.TestCase):
    def test_linked_push_is_explicit_and_includes_cancelled_attempts_and_child(self):
        m = load_report()
        sha = "a" * 40
        pr = run(1, sha, event="pull_request")
        push = run(2, sha, attempts=2, created_at="2026-01-02T00:00:00Z")
        association = {
            "number": 7,
            "head": {"ref": "topic", "sha": sha, "repo": {"id": 12}},
        }
        for item in [pr, push]:
            item.update(head_branch="topic", pull_requests=[association])
        api = FakeAPI(
            {1: [pr, push, run(3, "other", event="workflow_run", parent_run_id=2)]},
            {
                (1, 1, 1): [job(11, "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z")],
                (2, 1, 1): [
                    job(21, "2026-01-01T00:00:00Z", "2026-01-01T00:02:00Z", "cancelled")
                ],
                (2, 2, 1): [job(22, "2026-01-01T00:00:00Z", "2026-01-01T00:03:00Z")],
                (3, 1, 1): [job(31, "2026-01-01T00:00:00Z", "2026-01-01T00:04:00Z")],
            },
        )
        receipt = {
            "schema": "fleet-ci-linked-event/v1",
            "repository": "o/r",
            "anchor_run_id": 1,
            "linked_run_id": 2,
            "relation": "branch-push-to-pr-synchronize",
            "provenance": "operator-attested",
            "pull_request": 7,
            "head_repository_id": 12,
            "head_ref": "topic",
            "before_sha": "b" * 40,
            "after_sha": sha,
        }
        report = m.collect_sha(
            api,
            "o/r",
            sha,
            "pull_request",
            anchor_run_id=1,
            linked_event_receipts=[receipt],
        )
        self.assertEqual(report["runner_minutes"], 10)
        self.assertEqual(
            report["runner_minutes_by_event"],
            {"pull_request": 1, "push": 5, "workflow_run": 4},
        )
        self.assertFalse(report["linked_events_acceptance_evidence"])
        self.assertEqual(
            report["direct_root_attribution"], "120-second heuristic; not causal proof"
        )
        for field, value in [
            ("head_ref", "foreign"),
            ("after_sha", "c" * 40),
            ("relation", "branch-push-to-pr-opened"),
        ]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                m.collect_sha(
                    api,
                    "o/r",
                    sha,
                    "pull_request",
                    anchor_run_id=1,
                    linked_event_receipts=[{**receipt, field: value}],
                )
        captured = {
            **receipt,
            "provenance": "captured-event-payloads",
            "pr_payload": {
                "action": "synchronize",
                "number": 7,
                "before": "b" * 40,
                "after": sha,
                "repository": {"full_name": "o/r"},
                "pull_request": association,
            },
            "push_payload": {
                "before": "b" * 40,
                "after": sha,
                "created": False,
                "deleted": False,
                "ref": "refs/heads/topic",
                "repository": {"id": 12},
            },
        }
        self.assertTrue(
            m.collect_sha(
                api,
                "o/r",
                sha,
                "pull_request",
                anchor_run_id=1,
                linked_event_receipts=[captured],
            )["linked_events_acceptance_evidence"]
        )
        for action in ["opened", "labeled", "reopened"]:
            with self.subTest(action=action), self.assertRaises(ValueError):
                m.collect_sha(
                    api,
                    "o/r",
                    sha,
                    "pull_request",
                    anchor_run_id=1,
                    linked_event_receipts=[
                        {
                            **captured,
                            "pr_payload": {**captured["pr_payload"], "action": action},
                        }
                    ],
                )
        for field, value in [
            ("before_sha", "0" * 40),
            ("anchor_run_id", 9),
            ("head_repository_id", 99),
            ("repository", "other/repo"),
            ("provenance", "unspecified"),
        ]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                m.collect_sha(
                    api,
                    "o/r",
                    sha,
                    "pull_request",
                    anchor_run_id=1,
                    linked_event_receipts=[{**receipt, field: value}],
                )
        for field, value in [
            ("created", True),
            ("deleted", True),
            ("before", "c" * 40),
            ("ref", "refs/heads/other"),
        ]:
            with self.subTest(push_field=field), self.assertRaises(ValueError):
                m.collect_sha(
                    api,
                    "o/r",
                    sha,
                    "pull_request",
                    anchor_run_id=1,
                    linked_event_receipts=[
                        {
                            **captured,
                            "push_payload": {**captured["push_payload"], field: value},
                        }
                    ],
                )
        with self.assertRaises(ValueError):
            m.collect_sha(
                api,
                "o/r",
                sha,
                "pull_request",
                anchor_run_id=1,
                linked_event_receipts=[receipt, receipt],
            )
        # An explicitly linked run's jobs still paginate, including a cancelled
        # first attempt. Repeated discovery must not charge any run twice.
        api.jobs[2, 1, 1] = [
            job(100 + i, "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z", "cancelled")
            for i in range(100)
        ]
        api.jobs[2, 1, 2] = [job(200, "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z")]
        report = m.collect_sha(
            api,
            "o/r",
            sha,
            "pull_request",
            anchor_run_id=1,
            linked_event_receipts=[receipt],
        )
        self.assertEqual(report["runner_minutes"], 109)
        self.assertEqual(len(report["included_runs"]), 3)
        self.assertTrue(
            any(
                "runs/2/attempts/1/jobs" in endpoint and params["page"] == 2
                for endpoint, params in api.calls
            )
        )
        push["status"] = "in_progress"
        with self.assertRaisesRegex(ValueError, "not completed"):
            m.collect_sha(
                api,
                "o/r",
                sha,
                "pull_request",
                anchor_run_id=1,
                linked_event_receipts=[receipt],
            )

    def test_merge_queue_cost_is_a_disclosed_separate_event(self):
        m = load_report()
        api = FakeAPI(
            {1: [run(1, "a", event="merge_group"), run(2, "a", event="push")]},
            {
                (1, 1, 1): [job(11, "2026-01-01T00:00:00Z", "2026-01-01T00:02:00Z")],
                (2, 1, 1): [job(21, "2026-01-01T00:00:00Z", "2026-01-01T00:10:00Z")],
            },
        )
        report = m.collect_sha(api, "o/r", "a", "merge_group")
        self.assertEqual(report["event"], "merge_group")
        self.assertEqual(report["runner_minutes"], 2)
        self.assertEqual([item["id"] for item in report["included_runs"]], [1])

    def test_skipped_jobs_do_not_allocate_runner_time(self):
        m = load_report()
        api = FakeAPI(
            {1: [run(1, "a")]},
            {
                (1, 1, 1): [
                    job(
                        11,
                        "2026-01-01T00:00:00Z",
                        "2026-01-01T00:05:00Z",
                        "skipped",
                        labels=["macos-15"],
                    ),
                    job(12, "2026-01-01T00:00:00Z", "2026-01-01T00:02:00Z"),
                ]
            },
        )
        report = m.collect_sha(api, "o/r", "a", "push", weights={"ubuntu-latest": 4})
        self.assertEqual(report["runner_minutes"], 2)
        self.assertEqual(report["weighted_runner_minutes_known"], 8)
        self.assertEqual(report["unknown_weight_job_ids"], [])
        self.assertEqual(report["jobs"][0]["seconds"], 0)
        self.assertEqual(report["jobs"][0]["conclusion"], "skipped")

    def test_dispatch_matches_workflow_path_even_when_name_is_run_title(self):
        m = load_report()
        candidate = "a" * 40
        anchor = run(1, candidate, event="workflow_dispatch")
        anchor["name"] = anchor["display_title"]
        api = FakeAPI({1: [anchor]}, {})
        report = m.collect_sha(
            api, "o/r", candidate, "workflow_dispatch", anchor_run_id=1
        )
        self.assertEqual(report["included_runs"][0]["id"], 1)
        anchor["path"] = ".github/workflows/spoof.yml"
        with self.assertRaisesRegex(ValueError, "CI full"):
            m.collect_sha(api, "o/r", candidate, "workflow_dispatch", anchor_run_id=1)

    def test_workflow_run_descendants_are_counted_outside_anchor_window(self):
        m = load_report()
        api = FakeAPI(
            {
                1: [
                    run(1, "a"),
                    run(
                        2,
                        "b",
                        event="workflow_run",
                        parent_run_id=1,
                        created_at="2026-01-01T01:00:00Z",
                    ),
                    run(
                        3,
                        "c",
                        event="workflow_run",
                        parent_run_id=2,
                        created_at="2026-01-01T02:00:00Z",
                    ),
                    run(
                        4,
                        "d",
                        event="workflow_run",
                        parent_run_id=99,
                        created_at="2026-01-01T03:00:00Z",
                    ),
                ]
            },
            {
                (i, 1, 1): [job(i * 10, "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z")]
                for i in range(1, 5)
            },
        )
        report = m.collect_sha(api, "o/r", "a", "push", anchor_run_id=1)
        self.assertEqual(report["runner_minutes"], 3)
        self.assertEqual([r["id"] for r in report["included_runs"]], [1, 2, 3])
        self.assertEqual(report["included_runs"][2]["trigger_run_id"], 2)
        self.assertIn("parent", report["excluded_runs"][0]["reason"])

    def test_unattributed_workflow_run_refuses_a_budget_claim(self):
        m = load_report()
        api = FakeAPI({1: [run(1, "a"), run(2, "a", event="workflow_run")]}, {})
        with self.assertRaisesRegex(ValueError, "parent receipt"):
            m.collect_sha(api, "o/r", "a", "push")

    def test_parent_receipt_is_strict_and_versioned(self):
        m = load_report()
        self.assertEqual(m.parse_parent_receipt("ci-cost-parent-v1=123", 2), 123)
        for title in (
            None,
            "Cache Budget",
            "ci-cost-parent-v1=0",
            "ci-cost-parent-v2=123",
        ):
            with self.assertRaisesRegex(ValueError, "parent receipt"):
                m.parse_parent_receipt(title, 2)
        workflow = SCRIPT.parents[1] / "workflows" / "cache-budget.yml"
        text = workflow.read_text()
        self.assertIn("run-name:", text)
        self.assertIn(
            "format('ci-cost-parent-v1={0}', github.event.workflow_run.id)", text
        )

    def test_selected_pending_descendant_prevents_partial_total(self):
        m = load_report()
        child = run(2, "different", event="workflow_run", parent_run_id=1)
        child["status"] = "in_progress"
        api = FakeAPI({1: [run(1, "a"), child]}, {})
        with self.assertRaisesRegex(ValueError, "not completed"):
            m.collect_sha(api, "o/r", "a", "push")

    def test_search_limit_and_truncated_inventory_refuse_cost_reports(self):
        m = load_report()

        for count, message in (
            (None, "invalid total_count"),
            (-1, "invalid total_count"),
            (True, "invalid total_count"),
            (1.0, "invalid total_count"),
            (1000, "1000-run limit"),
            (1001, "1000-run limit"),
            (2, "incomplete inventory"),
        ):
            with self.assertRaisesRegex(ValueError, message):
                m.collect_sha(
                    SimpleNamespace(
                        get=lambda endpoint, params=None, count=count: {
                            "total_count": count,
                            "workflow_runs": [run(1, "a")],
                        }
                    ),
                    "o/r",
                    "a",
                    "push",
                )

    def test_repeated_label_runs_on_one_sha_need_an_anchor(self):
        m = load_report()
        api = FakeAPI(
            {
                1: [
                    run(1, "a", event="pull_request"),
                    run(
                        2,
                        "a",
                        event="pull_request",
                        created_at="2026-01-01T00:10:00Z",
                    ),
                ]
            },
            {
                (1, 1, 1): [job(11, "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z")],
            },
        )
        with self.assertRaisesRegex(ValueError, "anchor"):
            m.collect_sha(api, "o/r", "a", "pull_request")
        report = m.collect_sha(api, "o/r", "a", "pull_request", anchor_run_id=1)
        self.assertEqual([run["id"] for run in report["included_runs"]], [1])
        self.assertEqual(report["excluded_runs"][0]["reason"], "outside-anchor-window")

    def test_one_second_github_clock_skew_is_recorded_not_lost(self):
        m = load_report()
        api = FakeAPI(
            {1: [run(1, "a")], 2: []},
            {
                (1, 1, 1): [job(11, "2026-01-01T00:01:00Z", "2026-01-01T00:00:59Z")],
                (1, 1, 2): [],
            },
        )
        report = m.collect_sha(api, "o/r", "a", "push")
        self.assertEqual(report["runner_minutes"], 0)
        self.assertEqual(report["jobs"][0]["timestamp_skew_seconds"], 1)
        with self.assertRaises(ValueError):
            m.seconds("2026-01-01T00:01:00Z", "2026-01-01T00:00:55Z")

    def test_paginates_all_workflows_jobs_and_attempts_and_explains_exclusions(self):
        m = load_report()
        api = FakeAPI(
            {
                1: [run(1, "a", attempts=2), run(2, "a", event="workflow_dispatch")]
                + [run(n, "other") for n in range(100, 198)],
                2: [run(3, "b"), run(4, "a", workflow=20)],
                3: [],
            },
            {
                (1, 1, 1): [
                    job(11, "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z", "failure")
                ]
                + [job(n, None, None) for n in range(100, 199)],
                (1, 1, 2): [],
                (1, 2, 1): [job(12, "2026-01-01T00:00:00Z", "2026-01-01T00:02:00Z")],
                (1, 2, 2): [],
                (4, 1, 1): [
                    job(41, "2026-01-01T00:00:00Z", "2026-01-01T00:03:00Z", "cancelled")
                ],
                (4, 1, 2): [],
            },
        )
        report = m.collect_sha(api, "o/r", "a", "push")
        self.assertEqual(report["runner_minutes"], 6)
        self.assertEqual([r["id"] for r in report["included_runs"]], [1, 4])
        self.assertEqual(
            [(r["id"], r["reason"]) for r in report["excluded_runs"]],
            [(2, "event=workflow_dispatch")],
        )
        self.assertEqual(
            len(
                [
                    c
                    for c in api.calls
                    if c[0].endswith("/actions/runs")
                    and c[1].get("event") != "workflow_run"
                ]
            ),
            2,
        )
        self.assertTrue(
            any("/attempts/1/jobs" in c[0] and c[1]["page"] == 2 for c in api.calls)
        )
        self.assertTrue(any("/attempts/1/jobs" in c[0] for c in api.calls))
        self.assertTrue(any("/attempts/2/jobs" in c[0] for c in api.calls))

    def test_explicit_cohorts_median_range_ratio_and_threshold(self):
        m = load_report()
        baseline = [
            {"sha": "a", "runner_minutes": 100},
            {"sha": "b", "runner_minutes": 120},
        ]
        post = [{"sha": "c", "runner_minutes": 10}, {"sha": "d", "runner_minutes": 15}]
        result = m.compare(baseline, post)
        self.assertEqual(result["baseline"]["median_runner_minutes"], 110)
        self.assertEqual(result["baseline"]["range_runner_minutes"], [100, 120])
        self.assertEqual(result["post"]["median_runner_minutes"], 12.5)
        self.assertEqual(result["median_ratio"], 12.5 / 110)
        self.assertTrue(result["meets_12_5_percent_target"])
        with self.assertRaises(ValueError):
            m.compare(baseline, [post[0], {"sha": "a", "runner_minutes": 1}])

    def test_same_sha_label_waves_can_be_compared_by_distinct_anchors(self):
        m = load_report()
        result = m.compare(
            [{"sha": "a", "anchor_run_id": 1, "runner_minutes": 100}],
            [{"sha": "a", "anchor_run_id": 2, "runner_minutes": 10}],
        )
        self.assertEqual(result["median_ratio"], 0.1)
        self.assertEqual(len(result["workflow_ids_by_sample"]), 2)

    def test_explicit_full_dispatch_can_be_a_push_cost_control(self):
        m = load_report()
        api = FakeAPI(
            {1: [run(1, "a" * 40, event="workflow_dispatch")], 2: []},
            {(1, 1, 1): [job(11, "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z")]},
        )
        baseline = m.collect_sha(
            api, "o/r", "a" * 40, "workflow_dispatch", anchor_run_id=1
        )
        post = {
            "sha": "a" * 40,
            "anchor_run_id": 2,
            "event": "push",
            "runner_minutes": 5,
        }
        with self.assertRaisesRegex(ValueError, "same event"):
            m.compare([baseline], [post])
        result = m.compare([baseline], [post], allow_cross_event=True)
        self.assertEqual(result["median_ratio"], 5 / 60)
        self.assertTrue(result["cross_event_control"])

    def test_dispatch_cost_uses_candidate_title_not_workflow_ref_sha(self):
        m = load_report()
        api = FakeAPI(
            {
                1: [
                    run(
                        7,
                        "b" * 40,
                        event="workflow_dispatch",
                        display_title=f"CI full {'a' * 40}",
                    )
                ]
            },
            {(7, 1, 1): [job(71, "2026-01-01T00:00:00Z", "2026-01-01T00:02:00Z")]},
        )
        report = m.collect_sha(
            api, "o/r", "a" * 40, "workflow_dispatch", anchor_run_id=7
        )
        self.assertEqual(report["runner_minutes"], 2)
        self.assertEqual(report["included_runs"][0]["id"], 7)
        self.assertEqual(report["included_runs"][0]["head_sha"], "b" * 40)
        with self.assertRaisesRegex(ValueError, "anchor"):
            m.collect_sha(api, "o/r", "a" * 40, "workflow_dispatch")
        with self.assertRaisesRegex(ValueError, "candidate"):
            m.collect_sha(api, "o/r", "c" * 40, "workflow_dispatch", anchor_run_id=7)

    def test_weighting_unknown_labels_are_disclosed(self):
        m = load_report()
        api = FakeAPI(
            {1: [run(1, "a")], 2: []},
            {
                (1, 1, 1): [
                    job(
                        11,
                        "2026-01-01T00:00:00Z",
                        "2026-01-01T00:02:00Z",
                        labels=["ubuntu-latest"],
                    ),
                    job(
                        12,
                        "2026-01-01T00:00:00Z",
                        "2026-01-01T00:01:00Z",
                        labels=["self-hosted", "large"],
                    ),
                ],
                (1, 1, 2): [],
            },
        )
        report = m.collect_sha(api, "o/r", "a", "push", weights={"ubuntu-latest": 4})
        self.assertEqual(report["weighted_runner_minutes_known"], 8)
        self.assertEqual(report["unknown_weight_job_ids"], [12])


if __name__ == "__main__":
    unittest.main()
