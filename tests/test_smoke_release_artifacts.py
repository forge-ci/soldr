"""Unit coverage for ci/smoke_release_artifacts.py (soldr#2294, soldr#3076).

The native release smokes (smoke_macos_x64, smoke_windows in
auto-release.yml) execute this script; the target-dependent decisions are
pure functions so they are pinned here instead of being discovered on a
release run.
"""

import hashlib
import zipfile
from pathlib import Path

import pytest
from conftest import assert_recovery_verify_collected_contract, load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE = load_script_module(
    REPO_ROOT / "ci" / "smoke_release_artifacts.py", "smoke_release_artifacts"
)


def test_windows_targets_use_exe_suffix_everywhere() -> None:
    for target in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
        assert MODULE.exe_suffix(target) == ".exe"
        members = MODULE.required_members(target)
        assert "soldr.exe" in members
        assert "soldr-daemon.exe" in members
        assert "crgx.exe" in members
        assert "cargo-chef.exe" in members
        assert "manifest.json" in members


def test_unix_targets_have_no_suffix() -> None:
    for target in ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu"):
        assert MODULE.exe_suffix(target) == ""
        assert MODULE.required_members(target)[0] == "soldr"


def test_macho_arch_only_for_darwin() -> None:
    assert MODULE.macho_arch("aarch64-apple-darwin") == "arm64"
    assert MODULE.macho_arch("x86_64-apple-darwin") == "x86_64"
    assert MODULE.macho_arch("x86_64-pc-windows-msvc") is None
    assert MODULE.macho_arch("aarch64-unknown-linux-gnu") is None


def test_stub_floor_matches_the_workflow_contract() -> None:
    # 2 MiB, the soldr#1140 / soldr#1202 stub-binary floor used by the
    # inline archive smoke in auto-release.yml.
    assert MODULE.MIN_SOLDR_BYTES == 2 * 1024 * 1024


def test_workflow_smoke_jobs_invoke_the_script() -> None:
    release = (REPO_ROOT / ".github" / "workflows" / "auto-release.yml").read_text(
        encoding="utf-8"
    )
    assert release.count("ci/smoke_release_artifacts.py") >= 2
    assert "smoke_macos_x64:" in release
    assert "smoke_windows:" in release


# ---------- host-side checks that need no target execution ----------


def _write_wheel(tmp_path: Path, *, version: str) -> Path:
    wheel = tmp_path / f"soldr-{version}-cp313-abi3-macosx_11_0_arm64.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(
            "soldr-0.dist-info/METADATA",
            f"Metadata-Version: 2.1\nName: soldr\nVersion: {version}\n",
        )
    return wheel


def test_wheel_version_reads_metadata_without_executing_anything(
    tmp_path: Path,
) -> None:
    wheel = _write_wheel(tmp_path, version="0.9.10")
    assert MODULE.wheel_version(wheel) == "0.9.10"


def test_wheel_version_requires_exactly_one_metadata_entry(tmp_path: Path) -> None:
    wheel = tmp_path / "empty.whl"
    with zipfile.ZipFile(wheel, "w"):
        pass
    with pytest.raises(RuntimeError, match="exactly one"):
        MODULE.wheel_version(wheel)


def test_check_macho_architecture_accepts_a_matching_cputype(
    tmp_path: Path,
) -> None:
    binary = tmp_path / "soldr"
    # MH_MAGIC_64 + CPU_TYPE_ARM64, both little-endian, padded to 16 bytes.
    binary.write_bytes(MODULE.MACHO_MAGIC_64 + b"\x0c\x00\x00\x01" + b"\x00" * 8)
    MODULE.check_macho_architecture(binary, "arm64")  # must not raise/exit


def test_check_macho_architecture_rejects_a_mismatched_cputype(
    tmp_path: Path,
) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(MODULE.MACHO_MAGIC_64 + b"\x0c\x00\x00\x01" + b"\x00" * 8)
    with pytest.raises(SystemExit, match="expected Mach-O cputype for x86_64"):
        MODULE.check_macho_architecture(binary, "x86_64")


def test_check_macho_architecture_rejects_a_non_macho_file(tmp_path: Path) -> None:
    binary = tmp_path / "soldr.exe"
    binary.write_bytes(b"MZ\x90\x00" + b"\x00" * 12)
    with pytest.raises(SystemExit, match="not a 64-bit Mach-O binary"):
        MODULE.check_macho_architecture(binary, "x86_64")


def test_run_passes_the_argv_to_subprocess(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, list[str]] = {}

    class _Result:  # pylint: disable=too-few-public-methods
        stdout = "soldr 0.9.10\n"

    def fake_run(argv: list[str], **_kwargs: object) -> _Result:
        captured["argv"] = argv
        return _Result()

    monkeypatch.setattr(MODULE, "run_captured", fake_run)
    result = MODULE.run([Path("/extracted/soldr"), "--version"])
    assert captured["argv"] == ["/extracted/soldr", "--version"]
    assert result.stdout == "soldr 0.9.10\n"


# ---------- soldr#3076: Recovery guest-script generation ----------


def test_copy_into_share_dir_stages_every_guest_binary(tmp_path: Path) -> None:
    extract = tmp_path / "extracted"
    extract.mkdir()
    for binary in MODULE.GUEST_BINARY_CHECKS:
        name = binary.name
        (extract / name).write_bytes(b"fake binary")
    share = tmp_path / "share"
    MODULE.copy_into_share_dir(extract, share, "")
    for binary in MODULE.GUEST_BINARY_CHECKS:
        name = binary.name
        assert (share / name).read_bytes() == b"fake binary"


def test_copy_into_share_dir_fails_when_a_binary_is_missing(tmp_path: Path) -> None:
    extract = tmp_path / "extracted"
    extract.mkdir()
    share = tmp_path / "share"
    with pytest.raises(SystemExit, match="missing"):
        MODULE.copy_into_share_dir(extract, share, "")


def test_stage_macos_wheel_runtime_is_content_addressed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    wheel = _write_wheel(tmp_path, version="0.9.10")
    payload = b"portable cpython archive"
    observed: dict[str, str] = {}

    def fake_download(url: str, destination: Path, expected_sha256: str) -> None:
        observed["url"] = url
        assert expected_sha256 == hashlib.sha256(payload).hexdigest()
        destination.write_bytes(payload)

    monkeypatch.setattr(MODULE, "download_verified", fake_download)
    monkeypatch.setattr(
        MODULE, "MACOS_X64_PYTHON_SHA256", hashlib.sha256(payload).hexdigest()
    )
    share = tmp_path / "share"
    staged_name = MODULE.stage_macos_wheel_runtime(wheel, share)

    assert observed["url"] == MODULE.MACOS_X64_PYTHON_URL
    assert (share / MODULE.MACOS_X64_PYTHON_ARCHIVE).read_bytes() == payload
    assert staged_name == wheel.name
    assert (share / wheel.name).read_bytes() == wheel.read_bytes()


def test_download_verified_rejects_wrong_portable_python_digest(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    class _Response:
        def __init__(self) -> None:
            self.done = False

        def __enter__(self):
            return self

        def __exit__(self, *_args: object) -> None:
            return None

        def read(self, _size: int) -> bytes:
            if self.done:
                return b""
            self.done = True
            return b"wrong bytes"

    monkeypatch.setattr(
        MODULE.urllib.request, "urlopen", lambda *_args, **_kwargs: _Response()
    )
    with pytest.raises(SystemExit, match="SHA-256 mismatch"):
        MODULE.download_verified(
            "https://example.invalid/python.tar.gz",
            tmp_path / "python.tar.gz",
            "0" * 64,
        )


def test_build_release_guest_script_fetches_every_binary_by_basename() -> None:
    script = MODULE.build_release_guest_script(
        "0.9.11", "soldr-0.9.11-cp310-abi3-macosx_11_0_x86_64.whl"
    )
    assert script.startswith("#!/bin/sh")
    for binary in MODULE.GUEST_BINARY_CHECKS:
        name = binary.name
        assert f"curl -fsS -o /tmp/{name} {MODULE.GUEST_HTTP_BASE}/{name}" in script
    assert MODULE.MACOS_X64_PYTHON_ARCHIVE in script
    assert "-m pip install --no-index" in script
    assert "import soldr;" in script
    assert 'exit "$FAIL"' in script


def test_build_release_guest_script_checks_the_expected_version() -> None:
    script = MODULE.build_release_guest_script(
        "0.9.11", "soldr-0.9.11-cp310-abi3-macosx_11_0_x86_64.whl"
    )
    assert '"soldr_version": "0.9.11"' in script


def test_build_release_guest_script_help_check_for_daemon() -> None:
    script = MODULE.build_release_guest_script(
        "0.9.11", "soldr-0.9.11-cp310-abi3-macosx_11_0_x86_64.whl"
    )
    assert "/tmp/soldr-daemon --help" in script
    assert 'echo "soldr-daemon_help=pass"' in script


def test_parse_summary_splits_status_and_detail() -> None:
    text = "arch=pass:x86_64\nversion=fail:not soldr\nempty=\n\n"
    results = MODULE.parse_summary(text)
    assert MODULE.find_result(results, "arch") == MODULE.CheckResult(
        "arch", True, "x86_64"
    )
    assert MODULE.find_result(results, "version") == MODULE.CheckResult(
        "version", False, "not soldr"
    )
    assert MODULE.find_result(results, "empty") == MODULE.CheckResult(
        "empty", False, ""
    )


def test_parse_summary_flags_malformed_lines() -> None:
    results = MODULE.parse_summary("not a key value line\n")
    assert MODULE.find_result(results, "summary_line_1") == MODULE.CheckResult(
        "summary_line_1",
        False,
        "malformed line: 'not a key value line'",
    )


def _passing_summary_lines() -> list[str]:
    lines = ["arch=pass:x86_64"]
    for binary in MODULE.GUEST_BINARY_CHECKS:
        name, check = binary.name, binary.check
        lines.append(f"fetch_{name}=pass")
        lines.append(f"{name}_{check}=pass:ok")
    lines.append('soldr_version_json=pass:{"soldr_version": "0.9.11"}')
    lines.extend(
        [
            "fetch_python_runtime=pass",
            "fetch_wheel=pass",
            "python_runtime=pass:Python 3.13.15",
            "wheel_install=pass",
            "wheel_import=pass:soldr",
            "wheel_version=pass:soldr 0.9.11",
            'wheel_version_json=pass:{"soldr_version": "0.9.11"}',
        ]
    )
    return lines


def test_verify_collected_matches_the_shared_recovery_contract(tmp_path: Path) -> None:
    assert_recovery_verify_collected_contract(
        MODULE, tmp_path, passing_lines=_passing_summary_lines()
    )


def test_wheel_import_targets_a_module_the_wheel_ships() -> None:
    """The release wheel is a maturin *binary* wheel: `crates/soldr-cli` has
    no pyo3 library, so `[tool.maturin] module-name` builds no extension and
    the wheel ships only the `src/soldr` package plus the `soldr` script.
    Importing a module the wheel does not contain fails every release."""
    module = MODULE.WHEEL_IMPORT_MODULE
    package_root = Path(__file__).resolve().parents[1] / "src"
    parts = module.split(".")
    as_package = package_root.joinpath(*parts, "__init__.py")
    as_module = package_root.joinpath(*parts[:-1], f"{parts[-1]}.py")
    assert as_package.is_file() or as_module.is_file(), module
    script = MODULE.build_release_guest_script(
        "0.9.11", "soldr-0.9.11-cp310-abi3-macosx_11_0_x86_64.whl"
    )
    assert f"import {module}" in script
