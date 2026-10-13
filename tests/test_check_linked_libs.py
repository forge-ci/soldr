"""Tests for the linked-shared-library lint (soldr static-liblzma follow-up).

Builds tiny synthetic ELF64 and Mach-O fixtures by hand rather than shelling
out to a real compiler, so the whole suite runs with no toolchain and no
`readelf`/`otool` dependency -- matching the script under test.
"""

from __future__ import annotations

import struct
from pathlib import Path

from conftest import load_script_module

check_linked_libs = load_script_module(
    Path(__file__).parents[1] / "ci" / "check_linked_libs.py", "check_linked_libs"
)


# --------------------------------------------------------------------------
# ELF64 fixture builder
# --------------------------------------------------------------------------


def build_elf64(needed: list[str]) -> bytes:
    """A minimal 64-bit LE ELF whose dynamic segment lists *needed* names.

    One `PT_LOAD` segment identity-maps the whole file at vaddr 0, so the
    dynamic string table's virtual address equals its file offset -- this
    keeps the fixture's address translation trivial while still exercising
    the real translation code path in `read_elf_needed`.
    """
    ehdr_size = 64
    phdr_off = ehdr_size
    phdr_size = 56 * 2  # PT_LOAD + PT_DYNAMIC
    dynamic_off = phdr_off + phdr_size
    dynamic_entry_count = len(needed) + 2  # + DT_STRTAB + DT_NULL
    dynamic_size = 16 * dynamic_entry_count
    strtab_off = dynamic_off + dynamic_size

    strtab = bytearray(b"\x00")
    name_offsets = []
    for name in needed:
        name_offsets.append(len(strtab))
        strtab += name.encode("utf-8") + b"\x00"
    file_size = strtab_off + len(strtab)

    e_ident = b"\x7fELF" + bytes([2, 1, 1, 0]) + b"\x00" * 8
    ehdr_rest = struct.pack(
        "<HHIQQQIHHHHHH",
        3,  # e_type: ET_DYN
        0x3E,  # e_machine: EM_X86_64
        1,  # e_version
        0,  # e_entry
        phdr_off,  # e_phoff
        0,  # e_shoff
        0,  # e_flags
        ehdr_size,  # e_ehsize
        56,  # e_phentsize
        2,  # e_phnum
        0,  # e_shentsize
        0,  # e_shnum
        0,  # e_shstrndx
    )
    assert len(e_ident) + len(ehdr_rest) == ehdr_size

    phdr_load = struct.pack("<IIQQQQQQ", 1, 5, 0, 0, 0, file_size, file_size, 0x1000)
    phdr_dynamic = struct.pack(
        "<IIQQQQQQ",
        2,
        6,
        dynamic_off,
        dynamic_off,
        dynamic_off,
        dynamic_size,
        dynamic_size,
        8,
    )

    dynamic = bytearray()
    for offset in name_offsets:
        dynamic += struct.pack("<QQ", 1, offset)  # DT_NEEDED
    dynamic += struct.pack("<QQ", 5, strtab_off)  # DT_STRTAB (vaddr == offset)
    dynamic += struct.pack("<QQ", 0, 0)  # DT_NULL
    assert len(dynamic) == dynamic_size

    return bytes(e_ident + ehdr_rest + phdr_load + phdr_dynamic + dynamic + strtab)


def build_static_elf64() -> bytes:
    """An ELF with no `PT_DYNAMIC` segment at all -- a static binary."""
    ehdr_size = 64
    phdr_off = ehdr_size
    phdr_size = 56
    file_size = phdr_off + phdr_size

    e_ident = b"\x7fELF" + bytes([2, 1, 1, 0]) + b"\x00" * 8
    ehdr_rest = struct.pack(
        "<HHIQQQIHHHHHH",
        2,
        0x3E,
        1,
        0,
        phdr_off,
        0,
        0,
        ehdr_size,
        56,
        1,
        0,
        0,
        0,
    )
    phdr_load = struct.pack("<IIQQQQQQ", 1, 5, 0, 0, 0, file_size, file_size, 0x1000)
    return bytes(e_ident + ehdr_rest + phdr_load)


# --------------------------------------------------------------------------
# Mach-O fixture builder
# --------------------------------------------------------------------------

_LC_LOAD_DYLIB = 0xC


def _dylib_command(name: str) -> bytes:
    name_bytes = name.encode("utf-8") + b"\x00"
    name_offset = 24
    cmdsize = name_offset + len(name_bytes)
    return (
        struct.pack("<IIIIII", _LC_LOAD_DYLIB, cmdsize, name_offset, 0, 0, 0)
        + name_bytes
    )


def build_macho_thin(names: list[str]) -> bytes:
    commands = b"".join(_dylib_command(name) for name in names)
    header = struct.pack(
        "<IIIIIIII",
        0xFEEDFACF,  # MH_MAGIC_64
        0x01000007,  # CPU_TYPE_X86_64
        3,
        2,  # MH_EXECUTE
        len(names),
        len(commands),
        0,
        0,
    )
    return header + commands


def build_macho_fat(slices: list[bytes]) -> bytes:
    header = struct.pack(">II", 0xCAFEBABE, len(slices))
    arch_table_size = 20 * len(slices)
    offset = len(header) + arch_table_size
    arch_entries = bytearray()
    body = bytearray()
    for data in slices:
        arch_entries += struct.pack(">iiIII", 0x01000007, 3, offset, len(data), 12)
        body += data
        offset += len(data)
    return bytes(header) + bytes(arch_entries) + bytes(body)


# --------------------------------------------------------------------------
# read_elf_needed / read_macho_dylibs
# --------------------------------------------------------------------------


def test_reads_needed_entries_from_a_dynamic_elf(tmp_path: Path) -> None:
    binary = tmp_path / "dynamic.elf"
    binary.write_bytes(build_elf64(["libc.so.6", "liblzma.so.5"]))
    with binary.open("rb") as f:
        assert check_linked_libs.read_elf_needed(f) == ["libc.so.6", "liblzma.so.5"]


def test_a_static_elf_has_no_needed_entries(tmp_path: Path) -> None:
    binary = tmp_path / "static.elf"
    binary.write_bytes(build_static_elf64())
    with binary.open("rb") as f:
        assert check_linked_libs.read_elf_needed(f) == []


def test_non_elf_bytes_return_none() -> None:
    import io

    assert check_linked_libs.read_elf_needed(io.BytesIO(b"not an elf file")) is None


def test_reads_dylib_names_from_a_thin_macho(tmp_path: Path) -> None:
    binary = tmp_path / "thin.macho"
    binary.write_bytes(
        build_macho_thin(["/usr/lib/libSystem.B.dylib", "/usr/local/lib/libfoo.dylib"])
    )
    with binary.open("rb") as f:
        assert check_linked_libs.read_macho_dylibs(f) == [
            "/usr/lib/libSystem.B.dylib",
            "/usr/local/lib/libfoo.dylib",
        ]


def test_reads_dylib_names_from_a_fat_macho(tmp_path: Path) -> None:
    slice_a = build_macho_thin(["/usr/lib/libSystem.B.dylib"])
    slice_b = build_macho_thin(["/usr/local/lib/libbar.dylib"])
    binary = tmp_path / "universal.macho"
    binary.write_bytes(build_macho_fat([slice_a, slice_b]))
    with binary.open("rb") as f:
        names = check_linked_libs.read_macho_dylibs(f)
    assert names == ["/usr/lib/libSystem.B.dylib", "/usr/local/lib/libbar.dylib"]


# --------------------------------------------------------------------------
# disallowed_libs / check_binary / main
# --------------------------------------------------------------------------


def test_linux_allowlist_accepts_glibc_musl_and_the_loader() -> None:
    names = [
        "libc.so.6",
        "libm.so.6",
        "libgcc_s.so.1",
        "libdl.so.2",
        "librt.so.1",
        "libpthread.so.0",
        "libutil.so.1",
        "ld-linux-x86-64.so.2",
        "libc.musl-x86_64.so.1",
        "ld-musl-x86_64.so.1",
    ]
    assert check_linked_libs.disallowed_libs("elf", names) == []


def test_linux_allowlist_rejects_a_vendored_shared_library() -> None:
    assert check_linked_libs.disallowed_libs("elf", ["liblzma.so.5"]) == [
        "liblzma.so.5"
    ]


def test_macos_allowlist_accepts_system_libraries_and_frameworks() -> None:
    names = [
        "/usr/lib/libSystem.B.dylib",
        "/usr/lib/libc++.1.dylib",
        "/usr/lib/libiconv.2.dylib",
        "/usr/lib/libresolv.9.dylib",
        "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
    ]
    assert check_linked_libs.disallowed_libs("macho", names) == []


def test_macos_allowlist_rejects_a_third_party_dylib() -> None:
    assert check_linked_libs.disallowed_libs(
        "macho", ["/usr/local/lib/libssl.3.dylib"]
    ) == ["/usr/local/lib/libssl.3.dylib"]


def test_check_binary_passes_a_compliant_elf(tmp_path: Path) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(build_elf64(["libc.so.6", "libm.so.6"]))
    result = check_linked_libs.check_binary(binary)
    assert result is not None
    assert result.kind == "elf"
    assert result.linked == ("libc.so.6", "libm.so.6")
    assert result.offenders == ()


def test_check_binary_fails_an_elf_with_a_vendored_dependency(tmp_path: Path) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(build_elf64(["libc.so.6", "liblzma.so.5"]))
    result = check_linked_libs.check_binary(binary)
    assert result is not None
    assert result.kind == "elf"
    assert result.offenders == ("liblzma.so.5",)


def test_check_binary_returns_none_for_an_unsupported_format(tmp_path: Path) -> None:
    binary = tmp_path / "soldr.exe"
    binary.write_bytes(b"MZ\x90\x00\x03\x00\x00\x00 not a real PE file")
    assert check_linked_libs.check_binary(binary) is None


def test_main_passes_on_a_compliant_binary(tmp_path: Path, capsys) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(build_elf64(["libc.so.6"]))
    exit_code = check_linked_libs.main([str(binary)])
    assert exit_code == 0
    assert "OK" in capsys.readouterr().out


def test_main_fails_on_a_vendored_dependency(tmp_path: Path, capsys) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(build_elf64(["liblzma.so.5"]))
    exit_code = check_linked_libs.main([str(binary)])
    captured = capsys.readouterr()
    assert exit_code == 1
    assert "liblzma.so.5" in captured.err
    assert "static" in captured.err


def test_main_skips_and_exits_zero_for_an_unsupported_format(
    tmp_path: Path, capsys
) -> None:
    binary = tmp_path / "soldr.exe"
    binary.write_bytes(b"MZ\x90\x00 not a real PE file")
    exit_code = check_linked_libs.main([str(binary)])
    assert exit_code == 0
    assert "skipped: unsupported format" in capsys.readouterr().out


def test_main_fails_on_a_missing_file(tmp_path: Path, capsys) -> None:
    exit_code = check_linked_libs.main([str(tmp_path / "does-not-exist")])
    assert exit_code == 1
    assert "no such file" in capsys.readouterr().err
