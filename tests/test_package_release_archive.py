"""Tests for the extracted release archive packaging step (soldr#2469)."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"

package = load_script_module(
    SCRIPTS / "package_release_archive.py", "package_release_archive"
)


def test_driver_path_tracks_the_runner_not_the_archive_target(tmp_path: Path) -> None:
    driver_dir = tmp_path / "release"

    assert package.driver_path("Windows", driver_dir) == driver_dir / "soldr.exe"
    assert package.driver_path("Linux", driver_dir) == driver_dir / "soldr"
    assert package.driver_path("macOS", driver_dir) == driver_dir / "soldr"


@dataclass(frozen=True)
class RecordedRun:
    command: list[str]
    check: bool


def test_archive_path_preserves_version_and_target(tmp_path: Path) -> None:
    assert package.archive_path("v0.9.2", "x86_64-unknown-linux-gnu", tmp_path) == (
        tmp_path / "soldr-v0.9.2-x86_64-unknown-linux-gnu.tar.zst"
    )


def test_package_invokes_the_runner_specific_driver_and_reports_size(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    package_dir = tmp_path / "package"
    package_dir.mkdir()
    output_dir = tmp_path / "dist"
    driver_dir = tmp_path / "release"
    calls: list[RecordedRun] = []

    def fake_run(command: list[str], check: bool) -> None:
        calls.append(RecordedRun(command, check))
        output = Path(command[-1])
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(b"compressed")

    monkeypatch.setattr(package.subprocess, "run", fake_run)

    archive = package.package_archive(
        version="v0.9.2",
        target="aarch64-pc-windows-msvc",
        runner_os="Windows",
        package_dir=package_dir,
        output_dir=output_dir,
        driver_dir=driver_dir,
    )

    assert calls == [
        RecordedRun(
            [
                str(driver_dir / "soldr.exe"),
                "archive",
                "--stage-dir",
                str(package_dir),
                "--output",
                str(archive),
            ],
            True,
        )
    ]
    assert archive.read_bytes() == b"compressed"
    assert "compressed_size_bytes=10" in capsys.readouterr().out


def test_missing_output_after_successful_command_is_a_release_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def successful_but_empty(command: list[str], check: bool) -> None:
        assert command
        assert check

    monkeypatch.setattr(package.subprocess, "run", successful_but_empty)

    with pytest.raises(package.ArchivePackagingError, match="did not create"):
        package.package_archive(
            version="v0.9.2",
            target="x86_64-unknown-linux-gnu",
            runner_os="Linux",
            package_dir=tmp_path / "package",
            output_dir=tmp_path / "dist",
            driver_dir=tmp_path / "release",
        )


def test_workflow_invokes_the_script_instead_of_inlining_archive_packaging() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/package_release_archive.py" in workflow
    assert '"$driver" archive --stage-dir dist/package' not in workflow
    assert "compressed_size_bytes=$(wc -c" not in workflow
