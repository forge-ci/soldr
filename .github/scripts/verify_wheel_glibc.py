#!/usr/bin/env python3
"""Check the glibc floor of the binaries *inside* a manylinux wheel (soldr#1060).

`release-auto.yml` already asserts that every linux-gnu wheel carries the
`manylinux_2_17` platform tag. That catches maturin quietly falling back to the
runner's glibc and tagging `manylinux_2_39` -- a wheel that pip on an older
host then skips.

It does not catch the inverse, which is worse. A wheel tagged `manylinux_2_17`
whose embedded `soldr` actually requires `GLIBC_2.39` is *installed* by pip on
Debian 12, because the tag promises it will work -- and then fails at run time
with "version `GLIBC_2.39' not found". A skipped wheel is a visible install
problem; a mis-tagged one is a broken program.

The tag is a claim. This verifies the claim against the bytes.

Usage:
    python3 .github/scripts/verify_wheel_glibc.py --max-glibc 2.17 dist/*.whl

Exit codes:
  0 - every embedded binary is at or below the ceiling
  1 - a binary exceeds it, or no embedded binary was found to check
"""

from __future__ import annotations

import argparse
import importlib.util
import sys
import tempfile
import zipfile
from pathlib import Path

_HERE = Path(__file__).resolve().parent


def _load_baseline_module():
    """Reuse the readelf parsing rather than reimplementing it, so the two
    checks can never disagree about what a glibc requirement looks like."""
    spec = importlib.util.spec_from_file_location(
        "verify_glibc_baseline", _HERE / "verify_glibc_baseline.py"
    )
    if spec is None or spec.loader is None:  # pragma: no cover - packaging error
        raise ImportError("cannot load verify_glibc_baseline.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _is_script_binary(path: Path) -> bool:
    """maturin's `bin` layout: `<name>-<version>.data/scripts/<exe>`."""
    return path.parent.name == "scripts" and path.parent.parent.name.endswith(".data")


def _is_extension_module(path: Path, extract_dir: Path) -> bool:
    """A native extension module: `*.so` (`.abi3.so`, `.cpython-*.so`, ...)
    outside the `.dist-info` metadata directory.

    `soldr wheel` builds PyO3 abi3 extensions (soldr#2139), whose ELF lives in
    the package rather than under `.data/scripts`. Without this the gate found
    nothing in such a wheel and failed as "no binary", so it could not verify
    the manylinux_2_17 claim soldr#3432 made for host-target release wheels.
    """
    relative = path.relative_to(extract_dir)
    if any(part.endswith(".dist-info") for part in relative.parts):
        return False
    return path.suffix == ".so" or ".so." in path.name


def embedded_binaries(wheel: Path, extract_dir: Path) -> "list[Path]":
    """Extract `wheel` and return the native ELF files inside it.

    Two layouts: maturin places console binaries at
    `<name>-<version>.data/scripts/<exe>`, and extension modules at
    `<package>/<module>.abi3.so` (or a top-level `<module>.abi3.so`).
    Matching on those layouts rather than on a bare filename keeps unrelated
    files (`RECORD`, `METADATA`, Python sources) out.
    """
    with zipfile.ZipFile(wheel) as archive:
        archive.extractall(extract_dir)
    return [
        path
        for path in sorted(extract_dir.rglob("*"))
        if path.is_file()
        and (_is_script_binary(path) or _is_extension_module(path, extract_dir))
    ]


def main(argv: "list[str] | None" = None) -> int:  # noqa: C901
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wheels", nargs="+", help="wheel files to inspect")
    parser.add_argument(
        "--max-glibc",
        default="2.17",
        help="Highest glibc symbol version an embedded binary may require.",
    )
    args = parser.parse_args(argv)

    baseline = _load_baseline_module()
    ceiling = baseline.parse_version(args.max_glibc)
    if not ceiling:
        print(
            f"verify_wheel_glibc: --max-glibc {args.max_glibc!r} is not a version",
            file=sys.stderr,
        )
        return 1

    failures = 0
    checked = 0
    with tempfile.TemporaryDirectory(prefix="soldr-wheel-glibc-") as tmp:
        for index, wheel_arg in enumerate(args.wheels):
            wheel = Path(wheel_arg)
            if not wheel.is_file():
                print(
                    f"verify_wheel_glibc: not a file: {wheel}",
                    file=sys.stderr,
                )
                failures += 1
                continue
            target = Path(tmp) / str(index)
            try:
                binaries = embedded_binaries(wheel, target)
            except (zipfile.BadZipFile, OSError) as error:
                print(
                    f"verify_wheel_glibc: cannot read {wheel}: {error}",
                    file=sys.stderr,
                )
                failures += 1
                continue

            if not binaries:
                # A wheel with nothing to check is not a pass. Either the
                # layout changed or maturin shipped no binary, and both mean
                # this gate stopped gating.
                print(
                    f"verify_wheel_glibc: {wheel.name} contains no "
                    f"'*.data/scripts/*' binary or '*.so' extension module to verify",
                    file=sys.stderr,
                )
                failures += 1
                continue

            for binary in binaries:
                checked += 1
                try:
                    # Reuses the sibling baseline script's readelf parser
                    # rather than duplicating it; these two ship together.
                    # pylint: disable-next=protected-access
                    code, output = baseline._readelf_versions(str(binary))
                except (OSError, FileNotFoundError) as error:
                    print(
                        f"verify_wheel_glibc: cannot inspect {binary.name}: {error}",
                        file=sys.stderr,
                    )
                    failures += 1
                    continue
                if code != 0:
                    print(
                        f"verify_wheel_glibc: cannot inspect {binary.name}: "
                        f"readelf exited {code}",
                        file=sys.stderr,
                    )
                    failures += 1
                    continue

                required = baseline.max_glibc_requirement(output)
                if required is None or required <= ceiling:
                    shown = (
                        "no glibc symbols"
                        if required is None
                        else f"GLIBC_{baseline.format_version(required)}"
                    )
                    print(
                        f"verify_wheel_glibc: {wheel.name} -> {binary.name} "
                        f"needs at most {shown} (ceiling "
                        f"{baseline.format_version(ceiling)}) - OK"
                    )
                    continue

                failures += 1
                print(
                    f"verify_wheel_glibc: {wheel.name} is tagged for a "
                    f"{baseline.format_version(ceiling)} floor but its "
                    f"{binary.name} requires "
                    f"GLIBC_{baseline.format_version(required)} (soldr#1060).",
                    file=sys.stderr,
                )
                print(
                    "  pip trusts the tag, so this wheel INSTALLS on older "
                    "hosts and then fails at run time -- worse than being "
                    "skipped.",
                    file=sys.stderr,
                )

    if not checked and not failures:
        print("verify_wheel_glibc: no binaries were checked", file=sys.stderr)
        return 1
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
