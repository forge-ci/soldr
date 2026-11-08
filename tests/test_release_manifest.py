"""Unit tests for the extracted manifest writer (soldr#2469 step 2.2).

The 127-line heredoc this replaces published a corrupted manifest once
(v0.8.29) and nothing caught it, because nothing could run it.
"""

from __future__ import annotations

import json
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
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"

manifest_mod = load_script_module(SCRIPTS / "release_manifest.py", "release_manifest")


def stage(package_dir: Path, suffix: str = "", extra: dict[str, bytes] | None = None):
    package_dir.mkdir(parents=True, exist_ok=True)
    for name in ("soldr", "soldr-daemon", "crgx", "cargo-chef"):
        (package_dir / f"{name}{suffix}").write_bytes(name.encode())
    for name, payload in (extra or {}).items():
        (package_dir / name).write_bytes(payload)
    return package_dir


class TestPinnedVersions:
    def test_reads_each_pin_from_the_real_tree(self) -> None:
        for spec in (
            manifest_mod.ZCCACHE_VERSION,
            manifest_mod.CRGX_VERSION,
            manifest_mod.CARGO_CHEF_VERSION,
        ):
            value = manifest_mod.read_pinned_version(REPO_ROOT, spec)
            assert value and value.strip() == value, (spec, value)

    def test_a_missing_pin_is_an_error_not_an_empty_string(
        self, tmp_path: Path
    ) -> None:
        target = tmp_path / "some.rs"
        target.write_text("nothing here\n", encoding="utf-8")
        with pytest.raises(manifest_mod.ManifestError, match="WIDGET"):
            manifest_mod.read_pinned_version(
                tmp_path,
                manifest_mod.PinnedVersionSpec(
                    "some.rs", r'WIDGET: &str = "(.*)";', "WIDGET"
                ),
            )


class TestArtifactProvenance:
    def test_every_included_target_has_manifest_provenance(self) -> None:
        contract = json.loads(
            (REPO_ROOT / "ci" / "canonical-targets.json").read_text(encoding="utf-8")
        )
        for target in contract["targets"]:
            if target["release"]["status"] != "included":
                continue
            expected = target["release"]["artifact_provenance"]
            assert (
                manifest_mod.load_artifact_provenance(REPO_ROOT, target["triple"])
                == expected
            )

    def test_missing_target_fails_instead_of_implying_execution(
        self, tmp_path: Path
    ) -> None:
        (tmp_path / "ci").mkdir()
        (tmp_path / "ci" / "canonical-targets.json").write_text(
            '{"targets": []}', encoding="utf-8"
        )
        with pytest.raises(manifest_mod.ManifestError, match="missing"):
            manifest_mod.load_artifact_provenance(tmp_path, "aarch64-apple-darwin")


class TestDebugSidecars:
    def test_windows_requires_a_pdb(self, tmp_path: Path) -> None:
        package = stage(tmp_path / "pkg", suffix=".exe")
        with pytest.raises(manifest_mod.ManifestError, match="PDB"):
            manifest_mod.collect_debug_info(package, ".exe")

    def test_windows_pdb_is_recorded_under_either_name(self, tmp_path: Path) -> None:
        package = stage(tmp_path / "pkg", suffix=".exe", extra={"soldr_cli.pdb": b"p"})
        entries = manifest_mod.collect_debug_info(package, ".exe")
        assert [e.format for e in entries] == ["pdb"]
        assert entries[0].name == "soldr_cli.pdb"

    def test_unix_sidecars_are_optional(self, tmp_path: Path) -> None:
        package = stage(tmp_path / "pkg")
        assert manifest_mod.collect_debug_info(package, "") == []

    def test_a_dwp_staged_in_package_dir_is_never_recorded(
        self, tmp_path: Path
    ) -> None:
        """soldr#3038 regression guard.

        `soldr.debug_info` is read by every `setup-soldr` consumer via
        `zccache_contract.py::validate_release_manifest`, which hard-rejects
        any entry whose `format` is not `"pdb"`. Even if something upstream
        ever stages a `.dwp` back into `package_dir` (it should not --
        `stage_release_binaries.py` keeps it in a separate directory), this
        function must still never report it, or every Linux/macOS
        `setup-soldr` consumer breaks on the next release.
        """
        package = stage(tmp_path / "pkg", extra={"soldr.dwp": b"d"})
        assert manifest_mod.collect_debug_info(package, "") == []

    def test_a_dsym_staged_in_package_dir_is_never_recorded(
        self, tmp_path: Path
    ) -> None:
        dsym = tmp_path / "pkg" / "soldr.dSYM" / "Contents" / "Resources"
        dsym.mkdir(parents=True)
        package = stage(tmp_path / "pkg")
        (dsym / "DWARF").write_bytes(b"symbols")
        assert manifest_mod.collect_debug_info(package, "") == []


class TestManifestShape:
    def build(self, **overrides):
        base = {
            "version": "v0.9.2",
            "commit_sha": "deadbeef",
            "target": "x86_64-unknown-linux-gnu",
            "suffix": "",
            "digests": dict.fromkeys(
                ("soldr", "soldr-daemon", "crgx", "cargo-chef"), "a" * 64
            ),
            "debug_info": [],
            "versions": {"zccache": "1.13.5", "crgx": "0.1.0", "cargo_chef": "0.1.73"},
            "commits": {"crgx": "abc", "cargo_chef": "def"},
            "built_at": "2026-08-17T00:00:00Z",
            "artifact_provenance": {
                "build": {"status": "cross-built", "runner": "ubuntu-24.04"},
                "execution": {
                    "archive": {"status": "not-executed", "issue": 3071},
                    "wheel": {"status": "not-executed", "issue": 3071},
                },
            },
        }
        base.update(overrides)
        return manifest_mod.build_manifest(**base)

    def test_schema_and_required_sections(self) -> None:
        manifest = self.build()
        assert manifest["schema_version"] == 4
        for section in ("soldr", "zccache", "crgx", "cargo_chef", "archive"):
            assert section in manifest
        assert manifest["archive"] == {"format": "tar.zst", "compression_level": 19}
        assert manifest["zccache"]["embedded"] is True
        assert manifest["soldr"]["sidecars"][0]["name"] == "soldr-daemon"
        assert manifest["artifact_provenance"]["build"]["status"] == "cross-built"
        assert manifest["artifact_provenance"]["execution"] == {
            "archive": {"status": "not-executed", "issue": 3071},
            "wheel": {"status": "not-executed", "issue": 3071},
        }

    def test_windows_suffixes_every_binary_name(self) -> None:
        manifest = self.build(suffix=".exe", target="x86_64-pc-windows-msvc")
        assert manifest["soldr"]["binary"] == "soldr.exe"
        assert manifest["soldr"]["sidecars"][0]["name"] == "soldr-daemon.exe"
        assert manifest["crgx"]["binary"] == "crgx.exe"
        assert manifest["cargo_chef"]["binary"] == "cargo-chef.exe"

    def test_a_multiline_source_commit_cannot_corrupt_the_document(self) -> None:
        """The v0.8.29 failure, made structurally impossible.

        A `$GITHUB_ENV` writer terminated lines with a literal backslash-n, so
        the heredoc interpolated a two-line value into `crgx.source_commit`
        and `cargo_chef.source_commit` came out `"unknown"`. `json.dumps`
        escapes the newline instead of letting it end a field.
        """
        leaked = (
            "soldr-toolchain:v0.1.0\nCARGO_CHEF_SOURCE_COMMIT=soldr-toolchain:v0.1.73"
        )
        manifest = self.build(commits={"crgx": leaked, "cargo_chef": "clean"})
        reparsed = json.loads(json.dumps(manifest))
        assert reparsed["crgx"]["source_commit"] == leaked
        assert reparsed["cargo_chef"]["source_commit"] == "clean"


def test_end_to_end_writes_a_manifest_the_verifier_accepts(tmp_path: Path) -> None:
    """The writer and `verify_release_manifest.py` must agree.

    They are two halves of one contract — a manifest the verifier rejects
    fails the release lane — so pinning them together is worth more than
    either in isolation.
    """
    package = stage(tmp_path / "pkg")
    exit_code = manifest_mod.main(
        [
            "--version",
            "v0.9.2",
            "--commit-sha",
            "c0ffee",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--package-dir",
            str(package),
            "--crgx-source-commit",
            "soldr-toolchain:v0.1.0",
        ]
    )
    assert exit_code == 0

    written = json.loads((package / "manifest.json").read_text(encoding="utf-8"))
    assert written["soldr"]["commit_sha"] == "c0ffee"
    assert written["crgx"]["source_commit"] == "soldr-toolchain:v0.1.0"
    assert written["cargo_chef"]["source_commit"] == "unknown"
    # soldr#3038: a Linux/macOS release ships no debug_info at all -- the
    # dwp/dsym sidecar goes out as a separate, unmanifested asset instead.
    # See TestDebugSidecars.test_a_dwp_staged_in_package_dir_is_never_recorded.
    assert written["soldr"]["debug_info"] == []

    verify = run_captured(
        [
            "python3" if Path("/usr/bin/python3").exists() else "python",
            str(SCRIPTS / "verify_release_manifest.py"),
            str(package / "manifest.json"),
            "--package-dir",
            str(package),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert verify.returncode == 0, verify.stdout + verify.stderr


def test_workflow_invokes_the_script_instead_of_inlining_the_heredoc() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/release_manifest.py" in workflow
    assert '"schema_version": 3' not in workflow, (
        "the manifest body reappeared inline in auto-release.yml; the script "
        "is the single source (soldr#2469 step 2.2)"
    )
