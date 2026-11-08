"""Unit tests for the extracted archive smoke gate (soldr#2469 step 2.2).

The bash this replaces is the last gate before an archive reaches the GitHub
release, and it encodes two shipped regressions. Neither was testable.
"""

from __future__ import annotations

import argparse
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"
CONTRACT = REPO_ROOT / "ci" / "canonical-targets.json"

smoke = load_script_module(
    SCRIPTS / "release_archive_smoke.py", "release_archive_smoke"
)


class TestRequiredEntries:
    def test_every_archive_ships_the_bundle_and_a_manifest(self) -> None:
        entries = smoke.required_entries("x86_64-unknown-linux-gnu", "soldr")
        assert entries == [
            "soldr",
            "soldr-daemon",
            "crgx",
            "cargo-chef",
            "manifest.json",
        ]

    def test_windows_suffixes_every_binary_but_not_the_manifest(self) -> None:
        entries = smoke.required_entries("x86_64-pc-windows-msvc", "soldr.exe")
        assert entries == [
            "soldr.exe",
            "soldr-daemon.exe",
            "crgx.exe",
            "cargo-chef.exe",
            "manifest.json",
        ]


class TestNativeArchGate:
    """The gate decides whether the dynamic checks run at all.

    A gate that is wrong in the permissive direction fails a lane loudly; wrong
    in the restrictive direction it *skips* the checks and still reports
    success, which is how a stub reaches users.
    """

    @pytest.mark.parametrize(
        ("runner_os", "runner_arch", "target", "expected"),
        [
            ("Linux", "x86_64", "x86_64-unknown-linux-gnu", True),
            ("Linux", "x86_64", "x86_64-unknown-linux-musl", True),
            ("Linux", "aarch64", "aarch64-unknown-linux-gnu", True),
            ("Windows", "x86_64", "x86_64-pc-windows-msvc", True),
            ("macOS", "x86_64", "x86_64-apple-darwin", True),
            # `uname -m` on Apple silicon is `arm64`, not `aarch64`. Dropping
            # this alias would skip every dynamic check on the macOS ARM lane
            # while still passing.
            ("macOS", "arm64", "aarch64-apple-darwin", True),
            ("Linux", "aarch64", "aarch64-apple-darwin", False),
            ("Linux", "x86_64", "aarch64-unknown-linux-gnu", False),
            ("Windows", "x86_64", "x86_64-unknown-linux-gnu", False),
            ("macOS", "arm64", "x86_64-apple-darwin", False),
        ],
    )
    def test_gate(
        self, runner_os: str, runner_arch: str, target: str, expected: bool
    ) -> None:
        assert smoke.native_arch_match(runner_os, runner_arch, target) is expected

    def test_every_contracted_target_can_be_matched_by_some_runner(self) -> None:
        """No contracted target may be permanently unexecutable.

        If a target matches no runner shape, its archive never has its binary
        run anywhere — exactly the blind spot the 2 MiB floor exists to cover.
        """
        import json

        contract = json.loads(CONTRACT.read_text(encoding="utf-8"))
        runners = [
            ("Linux", "x86_64"),
            ("Linux", "aarch64"),
            ("Windows", "x86_64"),
            ("Windows", "aarch64"),
            ("macOS", "x86_64"),
            ("macOS", "arm64"),
        ]
        for entry in contract["targets"]:
            if entry["release"]["status"] != "included":
                continue
            triple = entry["triple"]
            assert any(
                smoke.native_arch_match(os_name, arch, triple)
                for os_name, arch in runners
            ), f"{triple} matches no runner shape"


class TestStubFloor:
    def test_a_stub_is_rejected_by_name_and_issue(self) -> None:
        problem = smoke.stub_floor_problem(332 * 1024, "soldr")
        assert problem is not None
        assert str(332 * 1024) in problem and "soldr#1140" in problem

    def test_a_real_binary_passes(self) -> None:
        assert smoke.stub_floor_problem(14 * 1024 * 1024, "soldr") is None

    def test_the_floor_is_two_mib(self) -> None:
        assert smoke.MIN_SOLDR_BYTES == 2 * 1024 * 1024
        assert smoke.stub_floor_problem(smoke.MIN_SOLDR_BYTES, "soldr") is None
        assert smoke.stub_floor_problem(smoke.MIN_SOLDR_BYTES - 1, "soldr") is not None


class TestVersionJson:
    def test_empty_stdout_is_the_v0_7_87_signature(self) -> None:
        problem = smoke.version_json_problem("", "0.9.2")
        assert problem is not None and "empty stdout" in problem

    def test_whitespace_only_counts_as_empty(self) -> None:
        assert smoke.version_json_problem("  \n\t ", "0.9.2") is not None

    def test_a_matching_version_passes_in_any_formatting(self) -> None:
        for payload in (
            '{"soldr_version":"0.9.2"}',
            '{\n  "soldr_version": "0.9.2",\n  "other": 1\n}\n',
            '{ "soldr_version" : "0.9.2" }',
        ):
            assert smoke.version_json_problem(payload, "0.9.2") is None

    def test_a_mismatched_version_is_rejected(self) -> None:
        # The stub printed `soldr 0.0.1` from `--version` and passed the
        # "starts with soldr " check; this is the path that catches it.
        problem = smoke.version_json_problem('{"soldr_version":"0.0.1"}', "0.9.2")
        assert problem is not None and "0.9.2" in problem


class TestInvocationDefaults:
    def test_default_paths_follow_runner_and_release_target(
        self, tmp_path: Path
    ) -> None:
        assert smoke.archive_path("v0.9.2", "x86_64-unknown-linux-gnu", tmp_path) == (
            tmp_path / "soldr-v0.9.2-x86_64-unknown-linux-gnu.tar.zst"
        )
        assert smoke.driver_path("Windows", tmp_path) == tmp_path / "soldr.exe"
        assert smoke.driver_path("Linux", tmp_path) == tmp_path / "soldr"

    def test_main_derives_missing_driver_and_archive(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured: list[argparse.Namespace] = []
        monkeypatch.setattr(smoke, "smoke", captured.append)

        assert (
            smoke.main(
                [
                    "--version",
                    "v0.9.2",
                    "--target",
                    "x86_64-pc-windows-msvc",
                    "--binary",
                    "soldr.exe",
                    "--runner-os",
                    "Windows",
                    "--dist",
                    str(tmp_path / "dist"),
                    "--driver-dir",
                    str(tmp_path / "release"),
                ]
            )
            == 0
        )
        args = captured[0]
        assert args.archive == str(
            tmp_path / "dist" / "soldr-v0.9.2-x86_64-pc-windows-msvc.tar.zst"
        )
        assert args.driver == str(tmp_path / "release" / "soldr.exe")


def test_workflow_invokes_the_script_instead_of_inlining_the_gate() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/release_archive_smoke.py" in workflow
    assert "MIN_SOLDR_BYTES" not in workflow, (
        "the stub floor reappeared inline in auto-release.yml; the script is "
        "the single source (soldr#2469 step 2.2)"
    )
