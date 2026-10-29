"""Tests for the extracted standalone musl binary smoke gate (soldr#2469)."""

from __future__ import annotations

import stat
from dataclasses import dataclass
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"

smoke = load_script_module(SCRIPTS / "smoke_musl_binary.py", "smoke_musl_binary")


def test_binary_path_uses_target_release_layout(tmp_path: Path) -> None:
    assert smoke.binary_path("x86_64-unknown-linux-musl", "soldr", tmp_path) == (
        tmp_path / "x86_64-unknown-linux-musl" / "release" / "soldr"
    )


def test_musl_binary_contract_uses_release_version_helpers() -> None:
    assert smoke.expected_version("v0.9.2") == "0.9.2"
    assert smoke.version_problem("soldr 0.9.2\n") is None
    assert (
        smoke.version_json_problem('warning\n{"soldr_version":"0.9.2"}', "0.9.2")
        is not None
    )


def test_missing_or_non_executable_binary_is_a_named_failure(tmp_path: Path) -> None:
    with pytest.raises(smoke.MuslBinarySmokeError, match="missing executable"):
        smoke.smoke_binary(
            target="x86_64-unknown-linux-musl",
            binary="soldr",
            expected="0.9.2",
            target_dir=tmp_path,
        )


def test_missing_file_utility_does_not_block_the_release_smoke(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(b"")

    def missing_file_utility(command: list[str], **kwargs: object) -> SimpleNamespace:
        assert command == ["file", str(binary)]
        assert kwargs == {"check": False}
        raise FileNotFoundError("file not installed")

    monkeypatch.setattr(smoke.subprocess, "run", missing_file_utility)

    smoke.print_file_metadata(binary)

    assert "could not run file(1)" in capsys.readouterr().err


@dataclass(frozen=True)
class CliCall:
    """One recorded `run_cli` invocation."""

    command: list[str]
    capture: bool


def test_smoke_runs_file_and_both_cli_paths(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    binary = tmp_path / "x86_64-unknown-linux-musl" / "release" / "soldr"
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b"")
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    file_calls: list[Path] = []
    cli_calls: list[CliCall] = []

    def fake_file_metadata(path: Path) -> None:
        file_calls.append(path)

    def fake_cli(command: list[str], *, capture: bool) -> str:
        cli_calls.append(CliCall(command, capture))
        if command[-1] == "--version":
            return "soldr 0.9.2\n"
        if command[-2:] == ["version", "--json"]:
            return '{"soldr_version":"0.9.2"}\n'
        raise AssertionError(f"unexpected command: {command}")

    monkeypatch.setattr(smoke, "print_file_metadata", fake_file_metadata)
    monkeypatch.setattr(smoke, "run_cli", fake_cli)

    smoke.smoke_binary(
        target="x86_64-unknown-linux-musl",
        binary="soldr",
        expected="0.9.2",
        target_dir=tmp_path,
    )

    assert file_calls == [binary]
    assert cli_calls == [
        CliCall([str(binary), "--version"], True),
        CliCall([str(binary), "version", "--json"], True),
    ]


def test_json_probe_failure_keeps_the_binary_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    binary = tmp_path / "x86_64-unknown-linux-musl" / "release" / "soldr"
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b"")
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)

    def failed_json_probe(command: list[str], **kwargs: object) -> SimpleNamespace:
        if command[0] == "file":
            assert kwargs == {"check": False}
            return SimpleNamespace(stdout="")
        if command[-1] == "--version":
            assert kwargs == {"check": True, "capture_output": True, "text": True}
            return SimpleNamespace(stdout="soldr 0.9.2\n")
        if command[-2:] == ["version", "--json"]:
            assert kwargs == {"check": True, "capture_output": True, "text": True}
            raise smoke.subprocess.CalledProcessError(
                1, command, output="", stderr="loader error"
            )
        raise AssertionError(f"unexpected command: {command}")

    monkeypatch.setattr(smoke, "run_captured", failed_json_probe)

    with pytest.raises(smoke.MuslBinarySmokeError, match="loader error"):
        smoke.smoke_binary(
            target="x86_64-unknown-linux-musl",
            binary="soldr",
            expected="0.9.2",
            target_dir=tmp_path,
        )


def test_workflow_invokes_the_script_instead_of_inlining_musl_binary_smoke() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/smoke_musl_binary.py" in workflow
    assert "musl smoke test — soldr version --json output" not in workflow
