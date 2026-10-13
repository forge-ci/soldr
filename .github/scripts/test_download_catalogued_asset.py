#!/usr/bin/env python3
"""Tests for verified catalogue downloads."""

from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
import urllib.error
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import ClassVar, Self
from unittest import mock

from _script_loader import load_sibling_script

script = load_sibling_script("download_catalogued_asset")


@dataclass(frozen=True)
class PartFixture:
    """One multipart chunk of a catalogue asset."""

    number: int
    size_bytes: int
    sha256: str
    urls: list[str]


@dataclass(frozen=True)
class AssetFixture:
    """Catalogue asset metadata; `None` fields are left out of the wire form."""

    size_bytes: int
    filename: str = "asset.tar.gz"
    sha256: str | None = None
    urls: list[str] | None = None
    parts: list[PartFixture] | None = None

    def to_wire(self) -> dict:
        """The metadata dict `download_verified` consumes."""
        return {key: value for key, value in asdict(self).items() if value is not None}


class DownloadCataloguedAssetTests(unittest.TestCase):
    def test_valid_download_is_written_and_reported(self) -> None:
        payload = b"catalogue payload"
        metadata = AssetFixture(
            size_bytes=len(payload),
            urls=["https://example.test/asset.tar.gz"],
            sha256=script.hashlib.sha256(payload).hexdigest(),
        )
        response = mock.MagicMock()
        response.__enter__.return_value = io.BytesIO(payload)
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(script, "open_url", return_value=response),
        ):
            output = Path(temp) / metadata.filename
            result = script.download_verified(metadata.to_wire(), output)
            self.assertEqual(output.read_bytes(), payload)
            self.assertEqual(result["verified_sha256"], metadata.sha256)

    def test_mismatch_fails_before_publish(self) -> None:
        metadata = AssetFixture(
            size_bytes=len(b"tampered"),
            urls=["https://example.test/asset.tar.gz"],
            sha256="0" * 64,
        )
        response = mock.MagicMock()
        response.__enter__.return_value = io.BytesIO(b"tampered")
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(script, "open_url", return_value=response),
        ):
            with self.assertRaisesRegex(SystemExit, "sha256 mismatch"):
                script.download_verified(
                    metadata.to_wire(), Path(temp) / metadata.filename
                )
            self.assertFalse((Path(temp) / metadata.filename).exists())

    def test_missing_digest_fails_closed(self) -> None:
        metadata = AssetFixture(size_bytes=1, urls=["https://example.test"])
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaisesRegex(SystemExit, "no valid sha256"):
                script.download_verified(
                    metadata.to_wire(), Path(temp) / metadata.filename
                )

    def test_multipart_download_reconstructs_and_verifies(self) -> None:
        payloads = [b"catalogue ", b"multipart payload"]
        payload = b"".join(payloads)
        metadata = AssetFixture(
            size_bytes=len(payload),
            urls=[],
            parts=[
                PartFixture(
                    number=number,
                    size_bytes=len(part),
                    sha256=script.hashlib.sha256(part).hexdigest(),
                    urls=[f"https://example.test/part-{number}"],
                )
                for number, part in enumerate(payloads, start=1)
            ],
            sha256=script.hashlib.sha256(payload).hexdigest(),
        )
        responses = []
        for part in payloads:
            response = mock.MagicMock()
            response.__enter__.return_value = io.BytesIO(part)
            responses.append(response)
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(script, "open_url", side_effect=responses),
        ):
            output = Path(temp) / metadata.filename
            result = script.download_verified(metadata.to_wire(), output)
            self.assertEqual(output.read_bytes(), payload)
            self.assertEqual(result["verified_sha256"], metadata.sha256)

    def test_single_url_retries_a_transient_failure(self) -> None:
        payload = b"retry payload"
        metadata = AssetFixture(
            size_bytes=len(payload),
            urls=["https://example.test/asset.tar.gz"],
            sha256=script.hashlib.sha256(payload).hexdigest(),
        )
        response = mock.MagicMock()
        response.__enter__.return_value = io.BytesIO(payload)
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(
                script,
                "open_url",
                side_effect=[urllib.error.URLError("reset"), response],
            ) as open_url,
            mock.patch.object(script.time, "sleep") as sleep,
        ):
            output = Path(temp) / metadata.filename
            script.download_verified(metadata.to_wire(), output)

        self.assertEqual(open_url.call_count, 2)
        sleep.assert_called_once_with(script.RETRY_BASE_DELAY_SECS)

    def test_failed_signed_url_never_leaks_its_query_token(self) -> None:
        secret = "download-token-must-not-leak"
        metadata = AssetFixture(
            size_bytes=1,
            urls=[f"https://example.test/asset.tar.gz?token={secret}"],
            sha256="0" * 64,
        )
        stderr = io.StringIO()
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(
                script,
                "open_url",
                side_effect=urllib.error.URLError(f"transport included {secret}"),
            ),
            mock.patch.object(script, "DOWNLOAD_ATTEMPTS", 1),
            contextlib.redirect_stderr(stderr),
            self.assertRaisesRegex(SystemExit, "all catalogue URLs failed") as raised,
        ):
            script.download_verified(metadata.to_wire(), Path(temp) / metadata.filename)

        self.assertNotIn(secret, stderr.getvalue())
        self.assertNotIn(secret, str(raised.exception))

    def test_retry_resumes_only_from_a_matching_partial_response(self) -> None:
        prefix = b"partial "
        suffix = b"payload"
        payload = prefix + suffix

        class PartialResponse:
            def __enter__(self) -> Self:
                return self

            def __exit__(self, *_args: object) -> None:
                return None

            def read(self, _size: int) -> bytes:
                if self.chunk:
                    result = self.chunk
                    self.chunk = b""
                    return result
                raise urllib.error.URLError("connection reset")

            chunk = prefix

        class ResumeResponse:
            status = 206
            headers: ClassVar[dict[str, str]] = {
                "Content-Range": f"bytes {len(prefix)}-{len(payload) - 1}/{len(payload)}"
            }

            def __enter__(self) -> Self:
                return self

            def __exit__(self, *_args: object) -> None:
                return None

            def read(self, _size: int) -> bytes:
                result = self.chunk
                self.chunk = b""
                return result

            chunk = suffix

        metadata = AssetFixture(
            size_bytes=len(payload),
            urls=["https://example.test/asset.tar.gz"],
            sha256=script.hashlib.sha256(payload).hexdigest(),
        )
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(
                script,
                "open_url",
                side_effect=[PartialResponse(), ResumeResponse()],
            ) as open_url,
            mock.patch.object(script.time, "sleep"),
        ):
            output = Path(temp) / metadata.filename
            script.download_verified(metadata.to_wire(), output)
            self.assertEqual(output.read_bytes(), payload)

        resumed_request = open_url.call_args_list[1].args[0]
        self.assertEqual(resumed_request.get_header("Range"), f"bytes={len(prefix)}-")

    def test_direct_transport_rejects_plaintext_and_credentials(self) -> None:
        for url in (
            "http://example.test/asset.tar.gz",
            "https://user:secret@example.test/asset.tar.gz",
        ):
            metadata = AssetFixture(size_bytes=1, urls=[url], sha256="0" * 64)
            with tempfile.TemporaryDirectory() as temp:
                with self.assertRaisesRegex(
                    SystemExit, "credential-free absolute HTTPS"
                ):
                    script.download_verified(
                        metadata.to_wire(), Path(temp) / metadata.filename
                    )

    def test_multipart_transport_rejects_unsafe_url(self) -> None:
        for url in (
            "http://example.test/plaintext",
            "https://user:secret@example.test/credentialed",
        ):
            metadata = AssetFixture(
                size_bytes=1,
                parts=[
                    PartFixture(number=1, size_bytes=1, sha256="0" * 64, urls=[url])
                ],
                sha256="0" * 64,
            )
            with tempfile.TemporaryDirectory() as temp:
                with self.assertRaisesRegex(
                    SystemExit, "credential-free absolute HTTPS"
                ):
                    script.download_verified(
                        metadata.to_wire(), Path(temp) / metadata.filename
                    )


if __name__ == "__main__":
    unittest.main()
