#!/usr/bin/env python3
"""Download one soldr-toolchain asset and verify its catalogue digest.

The catalogue query intentionally exposes metadata (including SHA-256) rather
than a URL-only fast path.  Every CI consumer that extracts or executes a
catalogued archive should use this helper so a changed or substituted asset is
rejected before it reaches PATH or an archive.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import shutil
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

from catalogue_http import display_url, open_url, validate_https_url
from toolchain_asset_query import resolve_metadata

DOWNLOAD_ATTEMPTS = 5
RETRY_BASE_DELAY_SECS = 1.0
STALL_TIMEOUT_SECS = 120
MAX_TRANSFER_SECS = 7200
COPY_CHUNK_BYTES = 1024 * 1024


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def copy_response(response: Any, output: Path, *, append: bool) -> None:
    """Copy with a socket-level stall timeout and an overall safety bound."""
    started = time.monotonic()
    with output.open("ab" if append else "wb") as handle:
        while True:
            chunk = response.read(COPY_CHUNK_BYTES)
            if not chunk:
                return
            handle.write(chunk)
            if time.monotonic() - started > MAX_TRANSFER_SECS:
                raise TimeoutError(
                    f"catalogue transfer exceeded {MAX_TRANSFER_SECS} seconds"
                )


def response_supports_resume(response: Any, offset: int) -> bool:
    """Accept a resume only when the server explicitly starts at our offset."""
    status = getattr(response, "status", None)
    headers = getattr(response, "headers", None)
    content_range = headers.get("Content-Range", "") if headers is not None else ""
    return status == 206 and content_range.startswith(f"bytes {offset}-")


def download_one_of(
    urls: list[object], expected: str, expected_size: int, output: Path
) -> str:
    last_error: Exception | None = None
    for raw_url in urls:
        url = validate_https_url(raw_url, label="catalogued asset URL")
        output.unlink(missing_ok=True)
        for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
            offset = output.stat().st_size if output.exists() else 0
            headers = {"Accept-Encoding": "identity"}
            if offset:
                headers["Range"] = f"bytes={offset}-"
            request = urllib.request.Request(
                url,
                headers=headers,
            )
            try:
                with open_url(request, timeout=STALL_TIMEOUT_SECS) as response:
                    append = bool(offset and response_supports_resume(response, offset))
                    copy_response(response, output, append=append)
                actual_size = output.stat().st_size
                if actual_size < expected_size:
                    raise http.client.IncompleteRead(b"", expected_size - actual_size)
                if actual_size > expected_size:
                    raise SystemExit(
                        "catalogued asset size mismatch: "
                        f"expected {expected_size}, got {actual_size}"
                    )
                actual = sha256(output)
                if actual != expected:
                    raise SystemExit(
                        "catalogued asset sha256 mismatch: "
                        f"expected {expected}, got {actual}"
                    )
                return url
            except (
                urllib.error.URLError,
                TimeoutError,
                ConnectionError,
                http.client.IncompleteRead,
            ) as exc:
                last_error = exc
                print(
                    f"catalogue download failed attempt={attempt}/"
                    f"{DOWNLOAD_ATTEMPTS} url={display_url(url)} "
                    f"error={type(exc).__name__}",
                    file=sys.stderr,
                )
                if attempt < DOWNLOAD_ATTEMPTS:
                    time.sleep(RETRY_BASE_DELAY_SECS * (2 ** (attempt - 1)))
        output.unlink(missing_ok=True)
    error_kind = type(last_error).__name__ if last_error is not None else "unknown"
    raise SystemExit(f"all catalogue URLs failed: {error_kind}")


def download_verified(metadata: dict, output: Path) -> dict:  # noqa: C901
    expected = str(metadata.get("sha256", "")).lower()
    if len(expected) != 64 or any(char not in "0123456789abcdef" for char in expected):
        raise SystemExit("catalogued asset has no valid sha256")
    expected_size = metadata.get("size_bytes")
    if (
        not isinstance(expected_size, int)
        or isinstance(expected_size, bool)
        or expected_size <= 0
    ):
        raise SystemExit("catalogued asset has no valid size_bytes")
    urls = metadata.get("urls") or []
    parts = metadata.get("parts") or []
    if not isinstance(urls, list) or not isinstance(parts, list):
        raise SystemExit("catalogued asset has invalid transport data")
    if bool(urls) == bool(parts):
        raise SystemExit("catalogued asset must contain exactly one transport shape")

    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_suffix(output.suffix + ".part")
    part_paths: list[Path] = []
    try:
        if urls:
            download_one_of(urls, expected, expected_size, temporary)
        else:
            with temporary.open("wb") as assembled:
                for expected_number, part in enumerate(parts, start=1):
                    if not isinstance(part, dict):
                        raise SystemExit("catalogued asset has invalid multipart data")
                    number = part.get("number")
                    part_size = part.get("size_bytes")
                    part_sha = str(part.get("sha256", "")).lower()
                    part_urls = part.get("urls") or []
                    if (
                        number != expected_number
                        or not isinstance(part_size, int)
                        or isinstance(part_size, bool)
                        or part_size <= 0
                        or len(part_sha) != 64
                        or any(char not in "0123456789abcdef" for char in part_sha)
                        or not isinstance(part_urls, list)
                        or not part_urls
                    ):
                        raise SystemExit("catalogued asset has invalid multipart data")
                    part_path = output.with_name(
                        f"{output.name}.part-{expected_number:04d}"
                    )
                    part_paths.append(part_path)
                    download_one_of(part_urls, part_sha, part_size, part_path)
                    if part_path.stat().st_size != part_size:
                        raise SystemExit(
                            f"catalogued part {expected_number} size mismatch: "
                            f"expected {part_size}, got {part_path.stat().st_size}"
                        )
                    with part_path.open("rb") as part_input:
                        shutil.copyfileobj(part_input, assembled)

        actual_size = temporary.stat().st_size
        if actual_size != expected_size:
            raise SystemExit(
                f"catalogued asset size mismatch: expected {expected_size}, "
                f"got {actual_size}"
            )
        actual = sha256(temporary)
        if actual != expected:
            raise SystemExit(
                f"catalogued asset sha256 mismatch: expected {expected}, got {actual}"
            )
        temporary.replace(output)
        return {**metadata, "path": str(output), "verified_sha256": actual}
    finally:
        temporary.unlink(missing_ok=True)
        for part_path in part_paths:
            part_path.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tool")
    parser.add_argument(
        "--origin",
        default=None,
        help=(
            "soldr-toolchain origin. Default: SOLDR_TOOLCHAIN_ORIGIN, else the "
            "redirect manifest's origins then the built-ins, with failover."
        ),
    )
    parser.add_argument("--platform", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--extra", default=None)
    parser.add_argument("--version", default="latest")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    metadata = resolve_metadata(
        tool=args.tool,
        origin=args.origin,
        tool_manifest_url_override=None,
        platform=args.platform,
        arch=args.arch,
        extra=args.extra,
        version=args.version,
    )
    print(json.dumps(download_verified(metadata, args.output), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
