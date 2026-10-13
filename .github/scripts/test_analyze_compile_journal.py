import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from _script_loader import load_sibling_script

analyze_compile_journal = load_sibling_script("analyze_compile_journal")

BASE_TS_PREFIX = "2026-09-01T19:47:"


def _write_journal(path: Path, records: list[dict]) -> None:
    with path.open("w", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record) + "\n")


def _base_records(repo_root: Path) -> list[dict]:
    """Ten records covering every bullet in the T1 task spec.

    1: hit, third-party via a `/registry/src/` cwd, `--crate-name=` (equals).
    2: miss/context_not_found, first-party (cwd == repo root),
       `--crate-name` (space form), no `context_key`.
    3: miss/uncacheable_input, `--test` harness link, tree dylint/tests.
    4: miss/input_fingerprint_mismatch, tree dylint/libraries.
    5: miss/no_artifact_for_key, dylint-link linker, tree dylint/target.
    6: miss/unknown, native `cc` unit (no --crate-name), third-party.
    7-8: hit/hit duplicate pair, same context_key + generation, overlapping.
    9-10: hit/hit duplicate pair, same context_key + generation, disjoint.
    """
    return [
        {
            "ts": f"{BASE_TS_PREFIX}02.000Z",
            "outcome": "hit",
            "context_key": "ctx-hit-1",
            "daemon_generation": "gen-a",
            "latency_ns": 1_000_000_000,
            "compiler": "/usr/bin/rustc",
            "args": [
                "--crate-name=soldr_cli",
                "--crate-type",
                "lib",
                "--out-dir",
                "/repo/target/x86_64-unknown-linux-gnu/debug/deps",
            ],
            "cwd": "/home/user/.cargo/registry/src/index.crates.io-abc/soldr_cli-0.1.0",
            "exit_code": 0,
            "session_id": "s1",
        },
        {
            "ts": f"{BASE_TS_PREFIX}03.000Z",
            "outcome": "miss",
            "miss_reason": "context_not_found",
            "daemon_generation": "gen-a",
            "latency_ns": 500_000_000,
            "compiler": "/repo/.soldr/shims/rustc",
            "args": [
                "--crate-name",
                "build_script_build",
                "--crate-type",
                "bin",
                "--out-dir",
                "/repo/target/debug/build/foo-abc/out",
            ],
            "cwd": str(repo_root),
            "exit_code": 0,
            "session_id": "s2",
        },
        {
            "ts": f"{BASE_TS_PREFIX}04.000Z",
            "outcome": "miss",
            "miss_reason": "uncacheable_input",
            "context_key": "ctx-3",
            "daemon_generation": "gen-a",
            "latency_ns": 2_000_000_000,
            "compiler": "/repo/.soldr/shims/rustc",
            "args": [
                "--crate-name=dlint_test_h",
                "--test",
                "--out-dir",
                "/repo/target/dylint/tests/deps",
            ],
            "cwd": "/repo/dylints/ban_raw_env_flag",
            "exit_code": 0,
            "session_id": "s3",
        },
        {
            "ts": f"{BASE_TS_PREFIX}05.000Z",
            "outcome": "miss",
            "miss_reason": "input_fingerprint_mismatch",
            "context_key": "ctx-4",
            "daemon_generation": "gen-a",
            "latency_ns": 1_500_000_000,
            "compiler": "/repo/.soldr/shims/rustc",
            "args": [
                "--crate-name=ban_raw_env_flag",
                "--crate-type",
                "cdylib",
                "--out-dir",
                "/repo/target/dylint/libraries/deps",
            ],
            "cwd": "/repo/dylints/ban_raw_env_flag",
            "exit_code": 0,
            "session_id": "s4",
        },
        {
            "ts": f"{BASE_TS_PREFIX}06.000Z",
            "outcome": "miss",
            "miss_reason": "no_artifact_for_key",
            "context_key": "ctx-5",
            "daemon_generation": "gen-a",
            "latency_ns": 800_000_000,
            "compiler": "/repo/.soldr/shims/rustc",
            "args": [
                "--crate-name=ban_raw_env_flag",
                "-Clinker=dylint-link",
                "--out-dir",
                "/repo/target/dylint/target/debug/deps",
            ],
            "cwd": "/repo/dylints/ban_raw_env_flag",
            "exit_code": 0,
            "session_id": "s5",
        },
        {
            "ts": f"{BASE_TS_PREFIX}07.000Z",
            "outcome": "miss",
            "miss_reason": "unknown",
            "context_key": "ctx-6",
            "daemon_generation": "gen-a",
            "latency_ns": 300_000_000,
            "compiler": "/usr/bin/cc",
            "args": ["foo.c", "-o", "/repo/target/debug/build/y/out/foo.o"],
            "cwd": "/home/user/.cargo/registry/src/index.crates.io-abc/sevenz_rust2-1.0.0",
            "exit_code": 0,
            "session_id": "s6",
        },
        {
            "ts": f"{BASE_TS_PREFIX}10.000Z",
            "outcome": "hit",
            "context_key": "ctx-dup-concurrent",
            "daemon_generation": "gen-b",
            "latency_ns": 5_000_000_000,
            "compiler": "/repo/.soldr/shims/rustc",
            "args": ["--crate-name=dup_crate", "--out-dir", "/repo/target/debug/deps"],
            "cwd": "/repo/some-crate",
            "exit_code": 0,
            "session_id": "s7",
        },
        {
            "ts": f"{BASE_TS_PREFIX}12.000Z",
            "outcome": "hit",
            "context_key": "ctx-dup-concurrent",
            "daemon_generation": "gen-b",
            "latency_ns": 5_000_000_000,
            "compiler": "/repo/.soldr/shims/rustc",
            "args": ["--crate-name=dup_crate", "--out-dir", "/repo/target/debug/deps"],
            "cwd": "/repo/some-crate",
            "exit_code": 0,
            "session_id": "s8",
        },
        {
            "ts": f"{BASE_TS_PREFIX}20.000Z",
            "outcome": "hit",
            "context_key": "ctx-dup-sequential",
            "daemon_generation": "gen-c",
            "latency_ns": 2_000_000_000,
            "compiler": "/repo/.soldr/shims/rustc",
            "args": ["--crate-name=dup_crate2", "--out-dir", "/repo/target/debug/deps"],
            "cwd": "/repo/some-crate",
            "exit_code": 0,
            "session_id": "s9",
        },
        {
            "ts": f"{BASE_TS_PREFIX}25.000Z",
            "outcome": "hit",
            "context_key": "ctx-dup-sequential",
            "daemon_generation": "gen-c",
            "latency_ns": 2_000_000_000,
            "compiler": "/repo/.soldr/shims/rustc",
            "args": ["--crate-name=dup_crate2", "--out-dir", "/repo/target/debug/deps"],
            "cwd": "/repo/some-crate",
            "exit_code": 0,
            "session_id": "s10",
        },
    ]


class RecordDerivationTests(unittest.TestCase):
    def test_crate_name_equals_form(self) -> None:
        record = analyze_compile_journal.record_from_json(
            {"outcome": "hit", "args": ["--crate-name=my_crate"]}, "src"
        )
        assert record is not None
        self.assertEqual(record.crate_name, "my_crate")

    def test_crate_name_space_form(self) -> None:
        record = analyze_compile_journal.record_from_json(
            {"outcome": "hit", "args": ["--crate-name", "my_crate"]}, "src"
        )
        assert record is not None
        self.assertEqual(record.crate_name, "my_crate")

    def test_native_unit_derives_crate_name_from_source_file(self) -> None:
        record = analyze_compile_journal.record_from_json(
            {
                "outcome": "miss",
                "compiler": "/usr/bin/cc",
                "args": ["-Wall", "foo.c", "-o", "/tmp/target/debug/build/x/out/foo.o"],
            },
            "src",
        )
        assert record is not None
        self.assertEqual(record.crate_name, "foo")
        self.assertEqual(record.crate_type, "native")
        self.assertEqual(record.out_dir, "/tmp/target/debug/build/x/out")

    def test_missing_outcome_is_not_a_record(self) -> None:
        self.assertIsNone(
            analyze_compile_journal.record_from_json({"foo": "bar"}, "src")
        )
        self.assertIsNone(analyze_compile_journal.record_from_json("not-a-dict", "src"))

    def test_source_is_stored(self) -> None:
        record = analyze_compile_journal.record_from_json(
            {"outcome": "hit", "args": []}, "the-source"
        )
        assert record is not None
        self.assertEqual(record.source, "the-source")


class TreeClassificationTests(unittest.TestCase):
    def test_dylint_trees(self) -> None:
        self.assertEqual(
            analyze_compile_journal.classify_tree("/repo/target/dylint/libraries/deps"),
            "dylint/libraries",
        )
        self.assertEqual(
            analyze_compile_journal.classify_tree("/repo/target/dylint/tests/deps"),
            "dylint/tests",
        )
        self.assertEqual(
            analyze_compile_journal.classify_tree("/repo/target/dylint/target/debug"),
            "dylint/target",
        )

    def test_triple_and_debug_release_are_stable(self) -> None:
        self.assertEqual(
            analyze_compile_journal.classify_tree(
                "/repo/target/x86_64-unknown-linux-gnu/debug/deps"
            ),
            "stable",
        )
        self.assertEqual(
            analyze_compile_journal.classify_tree("/repo/target/debug"), "stable"
        )
        self.assertEqual(
            analyze_compile_journal.classify_tree("/repo/target/release/deps"), "stable"
        )

    def test_unrecognized_out_dir_is_other(self) -> None:
        self.assertEqual(
            analyze_compile_journal.classify_tree("/somewhere/else"), "other"
        )
        self.assertEqual(analyze_compile_journal.classify_tree(""), "other")


class WorkspaceNamesTests(unittest.TestCase):
    def test_members_and_dylints_are_collected(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "Cargo.toml").write_text(
                '[workspace]\nmembers = ["crates/fake-one"]\n', encoding="utf-8"
            )
            member_dir = root / "crates" / "fake-one"
            member_dir.mkdir(parents=True)
            (member_dir / "Cargo.toml").write_text(
                '[package]\nname = "fake-one"\nversion = "0.1.0"\n', encoding="utf-8"
            )
            dylint_dir = root / "dylints" / "ban_something"
            dylint_dir.mkdir(parents=True)
            (dylint_dir / "Cargo.toml").write_text(
                '[package]\nname = "ban_something"\n', encoding="utf-8"
            )
            names = analyze_compile_journal.workspace_crate_names(root)
        self.assertEqual(names, {"fake-one", "ban_something"})

    def test_a_missing_cargo_toml_is_not_fatal(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            names = analyze_compile_journal.workspace_crate_names(Path(raw))
        self.assertEqual(names, set())


class RepoLockfileDiscoveryTests(unittest.TestCase):
    """`--lockfiles-from-repo` resolves through one implementation.

    Both entry points (this script's `main` and
    `check_third_party_compiles.py`) call `repo_lockfiles`, so the flag
    cannot mean two different things depending on which one CI invoked.
    """

    def test_root_and_dylint_lockfiles_are_found_in_order(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "Cargo.lock").write_text("", encoding="utf-8")
            for name in ("ban_b", "ban_a"):
                lint_dir = root / "dylints" / name
                lint_dir.mkdir(parents=True)
                (lint_dir / "Cargo.lock").write_text("", encoding="utf-8")
            (root / "dylints" / "no_lockfile").mkdir()
            found = analyze_compile_journal.repo_lockfiles(root)
        self.assertEqual(
            found,
            [
                root / "Cargo.lock",
                root / "dylints" / "ban_a" / "Cargo.lock",
                root / "dylints" / "ban_b" / "Cargo.lock",
            ],
        )

    def test_a_repo_without_lockfiles_is_empty_not_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            self.assertEqual(analyze_compile_journal.repo_lockfiles(Path(raw)), [])

    def test_main_feeds_discovered_lockfiles_into_the_summary(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "Cargo.lock").write_text(
                '[[package]]\nname = "some_dep"\nversion = "1.0.0"\n', encoding="utf-8"
            )
            journal_dir = root / "journals"
            journal_dir.mkdir()
            _write_journal(
                journal_dir / "compile_journal.jsonl",
                [{"outcome": "hit", "args": [], "cwd": "/somewhere"}],
            )
            out = root / "summary.json"
            buffer = io.StringIO()
            with contextlib.redirect_stdout(buffer):
                status = analyze_compile_journal.main(
                    [
                        str(journal_dir),
                        "--repo-root",
                        str(root),
                        "--lockfiles-from-repo",
                        "--json-out",
                        str(out),
                    ]
                )
            self.assertEqual(status, 0)
            summary = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual(summary["inputs"]["lockfiles"], [str(root / "Cargo.lock")])
        # A lockfile turns the two nullable buckets into real integers.
        self.assertEqual(summary["buckets"]["fresh"], 1)
        self.assertEqual(summary["buckets"]["compiling_no_record"], 0)

    def test_json_out_creates_its_parent_directory(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            out = Path(raw) / "logs" / "compile-journal-analysis.json"
            buffer = io.StringIO()
            with contextlib.redirect_stdout(buffer):
                status = analyze_compile_journal.main(
                    [str(Path(raw) / "no-journals-here"), "--json-out", str(out)]
                )
            self.assertEqual(status, 0)
            self.assertEqual(
                json.loads(out.read_text(encoding="utf-8"))["schema_version"], 1
            )


class CargoLogParsingTests(unittest.TestCase):
    def test_dirty_line_and_status_lines(self) -> None:
        text = "\n".join(
            [
                "   0.12 Compiling bar v0.1.0 (/repo)",
                "Fresh baz v2.0.0",
                "fingerprint dirty for foo v1.2.3 (path+file:///repo/foo)",
                "    dirty: FsStatusOutdated",
            ]
        )
        parsed = analyze_compile_journal.parse_cargo_logs(text)
        self.assertEqual(
            parsed.dirty,
            [
                analyze_compile_journal.DirtyEntry(
                    crate="foo", version="1.2.3", reason="FsStatusOutdated"
                )
            ],
        )
        status_entry = analyze_compile_journal.StatusEntry
        self.assertIn(status_entry("Compiling", "bar", "0.1.0"), parsed.status)
        self.assertIn(status_entry("Fresh", "baz", "2.0.0"), parsed.status)

    def test_checking_verb_is_captured(self) -> None:
        parsed = analyze_compile_journal.parse_cargo_logs("Checking qux v0.3.0\n")
        self.assertEqual(
            parsed.status,
            [analyze_compile_journal.StatusEntry("Checking", "qux", "0.3.0")],
        )

    def test_dirty_reason_falls_back_to_camel_case(self) -> None:
        text = "fingerprint dirty for foo v1.2.3\n    TargetInnerChanged happened\n"
        parsed = analyze_compile_journal.parse_cargo_logs(text)
        self.assertEqual(parsed.dirty[0].reason, "TargetInnerChanged")

    def test_no_reason_found_is_unknown(self) -> None:
        text = "fingerprint dirty for foo v1.2.3\nnothing useful here\nor here\n"
        parsed = analyze_compile_journal.parse_cargo_logs(text)
        self.assertEqual(parsed.dirty[0].reason, "unknown")


class AnalyzeFixtureTests(unittest.TestCase):
    """Exercises `analyze()` end-to-end against the 10-record fixture."""

    def setUp(self) -> None:
        self._stack = contextlib.ExitStack()
        journal_raw = self._stack.enter_context(tempfile.TemporaryDirectory())
        repo_raw = self._stack.enter_context(tempfile.TemporaryDirectory())
        self.journal_dir = Path(journal_raw)
        self.repo_root = Path(repo_raw)
        self.records = _base_records(self.repo_root)
        _write_journal(self.journal_dir / "compile_journal.jsonl", self.records)
        self.summary = analyze_compile_journal.analyze(
            [str(self.journal_dir)], repo_root=self.repo_root
        )

    def tearDown(self) -> None:
        self._stack.close()

    def test_totals(self) -> None:
        self.assertEqual(self.summary["totals"]["records"], 10)
        self.assertEqual(self.summary["totals"]["third_party"], 2)
        self.assertEqual(self.summary["totals"]["first_party"], 8)
        self.assertEqual(self.summary["totals"]["unclassified"], 0)

    def test_party_classification_by_cwd_overrides_name(self) -> None:
        # record 1: crate name "soldr_cli" but a /registry/src/ cwd -> third-party.
        self.assertIn("soldr_cli", self.summary["third_party"]["crates"])
        # record 2: crate name "build_script_build" but cwd == repo root -> first-party.
        self.assertIn("build_script_build", self.summary["first_party"]["crates"])

    def test_outcomes_and_miss_reasons(self) -> None:
        self.assertEqual(self.summary["outcomes"], {"hit": 5, "miss": 5})
        self.assertEqual(
            self.summary["miss_reasons"],
            {
                "context_not_found": 1,
                "input_fingerprint_mismatch": 1,
                "no_artifact_for_key": 1,
                "uncacheable_input": 1,
                "unknown": 1,
            },
        )

    def test_duplicates(self) -> None:
        duplicates = self.summary["duplicates"]
        self.assertEqual(duplicates["groups"], 2)
        self.assertEqual(duplicates["records_in_groups"], 4)
        self.assertEqual(duplicates["excess_records"], 2)
        self.assertEqual(duplicates["concurrent"], 2)
        self.assertEqual(duplicates["sequential"], 2)
        self.assertEqual(duplicates["cross_generation"], 0)
        self.assertEqual(duplicates["records_without_context_key"], 1)

    def test_tree_map(self) -> None:
        trees = self.summary["trees"]
        self.assertEqual(trees["dylint/tests"], 1)
        self.assertEqual(trees["dylint/libraries"], 1)
        self.assertEqual(trees["dylint/target"], 1)
        self.assertEqual(trees["stable"], 7)
        self.assertEqual(trees["other"], 0)

    def test_third_party_miss_wall_seconds(self) -> None:
        expected = 300_000_000 / 1e9
        self.assertAlmostEqual(
            self.summary["third_party"]["miss_wall_seconds"], expected
        )
        # The strict `outcome == "miss"` count the ratchet reads. Record 1 is
        # a third-party HIT, so the broad party count and this one only agree
        # here because the fixture has no link_miss records.
        self.assertEqual(self.summary["cost"]["third_party_miss_records"], 1)
        self.assertEqual(self.summary["third_party"]["misses"], 1)
        self.assertAlmostEqual(
            self.summary["cost"]["third_party_miss_wall_seconds"], expected
        )
        self.assertIsNone(self.summary["cost"]["third_party_miss_cpu_seconds"])
        self.assertIsNone(self.summary["third_party"]["miss_cpu_seconds"])

    def test_buckets_sum_to_third_party_total(self) -> None:
        buckets = self.summary["buckets"]
        self.assertEqual(buckets["third_party_total"], 2)
        self.assertEqual(
            buckets["compiling_hit"]
            + buckets["compiling_miss"]
            + buckets["compiling_other"],
            buckets["third_party_total"],
        )
        self.assertIsNone(buckets["fresh"])
        self.assertIsNone(buckets["compiling_no_record"])

    def test_fresh_becomes_an_int_with_a_lockfile(self) -> None:
        lockfile = self.journal_dir / "Cargo.lock"
        lockfile.write_text(
            '[[package]]\nname = "totally_fresh_crate"\nversion = "1.0.0"\n',
            encoding="utf-8",
        )
        # A `Fresh` status line for the same crate must NOT flip it into
        # compiling_no_record -- only Compiling/Checking/dirty lines count as
        # "cargo touched this crate" for that bucket.
        cargo_log = self.journal_dir / "build.log"
        cargo_log.write_text("Fresh totally_fresh_crate v1.0.0\n", encoding="utf-8")
        summary = analyze_compile_journal.analyze(
            [str(self.journal_dir)],
            cargo_log_paths=[str(cargo_log)],
            repo_root=self.repo_root,
            lockfiles=[str(lockfile)],
        )
        self.assertEqual(summary["buckets"]["fresh"], 1)
        self.assertEqual(summary["buckets"]["compiling_no_record"], 0)

    def test_uncacheable_input_bucket(self) -> None:
        uncacheable = self.summary["uncacheable_input"]
        self.assertEqual(uncacheable["total"], 1)
        self.assertEqual(uncacheable["buckets"][0]["crate"], "dlint_test_h")
        self.assertTrue(uncacheable["buckets"][0]["test_harness"])
        self.assertFalse(uncacheable["buckets"][0]["dylint_link"])


class DedupeAndDiscoveryTests(unittest.TestCase):
    def test_dedupe_drops_byte_identical_lines_across_files(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            journal_dir = Path(raw)
            record = {"ts": f"{BASE_TS_PREFIX}01.000Z", "outcome": "hit", "args": []}
            _write_journal(journal_dir / "compile_journal.jsonl", [record])
            history_dir = journal_dir / "history" / "abc"
            history_dir.mkdir(parents=True)
            _write_journal(history_dir / "compile_journal.jsonl", [record])

            deduped = analyze_compile_journal.analyze([str(journal_dir)])
            self.assertEqual(deduped["totals"]["records"], 1)
            self.assertEqual(deduped["inputs"]["duplicate_lines_dropped"], 1)

            kept = analyze_compile_journal.analyze([str(journal_dir)], dedupe=False)
            self.assertEqual(kept["totals"]["records"], 2)
            self.assertEqual(kept["inputs"]["duplicate_lines_dropped"], 0)

    def test_rotated_journal_files_are_discovered(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            journal_dir = Path(raw)
            (journal_dir / "compile_journal.jsonl").write_text("", encoding="utf-8")
            rotated = journal_dir / "compile_journal.jsonl.2026-09-01T19-47-02.568Z"
            rotated.write_text(
                json.dumps({"outcome": "hit", "args": []}) + "\n", encoding="utf-8"
            )
            found = analyze_compile_journal.discover_journal_files([str(journal_dir)])
            self.assertIn(rotated, found)

    def test_malformed_line_is_counted_not_fatal(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            journal_dir = Path(raw)
            path = journal_dir / "compile_journal.jsonl"
            path.write_text(
                "not-json-at-all\n"
                + json.dumps({"outcome": "hit", "args": []})
                + "\n"
                + json.dumps({"no_outcome_here": True})
                + "\n",
                encoding="utf-8",
            )
            summary = analyze_compile_journal.analyze([str(journal_dir)])
            self.assertEqual(summary["totals"]["records"], 1)
            self.assertEqual(summary["inputs"]["malformed_lines"], 2)


class CompareBaselineTests(unittest.TestCase):
    def test_delta_and_tolerance(self) -> None:
        summary = analyze_compile_journal.Summary(
            totals=analyze_compile_journal.Totals(
                records=9, third_party=2, first_party=7
            ),
            outcomes=[analyze_compile_journal.NamedCount("hit", 1)],
        )
        baseline = analyze_compile_journal.Baseline(
            tolerance=10,
            metrics=[
                analyze_compile_journal.NamedCount("total_units", 10),
                analyze_compile_journal.NamedCount("hits", 1),
                analyze_compile_journal.NamedCount("first_party", 100),
            ],
        )
        rows = analyze_compile_journal.compare_baseline(summary, baseline)
        by_metric = {row.metric: row for row in rows}
        self.assertEqual(by_metric["total_units"].delta, -1)
        self.assertTrue(by_metric["total_units"].within_tolerance)
        self.assertEqual(by_metric["hits"].delta, 0)
        self.assertTrue(by_metric["hits"].within_tolerance)
        # actual first_party (7) vs baseline 100 with 10% tolerance -> far outside.
        self.assertFalse(by_metric["first_party"].within_tolerance)

    def test_the_pinned_baseline_names_only_mapped_metrics(self) -> None:
        # `compare_baseline` skips a metric it has no path for, so a typo in
        # the checked-in baseline would silently drop that row instead of
        # reporting a delta -- the failure mode of a comparison nobody reads
        # closely.
        baseline_path = (
            Path(__file__).resolve().parent
            / "baselines"
            / "compile_journal_baseline_33536940076.json"
        )
        baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
        self.assertEqual(baseline["run_id"], "33536940076")
        unmapped = sorted(
            set(baseline["metrics"])
            - set(analyze_compile_journal.BASELINE_METRIC_PATHS)
        )
        self.assertEqual(unmapped, [])
        rows = analyze_compile_journal.compare_baseline(
            analyze_compile_journal.analyze_summary([]),
            analyze_compile_journal.Baseline.from_json(baseline),
        )
        self.assertEqual(len(rows), len(baseline["metrics"]))

    def test_zero_baseline_requires_exact_match(self) -> None:
        summary = analyze_compile_journal.Summary(
            outcomes=[analyze_compile_journal.NamedCount("hit", 1)]
        )
        baseline = analyze_compile_journal.Baseline(
            tolerance=50, metrics=[analyze_compile_journal.NamedCount("hits", 0)]
        )
        rows = analyze_compile_journal.compare_baseline(summary, baseline)
        self.assertFalse(rows[0].within_tolerance)


class MainTests(unittest.TestCase):
    def test_no_paths_reports_and_exits_zero(self) -> None:
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            status = analyze_compile_journal.main([])
        self.assertEqual(status, 0)
        self.assertIn("no compile journals found", buffer.getvalue())

    def test_json_mode_emits_parseable_json(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            journal_dir = Path(raw)
            _write_journal(
                journal_dir / "compile_journal.jsonl",
                [{"outcome": "hit", "args": [], "cwd": "/somewhere"}],
            )
            buffer = io.StringIO()
            with contextlib.redirect_stdout(buffer):
                status = analyze_compile_journal.main([str(journal_dir), "--json"])
        self.assertEqual(status, 0)
        payload = json.loads(buffer.getvalue())
        self.assertEqual(payload["schema_version"], 1)
        self.assertEqual(payload["totals"]["records"], 1)

    def test_render_text_includes_expected_sections(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            journal_dir = Path(raw)
            _write_journal(
                journal_dir / "compile_journal.jsonl",
                [{"outcome": "hit", "args": [], "cwd": "/somewhere"}],
            )
            summary = analyze_compile_journal.analyze_summary([str(journal_dir)])
        rendered = analyze_compile_journal.render_text(summary)
        self.assertIn("totals", rendered)
        self.assertIn("uncacheable_input", rendered)
        self.assertIn("cargo logs: none supplied", rendered)
        self.assertIn("four-bucket join", rendered)
        self.assertIn("unavailable (the zccache journal", rendered)


if __name__ == "__main__":
    unittest.main()
