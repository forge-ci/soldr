#!/usr/bin/env python3
"""Scheduled win-gnu link smoke: cross-build a fixture to
`x86_64-pc-windows-gnu` and assert the output is a real PE.

soldr#2336's last checklist item — a scheduled-only (never path-triggered)
win-gnu CI lane, with a Windows-host and a Linux-host lane. #2341 landed the
consumption path (Windows host → WinLibs gcc; Linux x64 host → conda-forge mingw
*cross* gcc; PE-link-proven on forge). This is the regression canary for it.

Rather than parse `soldr build` logs, the check is end-to-end and objective:
build a minimal crate through the blessed surface and assert the emitted binary
is a **PE32+ x86-64** image. The PE parse is pure and unit-tested by
`test_win_gnu_link_smoke.py`; the `smoke` subcommand wires it to a real build.

`--no-cache` is deliberate: it exercises the *toolchain/link* path, which is
what this lane guards, and side-steps an unrelated embedded-cache defect on
win-gnu output (no `.pdb` is produced, so the cache's compiler-output
materialization trips — soldr#2347). Conflating that cache defect with a link
regression here would make the canary lie.

Subcommands
-----------
  smoke --soldr <bin> [--target x86_64-pc-windows-gnu] [--summary <file>]
        create a fixture, `soldr build --no-cache --target ...`, assert the
        output is a PE32+ x86-64 executable. Exit non-zero on any failure.
  assert-pe --path <file>
        assert one existing file is a PE32+ x86-64 image (used in tests/ad hoc).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import struct
import subprocess
import sys
import tempfile
from dataclasses import dataclass

TARGET = "x86_64-pc-windows-gnu"
TOOLCHAIN = "1.98.1"
IMAGE_FILE_MACHINE_AMD64 = 0x8664


@dataclass(frozen=True)
class PeCheck:
    """Whether a file is an x86-64 PE, and the reason for the verdict."""

    ok: bool
    reason: str


def is_pe_amd64(path: str) -> PeCheck:
    """Return a PeCheck. `ok` is True only for a PE whose COFF machine is
    AMD64 (x86-64). Parses the DOS stub `e_lfanew` -> `PE\\0\\0` -> machine word,
    the minimal structure that distinguishes a real PE from an ELF/Mach-O or a
    truncated file — no external `file`/objdump dependency."""
    try:
        with open(path, "rb") as fh:
            data = fh.read(0x400)
    except OSError as exc:
        return PeCheck(False, f"cannot read {path}: {exc}")
    if len(data) < 0x40 or data[:2] != b"MZ":
        return PeCheck(False, "missing 'MZ' DOS signature (not a PE)")
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if e_lfanew + 6 > len(data):
        return PeCheck(False, f"PE header offset {e_lfanew:#x} outside the read window")
    if data[e_lfanew : e_lfanew + 4] != b"PE\x00\x00":
        return PeCheck(False, "missing 'PE\\0\\0' signature at e_lfanew")
    machine = struct.unpack_from("<H", data, e_lfanew + 4)[0]
    if machine != IMAGE_FILE_MACHINE_AMD64:
        return PeCheck(
            False,
            f"COFF machine {machine:#06x} is not AMD64 ({IMAGE_FILE_MACHINE_AMD64:#06x})",
        )
    return PeCheck(True, "PE32+ x86-64")


def _write_fixture(root: str) -> str:
    """Materialize a minimal buildable bin crate, pinned so soldr's toolchain
    contract is satisfied without an `--allow-unpinned` escape hatch."""
    crate = os.path.join(root, "wg_smoke")
    os.makedirs(os.path.join(crate, "src"), exist_ok=True)
    with open(os.path.join(crate, "Cargo.toml"), "w", encoding="utf-8") as fh:
        fh.write(
            "[package]\n"
            'name = "wg_smoke"\n'
            'version = "0.0.0"\n'
            'edition = "2021"\n\n'
            "[profile.dev]\n"
            "debug = false\n"
        )
    with open(os.path.join(crate, "src", "main.rs"), "w", encoding="utf-8") as fh:
        fh.write('fn main() {\n    println!("win-gnu link smoke");\n}\n')
    with open(os.path.join(crate, "rust-toolchain.toml"), "w", encoding="utf-8") as fh:
        fh.write(f'[toolchain]\nchannel = "{TOOLCHAIN}"\n')
    return crate


def _output_exe(crate: str, target: str) -> str:
    # CI leaves CARGO_TARGET_DIR unset -> per-crate `target/`. Some dev
    # containers export a shared one; honor it so the smoke works in both.
    base = os.environ.get("CARGO_TARGET_DIR") or os.path.join(crate, "target")
    return os.path.join(base, target, "debug", "wg_smoke.exe")


def resolve_soldr(raw: str) -> str:
    """Absolute path to the soldr binary, resolved against the *invocation* cwd.

    The smoke runs soldr with `cwd` set to a throwaway fixture crate, so a
    relative `--soldr` -- which is what the workflow passes
    (`./target/debug/soldr`) -- would otherwise be resolved against that
    temporary directory instead of the checkout. Every scheduled run of this
    lane on record died that way:

        FileNotFoundError: [Errno 2] No such file or directory:
            './target/debug/soldr'

    even though the preceding build step had just succeeded.
    """
    return os.path.abspath(raw)


def print_ascii(text: str) -> None:
    """`print`, but never dependent on the console's codec.

    soldr#2819: a U+2705 verdict was printed and the windows-host lane died with
    `UnicodeEncodeError: 'charmap' codec can't encode character` -- *after* the
    cross-build had succeeded and the PE check had passed. Python encodes stdout
    with the locale codepage on Windows, so any non-ASCII byte is a gamble on
    the runner's locale.

    Escaping here rather than choosing an ASCII rendering at each call site: a
    call site can be changed back, and the failure only appears on one platform
    that no PR exercises. This holds however it is called.
    """
    print(text.encode("ascii", "backslashreplace").decode("ascii"))


def render_summary(target: str, exe: str, ok: bool, reason: str, *, marks: bool) -> str:
    """The smoke's verdict, with or without Unicode verdict marks.

    soldr#2819: the verdict used U+2705 / U+274C and was `print`ed. The step
    summary file is opened with `encoding="utf-8"` and was fine; **stdout is
    not** -- on Windows, Python encodes it with the locale codepage, so the
    windows-host lane died with

        UnicodeEncodeError: 'charmap' codec can't encode character '\u2705'

    *after* the cross-build had succeeded and the PE check had passed. The lane
    was one `print` away from green.

    Rather than reconfiguring global stdio, the marks stay in the file that
    renders them. Nothing this script prints needs to be non-ASCII, so nothing
    it prints depends on the console's codec.
    """
    if marks:
        verdict = ("\u2705 " if ok else "\u274c ") + reason
    else:
        verdict = ("PASS: " if ok else "FAIL: ") + reason
    return (
        f"## win-gnu link smoke ({target})\n\n"
        f"- build: `soldr --no-cache build --target {target}`\n"
        f"- output: `{os.path.basename(exe)}`\n"
        f"- verdict: {verdict}\n"
    )


def cmd_smoke(args: argparse.Namespace) -> int:
    soldr = resolve_soldr(args.soldr)
    if not os.path.isfile(soldr):
        # A named error beats a traceback: this lane is scheduled-only, so
        # whoever reads the failure is doing so days later with no context.
        print(
            f"FAIL: soldr binary not found at {soldr}\n"
            f"  (--soldr was {args.soldr!r}, resolved against {os.getcwd()})\n"
            "  The build step must produce it before this step runs.",
            file=sys.stderr,
        )
        return 1
    with tempfile.TemporaryDirectory() as tmp:
        crate = _write_fixture(tmp)
        cmd = [soldr, "--no-cache", "build", "--target", args.target]
        print_ascii(f"$ {' '.join(cmd)}  (cwd={crate})")
        proc = subprocess.run(cmd, cwd=crate, check=False)
        if proc.returncode != 0:
            print(f"FAIL: soldr build exited {proc.returncode}", file=sys.stderr)
            return proc.returncode or 1
        exe = _output_exe(crate, args.target)
        check = is_pe_amd64(exe)
        ok, reason = check.ok, check.reason
        if args.summary:
            with open(args.summary, "a", encoding="utf-8") as fh:
                fh.write(render_summary(args.target, exe, ok, reason, marks=True))
        # ASCII to stdout: see render_summary.
        print_ascii(render_summary(args.target, exe, ok, reason, marks=False))
        if not ok:
            print(f"FAIL: {reason}", file=sys.stderr)
            return 1
        print("OK: produced a PE32+ x86-64 win-gnu executable.")
        return 0


def _basename_either_separator(path: str) -> str:
    r"""The final component, splitting on `/` and `\` regardless of host.

    `os.path.basename` only knows the separators of the interpreter it runs on,
    so on Linux it reads `D:\a\soldr\target\...\soldr.exe` as one long component
    and finds no match at all. That made this parser depend on the OS running
    it rather than on the log it was handed -- which is backwards, because the
    log records whatever host produced the build.

    In practice the smoke locates on the same host that built, so both spellings
    resolve correctly today; the host-dependence was invisible until the test
    covering the Windows shape actually ran on Linux (soldr#2823).
    """
    return re.split(r"[\\/]", path)[-1]


def locate_executable(build_log: str, name: str) -> str | None:
    """The path cargo says it wrote for binary `name`.

    `cargo build --message-format=json-render-diagnostics` emits one
    `compiler-artifact` record per unit, and the ones that produced a binary
    carry an absolute `executable`. Reading it is the only way to know the
    output path without re-deriving cargo's layout.

    Guessing `target/debug/<name>` is wrong on Windows: `soldr cargo build`
    injects `CARGO_BUILD_TARGET` there and nowhere else --

        // cargo_front_door/target.rs
        if host_os != Windows || !should_inject_windows_target(..) {
            return Ok(None);
        }
        Ok(Some(TargetTriple::detect()?.triple()))

    -- so cargo writes `target/<triple>/debug/<name>.exe` on Windows and
    `target/debug/<name>` on Linux. The smoke's windows-host lane hardcoded the
    Linux shape and never found the binary it had just built.

    Deliberately not re-deriving the triple here: that would be a second
    implementation of soldr's host-target rule, free to disagree with the first.
    """
    found = None
    with open(build_log, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if record.get("reason") != "compiler-artifact":
                continue
            executable = record.get("executable")
            if not executable:
                continue
            stem = os.path.splitext(_basename_either_separator(executable))[0]
            if stem == name:
                # Later records win: a rebuild in the same log supersedes.
                found = executable
    return found


def cmd_locate(args: argparse.Namespace) -> int:
    executable = locate_executable(args.build_log, args.name)
    if executable is None:
        print(
            f"FAIL: no compiler-artifact record for {args.name!r} in "
            f"{args.build_log}.\n"
            "  The build must run with --message-format=json-render-diagnostics "
            "for its output path to be readable.",
            file=sys.stderr,
        )
        return 1
    print(executable)
    return 0


def cmd_assert_pe(args: argparse.Namespace) -> int:
    check = is_pe_amd64(args.path)
    ok, reason = check.ok, check.reason
    print(reason)
    return 0 if ok else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_smoke = sub.add_parser("smoke", help="build a fixture and assert a PE")
    p_smoke.add_argument("--soldr", required=True, help="path to the soldr binary")
    p_smoke.add_argument("--target", default=TARGET)
    p_smoke.add_argument("--summary", default=None)
    p_smoke.set_defaults(func=cmd_smoke)

    p_locate = sub.add_parser(
        "locate",
        help="print the executable path cargo recorded for a binary",
    )
    p_locate.add_argument(
        "--build-log", required=True, help="cargo --message-format=json output"
    )
    p_locate.add_argument("--name", default="soldr", help="binary name")
    p_locate.set_defaults(func=cmd_locate)

    p_pe = sub.add_parser("assert-pe", help="assert a file is a PE32+ x86-64 image")
    p_pe.add_argument("--path", required=True)
    p_pe.set_defaults(func=cmd_assert_pe)

    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
