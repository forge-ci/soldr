#!/usr/bin/env python3
"""Fail if a binary dynamically links a non-system shared library.

Follow-up to the static-liblzma fix: `lzma-sys`'s build.rs used to link
whatever liblzma pkg-config found on the build host, which auditwheel then
vendored into `soldr.libs/` with an `$ORIGIN/../soldr.libs` RPATH. soldr's
relocated shim images cannot resolve that relative RPATH, so every wrapped
compile failed with "error while loading shared libraries: liblzma...".
Static-linking `xz2`'s liblzma fixed the one instance found; this script
generalizes the check so the *next* accidental dynamic dependency on a
non-system library is a loud build failure instead of a silent host-luck
bug.

Only Linux ELF (`DT_NEEDED` via the dynamic segment, read straight from the
program headers so it also works on a fully stripped binary with no section
header table) and macOS Mach-O (`LC_LOAD_DYLIB` and its weak/reexport/upward
variants, including universal/fat binaries) are checked. Every other format
(Windows PE, unknown) is reported as skipped and does not fail the run --
Windows binaries do not exhibit this bug class (no auditwheel-style vendoring
step exists for them in soldr's release pipeline).

No `readelf` / `otool` dependency: everything here is a small pure-Python
binary-format reader, so it is unit-testable with synthetic fixture bytes.

Usage::

    python3 ci/check_linked_libs.py <binary> [<binary> ...]

Exit code is 0 when every binary is either compliant or skipped (unsupported
format), 1 when any binary links a disallowed library or does not exist.
"""

from __future__ import annotations

import argparse
import fnmatch
import struct
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO

# --------------------------------------------------------------------------
# ELF (Linux)
# --------------------------------------------------------------------------

_ELF_MAGIC = b"\x7fELF"
_PT_LOAD = 1
_PT_DYNAMIC = 2
_DT_NULL = 0
_DT_NEEDED = 1
_DT_STRTAB = 5


@dataclass(frozen=True)
class LoadSegment:
    """A `PT_LOAD` program header: virtual address, size and file offset."""

    vaddr: int
    memsz: int
    file_offset: int


def _elf_program_headers(f: BinaryIO, e_phoff: int, e_phentsize: int, e_phnum: int):
    for i in range(e_phnum):
        f.seek(e_phoff + i * e_phentsize)
        raw = f.read(56)
        if len(raw) < 56:
            return
        p_type, _flags, p_offset, p_vaddr, _paddr, p_filesz, p_memsz, _align = (
            struct.unpack("<IIQQQQQQ", raw)
        )
        yield p_type, p_offset, p_vaddr, p_filesz, p_memsz


def read_elf_needed(f: BinaryIO) -> list[str] | None:  # noqa: C901
    """Return every `DT_NEEDED` name, or `None` if *f* is not a supported ELF.

    Only 64-bit little-endian ELF is supported -- every soldr Linux release
    target (`x86_64`/`aarch64`-*-linux-*) is exactly that. Reads via the
    program headers (`PT_DYNAMIC`), which the dynamic loader itself relies
    on, so this also works when the section header table has been stripped.
    """
    f.seek(0)
    ident = f.read(16)
    if len(ident) < 16 or ident[:4] != _ELF_MAGIC:
        return None
    ei_class, ei_data = ident[4], ident[5]
    if ei_class != 2 or ei_data != 1:  # not 64-bit, or not little-endian
        return None

    ehdr = f.read(48)
    if len(ehdr) < 48:
        return None
    (
        _e_type,
        _e_machine,
        _e_version,
        _e_entry,
        e_phoff,
        _e_shoff,
        _e_flags,
        _e_ehsize,
        e_phentsize,
        e_phnum,
        _e_shentsize,
        _e_shnum,
        _e_shstrndx,
    ) = struct.unpack("<HHIQQQIHHHHHH", ehdr)

    dynamic_off: int | None = None
    dynamic_size = 0
    loads: list[LoadSegment] = []
    for p_type, p_offset, p_vaddr, p_filesz, p_memsz in _elf_program_headers(
        f, e_phoff, e_phentsize, e_phnum
    ):
        if p_type == _PT_DYNAMIC:
            dynamic_off, dynamic_size = p_offset, p_filesz
        elif p_type == _PT_LOAD:
            loads.append(
                LoadSegment(vaddr=p_vaddr, memsz=p_memsz, file_offset=p_offset)
            )

    if dynamic_off is None:
        # No PT_DYNAMIC segment at all: a fully static binary. Trivially
        # compliant -- there is nothing to dynamically link.
        return []

    f.seek(dynamic_off)
    raw = f.read(dynamic_size)
    strtab_vaddr: int | None = None
    needed_offsets: list[int] = []
    for i in range(dynamic_size // 16):
        d_tag, d_val = struct.unpack_from("<QQ", raw, i * 16)
        if d_tag == _DT_NULL:
            break
        if d_tag == _DT_NEEDED:
            needed_offsets.append(d_val)
        elif d_tag == _DT_STRTAB:
            strtab_vaddr = d_val

    if strtab_vaddr is None:
        return []

    strtab_file_off = None
    for segment in loads:
        if segment.vaddr <= strtab_vaddr < segment.vaddr + segment.memsz:
            strtab_file_off = segment.file_offset + (strtab_vaddr - segment.vaddr)
            break
    if strtab_file_off is None:
        return []

    names = []
    for rel_off in needed_offsets:
        f.seek(strtab_file_off + rel_off)
        chunk = bytearray()
        while True:
            byte = f.read(1)
            if not byte or byte == b"\x00":
                break
            chunk += byte
        names.append(chunk.decode("utf-8", "replace"))
    return names


# --------------------------------------------------------------------------
# Mach-O (macOS)
# --------------------------------------------------------------------------

_MH_MAGIC_64 = 0xFEEDFACF
_MH_CIGAM_64 = 0xCFFAEDFE
_FAT_MAGIC = 0xCAFEBABE
_DYLIB_LOAD_COMMANDS = {
    0xC,  # LC_LOAD_DYLIB
    0x18,  # LC_LOAD_WEAK_DYLIB
    0x1F,  # LC_REEXPORT_DYLIB
    0x23,  # LC_LOAD_UPWARD_DYLIB
}


def _macho_thin_dylibs(f: BinaryIO, base_offset: int, endian: str) -> list[str]:
    f.seek(base_offset)
    header = f.read(32)
    if len(header) < 32:
        return []
    _magic, _cputype, _cpusubtype, _filetype, ncmds, _sizeofcmds, _flags, _reserved = (
        struct.unpack(endian + "IIIIIIII", header)
    )

    names: list[str] = []
    offset = base_offset + 32
    for _ in range(ncmds):
        f.seek(offset)
        cmd_header = f.read(8)
        if len(cmd_header) < 8:
            break
        cmd, cmdsize = struct.unpack(endian + "II", cmd_header)
        if cmdsize < 8:
            break
        if cmd in _DYLIB_LOAD_COMMANDS and cmdsize >= 24:
            f.seek(offset + 8)
            (name_offset,) = struct.unpack(endian + "I", f.read(4))
            if 0 <= name_offset < cmdsize:
                f.seek(offset + name_offset)
                raw = f.read(cmdsize - name_offset)
                names.append(raw.split(b"\x00", 1)[0].decode("utf-8", "replace"))
        offset += cmdsize
    return names


def read_macho_dylibs(f: BinaryIO) -> list[str] | None:
    """Return every `LC_LOAD_DYLIB`-family name, or `None` if unsupported.

    Handles a thin 64-bit Mach-O directly and a fat/universal binary by
    reading each 32-bit `fat_arch` slice's own thin header. 32-bit Mach-O
    and the 64-bit fat-arch variant are not part of soldr's release matrix
    and are reported as unsupported.
    """
    f.seek(0)
    header = f.read(4)
    if len(header) < 4:
        return None

    magic_le = struct.unpack("<I", header)[0]
    if magic_le in (_MH_MAGIC_64, _MH_CIGAM_64):
        endian = "<" if magic_le == _MH_MAGIC_64 else ">"
        return _macho_thin_dylibs(f, 0, endian)

    magic_be = struct.unpack(">I", header)[0]
    if magic_be != _FAT_MAGIC:
        return None

    f.seek(4)
    (nfat,) = struct.unpack(">I", f.read(4))
    names: list[str] = []
    for i in range(nfat):
        f.seek(8 + i * 20)
        arch = f.read(20)
        if len(arch) < 20:
            break
        _cputype, _cpusubtype, slice_offset, _size, _align = struct.unpack(
            ">iiIII", arch
        )
        f.seek(slice_offset)
        inner_magic_bytes = f.read(4)
        if len(inner_magic_bytes) < 4:
            continue
        inner_magic = struct.unpack("<I", inner_magic_bytes)[0]
        if inner_magic in (_MH_MAGIC_64, _MH_CIGAM_64):
            inner_endian = "<" if inner_magic == _MH_MAGIC_64 else ">"
            names.extend(_macho_thin_dylibs(f, slice_offset, inner_endian))
    return names


# --------------------------------------------------------------------------
# Allowlists and the shared check
# --------------------------------------------------------------------------

# glibc/toolchain runtime + musl equivalents. `ld-linux-*.so.*` and
# `ld-musl-*` are the dynamic loader itself, which some toolchains list as a
# DT_NEEDED entry as well as the PT_INTERP path.
LINUX_ALLOW = [
    "libc.so.6",
    "libm.so.6",
    "libgcc_s.so.1",
    "libdl.so.2",
    "librt.so.1",
    "libpthread.so.0",
    "libutil.so.1",
    "ld-linux-*.so.*",
    "libc.musl-*.so.1",
    "ld-musl-*",
]

MACOS_ALLOW = [
    "/usr/lib/libSystem.B.dylib",
    "/usr/lib/libc++*.dylib",
    "/usr/lib/libiconv*.dylib",
    "/usr/lib/libresolv*.dylib",
    "/System/Library/Frameworks/*",
]

_ALLOWLISTS = {"elf": LINUX_ALLOW, "macho": MACOS_ALLOW}


def disallowed_libs(kind: str, names: list[str]) -> list[str]:
    """Every name in *names* that matches none of *kind*'s allowlist patterns."""
    allow = _ALLOWLISTS[kind]
    return [
        name
        for name in names
        if not any(fnmatch.fnmatchcase(name, pattern) for pattern in allow)
    ]


@dataclass(frozen=True)
class BinaryCheck:
    """`kind` is `elf` or `macho`; `offenders` is the disallowed subset of `linked`."""

    kind: str
    linked: tuple[str, ...]
    offenders: tuple[str, ...]


def check_binary(path: Path) -> BinaryCheck | None:
    """Classify *path* and report its disallowed dynamic dependencies.

    Returns a `BinaryCheck` (kind, all linked names, offenders), or `None` if the file is
    not a format this script understands (skip, not a failure).
    """
    with path.open("rb") as f:
        elf_needed = read_elf_needed(f)
        if elf_needed is not None:
            return BinaryCheck(
                "elf",
                tuple(elf_needed),
                tuple(disallowed_libs("elf", elf_needed)),
            )
        macho_dylibs = read_macho_dylibs(f)
        if macho_dylibs is not None:
            return BinaryCheck(
                "macho",
                tuple(macho_dylibs),
                tuple(disallowed_libs("macho", macho_dylibs)),
            )
    return None


_ADVICE = (
    "A non-system shared library dependency means this binary was dynamically "
    "linked against something found on the build host instead of being built "
    "in statically. Enable the offending crate's static/bundled Cargo feature "
    '(e.g. xz2\'s "static" feature for liblzma) instead -- see '
    "crates/soldr-cli/tests/guards/static_liblzma.rs for the shim-image "
    "failure this class of bug causes."
)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+", type=Path)
    args = parser.parse_args(argv)

    exit_code = 0
    for binary in args.binaries:
        if not binary.is_file():
            print(f"{binary}: FAIL - no such file", file=sys.stderr)
            exit_code = 1
            continue

        result = check_binary(binary)
        if result is None:
            print(f"{binary}: skipped: unsupported format")
            continue

        kind, needed, offenders = result.kind, result.linked, result.offenders
        if offenders:
            exit_code = 1
            print(
                f"{binary}: FAIL ({kind}) - disallowed dynamic dependencies:",
                file=sys.stderr,
            )
            for lib in offenders:
                print(f"  {lib}", file=sys.stderr)
            print(f"  {_ADVICE}", file=sys.stderr)
        else:
            plural = "y" if len(needed) == 1 else "ies"
            print(f"{binary}: OK ({kind}, {len(needed)} allowed dependenc{plural})")

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
