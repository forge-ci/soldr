"""soldr#3519: ``./install`` must never write a project env into a mounted checkout.

``uv venv --no-project`` ignores ``UV_PROJECT_ENVIRONMENT`` and always writes
``.venv`` in the working directory. A container from the soldr image (which
sets ``UV_PROJECT_ENVIRONMENT=/venv``) running ``./install`` -- directly, or
via ``./lint`` / ``./test`` -- against the bind-mounted checkout therefore
left a root-owned ``.venv`` there that broke every host ``uv`` invocation.

These tests run the real script against a fake ``uv`` that records its argv,
so they assert exactly which environment path each exec path targets.

soldr#3565: CI runs this file inside a Docker container, where the script's
container refusal (``/.dockerenv``) fires before the host-path assertions can
run -- clearing ``container`` from the environment does not emulate a host
filesystem. The two host-contract cases therefore set
``SOLDR_INSTALL_CONTAINER_CHECK=off``; the container cases leave it unset so
the refusal itself stays under test.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

import pytest

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
INSTALL = REPO_ROOT / "install"

pytestmark = pytest.mark.skipif(
    os.name == "nt" or shutil.which("bash") is None,
    reason="./install is a bash script; this exercises its POSIX contract",
)


def _fake_uv(bin_dir: Path, log: Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    uv = bin_dir / "uv"
    uv.write_text(
        f"#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n",
        encoding="utf-8",
    )
    uv.chmod(0o755)


@dataclass(frozen=True)
class InstallRun:
    result: subprocess.CompletedProcess[str]
    calls: tuple[str, ...]


def _run_install(
    workdir: Path, tmp_path: Path, extra_env: dict[str, str]
) -> InstallRun:
    log = tmp_path / "uv.log"
    bin_dir = tmp_path / "bin"
    _fake_uv(bin_dir, log)
    # soldr#3565: scrub the seam too, so an ambient value in the developer
    # or CI environment cannot disable the refusal assertions below. Each
    # test opts into the seam explicitly via extra_env.
    scrubbed = {
        "UV_PROJECT_ENVIRONMENT",
        "VIRTUAL_ENV",
        "container",
        "SOLDR_INSTALL_CONTAINER_CHECK",
    }
    env = {key: value for key, value in os.environ.items() if key not in scrubbed}
    env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
    env.update(extra_env)
    result = run_captured(
        ["bash", str(INSTALL)],
        cwd=workdir,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    calls = log.read_text(encoding="utf-8").splitlines() if log.exists() else []
    return InstallRun(result, tuple(calls))


def test_container_project_environment_is_where_the_env_is_written(
    tmp_path: Path,
) -> None:
    checkout = tmp_path / "repo"
    checkout.mkdir()
    venv = tmp_path / "container-venv"

    install = _run_install(
        checkout,
        tmp_path,
        {"UV_PROJECT_ENVIRONMENT": str(venv), "container": "docker"},
    )
    result, calls = install.result, install.calls

    assert result.returncode == 0, result.stderr
    assert calls[0].startswith(f"venv {venv} "), calls
    assert calls[1].startswith(f"pip install --python {venv} "), calls
    assert not (checkout / ".venv").exists()


def test_host_default_stays_in_the_checkout(tmp_path: Path) -> None:
    checkout = tmp_path / "repo"
    checkout.mkdir()

    # soldr#3565: host-contract case -- ask the script to probe as if on a
    # host so this passes inside CI's container too.
    install = _run_install(checkout, tmp_path, {"SOLDR_INSTALL_CONTAINER_CHECK": "off"})
    result, calls = install.result, install.calls

    assert result.returncode == 0, result.stderr
    assert calls[0].startswith("venv .venv --python 3.13 "), calls
    assert calls[1].startswith("pip install --python .venv "), calls


def test_container_without_project_environment_refuses_before_uv(
    tmp_path: Path,
) -> None:
    checkout = tmp_path / "repo"
    checkout.mkdir()

    install = _run_install(checkout, tmp_path, {"container": "podman"})
    result, calls = install.result, install.calls

    assert result.returncode != 0
    assert not calls, "uv must not run, or it writes .venv into the mount"
    assert "UV_PROJECT_ENVIRONMENT" in result.stderr
    assert "soldr#3519" in result.stderr
    assert not (checkout / ".venv").exists()


def test_unusable_checkout_venv_fails_with_the_remedy(tmp_path: Path) -> None:
    checkout = tmp_path / "repo"
    (checkout / ".venv" / "bin").mkdir(parents=True)
    # The shape a container leaves behind: an interpreter link into a
    # directory the host cannot reach.
    (checkout / ".venv" / "bin" / "python").symlink_to(
        tmp_path / "root-only" / "python3.13"
    )

    # soldr#3565: host-contract case -- bypass the container refusal so the
    # broken-.venv remedy is what this asserts, on any runner.
    install = _run_install(checkout, tmp_path, {"SOLDR_INSTALL_CONTAINER_CHECK": "off"})
    result, calls = install.result, install.calls

    assert result.returncode != 0
    assert not calls
    assert "rm -rf .venv" in result.stderr
    assert "soldr#3519" in result.stderr


def test_container_check_refusal_fires_without_the_override(tmp_path: Path) -> None:
    """soldr#3565: without the seam, a container still refuses before uv.

    Guards the production default: `SOLDR_INSTALL_CONTAINER_CHECK` is the
    only thing that can suppress the soldr#3519 refusal, and only when it
    is explicitly off. Absent -- as in every real container run -- the
    refusal must still fire.
    """
    checkout = tmp_path / "repo"
    checkout.mkdir()

    for override in (None, "", "on", "maybe"):
        extra_env: dict[str, str] = {"container": "podman"}
        if override is not None:
            extra_env["SOLDR_INSTALL_CONTAINER_CHECK"] = override

        install = _run_install(checkout, tmp_path, extra_env)
        result, calls = install.result, install.calls

        assert result.returncode != 0, override
        assert not calls, (override, calls)
        assert "UV_PROJECT_ENVIRONMENT" in result.stderr, override
        assert "soldr#3519" in result.stderr, override
        assert not (checkout / ".venv").exists(), override


def test_container_check_off_suppresses_every_marker(tmp_path: Path) -> None:
    """soldr#3565: the explicit-off spellings suppress the whole probe.

    The value space mirrors env_flag.rs `EXPLICIT_OFF` (crates/soldr-core):
    trimmed, case-insensitive, and only these spellings turn the check off.
    `container` is set here so the suppression is observable even on a
    host, where the file markers are absent.
    """
    checkout = tmp_path / "repo"
    checkout.mkdir()

    for override in ("off", "OFF", " off ", "0", "false", "no"):
        install = _run_install(
            checkout,
            tmp_path,
            {"container": "podman", "SOLDR_INSTALL_CONTAINER_CHECK": override},
        )
        result, calls = install.result, install.calls

        assert result.returncode == 0, (override, result.stderr)
        assert calls[0].startswith("venv .venv --python 3.13 "), (override, calls)
