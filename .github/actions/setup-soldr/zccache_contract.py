#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
CONTRACT_PATH = REPO_ROOT / "contracts" / "zccache-runtime.v1.json"


@dataclass(frozen=True)
class Contract:
    """The slice of ``zccache-runtime.v1.json`` this action consumes."""

    archive_extension: str
    manifest_name: str
    manifest_min_schema_version: int
    compression_level: int
    release_required_binaries: tuple[str, ...]
    linux_zccache_target_libc: str
    zccache_embedded: bool
    zccache_required_binaries: tuple[str, ...]
    crgx_managed_version: str
    crgx_local_dir_env: str
    crgx_required_binaries: tuple[str, ...]
    cargo_chef_managed_version: str
    cargo_chef_local_dir_env: str
    cargo_chef_required_binaries: tuple[str, ...]


def load_contract(path: Path = CONTRACT_PATH) -> Contract:
    raw = json.loads(path.read_text(encoding="utf-8"))
    archive = raw["release_archive"]
    zccache = raw.get("zccache", {})
    crgx = raw["crgx"]
    cargo_chef = raw["cargo_chef"]
    return Contract(
        archive_extension=str(archive["extension"]),
        manifest_name=str(archive["manifest_name"]),
        manifest_min_schema_version=int(archive["manifest_min_schema_version"]),
        compression_level=int(archive["compression_level"]),
        release_required_binaries=tuple(archive["required_binaries"]),
        linux_zccache_target_libc=str(archive["linux_zccache_target_libc"]),
        zccache_embedded=bool(zccache.get("embedded")),
        zccache_required_binaries=tuple(zccache.get("required_binaries", ())),
        crgx_managed_version=str(crgx["managed_version"]),
        crgx_local_dir_env=str(crgx["local_dir_env"]),
        crgx_required_binaries=tuple(crgx["required_binaries"]),
        cargo_chef_managed_version=str(cargo_chef["managed_version"]),
        cargo_chef_local_dir_env=str(cargo_chef["local_dir_env"]),
        cargo_chef_required_binaries=tuple(cargo_chef["required_binaries"]),
    )


CONTRACT = load_contract()
ARCHIVE_EXT = CONTRACT.archive_extension
MANIFEST_NAME = CONTRACT.manifest_name
MANIFEST_MIN_SCHEMA_VERSION = CONTRACT.manifest_min_schema_version
ZCCACHE_BUNDLED_BINARIES = CONTRACT.zccache_required_binaries
CRGX_BUNDLED_BINARY = str(CONTRACT.crgx_required_binaries[0])
CARGO_CHEF_BUNDLED_BINARY = str(CONTRACT.cargo_chef_required_binaries[0])
RELEASE_BUNDLED_BINARIES = CONTRACT.release_required_binaries
CRGX_LOCAL_DIR_ENV = CONTRACT.crgx_local_dir_env
CARGO_CHEF_LOCAL_DIR_ENV = CONTRACT.cargo_chef_local_dir_env


def binary_name(base: str, *, windows: bool) -> str:
    return f"{base}.exe" if windows else base


def release_binary_names(*, windows: bool) -> tuple[str, ...]:
    return tuple(
        binary_name(base, windows=windows) for base in RELEASE_BUNDLED_BINARIES
    )


def zccache_target_for_soldr_target(soldr_target: str) -> str:
    if CONTRACT.zccache_embedded:
        return soldr_target
    if "-unknown-linux-" not in soldr_target:
        return soldr_target
    arch, _, _ = soldr_target.partition("-unknown-linux-")
    libc = CONTRACT.linux_zccache_target_libc
    return f"{arch}-unknown-linux-{libc}"


def _sha256_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


@dataclass(frozen=True)
class BinaryRecord:
    """A ``name`` + ``sha256`` pair recorded in the release manifest."""

    name: str
    sha256: str


@dataclass(frozen=True)
class DebugInfoEntry:
    """One ``soldr.debug_info`` manifest entry."""

    name: str
    sha256: str
    format: object


@dataclass(frozen=True)
class ReleaseManifest:
    """The validated-by-shape view of a release ``manifest.json``.

    A ``None`` field means the manifest block or key was absent or of the wrong
    JSON type; validation reports that as a mismatch.
    """

    schema_version: int | None
    archive_format: str | None
    soldr_target: str | None
    zccache_target: str | None
    crgx_target: str | None
    cargo_chef_target: str | None
    binaries: tuple[BinaryRecord, ...]
    debug_info: tuple[DebugInfoEntry, ...]


def _str_field(block: object, key: str) -> str | None:
    if isinstance(block, dict):
        value = block.get(key)
        if isinstance(value, str):
            return value
    return None


def _record(entry: object, name_key: str) -> BinaryRecord | None:
    name = _str_field(entry, name_key)
    sha = _str_field(entry, "sha256")
    if name is None or sha is None:
        return None
    return BinaryRecord(name=name, sha256=sha)


def _entry_records(entries: object) -> list[BinaryRecord]:
    if not isinstance(entries, list):
        return []
    found = (_record(entry, "name") for entry in entries)
    return [record for record in found if record is not None]


def _manifest_binaries(manifest: object) -> tuple[BinaryRecord, ...]:
    raw = manifest if isinstance(manifest, dict) else {}
    records: list[BinaryRecord] = []
    soldr = raw.get("soldr", {})
    own = _record(soldr, "binary")
    if own is not None:
        records.append(own)
    if isinstance(soldr, dict):
        records.extend(_entry_records(soldr.get("sidecars", [])))
    zccache = raw.get("zccache", {})
    if isinstance(zccache, dict):
        records.extend(_entry_records(zccache.get("binaries", [])))
    for block in ("crgx", "cargo_chef"):
        bundled = _record(raw.get(block, {}), "binary")
        if bundled is not None:
            records.append(bundled)
    return tuple(records)


def _debug_info_entries(manifest: object) -> tuple[DebugInfoEntry, ...]:
    soldr = manifest.get("soldr", {}) if isinstance(manifest, dict) else {}
    if not isinstance(soldr, dict):
        return ()
    entries = soldr.get("debug_info", [])
    if not isinstance(entries, list):
        return ()
    return tuple(
        DebugInfoEntry(
            name=entry["name"], sha256=entry["sha256"], format=entry.get("format")
        )
        for entry in entries
        if isinstance(entry, dict)
        and isinstance(entry.get("name"), str)
        and isinstance(entry.get("sha256"), str)
    )


def parse_release_manifest(manifest: object) -> ReleaseManifest:
    """Project the parsed ``manifest.json`` JSON onto a typed record."""

    raw = manifest if isinstance(manifest, dict) else {}
    schema_version = raw.get("schema_version")
    return ReleaseManifest(
        schema_version=schema_version if isinstance(schema_version, int) else None,
        archive_format=_str_field(raw.get("archive"), "format"),
        soldr_target=_str_field(raw.get("soldr"), "target"),
        zccache_target=_str_field(raw.get("zccache"), "target"),
        crgx_target=_str_field(raw.get("crgx"), "target"),
        cargo_chef_target=_str_field(raw.get("cargo_chef"), "target"),
        binaries=_manifest_binaries(raw),
        debug_info=_debug_info_entries(raw),
    )


def soldr_debug_info_entries(manifest: object) -> tuple[DebugInfoEntry, ...]:
    return parse_release_manifest(manifest).debug_info


def validate_release_manifest(  # noqa: C901
    manifest: object,
    *,
    soldr_target: str,
    windows: bool,
    extract_dir: Path,
) -> None:
    parsed = parse_release_manifest(manifest)
    schema_version = parsed.schema_version
    if schema_version is None or schema_version < MANIFEST_MIN_SCHEMA_VERSION:
        raise RuntimeError(
            f"release manifest schema_version must be >= {MANIFEST_MIN_SCHEMA_VERSION}"
        )
    if parsed.archive_format != ARCHIVE_EXT:
        raise RuntimeError(f"release manifest archive.format must be {ARCHIVE_EXT}")
    if parsed.soldr_target != soldr_target:
        raise RuntimeError(f"release manifest soldr.target must be {soldr_target}")
    expected_zccache_target = zccache_target_for_soldr_target(soldr_target)
    if parsed.zccache_target != expected_zccache_target:
        raise RuntimeError(
            f"release manifest zccache.target must be {expected_zccache_target}"
        )
    if parsed.crgx_target != soldr_target:
        raise RuntimeError(f"release manifest crgx.target must be {soldr_target}")
    if parsed.cargo_chef_target != soldr_target:
        raise RuntimeError(f"release manifest cargo_chef.target must be {soldr_target}")

    expected_names = set(release_binary_names(windows=windows))
    recorded_names = {record.name for record in parsed.binaries}
    missing = sorted(expected_names.difference(recorded_names))
    if missing:
        raise RuntimeError(
            f"release manifest is missing bundled binary records: {', '.join(missing)}"
        )

    for name in sorted(expected_names):
        # Later records win, matching the manifest's historical dict semantics.
        recorded_sha = [r.sha256 for r in parsed.binaries if r.name == name][-1]
        expected_sha = recorded_sha.lower()
        if len(expected_sha) != 64 or any(
            ch not in "0123456789abcdef" for ch in expected_sha
        ):
            raise RuntimeError(
                f"release manifest sha256 for {name} is not lowercase hex"
            )
        binary_path = locate_extracted_file(extract_dir, name)
        actual_sha = _sha256_file(binary_path)
        if actual_sha != expected_sha:
            raise RuntimeError(
                f"release manifest sha256 mismatch for {name}: expected {expected_sha}, got {actual_sha}"
            )

    debug_info = parsed.debug_info
    if windows and not debug_info:
        raise RuntimeError("release manifest is missing soldr debug_info PDB entry")
    for entry in debug_info:
        name = entry.name
        if entry.format != "pdb":
            raise RuntimeError(
                f"unsupported soldr debug_info format for {name}: {entry.format}"
            )
        if not name.lower().endswith(".pdb"):
            raise RuntimeError(f"soldr debug_info entry must name a .pdb file: {name}")
        expected_sha = entry.sha256.lower()
        if len(expected_sha) != 64 or any(
            ch not in "0123456789abcdef" for ch in expected_sha
        ):
            raise RuntimeError(
                f"release manifest sha256 for {name} is not lowercase hex"
            )
        actual_sha = _sha256_file(locate_extracted_file(extract_dir, name))
        if actual_sha != expected_sha:
            raise RuntimeError(
                f"release manifest sha256 mismatch for {name}: expected {expected_sha}, got {actual_sha}"
            )


def locate_extracted_file(root: Path, file_name: str) -> Path:
    for candidate in root.rglob(file_name):
        if candidate.is_file():
            return candidate
    raise RuntimeError(f"downloaded archive did not contain {file_name}")
