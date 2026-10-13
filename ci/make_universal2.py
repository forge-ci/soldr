#!/usr/bin/env python3
"""Combine per-architecture Mach-O binaries into one universal2 binary.

Why this exists rather than `lipo`: soldr's blessed LLVM (llvm-21.1.5) ships
llvm-ar and llvm-strip but no llvm-lipo, and Apple's lipo needs a Mac. Adding a
toolchain dependency to emit a header that is 20 lines of struct packing would
work against the point of the change, which is to carry fewer moving parts.

The fat format is a big-endian header followed by the untouched thin files:

    uint32  magic        0xCAFEBABE (FAT_MAGIC)
    uint32  nfat_arch
    per arch:
      int32   cputype        read from each slice, never hardcoded
      int32   cpusubtype
      uint32  offset         aligned to 1 << align
      uint32  size
      uint32  align          2^12 for x86_64, 2^14 for arm64, as lipo emits

    ci/make_universal2.py -o soldr-universal2 soldr-x86_64 soldr-arm64
"""

import argparse
import struct
import sys
from dataclasses import dataclass
from pathlib import Path

MH_MAGIC_64 = 0xFEEDFACF
MH_CIGAM_64 = 0xCFFAEDFE
FAT_MAGIC = 0xCAFEBABE
CPU_TYPE_X86_64 = 0x01000007
CPU_TYPE_ARM64 = 0x0100000C
# Apple's lipo aligns arm64 slices to 16K pages and x86_64 to 4K.
DEFAULT_ALIGN = {CPU_TYPE_X86_64: 12, CPU_TYPE_ARM64: 14}
NAMES = {CPU_TYPE_X86_64: "x86_64", CPU_TYPE_ARM64: "arm64"}


@dataclass(frozen=True)
class ThinSlice:
    """One thin 64-bit Mach-O: its CPU identity and raw contents."""

    cputype: int
    cpusubtype: int
    data: bytes


@dataclass(frozen=True)
class FatArch:
    """One `fat_arch` header entry laid out for a slice."""

    cputype: int
    cpusubtype: int
    offset: int
    size: int
    align: int


def thin_info(path: Path) -> ThinSlice:
    """The cputype, cpusubtype and contents of one thin 64-bit Mach-O."""
    data = path.read_bytes()
    if len(data) < 16:
        raise SystemExit(f"{path}: too small to be a Mach-O")
    (magic,) = struct.unpack_from("<I", data, 0)
    if magic in (FAT_MAGIC, 0xBEBAFECA):
        raise SystemExit(f"{path}: already a fat binary; pass thin slices")
    if magic not in (MH_MAGIC_64, MH_CIGAM_64):
        raise SystemExit(f"{path}: not a 64-bit Mach-O (magic {magic:#x})")
    endian = "<" if magic == MH_MAGIC_64 else ">"
    cputype, cpusubtype = struct.unpack_from(endian + "ii", data, 4)
    return ThinSlice(cputype, cpusubtype, data)


def build(out: Path, inputs: list[Path]) -> None:
    slices = [thin_info(p) for p in inputs]

    seen: set[int] = set()
    for thin, p in zip(slices, inputs, strict=True):
        if thin.cputype in seen:
            raise SystemExit(
                f"{p}: duplicate architecture {NAMES.get(thin.cputype, thin.cputype)}"
            )
        seen.add(thin.cputype)

    header = 8 + 20 * len(slices)
    offset = header
    entries: list[FatArch] = []
    for thin in slices:
        align = DEFAULT_ALIGN.get(thin.cputype, 14)
        step = 1 << align
        offset = (offset + step - 1) // step * step
        entries.append(
            FatArch(thin.cputype, thin.cpusubtype, offset, len(thin.data), align)
        )
        offset += len(thin.data)

    blob = bytearray(struct.pack(">II", FAT_MAGIC, len(slices)))
    for arch in entries:
        blob += struct.pack(
            ">iiIII",
            arch.cputype,
            arch.cpusubtype,
            arch.offset,
            arch.size,
            arch.align,
        )
    for thin, arch in zip(slices, entries, strict=True):
        blob += b"\x00" * (arch.offset - len(blob))
        blob += thin.data

    out.write_bytes(bytes(blob))
    out.chmod(0o755)
    for arch in entries:
        print(
            f"  {NAMES.get(arch.cputype, hex(arch.cputype)):8} "
            f"cputype={arch.cputype:#x} subtype={arch.cpusubtype} "
            f"offset={arch.offset} size={arch.size} align=2^{arch.align}"
        )
    print(f"wrote {out} ({out.stat().st_size} bytes, {len(slices)} slices)")


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("-o", "--output", required=True, type=Path)
    ap.add_argument("inputs", nargs="+", type=Path)
    args = ap.parse_args(argv)
    if len(args.inputs) < 2:
        raise SystemExit("need at least two thin slices")
    build(args.output, args.inputs)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
