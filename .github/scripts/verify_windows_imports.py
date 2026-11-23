#!/usr/bin/env python3
"""Ratchet the DLLs the Windows release binaries import (soldr#1060).

Every other release surface now has a portability guard: musl artifacts are
checked for static linking, gnu artifacts have a glibc-floor ratchet, Darwin
binaries have a minimum-OS ratchet. Windows had none, and it has the same
failure mode -- a binary that imports a DLL the target machine lacks does not
report a helpful error, it fails to start at all.

Measured on published v0.8.29, `soldr.exe` and `soldr-daemon.exe` import
exactly one non-system DLL:

    VCRUNTIME140.dll

That one ships with the Visual C++ Redistributable, not with Windows. The
`api-ms-win-crt-*` entries beside it are UCRT forwarders and *are* part of
Windows 10+, so they are not a dependency in the same sense. On a machine
without the redistributable -- a fresh Windows Server, a clean container,
a locked-down build agent -- soldr.exe fails with "VCRUNTIME140.dll was not
found".

Nothing in the repo sets `-C target-feature=+crt-static`, so this is Rust's
MSVC default rather than a decision. Building the release lane with a static
CRT would remove the dependency outright; that is a build change with release
risk, so this script does not make it. It records the current import set so a
*new* non-system dependency cannot appear unnoticed, and names the
redistributable one explicitly so it stays visible.

Usage:
    python3 .github/scripts/verify_windows_imports.py <binary.exe> [...]

Exit codes:
  0 - every import is a stock-Windows DLL or a known redistributable one
  1 - an unexpected import appeared, or the file could not be parsed
"""

from __future__ import annotations

import argparse
import struct
import sys
from pathlib import Path

# Present on a stock Windows 10+ install. `api-ms-win-*` are the API-set
# forwarders (UCRT and friends), which ship with the OS.
SYSTEM_PREFIXES = ("api-ms-win-", "ext-ms-win-")
SYSTEM_DLLS = frozenset(
    {
        "advapi32.dll",
        "bcrypt.dll",
        "bcryptprimitives.dll",
        "combase.dll",
        "crypt32.dll",
        "iphlpapi.dll",  # Windows IP Helper API, used by network interface discovery.
        "kernel32.dll",
        "kernelbase.dll",
        "ntdll.dll",
        "ole32.dll",
        "oleaut32.dll",
        "pdh.dll",
        "powrprof.dll",
        "psapi.dll",
        "secur32.dll",
        "shell32.dll",
        "user32.dll",
        "userenv.dll",
        "version.dll",
        "ws2_32.dll",
    }
)

# Allowed, but NOT part of Windows -- they come from the Visual C++
# Redistributable. Listed rather than silently tolerated so the cost stays
# visible: every one of these is a machine that needs the redist installed.
REDISTRIBUTABLE_DLLS = frozenset(
    {
        "vcruntime140.dll",
        "vcruntime140_1.dll",
        "msvcp140.dll",
        "concrt140.dll",
    }
)


class PEError(Exception):
    """The file could not be parsed. Never a pass."""


def is_system_dll(name: str) -> bool:
    lowered = name.lower()
    if lowered in SYSTEM_DLLS:
        return True
    return any(lowered.startswith(prefix) for prefix in SYSTEM_PREFIXES)


def is_redistributable_dll(name: str) -> bool:
    return name.lower() in REDISTRIBUTABLE_DLLS


def unexpected_imports(names: "list[str]") -> "list[str]":
    """Imports that are neither stock Windows nor a known redistributable.

    Deduplicated case-insensitively, because DLL names are: `Foo.dll` and
    `FOO.dll` name the same file on Windows, and reporting both would read as
    two separate problems. The first spelling seen is the one reported, so the
    message matches what is actually in the import table.
    """
    seen: dict[str, str] = {}
    for name in names:
        if is_system_dll(name) or is_redistributable_dll(name):
            continue
        seen.setdefault(name.lower(), name)
    return sorted(seen.values(), key=str.lower)


def pe_imports(data: bytes) -> "list[str]":  # noqa: C901
    """The DLL names in a PE's import directory.

    Raises PEError rather than returning an empty list on anything it cannot
    read: an empty list would read as "no dependencies", which is the most
    reassuring possible answer and the least likely to be true.
    """
    if len(data) < 0x40 or data[:2] != b"MZ":
        raise PEError("not a PE image (no MZ header)")
    (pe_offset,) = struct.unpack_from("<I", data, 0x3C)
    if pe_offset + 24 > len(data) or data[pe_offset : pe_offset + 4] != b"PE\0\0":
        raise PEError("no PE signature")

    (num_sections,) = struct.unpack_from("<H", data, pe_offset + 6)
    (opt_size,) = struct.unpack_from("<H", data, pe_offset + 20)
    opt_offset = pe_offset + 24
    if opt_offset + 2 > len(data):
        raise PEError("optional header runs past the end")
    (magic,) = struct.unpack_from("<H", data, opt_offset)
    if magic == 0x20B:  # PE32+
        dir_offset = opt_offset + 112
    elif magic == 0x10B:  # PE32
        dir_offset = opt_offset + 96
    else:
        raise PEError(f"unknown optional-header magic {magic:#x}")

    # Data directory entry 1 is the import table.
    if dir_offset + 16 > len(data):
        raise PEError("data directory runs past the end")
    import_rva, _import_size = struct.unpack_from("<II", data, dir_offset + 8)
    if import_rva == 0:
        raise PEError("no import directory")

    sections = []
    section_offset = opt_offset + opt_size
    for index in range(num_sections):
        base = section_offset + 40 * index
        if base + 40 > len(data):
            raise PEError("section headers run past the end")
        virtual_size, virtual_address = struct.unpack_from("<II", data, base + 8)
        raw_size, raw_pointer = struct.unpack_from("<II", data, base + 16)
        sections.append((virtual_address, max(virtual_size, raw_size), raw_pointer))

    def to_offset(rva: int) -> int:
        for virtual_address, span, raw_pointer in sections:
            if virtual_address <= rva < virtual_address + span:
                return raw_pointer + (rva - virtual_address)
        raise PEError(f"cannot map RVA {rva:#x} to a file offset")

    names: list[str] = []
    cursor = to_offset(import_rva)
    while True:
        if cursor + 20 > len(data):
            raise PEError("import descriptors run past the end")
        descriptor = data[cursor : cursor + 20]
        if descriptor == b"\0" * 20:
            break
        (name_rva,) = struct.unpack_from("<I", descriptor, 12)
        if name_rva == 0:
            break
        name_offset = to_offset(name_rva)
        end = data.find(b"\0", name_offset)
        if end < 0:
            raise PEError("unterminated DLL name")
        names.append(data[name_offset:end].decode("ascii", "replace"))
        cursor += 20

    if not names:
        raise PEError("import directory contains no DLLs")
    return names


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+", help="PE binaries to inspect")
    args = parser.parse_args(argv)

    failures = 0
    for name in args.binaries:
        try:
            data = Path(name).read_bytes()
        except OSError as error:
            print(
                f"verify_windows_imports: cannot read {name}: {error}", file=sys.stderr
            )
            failures += 1
            continue

        try:
            names = pe_imports(data)
        except PEError as error:
            print(
                f"verify_windows_imports: cannot inspect {name}: {error}",
                file=sys.stderr,
            )
            failures += 1
            continue

        unexpected = unexpected_imports(names)
        redist = sorted({n for n in names if is_redistributable_dll(n)}, key=str.lower)

        if unexpected:
            failures += 1
            print(
                f"verify_windows_imports: {name} imports "
                f"{len(unexpected)} unexpected DLL(s) (soldr#1060):",
                file=sys.stderr,
            )
            for dll in unexpected:
                print(f"  {dll}", file=sys.stderr)
            print(
                "  These are neither stock Windows nor a known redistributable, "
                "so any machine without them cannot start the binary at all.",
                file=sys.stderr,
            )
            print(
                "  Either drop the dependency, or add it to the allowlist "
                "deliberately with a note about who has to install it.",
                file=sys.stderr,
            )
            continue

        detail = (
            f"; needs the VC++ redistributable for {', '.join(redist)}"
            if redist
            else ""
        )
        print(
            f"verify_windows_imports: {name} imports {len(names)} DLL(s), "
            f"all expected{detail} - OK"
        )

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
