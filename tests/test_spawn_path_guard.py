"""Regression tests for the tree-level raw-spawn guard (soldr#2442 slice 4)."""

from __future__ import annotations

from pathlib import Path

from conftest import lint_lane_text, load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "spawn_path_guard.py"
GUARD = load_script_module(SCRIPT_PATH, "spawn_path_guard")


def test_current_tree_passes_the_guard() -> None:
    counts = GUARD.spawn_counts()
    assert GUARD.violations(counts) == []


def test_every_allowlist_entry_is_still_live() -> None:
    """A file that no longer spawns must lose its entry (anti-mask rule)."""
    counts = GUARD.spawn_counts()
    live = {entry.path for entry in counts}
    stale = [entry.path for entry in GUARD.ALLOWLIST if entry.path not in live]
    assert stale == [], f"allowlist entries for files with no spawns: {stale}"


def test_a_new_spawn_file_is_a_named_violation() -> None:
    counts = GUARD.spawn_counts()
    counts.append(GUARD.SpawnCount("crates/soldr-cli/src/imaginary_new_module.rs", 1))
    problems = GUARD.violations(counts)
    assert any(
        "imaginary_new_module.rs" in p and "no allowlist entry" in p for p in problems
    )


def test_a_grown_count_is_a_named_violation() -> None:
    counts = GUARD.spawn_counts()
    first = GUARD.ALLOWLIST[0]
    some_file = first.path
    counts = [entry for entry in counts if entry.path != some_file]
    counts.append(GUARD.SpawnCount(some_file, first.count + 1))
    problems = GUARD.violations(counts)
    assert any(some_file in p and "grew" in p for p in problems)


def test_sanctioned_broker_and_daemon_paths_are_not_allowlisted() -> None:
    """The front-door broker spawn and all daemon children go through
    running_process::spawn*; the guard exists so raw spawns cannot appear
    beside them. Their files must never need entries."""
    paths = [entry.path for entry in GUARD.ALLOWLIST]
    assert "crates/soldr-cli/src/broker_spawn.rs" not in paths
    daemon_entries = [
        entry.path
        for entry in GUARD.ALLOWLIST
        if entry.path.startswith("crates/soldr-daemon/")
        and "false positive" not in entry.justification
    ]
    assert daemon_entries == []


def test_test_sources_are_out_of_scope() -> None:
    assert not GUARD.is_production_source(
        REPO_ROOT / "crates/soldr-cli/src/cargo_front_door/tests.rs"
    )
    assert not GUARD.is_production_source(
        REPO_ROOT / "crates/soldr-cli/src/prepare_cmd_tests.rs"
    )
    assert GUARD.is_production_source(
        REPO_ROOT / "crates/soldr-cli/src/broker_spawn.rs"
    )


def test_lint_job_runs_the_guard() -> None:
    workflow = lint_lane_text()
    assert "spawn_path_guard.py" in workflow
