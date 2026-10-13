"""Tests for the soldr#3045 duplicate-identity cost verifier.

soldr#3045 phase 1 step 0 asks one question before any optimization work
happens: is the observed 525x duplicate `cc` compiler identity in a real
journal actually 525 real per-file compiles, or is most of it cc-rs running
its own compiler-capability probes (`-dM -E`, `-Wa,--help -c`, `--version`,
`-E -`) under the same `context_key`? `verify_duplicate_identity_cost.py`
(T1) answers that from a compile-journal, and this file pins its contract.

The fixtures below build real zccache `JournalEntry` records -- one JSON
object per line, the same shape `analyze_compile_journal.py` already
consumes (`record_from_json`, `_interval`, `_classify_duplicate`,
`NATIVE_SRC_EXTS` are the schema authority; see that module's docstring).
`ts` is the record-WRITE time, i.e. the END of the compile, so every
record's interval is derived as `[ts_ns - latency_ns, ts_ns]`: two records
overlap when their `ts` values sit close together and their `latency_ns`
values are long enough to span each other, and are sequential when their
`ts` values are separated by more than either latency.

If a test here disagrees with the tool once T1 lands,
`analyze_compile_journal.py` is the authority for record semantics -- fix
the assertion only if reading that module shows the fixture, not the tool,
got the schema wrong.
"""

from __future__ import annotations

import dataclasses
import json
from pathlib import Path
from typing import Any

import pytest
from _script_loader import load_sibling_script

verify_duplicate_identity_cost = load_sibling_script("verify_duplicate_identity_cost")
# The module name is 31 characters; aliasing keeps every `tool.foo(...)` call
# site below the repo's 88-column black width instead of forcing every one of
# them onto its own multi-line wrap.
tool = verify_duplicate_identity_cost


def _journal_record(
    *,
    args: list,
    compiler: str,
    ts: str,
    context_key: "str | None" = None,
    latency_ns: int = 100_000_000,
    outcome: str = "miss",
    miss_reason: "str | None" = None,
) -> dict:
    """One zccache compile-journal record, using the real field names.

    Only the fields the tests below actually vary are parameters; the rest
    (`cwd`, `daemon_generation`, `env`, `exit_code`, `session_id`, the two
    peak-RSS fields) are realistic constants -- soldr#3045 step 0 does not
    need them to vary to answer the probe-vs-real-compile question.
    """
    return {
        "args": args,
        "compiler": compiler,
        "context_key": context_key,
        "cwd": "/home/ci/build",
        "daemon_generation": "1",
        "env": [],
        "exit_code": 0,
        "latency_ns": latency_ns,
        "miss_reason": miss_reason,
        "outcome": outcome,
        "session_id": None,
        "ts": ts,
        "child_peak_rss_bytes": 50_000_000,
        "tree_peak_rss_bytes": 80_000_000,
    }


def _write_journal(tmp_path: Path, records: list) -> Path:
    path = tmp_path / "compile_journal.jsonl"
    path.write_text(
        "\n".join(json.dumps(record) for record in records) + "\n", encoding="utf-8"
    )
    return path


def _find_identity(identities: list, context_key: "str | None") -> dict:
    for identity in identities:
        if identity.get("context_key") == context_key:
            return identity
    raise AssertionError(f"no identity found for context_key={context_key!r}")


def _walk_no_cpu_numbers(value: Any, path: str = "") -> None:
    """Recursively assert no `*cpu*` key maps to a non-None number.

    The zccache journal has no CPU-time field (see analyze_compile_journal's
    module docstring); a tool that invents one would make this whole step-0
    investigation wrong.
    """
    if isinstance(value, dict):
        for key, sub in value.items():
            new_path = f"{path}.{key}" if path else str(key)
            is_number = isinstance(sub, (int, float)) and not isinstance(sub, bool)
            if "cpu" in str(key).lower() and is_number:
                raise AssertionError(f"{new_path} is a numeric CPU value: {sub!r}")
            _walk_no_cpu_numbers(sub, new_path)
    elif isinstance(value, list):
        for index, item in enumerate(value):
            _walk_no_cpu_numbers(item, f"{path}[{index}]")


def _totals(**overrides: Any) -> Any:
    """A `Totals` with every field zeroed except the overrides."""
    defaults = tool.Totals(
        journal_files=[],
        records=0,
        malformed=0,
        deduped_lines=0,
        records_with_context_key=0,
        records_without_context_key=0,
        duplicate_groups=0,
        duplicate_records=0,
        excess_records=0,
        concurrent=0,
        sequential=0,
        cross_generation=0,
        total_wall_seconds=0.0,
        coalescable_wall_seconds_total=0.0,
        coalescable_wall_seconds_probe=0.0,
        coalescable_wall_seconds_real=0.0,
        coalescable_wall_seconds_unknown=0.0,
        coalescable_fraction_real=0.0,
    )
    return dataclasses.replace(defaults, **overrides)


NATIVE_CC = "/run/current-system/sw/bin/cc"
RUSTC = "/home/ci/.rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/bin/rustc"


def test_classify_invocation_native_probes_are_probes() -> None:
    probe_argvs = [
        ["-dM", "-E", "-x", "c", "/dev/null"],
        ["-Wa,--help", "-c", "-o", "null.3117452.o", "-x", "assembler", "/dev/null"],
        ["--version"],
        ["-E", "-"],
    ]
    for argv in probe_argvs:
        # Keyword arguments on purpose: the two positional orders
        # (`args, compiler` and `compiler, args`) are both plausible readings
        # of the same signature, and a silently-swapped pair would still be
        # two strings-or-lists at the call site. Naming them means this test
        # cannot pass against a transposed implementation.
        assert tool.classify_invocation(args=argv, compiler=NATIVE_CC) == "probe"


def test_classify_invocation_real_compiles_are_real_compile() -> None:
    assert (
        tool.classify_invocation(
            args=["-c", "-o", "foo.o", "src/foo.c"], compiler=NATIVE_CC
        )
        == "real-compile"
    )
    # The crate is literally named "cc" (the cc-rs build helper crate), but
    # the compiler is rustc, not the native cc binary -- rustc is never a
    # probe regardless of what the compiled crate is named.
    assert (
        tool.classify_invocation(
            args=[
                "--crate-name",
                "cc",
                "--crate-type",
                "lib",
                "-C",
                "linker=dylint-link",
            ],
            compiler=RUSTC,
        )
        == "real-compile"
    )


def test_group_classification_majority_and_tie_break() -> None:
    # A `context_key` group is one invocation identity, so its coalescable
    # seconds must not be bucketed off whichever member the journal happened
    # to write first.
    majority = tool._group_classification
    assert majority({"probe": 524, "real-compile": 1}) == "probe"
    assert majority({"probe": 1, "real-compile": 524}) == "real-compile"
    # Ties resolve toward "real-compile": the negligible verdict argues for
    # closing soldr#3045, so a tie must not shrink the measured payoff.
    assert majority({"probe": 3, "real-compile": 3}) == "real-compile"
    assert majority({}) == "unknown"


def test_dylint_link_flag_reflected_in_identity(tmp_path: Path) -> None:
    dylint_args = [
        "--crate-name",
        "cc",
        "--edition=2021",
        "src/lib.rs",
        "--crate-type",
        "lib",
        "-C",
        "linker=dylint-link",
    ]
    plain_args = [
        "--crate-name",
        "cc",
        "--edition=2021",
        "src/lib.rs",
        "--crate-type",
        "lib",
    ]
    records = [
        _journal_record(
            args=dylint_args,
            compiler=RUSTC,
            context_key="ctx-dylint",
            ts="2026-09-17T19:19:28.450Z",
        ),
        _journal_record(
            args=dylint_args,
            compiler=RUSTC,
            context_key="ctx-dylint",
            ts="2026-09-17T19:19:29.450Z",
        ),
        _journal_record(
            args=plain_args,
            compiler=RUSTC,
            context_key="ctx-plain",
            ts="2026-09-17T19:19:28.450Z",
        ),
        _journal_record(
            args=plain_args,
            compiler=RUSTC,
            context_key="ctx-plain",
            ts="2026-09-17T19:19:29.450Z",
        ),
    ]
    _write_journal(tmp_path, records)
    out_path = tmp_path / "summary.json"
    code = tool.main([str(tmp_path), "--json-out", str(out_path)])
    assert code == 0
    summary = json.loads(out_path.read_text(encoding="utf-8"))

    dylint_identity = _find_identity(summary["identities"], "ctx-dylint")
    plain_identity = _find_identity(summary["identities"], "ctx-plain")
    assert dylint_identity["dylint_link"] is True
    assert plain_identity["dylint_link"] is False


def test_concurrent_duplicates_report_flavour_and_coalescable_wall(
    tmp_path: Path,
) -> None:
    # Three records, ts one second apart, latency 5s each -> every pair's
    # [ts - latency, ts] window overlaps every other pair's window.
    records = [
        _journal_record(
            args=["-c", "-o", "foo.o", "src/foo.c"],
            compiler=NATIVE_CC,
            context_key="ctx-concurrent",
            ts=ts,
            latency_ns=5_000_000_000,
        )
        for ts in (
            "2026-09-17T19:19:28.000Z",
            "2026-09-17T19:19:29.000Z",
            "2026-09-17T19:19:30.000Z",
        )
    ]
    _write_journal(tmp_path, records)
    out_path = tmp_path / "summary.json"
    code = tool.main([str(tmp_path), "--json-out", str(out_path)])
    assert code == 0
    summary = json.loads(out_path.read_text(encoding="utf-8"))

    identity = _find_identity(summary["identities"], "ctx-concurrent")
    assert identity["flavours"]["concurrent"] == 3
    # (sum(latency_ns) - max(latency_ns)) / 1e9 = (15e9 - 5e9) / 1e9 = 10.0
    assert identity["coalescable_wall_seconds"] == pytest.approx(10.0)


def test_sequential_duplicates_report_flavour_and_no_coalescable_wall(
    tmp_path: Path,
) -> None:
    # Three records ten seconds apart, latency 1s each -> disjoint windows.
    records = [
        _journal_record(
            args=["-c", "-o", "foo.o", "src/foo.c"],
            compiler=NATIVE_CC,
            context_key="ctx-sequential",
            ts=ts,
            latency_ns=1_000_000_000,
        )
        for ts in (
            "2026-09-17T19:00:10.000Z",
            "2026-09-17T19:00:20.000Z",
            "2026-09-17T19:00:30.000Z",
        )
    ]
    _write_journal(tmp_path, records)
    out_path = tmp_path / "summary.json"
    code = tool.main([str(tmp_path), "--json-out", str(out_path)])
    assert code == 0
    summary = json.loads(out_path.read_text(encoding="utf-8"))

    identity = _find_identity(summary["identities"], "ctx-sequential")
    assert identity["flavours"]["sequential"] == 3
    # Sequential-only groups are already served by the store; they must not
    # contribute to the coalescable-wall total.
    assert summary["totals"]["coalescable_wall_seconds_total"] == pytest.approx(0.0)


def test_records_without_context_key_are_never_grouped(tmp_path: Path) -> None:
    records = [
        _journal_record(
            args=["-c", "-o", "a.o", "src/a.c"],
            compiler=NATIVE_CC,
            context_key=None,
            ts="2026-09-17T19:00:10.000Z",
        ),
        _journal_record(
            args=["-c", "-o", "b.o", "src/b.c"],
            compiler=NATIVE_CC,
            context_key=None,
            ts="2026-09-17T19:00:11.000Z",
        ),
    ]
    _write_journal(tmp_path, records)
    out_path = tmp_path / "summary.json"
    code = tool.main([str(tmp_path), "--json-out", str(out_path)])
    assert code == 0
    summary = json.loads(out_path.read_text(encoding="utf-8"))

    assert summary["totals"]["records_without_context_key"] == 2
    assert not any(
        identity.get("context_key") is None for identity in summary["identities"]
    )


def test_dedupe_default_and_no_dedupe_flag(tmp_path: Path) -> None:
    record = _journal_record(
        args=["-c", "-o", "foo.o", "src/foo.c"],
        compiler=NATIVE_CC,
        context_key="ctx-dupe-line",
        ts="2026-09-17T19:00:10.000Z",
    )
    line = json.dumps(record)
    journal_path = tmp_path / "compile_journal.jsonl"
    journal_path.write_text(line + "\n" + line + "\n", encoding="utf-8")

    deduped_out = tmp_path / "deduped.json"
    assert tool.main([str(tmp_path), "--json-out", str(deduped_out)]) == 0
    deduped_summary = json.loads(deduped_out.read_text(encoding="utf-8"))

    raw_out = tmp_path / "raw.json"
    assert tool.main([str(tmp_path), "--json-out", str(raw_out), "--no-dedupe"]) == 0
    raw_summary = json.loads(raw_out.read_text(encoding="utf-8"))

    assert deduped_summary["totals"]["records"] != raw_summary["totals"]["records"]


def test_no_cpu_numbers_anywhere_in_summary(tmp_path: Path) -> None:
    records = [
        _journal_record(
            args=["-c", "-o", "foo.o", "src/foo.c"],
            compiler=NATIVE_CC,
            context_key="ctx-cpu-check",
            ts="2026-09-17T19:00:10.000Z",
        )
    ]
    _write_journal(tmp_path, records)
    out_path = tmp_path / "summary.json"
    code = tool.main([str(tmp_path), "--json-out", str(out_path)])
    assert code == 0
    summary = json.loads(out_path.read_text(encoding="utf-8"))
    _walk_no_cpu_numbers(summary)


def test_verdict_boundaries() -> None:
    cases = [
        (0.01, 37.0, "negligible"),
        (0.20, 128.0, "proceed"),
        (0.10, 64.0, "need-better-data"),
    ]
    for fraction, seconds, expected_code in cases:
        # `coalescable_wall_seconds_real` (not `_total`) is the numerator the
        # fraction and the verdict are both defined on: `_total` also carries
        # probe and unknown groups, which are exactly what this step-0 check
        # exists to subtract out.
        totals = _totals(
            coalescable_fraction_real=fraction,
            coalescable_wall_seconds_real=seconds,
            total_wall_seconds=seconds / fraction,
        )
        result = tool.verdict(totals)
        code, sentence = result.code, result.sentence
        assert code == expected_code
        # Integer substring matching, so the assertion survives a change in
        # decimal places.
        assert str(int(seconds)) in sentence
        assert str(int(fraction * 100)) in sentence


def test_verdict_unknown_bucket_blocks_a_negligible_close() -> None:
    # "negligible" is the verdict that argues for closing soldr#3045. It is
    # only honest when the work it excluded was actually classified: a
    # coalescable-but-unclassified bucket above the same 5% threshold means
    # the classifier did not recognise this journal, not that there is
    # nothing here.
    blocked = _totals(
        coalescable_fraction_real=0.01,
        coalescable_wall_seconds_real=1.0,
        coalescable_wall_seconds_unknown=20.0,
        total_wall_seconds=100.0,
    )
    blocked_verdict = tool.verdict(blocked)
    assert blocked_verdict.code == "need-better-data"
    assert "20.00s" in blocked_verdict.sentence

    # Control: the same fraction with a small unknown bucket still closes.
    allowed = dataclasses.replace(blocked, coalescable_wall_seconds_unknown=1.0)
    assert tool.verdict(allowed).code == "negligible"

    # The override only ever weakens a close -- it can never touch "proceed",
    # so it cannot hide payoff.
    proceeding = _totals(
        coalescable_fraction_real=0.30,
        coalescable_wall_seconds_real=30.0,
        coalescable_wall_seconds_unknown=60.0,
        total_wall_seconds=100.0,
    )
    assert tool.verdict(proceeding).code == "proceed"


def test_verdict_sentence_does_not_claim_elapsed_build_time() -> None:
    # `total_wall_seconds` is summed per-compile latency across parallel
    # compiles, not the build's elapsed time. This sentence gets quoted into
    # an issue comment away from the docstring that explains it, so it has to
    # carry the qualifier itself.
    sentence = tool.verdict(
        _totals(
            coalescable_fraction_real=0.01,
            coalescable_wall_seconds_real=1.0,
            coalescable_wall_seconds_unknown=0.0,
            total_wall_seconds=100.0,
        )
    ).sentence
    assert "not the build's elapsed time" in sentence


def test_markdown_detail_names_the_compiler_and_classification_counts(
    tmp_path: Path,
) -> None:
    # soldr#3045's actual question is whether the 525x `cc` identity is
    # native probe work or a rustc compile of the crate *named* `cc`. The
    # crate column cannot tell those apart, so the detail block must name the
    # compiler.
    records = [
        _journal_record(
            args=["-dM", "-E", "-x", "c", "/dev/null"],
            compiler=NATIVE_CC,
            context_key="ctx-detail",
            ts=ts,
            latency_ns=5_000_000_000,
        )
        for ts in ("2026-09-17T19:19:28.000Z", "2026-09-17T19:19:29.000Z")
    ]
    journal_path = _write_journal(tmp_path, records)
    markdown = tool.render_markdown(tool.build_summary([journal_path]))

    assert "compiler: cc" in markdown
    assert "native compiler: True" in markdown
    assert "distinct argv in group: 1" in markdown
    assert "classification counts: probe=2" in markdown
    # Still brace-free: counts are rendered as key=value pairs, never as a
    # dict repr, so no unrendered placeholder can hide in a posted comment.
    assert "{" not in markdown
    assert "}" not in markdown


def test_unwritable_markdown_out_is_a_nonzero_exit(tmp_path: Path) -> None:
    # The documented workflow feeds --markdown-out straight into
    # `gh issue comment --body-file`. A swallowed write error would post
    # whatever that path held before, which looks exactly like a fresh
    # result.
    blocker = tmp_path / "not-a-directory.txt"
    blocker.write_text("", encoding="utf-8")
    records = [
        _journal_record(
            args=["-c", "-o", "foo.o", "src/foo.c"],
            compiler=NATIVE_CC,
            context_key="ctx-unwritable",
            ts="2026-09-17T19:00:10.000Z",
        )
    ]
    _write_journal(tmp_path, records)

    code = tool.main([str(tmp_path), "--markdown-out", str(blocker / "out.md")])
    assert code == 2
    # A path typo in the journal argument is still 1, not 2 -- the two
    # failures must stay distinguishable.
    assert tool.main(["/nonexistent/path"]) == 1


def test_render_markdown_negligible_case(tmp_path: Path) -> None:
    # The fixture is a real journal run through `build_summary` rather than a
    # hand-written summary dict. A hand-written one would be a second copy of
    # the tool's own output schema living in its test, and it would drift
    # silently the moment a key is added -- the "test re-implements what it
    # tests" smell CLAUDE.md calls out.
    top_args = ["-dM", "-E", "-x", "c", "/dev/null"]
    records = [
        _journal_record(
            args=top_args,
            compiler=NATIVE_CC,
            context_key="ctx-probe-1",
            ts=ts,
            latency_ns=5_000_000_000,
        )
        for ts in (
            "2026-09-17T19:19:28.000Z",
            "2026-09-17T19:19:29.000Z",
            "2026-09-17T19:19:30.000Z",
        )
    ]
    journal_path = _write_journal(tmp_path, records)
    summary = tool.build_summary([journal_path])

    totals = summary.totals
    # Concurrent, so the wall time IS coalescable in principle -- but it is
    # probe work, so none of it counts toward the payoff the verdict is about.
    assert totals.coalescable_wall_seconds_total == pytest.approx(10.0)
    assert totals.coalescable_wall_seconds_probe == pytest.approx(10.0)
    assert totals.coalescable_wall_seconds_real == pytest.approx(0.0)
    assert summary.identities[0].classification == "probe"

    markdown = tool.render_markdown(summary)
    assert "negligible" in markdown
    assert "soldr#3045" in markdown
    assert "-dM" in markdown
    # No unrendered format placeholders survived into the comment body.
    assert "{" not in markdown
    assert "}" not in markdown


def test_main_returns_zero_and_writes_totals_and_identities(tmp_path: Path) -> None:
    records = [
        _journal_record(
            args=["-c", "-o", "foo.o", "src/foo.c"],
            compiler=NATIVE_CC,
            context_key="ctx-main",
            ts="2026-09-17T19:00:10.000Z",
        )
    ]
    _write_journal(tmp_path, records)
    out_path = tmp_path / "summary.json"
    code = tool.main([str(tmp_path), "--json-out", str(out_path)])
    assert code == 0
    written = json.loads(out_path.read_text(encoding="utf-8"))
    assert "totals" in written
    assert "identities" in written


def test_main_over_nonexistent_path_returns_one() -> None:
    assert tool.main(["/nonexistent/path"]) == 1
