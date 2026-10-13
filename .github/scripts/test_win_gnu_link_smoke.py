#!/usr/bin/env python3
"""Pure tests for the win-gnu link-smoke PE verifier."""

from __future__ import annotations

import os
import struct
import tempfile
import unittest

from _script_loader import load_sibling_script

script = load_sibling_script("win_gnu_link_smoke")


def _make_pe(machine: int = 0x8664, *, pe_sig: bytes = b"PE\x00\x00") -> bytes:
    """Build a minimal in-memory PE: DOS header with `e_lfanew`, then the PE
    signature + a COFF machine word at the pointed-to offset."""
    e_lfanew = 0x80
    buf = bytearray(0x100)
    buf[0:2] = b"MZ"
    struct.pack_into("<I", buf, 0x3C, e_lfanew)
    buf[e_lfanew : e_lfanew + 4] = pe_sig
    struct.pack_into("<H", buf, e_lfanew + 4, machine)
    return bytes(buf)


def _write(tmp: str, name: str, data: bytes) -> str:
    path = os.path.join(tmp, name)
    with open(path, "wb") as fh:
        fh.write(data)
    return path


class PeDetectionTests(unittest.TestCase):
    def test_amd64_pe_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = _write(tmp, "ok.exe", _make_pe(0x8664))
            check = script.is_pe_amd64(path)
            ok, reason = check.ok, check.reason
            self.assertTrue(ok, reason)

    def test_non_amd64_machine_rejected(self) -> None:
        # 0x01C0 == ARM; a valid PE but not the target architecture.
        with tempfile.TemporaryDirectory() as tmp:
            path = _write(tmp, "arm.exe", _make_pe(0x01C0))
            check = script.is_pe_amd64(path)
            ok, reason = check.ok, check.reason
            self.assertFalse(ok)
            self.assertIn("AMD64", reason)

    def test_missing_mz_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = _write(tmp, "elf", b"\x7fELF" + b"\x00" * 0x80)
            check = script.is_pe_amd64(path)
            ok, reason = check.ok, check.reason
            self.assertFalse(ok)
            self.assertIn("MZ", reason)

    def test_missing_pe_sig_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = _write(tmp, "bad.exe", _make_pe(0x8664, pe_sig=b"XX\x00\x00"))
            check = script.is_pe_amd64(path)
            ok, reason = check.ok, check.reason
            self.assertFalse(ok)
            self.assertIn("PE", reason)

    def test_truncated_file_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = _write(tmp, "tiny", b"MZ")
            ok = script.is_pe_amd64(path).ok
            self.assertFalse(ok)

    def test_missing_file_rejected(self) -> None:
        check = script.is_pe_amd64("/nonexistent/never.exe")
        ok, reason = check.ok, check.reason
        self.assertFalse(ok)
        self.assertIn("cannot read", reason)


class FixtureTests(unittest.TestCase):
    def test_fixture_is_pinned_and_buildable_shape(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            crate = script._write_fixture(tmp)
            self.assertTrue(os.path.isfile(os.path.join(crate, "Cargo.toml")))
            self.assertTrue(os.path.isfile(os.path.join(crate, "src", "main.rs")))
            with open(
                os.path.join(crate, "rust-toolchain.toml"), encoding="utf-8"
            ) as fh:
                self.assertIn(script.TOOLCHAIN, fh.read())
            exe = script._output_exe(crate, script.TARGET)
            self.assertTrue(exe.replace("\\", "/").endswith("wg_smoke.exe"))
            self.assertIn(script.TARGET, exe)


if __name__ == "__main__":
    unittest.main()
