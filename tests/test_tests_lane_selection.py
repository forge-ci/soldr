"""The bosn `tests` lane narrows to the reverse-dependency closure of the
changed workspace crates only on proof (soldr#3731, follow-up to #3704).

A crate's test binaries are built from that crate and its path-dependency
closure, with an unchanged Cargo.lock. A change set made only of crate source
files therefore cannot change the test binaries of a crate that does not
depend on a changed crate. soldr-cli is always selected: its guard tests read
the whole workspace tree. Anything else -- a root manifest, the lockfile, the
toolchain, the test infrastructure, an unknown path, an undeterminable diff --
runs the full suite."""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
GRAPH = load_script_module(ROOT / "ci" / "crate_graph.py", "soldr_crate_graph")
HANDOFF = load_script_module(
    ROOT / "ci" / "bosn_workspace_test.py", "soldr_bosn_workspace_test_sel"
)
WINE = load_script_module(ROOT / "ci" / "wine_lane.py", "soldr_wine_lane_sel")


def test_wine_lane_reuses_the_shared_closure() -> None:
    # One closure implementation (soldr#3731): the wine lane's must be it.
    shared = str(ROOT / "ci" / "crate_graph.py")
    for fn in (WINE.dependency_closure, WINE.changed_paths, WINE.workspace_crates):
        assert fn.__code__.co_filename == shared, fn
    for fn in (HANDOFF.reverse_dependency_closure, HANDOFF.changed_paths):
        assert fn.__code__.co_filename == shared, fn


def test_reverse_closure_is_the_real_one() -> None:
    assert GRAPH.reverse_dependency_closure(ROOT, ("soldr-cli",)) == {"soldr-cli"}
    rdeps = GRAPH.reverse_dependency_closure(ROOT, ("soldr-core",))
    assert {"soldr-core", "soldr-cache", "soldr-daemon", "soldr-cli"} <= rdeps
    assert "soldr-platform" not in rdeps


def test_cli_only_change_selects_cli() -> None:
    sel = HANDOFF.select_packages(ROOT, ["crates/soldr-cli/src/lib.rs"])
    assert sel.packages == ("soldr-cli",)
    assert "soldr-cli" in sel.reason


def test_crate_change_selects_reverse_closure_plus_cli() -> None:
    sel = HANDOFF.select_packages(ROOT, ["crates/soldr-nextest-wrapper/src/main.rs"])
    assert sel.packages == ("soldr-cli", "soldr-nextest-wrapper")
    sel = HANDOFF.select_packages(ROOT, ["crates/soldr-cache/src/a.rs"])
    assert sel.packages is not None
    assert {"soldr-cache", "soldr-daemon", "soldr-cli"} <= set(sel.packages)
    assert "soldr-fetch" not in sel.packages


def test_conservative_inputs_run_everything() -> None:
    for path in (
        "Cargo.lock",
        "Cargo.toml",
        "rust-toolchain.toml",
        ".config/nextest.toml",
        ".cargo/config.toml",
        "ci/bosn_workspace_test.py",
        "ci/crate_graph.py",
        ".github/scripts/nextest_wrapper.sh",
        "tests/test_nextest_memory_guard.py",
        "crates/soldr-cli/tests/guards/main.rs",
        "crates/soldr-fetch/tests/fixtures/catalogue.v2.json",
        "crates/soldr-cli/Cargo.toml",
        "crates/nonexistent/src/a.rs",
        "docs/API.md",
    ):
        sel = HANDOFF.select_packages(ROOT, ["crates/soldr-cli/src/lib.rs", path])
        assert sel.packages is None, path


def test_unknown_change_set_runs_everything() -> None:
    assert HANDOFF.select_packages(ROOT, None).packages is None
    assert HANDOFF.select_packages(ROOT, []).packages is None


def test_plan_scopes_both_test_stages_to_the_selection() -> None:
    plan = HANDOFF.workspace_test_plan(
        target=Path("/target"),
        bootstrap=Path("/b/soldr"),
        packages=("soldr-cli", "soldr-core"),
    )
    script = plan.validation.argv[-1]
    assert "--workspace" not in script
    assert script.count("-p soldr-cli -p soldr-core") == 2
    full = HANDOFF.workspace_test_plan(target=Path("/target"), bootstrap=Path("/b"))
    assert full.validation.argv[-1].count("--workspace") == 2
