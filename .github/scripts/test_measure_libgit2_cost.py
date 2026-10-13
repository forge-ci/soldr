"""Unit tests for `measure_libgit2_cost.py` (soldr Phase 5 step 1, #3046).

Mirrors `test_analyze_compile_journal.py`'s idioms -- `load_sibling_script`
to import the script under test, one JSON object per journal line, a
`tempfile.TemporaryDirectory()` per case -- with two deliberate departures:

* It reaches the analyzer through `measure_libgit2_cost`, not by loading a
  second copy. The script registers its own `analyze_compile_journal` in
  `sys.modules` as part of import, so a separate `load_sibling_script` call
  here would leave the tests building `Record`s from one module object while
  `measure()` filtered `Record`s from another.
* It funnels every journal-writing case through `_summarize` instead of
  repeating an open-coded write-then-measure block. `tests/.pylintrc` keeps
  `duplicate-code` (R0801) enabled on purpose, and four identical consecutive
  lines shared with a sibling test file in this directory is all it takes to
  fail the Lint job.

The regression this file exists to protect is `test_attributes_only_the_
package_root_cwd`: an args-based "does this crate mention libgit2 anywhere"
predicate over-attributes (438 records against a correct 416 on real data)
because `git2`'s own compiles reference `libgit2-sys` in `-L native=...` and
`--extern` flags without actually *being* libgit2-sys work. Attribution must
be `cwd`-only, exactly like `analyze_compile_journal`'s first/third-party
split.
"""

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any

from _script_loader import load_sibling_script

measure_libgit2_cost = load_sibling_script("measure_libgit2_cost")
analyze_compile_journal = measure_libgit2_cost.analyze_compile_journal

BASE_TS_PREFIX = "2026-09-17T18:20:"

REGISTRY_PREFIX = "/home/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/"
PKG_DIR_NAME = "libgit2-sys-0.18.8+1.9.7"
PKG_CWD = REGISTRY_PREFIX + PKG_DIR_NAME
GIT2_CWD = REGISTRY_PREFIX + "git2-0.21.0"
LZMA_CWD = REGISTRY_PREFIX + "lzma-sys-0.1.20"

RUSTC_BIN = (
    "/home/runner/.rustup/toolchains/nightly-2026-05-28-x86_64-unknown-linux-gnu"
    "/bin/rustc"
)

# A libgit2-sys object built by the Dylint half of the `ci-test` DAG. Used to
# pin `by_tree`, which reads `analyze_compile_journal`'s tree classification
# of the `-o` parent.
DYLINT_TESTS_OUT = (
    "/home/runner/work/soldr/soldr/target/dylint/tests/debug/build/"
    "libgit2-sys-4f1a2b3c4d5e6f70/out"
)

# The `-L native=` / `--extern` flags a downstream crate carries when it links
# libgit2. These are exactly what an args-based predicate over-attributes on.
LIBGIT2_LINK_ARGS = [
    "-L",
    f"native={DYLINT_TESTS_OUT}",
    "--extern",
    "libgit2_sys=/home/runner/work/soldr/soldr/target/dylint/tests/debug/deps/"
    "liblibgit2_sys-abc123.rlib",
]


def _seed(root: Path, records: list[dict]) -> str:
    """Write `records` as one journal under `root`; return `root` as a str."""
    journal = root / "compile_journal.jsonl"
    journal.write_text(
        "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
    )
    return str(root)


def _summarize(records: list[dict], **kwargs: object) -> Any:
    """Measure `records` from a throwaway journal root."""
    with tempfile.TemporaryDirectory() as raw:
        return measure_libgit2_cost.measure([_seed(Path(raw), records)], **kwargs)


def _cc_record(
    ts: str | None,
    outcome: str,
    latency_ns: int,
    cwd: str,
    *,
    miss_reason: str | None = None,
    context_key: str | None = None,
    args: list[str] | None = None,
) -> dict:
    record: dict = {
        "outcome": outcome,
        "latency_ns": latency_ns,
        "compiler": "/usr/bin/cc",
        "args": args if args is not None else ["unit.c", "-o", "/tmp/out/unit.o"],
        "cwd": cwd,
        "exit_code": 0,
    }
    if ts is not None:
        record["ts"] = ts
    if miss_reason is not None:
        record["miss_reason"] = miss_reason
    if context_key is not None:
        record["context_key"] = context_key
    return record


def _rustc_record(
    ts: str | None,
    outcome: str,
    latency_ns: int,
    cwd: str,
    crate_name: str,
    *,
    miss_reason: str | None = None,
    extra_args: list[str] | None = None,
) -> dict:
    record: dict = {
        "outcome": outcome,
        "latency_ns": latency_ns,
        "compiler": RUSTC_BIN,
        "args": [
            "--crate-name",
            crate_name,
            "--crate-type",
            "lib",
        ]
        + (extra_args or []),
        "cwd": cwd,
        "exit_code": 0,
    }
    if ts is not None:
        record["ts"] = ts
    if miss_reason is not None:
        record["miss_reason"] = miss_reason
    return record


class Libgit2PredicateTests(unittest.TestCase):
    """Direct coverage of `is_libgit2_record` / `LIBGIT2_PKG_RE`."""

    def test_matches_package_root_cwd(self) -> None:
        record = analyze_compile_journal.record_from_json(
            _cc_record(None, "miss", 1, PKG_CWD), "src"
        )
        assert record is not None
        self.assertTrue(measure_libgit2_cost.is_libgit2_record(record))

    def test_does_not_match_via_args_alone(self) -> None:
        record = analyze_compile_journal.record_from_json(
            _rustc_record(
                None, "miss", 1, GIT2_CWD, "git2", extra_args=LIBGIT2_LINK_ARGS
            ),
            "src",
        )
        assert record is not None
        self.assertFalse(measure_libgit2_cost.is_libgit2_record(record))

    def test_does_not_match_lookalike_package(self) -> None:
        record = analyze_compile_journal.record_from_json(
            _cc_record(None, "miss", 1, LZMA_CWD), "src"
        )
        assert record is not None
        self.assertFalse(measure_libgit2_cost.is_libgit2_record(record))


class UnionSecondsHelperTests(unittest.TestCase):
    def test_merges_overlapping_and_leaves_disjoint_separate(self) -> None:
        # [0, 10) and [5, 20) overlap -> merge to [0, 20) width 20;
        # [30, 40) is disjoint -> +10. Total union width 30 (nanoseconds in,
        # seconds out per the documented ns->s contract).
        result = measure_libgit2_cost.union_seconds(
            [
                analyze_compile_journal.Interval(0, 10),
                analyze_compile_journal.Interval(5, 20),
                analyze_compile_journal.Interval(30, 40),
            ]
        )
        self.assertAlmostEqual(result, 30.0 / 1e9)

    def test_empty_input_is_zero(self) -> None:
        self.assertEqual(measure_libgit2_cost.union_seconds([]), 0.0)


class AttributionRegressionTests(unittest.TestCase):
    """The regression test that matters: attribution is cwd-only."""

    def test_attributes_only_the_package_root_cwd(self) -> None:
        summary = _summarize(
            [
                _cc_record(
                    f"{BASE_TS_PREFIX}01.000Z",
                    "miss",
                    250_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                    context_key="ctx-a",
                ),
                _rustc_record(
                    f"{BASE_TS_PREFIX}02.000Z",
                    "miss",
                    500_000_000,
                    GIT2_CWD,
                    "git2",
                    miss_reason="context_not_found",
                    extra_args=LIBGIT2_LINK_ARGS,
                ),
                _cc_record(
                    f"{BASE_TS_PREFIX}03.000Z",
                    "miss",
                    100_000_000,
                    LZMA_CWD,
                    miss_reason="context_not_found",
                    context_key="ctx-c",
                ),
            ]
        )
        self.assertEqual(summary.attributed_records, 1)
        self.assertEqual(summary.package_dirs, [PKG_DIR_NAME])


class BucketingTests(unittest.TestCase):
    def test_cc_and_rustc_are_bucketed_separately(self) -> None:
        summary = _summarize(
            [
                _cc_record(
                    f"{BASE_TS_PREFIX}01.000Z",
                    "miss",
                    250_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                ),
                _cc_record(f"{BASE_TS_PREFIX}02.000Z", "hit", 100_000_000, PKG_CWD),
                _rustc_record(
                    f"{BASE_TS_PREFIX}03.000Z",
                    "miss",
                    500_000_000,
                    PKG_CWD,
                    "libgit2_sys",
                    miss_reason="context_not_found",
                ),
                _rustc_record(
                    f"{BASE_TS_PREFIX}04.000Z",
                    "hit",
                    50_000_000,
                    PKG_CWD,
                    "build_script_build",
                ),
            ]
        )

        by_kind = summary.by_kind
        self.assertEqual(by_kind.cc.miss.count, 1)
        self.assertEqual(by_kind.cc.hit.count, 1)
        self.assertEqual(by_kind.rustc.miss.count, 1)
        self.assertEqual(by_kind.rustc.hit.count, 1)

        self.assertAlmostEqual(
            by_kind.cc.miss.wall_seconds, 250_000_000 / 1e9, places=3
        )
        self.assertAlmostEqual(by_kind.cc.hit.wall_seconds, 100_000_000 / 1e9, places=3)
        self.assertAlmostEqual(
            by_kind.rustc.miss.wall_seconds, 500_000_000 / 1e9, places=3
        )
        self.assertAlmostEqual(
            by_kind.rustc.hit.wall_seconds, 50_000_000 / 1e9, places=3
        )


class WallClockTests(unittest.TestCase):
    def test_union_wall_clock_merges_overlapping_compiles(self) -> None:
        ts = f"{BASE_TS_PREFIX}05.000Z"
        # Distinct `context_key`s are load-bearing, not decoration: `measure`
        # reads journals with `dedupe=True`, which drops byte-identical
        # lines, so two perfectly-overlapping records must still differ as
        # text or the second one never reaches the rollup at all.
        summary = _summarize(
            [
                _cc_record(
                    ts,
                    "miss",
                    2_000_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                    context_key="ctx-overlap-1",
                ),
                _cc_record(
                    ts,
                    "miss",
                    2_000_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                    context_key="ctx-overlap-2",
                ),
            ]
        )

        totals = summary.totals
        self.assertAlmostEqual(totals.miss_wall_seconds, 4.0)
        self.assertAlmostEqual(totals.miss_union_wall_seconds, 2.0)
        self.assertAlmostEqual(
            totals.miss_wall_seconds, 2 * totals.miss_union_wall_seconds
        )

    def test_records_without_timestamps_still_count_in_wall_sum(self) -> None:
        summary = _summarize(
            [
                _cc_record(
                    None,
                    "miss",
                    300_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                )
            ]
        )

        totals = summary.totals
        self.assertAlmostEqual(totals.miss_wall_seconds, 300_000_000 / 1e9)
        self.assertEqual(totals.miss_union_wall_seconds, 0.0)


class DedupeTests(unittest.TestCase):
    """Byte-identical journal lines are dropped by default.

    Worth its own test because it is a silent trap for anyone reading these
    numbers: `analyze_compile_journal._read_journals` deduplicates on the raw
    line, and two genuinely distinct compiles that happen to serialize
    identically (same ts, same latency, no `context_key`) collapse into one.
    The count is reported as `records_deduped` rather than hidden.
    """

    def _journal(self) -> list[dict]:
        ts = f"{BASE_TS_PREFIX}14.000Z"
        return [
            _cc_record(
                ts, "miss", 1_000_000_000, PKG_CWD, miss_reason="context_not_found"
            ),
            _cc_record(
                ts, "miss", 1_000_000_000, PKG_CWD, miss_reason="context_not_found"
            ),
        ]

    def test_identical_lines_are_deduped_by_default(self) -> None:
        summary = _summarize(self._journal())

        self.assertEqual(summary.records_deduped, 1)
        self.assertEqual(summary.attributed_records, 1)
        self.assertAlmostEqual(summary.totals.miss_wall_seconds, 1.0)

    def test_no_dedupe_keeps_both_records(self) -> None:
        summary = _summarize(self._journal(), dedupe=False)

        self.assertEqual(summary.records_deduped, 0)
        self.assertEqual(summary.attributed_records, 2)
        self.assertAlmostEqual(summary.totals.miss_wall_seconds, 2.0)


class ProvenanceTests(unittest.TestCase):
    """`by_tree` / `by_source` / `unit_overlap` keep two builds distinguishable.

    A `build-logs-<target>` artifact can carry several archived builds (the
    upload globs `history/**`), and the headline reading depends on knowing
    whether a hit/miss pair is one build serving itself or two builds summed.
    """

    def test_tree_split_and_unit_overlap(self) -> None:
        summary = _summarize(
            [
                _cc_record(
                    f"{BASE_TS_PREFIX}20.000Z",
                    "miss",
                    1_000_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                    args=["odb.c", "-o", f"{DYLINT_TESTS_OUT}/odb.o"],
                ),
                _cc_record(
                    f"{BASE_TS_PREFIX}21.000Z",
                    "hit",
                    100_000_000,
                    PKG_CWD,
                    args=["odb.c", "-o", f"{DYLINT_TESTS_OUT}/odb.o"],
                ),
                _cc_record(
                    f"{BASE_TS_PREFIX}22.000Z",
                    "miss",
                    1_000_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                    args=["hash.c", "-o", "/tmp/out/hash.o"],
                ),
            ]
        )

        self.assertEqual(
            summary.by_tree,
            [
                measure_libgit2_cost.HitMiss(name="dylint/tests", hit=1, miss=1),
                measure_libgit2_cost.HitMiss(name="other", hit=0, miss=1),
            ],
        )
        # `odb` was both missed and served; `hash` was only missed.
        self.assertEqual(
            summary.unit_overlap,
            measure_libgit2_cost.UnitOverlap(miss_units=2, hit_units=1, both=1),
        )
        self.assertIn("1 both", measure_libgit2_cost.render_text(summary))

    def test_by_source_separates_two_archived_builds(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            build_a = Path(raw) / "history" / "build-a"
            build_b = Path(raw) / "history" / "build-b"
            build_a.mkdir(parents=True)
            build_b.mkdir(parents=True)
            _seed(
                build_a,
                [
                    _cc_record(
                        f"{BASE_TS_PREFIX}23.000Z",
                        "miss",
                        1_000_000_000,
                        PKG_CWD,
                        miss_reason="context_not_found",
                        context_key="ctx-build-a",
                    )
                ],
            )
            _seed(
                build_b,
                [
                    _cc_record(
                        f"{BASE_TS_PREFIX}24.000Z",
                        "hit",
                        100_000_000,
                        PKG_CWD,
                        context_key="ctx-build-b",
                    )
                ],
            )
            summary = measure_libgit2_cost.measure([raw])

        journal_a = str(build_a / "compile_journal.jsonl")
        journal_b = str(build_b / "compile_journal.jsonl")
        self.assertEqual(summary.journal_files, 2)
        self.assertEqual(sorted(summary.journal_paths), sorted([journal_a, journal_b]))
        self.assertEqual(
            summary.by_source,
            [
                measure_libgit2_cost.HitMiss(name=journal_a, hit=0, miss=1),
                measure_libgit2_cost.HitMiss(name=journal_b, hit=1, miss=0),
            ],
        )
        # Same unit name on both sides, but from two different journals --
        # which is precisely the case a bare hit/miss total cannot express.
        self.assertEqual(summary.unit_overlap.both, 1)


class BarMetricContractTests(unittest.TestCase):
    def _one_miss(self) -> list[dict]:
        return [
            _cc_record(
                f"{BASE_TS_PREFIX}15.000Z",
                "miss",
                1_000_000_000,
                PKG_CWD,
                miss_reason="context_not_found",
            )
        ]

    def test_every_bar_metric_exists_in_totals(self) -> None:
        records = self._one_miss()
        summary = _summarize(records)
        for metric in measure_libgit2_cost.BAR_METRICS:
            self.assertTrue(hasattr(summary.totals, metric))
            scoped = _summarize(records, bar_metric=metric)
            self.assertEqual(scoped.verdict.metric, metric)

    def test_record_counts_are_not_accepted_as_a_seconds_bar(self) -> None:
        # `totals` also carries `miss_records`/`hit_records`; comparing a
        # count against a bar measured in seconds is never what a caller
        # means, so the metric set is narrower than `totals`' key set.
        with self.assertRaises(ValueError):
            measure_libgit2_cost.measure(["/nonexistent"], bar_metric="miss_records")


class MissReasonsTests(unittest.TestCase):
    def test_miss_reasons_are_counted(self) -> None:
        summary = _summarize(
            [
                _cc_record(
                    f"{BASE_TS_PREFIX}06.000Z",
                    "miss",
                    100_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                ),
                _cc_record(
                    f"{BASE_TS_PREFIX}07.000Z",
                    "miss",
                    100_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                ),
                _cc_record(
                    f"{BASE_TS_PREFIX}08.000Z",
                    "miss",
                    100_000_000,
                    PKG_CWD,
                    miss_reason="uncacheable_input",
                ),
            ]
        )

        self.assertEqual(
            {row.reason: row.count for row in summary.miss_reasons},
            {"context_not_found": 2, "uncacheable_input": 1},
        )


class VerdictTests(unittest.TestCase):
    def test_verdict_under_and_over_the_bar(self) -> None:
        under_summary = _summarize(
            [
                _cc_record(
                    f"{BASE_TS_PREFIX}09.000Z",
                    "miss",
                    10_000_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                )
            ]
        )
        over_summary = _summarize(
            [
                _cc_record(
                    f"{BASE_TS_PREFIX}10.000Z",
                    "miss",
                    40_000_000_000,
                    PKG_CWD,
                    miss_reason="context_not_found",
                )
            ]
        )

        self.assertTrue(under_summary.verdict.under_bar)
        self.assertFalse(over_summary.verdict.under_bar)
        for summary in (under_summary, over_summary):
            self.assertEqual(summary.verdict.bar_seconds, 30.0)
            self.assertEqual(summary.verdict.metric, "miss_wall_seconds")

    def test_bar_metric_selects_the_measured_value(self) -> None:
        ts = f"{BASE_TS_PREFIX}11.000Z"
        # Distinct `context_key`s for the same reason as the union test
        # above: identical JSON lines are deduplicated away before the
        # rollup sees them.
        records = [
            _cc_record(
                ts,
                "miss",
                20_000_000_000,
                PKG_CWD,
                miss_reason="context_not_found",
                context_key="ctx-bar-1",
            ),
            _cc_record(
                ts,
                "miss",
                20_000_000_000,
                PKG_CWD,
                miss_reason="context_not_found",
                context_key="ctx-bar-2",
            ),
        ]

        wall_summary = _summarize(records)
        union_summary = _summarize(records, bar_metric="miss_union_wall_seconds")

        self.assertAlmostEqual(wall_summary.verdict.measured_seconds, 40.0)
        self.assertFalse(wall_summary.verdict.under_bar)

        self.assertAlmostEqual(union_summary.verdict.measured_seconds, 20.0)
        self.assertTrue(union_summary.verdict.under_bar)

        with self.assertRaises(ValueError):
            _summarize(records, bar_metric="not_a_real_metric")


class AvailabilityTests(unittest.TestCase):
    def test_no_attributed_records_is_unavailable_not_zero(self) -> None:
        no_attribution_summary = _summarize(
            [
                _cc_record(
                    f"{BASE_TS_PREFIX}12.000Z",
                    "miss",
                    100_000_000,
                    LZMA_CWD,
                    miss_reason="context_not_found",
                )
            ]
        )
        empty_summary = _summarize([])

        for summary in (no_attribution_summary, empty_summary):
            self.assertFalse(summary.verdict.data_available)
            self.assertFalse(summary.verdict.under_bar)
            self.assertIn("UNAVAILABLE", measure_libgit2_cost.render_text(summary))

    def test_a_root_with_no_journals_at_all_is_unavailable(self) -> None:
        # The likeliest operator error: pointing the script at a guessed
        # artifact subdirectory that holds no journals. It must not read as a
        # cost of zero, and `journal_paths` must show the emptiness.
        with tempfile.TemporaryDirectory() as raw:
            summary = measure_libgit2_cost.measure([raw])

        self.assertEqual(summary.journal_paths, [])
        self.assertFalse(summary.verdict.data_available)
        self.assertIn("UNAVAILABLE", measure_libgit2_cost.render_text(summary))


class MainTests(unittest.TestCase):
    def test_main_json_round_trip(self) -> None:
        records = [
            _cc_record(
                f"{BASE_TS_PREFIX}13.000Z",
                "miss",
                100_000_000,
                PKG_CWD,
                miss_reason="context_not_found",
            )
        ]
        with tempfile.TemporaryDirectory() as raw:
            root = _seed(Path(raw), records)

            buffer = io.StringIO()
            with contextlib.redirect_stdout(buffer):
                status = measure_libgit2_cost.main([root, "--json"])
            self.assertEqual(status, 0)
            payload = json.loads(buffer.getvalue())
            self.assertEqual(payload["attributed_records"], 1)

            out_path = Path(raw) / "summary.json"
            buffer = io.StringIO()
            with contextlib.redirect_stdout(buffer):
                status = measure_libgit2_cost.main([root, "--json-out", str(out_path)])
            self.assertEqual(status, 0)
            on_disk = json.loads(out_path.read_text(encoding="utf-8"))
            self.assertEqual(on_disk["attributed_records"], 1)


if __name__ == "__main__":
    unittest.main()
