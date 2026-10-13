from __future__ import annotations

import json
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pytest
from conftest import load_script_module

from soldr._process import run_captured

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "verify_soldr.py"


@dataclass(frozen=True)
class RecordedCall:
    command: list[str]
    capture_output: bool = False
    text: bool = False


def _load_module():
    return load_script_module(SCRIPT_PATH, "verify_soldr")


def test_main_tolerates_missing_zccache_daemon_during_status_probe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    module = _load_module()
    github_output = tmp_path / "github.output"
    monkeypatch.setenv("SETUP_SOLDR_PATH", "C:/temp/soldr.exe")
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))

    calls: list[RecordedCall] = []

    def fake_check_output(
        cmd: list[str], *, text: bool = True, timeout: int = 30
    ) -> str:
        assert text is True
        assert timeout == 30
        assert cmd == ["C:/temp/soldr.exe", "version", "--json"]
        return json.dumps({"soldr_version": "0.7.4"})

    def fake_run(cmd: list[str], *, capture_output: bool = False, text: bool = False):
        calls.append(RecordedCall(cmd, capture_output, text))
        if cmd == ["soldr", "status", "--json"]:
            raise subprocess.CalledProcessError(
                1,
                cmd,
                output="",
                stderr=(
                    "soldr: zccache status failed: daemon not running at "
                    "\\\\.\\pipe\\zccache-runneradmin"
                ),
            )
        return subprocess.CompletedProcess(cmd, 0)

    monkeypatch.setattr(module, "_check_output", fake_check_output)
    monkeypatch.setattr(module, "_run", fake_run)

    module.main()

    assert calls == [
        RecordedCall(["cargo", "--version"]),
        RecordedCall(["rustc", "--version"]),
        RecordedCall(["soldr", "status", "--json"], capture_output=True, text=True),
    ]
    assert github_output.read_text(encoding="utf-8") == "soldr_version=0.7.4\n"


def test_main_propagates_unexpected_status_failures(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    module = _load_module()
    github_output = tmp_path / "github.output"
    monkeypatch.setenv("SETUP_SOLDR_PATH", "C:/temp/soldr.exe")
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))

    def fake_check_output(
        cmd: list[str], *, text: bool = True, timeout: int = 30
    ) -> str:
        assert text is True
        assert timeout == 30
        assert cmd == ["C:/temp/soldr.exe", "version", "--json"]
        return json.dumps({"soldr_version": "0.7.4"})

    def fake_run(cmd: list[str], **kwargs: Any):
        if cmd == ["soldr", "status", "--json"]:
            raise subprocess.CalledProcessError(1, cmd, stderr="unexpected failure")
        return subprocess.CompletedProcess(cmd, 0)

    monkeypatch.setattr(module, "_check_output", fake_check_output)
    monkeypatch.setattr(module, "_run", fake_run)

    with pytest.raises(subprocess.CalledProcessError, match="soldr"):
        module.main()


def test_subprocess_helpers_translate_timeouts(monkeypatch: pytest.MonkeyPatch) -> None:
    module = _load_module()

    def fake_run(cmd: list[str], **kwargs: Any):
        assert kwargs["timeout"] == 30
        raise subprocess.TimeoutExpired(cmd, kwargs["timeout"])

    monkeypatch.setattr(module.subprocess, "run", fake_run)
    with pytest.raises(RuntimeError, match="version --json timed out after 30s"):
        module._check_output(["soldr", "version", "--json"])
    with pytest.raises(RuntimeError, match="status --json timed out after 30s"):
        module._run(["soldr", "status", "--json"])


def test_captured_probe_preserves_large_output_and_failure_stderr() -> None:
    module = _load_module()
    result = module._run(
        [sys.executable, "-c", "print('x' * 262144)"], capture_output=True, text=True
    )
    assert len(result.stdout) == 262145
    with pytest.raises(subprocess.CalledProcessError) as failure:
        module._run(
            [
                sys.executable,
                "-c",
                "import sys; print('partial'); print('diagnostic', file=sys.stderr); sys.exit(7)",
            ],
            capture_output=True,
            text=True,
        )
    assert failure.value.returncode == 7
    assert failure.value.stdout == "partial\n"
    assert failure.value.stderr == "diagnostic\n"


def test_failed_version_probe_prints_child_diagnostics(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("SETUP_SOLDR_PATH", sys.executable)
    monkeypatch.setenv("GITHUB_OUTPUT", str(tmp_path / "output"))
    # Python rejects the Soldr arguments with a real stderr diagnostic.
    result = run_captured(
        [sys.executable, str(SCRIPT_PATH)], capture_output=True, text=True, timeout=10
    )
    assert result.returncode != 0
    assert "version" in result.stderr
