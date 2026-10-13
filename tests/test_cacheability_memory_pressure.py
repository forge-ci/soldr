"""The cacheability lane's memory-pressure evidence (soldr#2781 / soldr#2817).

`Nextest Cacheability` has failed every run since 2026-08-10, repeatedly with
a compiler killed by a signal, and soldr's own message says only that this
"can indicate an OOM/resource-limit kill". Three separate triages stopped at
"can indicate" -- including one that checked `dmesg` and found nothing, which
is a different question from whether the *cgroup* recorded a kill.

The container's cgroup answers it, and answers it in both directions: cgroup
v2's `memory.events` counts every process in the cgroup killed by any OOM
killer, so a zero rules memory out rather than merely failing to confirm it.

These tests cover that reader, and the phase-attribution bug that made the
lane misreport where it died, without needing Docker or the 40-minute run.
"""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
_SCRIPT = REPO_ROOT / "ci" / "assert_nextest_archive_cacheability.py"
cacheability = load_script_module(_SCRIPT, "cacheability_memory_pressure")

memory_pressure_lines = cacheability.memory_pressure_lines
parse_memory_events = cacheability.parse_memory_events
oom_verdict = cacheability.oom_verdict
PhaseTracker = cacheability.PhaseTracker


def write_cgroup(root: Path, **files: str) -> Path:
    """Build a fake cgroup dir; ``memory_max=`` writes ``memory.max``."""
    root.mkdir(parents=True, exist_ok=True)
    for name, body in files.items():
        (root / name.replace("_", ".", 1)).write_text(body, encoding="utf-8")
    return root


MEMINFO = "MemTotal:       16384000 kB\nMemAvailable:    2048000 kB\nSwapTotal:  0 kB\n"


def test_events_are_parsed_into_counts() -> None:
    parsed = parse_memory_events("low 0\nhigh 12\nmax 412\noom 3\noom_kill 1\n")
    assert [(e.name, e.count) for e in parsed] == [
        ("low", 0),
        ("high", 12),
        ("max", 412),
        ("oom", 3),
        ("oom_kill", 1),
    ]


def test_unparsable_event_lines_are_skipped_not_raised() -> None:
    # This runs while something else is already failing; a malformed line must
    # not become a second traceback stacked on the first.
    parsed = parse_memory_events("oom_kill notanumber\nhigh\n\noom_kill 2\n")
    assert [(e.name, e.count) for e in parsed] == [("oom_kill", 2)]


def test_a_recorded_kill_is_stated_as_a_determination() -> None:
    verdict = oom_verdict(parse_memory_events("oom_kill 1"), events_readable=True)
    assert "IS a memory kill" in verdict
    assert "1 process" in verdict


def test_group_kills_count_toward_the_verdict() -> None:
    # oom_group_kill is how a cgroup with memory.oom.group set reports it; a
    # reader that only looked at oom_kill would call that case "no OOM".
    assert "IS a memory kill" in oom_verdict(
        parse_memory_events("oom_kill 0\noom_group_kill 1"), events_readable=True
    )


def test_zero_kills_rules_memory_out_rather_than_staying_silent() -> None:
    verdict = oom_verdict(parse_memory_events("oom_kill 0"), events_readable=True)
    assert "NOT the memory limit" in verdict
    assert "look elsewhere" in verdict


def test_an_unreadable_events_file_says_unknown_not_no_kill() -> None:
    # The failure mode this guards: reporting "no OOM kill" because the file
    # could not be read would be a false exoneration, which is worse than the
    # hedge it replaces.
    verdict = oom_verdict(parse_memory_events(None), events_readable=False)
    assert "unknown" in verdict
    assert "NOT the memory limit" not in verdict


def test_the_snapshot_reports_the_limit_the_peak_and_the_verdict(
    tmp_path: Path,
) -> None:
    root = write_cgroup(
        tmp_path / "cg",
        memory_max="8000000000\n",
        memory_peak="7900000000\n",
        memory_high="max\n",
        memory_events="oom_kill 1\nmax 40\n",
    )
    meminfo = tmp_path / "meminfo"
    meminfo.write_text(MEMINFO, encoding="utf-8")

    rendered = "\n".join(
        memory_pressure_lines(
            "failure", cgroup_root=root, meminfo=meminfo, environ={"SOLDR_JOBS": "2"}
        )
    )

    assert "## failure memory pressure" in rendered
    assert "7.5 GiB (8000000000)" in rendered
    assert "memory.peak" in rendered
    assert "oom_kill=1" in rendered
    assert "MemAvailable" in rendered
    assert "SOLDR_JOBS: 2" in rendered
    assert "IS a memory kill" in rendered


def test_an_unset_concurrency_var_is_named_rather_than_omitted(
    tmp_path: Path,
) -> None:
    # "SOLDR_JOBS: (unset)" and a missing line read the same to a grep but not
    # to a person: the limit is only interpretable next to the job count.
    root = write_cgroup(tmp_path / "cg", memory_max="max\n")
    rendered = "\n".join(
        memory_pressure_lines(
            "startup", cgroup_root=root, meminfo=tmp_path / "nope", environ={}
        )
    )
    assert "SOLDR_JOBS: (unset)" in rendered
    assert "no cgroup limit" in rendered


def test_a_missing_cgroup_degrades_to_a_note(tmp_path: Path) -> None:
    absent = tmp_path / "absent"
    absent.mkdir()
    rendered = "\n".join(
        memory_pressure_lines(
            "startup", cgroup_root=absent, meminfo=tmp_path / "nope", environ={}
        )
    )
    assert "not cgroup v2" in rendered
    assert "unknown" in rendered
    assert "(unreadable)" in rendered


def test_the_failing_phase_survives_the_diagnostics_that_explain_it() -> None:
    """soldr#2817's mis-attribution, as observed in run 32893551296.

    That run died compiling `soldr-cli` inside `cold nextest archive build`
    and reported `failed during phase: retained diagnostic files` -- the last
    section the failure trap printed. The trap's own output was overwriting
    the fact the tracker exists to preserve.
    """
    tracker = PhaseTracker()
    tracker.feed("## cold nextest archive build\n")
    tracker.feed("## post-failure diagnostics\n")
    tracker.feed("## failure memory pressure\n")
    tracker.feed("## soldr daemon diagnostics\n")
    tracker.feed("## retained diagnostic files\n")

    assert tracker.failed == "cold nextest archive build"


def test_without_a_failure_the_frozen_phase_stays_unset() -> None:
    tracker = PhaseTracker()
    tracker.feed("## cold nextest archive build\n")
    tracker.feed("## warm nextest archive build\n")
    assert tracker.failed is None


def test_only_the_first_diagnostics_sentinel_freezes_the_blame() -> None:
    # ensure_soldr_daemon and the EXIT trap can both print the trap's block in
    # one run; the second must not re-blame a diagnostic section.
    tracker = PhaseTracker()
    tracker.feed("## ensure soldr daemon\n")
    tracker.feed("## post-failure diagnostics\n")
    tracker.feed("## retained diagnostic files\n")
    tracker.feed("## post-failure diagnostics\n")
    assert tracker.failed == "ensure soldr daemon"


def test_the_harness_announces_the_sentinel_before_any_diagnostic_section() -> None:
    """The tracker's freeze is worthless if the harness never says the word."""
    script = cacheability.BASH_SCRIPT
    sentinel = 'echo "## ' + PhaseTracker.DIAGNOSTICS_MARKER + '" >&2'
    assert sentinel in script
    body = script.split("print_daemon_diagnostics() {", 1)[1]
    assert body.index(sentinel) < body.index('echo "## soldr daemon diagnostics"')


def test_the_harness_captures_memory_pressure_at_start_and_on_failure() -> None:
    script = cacheability.BASH_SCRIPT
    assert "--memory-pressure" in script
    assert "report_memory_pressure startup" in script
    assert "report_memory_pressure failure" in script
    # Bash resolves functions at call time, but the startup call runs long
    # before the diagnostics block; the definition has to precede it.
    assert script.index("report_memory_pressure() {") < script.index(
        "report_memory_pressure startup"
    )


def test_the_summary_reads_the_frozen_phase_not_the_open_one() -> None:
    """Freezing the phase is inert unless the reporting path uses it.

    Source-level for the same reason the BASH_SCRIPT assertions above are:
    `main` owns a Docker run and a 40-minute subprocess, and the one line
    that matters here is which attribute it blames.
    """
    import inspect

    source = inspect.getsource(cacheability.main)
    assert "tracker.failed or tracker.current" in source
