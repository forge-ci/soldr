"""Tests for the extracted Alpine musllinux wheel smoke gate (soldr#2469)."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"

smoke = load_script_module(
    SCRIPTS / "smoke_musllinux_wheel.py", "smoke_musllinux_wheel"
)


def test_expected_version_strips_only_the_release_prefix() -> None:
    assert smoke.expected_version("v0.9.2") == "0.9.2"
    assert smoke.expected_version("0.9.2") == "0.9.2"


def test_docker_command_mounts_dist_read_only_and_sets_version(tmp_path: Path) -> None:
    command = smoke.docker_command(expected="0.9.2", dist=tmp_path)

    assert command[:5] == ["docker", "run", "--rm", "-e", "EXPECTED_VERSION=0.9.2"]
    assert command[5:7] == ["-v", f"{tmp_path.resolve()}:/dist:ro"]
    assert command[7:10] == ["alpine:3.20", "sh", "-euxc"]
    assert "--only-binary=:all:" in command[-1]
    assert "soldr version --json" in command[-1]


@dataclass(frozen=True)
class RunCall:
    """One recorded `subprocess.run` invocation."""

    command: list[str]
    check: bool


def test_missing_dist_is_a_named_smoke_failure(tmp_path: Path) -> None:
    with pytest.raises(smoke.MusllinuxWheelSmokeError, match="does not exist"):
        smoke.smoke_musllinux_wheel(expected="0.9.2", dist=tmp_path / "missing")


def test_smoke_runs_the_checked_docker_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dist = tmp_path / "dist"
    dist.mkdir()
    observed: list[RunCall] = []

    def fake_run(command: list[str], *, check: bool) -> None:
        observed.append(RunCall(command, check))

    monkeypatch.setattr(smoke.subprocess, "run", fake_run)

    smoke.smoke_musllinux_wheel(expected="0.9.2", dist=dist)

    assert observed == [
        RunCall(smoke.docker_command(expected="0.9.2", dist=dist), True)
    ]


def test_workflow_invokes_the_script_instead_of_inlining_alpine_smoke() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/smoke_musllinux_wheel.py" in workflow
    assert "alpine wheel smoke test - soldr --version output" not in workflow
