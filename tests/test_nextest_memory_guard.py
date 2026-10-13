"""soldr#2885: memory-aware admission and memory-failure isolation in the
Nextest wrapper.

Every Unix test runs through the native ``soldr-nextest-wrapper``
(``crates/soldr-nextest-wrapper``, soldr#3454); its pure helpers -- signature
matching, tree sampling, cgroup bookkeeping, record names -- are unit-tested
in that crate.

``soldr ci-test`` hands the wrapper an admission directory, a per-test memory
ceiling and a one-line admission summary. These tests drive the real wrapper
as Nextest does -- ``wrapper <test-binary> <args>`` -- with bounded-memory
fixtures, so they pin observable behaviour rather than helper internals.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest
from conftest import nextest_wrapper_argv

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

MIB = 1024 * 1024
INFRA_EXIT = 75
# soldr ci-test's admission protocol (crates/soldr-cli/src/ci_test/test_pressure.rs).
PAUSED_FLAG = "paused"
POSIX = pytest.mark.skipif(
    os.name != "posix", reason="the wrapper runs Unix tests only"
)
LINUX = pytest.mark.skipif(
    not sys.platform.startswith("linux"), reason="exercises Linux kernel limits"
)

# Touches every page, so the allocation is resident rather than lazily mapped.
_HOG = """
import sys, time
hog = b"\\x01" * (int(sys.argv[1]) * 1024 * 1024)
print("allocated", flush=True)
time.sleep(float(sys.argv[2]))
"""


def _fill_env(env: dict[str, str], tmp_path: Path, **extra: str) -> None:
    env.update(
        {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("SOLDR_NEXTEST_") and not key.startswith("NEXTEST_")
        }
    )
    env.update(
        {
            "TMPDIR": str(tmp_path),
            "NEXTEST_BINARY_ID": "soldr-cli::fixture",
            "NEXTEST_TEST_NAME": "memory::fixture_test",
        }
    )
    env.update(extra)


def _run(
    args: list[str], env: dict[str, str], timeout: float = 60, **kwargs
) -> subprocess.CompletedProcess[str]:
    return run_captured(
        [*nextest_wrapper_argv(), sys.executable, *args],
        capture_output=True,
        text=True,
        env=env,
        timeout=timeout,
        check=False,
        **kwargs,
    )


def _admission_dir(tmp_path: Path) -> Path:
    directory = tmp_path / "admission"
    (directory / "active").mkdir(parents=True)
    return directory


# ---------------------------------------------------------------------------
# RED -> GREEN: an over-budget test tree fails alone, with a named diagnostic.
# ---------------------------------------------------------------------------


@POSIX
def test_over_ceiling_test_tree_is_killed_with_a_named_memory_diagnostic(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    env: dict[str, str] = {}
    _fill_env(
        env,
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(96 * MIB),
        SOLDR_NEXTEST_ADMISSION_SUMMARY=(
            "requested NEXTEST_TEST_THREADS=unset, effective=4 (measured)"
        ),
    )
    started = time.monotonic()
    result = _run(["-c", _HOG, "400", "30"], env)
    elapsed = time.monotonic() - started

    assert result.returncode == INFRA_EXIT, result.stderr
    assert elapsed < 20, "the ceiling must stop the tree, not wait for the test"
    stderr = result.stderr
    assert "nextest memory: infrastructure failure, not a test assertion" in stderr
    assert "test: soldr-cli::fixture memory::fixture_test" in stderr
    assert "per-test memory ceiling 96.0 MiB" in stderr
    assert "process-tree peak RSS" in stderr
    assert "requested NEXTEST_TEST_THREADS=unset, effective=4 (measured)" in stderr
    assert "memory available at admission" in stderr
    assert "memory available at failure" in stderr
    assert "pid pressure" in stderr
    # ci-test lists every infrastructure failure after Nextest exits.
    recorded = [path.name for path in (admission / "infra").iterdir()]
    assert recorded == ["soldr-cli::fixture memory::fixture_test"]


@POSIX
def test_ordinary_assertion_failure_is_not_labelled_a_memory_failure(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    env: dict[str, str] = {}
    _fill_env(
        env,
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(512 * MIB),
    )
    child = "import sys; print('assertion failed: left == right', file=sys.stderr); sys.exit(101)"
    result = _run(["-c", child], env)

    assert result.returncode == 101
    assert "nextest memory:" not in result.stderr
    assert not (admission / "infra").exists()


@POSIX
def test_well_behaved_neighbour_is_untouched_while_a_hog_is_killed(
    tmp_path: Path,
) -> None:
    nextest_wrapper_argv()  # skip here, not inside a worker thread
    admission = _admission_dir(tmp_path)
    env: dict[str, str] = {}
    _fill_env(
        env,
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(96 * MIB),
    )
    results: dict[str, subprocess.CompletedProcess[str]] = {}

    def run(name: str, megabytes: str) -> None:
        results[name] = _run(
            ["-c", _HOG, megabytes, "2"],
            {**env, "NEXTEST_TEST_NAME": f"memory::{name}"},
        )

    threads = [
        threading.Thread(target=run, args=("hog", "400")),
        threading.Thread(target=run, args=("neighbour", "8")),
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert results["hog"].returncode == INFRA_EXIT, results["hog"].stderr
    assert results["neighbour"].returncode == 0, results["neighbour"].stderr
    assert "nextest memory:" not in results["neighbour"].stderr
    assert [path.name for path in (admission / "infra").iterdir()] == [
        "soldr-cli::fixture memory::hog"
    ]


@LINUX
def test_enomem_inside_the_test_is_classified_as_infrastructure(
    tmp_path: Path,
) -> None:
    """The soldr#2885 shape: an allocation fails with ENOMEM mid-test."""

    admission = _admission_dir(tmp_path)
    env: dict[str, str] = {}
    _fill_env(env, tmp_path, SOLDR_NEXTEST_ADMISSION_DIR=str(admission))
    child = (
        "import resource\n"
        "resource.setrlimit(resource.RLIMIT_AS, (512 * 1024 * 1024,) * 2)\n"
        "hog = bytearray(2048 * 1024 * 1024)\n"
    )
    result = _run(["-c", child], env)

    assert result.returncode == INFRA_EXIT, result.stderr
    assert "MemoryError" in result.stderr, "the test's own output is preserved"
    assert (
        "nextest memory: infrastructure failure, not a test assertion" in result.stderr
    )
    assert "memory-exhaustion signature" in result.stderr
    assert "original exit status: 1" in result.stderr


@LINUX
def test_process_start_failure_is_named_as_pid_or_memory_pressure(
    tmp_path: Path,
) -> None:
    if os.geteuid() == 0:
        pytest.skip("RLIMIT_NPROC does not bind root")
    import resource  # pylint: disable=import-outside-toplevel  # Unix-only module

    env: dict[str, str] = {}

    _fill_env(env, tmp_path)

    def no_more_processes() -> None:
        resource.setrlimit(resource.RLIMIT_NPROC, (0, 0))

    result = _run(
        ["-c", "pass"],
        env,
        preexec_fn=no_more_processes,  # pylint: disable=subprocess-popen-preexec-fn
    )

    assert result.returncode == INFRA_EXIT, result.stderr
    assert (
        "nextest memory: infrastructure failure, not a test assertion" in result.stderr
    )
    assert "could not start the test process" in result.stderr
    assert "Traceback" not in result.stderr


# ---------------------------------------------------------------------------
# Admission gate: soldr ci-test's controller pauses and resumes admissions.
# ---------------------------------------------------------------------------


def _live_sleeper() -> subprocess.Popen[bytes]:
    return subprocess.Popen(  # pylint: disable=consider-using-with
        [sys.executable, "-c", "import time; time.sleep(60)"]
    )


@POSIX
def test_paused_admission_holds_a_new_test_until_pressure_clears(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    sleeper = _live_sleeper()
    try:
        (admission / "active" / str(sleeper.pid)).touch()
        (admission / PAUSED_FLAG).touch()
        env: dict[str, str] = {}
        _fill_env(
            env,
            tmp_path,
            SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
            SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS="30",
        )
        cleared_at: list[float] = []

        def clear_later() -> None:
            time.sleep(1.5)
            cleared_at.append(time.time())
            (admission / PAUSED_FLAG).unlink()

        clearer = threading.Thread(target=clear_later)
        clearer.start()
        result = _run(["-c", "import time; print(time.time())"], env)
        clearer.join()
    finally:
        sleeper.kill()
        sleeper.wait()

    assert result.returncode == 0, result.stderr
    started = float(result.stdout.split()[0])
    assert started >= cleared_at[0], "the test started while admission was paused"
    assert "admission paused by memory pressure" in result.stderr


@POSIX
def test_paused_admission_is_bounded_and_says_so(tmp_path: Path) -> None:
    admission = _admission_dir(tmp_path)
    sleeper = _live_sleeper()
    try:
        (admission / "active" / str(sleeper.pid)).touch()
        (admission / PAUSED_FLAG).touch()
        env: dict[str, str] = {}
        _fill_env(
            env,
            tmp_path,
            SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
            SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS="1",
        )
        started = time.monotonic()
        result = _run(["-c", "pass"], env)
        elapsed = time.monotonic() - started
    finally:
        sleeper.kill()
        sleeper.wait()

    assert result.returncode == 0, result.stderr
    assert 1.0 <= elapsed < 15
    assert "admitted after the 1s pressure wait bound" in result.stderr


@POSIX
def test_a_paused_gate_never_holds_the_only_test(tmp_path: Path) -> None:
    """With nothing else running there is nothing to drain; waiting cannot help."""

    admission = _admission_dir(tmp_path)
    # A stale slot from a SIGKILLed wrapper names a dead pid and must not count.
    (admission / "active" / "999999999").touch()
    (admission / PAUSED_FLAG).touch()
    env: dict[str, str] = {}
    _fill_env(
        env,
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS="30",
    )
    started = time.monotonic()
    result = _run(["-c", "pass"], env)

    assert result.returncode == 0, result.stderr
    assert time.monotonic() - started < 10
    assert "admission paused" not in result.stderr


@POSIX
def test_active_slot_is_held_for_the_test_lifetime_and_released(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    env: dict[str, str] = {}
    _fill_env(env, tmp_path, SOLDR_NEXTEST_ADMISSION_DIR=str(admission))
    child = "import os, sys; print(sorted(os.listdir(sys.argv[1])))"
    result = _run(["-c", child, str(admission / "active")], env)

    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() != "[]", "the running test must hold a slot"
    assert not list((admission / "active").iterdir()), "the slot outlived the test"


@POSIX
def test_admission_controls_are_not_inherited_by_the_test_process(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    env: dict[str, str] = {}
    _fill_env(
        env,
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(512 * MIB),
        SOLDR_NEXTEST_ADMISSION_SUMMARY="summary",
    )
    child = (
        "import os\n"
        "print(sorted(k for k in os.environ if k.startswith('SOLDR_NEXTEST_ADMISSION')"
        " or k == 'SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES'))"
    )
    result = _run(["-c", child], env)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "[]"


@POSIX
def test_without_ci_test_controls_the_wrapper_behaves_as_before(tmp_path: Path) -> None:
    env: dict[str, str] = {}
    _fill_env(env, tmp_path)
    result = _run(["-c", _HOG, "64", "0"], env)
    assert result.returncode == 0, result.stderr
    assert "nextest memory:" not in result.stderr


# The wrapper below runs inside a transient systemd scope that delegates the
# memory controller: it moves itself into an `init` leaf (cgroup v2 forbids
# processes in a cgroup that distributes controllers), enables memory+pids for
# the scope's children, then runs the wrapper with that scope as its root.
_DELEGATED_SCOPE = """
set -e
cg=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
mkdir "$cg/init"
echo 0 > "$cg/init/cgroup.procs"
echo '+memory +pids' > "$cg/cgroup.subtree_control"
echo "ROOT=$cg"
set +e
SOLDR_NEXTEST_CGROUP_ROOT="$cg" "$@"
echo "EXIT=$?"
ls -d "$cg"/snt-* 2>/dev/null || echo "LEAVES=none"
"""


@LINUX
def test_delegated_cgroup_v2_ceiling_is_enforced_by_the_kernel(tmp_path: Path) -> None:
    """Linux cgroup v2: memory.max + memory.oom.group on a per-test leaf.

    Needs a cgroup the current user may delegate. A systemd user manager
    provides one on developer machines; hosted CI runners have no user bus, so
    the test skips there rather than pretending.
    """

    probe = (
        run_captured(
            [
                "systemd-run",
                "--user",
                "--scope",
                "--quiet",
                "-p",
                "Delegate=yes",
                "true",
            ],
            capture_output=True,
            check=False,
        )
        if shutil.which("systemd-run")
        else None
    )
    if probe is None or probe.returncode != 0:
        pytest.skip("no delegatable systemd user scope on this host")
    admission = _admission_dir(tmp_path)
    env: dict[str, str] = {}
    _fill_env(
        env,
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(96 * MIB),
    )
    result = run_captured(
        [
            "systemd-run",
            "--user",
            "--scope",
            "--quiet",
            "-p",
            "Delegate=yes",
            "bash",
            "-c",
            _DELEGATED_SCOPE,
            "scope",
            *nextest_wrapper_argv(),
            sys.executable,
            "-c",
            _HOG,
            "400",
            "30",
        ],
        capture_output=True,
        text=True,
        env=env,
        timeout=120,
        check=False,
    )
    assert f"EXIT={INFRA_EXIT}" in result.stdout, result.stdout + result.stderr
    assert "the kernel OOM-killed the test inside its per-test cgroup" in result.stderr
    assert "per-test memory ceiling: 96.0 MiB (cgroup v2 memory.max)" in result.stderr
    assert "(cgroup memory.peak)" in result.stderr
    assert "LEAVES=none" in result.stdout, "the per-test leaf must be removed"
