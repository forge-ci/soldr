"""Tests for ci/build_target_group.py (soldr#2460).

RED-first per the issue's acceptance criteria: group expansion, per-target
command vectors, artifact destination paths, canonical-alias parity, and
the unknown-group failure mode.
"""

from __future__ import annotations

import json
from collections.abc import Sequence
from pathlib import Path
from typing import Any

import pytest
from conftest import load_script_module


def load_module():
    path = Path(__file__).parents[1] / "ci" / "build_target_group.py"
    return load_script_module(path, "build_target_group")


build_target_group = load_module()

REPO_ROOT = Path(__file__).parents[1]


def canonical_aliases() -> Sequence[Any]:
    data = json.loads(
        (REPO_ROOT / "ci" / "canonical-targets.json").read_text(encoding="utf-8")
    )
    return tuple(
        build_target_group.CanonicalTarget(entry["alias"], entry["triple"])
        for entry in data["targets"]
    )


def test_win_mac_musl_expands_to_exact_triples_in_order() -> None:
    plan = build_target_group.resolve_group("win-mac-musl", canonical_aliases())
    expected = [
        ("win-x64", "x86_64-pc-windows-msvc"),
        ("mac-arm64", "aarch64-apple-darwin"),
        ("linux-x64-musl", "x86_64-unknown-linux-musl"),
        ("linux-arm64-musl", "aarch64-unknown-linux-musl"),
    ]
    assert [(t.alias, t.triple) for t in plan] == expected


def test_per_target_command_is_soldr_build_release_target() -> None:
    cmd = build_target_group.build_command("win-x64", [])
    assert cmd == ["soldr", "build", "--release", "--target", "win-x64"]


def test_passthrough_args_are_forwarded_to_each_build() -> None:
    cmd = build_target_group.build_command("mac-arm64", ["--locked", "-v"])
    assert cmd == [
        "soldr",
        "build",
        "--release",
        "--target",
        "mac-arm64",
        "--locked",
        "-v",
    ]


def test_no_bare_cargo_or_rustc_in_script() -> None:
    # Dogfooding policy: every toolchain invocation routes through soldr.
    source = (REPO_ROOT / "ci" / "build_target_group.py").read_text(encoding="utf-8")
    for line in source.splitlines():
        stripped = line.split("#", 1)[0]
        assert '"cargo"' not in stripped and '"rustc"' not in stripped, line


def test_artifact_destinations_use_group_and_alias_with_exe_only_on_windows() -> None:
    moves = build_target_group.artifact_moves(
        group="win-mac-musl",
        alias="win-x64",
        triple="x86_64-pc-windows-msvc",
        out_dir=Path("dist"),
    )
    dests = [move.destination for move in moves]
    assert Path("dist/win-mac-musl/win-x64/soldr.exe") in dests
    assert Path("dist/win-mac-musl/win-x64/soldr-daemon.exe") in dests

    moves = build_target_group.artifact_moves(
        group="win-mac-musl",
        alias="linux-x64-musl",
        triple="x86_64-unknown-linux-musl",
        out_dir=Path("dist"),
    )
    for move in moves:
        src, dest = move.source, move.destination
        assert not src.name.endswith(".exe"), src
        assert not dest.name.endswith(".exe"), dest
        assert dest.parent == Path("dist/win-mac-musl/linux-x64-musl")
        assert (
            src == Path("target") / "x86_64-unknown-linux-musl" / "release" / src.name
        )


def test_group_table_aliases_are_canonical() -> None:
    # Parity guard (soldr#1695 spirit): every alias named by a group must
    # exist in ci/canonical-targets.json.
    known = {target.alias for target in canonical_aliases()}
    for group, aliases in build_target_group.GROUPS.items():
        for alias in aliases:
            assert alias in known, f"{group} names non-canonical alias {alias}"


def test_unknown_group_fails_naming_known_groups() -> None:
    with pytest.raises(SystemExit) as exc:
        build_target_group.main(["--group", "no-such-group", "--dry-run"])
    assert exc.value.code != 0


def test_dry_run_prints_plan_and_exits_zero(capsys) -> None:
    rc = build_target_group.main(["--dry-run"])
    assert rc == 0
    out = capsys.readouterr().out
    for alias in ("win-x64", "mac-arm64", "linux-x64-musl", "linux-arm64-musl"):
        assert alias in out
    assert "soldr build --release --target win-x64" in out
