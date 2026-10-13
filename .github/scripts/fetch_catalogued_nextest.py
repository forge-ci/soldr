#!/usr/bin/env python3
"""Download and verify the target-native cargo-nextest catalog asset.

This helper is intentionally independent of soldr's runtime resolver. The
Linux cross-builder uses it to package the executable that the native
target-run worker will invoke, so a missing row or checksum mismatch is a
producer-contract failure rather than a reason to query live release APIs.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import tarfile
import tempfile
import zipfile
from dataclasses import dataclass
from pathlib import Path

from download_catalogued_asset import download_verified as materialize_asset
from toolchain_asset_query import resolve_metadata

CARGO_NEXTEST_RELEASE_PREFIX = "cargo-nextest-"


@dataclass(frozen=True)
class CatalogueQuery:
    """The `(platform, arch, extra)` selector of a catalogue row."""

    platform: str
    arch: str
    extra: str | None


def query_for_target(target: str) -> CatalogueQuery:
    table = {
        "x86_64-unknown-linux-gnu": CatalogueQuery("linux", "x86", "gnu"),
        "aarch64-unknown-linux-gnu": CatalogueQuery("linux", "arm", "gnu"),
        "x86_64-unknown-linux-musl": CatalogueQuery("linux", "x86", "musl"),
        "aarch64-unknown-linux-musl": CatalogueQuery("linux", "arm", "musl"),
        "x86_64-apple-darwin": CatalogueQuery("mac", "x86", None),
        "aarch64-apple-darwin": CatalogueQuery("mac", "arm", None),
        "x86_64-pc-windows-msvc": CatalogueQuery("windows", "x86", "msvc"),
        # nextest is a host tool. The published Windows x64 executable runs
        # the GNU-target test archive just as it runs the MSVC-target archive.
        "x86_64-pc-windows-gnu": CatalogueQuery("windows", "x86", "msvc"),
        "aarch64-pc-windows-msvc": CatalogueQuery("windows", "arm", "msvc"),
    }
    try:
        return table[target]
    except KeyError as exc:
        raise SystemExit(
            f"no cargo-nextest catalogue mapping for target {target}"
        ) from exc


def canonical_catalogue_version(version: str) -> str:
    """Return the semantic release key used by the soldr-toolchain catalog."""
    if version in {"", "latest"}:
        return version
    return version.removeprefix(CARGO_NEXTEST_RELEASE_PREFIX).removeprefix("v")


def resolve_catalogued_metadata(*, target: str, version: str, origin: str) -> dict:
    query = query_for_target(target)
    return resolve_metadata(
        tool="cargo-nextest",
        origin=origin,
        tool_manifest_url_override=None,
        platform=query.platform,
        arch=query.arch,
        extra=query.extra,
        version=canonical_catalogue_version(version),
    )


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def safe_member(name: str) -> bool:
    normalized = name.replace("\\", "/")
    if normalized.startswith("/") or (len(normalized) >= 2 and normalized[1] == ":"):
        return False
    return ".." not in normalized.split("/")


def extract_verified(archive: Path, destination: Path) -> Path:
    with tempfile.TemporaryDirectory(prefix="soldr-nextest-") as temp:
        root = Path(temp)
        if archive.name.endswith((".tar.gz", ".tgz")):
            with tarfile.open(archive, "r:gz") as handle:
                members = handle.getmembers()
                if any(
                    not safe_member(member.name) or member.issym() or member.islnk()
                    for member in members
                ):
                    raise SystemExit("cargo-nextest archive contains an unsafe path")
                handle.extractall(root)
        elif archive.name.endswith(".zip"):
            with zipfile.ZipFile(archive) as handle:
                if any(not safe_member(name) for name in handle.namelist()):
                    raise SystemExit("cargo-nextest archive contains an unsafe path")
                handle.extractall(root)
        else:
            raise SystemExit(
                f"unsupported cargo-nextest archive format: {archive.name}"
            )

        candidates = [
            path
            for path in root.rglob("cargo-nextest*")
            if path.is_file() and path.name in {"cargo-nextest", "cargo-nextest.exe"}
        ]
        if len(candidates) != 1:
            raise SystemExit(
                f"expected one cargo-nextest executable, found {len(candidates)}"
            )
        destination.mkdir(parents=True, exist_ok=True)
        output = destination / candidates[0].name
        shutil.copy2(candidates[0], output)
        if os.name != "nt":
            output.chmod(output.stat().st_mode | 0o111)
        return output


def download_verified(metadata: dict, destination: Path) -> Path:
    with tempfile.TemporaryDirectory(prefix="soldr-nextest-download-") as temp:
        archive = Path(temp) / str(metadata["filename"])
        materialize_asset(metadata, archive)
        return extract_verified(archive, destination)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--origin", default="https://zackees.github.io/soldr-toolchain")
    args = parser.parse_args()

    metadata = resolve_catalogued_metadata(
        target=args.target,
        version=args.version,
        origin=args.origin,
    )
    binary = download_verified(metadata, args.output_dir)
    metadata = {
        **metadata,
        "target": args.target,
        "binary": binary.name,
        "verified_sha256": sha256(binary),
    }
    (args.output_dir / "cargo-nextest.json").write_text(
        json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(json.dumps(metadata, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
