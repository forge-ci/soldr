#!/usr/bin/env python3
"""Tests for the hash-verified catalogue tool installer."""

from __future__ import annotations

import hashlib
import io
import json
import tarfile
import tempfile
import unittest
import urllib.error
from dataclasses import dataclass
from pathlib import Path
from unittest import mock

from _script_loader import load_sibling_script
from catalogue_http import SafeRedirectHandler

script = load_sibling_script("install_catalogued_tools")

TARGETS = [
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
]
GENERATION = "test-generation"
STATE_URL = f"https://example.test/generations/{GENERATION}/publish-state.v1.json"


def encoded(value: object) -> bytes:
    return json.dumps(value, separators=(",", ":"), sort_keys=True).encode()


@dataclass(frozen=True)
class PublicationDocuments:
    """Encoded catalogue and publication-state documents."""

    catalogue: bytes
    state: bytes


def publication_documents(entries: list[object] | None = None) -> PublicationDocuments:
    catalogue = {
        "schema_version": 2,
        "generation": GENERATION,
        "publication_state": {"generation": GENERATION, "url": STATE_URL},
        "entries": entries or [],
    }
    state = {
        "schema_version": 1,
        "generation": GENERATION,
        "source": {"branch": "assets", "commit": "1" * 40, "tree": "2" * 40},
        "active": {"slot": "public-a", "commit": "3" * 40, "tree": "4" * 40},
        "previous": {"slot": "public-b", "commit": "5" * 40, "tree": "6" * 40},
        "catalogue_sha256": script.canonical_json_sha256(catalogue),
        "assets_by_sha256": {},
        "logical_assets": {},
        "partitioner_default": {
            "version": 1,
            "target_bytes": 32 * 1024 * 1024,
            "max_bytes": script.MAX_PART_BYTES,
        },
        "published_at": 1,
        "retained_generations": [{"generation": GENERATION, "published_at": 1}],
        "parts_by_sha256": {},
    }
    return PublicationDocuments(encoded(catalogue), encoded(state))


class InstallCataloguedToolsTests(unittest.TestCase):
    def test_catalogue_fetch_retries_a_transient_failure(self) -> None:
        documents = publication_documents()
        catalogue, state = documents.catalogue, documents.state
        with (
            mock.patch.object(
                script,
                "open_url",
                side_effect=[
                    urllib.error.URLError("reset"),
                    io.BytesIO(catalogue),
                    io.BytesIO(state),
                ],
            ) as urlopen,
            mock.patch.object(script.time, "sleep") as sleep,
        ):
            catalogue = script.fetch_catalogue("https://example.test/catalogue.json")

        self.assertEqual(catalogue["schema_version"], 2)
        self.assertEqual(urlopen.call_count, 3)
        sleep.assert_called_once_with(script.RETRY_BASE_DELAY_SECS)

    def test_catalogue_fetch_rejects_duplicate_keys_and_unbound_state(self) -> None:
        with self.assertRaisesRegex(SystemExit, "duplicate JSON key"):
            script.strict_json_document(
                b'{"schema_version":2,"schema_version":2}', label="catalogue"
            )

        documents = publication_documents()
        catalogue, state = documents.catalogue, documents.state
        forged_state = json.loads(state)
        forged_state["catalogue_sha256"] = "0" * 64
        with (
            mock.patch.object(
                script,
                "open_url",
                side_effect=[io.BytesIO(catalogue), io.BytesIO(encoded(forged_state))],
            ),
            self.assertRaisesRegex(SystemExit, "does not bind"),
        ):
            script.fetch_catalogue("https://example.test/catalogue.v2.json")

    def test_asset_download_retries_then_verifies_sha256(self) -> None:
        payload = b"catalogued binary"
        entry = {
            "asset": "tool.tar.gz",
            "owner": "example",
            "repo": "tools",
            "tag": "v1",
            "size_bytes": len(payload),
            "urls": ["https://example.test/tool.tar.gz"],
            "sha256": hashlib.sha256(payload).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "tool.tar.gz"
            with (
                mock.patch.object(
                    script,
                    "open_url",
                    side_effect=[
                        urllib.error.URLError("reset"),
                        io.BytesIO(payload),
                    ],
                ) as urlopen,
                mock.patch.object(script.time, "sleep"),
            ):
                script.download_verified(script.JsonObject(entry), output)

            self.assertEqual(urlopen.call_count, 2)
            self.assertEqual(output.read_bytes(), payload)

    def test_multipart_download_reconstructs_and_verifies_asset(self) -> None:
        payloads = [b"catalogued ", b"multipart binary"]
        payload = b"".join(payloads)
        entry = {
            "asset": "tool.tar.gz",
            "owner": "example",
            "repo": "tools",
            "tag": "v1",
            "size_bytes": len(payload),
            "sha256": hashlib.sha256(payload).hexdigest(),
            "min_client_version": script.CATALOGUE_CAPABILITY,
            "source_path": "tool/v1/windows-x64/tool.tar.gz",
            "parts": [
                {
                    "number": number,
                    "size_bytes": len(part),
                    "sha256": hashlib.sha256(part).hexdigest(),
                    "urls": [f"https://example.test/part-{number}"],
                }
                for number, part in enumerate(payloads, start=1)
            ],
        }
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "tool.tar.gz"
            with mock.patch.object(
                script,
                "open_url",
                side_effect=[io.BytesIO(part) for part in payloads],
            ) as urlopen:
                script.download_verified(script.JsonObject(entry), output)

            self.assertEqual(urlopen.call_count, 2)
            self.assertEqual(output.read_bytes(), payload)

    def test_all_supported_targets_have_exact_asset_names(self) -> None:
        names = {
            script.asset_name("cargo-dylint", "6.0.3", target) for target in TARGETS
        }
        self.assertEqual(len(names), 8)
        self.assertEqual(
            script.asset_name("cargo-dylint", "6.0.3", "aarch64-pc-windows-msvc"),
            "cargo-dylint-6.0.3-aarch64-pc-windows-msvc.tar.gz",
        )

    def test_select_entry_requires_one_exact_hash_pinned_row(self) -> None:
        expected = script.asset_name("dylint-link", "6.0.3", "x86_64-unknown-linux-gnu")
        row = {
            "asset": expected,
            "owner": "zackees",
            "repo": "soldr-toolchain",
            "tag": "assets",
            "size_bytes": 1,
            "urls": ["https://example.test/dylint-link.tar.gz"],
            "sha256": "a" * 64,
        }
        self.assertEqual(
            script.select_entry(
                script.JsonObject({"schema_version": 2, "entries": [row]}),
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            ).fields,
            row,
        )
        with self.assertRaisesRegex(SystemExit, "exactly one"):
            script.select_entry(
                script.JsonObject({"schema_version": 2, "entries": []}),
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )
        with self.assertRaisesRegex(SystemExit, "valid sha256"):
            script.select_entry(
                script.JsonObject(
                    {"schema_version": 2, "entries": [{**row, "sha256": "bad"}]}
                ),
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )

    def test_select_entry_rejects_ambiguous_or_invalid_multipart_rows(self) -> None:
        expected = script.asset_name("dylint-link", "6.0.3", "x86_64-unknown-linux-gnu")
        row = {
            "asset": expected,
            "owner": "zackees",
            "repo": "soldr-toolchain",
            "tag": "assets",
            "size_bytes": 1,
            "sha256": "a" * 64,
            "min_client_version": script.CATALOGUE_CAPABILITY,
            "source_path": f"dylint-link/6.0.3/linux-x64/{expected}",
            "parts": [
                {
                    "number": 1,
                    "size_bytes": 1,
                    "sha256": "b" * 64,
                    "urls": ["https://example.test/part-1"],
                }
            ],
        }
        with self.assertRaisesRegex(SystemExit, "exactly one transport"):
            script.select_entry(
                script.JsonObject(
                    {
                        "schema_version": 2,
                        "entries": [{**row, "urls": ["https://example.test/full"]}],
                    }
                ),
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )
        with self.assertRaisesRegex(SystemExit, "non-contiguous"):
            script.select_entry(
                script.JsonObject(
                    {
                        "schema_version": 2,
                        "entries": [
                            {**row, "parts": [{**row["parts"][0], "number": 2}]}
                        ],
                    }
                ),
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )

    def test_select_entry_rejects_unsafe_transport_url(self) -> None:
        expected = script.asset_name("dylint-link", "6.0.3", "x86_64-unknown-linux-gnu")
        row = {
            "asset": expected,
            "owner": "zackees",
            "repo": "soldr-toolchain",
            "tag": "assets",
            "size_bytes": 1,
            "sha256": "a" * 64,
            "urls": ["https://user:secret@example.test/tool.tar.gz"],
        }
        with self.assertRaisesRegex(SystemExit, "credential-free absolute HTTPS"):
            script.select_entry(
                script.JsonObject({"schema_version": 2, "entries": [row]}),
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )

    def test_redirect_policy_rejects_https_downgrade_and_credentials(self) -> None:
        secret = "redirect-token-must-not-leak"
        request = script.urllib.request.Request(
            f"https://example.test/source?token={secret}"
        )
        for target in (
            "http://example.test/plaintext",
            "https://user:secret@example.test/credentialed",
        ):
            with self.assertRaisesRegex(
                SystemExit, "credential-free absolute HTTPS"
            ) as raised:
                SafeRedirectHandler().redirect_request(
                    request, None, 302, "Found", {}, target
                )
            self.assertNotIn(secret, str(raised.exception))

    def test_extract_binary_rejects_tar_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "asset.tar.gz"
            with tarfile.open(archive, "w:gz") as handle:
                link = tarfile.TarInfo("cargo-dylint")
                link.type = tarfile.SYMTYPE
                link.linkname = "outside"
                handle.addfile(link)
            with self.assertRaisesRegex(SystemExit, "unsafe path"):
                script.extract_binary(
                    archive,
                    tool="cargo-dylint",
                    target="x86_64-unknown-linux-gnu",
                    output_dir=root / "bin",
                )

    def test_extract_binary_materializes_only_the_expected_executable(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "asset.tar.gz"
            payload = b"#!/bin/sh\necho cargo-dylint 6.0.3\n"
            with tarfile.open(archive, "w:gz") as handle:
                member = tarfile.TarInfo("nested/cargo-dylint")
                member.mode = 0o755
                member.size = len(payload)
                handle.addfile(member, io.BytesIO(payload))
            installed = script.extract_binary(
                archive,
                tool="cargo-dylint",
                target="x86_64-unknown-linux-gnu",
                output_dir=root / "bin",
            )
            self.assertEqual(installed, root / "bin" / "cargo-dylint")
            self.assertEqual(installed.read_bytes(), payload)


class MsvcLinkerSmokeTest(unittest.TestCase):
    """`dylint-link` on MSVC has no version surface of its own.

    It forwards to `link.exe`, which rejects `--version` and exits non-zero,
    so an exit-code-only smoke can never pass on Windows -- the install
    aborts and the tool is left uninstalled.
    """

    MSVC_FAILURE = "\n".join(
        (
            "Microsoft (R) Incremental Linker Version 14.51.36252.0",
            "LINK : warning LNK4044: unrecognized option '/-version'; ignored",
            "LINK : fatal error LNK1561: entry point must be defined",
        )
    )

    def test_reaching_the_msvc_linker_counts_as_smoked(self) -> None:
        self.assertTrue(
            script._delegated_to_msvc_linker("dylint-link", self.MSVC_FAILURE)
        )

    def test_only_dylint_link_gets_the_exemption(self) -> None:
        """A real tool failing must never be excused by linker output."""
        self.assertFalse(
            script._delegated_to_msvc_linker("cargo-dylint", self.MSVC_FAILURE)
        )

    def test_unrecognisable_output_is_still_a_failure(self) -> None:
        """The banner is the evidence; without it nothing was established."""
        for output in ("", "command not found", "Segmentation fault"):
            self.assertFalse(script._delegated_to_msvc_linker("dylint-link", output))

    def test_unix_dylint_link_is_unaffected(self) -> None:
        """On Unix it forwards to `cc`, which answers `--version` properly."""
        self.assertFalse(
            script._delegated_to_msvc_linker("dylint-link", "cc (GCC) 13.2.0")
        )


if __name__ == "__main__":
    unittest.main()
