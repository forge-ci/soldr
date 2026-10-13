#!/usr/bin/env python3
"""Resolve tool asset transport metadata from soldr-toolchain manifests.

The script preserves the friendly call surface the old manifest-branch
query script exposed:

    python3 .github/scripts/toolchain_asset_query.py \
        --platform linux --arch x86 --extra gnu cargo-zigbuild

It reads ``https://zackees.github.io/soldr-toolchain/manifest.json``, follows
and verifies the selected tool descriptor, then selects a release/platform
entry. With ``--json`` it emits
transport-polymorphic metadata (``urls`` XOR ``parts``) used by deterministic
CI packaging.
Exit status is non-zero when the requested tool/version/platform is absent.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import time
import urllib.error
import urllib.request
from dataclasses import asdict, dataclass, replace
from pathlib import PurePosixPath, PureWindowsPath
from typing import Any
from urllib.parse import urljoin

from catalogue_http import display_url, open_url, validate_https_url

DEFAULT_ORIGIN = "https://zackees.github.io/soldr-toolchain"
FETCH_ATTEMPTS = 3
RETRY_BASE_DELAY_SECS = 0.5


class NetworkFetchError(SystemExit):
    """A catalogue fetch exhausted retries without receiving bytes."""


OS_ALIASES: dict[str, str] = {
    "linux": "linux",
    "mac": "darwin",
    "macos": "darwin",
    "darwin": "darwin",
    "windows": "windows",
    "win": "windows",
}

ARCH_ALIASES: dict[str, str] = {
    "x86": "x86_64",
    "x64": "x86_64",
    "amd64": "x86_64",
    "x86_64": "x86_64",
    "arm": "aarch64",
    "arm64": "aarch64",
    "aarch64": "aarch64",
    "universal2": "universal2",
}


@dataclass(frozen=True)
class Platform:
    """A catalogue platform selector: `os`/`arch` plus optional `libc`/`abi`.

    Used both for the candidates `platform_candidates` produces and for the
    platform recorded on each catalogue entry. A `None` field is absent.
    """

    os: str | None = None
    arch: str | None = None
    libc: str | None = None
    abi: str | None = None

    @classmethod
    def from_wire(cls, raw: Any) -> Platform:
        """Build from a decoded catalogue platform object."""
        return cls(
            os=raw.get("os"),
            arch=raw.get("arch"),
            libc=raw.get("libc"),
            abi=raw.get("abi"),
        )

    def matches(self, expected: Platform) -> bool:
        """True when this (actual) platform satisfies the `expected` candidate."""
        for key in ("os", "arch", "libc", "abi"):
            wanted = getattr(expected, key)
            actual = getattr(self, key)
            if wanted is not None:
                if actual != wanted:
                    return False
            elif actual is not None:
                return False
        return True

    def label(self) -> str:
        """`os-arch-libc-abi` with the absent parts left out."""
        return "-".join(
            str(part) for part in (self.os, self.arch, self.libc, self.abi) if part
        )

    def to_dict(self) -> dict:
        """Wire form: only the keys that are present."""
        return {key: value for key, value in asdict(self).items() if value is not None}


@dataclass(frozen=True)
class CataloguePart:
    """One multipart chunk as published, before validation."""

    number: Any
    size_bytes: Any
    sha256: Any
    urls: Any

    @classmethod
    def from_wire(cls, raw: Any) -> CataloguePart:
        """Build from a decoded part object (`raw` must be a dict)."""
        return cls(
            number=raw.get("number"),
            size_bytes=raw.get("size_bytes"),
            sha256=raw.get("sha256", ""),
            urls=raw.get("urls") or [],
        )


@dataclass(frozen=True)
class CatalogueAsset:
    """One asset as published, before validation.

    Fields hold whatever the JSON carried; `normalize_catalogue_asset` is the
    single place that checks them.
    """

    filename: Any
    size_bytes: Any
    sha256: Any
    urls: Any
    parts: Any

    @classmethod
    def from_wire(cls, raw: Any) -> CatalogueAsset:
        """Build from a decoded asset object (`raw` must be a dict).

        `parts` stays the raw decoded value: its shape (list of objects) is
        one of the things `normalize_catalogue_asset` validates.
        """
        return cls(
            filename=raw.get("filename"),
            size_bytes=raw.get("size_bytes"),
            sha256=raw.get("sha256", ""),
            urls=raw.get("urls") or [],
            parts=raw.get("parts") or [],
        )


@dataclass(frozen=True)
class AssetPart:
    """A validated multipart chunk."""

    number: int
    sha256: str
    size_bytes: int
    urls: list[str]


@dataclass(frozen=True)
class Asset:
    """A validated asset: exactly one of `urls` / `parts` is non-empty."""

    platform: Platform
    filename: str
    urls: list[str]
    parts: list[AssetPart]
    sha256: str
    size_bytes: Any

    def to_dict(self) -> dict:
        """Wire form used by the JSON consumers."""
        wire = asdict(self)
        wire["platform"] = self.platform.to_dict()
        return wire


@dataclass(frozen=True)
class PlatformEntry:
    """One `platforms[]` row; either half is `None` when not an object."""

    platform: Platform | None
    asset: CatalogueAsset | None


@dataclass(frozen=True)
class Release:
    """One release; `platforms` is `None` when the manifest had no list."""

    version: Any
    platforms: list[PlatformEntry] | None

    @classmethod
    def from_wire(cls, raw: Any) -> Release:
        """Build from a decoded release object (`raw` must be a dict)."""
        raw_platforms = raw.get("platforms")
        platforms: list[PlatformEntry] | None = None
        if isinstance(raw_platforms, list):
            platforms = []
            for entry in raw_platforms:
                if not isinstance(entry, dict):
                    continue
                platform = entry.get("platform")
                asset = entry.get("asset")
                platforms.append(
                    PlatformEntry(
                        platform=(
                            Platform.from_wire(platform)
                            if isinstance(platform, dict)
                            else None
                        ),
                        asset=(
                            CatalogueAsset.from_wire(asset)
                            if isinstance(asset, dict)
                            else None
                        ),
                    )
                )
        return cls(version=raw.get("version"), platforms=platforms)


@dataclass(frozen=True)
class ToolManifest:
    """A per-tool catalogue manifest: its releases and release channels."""

    releases: list[Release]
    latest_stable: Any
    stable: Any


@dataclass(frozen=True)
class LoadedManifest:
    """A tool manifest together with the URL it was loaded from."""

    url: str
    manifest: ToolManifest


@dataclass(frozen=True)
class ToolMetadata:
    """What `--json` prints: the selected asset plus where it came from."""

    tool: str
    version: Any
    asset: Asset
    manifest_url: str
    schema_version: int = 1

    def to_dict(self) -> dict:
        """Wire form (the JSON schema CI consumers read)."""
        asset = self.asset.to_dict()
        return {
            "schema_version": self.schema_version,
            "tool": self.tool,
            "version": self.version,
            "platform": asset["platform"],
            "filename": asset["filename"],
            "urls": asset["urls"],
            "parts": asset["parts"],
            "sha256": asset["sha256"],
            "size_bytes": asset["size_bytes"],
            "manifest_url": self.manifest_url,
        }


def parse_manifest(payload: Any, url: str) -> ToolManifest:
    """Decode a tool manifest; `url` is only used in the error message."""
    if not isinstance(payload, dict):
        raise SystemExit(f"tool manifest at {url} is not a JSON object")
    raw_releases = payload.get("releases")
    releases = (
        [Release.from_wire(r) for r in raw_releases if isinstance(r, dict)]
        if isinstance(raw_releases, list)
        else []
    )
    raw_channels = payload.get("channels")
    channels = raw_channels if isinstance(raw_channels, dict) else {}
    return ToolManifest(
        releases=releases,
        latest_stable=channels.get("latest-stable"),
        stable=channels.get("stable"),
    )


def fetch_bytes(url: str) -> bytes:
    validate_https_url(url, label="tool manifest")
    shown_url = display_url(url)
    request = urllib.request.Request(url, headers={"Accept-Encoding": "identity"})
    for attempt in range(1, FETCH_ATTEMPTS + 1):
        try:
            with open_url(request, timeout=30) as resp:
                return resp.read()
        except urllib.error.HTTPError as exc:
            if attempt == FETCH_ATTEMPTS:
                raise NetworkFetchError(
                    f"HTTP {exc.code} fetching {shown_url}"
                ) from None
            time.sleep(RETRY_BASE_DELAY_SECS * (2 ** (attempt - 1)))
        except (
            urllib.error.URLError,
            TimeoutError,
            ConnectionError,
            http.client.IncompleteRead,
        ) as exc:
            if attempt == FETCH_ATTEMPTS:
                raise NetworkFetchError(
                    f"network error fetching {shown_url}: {type(exc).__name__}"
                ) from None
            time.sleep(RETRY_BASE_DELAY_SECS * (2 ** (attempt - 1)))
    raise AssertionError("retry loop exhausted without returning or raising")


def decode_json(raw: bytes, url: str) -> Any:
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise SystemExit(f"invalid UTF-8 JSON at {url}: {exc}") from exc


def fetch_json(url: str) -> Any:
    return decode_json(fetch_bytes(url), url)


def load_tool_manifest(origin: str, tool: str) -> LoadedManifest:
    """Follow and verify a tool descriptor from the published root index."""
    root = f"{origin.rstrip('/')}/manifest.json"
    index = decode_json(fetch_bytes(root), root)
    if not isinstance(index, dict) or index.get("schema_version") != 1:
        raise SystemExit(f"catalogue index at {root} is not a schema_version 1 object")
    tools = index.get("tools")
    if not isinstance(tools, dict):
        raise SystemExit(f"catalogue index at {root} has no tools object")
    entry = tools.get(tool)
    if not isinstance(entry, dict):
        raise SystemExit(f"catalogue index has no entry for {tool}")
    descriptor = entry.get("descriptor")
    if not isinstance(descriptor, dict):
        raise SystemExit(f"catalogue index has no descriptor for {tool}")
    relative_url = descriptor.get("url")
    if not isinstance(relative_url, str) or not relative_url:
        raise SystemExit(f"catalogue index has no descriptor URL for {tool}")
    expected_sha = str(descriptor.get("sha256", "")).lower()
    if len(expected_sha) != 64 or any(
        character not in "0123456789abcdef" for character in expected_sha
    ):
        raise SystemExit(f"catalogue descriptor for {tool} has no valid sha256")
    expected_size = descriptor.get("size_bytes")
    if (
        not isinstance(expected_size, int)
        or isinstance(expected_size, bool)
        or expected_size <= 0
    ):
        raise SystemExit(f"catalogue descriptor for {tool} has invalid size_bytes")

    url = urljoin(f"{origin.rstrip('/')}/", relative_url)
    raw = fetch_bytes(url)
    if len(raw) != expected_size:
        raise SystemExit(
            f"catalogue descriptor size mismatch for {tool}: "
            f"expected {expected_size}, got {len(raw)}"
        )
    actual_sha = hashlib.sha256(raw).hexdigest()
    if actual_sha != expected_sha:
        raise SystemExit(
            f"catalogue descriptor sha256 mismatch for {tool}: "
            f"expected {expected_sha}, got {actual_sha}"
        )
    return LoadedManifest(url, parse_manifest(decode_json(raw, url), url))


def normalize_os(value: str) -> str:
    os_key = OS_ALIASES.get(value.lower())
    if os_key is None:
        raise SystemExit(
            f"unknown --platform '{value}'. Accepted: {', '.join(sorted(OS_ALIASES))}"
        )
    return os_key


def normalize_arch(value: str) -> str:
    arch_key = ARCH_ALIASES.get(value.lower())
    if arch_key is None:
        raise SystemExit(
            f"unknown --arch '{value}'. Accepted: {', '.join(sorted(ARCH_ALIASES))}"
        )
    return arch_key


def platform_candidates(
    os_key: str, arch_key: str, extra: str | None
) -> list[Platform]:
    base = Platform(os=os_key, arch=arch_key)
    if extra:
        normalized = extra.lower()
        if os_key == "linux":
            libc = "glibc" if normalized in {"gnu", "glibc"} else normalized
            if libc == "glibc":
                return [replace(base, libc="glibc"), replace(base, libc="musl")]
            return [replace(base, libc=libc)]
        return [replace(base, abi=normalized)]

    if os_key == "linux":
        return [replace(base, libc="glibc"), replace(base, libc="musl"), base]
    if os_key == "windows":
        return [
            replace(base, abi="msvc"),
            replace(base, abi="gnu"),
            replace(base, abi="gnullvm"),
            base,
        ]
    if os_key == "darwin" and arch_key != "universal2":
        return [base, Platform(os="darwin", arch="universal2")]
    return [base]


def version_forms(value: str) -> set[str]:
    if value in {"", "latest"}:
        return {value}
    bare = value.removeprefix("v")
    return {value, bare, f"v{bare}"}


def find_release(manifest: ToolManifest, requested: str) -> Release:
    releases = manifest.releases
    if not releases:
        raise SystemExit("tool manifest has no releases")

    selected = requested
    if requested in {"", "latest"}:
        selected = (
            manifest.latest_stable or manifest.stable or releases[0].version or ""
        )

    accepted = version_forms(str(selected))
    for release in releases:
        version = str(release.version if release.version is not None else "")
        if version in accepted:
            return release

    known = ", ".join(
        str(r.version if r.version is not None else "?") for r in releases[:8]
    )
    raise SystemExit(
        f"no release '{requested}' in tool manifest. Known versions: {known}"
    )


def platform_matches(actual: Platform, expected: Platform) -> bool:
    return actual.matches(expected)


def find_asset_url(release: Release, candidates: list[Platform]) -> str:
    asset = find_asset(release, candidates, require_sha256=False)
    if not asset.urls:
        raise SystemExit("matched asset is multipart; use --json and a materializer")
    return str(asset.urls[0])


def normalize_catalogue_asset(  # noqa: C901
    platform: Platform, asset: CatalogueAsset, *, require_sha256: bool
) -> Asset:
    urls = asset.urls
    parts = asset.parts
    if bool(urls) == bool(parts):
        raise SystemExit("matched asset must contain exactly one transport shape")
    filename = asset.filename
    if (
        not isinstance(filename, str)
        or not filename
        or filename in {".", ".."}
        or "/" in filename
        or "\\" in filename
        or ":" in filename
        or any(ord(char) < 32 or ord(char) == 127 for char in filename)
        or PurePosixPath(filename).name != filename
        or PureWindowsPath(filename).name != filename
        or PureWindowsPath(filename).drive
        or PureWindowsPath(filename).root
    ):
        raise SystemExit("matched asset has unsafe filename")
    digest = str(asset.sha256).lower()
    if require_sha256 and (
        len(digest) != 64 or any(ch not in "0123456789abcdef" for ch in digest)
    ):
        raise SystemExit("matched asset has no valid sha256")
    normalized_parts: list[AssetPart] = []
    if parts:
        if not isinstance(parts, list):
            raise SystemExit("matched asset has invalid multipart data")
        total_size = 0
        for expected_number, raw_part in enumerate(parts, start=1):
            if not isinstance(raw_part, dict):
                raise SystemExit("matched asset has invalid multipart data")
            part = CataloguePart.from_wire(raw_part)
            part_digest = str(part.sha256).lower()
            part_size = part.size_bytes
            part_urls = part.urls
            if (
                part.number != expected_number
                or not isinstance(part_size, int)
                or isinstance(part_size, bool)
                or part_size <= 0
                or len(part_digest) != 64
                or any(ch not in "0123456789abcdef" for ch in part_digest)
                or not isinstance(part_urls, list)
                or not part_urls
            ):
                raise SystemExit("matched asset has invalid multipart data")
            total_size += part_size
            normalized_part_urls = [
                validate_https_url(
                    url, label=f"matched asset part {expected_number} URL"
                )
                for url in part_urls
            ]
            normalized_parts.append(
                AssetPart(
                    number=expected_number,
                    sha256=part_digest,
                    size_bytes=part_size,
                    urls=normalized_part_urls,
                )
            )
        if total_size != asset.size_bytes:
            raise SystemExit("matched asset multipart size mismatch")
    if urls and not isinstance(urls, list):
        raise SystemExit("matched asset has invalid URLs")
    normalized_urls = [
        validate_https_url(url, label="matched asset URL") for url in urls
    ]
    return Asset(
        platform=platform,
        filename=filename,
        urls=normalized_urls,
        parts=normalized_parts,
        sha256=digest,
        size_bytes=asset.size_bytes,
    )


def normalize_asset(platform: dict, asset: dict, *, require_sha256: bool) -> dict:
    """Wire-dict entry point for callers that hold raw catalogue JSON.

    Typed callers use `normalize_catalogue_asset`; this decodes the two
    objects, delegates, and re-encodes the validated asset.
    """
    return normalize_catalogue_asset(
        Platform.from_wire(platform),
        CatalogueAsset.from_wire(asset),
        require_sha256=require_sha256,
    ).to_dict()


def find_asset(
    release: Release,
    candidates: list[Platform],
    *,
    require_sha256: bool = True,
) -> Asset:
    platforms = release.platforms
    if platforms is None:
        raise SystemExit("release has no platforms list")

    for candidate in candidates:
        for entry in platforms:
            if entry.platform is None or entry.asset is None:
                continue
            if entry.platform.matches(candidate):
                return normalize_catalogue_asset(
                    entry.platform, entry.asset, require_sha256=require_sha256
                )

    wanted = " or ".join(candidate.label() for candidate in candidates)
    available = [
        entry.platform.label() for entry in platforms if entry.platform is not None
    ]
    raise SystemExit(
        f"no platform match for {wanted}; available: {', '.join(sorted(available))}"
    )


def _load_manifest(
    *, tool: str, origin: str, tool_manifest_url_override: str | None
) -> LoadedManifest:
    if tool_manifest_url_override:
        url = tool_manifest_url_override
        return LoadedManifest(url, parse_manifest(fetch_json(url), display_url(url)))
    return load_tool_manifest(origin, tool)


def resolve_url(
    *,
    tool: str,
    origin: str,
    tool_manifest_url_override: str | None,
    platform: str,
    arch: str,
    extra: str | None,
    version: str,
) -> str:
    loaded = _load_manifest(
        tool=tool,
        origin=origin,
        tool_manifest_url_override=tool_manifest_url_override,
    )
    release = find_release(loaded.manifest, version)
    candidates = platform_candidates(
        normalize_os(platform), normalize_arch(arch), extra
    )
    return find_asset_url(release, candidates)


def resolve_tool_metadata(
    *,
    tool: str,
    origin: str,
    tool_manifest_url_override: str | None,
    platform: str,
    arch: str,
    extra: str | None,
    version: str,
) -> ToolMetadata:
    loaded = _load_manifest(
        tool=tool,
        origin=origin,
        tool_manifest_url_override=tool_manifest_url_override,
    )
    release = find_release(loaded.manifest, version)
    os_key = normalize_os(platform)
    arch_key = normalize_arch(arch)
    asset = find_asset(
        release, platform_candidates(os_key, arch_key, extra), require_sha256=True
    )
    return ToolMetadata(
        tool=tool,
        version=release.version,
        asset=asset,
        manifest_url=display_url(loaded.url),
    )


def resolve_metadata(
    *,
    tool: str,
    origin: str,
    tool_manifest_url_override: str | None,
    platform: str,
    arch: str,
    extra: str | None,
    version: str,
) -> dict:
    """Wire-dict form of `resolve_tool_metadata` for the JSON consumers."""
    return resolve_tool_metadata(
        tool=tool,
        origin=origin,
        tool_manifest_url_override=tool_manifest_url_override,
        platform=platform,
        arch=arch,
        extra=extra,
        version=version,
    ).to_dict()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tool", help="Tool name under the soldr-toolchain origin.")
    parser.add_argument(
        "--origin",
        default=DEFAULT_ORIGIN,
        help=f"Catalogue origin (default: {DEFAULT_ORIGIN})",
    )
    parser.add_argument(
        "--tool-manifest-url",
        default=None,
        help="Full per-tool manifest URL override, mainly for tests and mirrors.",
    )
    parser.add_argument(
        "--platform", required=True, help="OS: linux, mac/darwin, windows."
    )
    parser.add_argument(
        "--arch",
        required=True,
        help="Arch: x86/x64/x86_64, arm/arm64/aarch64, universal2.",
    )
    parser.add_argument(
        "--extra", default=None, help="ABI/libc extra: gnu, musl, msvc, gnullvm."
    )
    parser.add_argument(
        "--version", default="latest", help="Release version or latest (default)."
    )
    parser.add_argument(
        "--json", action="store_true", help="emit selected asset metadata as JSON"
    )
    args = parser.parse_args(argv)

    kwargs = {
        "tool": args.tool,
        "origin": args.origin,
        "tool_manifest_url_override": args.tool_manifest_url,
        "platform": args.platform,
        "arch": args.arch,
        "extra": args.extra,
        "version": args.version,
    }
    if args.json:
        print(json.dumps(resolve_metadata(**kwargs), sort_keys=True))
    else:
        print(resolve_url(**kwargs))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
