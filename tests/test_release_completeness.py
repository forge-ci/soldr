"""Tests for .github/scripts/release_completeness.py (soldr#2469 step 1.1).

The RED case reproduces the 0.9.0 incident exactly: the immutable GitHub
Release holds only 7 of 17 assets while PyPI and npm look healthy — the
verifier must fail loudly, naming every missing asset.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]


def load_module():
    path = REPO_ROOT / ".github" / "scripts" / "release_completeness.py"
    return load_script_module(path, "release_completeness")


release_completeness = load_module()


def contract_triples() -> list[str]:
    data = json.loads(
        (REPO_ROOT / "ci" / "canonical-targets.json").read_text(encoding="utf-8")
    )
    return [
        entry["triple"]
        for entry in data["targets"]
        if entry["release"]["status"] == "included"
    ]


# The exact asset set present on the live, immutable v0.9.0 release.
V090_PRESENT = [
    "soldr-0.9.0-py3-none-macosx_11_0_arm64.whl",
    "soldr-0.9.0-py3-none-musllinux_1_2_x86_64.whl",
    "soldr-0.9.0-py3-none-win_amd64.whl",
    "soldr-v0.9.0-SHA256SUMS.txt",
    "soldr-v0.9.0-aarch64-apple-darwin.tar.zst",
    "soldr-v0.9.0-x86_64-pc-windows-msvc.tar.zst",
    "soldr-v0.9.0-x86_64-unknown-linux-musl.tar.zst",
]


@dataclass(frozen=True)
class Surfaces:
    """A release's expected GitHub assets, PyPI files, and npm versions."""

    github: list[str]
    pypi: list[str]
    npm: list[str]


def full_surface(tag: str) -> Surfaces:
    triples = contract_triples()
    github = release_completeness.expected_github_assets(tag, triples)
    pypi = release_completeness.expected_pypi_files(tag, triples)
    return Surfaces(github, pypi, [tag.lstrip("v")])


def test_wheel_tag_table_covers_exactly_the_contract() -> None:
    assert set(release_completeness.WHEEL_TAGS) == set(contract_triples())


def test_expected_github_assets_are_eighteen_for_eight_targets() -> None:
    triples = contract_triples()
    assets = release_completeness.expected_github_assets("v0.9.1", triples)
    assert len(assets) == 2 * len(triples) + 2 == 18
    assert "soldr-v0.9.1-x86_64-unknown-linux-gnu.tar.zst" in assets
    assert "soldr-0.9.1-py3-none-win_arm64.whl" in assets
    assert "soldr-v0.9.1-SHA256SUMS.txt" in assets
    assert "soldr-v0.9.1-info.json" in assets  # soldr#3343


def test_the_0_9_0_incident_fails_the_gate() -> None:
    # PyPI and npm complete, GitHub Release missing 10 of 17: the state
    # that concluded green in run 31487675391 must fail here. (0.9.0 also
    # predates info.json, soldr#3343, the eleventh gap.)
    triples = contract_triples()
    surface = full_surface("v0.9.0")
    pypi, npm = surface.pypi, surface.npm
    failures = release_completeness.verify_surfaces(
        "v0.9.0", triples, V090_PRESENT, pypi, npm
    )
    assert len(failures) == 11, failures
    assert all(f.startswith("github-release missing asset:") for f in failures)
    assert any("x86_64-unknown-linux-gnu.tar.zst" in f for f in failures)
    assert any("win_arm64.whl" in f for f in failures)


def test_a_complete_release_passes() -> None:
    triples = contract_triples()
    surface = full_surface("v0.9.1")
    github, pypi, npm = surface.github, surface.pypi, surface.npm
    assert (
        release_completeness.verify_surfaces("v0.9.1", triples, github, pypi, npm) == []
    )
    # Extra assets (e.g. debug sidecars) never count against completeness.
    assert (
        release_completeness.verify_surfaces(
            "v0.9.1", triples, [*github, "extra.dwp"], pypi, npm
        )
        == []
    )


def test_missing_pypi_wheel_and_npm_version_are_named() -> None:
    triples = contract_triples()
    surface = full_surface("v0.9.1")
    github, pypi = surface.github, surface.pypi
    failures = release_completeness.verify_surfaces(
        "v0.9.1", triples, github, pypi[:-1], ["0.9.0"]
    )
    assert any(f.startswith("pypi missing file:") for f in failures)
    assert any("npm" in f and "0.9.1" in f for f in failures)


def test_list_mode_prints_contract_assets_and_touches_no_network(capsys) -> None:
    rc = release_completeness.main(
        ["--version", "v0.9.1", "--list-expected-github-assets"]
    )
    assert rc == 0
    lines = capsys.readouterr().out.strip().splitlines()
    assert lines == release_completeness.expected_github_assets(
        "v0.9.1", contract_triples()
    )
    assert len(lines) == 18


def test_soldr_has_no_publishable_crates_and_says_why() -> None:
    """soldr#3343: crates.io is accounted for, not silently absent."""
    crates = release_completeness.publishable_crates()
    assert crates == []
    assert "skipped" in release_completeness.crates_io_plan(crates)


def test_a_crate_without_publish_false_joins_the_gate(tmp_path: Path) -> None:
    (tmp_path / "a").mkdir()
    (tmp_path / "b").mkdir()
    (tmp_path / "Cargo.toml").write_text(
        '[workspace]\nmembers = ["a", "b"]\n', encoding="utf-8"
    )
    (tmp_path / "a" / "Cargo.toml").write_text(
        '[package]\nname = "a"\npublish = false\n', encoding="utf-8"
    )
    (tmp_path / "b" / "Cargo.toml").write_text(
        '[package]\nname = "b"\n', encoding="utf-8"
    )
    crates = release_completeness.publishable_crates(tmp_path)
    assert crates == ["b"]
    assert release_completeness.crates_io_plan(crates) == "crates.io: gated on b"
    assert release_completeness.crates_io_failures("1.0.0", {"b": ["0.9.0"]}) == [
        "crates.io b missing version: 1.0.0"
    ]
    assert release_completeness.crates_io_failures("1.0.0", {"b": ["1.0.0"]}) == []
