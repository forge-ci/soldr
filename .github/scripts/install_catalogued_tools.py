#!/usr/bin/env python3
"""Install exact target-native tools from the hash-pinned toolchain catalogue."""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import shutil
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import zipfile
from dataclasses import asdict, dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Iterator, TypeGuard, TypeVar
from urllib.parse import urlsplit

from catalogue_http import display_url, open_url, validate_https_url

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

DEFAULT_CATALOGUE_URL = "https://zackees.github.io/soldr-toolchain/catalogue.v2.json"
SUPPORTED_TARGETS = {
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
}
DOWNLOAD_TIMEOUT_SECS = 120
SMOKE_TIMEOUT_SECS = 30
DOWNLOAD_ATTEMPTS = 3
RETRY_BASE_DELAY_SECS = 0.5
MAX_PART_BYTES = 95 * 1024 * 1024
MAX_PARTS = 4096
MAX_ASSET_BYTES = 8 * 1024 * 1024 * 1024 * 1024
CATALOGUE_CAPABILITY = 2

T = TypeVar("T")


@dataclass(frozen=True)
class JsonObject:
    """A strictly parsed JSON object whose schema is validated field by field."""

    fields: dict

    def get(self, key: str, default: Any = None) -> Any:
        return self.fields.get(key, default)

    def __getitem__(self, key: str) -> Any:
        return self.fields[key]

    def __iter__(self) -> Iterator[str]:
        return iter(self.fields)

    def __contains__(self, key: object) -> bool:
        return key in self.fields


@dataclass(frozen=True)
class PartIdentity:
    """Size and content identity of one published or catalogued part."""

    size_bytes: int
    ident: str


@dataclass(frozen=True)
class AssetIdentity:
    """Logical catalogue identity of one release asset."""

    owner: str
    repo: str
    tag: str
    asset: str


@dataclass(frozen=True)
class InstalledTool:
    """One verified and smoke-tested installed tool."""

    tool: str
    asset: str
    sha256: str
    version_output: str


def retry_network(action: Callable[[], T], *, label: str) -> T:
    """Retry bounded transient I/O failures without weakening verification."""
    for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
        try:
            return action()
        except (
            urllib.error.URLError,
            TimeoutError,
            ConnectionError,
            http.client.IncompleteRead,
        ) as error:
            if attempt == DOWNLOAD_ATTEMPTS:
                raise SystemExit(
                    f"{label} failed after {DOWNLOAD_ATTEMPTS} attempts: {type(error).__name__}"
                ) from None
            time.sleep(RETRY_BASE_DELAY_SECS * (2 ** (attempt - 1)))
    raise AssertionError("retry loop exhausted without returning or raising")


def asset_name(tool: str, version: str, target: str) -> str:
    if target not in SUPPORTED_TARGETS:
        raise SystemExit(f"unsupported catalogue target: {target}")
    bare_version = version.removeprefix("v")
    return f"{tool}-{bare_version}-{target}.tar.gz"


def valid_sha256(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(char in "0123456789abcdef" for char in value)
    )


def positive_int(value: object) -> TypeGuard[int]:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def valid_git_object(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 40
        and all(char in "0123456789abcdef" for char in value)
    )


def valid_generation(value: object) -> bool:
    return (
        isinstance(value, str)
        and bool(value)
        and len(value) <= 256
        and all(char.isascii() and (char.isalnum() or char in "._:-") for char in value)
    )


def strict_json_document(raw: bytes, *, label: str) -> JsonObject:
    def reject_duplicate_keys(pairs: Any) -> Any:
        document: dict = {}
        for key, value in pairs:
            if key in document:
                raise ValueError(f"duplicate JSON key {key!r}")
            document[key] = value
        return document

    def reject_nonfinite_number(value: str) -> None:
        raise ValueError(f"non-finite JSON number {value}")

    try:
        payload = json.loads(
            raw.decode("utf-8"),
            object_pairs_hook=reject_duplicate_keys,
            parse_constant=reject_nonfinite_number,
        )
    except (UnicodeDecodeError, ValueError) as error:
        raise SystemExit(f"{label} is not strict UTF-8 JSON: {error}") from error
    if not isinstance(payload, dict):
        raise SystemExit(f"{label} is not a JSON object")
    return JsonObject(payload)


def canonical_json_sha256(value: object) -> str:
    encoded = json.dumps(
        value, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def require_exact_keys(
    value: object, *, required: set[str], optional: set[str] | None = None, label: str
) -> JsonObject:
    if isinstance(value, JsonObject):
        value = value.fields
    if not isinstance(value, dict):
        raise SystemExit(f"{label} is not an object")
    keys = set(value)
    allowed = required | (optional or set())
    if not required <= keys or not keys <= allowed:
        raise SystemExit(f"{label} has missing or unknown fields")
    return JsonObject(value)


def safe_source_path(value: object) -> bool:
    if not isinstance(value, str) or not value or value.startswith("/"):
        return False
    return not any(part in {"", ".", ".."} for part in value.split("/"))


def validate_transport(entry: JsonObject, *, asset: str) -> None:  # noqa: C901
    """Validate the catalogue-v2 direct/multipart transport union."""
    size_bytes = entry.get("size_bytes")
    if not positive_int(size_bytes) or size_bytes > MAX_ASSET_BYTES:
        raise SystemExit(f"catalogue row {asset} has invalid size_bytes")
    if not valid_sha256(entry.get("sha256")):
        raise SystemExit(f"catalogue row {asset} has no valid sha256")
    if any(
        not isinstance(entry.get(field), str) or not entry[field]
        for field in ("owner", "repo", "tag", "asset")
    ):
        raise SystemExit(f"catalogue row {asset} has invalid identity fields")

    urls = entry.get("urls") or []
    parts = entry.get("parts") or []
    if bool(urls) == bool(parts):
        raise SystemExit(
            f"catalogue row {asset} must contain exactly one transport shape"
        )
    if urls:
        if not isinstance(urls, list) or any(
            not isinstance(url, str) or not url for url in urls
        ):
            raise SystemExit(f"catalogue row {asset} has invalid download URLs")
        if entry.get("source_path") is not None:
            raise SystemExit(f"catalogue row {asset} direct transport has source_path")
        for url in urls:
            validate_https_url(url, label=f"catalogue row {asset}")
        return

    if not isinstance(parts, list):
        raise SystemExit(f"catalogue row {asset} has invalid multipart data")
    if len(parts) > MAX_PARTS:
        raise SystemExit(f"catalogue row {asset} has too many parts")
    if entry.get("min_client_version") != CATALOGUE_CAPABILITY:
        raise SystemExit(
            f"catalogue row {asset} requires min_client_version {CATALOGUE_CAPABILITY}"
        )
    if not safe_source_path(entry.get("source_path")):
        raise SystemExit(f"catalogue row {asset} has invalid source_path")
    total_size = 0
    for expected_number, part in enumerate(parts, start=1):
        if not isinstance(part, dict):
            raise SystemExit(f"catalogue row {asset} has invalid multipart data")
        require_exact_keys(
            part,
            required={"number", "size_bytes", "sha256", "urls"},
            label=f"catalogue row {asset} part {expected_number}",
        )
        part_number = part.get("number")
        if (
            not isinstance(part_number, int)
            or isinstance(part_number, bool)
            or part_number != expected_number
        ):
            raise SystemExit(f"catalogue row {asset} has non-contiguous parts")
        part_size = part.get("size_bytes")
        if not positive_int(part_size) or part_size > MAX_PART_BYTES:
            raise SystemExit(
                f"catalogue row {asset} part {expected_number} has invalid size_bytes"
            )
        if not valid_sha256(part.get("sha256")):
            raise SystemExit(
                f"catalogue row {asset} part {expected_number} has no valid sha256"
            )
        part_urls = part.get("urls") or []
        if (
            not isinstance(part_urls, list)
            or not part_urls
            or any(not isinstance(url, str) or not url for url in part_urls)
        ):
            raise SystemExit(
                f"catalogue row {asset} part {expected_number} has invalid URLs"
            )
        for url in part_urls:
            validate_https_url(
                url, label=f"catalogue row {asset} part {expected_number}"
            )
        total_size += part_size
    if total_size != size_bytes:
        raise SystemExit(
            f"catalogue row {asset} multipart size mismatch: "
            f"expected {size_bytes}, got {total_size}"
        )


def select_entry(
    catalogue: JsonObject, *, tool: str, version: str, target: str
) -> JsonObject:
    expected = asset_name(tool, version, target)
    entries = catalogue.get("entries")
    if not isinstance(entries, list):
        raise SystemExit("catalogue has no entries list")
    matches = [entry for entry in entries if entry.get("asset") == expected]
    if len(matches) != 1:
        raise SystemExit(
            f"catalogue must contain exactly one {expected} row; found {len(matches)}"
        )
    entry = matches[0]
    if not valid_sha256(entry.get("sha256")):
        raise SystemExit(f"catalogue row {expected} has no valid sha256")
    selected = JsonObject(entry)
    validate_transport(selected, asset=expected)
    return selected


def fetch_bytes(url: str, *, label: str) -> bytes:
    def read() -> bytes:
        request = urllib.request.Request(url, headers={"Accept-Encoding": "identity"})
        with open_url(request, timeout=DOWNLOAD_TIMEOUT_SECS) as response:
            return response.read()

    return retry_network(read, label=label)


def validate_publication_state_url(value: object, generation: str) -> str:
    url = validate_https_url(value, label="catalogue publication_state")
    parsed = urlsplit(url)
    expected = f"/generations/{generation}/publish-state.v1.json"
    if (
        parsed.query
        or parsed.fragment
        or not parsed.path.endswith(expected)
        or any(part in {".", ".."} for part in parsed.path.split("/"))
    ):
        raise SystemExit(
            "catalogue publication_state URL is not immutable and generation-qualified"
        )
    return url


def validate_publication_state(  # noqa: C901
    state: JsonObject, *, generation: str, catalogue_sha256: str
) -> None:
    require_exact_keys(
        state,
        required={
            "schema_version",
            "generation",
            "source",
            "active",
            "previous",
            "catalogue_sha256",
            "assets_by_sha256",
            "logical_assets",
            "partitioner_default",
            "published_at",
            "retained_generations",
            "parts_by_sha256",
        },
        label="publication state",
    )
    if (
        state.get("schema_version") != 1
        or state.get("generation") != generation
        or state.get("catalogue_sha256") != catalogue_sha256
    ):
        raise SystemExit("publication state does not bind this catalogue generation")

    source = require_exact_keys(
        state["source"],
        required={"branch", "commit", "tree"},
        label="publication state source",
    )
    if (
        source.get("branch") != "assets"
        or not valid_git_object(source.get("commit"))
        or not valid_git_object(source.get("tree"))
    ):
        raise SystemExit("publication state has invalid source identity")

    slots: list[JsonObject] = []
    for slot_name in ("active", "previous"):
        slot = require_exact_keys(
            state[slot_name],
            required={"slot", "commit", "tree"},
            label=f"publication state {slot_name}",
        )
        if (
            slot.get("slot") not in {"public-a", "public-b"}
            or not valid_git_object(slot.get("commit"))
            or not valid_git_object(slot.get("tree"))
        ):
            raise SystemExit(f"publication state has invalid {slot_name} slot")
        slots.append(slot)
    if slots[0]["slot"] == slots[1]["slot"]:
        raise SystemExit("publication state active and previous slots are identical")

    partitioner = require_exact_keys(
        state["partitioner_default"],
        required={"version", "target_bytes", "max_bytes"},
        label="publication state partitioner_default",
    )
    if (
        partitioner.get("version") != 1
        or not positive_int(partitioner.get("target_bytes"))
        or partitioner["target_bytes"] > MAX_PART_BYTES
        or partitioner.get("max_bytes") != MAX_PART_BYTES
    ):
        raise SystemExit("publication state has invalid default partitioner")

    if not positive_int(state.get("published_at")):
        raise SystemExit("publication state has invalid published_at")
    retained = state["retained_generations"]
    if not isinstance(retained, list) or not retained:
        raise SystemExit("publication state has no retained generations")
    retained_names: set[str] = set()
    for row in retained:
        retained_row = require_exact_keys(
            row,
            required={"generation", "published_at"},
            label="publication state retained generation",
        )
        name = retained_row.get("generation")
        if (
            not valid_generation(name)
            or not positive_int(retained_row.get("published_at"))
            or name in retained_names
        ):
            raise SystemExit("publication state has invalid retained generation")
        retained_names.add(str(name))
    if generation not in retained_names:
        raise SystemExit("publication state does not retain its current generation")

    assets = state["assets_by_sha256"]
    logical_assets = state["logical_assets"]
    part_index = state["parts_by_sha256"]
    if not all(
        isinstance(value, dict) for value in (assets, logical_assets, part_index)
    ):
        raise SystemExit("publication state identity tables are invalid")

    expected_part_index: dict[str, PartIdentity] = {}
    for asset_sha, raw_asset in assets.items():
        published = require_exact_keys(
            raw_asset,
            required={"size_bytes", "partitioner", "parts"},
            label=f"publication asset {asset_sha}",
        )
        published_partitioner = require_exact_keys(
            published["partitioner"],
            required={"version", "target_bytes"},
            label=f"publication asset {asset_sha} partitioner",
        )
        asset_size = published.get("size_bytes")
        target_bytes = published_partitioner.get("target_bytes")
        parts = published.get("parts")
        if (
            not valid_sha256(asset_sha)
            or not positive_int(asset_size)
            or asset_size > MAX_ASSET_BYTES
            or published_partitioner.get("version") != 1
            or not positive_int(target_bytes)
            or target_bytes > MAX_PART_BYTES
            or not isinstance(parts, list)
            or not parts
            or len(parts) > MAX_PARTS
        ):
            raise SystemExit(f"publication asset {asset_sha} is invalid")
        total_size = 0
        for expected_number, raw_part in enumerate(parts, start=1):
            part = require_exact_keys(
                raw_part,
                required={"number", "sha256", "size_bytes", "path", "git_blob"},
                label=f"publication asset {asset_sha} part {expected_number}",
            )
            part_size = part.get("size_bytes")
            part_sha = part.get("sha256")
            expected_path = f"sha256/{asset_sha}/{expected_number:04d}-{part_sha}.part"
            if (
                part.get("number") != expected_number
                or not valid_sha256(part_sha)
                or not positive_int(part_size)
                or part_size > MAX_PART_BYTES
                or (expected_number < len(parts) and part_size != target_bytes)
                or (expected_number == len(parts) and part_size > target_bytes)
                or part.get("path") != expected_path
                or not valid_git_object(part.get("git_blob"))
            ):
                raise SystemExit(
                    f"publication asset {asset_sha} part {expected_number} is invalid"
                )
            identity = PartIdentity(part_size, str(part["git_blob"]))
            previous = expected_part_index.setdefault(str(part_sha), identity)
            if previous != identity:
                raise SystemExit("publication state has conflicting part identities")
            total_size += part_size
        if total_size != asset_size:
            raise SystemExit(f"publication asset {asset_sha} has invalid total size")

    if set(part_index) != set(expected_part_index):
        raise SystemExit("publication state part index is incomplete")
    for part_sha, identity in expected_part_index.items():
        row = require_exact_keys(
            part_index[part_sha],
            required={"size_bytes", "git_blob"},
            label=f"publication part index {part_sha}",
        )
        if (row.get("size_bytes"), row.get("git_blob")) != (
            identity.size_bytes,
            identity.ident,
        ):
            raise SystemExit(f"publication part index {part_sha} is invalid")

    logical_oids: set[str] = set()
    for logical_key, raw_logical in logical_assets.items():
        logical = require_exact_keys(
            raw_logical,
            required={
                "source_path",
                "asset",
                "source_oid_sha256",
                "source_size_bytes",
                "metadata_fingerprint",
                "provenance",
            },
            label=f"publication logical asset {logical_key!r}",
        )
        oid = logical.get("source_oid_sha256")
        size = logical.get("source_size_bytes")
        provenance = logical.get("provenance")
        published = assets.get(oid)
        if (
            not isinstance(logical_key, str)
            or not logical_key
            or not safe_source_path(logical.get("source_path"))
            or not isinstance(logical.get("asset"), str)
            or not logical["asset"]
            or not valid_sha256(oid)
            or not valid_sha256(logical.get("metadata_fingerprint"))
            or not positive_int(size)
            or size > MAX_ASSET_BYTES
            or not isinstance(provenance, dict)
            or not provenance
            or any(not isinstance(key, str) or not key for key in provenance)
            or not isinstance(published, dict)
            or published.get("size_bytes") != size
        ):
            raise SystemExit(f"publication logical asset {logical_key!r} is invalid")
        logical_oids.add(str(oid))
    if logical_oids != set(assets):
        raise SystemExit("publication state logical and payload identities differ")


def bind_catalogue_entries(catalogue: JsonObject, state: JsonObject) -> None:  # noqa: C901
    entries = catalogue.get("entries")
    logical_assets = state["logical_assets"]
    if not isinstance(entries, list):
        raise SystemExit("catalogue has no entries list")
    source_paths: set[str] = set()
    logical_keys: set[str] = set()
    logical_rows: set[AssetIdentity] = set()
    direct_urls: set[str] = {str(catalogue["publication_state"]["url"])}
    part_urls: dict[str, PartIdentity] = {}

    for offset, raw_entry in enumerate(entries):
        entry = require_exact_keys(
            raw_entry,
            required={"owner", "repo", "tag", "asset", "size_bytes", "sha256"},
            optional={"urls", "parts", "min_client_version", "source_path"},
            label=f"catalogue entry {offset}",
        )
        asset = str(entry.get("asset") or f"entry {offset}")
        min_client_version = entry.get("min_client_version")
        if min_client_version is not None and (
            not isinstance(min_client_version, int)
            or isinstance(min_client_version, bool)
            or min_client_version != CATALOGUE_CAPABILITY
        ):
            raise SystemExit(f"catalogue row {asset} has invalid min_client_version")
        validate_transport(entry, asset=asset)

        identity = AssetIdentity(
            str(entry["owner"]),
            str(entry["repo"]),
            str(entry["tag"]),
            str(entry["asset"]),
        )
        if identity in logical_rows:
            raise SystemExit(f"catalogue row {asset} duplicates a logical identity")
        logical_rows.add(identity)

        urls = entry.get("urls") or []
        if urls:
            for url in urls:
                if url in direct_urls or url in part_urls:
                    raise SystemExit(
                        f"catalogue row {asset} duplicates a transport URL"
                    )
                direct_urls.add(url)
            continue

        for part in entry["parts"]:
            part_identity = PartIdentity(int(part["size_bytes"]), str(part["sha256"]))
            local_urls: set[str] = set()
            for url in part["urls"]:
                if url in local_urls or url in direct_urls:
                    raise SystemExit(
                        f"catalogue row {asset} duplicates a transport URL"
                    )
                local_urls.add(url)
                previous = part_urls.setdefault(url, part_identity)
                if previous != part_identity:
                    raise SystemExit(
                        f"catalogue row {asset} reuses a URL for different bytes"
                    )

        source_path = str(entry["source_path"])
        if source_path in source_paths:
            raise SystemExit(f"catalogue row {asset} duplicates source_path")
        source_paths.add(source_path)
        logical_key = "\0".join(
            (identity.owner, identity.repo, identity.tag, identity.asset)
        )
        logical = logical_assets.get(logical_key)
        if not isinstance(logical, dict):
            raise SystemExit(f"publication state does not bind catalogue row {asset}")
        logical_asset = logical.get("asset")
        provenance = logical.get("provenance")
        identity_matches = asset == logical_asset or (
            asset == source_path and PurePosixPath(source_path).name == logical_asset
        )
        if (
            logical.get("source_path") != source_path
            or not identity_matches
            or logical.get("source_oid_sha256") != entry["sha256"]
            or logical.get("source_size_bytes") != entry["size_bytes"]
            or not isinstance(provenance, dict)
            or provenance
            != {
                "owner": entry["owner"],
                "repo": entry["repo"],
                "tag": entry["tag"],
                "asset": logical_asset,
            }
        ):
            raise SystemExit(f"publication state does not bind catalogue row {asset}")
        logical_keys.add(logical_key)

    if logical_keys != set(logical_assets):
        raise SystemExit("publication state has catalogue-orphan logical assets")


def fetch_catalogue(url: str) -> JsonObject:
    validate_https_url(url, label="toolchain catalogue")
    shown_url = display_url(url)
    raw = fetch_bytes(url, label=f"fetch toolchain catalogue {shown_url}")
    payload = strict_json_document(raw, label=f"toolchain catalogue {shown_url}")
    require_exact_keys(
        payload,
        required={"schema_version", "generation", "publication_state", "entries"},
        optional={"generated_at", "origin"},
        label="toolchain catalogue",
    )
    generation = payload.get("generation")
    if payload.get("schema_version") != 2 or not valid_generation(generation):
        raise SystemExit(
            "toolchain catalogue is not a canonical schema_version 2 object"
        )
    binding = require_exact_keys(
        payload["publication_state"],
        required={"generation", "url"},
        label="catalogue publication_state",
    )
    if binding.get("generation") != generation:
        raise SystemExit("catalogue publication_state generation does not match")
    state_url = validate_publication_state_url(binding.get("url"), str(generation))
    if payload.get("origin") is not None:
        validate_https_url(payload["origin"], label="catalogue origin")

    digest = canonical_json_sha256(payload.fields)
    shown_state_url = display_url(state_url)
    state_raw = fetch_bytes(
        state_url, label=f"fetch toolchain publication state {shown_state_url}"
    )
    state = strict_json_document(
        state_raw, label=f"toolchain publication state {shown_state_url}"
    )
    validate_publication_state(
        state, generation=str(generation), catalogue_sha256=digest
    )
    bind_catalogue_entries(payload, state)
    return payload


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download_one_of(urls: list[object], expected: str, output: Path) -> str:
    """Download one verified object, retrying availability failures by mirror."""
    last_error: SystemExit | None = None
    for raw_url in urls:
        url = str(raw_url)

        def transfer(source_url: str = url) -> None:
            output.unlink(missing_ok=True)
            request = urllib.request.Request(
                source_url, headers={"Accept-Encoding": "identity"}
            )
            with (
                open_url(request, timeout=DOWNLOAD_TIMEOUT_SECS) as response,
                output.open("wb") as handle,
            ):
                shutil.copyfileobj(response, handle)

        try:
            retry_network(transfer, label=f"download {display_url(url)}")
        except SystemExit as error:
            last_error = error
            continue
        actual = sha256(output)
        if actual != expected:
            raise SystemExit(
                f"catalogued asset sha256 mismatch: expected {expected}, got {actual}"
            )
        return url
    if last_error is not None:
        raise last_error
    raise SystemExit("catalogued asset has no download URL")


def download_verified(entry: JsonObject, output: Path) -> None:
    asset = str(entry.get("asset") or output.name)
    validate_transport(entry, asset=asset)
    expected = str(entry["sha256"]).lower()
    if not valid_sha256(expected):
        raise SystemExit(f"catalogue row {asset} has no valid sha256")
    expected_size = int(entry["size_bytes"])
    temporary = output.with_suffix(output.suffix + ".part")
    part_paths: list[Path] = []

    try:
        urls = entry.get("urls") or []
        if urls:
            download_one_of(urls, expected, temporary)
        else:
            with temporary.open("wb") as assembled:
                for part in entry["parts"]:
                    number = int(part["number"])
                    part_path = output.with_name(f"{output.name}.part-{number:04d}")
                    part_paths.append(part_path)
                    download_one_of(
                        part["urls"], str(part["sha256"]).lower(), part_path
                    )
                    actual_size = part_path.stat().st_size
                    if actual_size != part["size_bytes"]:
                        raise SystemExit(
                            f"catalogued part {number} size mismatch: expected "
                            f"{part['size_bytes']}, got {actual_size}"
                        )
                    with part_path.open("rb") as part_input:
                        shutil.copyfileobj(part_input, assembled)

        actual_size = temporary.stat().st_size
        if actual_size != expected_size:
            raise SystemExit(
                f"catalogued asset size mismatch: expected {expected_size}, got {actual_size}"
            )
        actual = sha256(temporary)
        if actual != expected:
            raise SystemExit(
                f"catalogued asset sha256 mismatch: expected {expected}, got {actual}"
            )
        temporary.replace(output)
    finally:
        temporary.unlink(missing_ok=True)
        for part_path in part_paths:
            part_path.unlink(missing_ok=True)


def safe_member(name: str) -> bool:
    normalized = name.replace("\\", "/")
    if normalized.startswith("/") or (len(normalized) >= 2 and normalized[1] == ":"):
        return False
    return ".." not in PurePosixPath(normalized).parts


def executable_name(tool: str, target: str) -> str:
    return f"{tool}.exe" if "-windows-" in target else tool


def extract_binary(archive: Path, *, tool: str, target: str, output_dir: Path) -> Path:
    expected = executable_name(tool, target)
    payload: bytes
    mode = 0o755

    if archive.name.endswith((".tar.gz", ".tgz")):
        with tarfile.open(archive, "r:gz") as handle:
            tar_members = handle.getmembers()
            if any(
                not safe_member(member.name) or member.issym() or member.islnk()
                for member in tar_members
            ):
                raise SystemExit(f"{archive.name} contains an unsafe path")
            tar_candidates = [
                member
                for member in tar_members
                if member.isfile() and PurePosixPath(member.name).name == expected
            ]
            if len(tar_candidates) != 1:
                raise SystemExit(
                    f"{archive.name} must contain exactly one {expected}; "
                    f"found {len(tar_candidates)}"
                )
            source = handle.extractfile(tar_candidates[0])
            if source is None:
                raise SystemExit(f"failed to read {expected} from {archive.name}")
            payload = source.read()
            mode = tar_candidates[0].mode
    elif archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as handle:
            zip_members = handle.infolist()
            if any(not safe_member(member.filename) for member in zip_members):
                raise SystemExit(f"{archive.name} contains an unsafe path")
            zip_candidates = [
                member
                for member in zip_members
                if not member.is_dir()
                and PurePosixPath(member.filename.replace("\\", "/")).name == expected
            ]
            if len(zip_candidates) != 1:
                raise SystemExit(
                    f"{archive.name} must contain exactly one {expected}; "
                    f"found {len(zip_candidates)}"
                )
            payload = handle.read(zip_candidates[0])
    else:
        raise SystemExit(f"unsupported catalogue archive format: {archive.name}")

    output_dir.mkdir(parents=True, exist_ok=True)
    output = output_dir / expected
    temporary = output.with_suffix(output.suffix + ".part")
    temporary.write_bytes(payload)
    if os.name != "nt":
        temporary.chmod(mode | 0o111)
    temporary.replace(output)
    return output


# `link.exe` prints this banner before doing anything else, including before
# rejecting an option it does not understand.
MSVC_LINKER_BANNER = "Microsoft (R) Incremental Linker"


def _delegated_to_msvc_linker(tool: str, output: str) -> bool:
    """True when `dylint-link` reached MSVC's linker, whatever it exited with.

    `dylint-link` is a linker *wrapper*: it forwards its arguments to the
    platform linker. On Unix that is `cc`, which accepts `--version`. On MSVC
    it is `link.exe`, which does not -- it warns `LNK4044: unrecognized option
    '/-version'` and then fails `LNK1561: entry point must be defined`,
    because it was asked to link a program with no inputs.

    So requiring exit 0 here can never pass on Windows: the install aborts and
    leaves the tool uninstalled, which is why `dylint-link` could not be
    installed on an MSVC host at all. The banner is the real signal -- it says
    the wrapper resolved and handed off to a genuine linker, which is all this
    smoke can establish for a tool that has no version surface of its own.
    (The version string is already not checked for `dylint-link` below.)
    """
    return tool == "dylint-link" and MSVC_LINKER_BANNER in output


def smoke_version(binary: Path, *, tool: str, version: str, target: str) -> str:
    arguments = [str(binary), "--version"]
    if tool == "cargo-dylint":
        arguments = [str(binary), "dylint", "--version"]
    environment = os.environ.copy()
    if tool == "dylint-link":
        environment["RUSTUP_TOOLCHAIN"] = f"nightly-{target}"
    result = run_captured(
        arguments,
        check=False,
        capture_output=True,
        env=environment,
        text=True,
        timeout=SMOKE_TIMEOUT_SECS,
    )
    output = "\n".join(part.strip() for part in (result.stdout, result.stderr) if part)
    if _delegated_to_msvc_linker(tool, output):
        return output
    wrong_version = tool != "dylint-link" and version.removeprefix("v") not in output
    if result.returncode != 0 or wrong_version:
        raise SystemExit(
            f"{tool} smoke failed: exit={result.returncode}, output={output!r}"
        )
    return output


def install_tools(
    *,
    catalogue: JsonObject,
    tools: list[str],
    version: str,
    target: str,
    output_dir: Path,
) -> list[InstalledTool]:
    installed: list[InstalledTool] = []
    with tempfile.TemporaryDirectory(prefix="soldr-catalogued-tools-") as temp:
        root = Path(temp)
        stage = root / "bin"
        for tool in tools:
            entry = select_entry(catalogue, tool=tool, version=version, target=target)
            archive = root / str(entry["asset"])
            download_verified(entry, archive)
            binary = extract_binary(archive, tool=tool, target=target, output_dir=stage)
            reported = smoke_version(binary, tool=tool, version=version, target=target)
            installed.append(
                InstalledTool(
                    tool=tool,
                    asset=str(entry["asset"]),
                    sha256=str(entry["sha256"]),
                    version_output=reported,
                )
            )

        output_dir.mkdir(parents=True, exist_ok=True)
        for tool in tools:
            name = executable_name(tool, target)
            destination = output_dir / name
            temporary = destination.with_suffix(destination.suffix + ".part")
            shutil.copy2(stage / name, temporary)
            temporary.replace(destination)
    return installed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tools", nargs="+")
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--catalogue-url", default=DEFAULT_CATALOGUE_URL)
    args = parser.parse_args()

    result = install_tools(
        catalogue=fetch_catalogue(args.catalogue_url),
        tools=args.tools,
        version=args.version,
        target=args.target,
        output_dir=args.output_dir,
    )
    print(
        json.dumps(
            {"target": args.target, "tools": [asdict(item) for item in result]},
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
