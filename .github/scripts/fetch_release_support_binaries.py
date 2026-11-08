#!/usr/bin/env python3
"""Fetch the release's crgx and cargo-chef support binaries (soldr#2469).

The release archive contains more than the soldr executable.  `crgx` and
`cargo-chef` are resolved from the soldr-toolchain catalogue for each release
target, checked against the catalogue's SHA-256, and staged beside soldr before
`release_manifest.py` records their provenance.

This was inline Python in `auto-release.yml`, where neither target mapping nor
catalogue integrity behavior was unit-testable.  Keep release workflow YAML to
orchestration: this script owns the fetch, verification, extraction, and
staging logic.

Usage (CI):
    python3 .github/scripts/fetch_release_support_binaries.py \
        --target x86_64-unknown-linux-gnu \
        --driver target/release/soldr \
        --issue-url https://github.com/zackees/soldr-toolchain/issues/47
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path, PureWindowsPath
from urllib.parse import urljoin

from catalogue_http import display_url, open_url, validate_https_url
from release_artifacts import binary_suffix


@dataclass(frozen=True)
class PinnedConstant:
    """A Rust string constant: the file holding it and its name."""

    relative: str
    constant: str


@dataclass(frozen=True)
class ToolchainPlatform:
    """A soldr-toolchain platform selector; `libc`/`abi` are absent when `None`."""

    os: str
    arch: str
    libc: str | None = None
    abi: str | None = None

    def to_dict(self) -> dict:
        """The catalogue's JSON platform object (absent keys omitted)."""
        wire = {"os": self.os, "arch": self.arch}
        if self.libc is not None:
            wire["libc"] = self.libc
        if self.abi is not None:
            wire["abi"] = self.abi
        return wire


@dataclass(frozen=True)
class ToolCatalog:
    """A verified per-tool catalogue: its URL and the decoded JSON document."""

    url: str
    document: dict


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_ORIGIN = "https://zackees.github.io/soldr-toolchain"
DEFAULT_ISSUE_URL = "https://github.com/zackees/soldr-toolchain/issues/47"
CRGX_VERSION = PinnedConstant(
    "crates/soldr-fetch/src/fetch/mod.rs", "MANAGED_CRGX_VERSION"
)
CARGO_CHEF_VERSION = PinnedConstant(
    "crates/soldr-fetch/src/fetch/known_tools.rs",
    "CARGO_CHEF_PINNED_VERSION",
)
MAX_PART_BYTES = 95 * 1024 * 1024


class SupportBinaryError(RuntimeError):
    """A catalogue, archive, or staging defect that must stop the release."""


def platform_for_target(target: str) -> ToolchainPlatform:
    """Translate Soldr's target triple to a soldr-toolchain platform object."""
    table = {
        "x86_64-unknown-linux-gnu": ToolchainPlatform("linux", "x86_64", libc="glibc"),
        "aarch64-unknown-linux-gnu": ToolchainPlatform(
            "linux", "aarch64", libc="glibc"
        ),
        "x86_64-unknown-linux-musl": ToolchainPlatform("linux", "x86_64", libc="musl"),
        "aarch64-unknown-linux-musl": ToolchainPlatform(
            "linux", "aarch64", libc="musl"
        ),
        "x86_64-apple-darwin": ToolchainPlatform("darwin", "x86_64"),
        "aarch64-apple-darwin": ToolchainPlatform("darwin", "aarch64"),
        "x86_64-pc-windows-msvc": ToolchainPlatform("windows", "x86_64", abi="msvc"),
        "aarch64-pc-windows-msvc": ToolchainPlatform("windows", "aarch64", abi="msvc"),
    }
    try:
        return table[target]
    except KeyError as error:
        raise SupportBinaryError(
            f"no soldr-toolchain platform mapping for {target}"
        ) from error


def read_pinned_version(root: Path, spec: PinnedConstant) -> str:
    """Read a Rust string constant without maintaining a second version pin."""
    relative, constant = spec.relative, spec.constant
    text = (root / relative).read_text(encoding="utf-8")
    match = re.search(rf'{constant}\s*:\s*&str\s*=\s*"([^"]+)"', text)
    if not match:
        raise SupportBinaryError(f"could not read {constant} from {relative}")
    return match.group(1)


def read_url(url: str, timeout: int) -> bytes:
    try:
        validate_https_url(url, label="soldr-toolchain URL")
        request = urllib.request.Request(url, headers={"Accept-Encoding": "identity"})
        with open_url(request, timeout=timeout) as response:
            return response.read()
    except SystemExit as error:
        raise SupportBinaryError(str(error)) from None


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def is_sha256(value: str) -> bool:
    return len(value) == 64 and all(
        character in "0123456789abcdef" for character in value
    )


def download_verified(urls: list[object], expected_sha: str, destination: Path) -> str:
    """Download the first reachable, digest-matching catalogue mirror."""
    if not is_sha256(expected_sha):
        raise SupportBinaryError("catalogued asset has no valid sha256")
    last_error: urllib.error.URLError | None = None
    for url in urls:
        source = str(url)
        try:
            destination.write_bytes(read_url(source, timeout=600))
        except urllib.error.URLError as error:
            last_error = error
            print(
                f"could not fetch {display_url(source)}: {type(error).__name__}; "
                "trying the next catalogue URL"
            )
            continue
        actual_sha = sha256_file(destination)
        if actual_sha != expected_sha:
            raise SupportBinaryError(
                f"catalogued asset sha256 mismatch from {display_url(source)}: "
                f"expected {expected_sha}, got {actual_sha}"
            )
        return source
    error_kind = type(last_error).__name__ if last_error is not None else "unknown"
    raise SupportBinaryError(f"all catalogue URLs failed: {error_kind}")


def download_catalogued_asset(asset: dict, destination: Path) -> str:  # noqa: C901
    """Download a direct asset or reconstruct a catalogue-v2 multipart asset."""
    expected_sha = str(asset.get("sha256", "")).lower()
    if not is_sha256(expected_sha):
        raise SupportBinaryError("catalogued asset has no valid sha256")
    declared_size = asset.get("size_bytes")
    if (
        not isinstance(declared_size, int)
        or isinstance(declared_size, bool)
        or declared_size <= 0
    ):
        raise SupportBinaryError(
            f"catalogued asset has invalid size_bytes {declared_size!r}"
        )

    urls = asset.get("urls") or []
    parts = asset.get("parts") or []
    if urls and parts:
        raise SupportBinaryError(
            "catalogued asset mixes direct URLs and multipart data"
        )
    if urls:
        source = download_verified(urls, expected_sha, destination)
    elif parts:
        sources: list[str] = []
        with destination.open("wb") as output:
            for expected_number, part in enumerate(parts, start=1):
                number = part.get("number")
                if (
                    not isinstance(number, int)
                    or isinstance(number, bool)
                    or number != expected_number
                ):
                    raise SupportBinaryError(
                        "catalogued multipart asset has non-contiguous parts"
                    )
                size = part.get("size_bytes")
                if (
                    not isinstance(size, int)
                    or isinstance(size, bool)
                    or not 1 <= size <= MAX_PART_BYTES
                ):
                    raise SupportBinaryError(
                        f"catalogued part {expected_number} has invalid size_bytes {size!r}"
                    )
                part_path = destination.with_name(
                    f"{destination.name}.part-{expected_number:04d}"
                )
                source = download_verified(
                    part.get("urls") or [],
                    str(part.get("sha256", "")).lower(),
                    part_path,
                )
                if part_path.stat().st_size != size:
                    raise SupportBinaryError(
                        f"catalogued part {expected_number} size mismatch: "
                        f"expected {size}, got {part_path.stat().st_size}"
                    )
                with part_path.open("rb") as part_input:
                    shutil.copyfileobj(part_input, output)
                part_path.unlink()
                sources.append(source)
        source = f"{sources[0]} ({len(sources)} multipart chunk(s))"
    else:
        raise SupportBinaryError(
            "catalogued asset has neither direct URLs nor multipart data"
        )

    if destination.stat().st_size != declared_size:
        raise SupportBinaryError(
            f"catalogued asset size mismatch: expected {declared_size}, "
            f"got {destination.stat().st_size}"
        )
    actual_sha = sha256_file(destination)
    if actual_sha != expected_sha:
        raise SupportBinaryError(
            f"reconstructed asset sha256 mismatch: expected {expected_sha}, got {actual_sha}"
        )
    return source


def load_tool_catalog(
    origin: str, index: dict, tool: str, issue_url: str
) -> ToolCatalog:
    """Load a per-tool catalogue after verifying its index-recorded digest."""
    index_url = f"{origin}/manifest.json"
    entry = (index.get("tools") or {}).get(tool)
    descriptor = (entry or {}).get("descriptor") or {}
    descriptor_url = descriptor.get("url")
    if not descriptor_url:
        raise SupportBinaryError(
            f"{display_url(index_url)} has no descriptor URL for {tool}; "
            f"support coverage is tracked in {issue_url}"
        )
    catalog_url = urljoin(f"{origin}/", descriptor_url)
    expected_sha = str(descriptor.get("sha256") or "").lower()
    if not is_sha256(expected_sha):
        raise SupportBinaryError(
            f"{display_url(index_url)} descriptor for {tool} lacks a valid sha256"
        )
    raw = read_url(catalog_url, timeout=60)
    if sha256_bytes(raw) != expected_sha:
        raise SupportBinaryError(
            f"{display_url(catalog_url)} sha256 mismatch against "
            f"{display_url(index_url)} descriptor"
        )
    try:
        return ToolCatalog(catalog_url, json.loads(raw.decode("utf-8")))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SupportBinaryError(
            f"invalid tool catalogue at {display_url(catalog_url)}: {error}"
        ) from error


def validate_archive_member(name: str) -> None:
    """Reject archive paths that could escape the temporary extraction root."""
    normalized = name.replace("\\", "/")
    posix_path = Path(normalized)
    windows_path = PureWindowsPath(name)
    if (
        not name
        or posix_path.is_absolute()
        or windows_path.is_absolute()
        or windows_path.drive
        or ".." in posix_path.parts
    ):
        raise SupportBinaryError(f"unsafe path in support archive: {name!r}")


def validate_zip_members(contents: zipfile.ZipFile) -> None:
    for member in contents.infolist():
        validate_archive_member(member.filename)
        if stat.S_ISLNK(member.external_attr >> 16):
            raise SupportBinaryError(
                f"symlink in support archive is not allowed: {member.filename!r}"
            )


def validate_tar_members(contents: tarfile.TarFile) -> None:
    for member in contents.getmembers():
        validate_archive_member(member.name)
        if member.issym() or member.islnk():
            raise SupportBinaryError(
                f"link in support archive is not allowed: {member.name!r}"
            )
        if not (member.isfile() or member.isdir()):
            raise SupportBinaryError(
                f"special entry in support archive is not allowed: {member.name!r}"
            )


def extract_archive(
    archive: Path, extract_dir: Path, target: str, driver: Path
) -> None:
    """Extract a verified support archive through the matching supported route."""
    name = archive.name
    if name.endswith(".tar.zst"):
        driver_candidates = [
            driver,
            Path(f"{driver}.exe"),
            Path(f"target/{target}/release/soldr{binary_suffix(target)}"),
            Path(f"target/release/soldr{binary_suffix(target)}"),
        ]
        actual_driver = next(
            (path for path in driver_candidates if path.is_file()), None
        )
        if actual_driver is None:
            raise SupportBinaryError(
                f"no soldr driver found; tried {driver_candidates}"
            )
        subprocess.run(
            [
                str(actual_driver),
                "archive",
                "--input",
                str(archive),
                "--extract-dir",
                str(extract_dir),
            ],
            check=True,
        )
    elif name.endswith(".zip"):
        with zipfile.ZipFile(archive) as contents:
            validate_zip_members(contents)
            contents.extractall(extract_dir)
    elif name.endswith(".tar.gz") or name.endswith(".tgz"):
        with tarfile.open(archive, "r:gz") as contents:
            validate_tar_members(contents)
            contents.extractall(extract_dir)
    else:
        raise SupportBinaryError(f"unsupported support archive format: {archive}")


def write_source_commit(github_env: Path, tool: str, release_version: str) -> None:
    variable = "CRGX_SOURCE_COMMIT" if tool == "crgx" else "CARGO_CHEF_SOURCE_COMMIT"
    with github_env.open("a", encoding="utf-8") as handle:
        handle.write(f"{variable}=soldr-toolchain:{release_version}\n")


def fetch_tool(
    *,
    origin: str,
    index: dict,
    target: str,
    tool: str,
    version: str,
    output_dir: Path,
    driver: Path,
    issue_url: str,
    github_env: Path,
) -> None:
    """Download, verify, extract, and stage one tool for the requested target."""
    release_version = version if version.startswith("v") else f"v{version}"
    loaded = load_tool_catalog(origin, index, tool, issue_url)
    catalog_url, catalog = loaded.url, loaded.document
    release = next(
        (
            entry
            for entry in catalog.get("releases", [])
            if entry.get("version") == release_version
        ),
        None,
    )
    if release is None:
        raise SupportBinaryError(
            f"{display_url(catalog_url)} has no release {release_version}; "
            f"support coverage is tracked in {issue_url}"
        )
    platform = platform_for_target(target)
    platform_entry = next(
        (
            entry
            for entry in release.get("platforms", [])
            if entry.get("platform") == platform.to_dict()
        ),
        None,
    )
    if platform_entry is None:
        raise SupportBinaryError(
            f"{display_url(catalog_url)} release {release_version} has no asset "
            f"for {target} ({platform.to_dict()}); "
            f"support coverage is tracked in {issue_url}"
        )
    asset = platform_entry["asset"]
    with tempfile.TemporaryDirectory(prefix=f"{tool}-support-") as temp_dir:
        temporary = Path(temp_dir)
        archive = temporary / asset["filename"]
        source_url = download_catalogued_asset(asset, archive)
        print(
            f"fetched {tool} {release_version} for {target}: {display_url(source_url)}"
        )
        extract_dir = temporary / "extract"
        extract_dir.mkdir()
        extract_archive(archive, extract_dir, target, driver)
        binary_name = f"{tool}{binary_suffix(target)}"
        matches = [path for path in extract_dir.rglob(binary_name) if path.is_file()]
        if not matches:
            raise SupportBinaryError(
                f"{tool} support archive did not contain {binary_name}"
            )
        matches.sort(
            key=lambda path: (
                0 if path.parent.name == "bin" else 1,
                len(path.parts),
                str(path),
            )
        )
        destination = output_dir / binary_name
        shutil.copy2(matches[0], destination)
        destination.chmod(destination.stat().st_mode | 0o755)
        write_source_commit(github_env, tool, release_version)
        print(f"staged {destination} from {matches[0]}")


def stage_support_binaries(args: argparse.Namespace) -> None:
    origin = args.origin.rstrip("/")
    output_dir: Path = args.package_dir
    output_dir.mkdir(parents=True, exist_ok=True)
    index_url = f"{origin}/manifest.json"
    try:
        index = json.loads(read_url(index_url, timeout=60).decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SupportBinaryError(
            f"invalid toolchain index at {display_url(index_url)}: {error}"
        ) from error

    versions = {
        "crgx": read_pinned_version(args.repo_root, CRGX_VERSION),
        "cargo-chef": read_pinned_version(args.repo_root, CARGO_CHEF_VERSION),
    }
    for tool, version in versions.items():
        fetch_tool(
            origin=origin,
            index=index,
            target=args.target,
            tool=tool,
            version=version,
            output_dir=output_dir,
            driver=args.driver,
            issue_url=args.issue_url,
            github_env=args.github_env,
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--driver", type=Path, required=True)
    parser.add_argument("--package-dir", type=Path, default=Path("dist/package"))
    parser.add_argument(
        "--origin", default=os.environ.get("SOLDR_TOOLCHAIN_ORIGIN", DEFAULT_ORIGIN)
    )
    parser.add_argument("--issue-url", default=DEFAULT_ISSUE_URL)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--github-env", type=Path, default=os.environ.get("GITHUB_ENV"))
    args = parser.parse_args(argv)
    if args.github_env is None:
        parser.error("--github-env is required outside GitHub Actions")
    try:
        stage_support_binaries(args)
    except (OSError, subprocess.CalledProcessError, SupportBinaryError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
