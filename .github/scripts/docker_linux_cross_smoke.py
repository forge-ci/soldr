#!/usr/bin/env python3
"""Exercise the Windows-host Docker Linux cross-build surface (soldr#2319)."""

from __future__ import annotations

import argparse
import os
import shutil
import struct
import subprocess
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class ElfVerdict:
    ok: bool
    reason: str


def is_elf_amd64(path: Path) -> ElfVerdict:
    try:
        data = path.read_bytes()[:32]
    except OSError as exc:
        return ElfVerdict(False, f"cannot read {path}: {exc}")
    if len(data) < 20 or data[:4] != b"\x7fELF":
        return ElfVerdict(False, "missing ELF signature")
    if data[4:6] != b"\x02\x01":
        return ElfVerdict(False, "not a 64-bit little-endian ELF")
    if struct.unpack_from("<H", data, 18)[0] != 62:
        return ElfVerdict(False, "ELF machine is not x86-64")
    return ElfVerdict(True, "ELF64 x86-64")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", required=True)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--target", default="x86_64-unknown-linux-gnu")
    parser.add_argument("--summary", type=Path)
    args = parser.parse_args()

    fixture = args.fixture.resolve()
    soldr_arg = Path(args.soldr)
    if soldr_arg.parent == Path("."):
        soldr = shutil.which(args.soldr)
        if soldr is None:
            parser.error(f"--soldr command not found on PATH: {args.soldr}")
    else:
        soldr = str(soldr_arg.resolve())
    command = [str(soldr), "--no-cache", "build", "--target", args.target]
    print(f"$ {' '.join(command)}  (cwd={fixture})", flush=True)
    result = subprocess.run(command, cwd=fixture, check=False)
    if result.returncode:
        return result.returncode or 1

    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", fixture / "target"))
    binary = target_dir / args.target / "debug" / "docker-cross-c"
    verdict = is_elf_amd64(binary)
    report = (
        "## Windows-host Docker Linux cross smoke\n\n"
        f"- target: `{args.target}`\n"
        f"- fixture: `{fixture.name}` (C + C++)\n"
        f"- output: `{binary}`\n"
        f"- verdict: {'OK' if verdict.ok else 'FAIL'} — {verdict.reason}\n"
    )
    print(report)
    if args.summary:
        args.summary.open("a", encoding="utf-8").write(report)
    return 0 if verdict.ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
