import dataclasses
import json
import tempfile
import unittest
from pathlib import Path

from _script_loader import load_script_module

SCRIPT = Path(__file__).with_name("target_run_summary.py")
target_run_summary = load_script_module(SCRIPT, "target_run_summary")


class TargetRunSummaryTests(unittest.TestCase):
    def write_list(self, path: Path, *, discovered: object, ignored: int = 0) -> None:
        testcases = {
            f"test-{index}": {"ignored": index < ignored}
            for index in range(discovered if isinstance(discovered, int) else 0)
        }
        path.write_text(
            json.dumps(
                {
                    "test-count": discovered,
                    "rust-suites": {"suite": {"testcases": testcases}},
                }
            ),
            encoding="utf-8",
        )

    def test_setup_summary_exists_before_nextest_runs(self) -> None:
        summary = target_run_summary.build_summary("aarch64-apple-darwin")
        self.assertEqual(summary.phase, "setup")
        self.assertIsNone(summary.discovered)
        self.assertIsNone(summary.executed)

    def test_list_and_junit_counts_are_combined(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            temp = Path(raw_temp)
            test_list = temp / "list.json"
            junit = temp / "junit.xml"
            test_list.write_text(
                json.dumps(
                    {
                        "test-count": 4,
                        "rust-suites": {
                            "suite": {
                                "testcases": {
                                    "passes": {"ignored": False},
                                    "fails": {"ignored": False},
                                    "ignored": {"ignored": True},
                                    "skipped": {"ignored": False},
                                }
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )
            junit.write_text(
                '<testsuites tests="3" failures="1" errors="0" skipped="1" />',
                encoding="utf-8",
            )

            summary = target_run_summary.build_summary(
                "x86_64-unknown-linux-musl", test_list, junit
            )

        self.assertEqual(
            dataclasses.asdict(summary),
            {
                "schema_version": 1,
                "target": "x86_64-unknown-linux-musl",
                "partition": None,
                "phase": "completed",
                "discovered": 4,
                "ignored": 1,
                "executed": 3,
                "passed": 1,
                "failed": 1,
                "skipped": 1,
            },
        )

    def test_filtered_list_counts_only_matching_tests(self) -> None:
        """A positive ownership filter leaves mismatch rows in list JSON.

        Nextest's top-level test-count still describes every testcase in each
        selected suite. Coverage reconciliation must use only rows whose
        filter-match status is matches, or a narrow exact-test allowlist looks
        like silent under-execution after every selected test passes.
        """
        with tempfile.TemporaryDirectory() as raw_temp:
            temp = Path(raw_temp)
            test_list = temp / "list.json"
            junit = temp / "junit.xml"
            test_list.write_text(
                json.dumps(
                    {
                        "test-count": 4,
                        "rust-suites": {
                            "suite": {
                                "testcases": {
                                    "selected": {
                                        "ignored": False,
                                        "filter-match": {"status": "matches"},
                                    },
                                    "selected-ignored": {
                                        "ignored": True,
                                        "filter-match": {"status": "matches"},
                                    },
                                    "portable-one": {
                                        "ignored": False,
                                        "filter-match": {"status": "mismatch"},
                                    },
                                    "portable-two": {
                                        "ignored": False,
                                        "filter-match": {"status": "mismatch"},
                                    },
                                }
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )
            junit.write_text('<testsuite tests="1" />', encoding="utf-8")

            summary = target_run_summary.build_summary(
                "aarch64-apple-darwin", test_list, junit
            )

        self.assertEqual(summary.discovered, 2)
        self.assertEqual(summary.ignored, 1)
        self.assertEqual(summary.executed, 1)

    def test_junit_suite_children_are_aggregated(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            junit = Path(raw_temp) / "junit.xml"
            junit.write_text(
                "<testsuites>"
                '<testsuite tests="2" failures="0" errors="0" skipped="0" />'
                '<testsuite tests="3" failures="1" errors="1" skipped="0" />'
                "</testsuites>",
                encoding="utf-8",
            )
            counts = target_run_summary.read_junit(junit)
        self.assertEqual(
            counts,
            target_run_summary.JunitCounts(executed=5, passed=3, failed=2, skipped=0),
        )

    def test_completed_summary_requires_junit_when_requested(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            missing = Path(raw_temp) / "missing.xml"
            with self.assertRaisesRegex(ValueError, "required JUnit report is missing"):
                target_run_summary.build_summary(
                    "x86_64-pc-windows-msvc", junit=missing, require_junit=True
                )

    def test_supplied_list_path_must_exist(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            missing = Path(raw_temp) / "missing-list.json"
            with self.assertRaisesRegex(ValueError, "nextest list JSON is missing"):
                target_run_summary.read_test_list(missing)

    def test_invalid_junit_shapes_and_totals_are_rejected(self) -> None:
        cases = {
            "wrong-root": '<not-junit tests="2" />',
            "empty-root": "<testsuites />",
            "negative": '<testsuite tests="2" failures="-1" />',
            "over-accounted": '<testsuite tests="2" failures="2" skipped="1" />',
        }
        with tempfile.TemporaryDirectory() as raw_temp:
            path = Path(raw_temp) / "junit.xml"
            for name, xml in cases.items():
                with self.subTest(name=name):
                    path.write_text(xml, encoding="utf-8")
                    with self.assertRaises(ValueError):
                        target_run_summary.read_junit(path)

    def test_discovered_executed_and_ignored_must_reconcile(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            temp = Path(raw_temp)
            test_list = temp / "list.json"
            junit = temp / "junit.xml"
            self.write_list(test_list, discovered=8, ignored=1)
            junit.write_text('<testsuite tests="1" />', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "coverage counts disagree"):
                target_run_summary.build_summary(
                    "aarch64-unknown-linux-gnu", test_list, junit
                )

    def test_zero_discovered_tests_is_rejected_before_execution(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            test_list = Path(raw_temp) / "list.json"
            self.write_list(test_list, discovered=0, ignored=0)
            with self.assertRaisesRegex(ValueError, "selected zero tests"):
                target_run_summary.build_summary("x86_64-unknown-linux-musl", test_list)

    def test_zero_executed_tests_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            temp = Path(raw_temp)
            test_list = temp / "list.json"
            junit = temp / "junit.xml"
            self.write_list(test_list, discovered=1, ignored=0)
            junit.write_text('<testsuite tests="0" />', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "executed zero tests"):
                target_run_summary.build_summary(
                    "x86_64-unknown-linux-musl", test_list, junit
                )

    def test_early_stop_with_failures_is_not_a_coverage_hole(self) -> None:
        """soldr#2724: `--max-fail 3:immediate` leaves tests unexecuted.

        The lane stops on its third failure by design, so under-execution
        *with* failures is the bounded stop, not a partition quietly
        skipping tests. Before this, the summary raised here -- so a lane
        that failed the way it was configured to fail reported a Python
        ValueError about counts instead of the tests that failed, and wrote
        no summary artifact at all.
        """
        with tempfile.TemporaryDirectory() as raw_temp:
            temp = Path(raw_temp)
            test_list = temp / "list.json"
            junit = temp / "junit.xml"
            self.write_list(test_list, discovered=8, ignored=1)
            junit.write_text('<testsuite tests="4" failures="2" />', encoding="utf-8")
            summary = target_run_summary.build_summary(
                "x86_64-pc-windows-msvc", test_list, junit
            )
        self.assertEqual(summary.phase, "completed")
        self.assertEqual(summary.discovered, 8)
        self.assertEqual(summary.executed, 4)
        self.assertEqual(summary.failed, 2)

    def test_over_execution_still_raises_even_with_failures(self) -> None:
        """An early stop can only ever run *fewer* tests.

        Running more than were discovered is unexplained by `--max-fail`, so
        the presence of failures must not excuse it -- otherwise the
        soldr#2724 allowance would blank the guard for any failing run.
        """
        with tempfile.TemporaryDirectory() as raw_temp:
            temp = Path(raw_temp)
            test_list = temp / "list.json"
            junit = temp / "junit.xml"
            self.write_list(test_list, discovered=8, ignored=1)
            junit.write_text('<testsuite tests="10" failures="1" />', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "coverage counts disagree"):
                target_run_summary.build_summary(
                    "x86_64-pc-windows-msvc", test_list, junit
                )

    def test_hash_shard_allows_a_bounded_subset(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            temp = Path(raw_temp)
            test_list = temp / "list.json"
            junit = temp / "junit.xml"
            self.write_list(test_list, discovered=9, ignored=1)
            junit.write_text('<testsuite tests="3" />', encoding="utf-8")
            summary = target_run_summary.build_summary(
                "x86_64-pc-windows-gnu",
                test_list,
                junit,
                partition="hash:1/3",
            )
        self.assertEqual(summary.partition, "hash:1/3")
        self.assertEqual(summary.executed, 3)

    def test_boolean_discovered_count_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            test_list = Path(raw_temp) / "list.json"
            self.write_list(test_list, discovered=True)
            with self.assertRaisesRegex(ValueError, "integer test-count"):
                target_run_summary.read_test_list(test_list)


if __name__ == "__main__":
    unittest.main()
