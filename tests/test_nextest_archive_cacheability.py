"""The warm-dependency cacheability verdict (soldr#2937, phase 5 of soldr#2931).

soldr#1391 asserted that the full LINKED nextest archive must be warm-cacheable
at positive hits and **zero** misses. soldr#2931 inverted that policy: cache
admission follows the stability of an artifact's identity key relative to its
size, and a linked test product has the least stable key and one of the largest
sizes in the build. Requiring it to be a cache hit required the store to carry
exactly what the policy forbids.

What survives is the half that was always valuable -- dependency compilation
must stay warm -- and the classification that decides it now lives in
`evaluate_warm_result`. These tests cover that classification without Docker,
which is the whole point of moving it out of the shell: the acceptance costs
~40 minutes, so a verdict bug must be findable in milliseconds. The Docker
acceptance itself stays here, opt-in, at the bottom.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest
from conftest import docker_available, load_script_module

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
_SCRIPT = REPO_ROOT / "ci" / "assert_nextest_archive_cacheability.py"
cacheability = load_script_module(_SCRIPT, "cacheability_verdict")

classify_warm_misses = cacheability.classify_warm_misses
evaluate_warm_result = cacheability.evaluate_warm_result
normalize_unit = cacheability.normalize_unit
first_party_units = cacheability.first_party_units

FIRST_PARTY = first_party_units(
    ["soldr-cli", "soldr_daemon", "soldr-core", "soldr", "soldr-cache", "soldr-fetch"]
)


def warm(hits: int = 100, misses: int = 0) -> Any:
    return cacheability.WarmResult(
        cold_hits=0,
        cold_misses=900,
        warm_hits=hits,
        warm_misses=misses,
    )


# --------------------------------------------------------------------------
# Classification
# --------------------------------------------------------------------------


def test_normalize_unit_folds_dashes_and_case() -> None:
    assert normalize_unit("soldr-CLI") == "soldr_cli"
    assert normalize_unit("  serde_json ") == "serde_json"


def test_first_party_units_are_expected_misses() -> None:
    dependency, expected = classify_warm_misses(
        ["soldr_cli", "soldr_daemon", "soldr-core", "soldr"], FIRST_PARTY
    )
    assert dependency == []
    assert expected == ["soldr", "soldr_cli", "soldr_core", "soldr_daemon"]


def test_dependency_units_are_regressions() -> None:
    dependency, expected = classify_warm_misses(
        ["serde", "tokio", "soldr_cli"], FIRST_PARTY
    )
    assert dependency == ["serde", "tokio"]
    assert expected == ["soldr_cli"]


def test_build_script_units_are_never_fatal() -> None:
    """`build_script_build` names every crate's build script, first-party or not.

    An unattributable name must not decide a verdict -- reporting a failure
    nobody can trace to a crate is the defect soldr#2824 spent three weeks on.
    """
    dependency, expected = classify_warm_misses(["build_script_build"], FIRST_PARTY)
    assert dependency == []
    assert expected == ["build_script_build"]


def test_classification_is_deduplicated_and_sorted() -> None:
    dependency, expected = classify_warm_misses(
        ["tokio", "tokio", "serde", "", "soldr_cli", "soldr_cli"], FIRST_PARTY
    )
    assert dependency == ["serde", "tokio"]
    assert expected == ["soldr_cli"]


# --------------------------------------------------------------------------
# The verdict
# --------------------------------------------------------------------------


def test_linked_test_products_do_not_fail_the_lane() -> None:
    """The soldr#1391 invariant, inverted.

    A warm run whose only misses are first-party units is exactly what
    soldr#2931 expects: the test-harness link products were rebuilt and were
    never required to be cache hits. Under the old rule this was a hard
    failure.
    """
    failures = evaluate_warm_result(
        warm(hits=812, misses=6),
        ["soldr_cli", "soldr_daemon", "soldr_cache"],
        FIRST_PARTY,
    )
    assert failures == []


def test_a_dependency_miss_fails() -> None:
    failures = evaluate_warm_result(
        warm(hits=812, misses=3), ["serde", "soldr_cli"], FIRST_PARTY
    )
    assert len(failures) == 1
    assert "serde" in failures[0]
    assert "soldr_cli" not in failures[0]


def test_zero_warm_hits_fails_even_with_no_miss_detail() -> None:
    failures = evaluate_warm_result(warm(hits=0, misses=0), [], FIRST_PARTY)
    assert failures and "zero compiler-cache hits" in failures[0]


def test_missing_miss_detail_degrades_instead_of_failing() -> None:
    """An absent diagnostic is not evidence of a regression.

    The harness cannot produce a per-unit list when the build log is missing.
    Failing there would teach people to ignore the lane, so the check falls
    back to the one condition it can still evaluate.
    """
    assert evaluate_warm_result(warm(hits=500), None, FIRST_PARTY) == []
    assert evaluate_warm_result(warm(hits=0), None, FIRST_PARTY) != []


RUN_37318182694_MISSES = [
    "broker", "cache_gc", "cargo_front_door", "cook_archive_executable_bit",
    "cook_dylint", "daemon", "daemon_windows_pipe_peer", "daemon_zccache_embedded",
    "fetch_tools", "guards", "soldr", "soldr_cache", "soldr_cli", "soldr_core",
    "soldr_daemon", "soldr_fetch", "soldr_nextest_wrapper", "soldr_platform",
    "toolchain_env", "wrapper",
]  # fmt: skip
RUN_37318182694_TARGETS = [
    "soldr-cli", "soldr-core", "soldr-cache", "soldr-daemon", "soldr-fetch",
    "soldr-platform", "soldr-nextest-wrapper", "soldr", "broker", "cache_gc",
    "cargo_front_door", "cook_archive_executable_bit", "cook_dylint", "daemon",
    "daemon_windows_pipe_peer", "daemon_zccache_embedded", "fetch_tools",
    "guards", "toolchain_env", "wrapper",
]  # fmt: skip


def test_first_party_units_normalizes_cargo_target_names() -> None:
    assert first_party_units(["soldr-cli", "Cargo_Front_Door", ""]) == frozenset(
        {"soldr_cli", "cargo_front_door"}
    )


def test_category_test_targets_are_first_party_soldr3606() -> None:
    """Run 37318182694: 518 hits / 0 misses still failed on category targets."""
    first_party = first_party_units(RUN_37318182694_TARGETS)
    assert (
        evaluate_warm_result(
            warm(hits=518, misses=20), RUN_37318182694_MISSES, first_party
        )
        == []
    )
    # RED: the old `soldr` name prefix rejects these.
    prefix_only = frozenset(u for u in RUN_37318182694_MISSES if u.startswith("soldr"))
    assert evaluate_warm_result(
        warm(hits=518, misses=20), RUN_37318182694_MISSES, prefix_only
    )


def test_real_dependency_still_fails_with_metadata_set() -> None:
    first_party = first_party_units(RUN_37318182694_TARGETS)
    failures = evaluate_warm_result(
        warm(hits=518, misses=2), ["serde", "broker"], first_party
    )
    assert len(failures) == 1 and "serde" in failures[0]


def test_missing_workspace_metadata_degrades_to_zero_hit_rule() -> None:
    assert evaluate_warm_result(warm(hits=5), ["serde"], None) == []
    assert evaluate_warm_result(warm(hits=0), ["serde"], None) != []


def test_clean_warm_run_passes() -> None:
    assert evaluate_warm_result(warm(hits=900, misses=0), [], FIRST_PARTY) == []


# --------------------------------------------------------------------------
# The Docker acceptance
# --------------------------------------------------------------------------


@pytest.mark.cacheability_integration
def test_warm_dependency_cacheability_acceptance() -> None:
    if not docker_available():
        pytest.skip("docker daemon not reachable")

    result = run_captured(
        [sys.executable, str(_SCRIPT)],
        cwd=REPO_ROOT,
        text=True,
        capture_stdout=True,
        stderr=subprocess.STDOUT,
        timeout=3600,
        check=False,
    )
    assert result.returncode == 0, result.stdout
    assert "CACHEABILITY_OK dependency units reused the compiler cache" in result.stdout
