"""Unit tests for the extracted release wheel-filename lints (soldr#2469 2.2)."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module


def load_module():
    path = Path(__file__).parents[1] / ".github" / "scripts" / "wheel_filename_lints.py"
    return load_script_module(path, "wheel_filename_lints")


lints = load_module()


def wheel(tmp_path: Path, name: str) -> None:
    (tmp_path / name).write_bytes(b"")


def test_version_gate_accepts_matching_wheels(tmp_path: Path) -> None:
    wheel(tmp_path, "soldr-0.9.2-cp310-abi3-manylinux_2_17_x86_64.whl")
    wheel(tmp_path, "soldr-0.9.2-cp310-abi3-win_amd64.whl")
    assert (
        lints.main(["version", "--expected-version", "v0.9.2", "--dist", str(tmp_path)])
        == 0
    )


def test_version_gate_names_every_mismatch(tmp_path: Path) -> None:
    """The v0.7.72 incident shape: a 0.0.1 stub beside real wheels."""
    wheel(tmp_path, "soldr-0.9.2-cp310-abi3-win_amd64.whl")
    wheel(tmp_path, "soldr-0.0.1-cp310-abi3-manylinux_2_17_x86_64.whl")
    with pytest.raises(SystemExit) as failure:
        lints.main(["version", "--expected-version", "v0.9.2", "--dist", str(tmp_path)])
    rendered = str(failure.value)
    assert "soldr-0.0.1" in rendered
    assert "'0.0.1' != expected '0.9.2'" in rendered
    assert "soldr#1083" in rendered


def test_version_gate_flags_a_versionless_filename(tmp_path: Path) -> None:
    wheel(tmp_path, "junk.whl")
    with pytest.raises(SystemExit) as failure:
        lints.main(["version", "--expected-version", "v0.9.2", "--dist", str(tmp_path)])
    assert "no version field" in str(failure.value)


def test_manylinux_gate_accepts_2_17(tmp_path: Path) -> None:
    wheel(tmp_path, "soldr-0.9.2-cp310-abi3-manylinux_2_17_x86_64.whl")
    assert lints.main(["manylinux", "--dist", str(tmp_path)]) == 0


def test_manylinux_gate_rejects_runner_glibc_tag(tmp_path: Path) -> None:
    """The soldr#1005 trap: maturin fell back to the runner glibc."""
    wheel(tmp_path, "soldr-0.9.2-cp310-abi3-manylinux_2_39_x86_64.whl")
    with pytest.raises(SystemExit) as failure:
        lints.main(["manylinux", "--dist", str(tmp_path)])
    rendered = str(failure.value)
    assert (
        "manylinux_2_39" in rendered
        or "manylinux_2_17 tag assertion FAILED" in rendered
    )
    assert "soldr#1005" in rendered


def test_musllinux_gate_accepts_1_2(tmp_path: Path) -> None:
    wheel(tmp_path, "soldr-0.9.2-cp310-abi3-musllinux_1_2_x86_64.whl")
    assert lints.main(["musllinux", "--dist", str(tmp_path)]) == 0


def test_musllinux_gate_rejects_a_native_linux_tag(tmp_path: Path) -> None:
    """The soldr#909 regression: Alpine skips an untagged binary wheel."""
    wheel(tmp_path, "soldr-0.9.2-cp310-abi3-linux_x86_64.whl")
    with pytest.raises(SystemExit) as failure:
        lints.main(["musllinux", "--dist", str(tmp_path)])
    rendered = str(failure.value)
    assert (
        "linux_x86_64" in rendered or "musllinux_1_2 tag assertion FAILED" in rendered
    )
    assert "soldr#909" in rendered


def test_empty_dist_is_a_hard_error(tmp_path: Path) -> None:
    with pytest.raises(SystemExit) as failure:
        lints.main(["manylinux", "--dist", str(tmp_path)])
    assert "maturin produced nothing" in str(failure.value)


def test_the_workflow_invokes_both_gates() -> None:
    workflow = (
        Path(__file__).parents[1] / ".github" / "workflows" / "release-auto.yml"
    ).read_text(encoding="utf-8")
    assert "wheel_filename_lints.py version" in workflow
    assert "wheel_filename_lints.py manylinux" in workflow
    assert "wheel_filename_lints.py musllinux" in workflow
