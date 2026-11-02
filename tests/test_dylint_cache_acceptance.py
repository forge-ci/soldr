from __future__ import annotations

from pathlib import Path

from conftest import load_script_module


def load_module():
    path = (
        Path(__file__).parents[1] / ".github" / "scripts" / "dylint_cache_acceptance.py"
    )
    return load_script_module(path, "dylint_cache_acceptance")


dylint_acceptance = load_module()


def test_absolute_watchdog_survives_noisy_semantic_progress() -> None:
    script = dylint_acceptance.BASH
    assert "WATCHDOG_ABSOLUTE_SECS=1800" in script
    assert 'elapsed_secs="$((elapsed_secs + WATCHDOG_POLL_SECS))"' in script

    progress_branch = script.split('if [[ "$semantic_progress" -eq 1 ]]', maxsplit=1)[
        1
    ].split('if [[ "$captured" -eq 1 ]]', maxsplit=1)[0]
    assert "elapsed_secs=0" not in progress_branch
    assert "continue" not in progress_branch

    absolute_branch = script.split(
        'if [[ "$elapsed_secs" -ge "$WATCHDOG_ABSOLUTE_SECS" ]]', maxsplit=1
    )[1].split('elif [[ "$idle_secs"', maxsplit=1)[0]
    assert "absolute_deadline=1" in absolute_branch

    capture_tail = script.split('cat "$dump" >&2', maxsplit=1)[1]
    assert 'if [[ "$absolute_deadline" -eq 1 ]]' in capture_tail
    assert 'terminate_scope "$command_pid"' in capture_tail


def test_initial_dylint_bootstrap_is_inside_monitored_cold_case() -> None:
    script = dylint_acceptance.BASH
    assert "cargo dylint --version" not in script
    assert script.count('"$SOLDR" cargo dylint --all') == 1
    assert "> >(" not in script
    assert '2>&1 | tee -a "$live_log"' in script
    assert 'run_case "$name" /tmp/dylint-acceptance/a' in script
    assert (
        "The cold case intentionally owns first-time cargo-dylint and driver" in script
    )


def test_cross_worktree_registration_trace_is_collected() -> None:
    script = dylint_acceptance.BASH
    assert (
        "ZCCACHE_INNER_TRACE=/tmp/dylint-acceptance/diagnostics/"
        "context-registration-trace.jsonl"
    ) in script


def test_watchdog_symbol_smoke_uses_full_debug_info_and_real_gdb_attach() -> None:
    script = dylint_acceptance.BASH
    assert "run_symbolized_watchdog_smoke" in script
    assert 'dump_one_pid "$command_pid"' in script
    assert "watchdog-symbol-smoke-stacks.txt" in script
    assert "watchdog-symbol-smoke-passed" in script
    assert "grep -Fq 'exe=/target/debug/soldr'" in script
    assert "grep -Eq 'soldr_(cli|daemon|core)::'" in script
    assert "grep -Eq 'crates/soldr-(cli|daemon|core)/src/[^ ]*\\.rs:[0-9]+'" in script

    source = (
        Path(__file__).parents[1] / ".github" / "scripts" / "dylint_cache_acceptance.py"
    ).read_text(encoding="utf-8")
    assert '"profile.dev.debug=2"' in source


def test_acceptance_uses_the_per_checkout_perf_runner_identity() -> None:
    source = (
        Path(__file__).parents[1] / ".github" / "scripts" / "dylint_cache_acceptance.py"
    ).read_text(encoding="utf-8")
    assert 'runpy.run_path(str(ROOT / "ci" / "perf_local.py"))' in source
    assert "runner_container = runner.container" in source
    assert '"soldr-perf-local"' not in source


def test_second_identical_run_carries_the_a5_compile_bound() -> None:
    """soldr#2436 phase 6 / soldr#2645: the full-mode check list must bound
    the second identical run's compiles under 20% of the cold run — the
    #2435 pathology was a warm run recompiling the world, and hit-count
    checks alone cannot see it."""
    source = (
        Path(__file__).parents[1] / ".github" / "scripts" / "dylint_cache_acceptance.py"
    ).read_text(encoding="utf-8")
    assert 'by_name["warm_same_target"]["misses"] * 5' in source
    assert "second identical run must compile <20% of the cold run" in source


def test_killed_symbol_smoke_never_races_first_toolchain_acquisition() -> None:
    script = dylint_acceptance.BASH
    cold = script.index("run_case cold /tmp/dylint-acceptance/a")
    smoke = script.index("  run_symbolized_watchdog_smoke\n")
    assert cold < smoke, (
        "the watchdog symbol smoke kills its soldr process; it must run after "
        "the cold case has installed the pinned toolchain"
    )
