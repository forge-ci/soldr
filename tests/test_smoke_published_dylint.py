"""Unit contracts for the published-wheel Dylint smoke (soldr#2972)."""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import (
    load_script_module,
    uv_pip_install_command,
    write_fake_soldr_console,
)

ROOT = Path(__file__).parents[1]
SCRIPTS = ROOT / ".github" / "scripts"
PUBLISHED_WORKFLOW = ROOT / ".github" / "workflows" / "published-dylint-smoke.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "auto-release.yml"
load_script_module(SCRIPTS / "catalogue_http.py", "catalogue_http")
load_script_module(SCRIPTS / "toolchain_asset_query.py", "toolchain_asset_query")
load_script_module(
    SCRIPTS / "check_dylint_driver_assets.py", "check_dylint_driver_assets"
)
smoke = load_script_module(
    SCRIPTS.parent.parent / "ci" / "smoke_published_dylint.py", "smoke_published_dylint"
)


def test_selected_version_prefers_exact_pin_and_can_monitor_pypi_latest() -> None:
    assert smoke.selected_version("v0.9.11") == "0.9.11"

    assert (
        smoke.selected_version("", lambda _url: b'{"info":{"version":"0.9.12"}}')
        == "0.9.12"
    )


def test_isolated_environment_overrides_all_user_tool_homes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    inherited_cargo = tmp_path / "inherited-cargo"
    monkeypatch.setenv("CARGO_HOME", str(inherited_cargo))
    monkeypatch.setenv(
        "PATH", os.pathsep.join([str(inherited_cargo / "bin"), "system-bin"])
    )
    for name in (
        "SOLDR_CACHE_DIR",
        "SOLDR_DYLINT_TOOLCHAIN",
        "SOLDR_DYLINT_CONFIGURED_TOOLCHAIN",
        "SOLDR_DYLINT_PREPARED_IDENTITY",
        "DYLINT_DRIVER_PATH",
        "RUSTUP_TOOLCHAIN",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "SOLDR_RUSTC_WRAPPER",
        "SOLDR_TOOLCHAIN_ORIGIN",
        "ZCCACHE_CACHE_DIR",
        "CARGO",
        "RUSTC",
    ):
        monkeypatch.setenv(name, f"ambient-{name.lower()}")
    monkeypatch.setenv("SOLDR_GITHUB_TOKEN", "auth-is-not-routing")
    env = smoke.isolated_environment(tmp_path)
    assert env["HOME"] == str(tmp_path / "home")
    assert env["USERPROFILE"] == str(tmp_path / "home")
    assert env["CARGO_HOME"] == str(tmp_path / "cargo-home")
    assert env["RUSTUP_HOME"] == str(tmp_path / "rustup-home")
    assert env["SOLDR_CACHE_DIR"] == str(tmp_path / "soldr-cache")
    assert env["UV_CACHE_DIR"] == str(tmp_path / "uv-cache")
    assert env["SOLDR_GITHUB_TOKEN"] == "auth-is-not-routing"
    assert str(inherited_cargo / "bin") not in env["PATH"]
    assert "system-bin" in env["PATH"]
    for name in (
        "SOLDR_DYLINT_TOOLCHAIN",
        "SOLDR_DYLINT_CONFIGURED_TOOLCHAIN",
        "SOLDR_DYLINT_PREPARED_IDENTITY",
        "DYLINT_DRIVER_PATH",
        "RUSTUP_TOOLCHAIN",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "SOLDR_RUSTC_WRAPPER",
        "SOLDR_TOOLCHAIN_ORIGIN",
        "ZCCACHE_CACHE_DIR",
        "CARGO",
        "RUSTC",
    ):
        assert name not in env


def test_smoke_installs_exact_wheel_and_proves_version_channel_and_driver(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = tmp_path / "repo"
    manifest = repo / "dylints" / "one" / "rust-toolchain.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text(
        '[toolchain]\nchannel = "nightly-2026-05-28"\n', encoding="utf-8"
    )
    venv = tmp_path / "venv"
    state = tmp_path / "state"
    calls: list[list[str]] = []
    soldr = venv / "Scripts" / "soldr.exe"
    probe = smoke.driver_probe_command(
        soldr,
        repo_root=repo,
        manifest=state / "driver-probe" / "Cargo.toml",
    )

    def fake_run(command: list[str], **_kwargs: object) -> SimpleNamespace:
        calls.append(command)
        if command[1:] in (["daemon", "stop"], ["broker", "stop"]):
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        if "--list" in command:
            raise AssertionError("published cargo-dylint 6 rejects deprecated --list")
        if command == ["uv", "venv", "--clear", str(venv)]:
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        if command == smoke.published_install_command(venv, "0.9.11"):
            write_fake_soldr_console(venv, windows=True)
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        if command == [str(soldr), "version", "--json"]:
            return SimpleNamespace(
                returncode=0, stdout=json.dumps({"soldr_version": "0.9.11"}), stderr=""
            )
        if command == [str(soldr), "dylint", "prepare"]:
            return SimpleNamespace(
                returncode=0, stdout="", stderr="channel nightly-2026-05-28"
            )
        if command == probe:
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        raise AssertionError(f"unexpected published-Dylint command: {command}")

    monkeypatch.setattr(smoke, "run_captured", fake_run)
    smoke.smoke(version="0.9.11", repo_root=repo, venv=venv, state_root=state)
    assert calls[0] == ["uv", "venv", "--clear", str(venv)]
    assert calls[1] == smoke.published_install_command(venv, "0.9.11")
    # soldr#3868: the private broker/daemon are stopped after the probe.
    assert calls[-2:] == smoke.teardown_commands(soldr)
    assert calls[-3] == [
        str(soldr),
        "dylint",
        "--manifest-path",
        str(state / "driver-probe" / "Cargo.toml"),
        "--path",
        str(repo / "dylints"),
        "--pattern",
        "ban_raw_env_flag",
    ]


def test_driver_probe_rejects_removed_list_flag_and_targets_only_one_lint(
    tmp_path: Path,
) -> None:
    command = smoke.driver_probe_command(
        tmp_path / "soldr.exe",
        repo_root=tmp_path / "repo",
        manifest=tmp_path / "probe" / "Cargo.toml",
    )

    assert "--list" not in command
    assert command[:2] == [str(tmp_path / "soldr.exe"), "dylint"]
    assert command[2:] == [
        "--manifest-path",
        str(tmp_path / "probe" / "Cargo.toml"),
        "--path",
        str(tmp_path / "repo" / "dylints"),
        "--pattern",
        "ban_raw_env_flag",
    ]


def test_smoke_rejects_wrong_binary_version_or_channel(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = tmp_path / "repo"
    manifest = repo / "dylints" / "one" / "rust-toolchain.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text(
        '[toolchain]\nchannel = "nightly-2026-05-28"\n', encoding="utf-8"
    )
    venv = tmp_path / "venv"

    def fake_run(command, **_kwargs):
        if command[:2] == ["uv", "pip"]:
            binary = venv / "Scripts" / "soldr.exe"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"")
        if command[-2:] == ["version", "--json"]:
            return SimpleNamespace(
                returncode=0, stdout='{"soldr_version":"0.0.1"}', stderr=""
            )
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(smoke, "run_captured", fake_run)
    with pytest.raises(smoke.PublishedDylintSmokeError, match="provenance mismatch"):
        smoke.smoke(
            version="0.9.11", repo_root=repo, venv=venv, state_root=tmp_path / "state"
        )


def test_smoke_times_out_a_stalled_probe_with_a_named_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A stall must surface as a named probe failure, not a silent job kill."""

    repo = tmp_path / "repo"
    manifest = repo / "dylints" / "one" / "rust-toolchain.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text(
        '[toolchain]\nchannel = "nightly-2026-05-28"\n', encoding="utf-8"
    )
    venv = tmp_path / "venv"
    binary = venv / "Scripts" / "soldr.exe"

    def fake_run(command, timeout=None, **_kwargs):
        if command == ["uv", "venv", "--clear", str(venv)]:
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        if command[:2] == ["uv", "pip"]:
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"")
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        if command[-2:] == ["version", "--json"]:
            return SimpleNamespace(
                returncode=0, stdout='{"soldr_version":"0.9.11"}', stderr=""
            )
        if command == [str(binary), "dylint", "prepare"]:
            return SimpleNamespace(
                returncode=0, stdout="", stderr="channel nightly-2026-05-28"
            )
        assert timeout is not None, "every probe needs a timeout budget"
        raise subprocess.TimeoutExpired(command, timeout, output=None)

    monkeypatch.setattr(smoke, "run_captured", fake_run)
    with pytest.raises(
        smoke.PublishedDylintSmokeError,
        match="timed out after 1500s.*soldr.exe.*dylint",
    ):
        smoke.smoke(
            version="0.9.11", repo_root=repo, venv=venv, state_root=tmp_path / "state"
        )


def test_windows_monitor_and_release_gate_orchestrate_the_tested_script() -> None:
    monitor = PUBLISHED_WORKFLOW.read_text(encoding="utf-8")
    release = RELEASE_WORKFLOW.read_text(encoding="utf-8")
    for trigger in ("schedule:", "workflow_dispatch:"):
        assert trigger in monitor
    assert "  push:" not in monitor
    assert "  pull_request:" not in monitor
    assert "runs-on: windows-2025" in monitor
    assert "ci/smoke_published_dylint.py" in monitor
    assert "python-version: '3.13'" in monitor
    assert "astral-sh/setup-uv@" in monitor
    assert "smoke-published-dylint:" in release
    assert (
        "needs.publish-pypi.result == 'success' || needs.publish-pypi.result == 'skipped'"
        in release
    )
    assert (
        'ci/smoke_published_dylint.py --expected-version "${{ needs.prepare.outputs.version }}"'
        in release
    )
    assert "- smoke-published-dylint" in release
    assert "needs.smoke-published-dylint.result != 'success'" in release


def test_published_install_ignores_config_and_cache_and_names_pypi(
    tmp_path: Path,
) -> None:
    """soldr#3834: resolution must not depend on runner/repo uv config."""

    venv = tmp_path / "venv"
    command = smoke.published_install_command(venv, "0.9.35")
    assert command[: len(uv_pip_install_command(venv))] == uv_pip_install_command(venv)
    for flag in ("--no-config", "--refresh", "--only-binary=:all:"):
        assert flag in command
    assert command[command.index("--index-url") + 1] == "https://pypi.org/simple"
    assert command[-1] == "soldr==0.9.35"


def test_install_retries_only_while_the_version_is_not_yet_resolvable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    attempts: list[list[str]] = []
    sleeps: list[float] = []

    def fake_run(command: list[str], **_kwargs: object) -> SimpleNamespace:
        attempts.append(command)
        if len(attempts) < 3:
            return SimpleNamespace(
                returncode=1,
                stdout="",
                stderr="error: No solution found when resolving dependencies\n"
                "  cause: Because there is no version of soldr==0.9.35 ...",
            )
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(smoke, "run_captured", fake_run)
    smoke.install_published(
        venv=tmp_path / "venv",
        version="0.9.35",
        env={},
        cwd=tmp_path,
        sleep=sleeps.append,
    )
    assert len(attempts) == 3
    assert sleeps == [smoke.INSTALL_RETRY_SECONDS] * 2


def test_install_gives_up_after_the_last_attempt_and_on_other_errors(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls = 0

    def never_resolvable(_command: list[str], **_kwargs: object) -> SimpleNamespace:
        nonlocal calls
        calls += 1
        return SimpleNamespace(
            returncode=1, stdout="", stderr="error: No solution found when resolving"
        )

    monkeypatch.setattr(smoke, "run_captured", never_resolvable)
    with pytest.raises(smoke.PublishedDylintSmokeError):
        smoke.install_published(
            venv=tmp_path / "v",
            version="9.9.9",
            env={},
            cwd=tmp_path,
            sleep=lambda _s: None,
        )
    assert calls == smoke.INSTALL_ATTEMPTS

    def network_error(_command: list[str], **_kwargs: object) -> SimpleNamespace:
        nonlocal calls
        calls += 1
        return SimpleNamespace(
            returncode=2, stdout="", stderr="error: connection reset"
        )

    calls = 0
    monkeypatch.setattr(smoke, "run_captured", network_error)
    with pytest.raises(smoke.PublishedDylintSmokeError):
        smoke.install_published(
            venv=tmp_path / "v",
            version="0.9.35",
            env={},
            cwd=tmp_path,
            sleep=lambda _s: None,
        )
    assert calls == 1, "a non-resolution failure must not be retried"


def test_teardown_is_best_effort_and_never_raises(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """soldr#3868: a failed or timed-out stop is reported, never raised."""

    soldr = tmp_path / "venv" / "bin" / "soldr"
    calls: list[list[str]] = []

    def fake_run(command: list[str], **_kwargs: object) -> SimpleNamespace:
        calls.append(command)
        if command[1:] == ["daemon", "stop"]:
            raise subprocess.TimeoutExpired(command, 120)
        return SimpleNamespace(returncode=3, stdout="", stderr="not running")

    monkeypatch.setattr(smoke, "run_captured", fake_run)
    smoke.stop_private_services(soldr, env={}, cwd=tmp_path)
    assert calls == smoke.teardown_commands(soldr)
