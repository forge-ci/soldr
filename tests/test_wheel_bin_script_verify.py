"""Unit tests for the extracted wheel bin-script verification (soldr#2469 2.2)."""

from __future__ import annotations

import zipfile
from pathlib import Path

import pytest
from conftest import load_script_module


def load_module():
    path = (
        Path(__file__).parents[1] / ".github" / "scripts" / "wheel_bin_script_verify.py"
    )
    return load_script_module(path, "wheel_bin_script_verify")


verify_mod = load_module()


def build_wheel(tmp_path: Path, entries: dict[str, bytes]) -> Path:
    dist = tmp_path / "dist"
    dist.mkdir(exist_ok=True)
    wheel = dist / "soldr-0.9.2-cp310-abi3-manylinux_2_17_x86_64.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        for name, payload in entries.items():
            archive.writestr(name, payload)
    return dist


BIG = b"\x7fELF" + b"x" * (2 * 1024 * 1024)


def test_wellformed_wheel_passes(tmp_path: Path, capsys) -> None:
    dist = build_wheel(tmp_path, {"soldr-0.9.2.data/scripts/soldr": BIG})
    assert verify_mod.main(["--binary", "soldr", "--dist", str(dist)]) == 0
    assert "payload >=" in capsys.readouterr().out


def test_macos_launcher_layout_passes_via_scripts_dir(tmp_path: Path) -> None:
    dist = build_wheel(
        tmp_path,
        {
            "soldr-0.9.2.data/scripts/soldr": b"#!python launcher stub",
            "soldr.scripts/soldr": BIG,
        },
    )
    assert verify_mod.main(["--binary", "soldr", "--dist", str(dist)]) == 0


def test_stub_binary_fails_the_floor(tmp_path: Path) -> None:
    """The soldr#1140 shape: a ~118 KB stub where a ~15 MB binary belongs."""
    dist = build_wheel(tmp_path, {"soldr-0.9.2.data/scripts/soldr": b"x" * 118_000})
    with pytest.raises(SystemExit) as failure:
        verify_mod.main(["--binary", "soldr", "--dist", str(dist)])
    assert "soldr#1140" in str(failure.value)


def test_missing_script_entry_fails(tmp_path: Path) -> None:
    dist = build_wheel(tmp_path, {"soldr/__init__.py": b""})
    with pytest.raises(SystemExit) as failure:
        verify_mod.main(["--binary", "soldr", "--dist", str(dist)])
    assert "expected exactly one" in str(failure.value)


def test_two_wheels_fail(tmp_path: Path) -> None:
    dist = build_wheel(tmp_path, {"soldr-0.9.2.data/scripts/soldr": BIG})
    (dist / "soldr-0.9.2-cp310-abi3-win_amd64.whl").write_bytes(b"")
    with pytest.raises(SystemExit) as failure:
        verify_mod.main(["--binary", "soldr", "--dist", str(dist)])
    assert "exactly one wheel" in str(failure.value)


def test_the_workflow_invokes_the_script() -> None:
    workflow = (
        Path(__file__).parents[1] / ".github" / "workflows" / "release-auto.yml"
    ).read_text(encoding="utf-8")
    assert "wheel_bin_script_verify.py" in workflow
