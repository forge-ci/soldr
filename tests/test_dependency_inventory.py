"""The dependency inventory must be a ratchet, in both directions (soldr#2752).

soldr#2752 wants third-party crates behind a `soldr-deps` gateway, enforced by
a manifest rule and a source lint. Its recommendation is to land the manifest
half first, because the expensive half (222 serde container attributes, plus
allowlists for `prost` and `clap`, whose derives hardcode `::prost::` /
`::clap::` paths with no crate override) should only be paid once the boundary
has proven worth maintaining.

What the manifest half buys on its own is that adding a dependency stops being
a line in a file nobody diffs. These tests pin the two properties that makes
true: an unlisted dependency fails, and a listed one that is gone also fails,
so the inventory cannot drift into describing a surface that no longer exists.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "check_dependency_inventory.py"
)


@pytest.fixture(scope="module")
def gate():
    return load_script_module(SCRIPT, "check_dependency_inventory")


def _surface(gate, crates: dict) -> tuple:
    return tuple(
        gate.CrateDependencies(crate, tuple(deps)) for crate, deps in crates.items()
    )


def _edge(gate, crate: str, dep: str) -> tuple:
    return (gate.DependencyEdge(crate, dep),)


def test_an_unlisted_dependency_is_reported(gate) -> None:
    changes = gate.diff(
        _surface(gate, {"soldr-core": ["serde", "regex"]}),
        _surface(gate, {"soldr-core": ["serde"]}),
    )
    assert changes.added == _edge(gate, "soldr-core", "regex")
    assert changes.removed == ()


def test_a_stale_entry_is_reported(gate) -> None:
    """The list shrinks with the surface, or it becomes folklore."""
    changes = gate.diff(
        _surface(gate, {"soldr-core": ["serde"]}),
        _surface(gate, {"soldr-core": ["serde", "regex"]}),
    )
    assert changes.added == ()
    assert changes.removed == _edge(gate, "soldr-core", "regex")


def test_a_matching_surface_is_silent(gate) -> None:
    surface = _surface(gate, {"soldr-core": ["serde"], "soldr-cli": []})
    changes = gate.diff(surface, surface)
    assert (changes.added, changes.removed) == ((), ())


def test_a_new_crate_is_not_invisible(gate) -> None:
    """A crate absent from the inventory must surface its whole edge set."""
    changes = gate.diff(_surface(gate, {"soldr-new": ["tokio"]}), ())
    assert changes.added == _edge(gate, "soldr-new", "tokio")


def test_workspace_internal_edges_are_not_third_party(gate, tmp_path) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(
        "[package]\nname='x'\n\n[dependencies]\n"
        "soldr-core = { path = '../soldr-core' }\n"
        "serde = '1'\n",
        encoding="utf-8",
    )
    assert gate.third_party_dependencies(manifest) == ["serde"]


def test_target_specific_dependencies_are_counted(gate, tmp_path) -> None:
    """`[target.'cfg(windows)'.dependencies]` is still a direct edge."""
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(
        "[package]\nname='x'\n\n[dependencies]\nserde = '1'\n\n"
        "[target.'cfg(windows)'.dependencies]\nwindows-sys = '0.52'\n",
        encoding="utf-8",
    )
    assert gate.third_party_dependencies(manifest) == ["serde", "windows-sys"]


def test_dev_dependencies_are_out_of_scope(gate, tmp_path) -> None:
    """soldr#2752's rule is about *normal* dependencies.

    A test fixture reaching for `tempfile` is not the boundary this guards,
    and inventorying dev-deps would make the list churn on every test that
    needs a helper.
    """
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(
        "[package]\nname='x'\n\n[dependencies]\nserde = '1'\n\n"
        "[dev-dependencies]\ntempfile = '3'\n\n"
        "[build-dependencies]\ncc = '1'\n",
        encoding="utf-8",
    )
    assert gate.third_party_dependencies(manifest) == ["serde"]


def test_the_checked_in_inventory_matches_the_workspace(gate) -> None:
    """The repo's own state must be clean, or the gate is already noise."""
    changes = gate.diff(gate.observed_surface(), gate.load_inventory())
    assert (changes.added, changes.removed) == ((), ()), (
        "run `python .github/scripts/check_dependency_inventory.py --write` "
        "and commit ci/dependency-inventory.json"
    )


def test_the_inventory_records_the_measured_surface(gate) -> None:
    """A sanity floor: soldr#2752 measured 40 distinct third-party crates."""
    surface = gate.observed_surface()
    distinct = {dep for entry in surface for dep in entry.dependencies}
    assert len(surface) >= 6, [entry.crate for entry in surface]
    assert len(distinct) >= 30, sorted(distinct)
