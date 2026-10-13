"""soldr#3386: perf helpers keep and surface a failing tool's stderr.

The shell helpers in perf/lib/common.sh used to run `soldr` with
`2>/dev/null` and silently fall back to `{}`; perf_local.py captured docker
output and printed a bare warning. Each test drives the real code with a
fake tool that writes a marker to stderr.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest
from conftest import load_script_module

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).parents[1]
COMMON_SH = REPO_ROOT / "perf" / "lib" / "common.sh"
BASH = shutil.which("bash")

pytestmark = pytest.mark.skipif(
    BASH is None or sys.platform == "win32", reason="needs a POSIX bash"
)


def fake_soldr(tmp_path: Path, code: int, stdout: str, stderr: str) -> dict:
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    tool = bin_dir / "soldr"
    tool.write_text(
        f"#!/bin/sh\necho '{stdout}'\necho '{stderr}' >&2\nexit {code}\n",
        encoding="utf-8",
    )
    tool.chmod(0o755)
    env = dict(os.environ)
    env["PATH"] = f"{bin_dir}{os.pathsep}{env['PATH']}"
    return env


def run_bash(script: str, env: dict) -> subprocess.CompletedProcess:
    assert BASH is not None
    return run_captured(
        [BASH, "-c", f'. "{COMMON_SH}"; {script}'],
        capture_output=True,
        text=True,
        check=False,
        env=env,
    )


def test_session_end_failure_warns_with_stderr(tmp_path):
    env = fake_soldr(tmp_path, 1, "", "MARKER_SESSION_3386")
    result = run_bash("measure::session_end_json", env)
    # First line only: some dev hosts wrap `rm` with a shim that chats on stdout.
    assert result.stdout.splitlines()[0].strip() == "{}"
    assert "soldr session-end --json failed" in result.stderr
    assert "MARKER_SESSION_3386" in result.stderr


def test_write_cache_report_failure_keeps_stderr_log(tmp_path):
    env = fake_soldr(tmp_path, 1, "", "MARKER_REPORT_3386")
    report = tmp_path / "report.json"
    result = run_bash(f'measure::write_cache_report "{tmp_path}" "{report}"', env)
    assert report.read_text(encoding="utf-8").strip() == "{}"
    assert "MARKER_REPORT_3386" in (tmp_path / "report.json.stderr").read_text(
        encoding="utf-8"
    )
    assert "soldr cache report --json failed" in result.stderr
    assert "MARKER_REPORT_3386" in result.stderr


def test_write_cache_report_success_forwards_stderr(tmp_path):
    """soldr#3389 contract: stderr is forwarded and kept on success too."""
    env = fake_soldr(tmp_path, 0, '{"hits": 1}', "MARKER_REPORT_OK")
    report = tmp_path / "report.json"
    result = run_bash(f'measure::write_cache_report "{tmp_path}" "{report}"', env)
    assert '"hits": 1' in report.read_text(encoding="utf-8")
    assert "soldr cache report: MARKER_REPORT_OK" in result.stderr
    assert "MARKER_REPORT_OK" in (tmp_path / "report.json.stderr").read_text(
        encoding="utf-8"
    )


PERF_LOCAL = load_script_module(REPO_ROOT / "ci" / "perf_local.py", "perf_local_3386")


def fake_docker(monkeypatch, tmp_path: Path, code: int, stderr: str) -> None:
    bin_dir = tmp_path / "dockerbin"
    bin_dir.mkdir()
    tool = bin_dir / "docker"
    tool.write_text(f"#!/bin/sh\necho '{stderr}' >&2\nexit {code}\n", encoding="utf-8")
    tool.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ['PATH']}")


def test_perf_local_buildx_failure_warns_with_stderr(monkeypatch, tmp_path, capsys):
    fake_docker(monkeypatch, tmp_path, 1, "MARKER_BUILDX_3386")
    assert PERF_LOCAL._ensure_builder() is False  # pylint: disable=protected-access
    err = capsys.readouterr().err
    assert "docker buildx unavailable" in err
    assert "MARKER_BUILDX_3386" in err


def test_perf_local_gc_failure_warns_with_stderr(monkeypatch, tmp_path, capsys):
    fake_docker(monkeypatch, tmp_path, 1, "MARKER_GC_3386")
    monkeypatch.setattr(PERF_LOCAL, "_builder_exists", lambda: True)
    PERF_LOCAL.incremental_buildkit_gc()
    err = capsys.readouterr().err
    assert "soldr BuildKit GC failed" in err
    assert "MARKER_GC_3386" in err


def test_teardown_work_dir_shuts_down_each_registered_root_once(tmp_path):
    """soldr#3607: daemons are stopped (once per root) before the dir is removed."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    log = tmp_path / "calls.log"
    tool = bin_dir / "soldr"
    tool.write_text(
        '#!/bin/sh\necho "$SOLDR_CACHE_DIR|$*" >> "$CALLS_LOG"\n'
        "echo STUB_STDERR_3607 >&2\n",
        encoding="utf-8",
    )
    tool.chmod(0o755)
    env = dict(os.environ)
    env["PATH"] = f"{bin_dir}{os.pathsep}{env['PATH']}"
    env["CALLS_LOG"] = str(log)
    work = tmp_path / "work"
    script = (
        f'W="{work}"; mkdir -p "$W/a" "$W/b"; '
        'measure::register_soldr_root "$W" "$W/a"; '
        'measure::register_soldr_root "$W" "$W/b"; '
        'measure::register_soldr_root "$W" "$W/a"; '
        'measure::teardown_work_dir "$W"; echo rc=$?'
    )
    result = run_bash(script, env)
    assert "rc=0" in result.stdout
    assert "STUB_STDERR_3607" in result.stderr  # stderr is not swallowed
    assert not work.exists()
    calls = sorted(log.read_text(encoding="utf-8").splitlines())
    assert calls == [
        f"{work}/a|cache shutdown --shutdown-timeout-seconds 15 --json",
        f"{work}/b|cache shutdown --shutdown-timeout-seconds 15 --json",
    ]
