"""Fast coverage for the soldr#2878 N=1/N=2/raised-count evidence runner."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
telemetry = load_script_module(
    REPO_ROOT / "ci" / "cargo_orchestration_telemetry.py",
    "cargo_orchestration_telemetry",
)


def write_cgroup(root: Path, **files: str) -> Path:
    root.mkdir(parents=True, exist_ok=True)
    for name, contents in files.items():
        (root / name.replace("_", ".", 1)).write_text(contents, encoding="utf-8")
    return root


def test_snapshot_reads_transient_resource_inputs_and_process_census(
    tmp_path: Path,
) -> None:
    cgroup = write_cgroup(
        tmp_path / "cgroup",
        memory_current="1073741824\n",
        memory_peak="3221225472\n",
        memory_swap_current="0\n",
        pids_current="37\n",
        memory_events="oom_kill 1\noom_group_kill 2\n",
    )
    proc = tmp_path / "proc"
    for pid, command in {"11": "cargo", "12": "rustc", "13": "soldr-daemon"}.items():
        directory = proc / pid
        directory.mkdir(parents=True)
        (directory / "comm").write_text(command, encoding="utf-8")

    sample = telemetry.snapshot(cgroup, proc, clock=lambda: 42.5)

    assert sample.monotonic_seconds == 42.5
    assert sample.memory_current_bytes == 1024**3
    assert sample.memory_peak_bytes == 3 * 1024**3
    assert sample.pids_current == 37
    assert sample.memory_events.get("oom_kill") == 1
    assert sample.memory_events.get("oom_group_kill") == 2
    assert sample.processes.cargo == 1
    assert sample.processes.compiler == 1
    assert sample.processes.soldr == 1
    assert sample.processes.toolchain == 3


def test_nested_membership_resolves_below_the_process_visible_cgroup_mount() -> None:
    resolved = telemetry.cgroup_v2_dir_from(
        "0::/runner/step-12\n",
        "36 25 0:32 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
    )

    assert resolved == Path("/sys/fs/cgroup/runner/step-12")


def test_mount_subtree_is_removed_before_joining_the_membership_path() -> None:
    resolved = telemetry.cgroup_v2_dir_from(
        "0::/delegated/job/step\n",
        "36 25 0:32 /delegated /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
    )

    assert resolved == Path("/sys/fs/cgroup/job/step")


def test_controlling_resolver_reads_injected_proc_membership_and_mountinfo(
    tmp_path: Path,
) -> None:
    membership = tmp_path / "self.cgroup"
    mountinfo = tmp_path / "self.mountinfo"
    membership.write_text("0::/actions/job-42/step\n", encoding="utf-8")
    mountinfo.write_text(
        "36 25 0:32 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
        encoding="utf-8",
    )

    assert telemetry.controlling_cgroup_v2_dir(membership, mountinfo) == Path(
        "/sys/fs/cgroup/actions/job-42/step"
    )


def test_missing_v2_membership_does_not_fall_back_to_a_parent_cgroup() -> None:
    assert (
        telemetry.cgroup_v2_dir_from(
            "0::/runner/step\n",
            "36 25 0:32 / /sys/fs/cgroup rw - tmpfs tmpfs rw\n",
        )
        is None
    )


def test_matrix_requires_an_explicit_opt_in_before_running_raised_count(
    tmp_path: Path,
) -> None:
    with pytest.raises(SystemExit):
        telemetry.parse_args(["--raised-jobs", "8", "--", "soldr", "cargo", "check"])

    parsed = telemetry.parse_args(
        [
            "--raised-jobs",
            "8",
            "--allow-raised-count",
            "--case-root",
            str(tmp_path / "fresh-cases"),
            "--",
            "soldr",
            "cargo",
            "check",
        ]
    )

    assert parsed.jobs == [1, 2, 8]
    assert parsed.case_root == tmp_path / "fresh-cases"
    assert parsed.command == ["soldr", "cargo", "check"]


def test_case_uses_a_fresh_target_and_soldr_cache_tree(tmp_path: Path) -> None:
    cgroup = write_cgroup(
        tmp_path / "cgroup",
        memory_current="1\n",
        memory_peak="1\n",
        memory_swap_current="0\n",
        pids_current="1\n",
        memory_events="oom_kill 0\n",
    )
    case_root = tmp_path / "jobs-2"

    result = telemetry.run_case(
        2,
        [sys.executable, "-c", "print('case output')"],
        case_root=case_root,
        cgroup_root=cgroup,
        interval_seconds=0.001,
    )

    assert result.returncode == 0
    assert result.case_root == str(case_root)
    assert result.command_log == str(case_root / "command.log")
    assert (case_root / "target").is_dir()
    assert (case_root / "soldr-cache").is_dir()
    assert (case_root / "command.log").read_text(encoding="utf-8") == "case output\n"


def test_case_isolates_session_cache_and_stops_its_daemon(tmp_path: Path) -> None:
    cgroup = write_cgroup(
        tmp_path / "cgroup",
        memory_current="1\n",
        memory_peak="1\n",
        memory_swap_current="0\n",
        pids_current="1\n",
        memory_events="oom_kill 0\n",
    )
    case_root = tmp_path / "jobs-1"
    keys = (
        "CARGO_TARGET_DIR",
        "SOLDR_CACHE_DIR",
        "ZCCACHE_CACHE_DIR",
        "SOLDR_CACHE_LIFECYCLE",
        "SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS",
    )
    expression = (
        "import os; print('\\n'.join(os.environ[key] for key in " + repr(keys) + "))"
    )

    telemetry.run_case(
        1,
        [sys.executable, "-c", expression],
        case_root=case_root,
        cgroup_root=cgroup,
        interval_seconds=0.001,
    )

    assert (case_root / "command.log").read_text(encoding="utf-8").splitlines() == [
        str(case_root / "target"),
        str(case_root / "soldr-cache"),
        str(case_root / "soldr-cache" / "cache" / "zccache"),
        "command",
        "30",
    ]


def test_prepared_toolchain_preflight_requires_cargo_proxy_or_rustup(
    tmp_path: Path,
) -> None:
    environment = {"CARGO_HOME": str(tmp_path / "cargo-home"), "PATH": ""}

    assert not telemetry.prepared_cargo_or_rustup_available(environment)

    cargo = tmp_path / "cargo-home" / "bin" / "cargo"
    cargo.parent.mkdir(parents=True)
    cargo.write_text("cargo proxy", encoding="utf-8")

    assert telemetry.prepared_cargo_or_rustup_available(environment)


def test_summary_uses_sampled_peak_not_only_post_failure_state() -> None:
    baseline = telemetry.Snapshot(
        monotonic_seconds=0,
        memory_current_bytes=1,
        memory_peak_bytes=9,
        memory_swap_current_bytes=0,
        pids_current=2,
        memory_events=telemetry.MemoryEvents(),
        processes=telemetry.ProcessCounts(0, 0, 0, 0),
    )
    transient = telemetry.Snapshot(
        monotonic_seconds=1,
        memory_current_bytes=8,
        memory_peak_bytes=9,
        memory_swap_current_bytes=3,
        pids_current=12,
        memory_events=telemetry.MemoryEvents(),
        processes=telemetry.ProcessCounts(2, 4, 1, 7),
    )
    post_failure = telemetry.Snapshot(
        monotonic_seconds=2,
        memory_current_bytes=1,
        memory_peak_bytes=9,
        memory_swap_current_bytes=0,
        pids_current=2,
        memory_events=telemetry.MemoryEvents(),
        processes=telemetry.ProcessCounts(0, 0, 0, 0),
    )

    maxima = telemetry.summarize_samples([baseline, transient, post_failure])

    assert maxima.max_memory_current_bytes == 8
    assert maxima.max_pids_current == 12
    assert maxima.max_cargo_processes == 2
    assert maxima.max_compiler_processes == 4
    assert maxima.max_toolchain_processes == 7


def test_parse_jobs_accepts_auto_alongside_positive_ints_and_dedupes() -> None:
    assert telemetry.parse_jobs("1,auto,2,auto") == [1, "auto", 2]

    with pytest.raises(argparse.ArgumentTypeError):
        telemetry.parse_jobs("bogus")


def test_run_case_auto_removes_job_env_and_marks_the_telemetry_var(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("CARGO_BUILD_JOBS", "4")
    monkeypatch.setenv("SOLDR_JOBS", "4")
    cgroup = write_cgroup(
        tmp_path / "cgroup",
        memory_current="1\n",
        memory_peak="1\n",
        memory_swap_current="0\n",
        pids_current="1\n",
        memory_events="oom_kill 0\n",
    )
    case_root = tmp_path / "jobs-auto"
    expression = (
        "import os; "
        "assert 'CARGO_BUILD_JOBS' not in os.environ; "
        "assert 'SOLDR_JOBS' not in os.environ; "
        "print(os.environ['SOLDR_CI_ORCHESTRATION_TELEMETRY_JOBS'])"
    )

    result = telemetry.run_case(
        "auto",
        [sys.executable, "-c", expression],
        case_root=case_root,
        cgroup_root=cgroup,
        interval_seconds=0.001,
    )

    assert result.returncode == 0
    assert result.requested_jobs == "auto"
    assert (case_root / "command.log").read_text(encoding="utf-8") == "auto\n"


def test_run_case_explicit_int_still_stamps_cargo_build_jobs(
    tmp_path: Path,
) -> None:
    cgroup = write_cgroup(
        tmp_path / "cgroup",
        memory_current="1\n",
        memory_peak="1\n",
        memory_swap_current="0\n",
        pids_current="1\n",
        memory_events="oom_kill 0\n",
    )
    case_root = tmp_path / "jobs-3"
    expression = "import os; print(os.environ['CARGO_BUILD_JOBS'])"

    result = telemetry.run_case(
        3,
        [sys.executable, "-c", expression],
        case_root=case_root,
        cgroup_root=cgroup,
        interval_seconds=0.001,
    )

    assert result.requested_jobs == 3
    assert (case_root / "command.log").read_text(encoding="utf-8") == "3\n"


def test_classify_failure_phase() -> None:
    assert telemetry.classify_failure_phase(0, False, None) == "none"
    assert telemetry.classify_failure_phase(1, False, None) == "pre_compiler"
    assert telemetry.classify_failure_phase(1, False, 12) == "compiler"
    assert telemetry.classify_failure_phase(0, True, None) == "pre_compiler"


def test_log_reports_enomem(tmp_path: Path) -> None:
    clean = tmp_path / "clean.log"
    clean.write_text("build succeeded\n", encoding="utf-8")
    assert not telemetry.log_reports_enomem(clean)

    noisy = tmp_path / "noisy.log"
    noisy.write_text(
        "failed to spawn: Cannot allocate memory (os error 12)\n", encoding="utf-8"
    )
    assert telemetry.log_reports_enomem(noisy)

    assert not telemetry.log_reports_enomem(tmp_path / "missing.log")


def test_run_case_classifies_pre_compiler_enomem_failure(tmp_path: Path) -> None:
    cgroup = write_cgroup(
        tmp_path / "cgroup",
        memory_current="1\n",
        memory_peak="1\n",
        memory_swap_current="0\n",
        pids_current="1\n",
        memory_events="oom_kill 0\n",
    )
    case_root = tmp_path / "jobs-1"
    # An empty fake /proc keeps unrelated host compilers (e.g. a concurrent
    # ci-test rustc) from being counted as this case's compiler phase.
    empty_proc = tmp_path / "proc"
    empty_proc.mkdir()
    expression = (
        "import sys; "
        "print('failed to spawn: Cannot allocate memory (os error 12)'); "
        "sys.exit(101)"
    )

    result = telemetry.run_case(
        1,
        [sys.executable, "-c", expression],
        case_root=case_root,
        cgroup_root=cgroup,
        proc_root=empty_proc,
        interval_seconds=0.001,
    )

    assert result.returncode == 101
    assert result.first_compiler_offset_ms is None
    assert result.failure_phase == "pre_compiler"
    assert result.enomem_in_log is True


def test_markdown_includes_the_required_cross_run_metrics() -> None:
    snapshot = telemetry.Snapshot(
        monotonic_seconds=0,
        memory_current_bytes=None,
        memory_peak_bytes=None,
        memory_swap_current_bytes=None,
        pids_current=None,
        memory_events=telemetry.MemoryEvents(),
        processes=telemetry.ProcessCounts(0, 0, 0, 0),
    )
    rendered = telemetry.format_markdown(
        [
            telemetry.CaseResult(
                requested_jobs=8,
                case_root="/case",
                command_log="/case/command.log",
                returncode=1,
                timed_out=False,
                wall_time_ms=1234,
                start=snapshot,
                end=snapshot,
                memory_events_delta=telemetry.MemoryEvents(
                    (telemetry.MemoryEvent("oom_kill", 1),)
                ),
                maxima=telemetry.Maxima(
                    max_memory_current_bytes=9,
                    max_memory_peak_bytes=None,
                    max_memory_swap_current_bytes=None,
                    max_pids_current=12,
                    max_cargo_processes=2,
                    max_compiler_processes=3,
                    max_soldr_processes=1,
                    max_toolchain_processes=6,
                ),
                samples=2,
                first_compiler_offset_ms=None,
                failure_phase="?",
                enomem_in_log=False,
            )
        ]
    )

    assert "max current" in rendered
    assert "max PIDs" in rendered
    assert "max Cargo/compiler/tools" in rendered
    assert "phase" in rendered
    assert "ENOMEM" in rendered
    assert "| 8 | 1 | ? | no | 1234 ms | 9 | 12 | 2/3/6 | 1 |" in rendered
