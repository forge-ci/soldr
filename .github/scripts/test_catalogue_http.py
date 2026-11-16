#!/usr/bin/env python3
"""Unit tests for the soldr-toolchain origin list in catalogue_http.py.

No test here touches the network: the redirect fetch is injected, and the
failover tests drive plain callables.
"""

from __future__ import annotations

import io
import json
import unittest
import urllib.error
from email.message import Message
from pathlib import Path
from unittest import mock

from _script_loader import load_script_module

ch = load_script_module(Path(__file__).with_name("catalogue_http.py"), "catalogue_http")

FORGE = "https://forge-ci.github.io/soldr-toolchain"
LEGACY = "https://zackees.github.io/soldr-toolchain"


def manifest(origins: object, schema: object = 1) -> bytes:
    return json.dumps(
        {"schema_version": schema, "toolchain": {"origins": origins}}
    ).encode()


class RedirectManifestTest(unittest.TestCase):
    def test_builtins_prefer_mirror_and_keep_legacy(self) -> None:
        self.assertEqual(ch.BUILTIN_TOOLCHAIN_ORIGINS, (FORGE, LEGACY))
        self.assertEqual(
            ch.DEFAULT_TOOLCHAIN_REDIRECT_URL,
            "https://forge-ci.github.io/soldr-redirect.v1.json",
        )

    def test_parses_published_manifest_and_strips_slashes(self) -> None:
        self.assertEqual(
            ch.parse_redirect_manifest(manifest([FORGE + "/", LEGACY])),
            [FORGE, LEGACY],
        )

    def test_rejects_invalid_manifests(self) -> None:
        for raw in (
            b"not json",
            b"[]",
            manifest([FORGE], schema=2),
            manifest([FORGE], schema=True),
            manifest([FORGE], schema="1"),
            manifest([]),
            manifest(FORGE),
            manifest([1]),
            manifest(["http://insecure.example"]),
            manifest([FORGE, "ftp://bad.example"]),
            manifest(["https://user:pw@a.example"]),
            manifest(["https://a.example/x?token=1"]),
            manifest([FORGE] * (ch.MAX_REDIRECT_ORIGINS + 1)),
            json.dumps({"schema_version": 1}).encode(),
        ):
            with self.subTest(raw=raw[:60]):
                self.assertIsNone(ch.parse_redirect_manifest(raw))

    def test_effective_origins_append_builtins_and_dedupe(self) -> None:
        self.assertEqual(ch.effective_origins([]), [FORGE, LEGACY])
        self.assertEqual(
            ch.effective_origins([LEGACY, "https://new.example/t", LEGACY + "/"]),
            [LEGACY, "https://new.example/t", FORGE],
        )


class ToolchainOriginsTest(unittest.TestCase):
    def test_explicit_origin_skips_redirect(self) -> None:
        fetch = mock.Mock()
        self.assertEqual(
            ch.toolchain_origins("https://m.example/x/", environ={}, fetch=fetch),
            ["https://m.example/x"],
        )
        self.assertEqual(
            ch.toolchain_origins(
                environ={"SOLDR_TOOLCHAIN_ORIGIN": "https://env.example"},
                fetch=fetch,
            ),
            ["https://env.example"],
        )
        fetch.assert_not_called()

    def test_redirect_disabled_uses_builtins_only(self) -> None:
        fetch = mock.Mock()
        for off in ("0", "false", "off", "NO"):
            self.assertEqual(
                ch.toolchain_origins(
                    environ={"SOLDR_TOOLCHAIN_REDIRECT": off}, fetch=fetch
                ),
                [FORGE, LEGACY],
            )
        fetch.assert_not_called()

    def test_redirect_url_override_and_merge(self) -> None:
        fetch = mock.Mock(return_value=["https://new.example/t"])
        origins = ch.toolchain_origins(
            environ={"SOLDR_TOOLCHAIN_REDIRECT_URL": "https://r.example/m.json"},
            fetch=fetch,
        )
        fetch.assert_called_once_with("https://r.example/m.json")
        self.assertEqual(origins, ["https://new.example/t", FORGE, LEGACY])

    def test_unreachable_manifest_falls_back_to_builtins(self) -> None:
        fetch = mock.Mock(return_value=None)
        self.assertEqual(ch.toolchain_origins(environ={}, fetch=fetch), [FORGE, LEGACY])
        fetch.assert_called_once_with(ch.DEFAULT_TOOLCHAIN_REDIRECT_URL)

    def test_fetch_redirect_origins_never_raises(self) -> None:
        with mock.patch.object(
            ch, "open_url", side_effect=urllib.error.URLError("offline")
        ):
            self.assertIsNone(
                ch.fetch_redirect_origins(ch.DEFAULT_TOOLCHAIN_REDIRECT_URL)
            )
        self.assertIsNone(ch.fetch_redirect_origins("http://insecure.example/m"))


class FailoverTest(unittest.TestCase):
    def test_failover_classification(self) -> None:
        def http_error(code: int) -> urllib.error.HTTPError:
            return urllib.error.HTTPError("https://x", code, "msg", Message(), None)

        self.assertTrue(ch.is_failover_error(http_error(404)))
        self.assertTrue(ch.is_failover_error(http_error(503)))
        self.assertFalse(ch.is_failover_error(http_error(403)))
        self.assertTrue(ch.is_failover_error(urllib.error.URLError("down")))
        self.assertTrue(ch.is_failover_error(TimeoutError()))
        self.assertFalse(ch.is_failover_error(SystemExit("sha256 mismatch")))
        self.assertTrue(ch.is_failover_error(ch.OriginFetchError("x", failover=True)))
        self.assertFalse(ch.is_failover_error(ch.OriginFetchError("x", failover=False)))

    def test_moves_on_after_404_and_stops_on_success(self) -> None:
        calls: list[str] = []

        def action(origin: str) -> str:
            calls.append(origin)
            if origin == "https://a.example":
                raise urllib.error.HTTPError(origin, 404, "gone", Message(), None)
            return origin

        stderr = io.StringIO()
        result = ch.with_origin_failover(
            ["https://a.example", "https://b.example", "https://c.example"],
            action,
            label="test file",
            stderr=stderr,
        )
        self.assertEqual(result, "https://b.example")
        self.assertEqual(calls, ["https://a.example", "https://b.example"])
        self.assertIn("trying https://b.example", stderr.getvalue())

    def test_integrity_errors_do_not_fail_over(self) -> None:
        calls: list[str] = []

        def action(origin: str) -> None:
            calls.append(origin)
            raise SystemExit("sha256 mismatch")

        with self.assertRaisesRegex(SystemExit, "sha256 mismatch"):
            ch.with_origin_failover(
                ["https://a.example", "https://b.example"], action, label="t"
            )
        self.assertEqual(calls, ["https://a.example"])

    def test_last_origin_error_propagates(self) -> None:
        def action(origin: str) -> None:
            raise ch.OriginFetchError(f"down {origin}", failover=True)

        with self.assertRaisesRegex(SystemExit, "down https://b.example"):
            ch.with_origin_failover(
                ["https://a.example", "https://b.example"],
                action,
                label="t",
                stderr=io.StringIO(),
            )
        with self.assertRaises(SystemExit):
            ch.with_origin_failover([], action, label="t")


if __name__ == "__main__":
    unittest.main()
