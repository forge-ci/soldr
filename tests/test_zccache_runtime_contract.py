from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
CONTRACT_PATH = REPO_ROOT / "contracts" / "zccache-runtime.v1.json"
PY_CONTRACT_PATH = (
    REPO_ROOT / ".github" / "actions" / "setup-soldr" / "zccache_contract.py"
)


def _load_py_contract() -> Any:
    return load_script_module(PY_CONTRACT_PATH, "zccache_contract")


def _sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


@dataclass(frozen=True)
class ManifestFixture:
    """A release manifest written for the validator, which takes raw JSON."""

    document: dict  # raw manifest JSON, mutated by tests to break one field


def _write_manifest_fixture(root: Path, *, windows: bool = False) -> ManifestFixture:
    module = _load_py_contract()
    names = module.release_binary_names(windows=windows)
    payloads: dict[str, bytes] = {}
    for name in names:
        payload = f"{name}\n".encode("utf-8")
        payloads[name] = payload
        (root / name).write_bytes(payload)

    suffix = ".exe" if windows else ""
    soldr_debug_info: list[dict[str, str]] = []
    if windows:
        pdb_name = "soldr.pdb"
        payloads[pdb_name] = b"soldr pdb\n"
        (root / pdb_name).write_bytes(payloads[pdb_name])
        soldr_debug_info.append(
            {
                "name": pdb_name,
                "sha256": _sha256(payloads[pdb_name]),
                "format": "pdb",
            }
        )
    return ManifestFixture(
        {
            "schema_version": 3,
            "soldr": {
                "version": "0.7.39",
                "target": "x86_64-unknown-linux-gnu",
                "binary": f"soldr{suffix}",
                "sha256": _sha256(payloads[f"soldr{suffix}"]),
                "sidecars": [
                    {
                        "name": f"{base}{suffix}",
                        "sha256": _sha256(payloads[f"{base}{suffix}"]),
                    }
                    for base in module.RELEASE_BUNDLED_BINARIES
                    if base not in {"soldr", "crgx", "cargo-chef"}
                ],
                "debug_info": soldr_debug_info,
                "commit_sha": "abc123",
            },
            "zccache": {
                "version": "embedded",
                "target": "x86_64-unknown-linux-gnu",
                "embedded": True,
            },
            "crgx": {
                "version": module.CONTRACT.crgx_managed_version,
                "target": "x86_64-unknown-linux-gnu",
                "binary": f"crgx{suffix}",
                "sha256": _sha256(payloads[f"crgx{suffix}"]),
                "source_commit": "def456",
            },
            "cargo_chef": {
                "version": module.CONTRACT.cargo_chef_managed_version,
                "target": "x86_64-unknown-linux-gnu",
                "binary": f"cargo-chef{suffix}",
                "sha256": _sha256(payloads[f"cargo-chef{suffix}"]),
                "source_commit": "789abc",
            },
            "archive": {
                "format": module.ARCHIVE_EXT,
                "compression_level": module.CONTRACT.compression_level,
            },
            "built_at": "2026-05-27T00:00:00Z",
        }
    )


def test_contract_json_has_expected_shape() -> None:
    contract = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))

    assert contract["schema_version"] == 1
    assert contract["release_archive"]["extension"] == "tar.zst"
    assert contract["release_archive"]["manifest_min_schema_version"] == 3
    assert contract["release_archive"]["required_binaries"] == [
        "soldr",
        "soldr-daemon",
        "crgx",
        "cargo-chef",
    ]
    assert contract["zccache"]["embedded"] is True
    assert "required_binaries" not in contract["zccache"]
    assert contract["crgx"]["local_dir_env"] == "SOLDR_CRGX_LOCAL_DIR"
    assert contract["crgx"]["required_binaries"] == ["crgx"]
    assert contract["cargo_chef"]["local_dir_env"] == "SOLDR_CARGO_CHEF_LOCAL_DIR"
    assert contract["cargo_chef"]["required_binaries"] == ["cargo-chef"]
    assert contract["maturin"] == {
        "managed_version": "1.14.1.post1",
        "pypi_package": "soldr-maturin",
        "repository": "zackees/soldr-maturin",
        "required_binaries": ["maturin"],
        "manifest_block": "maturin",
    }


def test_maturin_contract_matches_rust_constants_and_registry() -> None:
    contract = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))["maturin"]
    fetch_mod = (
        REPO_ROOT / "crates" / "soldr-fetch" / "src" / "fetch" / "mod.rs"
    ).read_text(encoding="utf-8")
    known_tools = (
        REPO_ROOT / "crates" / "soldr-fetch" / "src" / "fetch" / "known_tools.rs"
    ).read_text(encoding="utf-8")

    assert (
        f'MANAGED_MATURIN_VERSION: &str = "{contract["managed_version"]}"' in fetch_mod
    )
    assert (
        f'MANAGED_MATURIN_PYPI_PACKAGE: &str = "{contract["pypi_package"]}"'
        in fetch_mod
    )
    owner, repo = contract["repository"].split("/", 1)
    assert f'repo: Some(("{owner}", "{repo}"))' in known_tools


def test_python_contract_validates_release_manifest_sha256s(tmp_path: Path) -> None:
    module = _load_py_contract()
    manifest = _write_manifest_fixture(tmp_path).document

    module.validate_release_manifest(
        manifest,
        soldr_target="x86_64-unknown-linux-gnu",
        windows=False,
        extract_dir=tmp_path,
    )

    manifest["soldr"]["sidecars"][0]["sha256"] = "0" * 64
    with pytest.raises(RuntimeError, match="sha256 mismatch"):
        module.validate_release_manifest(
            manifest,
            soldr_target="x86_64-unknown-linux-gnu",
            windows=False,
            extract_dir=tmp_path,
        )


def test_python_contract_validates_windows_soldr_pdb_sha256(tmp_path: Path) -> None:
    module = _load_py_contract()
    manifest = _write_manifest_fixture(tmp_path, windows=True).document
    manifest["soldr"]["target"] = "x86_64-pc-windows-msvc"
    manifest["zccache"]["target"] = "x86_64-pc-windows-msvc"
    manifest["crgx"]["target"] = "x86_64-pc-windows-msvc"
    manifest["cargo_chef"]["target"] = "x86_64-pc-windows-msvc"

    module.validate_release_manifest(
        manifest,
        soldr_target="x86_64-pc-windows-msvc",
        windows=True,
        extract_dir=tmp_path,
    )

    manifest["soldr"]["debug_info"][0]["sha256"] = "0" * 64
    # `soldr\.pdb` escaped: `.` is a regex wildcard, so the unescaped form
    # also matched "soldrXpdb". The assertion is about a literal filename.
    with pytest.raises(RuntimeError, match=r"soldr\.pdb"):
        module.validate_release_manifest(
            manifest,
            soldr_target="x86_64-pc-windows-msvc",
            windows=True,
            extract_dir=tmp_path,
        )


def test_python_contract_requires_windows_soldr_pdb(tmp_path: Path) -> None:
    module = _load_py_contract()
    manifest = _write_manifest_fixture(tmp_path, windows=True).document
    manifest["soldr"]["target"] = "x86_64-pc-windows-msvc"
    manifest["zccache"]["target"] = "x86_64-pc-windows-msvc"
    manifest["crgx"]["target"] = "x86_64-pc-windows-msvc"
    manifest["cargo_chef"]["target"] = "x86_64-pc-windows-msvc"
    manifest["soldr"]["debug_info"] = []

    with pytest.raises(RuntimeError, match="missing soldr debug_info PDB"):
        module.validate_release_manifest(
            manifest,
            soldr_target="x86_64-pc-windows-msvc",
            windows=True,
            extract_dir=tmp_path,
        )


def test_python_action_helpers_import_contract_constants() -> None:
    module = _load_py_contract()
    ensure_soldr = load_script_module(
        REPO_ROOT / ".github" / "actions" / "setup-soldr" / "ensure_soldr.py",
        "ensure_soldr",
    )

    assert ensure_soldr.ARCHIVE_EXT == module.ARCHIVE_EXT
    assert ensure_soldr.RELEASE_BUNDLED_BINARIES == module.RELEASE_BUNDLED_BINARIES
    assert ensure_soldr.CRGX_BUNDLED_BINARY == module.CRGX_BUNDLED_BINARY
    assert ensure_soldr.CARGO_CHEF_BUNDLED_BINARY == module.CARGO_CHEF_BUNDLED_BINARY


def test_release_workflow_and_docs_reference_contract_layout() -> None:
    contract = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))
    release_workflow = (
        REPO_ROOT / ".github" / "workflows" / "auto-release.yml"
    ).read_text(encoding="utf-8")
    npm_docs = (REPO_ROOT / "docs" / "NPM_PUBLISHING.md").read_text(encoding="utf-8")
    runtime_docs = (REPO_ROOT / "docs" / "ZCCACHE_RUNTIME_CONTRACT.md").read_text(
        encoding="utf-8"
    )

    # soldr#2469 step 2.2: the manifest body moved out of a heredoc in
    # auto-release.yml into release_manifest.py, so the shape it must produce
    # is asserted at its new home. Kept as three separate facts rather than a
    # substring sweep of the script: the schema version, the archive format,
    # and that debug sidecars are still emitted at all.
    manifest_writer = (
        REPO_ROOT / ".github" / "scripts" / "release_manifest.py"
    ).read_text(encoding="utf-8")
    assert "SCHEMA_VERSION = 4" in manifest_writer
    assert 'ARCHIVE_FORMAT = "tar.zst"' in manifest_writer
    assert '"debug_info": debug_info' in manifest_writer
    assert ".github/scripts/release_manifest.py" in release_workflow
    assert "CARGO_PROFILE_RELEASE_DEBUG" in release_workflow
    for base in contract["release_archive"]["required_binaries"]:
        # The workflow still stages and verifies each binary by name; the
        # manifest writer names them too, now that it composes the entries.
        assert base in release_workflow or base in manifest_writer
        assert base in npm_docs
    assert "contracts/zccache-runtime.v1.json" in npm_docs
    assert "contracts/zccache-runtime.v1.json" in runtime_docs


def test_npm_package_exports_contract_files() -> None:
    package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))

    assert "contracts/zccache-runtime.v1.json" in package["files"]
    assert "contracts/zccache-integration-guardrails.v1.json" in package["files"]
    assert "scripts/zccache-contract.js" in package["files"]


def test_ci_cleanup_never_invokes_removed_bare_zccache_alias() -> None:
    offenders: list[str] = []
    for root in (
        REPO_ROOT / ".github" / "workflows",
        REPO_ROOT / ".github" / "actions",
    ):
        for path in (*root.rglob("*.yml"), *root.rglob("*.yaml")):
            for line_number, line in enumerate(
                path.read_text(encoding="utf-8").splitlines(), 1
            ):
                if line.lstrip().startswith("#"):
                    continue
                match = re.search(r"\bzccache\s+stop\b", line)
                if match and "soldr" not in line[: match.start()]:
                    offenders.append(
                        f"{path.relative_to(REPO_ROOT)}:{line_number}: {line.strip()}"
                    )
    assert not offenders, "bare zccache cleanup commands:\n" + "\n".join(offenders)
