"""Tests for release-bundle portability dispatch (soldr#2469 step 2.2)."""

from __future__ import annotations

import sys
from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"

bundle = load_script_module(
    SCRIPTS / "verify_release_bundle.py", "verify_release_bundle"
)


def write_bundle(package: Path, target: str) -> list[Path]:
    package.mkdir()
    suffix = bundle.binary_suffix(target)
    paths = [package / f"{stem}{suffix}" for stem in bundle.BINARY_STEMS]
    for path in paths:
        path.write_bytes(path.name.encode())
    return paths


def test_collects_every_windows_executable_in_bundle_order(tmp_path: Path) -> None:
    package = tmp_path / "package"
    expected = write_bundle(package, "x86_64-pc-windows-msvc")

    assert bundle.bundled_binaries(package, "x86_64-pc-windows-msvc") == expected


def test_collects_every_unix_executable_in_bundle_order(tmp_path: Path) -> None:
    package = tmp_path / "package"
    expected = write_bundle(package, "aarch64-unknown-linux-musl")

    assert bundle.bundled_binaries(package, "aarch64-unknown-linux-musl") == expected


def test_missing_bundle_fails_with_the_observed_package_contents(
    tmp_path: Path,
) -> None:
    package = tmp_path / "package"
    package.mkdir()
    (package / "manifest.json").write_text("{}", encoding="utf-8")

    with pytest.raises(
        bundle.BundleVerificationError, match="no bundled binaries"
    ) as error:
        bundle.bundled_binaries(package, "x86_64-unknown-linux-gnu")

    assert "manifest.json" in str(error.value)


@dataclass(frozen=True)
class GateCase:
    """One bundle gate and the verifier script and options it must delegate to."""

    check: str
    target: str
    max_glibc: str
    expected_script: str
    expected_options: list[str]


@pytest.mark.parametrize(
    "case",
    [
        GateCase(
            "windows-imports",
            "x86_64-pc-windows-msvc",
            "2.39",
            "verify_windows_imports.py",
            [],
        ),
        GateCase(
            "macos-min-version",
            "aarch64-apple-darwin",
            "2.39",
            "verify_macos_min_version.py",
            ["--target", "aarch64-apple-darwin"],
        ),
        GateCase(
            "static",
            "x86_64-unknown-linux-musl",
            "2.39",
            "verify_static_link.py",
            [],
        ),
        GateCase(
            "glibc-baseline",
            "x86_64-unknown-linux-gnu",
            "2.39",
            "verify_glibc_baseline.py",
            ["--max-glibc", "2.39"],
        ),
    ],
)
def test_each_bundle_gate_delegates_to_its_established_verifier(
    case: GateCase, tmp_path: Path
) -> None:
    check, target, max_glibc = case.check, case.target, case.max_glibc
    expected_script, expected_options = case.expected_script, case.expected_options
    binaries = write_bundle(tmp_path / "package", target)

    command = bundle.checker_command(check, target, binaries, max_glibc)

    assert command[:2] == [sys.executable, str(SCRIPTS / expected_script)]
    assert command[2 : 2 + len(expected_options)] == expected_options
    assert command[2 + len(expected_options) :] == [str(path) for path in binaries]


@dataclass(frozen=True)
class VerifierCall:
    """One recorded `subprocess.run` invocation of a verifier."""

    command: list[str]
    check: bool


def test_verify_bundle_runs_verifier_with_all_staged_binaries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    package = tmp_path / "package"
    expected = write_bundle(package, "x86_64-unknown-linux-musl")
    calls: list[VerifierCall] = []

    def fake_run(command: list[str], check: bool) -> None:
        calls.append(VerifierCall(command, check))

    monkeypatch.setattr(bundle.subprocess, "run", fake_run)

    bundle.verify_bundle("static", "x86_64-unknown-linux-musl", package, "2.39")

    assert calls == [
        VerifierCall(
            [
                sys.executable,
                str(SCRIPTS / "verify_static_link.py"),
                *(str(path) for path in expected),
            ],
            True,
        )
    ]


def test_workflow_invokes_dispatcher_instead_of_inline_bundle_collection() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert workflow.count(".github/scripts/verify_release_bundle.py") == 4
    assert "bundled=()" not in workflow
    assert "mapfile" not in workflow
