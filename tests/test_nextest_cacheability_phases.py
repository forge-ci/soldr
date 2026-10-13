"""Unit tests for the cacheability acceptance's phase tracking (soldr#1978 item 4).

The acceptance itself needs Docker and ~40 minutes; these cover the part that
does not -- turning the harness's ``## <name>`` markers into observable,
timed phases. That split is deliberate: the reason item 4 exists is that a
40-minute opaque step is undiagnosable, so the diagnosis machinery must be
verifiable without paying 40 minutes to find out it is wrong.
"""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
_SCRIPT = REPO_ROOT / "ci" / "assert_nextest_archive_cacheability.py"
cacheability = load_script_module(_SCRIPT, "cacheability_acceptance")

PhaseTracker = cacheability.PhaseTracker
format_duration = cacheability.format_duration


class FakeClock:
    """Monotonic clock the test drives by hand."""

    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def test_markers_become_phases_with_durations() -> None:
    clock = FakeClock()
    tracker = PhaseTracker(clock=clock)

    tracker.feed("## bootstrap soldr\n")
    clock.advance(12)
    tracker.feed("## cold nextest archive build\n")
    clock.advance(600)
    tracker.feed("## warm nextest archive build after cargo clean\n")
    clock.advance(90)
    tracker.finish()

    assert [(p.name, p.seconds) for p in tracker.phases] == [
        ("bootstrap soldr", 12),
        ("cold nextest archive build", 600),
        ("warm nextest archive build after cargo clean", 90),
    ]


def test_ordinary_output_is_not_a_phase() -> None:
    # The harness prints plenty of lines; only the `## ` announcements are
    # stage boundaries. Treating any other line as one would shred the
    # timings into noise.
    tracker = PhaseTracker(clock=FakeClock())
    for line in (
        "   Compiling serde v1.0.229\n",
        "CACHEABILITY_RESULT {}\n",
        "#not a marker\n",
        "##\n",  # marker with no name
    ):
        assert tracker.feed(line) is None
    assert tracker.phases == []
    assert tracker.current is None


def test_groups_are_closed_before_the_next_opens() -> None:
    # Actions nests groups if an ::endgroup:: is missing, and a nested group
    # renders as one uncollapsible blob -- i.e. exactly the opaque step this
    # change exists to remove.
    tracker = PhaseTracker(clock=FakeClock(), emit_groups=True)

    first = tracker.feed("## bootstrap soldr\n")
    assert first == "::group::bootstrap soldr"
    assert "::endgroup::" not in first

    second = tracker.feed("## cold nextest archive build\n")
    assert second == "::endgroup::\n::group::cold nextest archive build"


def test_grouping_markers_are_suppressed_off_actions() -> None:
    tracker = PhaseTracker(clock=FakeClock(), emit_groups=False)
    assert tracker.feed("## bootstrap soldr\n") is None
    assert tracker.feed("## cold nextest archive build\n") is None
    # Timings are still collected -- only the Actions control lines are off.
    tracker.finish()
    assert [phase.name for phase in tracker.phases] == [
        "bootstrap soldr",
        "cold nextest archive build",
    ]


def test_the_open_phase_is_reported_as_the_failure_site() -> None:
    # The single most useful fact on a failed run: which stage was running
    # when output stopped. Recovering it previously meant re-running ~40
    # minutes and watching.
    clock = FakeClock()
    tracker = PhaseTracker(clock=clock)
    tracker.feed("## bootstrap soldr\n")
    clock.advance(10)
    tracker.feed("## cold nextest archive build\n")
    clock.advance(300)

    assert tracker.current == "cold nextest archive build"
    summary = tracker.summary_markdown(failed_phase=tracker.current)
    assert "**Failed during:** `cold nextest archive build`" in summary


def test_finish_is_idempotent() -> None:
    # `main` closes the tracker in a `finally`, which can run after the
    # stream loop already closed it on the success path. Double-counting a
    # phase there would misreport every green run.
    clock = FakeClock()
    tracker = PhaseTracker(clock=clock)
    tracker.feed("## bootstrap soldr\n")
    clock.advance(5)
    tracker.finish()
    tracker.finish()
    assert [(p.name, p.seconds) for p in tracker.phases] == [("bootstrap soldr", 5)]


def test_summary_lists_every_phase_and_a_total() -> None:
    tracker = PhaseTracker(clock=FakeClock())
    tracker.record("docker build", 305)
    tracker.record("cold nextest archive build", 1200)
    summary = tracker.summary_markdown()

    assert "| docker build | 5m 05s |" in summary
    assert "| cold nextest archive build | 20m 00s |" in summary
    assert "| **total** | **25m 05s** |" in summary
    # No failure recorded, so no failure line.
    assert "Failed during" not in summary


def test_duration_formatting() -> None:
    assert format_duration(0) == "0s"
    assert format_duration(9) == "9s"
    assert format_duration(59) == "59s"
    assert format_duration(60) == "1m 00s"
    assert format_duration(2372) == "39m 32s"  # the step that motivated this


def test_summary_writer_is_a_noop_without_the_env_var(monkeypatch) -> None:
    # Local runs have no job summary; writing must not explode.
    monkeypatch.delenv("GITHUB_STEP_SUMMARY", raising=False)
    cacheability.write_step_summary("### nothing\n")


def test_summary_writer_appends(tmp_path, monkeypatch) -> None:
    target = tmp_path / "summary.md"
    target.write_text("existing\n", encoding="utf-8")
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(target))

    cacheability.write_step_summary("### Cacheability phases\n")

    contents = target.read_text(encoding="utf-8")
    assert contents.startswith("existing\n")
    assert "### Cacheability phases" in contents


def test_summary_writer_survives_an_unwritable_path(tmp_path, monkeypatch) -> None:
    # A job summary is a nicety; it must never turn a passing acceptance red.
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(tmp_path / "no-such-dir" / "s.md"))
    cacheability.write_step_summary("### Cacheability phases\n")
