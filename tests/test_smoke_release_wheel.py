"""Tests for the extracted release-wheel runtime smoke gate (soldr#2469)."""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import (
    load_script_module,
    uv_pip_install_command,
    write_fake_soldr_console,
)

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"

smoke = load_script_module(SCRIPTS / "smoke_release_wheel.py", "smoke_release_wheel")


def test_collect_wheels_rejects_an_empty_dist(tmp_path: Path) -> None:
    with pytest.raises(smoke.WheelSmokeError, match="maturin produced nothing"):
        smoke.collect_wheels(tmp_path)


def test_console_script_supports_unix_and_windows_virtualenv_layouts(
    tmp_path: Path,
) -> None:
    unix = tmp_path / "unix"
    unix_bin = unix / "bin" / "soldr"
    unix_bin.parent.mkdir(parents=True)
    unix_bin.write_bytes(b"")
    assert smoke.console_script(unix) == unix_bin

    windows = tmp_path / "windows"
    windows_bin = windows / "Scripts" / "soldr.exe"
    windows_bin.parent.mkdir(parents=True)
    windows_bin.write_bytes(b"")
    assert smoke.console_script(windows) == windows_bin


def test_version_contract_rejects_stub_or_wrong_json_payload() -> None:
    assert smoke.version_problem("soldr 0.9.2\n") is None
    assert smoke.version_problem("not soldr\n") is not None
    assert smoke.version_json_problem('{"soldr_version":"0.9.2"}', "0.9.2") is None
    assert smoke.version_json_problem("", "0.9.2") is not None
    assert smoke.version_json_problem('{"soldr_version":"0.0.1"}', "0.9.2") is not None


def test_cli_probe_failure_keeps_the_wheel_stderr(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    command = ["soldr", "version", "--json"]

    def failed_run(observed: list[str], **kwargs: object) -> SimpleNamespace:
        assert observed == command
        assert kwargs["capture_output"] is True
        raise smoke.subprocess.CalledProcessError(
            1, observed, output="", stderr="loader error"
        )

    monkeypatch.setattr(smoke, "run_captured", failed_run)

    with pytest.raises(smoke.WheelSmokeError, match="loader error"):
        smoke.run_cli(command)


def test_smoke_installs_all_wheels_and_exercises_both_cli_paths(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dist = tmp_path / "dist"
    dist.mkdir()
    first = dist / "soldr-0.9.2-cp310-abi3-manylinux_2_17_x86_64.whl"
    second = dist / "soldr-0.9.2-cp311-abi3-manylinux_2_17_x86_64.whl"
    first.write_bytes(b"")
    second.write_bytes(b"")
    venv = tmp_path / ".venv"
    calls: list[list[str]] = []

    def fake_run(command: list[str], **_kwargs: object) -> SimpleNamespace:
        calls.append(command)
        if command[:2] == ["uv", "venv"]:
            return SimpleNamespace(stdout="")
        if command == uv_pip_install_command(venv, str(first), str(second)):
            write_fake_soldr_console(venv, windows=False)
            return SimpleNamespace(stdout="")
        if command[-1] == "--version":
            return SimpleNamespace(stdout="soldr 0.9.2\n")
        if command[-2:] == ["version", "--json"]:
            return SimpleNamespace(stdout='{"soldr_version":"0.9.2"}\n')
        raise AssertionError(f"unexpected command: {command}")

    monkeypatch.setattr(smoke, "run_captured", fake_run)
    monkeypatch.setattr(smoke.subprocess, "run", fake_run)

    smoke.smoke_wheel(expected_version="v0.9.2", dist=dist, venv=venv)

    assert calls[0] == ["uv", "venv", str(venv)]
    assert calls[1] == uv_pip_install_command(venv, str(first), str(second))
    assert calls[2] == [str(venv / "bin" / "soldr"), "--version"]
    assert calls[3] == [str(venv / "bin" / "soldr"), "version", "--json"]


def test_workflow_invokes_the_script_instead_of_inlining_wheel_smoke() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/smoke_release_wheel.py" in workflow
    assert "wheel smoke test — soldr --version output" not in workflow
