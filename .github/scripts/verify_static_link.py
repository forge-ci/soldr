#!/usr/bin/env python3
"""Verify a musl release binary is actually statically linked (soldr#1060).

soldr#1060's whole premise is that the musl artifacts are the
"download and run anywhere" build: they sidestep
`/lib64/libc.so.6: version GLIBC_2.xx not found` precisely because they carry
no dynamic dependencies. Nothing checked that. A musl target that silently
picked up a dynamic link still *builds*, still passes tests on the modern CI
image, and only fails on the old distro the artifact exists to serve — which
is the worst place to find out.

This is the RFC's fourth acceptance item ("static-link verification step
(`readelf -d` must show 'no dynamic section') added to release CI"), which
stands alone: it guards the musl artifacts release-auto.yml already produces,
independent of the artifact-renaming and installer-default work the RFC
defers.

The RFC's literal wording is too strict, though, and taking it literally was a
bug. **static-PIE** keeps a `.dynamic` section for its own relocations while
carrying no interpreter and no `NEEDED` entries -- the published musl `crgx`
and `cargo-chef` are exactly that, and they run on glibc and musl alike. So
the real test is "nothing to resolve at load time": no INTERP, no NEEDED.

Usage:
    python3 .github/scripts/verify_static_link.py <binary> [<binary> ...]

Exits non-zero if any binary needs an interpreter or a shared library.
"""

from __future__ import annotations

import argparse
import shutil
import sys
from pathlib import Path

# Dependency-free capture also works when this script is invoked by absolute path.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

# `readelf -d` prints exactly this for a fully static ELF. Matched
# case-insensitively on a normalized string so binutils wording drift in
# spacing or capitalisation does not silently turn the check into a no-op.
_NO_DYNAMIC_SECTION = "there is no dynamic section in this file"


def has_no_dynamic_section(readelf_output: str) -> bool:
    """True when `readelf -d` reports no dynamic section at all."""
    normalized = " ".join(readelf_output.lower().split())
    return _NO_DYNAMIC_SECTION in normalized


def requires_interpreter(program_headers: str) -> bool:
    """True when the ELF asks the kernel for a dynamic loader.

    An INTERP program header is what makes a binary depend on an `ld.so`
    being present, which is the thing that actually fails on a foreign
    system.
    """
    return "INTERP" in program_headers


def is_statically_linked(readelf_output: str, program_headers: str = "") -> bool:
    """True when nothing has to be resolved at load time.

    Not "has no dynamic section" -- that was the original test, and it is
    wrong for **static-PIE**, which keeps a `.dynamic` section for its own
    relocations while carrying no interpreter and no `NEEDED` entries. The
    published musl `crgx` and `cargo-chef` are exactly that: `file` calls them
    "static-pie linked", they run on both glibc and musl hosts, and the old
    check called them dynamic.

    That false positive mattered because it blocked the obvious fix for the
    real gap -- only `soldr` was ever checked, while the release bundles four
    binaries. Extending the check would have failed the release on correct
    artifacts.

    So the test is what portability actually requires: no interpreter to find,
    and no shared libraries to resolve. Pure, so it is unit-tested without
    binutils or a real ELF.
    """
    if not readelf_output.strip():
        # No output is not evidence of anything. Without this, "found no
        # NEEDED entries" and "read nothing at all" reach the same answer,
        # and a broken invocation reports every binary as static.
        return False
    if requires_interpreter(program_headers):
        return False
    if has_no_dynamic_section(readelf_output):
        return True
    # A dynamic section with no NEEDED entries and no interpreter is
    # static-PIE: self-contained, nothing to load.
    return not dynamic_dependencies(readelf_output)


def dynamic_dependencies(readelf_output: str) -> "list[str]":
    """The `NEEDED` shared libraries named in the output, for the failure
    message. A reader needs to know *what* leaked in, not just that
    something did."""
    found: list[str] = []
    for line in readelf_output.splitlines():
        if "(NEEDED)" not in line:
            continue
        # e.g. " 0x0001 (NEEDED)  Shared library: [libc.so.6]"
        start, end = line.find("["), line.rfind("]")
        if 0 <= start < end:
            found.append(line[start + 1 : end])
        else:
            found.append(line.strip())
    return found


def _readelf_program_headers(binary: str) -> "tuple[int, str]":
    """Run `readelf -l`, returning `(exit_code, combined_output)`.

    Separate from `-d` because the INTERP program header is what tells us the
    binary needs a loader; the dynamic section alone cannot distinguish
    static-PIE from dynamically linked.
    """
    tool = shutil.which("readelf") or shutil.which("llvm-readelf")
    if tool is None:
        raise FileNotFoundError("readelf not found on PATH")
    completed = run_captured(
        [tool, "-l", binary],
        capture_output=True,
        text=True,
        check=False,
    )
    return completed.returncode, completed.stdout + completed.stderr


def _readelf(binary: str) -> "tuple[int, str]":
    """Run `readelf -d`, returning `(exit_code, combined_output)`.

    The exit code matters on its own: readelf exits non-zero when it cannot
    read the file at all (missing path, not an ELF). Treating that as ordinary
    output would let a *non-existent* binary fall through to the
    "has a dynamic section" branch and report the wrong reason -- which is
    exactly what happened on Linux while a Windows box, having no readelf at
    all, took the missing-tool path and looked fine.
    """
    tool = shutil.which("readelf") or shutil.which("llvm-readelf")
    if tool is None:
        raise FileNotFoundError("readelf not found on PATH")
    completed = run_captured(
        [tool, "-d", binary],
        capture_output=True,
        text=True,
        check=False,
    )
    # readelf reports "no dynamic section" on stdout but still exits 0; a
    # genuinely unreadable file lands on stderr, so keep both.
    return completed.returncode, completed.stdout + completed.stderr


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+", help="ELF binaries to verify")
    args = parser.parse_args(argv)

    failures = 0
    for binary in args.binaries:
        try:
            code, output = _readelf(binary)
            headers_code, headers = _readelf_program_headers(binary)
        except (OSError, FileNotFoundError) as error:
            # A missing tool or unreadable path is a wiring problem. Fail
            # loudly rather than pass by default -- a verification step that
            # silently skips is worse than no step at all.
            print(
                f"verify_static_link: cannot inspect {binary}: {error}", file=sys.stderr
            )
            failures += 1
            continue

        if code != 0:
            # readelf could not read the file (missing path, not an ELF). We
            # learned nothing, so this is "cannot verify", not "dynamic".
            print(
                f"verify_static_link: cannot inspect {binary}: "
                f"readelf exited {code}: {output.strip().splitlines()[-1] if output.strip() else 'no output'}",
                file=sys.stderr,
            )
            failures += 1
            continue

        if headers_code != 0:
            print(
                f"verify_static_link: cannot inspect {binary}: readelf -l exited {headers_code}",
                file=sys.stderr,
            )
            failures += 1
            continue

        if is_statically_linked(output, headers):
            print(f"verify_static_link: {binary} is statically linked - OK")
            continue

        failures += 1
        needed = dynamic_dependencies(output)
        print(
            f"verify_static_link: {binary} HAS a dynamic section, so it is not "
            f"the portable static artifact it is published as (soldr#1060).",
            file=sys.stderr,
        )
        for lib in needed:
            print(f"  needs: {lib}", file=sys.stderr)
        print(
            "  A musl target must link statically; a dynamic dependency here "
            "reproduces the GLIBC-version failure the musl build exists to avoid.",
            file=sys.stderr,
        )

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
