"""Unit tests for the cacheability harness's miss-reason reporting (soldr#2824).

The acceptance needs Docker and ~40 minutes; this covers the part that does
not. The split matters more here than usual, because the thing under test *is*
the diagnosis machinery -- and its predecessor shipped broken precisely because
nobody could afford 40 minutes to find out that the group it printed was empty.

soldr#2825 added the list of units that missed and closed it with the line
"the per-unit reason is in the compile journal named above". No journal was
named anywhere in the harness, so that pointed at nothing, and the Actions
group under it contained only its own header. The reason was never missing --
`soldr cache report --json` already carried the journal path, zccache's
analysis of it, the staged counters and any diagnoses. The harness captured the
whole report and read four integers out of it.

The fixtures below mirror a real `soldr cache report --json`, checked against
live output rather than written from the struct definition.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
_SCRIPT = REPO_ROOT / "ci" / "assert_nextest_archive_cacheability.py"
cacheability = load_script_module(_SCRIPT, "cacheability_evidence")

explain_report = cacheability.explain_report
emit_report_explanation = cacheability.emit_report_explanation


def report(**overrides: Any) -> object:
    """A report shaped like the real thing, with the parts under test filled.

    The harness parses `cache report --json` into an untyped JSON document, so
    the fixture is one too -- a raw-JSON boundary, not a record.
    """
    base = {
        "command": "cache report",
        "journal_path": "/root/.soldr/cache/zccache/history/42/last-session.jsonl",
        "journal_present": True,
        "diagnoses": [],
        "notes": [],
        "rollups": None,
        "last_session": {
            "hits": 489,
            "misses": 108,
            "phase_profile": {
                "staged": {
                    # Deliberately NOT in descending order. With the fixture
                    # already sorted, dict insertion order matched the
                    # expected output and the ordering assertion passed even
                    # with the sort deleted -- caught by mutating it out.
                    "counters": {
                        "publication_failure": 108,
                        "plan_unsupported": 0,
                        "plan_attempted": 597,
                        "materialize_failure": 0,
                        "publication_success": 489,
                        "plan_enabled": 597,
                    }
                }
            },
        },
    }
    base.update(overrides)
    return base


def rendered(**overrides: Any) -> str:
    return "\n".join(explain_report("warm", report(**overrides)))


# ------------------------------- the counters -------------------------------


def test_non_zero_counters_are_shown_largest_first() -> None:
    """The counters are the closest thing to a per-unit reason.

    `publication_failure` says a unit compiled and never became durable, which
    is the shape a warm miss takes; ordering by size puts it where it will be
    read.
    """
    lines = explain_report("warm", report())
    counters = [line for line in lines if " = " in line]
    assert counters == [
        "  plan_attempted = 597",
        "  plan_enabled = 597",
        "  publication_success = 489",
        "  publication_failure = 108",
    ], counters


def test_zero_counters_are_omitted() -> None:
    text = rendered()
    assert "plan_unsupported" not in text
    assert "materialize_failure" not in text


def test_all_zero_counters_say_so_rather_than_printing_nothing() -> None:
    """An empty group is what this change exists to remove.

    Printing a header with no body under it is indistinguishable from the tool
    being broken, which is exactly how the previous version failed.
    """
    session = {"phase_profile": {"staged": {"counters": {"plan_attempted": 0}}}}
    text = "\n".join(explain_report("warm", report(last_session=session)))
    assert "(every counter is zero)" in text


# --------------------------- degrading, not raising --------------------------


def test_a_moved_counter_shape_degrades_to_a_note() -> None:
    """`last_session` is passed through verbatim from zccache and its shape
    moves across protocol versions -- `cache/report.rs` says so in as many
    words. This runs while something is already failing, so a shape change
    must not raise a second error on top of the first.
    """
    for session in (
        {},
        {"phase_profile": None},
        {"phase_profile": {"staged": None}},
        {"phase_profile": {"staged": {"counters": "not-a-dict"}}},
    ):
        text = "\n".join(explain_report("warm", report(last_session=session)))
        assert "the shape may have moved" in text, session


def test_a_missing_last_session_degrades_to_a_note() -> None:
    text = "\n".join(explain_report("warm", report(last_session=None)))
    assert "(no last_session in the report)" in text


# -------------------------------- diagnoses ---------------------------------


def test_a_diagnosis_is_rendered_with_severity_kind_and_message() -> None:
    """The live one, copied from a real report."""
    diagnosis = {
        "kind": "cache_publication_failed",
        "severity": "warning",
        "message": (
            "cacheable compilations succeeded but none became durable; "
            "the cache will not warm"
        ),
    }
    text = rendered(diagnoses=[diagnosis])
    assert (
        "  [warning] cache_publication_failed: cacheable compilations "
        "succeeded but none became durable; the cache will not warm"
    ) in text


def test_absent_diagnoses_and_notes_say_none() -> None:
    text = rendered()
    assert text.count("  (none)") == 2, text


def test_a_malformed_diagnosis_is_printed_rather_than_dropped() -> None:
    text = rendered(diagnoses=["just a string"])
    assert "just a string" in text


# --------------------------------- rollups ----------------------------------


def test_null_rollups_is_distinguished_from_an_absent_key() -> None:
    """A null with a note explaining why is a different failure from a missing
    key, and the notes carry that explanation."""
    text = rendered(
        rollups=None,
        notes=["rollups: journal missing - soldr writes it on cache-enabled builds"],
    )
    assert "null -- see notes above" in text
    assert "journal missing" in text


def test_rollups_are_rendered_and_capped() -> None:
    """Unbounded output would bury the counters above it."""
    big = {f"unit_{index:03d}": {"misses": index} for index in range(200)}
    text = rendered(rollups=big)
    assert "... (truncated)" in text
    body = [line for line in text.splitlines() if line.startswith("  ")]
    assert len(body) < 120, len(body)


def test_small_rollups_are_not_truncated() -> None:
    text = rendered(rollups={"units": 2})
    assert "... (truncated)" not in text
    assert '"units": 2' in text


# ------------------------------- the journal --------------------------------


def test_the_journal_is_actually_named() -> None:
    """The whole point: soldr#2825 said "the journal named above" and named
    none. It is in the report, under `journal_path`."""
    text = rendered()
    assert "/root/.soldr/cache/zccache/history/42/last-session.jsonl" in text
    assert "journal_present: True" in text


# ------------------------------ the file layer ------------------------------


def test_a_missing_report_file_is_reported_not_raised(tmp_path, capsys) -> None:
    """This runs on the failure path. It must never be the thing that fails."""
    assert emit_report_explanation("cold", str(tmp_path / "nope.json")) == 0
    assert "evidence unavailable" in capsys.readouterr().out


def test_malformed_json_is_reported_not_raised(tmp_path, capsys) -> None:
    path = tmp_path / "report.json"
    path.write_text("{not json", encoding="utf-8")
    assert emit_report_explanation("cold", str(path)) == 0
    assert "evidence unavailable" in capsys.readouterr().out


def test_a_non_object_report_is_reported_not_raised(tmp_path, capsys) -> None:
    path = tmp_path / "report.json"
    path.write_text("[1, 2, 3]", encoding="utf-8")
    assert emit_report_explanation("cold", str(path)) == 0
    assert "is not an object" in capsys.readouterr().out


def test_a_real_shaped_report_round_trips_through_the_file_layer(
    tmp_path, capsys
) -> None:
    path = tmp_path / "report.json"
    path.write_text(json.dumps(report()), encoding="utf-8")
    assert emit_report_explanation("cold", str(path)) == 0
    out = capsys.readouterr().out
    assert "## cold report evidence" in out
    assert "publication_failure = 108" in out


# ---------------------------- the harness wiring ----------------------------


def harness_source() -> str:
    return _SCRIPT.read_text(encoding="utf-8")


def test_the_dead_pointer_is_no_longer_printed() -> None:
    """It must not be *echoed*; the comment explaining why still quotes it.

    Matching the bare sentence would forbid describing the bug in a comment,
    which is the opposite of what is wanted -- the next reader should find out
    why the group exists without going to the issue tracker.
    """
    assert (
        'echo "## the per-unit reason is in the compile journal named above"'
        not in harness_source()
    )


def test_the_evidence_path_explains_both_reports() -> None:
    """A warm miss is usually a *cold* publication failure, so the cold report
    is as load-bearing as the warm one. Neither may be quietly dropped.

    soldr#2937 collapsed what used to be two verdict-rendering exit paths into
    one evidence path: the shell no longer decides whether a miss is a
    regression (that moved to `evaluate_warm_result`, where it is testable
    without paying 40 minutes to find out it was wrong), so there is one block
    that emits evidence rather than two that each emitted a verdict. This
    asserts the same thing the two-path version did -- both reports get
    explained, together -- against the shape the harness now has.
    """
    source = harness_source()
    for report_name in ("/tmp/cold-report.json", "/tmp/warm-report.json"):
        assert (
            source.count(f"explain_report cold {report_name}")
            + source.count(f"explain_report warm {report_name}")
            == 1
        ), report_name

    # Both explanations live in the same block, so an edit cannot keep one and
    # silently drop the other onto a path that never runs.
    cold_at = source.index("explain_report cold /tmp/cold-report.json")
    warm_at = source.index("explain_report warm /tmp/warm-report.json")
    between = source[min(cold_at, warm_at) : max(cold_at, warm_at)]
    assert "\nif " not in between and "\nfi" not in between, (
        "the cold and warm explanations drifted onto different branches"
    )

    # And that block must still fire on both conditions the old pair covered:
    # a warm miss, and a warm run that recorded no hits at all.
    guard = source[:cold_at].rsplit("\nif ", 1)[-1]
    assert "warm_misses != 0" in guard, guard
    assert "warm_hits <= 0" in guard, guard


def test_the_explainer_runs_before_the_docker_check() -> None:
    """It runs inside the container, where there is no Docker daemon."""
    source = harness_source()
    explain_at = source.index("if args.explain_report is not None:")
    docker_at = source.index("if not docker_available():")
    assert explain_at < docker_at
