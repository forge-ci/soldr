#!/usr/bin/env python3
"""Stage Soldr's executable and debug sidecars for a release archive.

This is the release workflow's narrow packaging boundary: it copies the built
``soldr`` executable to ``dist/package``, creates the ``soldr-daemon``
sidecar, and (Windows only) stages the required PDB. ``package_dir`` is
exactly what ``release_manifest.py`` and the main ``.tar.zst`` archive step
consume, so a missing Windows PDB must fail here.

soldr#3038: Linux debug info and macOS ``.dSYM`` sidecars are staged by
:func:`stage_debug_symbols` into a SEPARATE directory, never into
``package_dir``. That tree is what ``release_manifest.py::collect_debug_info``
reads for ``soldr.debug_info``, and every ``setup-soldr`` consumer runs
``.github/actions/setup-soldr/zccache_contract.py::validate_release_manifest``
against that field, which hard-rejects any entry whose ``format`` is not
``"pdb"``. See ``docs/DEBUG_SIDECARS.md`` for the resulting contract: symbols
ship as their own opt-in ``-symbols.tar.zst`` release asset that
``setup-soldr`` never downloads or parses.

soldr#3038 also replaced rustc's ``split-debuginfo = "packed"`` with a
post-link ``objcopy``/``strip`` carve-out for Linux (both gnu and musl):
"packed" only captures DWARF for the crates rustc itself compiles in this
build, not the precompiled std or the C dependencies built through the `cc`
crate (`ring`, `zstd-sys`, `lzma-sys`, `libsqlite3-sys`, mimalloc-pprof),
which embed their own DWARF in the final link regardless of that setting. The
result was duplication, not splitting: soldr's shipped binary went 21.3 MiB
(stripped) -> 73.8 MiB ("packed") while the `.dwp` beside it held another
48.5 MiB. Running `objcopy --only-keep-debug` on the fully linked binary
after the fact captures every source of DWARF in one pass with nothing left
behind. macOS keeps `split-debuginfo = "packed"` (dsymutil's `.dSYM` model
does not have this duplication problem) with a `strip -x` pass afterward;
Windows is unaffected (MSVC already emits a separate, required PDB).

soldr#3085: the objcopy/strip pass above must use binutils that can read what
the lane just BUILT, not what the lane happens to RUN ON. Most of the release
matrix cross-builds on an x86_64 Linux runner, where the host's GNU binutils
rejects a foreign-arch ELF and cannot read Mach-O at all -- that is what broke
three lanes of release run 33820395040. `select_binutils` therefore keeps host
GNU binutils only for a host-native target and reaches for target-agnostic
`llvm-objcopy` / `llvm-strip` (or the managed cross toolchain's
target-prefixed binutils) otherwise. See that function for the full rationale.

It was extracted from auto-release.yml as part of soldr#2469 step 2.2 so the
platform-specific staging policy can be unit-tested without running a release.

Usage (CI):
    python3 .github/scripts/stage_release_binaries.py \
        --target x86_64-unknown-linux-gnu --package-dir dist/package \
        --symbols-dir dist/symbols
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import platform
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

from release_artifacts import binary_suffix

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

# Loaded with importlib.util.spec_from_file_location rather than a package
# import: this script lives in `.github/scripts/` and `check_linked_libs.py`
# lives in `ci/`, and both are executed as standalone files by CI, not
# imported as a package (see check_third_party_compiles.py for the same
# pattern, adopted here for the same flake8 E402 reason).
_CHECK_LINKED_LIBS_PATH = (
    Path(__file__).resolve().parents[2] / "ci" / "check_linked_libs.py"
)
_CHECK_LINKED_LIBS_SPEC = importlib.util.spec_from_file_location(
    "check_linked_libs", _CHECK_LINKED_LIBS_PATH
)
if (
    _CHECK_LINKED_LIBS_SPEC is None or _CHECK_LINKED_LIBS_SPEC.loader is None
):  # pragma: no cover - packaging accident
    raise ImportError(f"cannot load {_CHECK_LINKED_LIBS_PATH}")
check_linked_libs = importlib.util.module_from_spec(_CHECK_LINKED_LIBS_SPEC)
sys.modules[_CHECK_LINKED_LIBS_SPEC.name] = check_linked_libs
_CHECK_LINKED_LIBS_SPEC.loader.exec_module(check_linked_libs)


class StagingError(RuntimeError):
    """A required release artifact was absent or could not be staged."""


def release_contents(directory: Path) -> str:
    if not directory.is_dir():
        return f"{directory} does not exist"
    entries = sorted(path.name for path in directory.iterdir())
    return "\n".join(f"  {entry}" for entry in entries) or "  (empty)"


def first_file(directory: Path, names: list[str]) -> Path | None:
    return next(
        (directory / name for name in names if (directory / name).is_file()), None
    )


def first_directory(directory: Path, names: list[str]) -> Path | None:
    return next(
        (directory / name for name in names if (directory / name).is_dir()), None
    )


def files_equal(left: Path, right: Path) -> bool:
    """Byte-for-byte comparison.

    Used to VERIFY (soldr#3038 asked for this explicitly, rather than
    assuming) whether ``soldr-daemon`` is still byte-identical to ``soldr``
    after the post-link strip below. ``copy_or_link`` prefers a hardlink, in
    which case stripping ``soldr`` in place already strips ``soldr-daemon``
    for free (same inode). The cross-device copy fallback does not share
    that property, so this check -- not an assumption about which path
    `copy_or_link` took -- decides whether `soldr-daemon` needs re-deriving.
    """
    if not left.is_file() or not right.is_file():
        return False
    if left.stat().st_size != right.stat().st_size:
        return False
    with left.open("rb") as lf, right.open("rb") as rf:
        while True:
            lchunk = lf.read(1024 * 1024)
            rchunk = rf.read(1024 * 1024)
            if lchunk != rchunk:
                return False
            if not lchunk:
                return True


def copy_or_link(source: Path, destination: Path) -> None:
    """Prefer a hardlink for the daemon sidecar, then preserve portability."""
    # os.link rather than Path.hardlink_to: the latter is Python 3.10+, and a
    # release runner that shipped an older python3 broke the v0.9.3 macOS
    # ARM64 lane with AttributeError (soldr#2763). The workflow now pins the
    # interpreter, but the staging boundary must not depend on that to be
    # correct. Note the argument order inverts: os.link(src, dst) creates dst.
    #
    # If `destination` already exists (soldr#3038's stage_debug_symbols
    # re-derives it after a strip), `os.link` raises FileExistsError -- an
    # OSError -- so this still lands on the `shutil.copy2` fallback, which
    # overwrites in place. Correct either way; only the hardlink space
    # saving is lost on a re-derive.
    try:
        os.link(source, destination)
    except OSError:
        shutil.copy2(source, destination)


def mark_executable(path: Path) -> None:
    try:
        path.chmod(path.stat().st_mode | 0o755)
    except OSError:
        # The Windows executable bit is irrelevant, and archive creation still
        # carries the regular file.  Preserve the old workflow's best-effort
        # chmod behavior on hosts that reject the mode change.
        pass


# --------------------------------------------------------------------------
# Cross-target binutils selection (soldr#3085)
#
# soldr#3038 (landed as 6f5b0f1b, after v0.9.11 was cut) made this script run
# `objcopy`/`strip` over the staged binary. Those bare names resolve to the
# HOST's GNU binutils, and half the release matrix cross-builds on an x86_64
# Linux runner:
#
#   * `aarch64-unknown-linux-gnu` has ALWAYS been an x86_64-hosted cross lane.
#     Ubuntu's stock binutils is single-target, so it rejects the aarch64 ELF:
#     "objcopy: Unable to recognise the format of the input file".
#   * `*-apple-darwin` moved from macOS runners to Linux ones in soldr#3073.
#     GNU binutils cannot read Mach-O at ALL, at any architecture:
#     "strip: dist/package/soldr: file format not recognized".
#
# Both were observed in release run 33820395040 (v0.9.12) and both are
# reproducible locally against a clang-built aarch64 ELF / arm64 Mach-O.
#
# The fix is to pick a tool that can read what this lane just BUILT rather
# than what this lane happens to RUN ON. `llvm-objcopy` / `llvm-strip` are
# target-agnostic by construction (one binary, every LLVM object format), so
# they are the first choice for any foreign target; they implement the exact
# GNU options this carve-out uses ("compatible with GNU objcopy"), including
# `--add-gnu-debuglink` and Mach-O `-x`. Host GNU binutils is still used
# verbatim when the target IS the host, so the lanes that work today keep
# byte-for-byte the same tooling.

GNU_OBJCOPY = "objcopy"
GNU_STRIP = "strip"
LLVM_OBJCOPY = "llvm-objcopy"
LLVM_STRIP = "llvm-strip"

_ARCH_ALIASES = {
    "amd64": "x86_64",
    "x86_64": "x86_64",
    "arm64": "aarch64",
    "aarch64": "aarch64",
}


def target_is_host_native(target: str) -> bool:
    """Can the HOST's GNU binutils read an artifact built for ``target``?

    Only when the host is Linux, the target is a Linux (ELF) triple, and the
    architectures match. `x86_64-apple-darwin` on an x86_64 Linux runner is
    the trap this guards: the architecture matches and the object format does
    not, so an arch-only comparison would wrongly keep GNU `strip`.
    """
    if sys.platform != "linux" or "-linux-" not in target:
        return False
    host_arch = _ARCH_ALIASES.get(platform.machine().lower())
    return host_arch is not None and target.split("-", 1)[0] == host_arch


def _managed_llvm_search_dirs() -> list[Path]:
    """Directories that may hold soldr's managed `llvm-*` tools.

    Ordered cheapest/most-specific first. Every entry is a real place these
    binaries are observed in a release lane, not a guess:

      * ``SOLDR_LLVM_DIR`` -- the documented escape hatch soldr's own
        `fetch::llvm` honors, pointing straight at a bin dir.
      * ``$SOLDR_CACHE_DIR/bin`` (CI) and ``~/.soldr/bin`` (developer) hold
        both managed layouts: the full LLVM toolchain
        (``llvm-<version>/[hardlinked/]bin``) and the selective LLVM-tools
        bundle (``syslib/llvm-tools/<ver>/<slug>/package/bin``). Both are
        already first on PATH in the darwin release lanes.
      * ``$RUSTUP_HOME/toolchains/*/lib/rustlib/*/bin`` is rustup's
        `llvm-tools` component, which ships llvm-objcopy and llvm-strip. The
        release workflow installs it so the Linux-cross lanes have a
        deterministic source rather than relying on what a runner image
        happens to preinstall. RUSTUP_HOME is read from the environment
        because setup-soldr relocates it away from ``~/.rustup``.
    """
    dirs: list[Path] = []

    explicit = os.environ.get("SOLDR_LLVM_DIR")
    if explicit:
        dirs.append(Path(explicit))

    soldr_roots: list[Path] = []
    cache_dir = os.environ.get("SOLDR_CACHE_DIR")
    if cache_dir:
        soldr_roots.append(Path(cache_dir) / "bin")
    soldr_roots.append(Path.home() / ".soldr" / "bin")
    for root in soldr_roots:
        for pattern in (
            "llvm-*/bin",
            "llvm-*/hardlinked/bin",
            "syslib/llvm-tools/*/*/package/bin",
        ):
            dirs.extend(sorted(root.glob(pattern)))

    rustup_home = os.environ.get("RUSTUP_HOME")
    rustup_roots = [Path(rustup_home)] if rustup_home else [Path.home() / ".rustup"]
    for root in rustup_roots:
        dirs.extend(sorted(root.glob("toolchains/*/lib/rustlib/*/bin")))

    return dirs


def find_llvm_tool(name: str) -> str | None:
    """Locate one `llvm-*` tool on PATH, then in soldr's managed layouts."""
    found = shutil.which(name)
    if found:
        return found
    for directory in _managed_llvm_search_dirs():
        candidate = directory / name
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    return None


def find_cross_gnu_tool(target: str, name: str) -> str | None:
    """Locate target-prefixed GNU binutils from the managed cross toolchain.

    A secondary fallback for ELF targets only. soldr's GNU/Linux bundle
    exports ``AR_<target_with_underscores>`` as an ABSOLUTE path to
    ``<bundle>/bin/<prefix>-ar`` (e.g. ``aarch64-conda-linux-gnu-ar``), so the
    sibling ``<prefix>-objcopy`` / ``<prefix>-strip`` are derivable from it
    without hard-coding a vendor prefix that soldr may change.

    Deliberately restricted to ``-linux-`` targets with an absolute AR path:
    the darwin lanes export ``AR_aarch64_apple_darwin=llvm-ar`` and the native
    musl lane exports a bare ``ar``, neither of which names a cross bundle.
    """
    if "-linux-" not in target:
        return None
    ar = os.environ.get(f"AR_{target.replace('-', '_')}")
    if not ar:
        return None
    ar_path = Path(ar)
    if not ar_path.is_absolute() or not ar_path.name.endswith("-ar"):
        return None
    candidate = ar_path.with_name(f"{ar_path.name[: -len('ar')]}{name}")
    if candidate.is_file() and os.access(candidate, os.X_OK):
        return str(candidate)
    return None


@dataclass(frozen=True)
class Binutils:
    """The ``objcopy`` and ``strip`` tools selected for one target."""

    objcopy: str
    strip: str


def select_binutils(target: str) -> Binutils:
    """Choose the ``objcopy`` and ``strip`` tools able to process artifacts built for ``target``.

    Never returns a "skip" sentinel: a release that cannot carve symbols must
    say which tools it looked for and stop, not silently ship an unstripped
    binary with no sidecar.
    """
    if target_is_host_native(target):
        return Binutils(GNU_OBJCOPY, GNU_STRIP)

    objcopy = find_llvm_tool(LLVM_OBJCOPY) or find_cross_gnu_tool(target, "objcopy")
    strip_tool = find_llvm_tool(LLVM_STRIP) or find_cross_gnu_tool(target, "strip")
    if objcopy and strip_tool:
        return Binutils(objcopy, strip_tool)

    missing = [
        name
        for name, found in ((LLVM_OBJCOPY, objcopy), (LLVM_STRIP, strip_tool))
        if not found
    ]
    searched = (
        "\n".join(f"  {path}" for path in _managed_llvm_search_dirs()) or "  (none)"
    )
    raise StagingError(
        f"{target} is not native to this {sys.platform}/{platform.machine()} host, so "
        f"the host GNU binutils cannot read the artifact it just built; no "
        f"cross-capable {' or '.join(missing)} was found.\n"
        f"Install the rustup llvm-tools component, set SOLDR_LLVM_DIR, or pass "
        f"--objcopy/--strip-tool/--darwin-strip explicitly.\n"
        f"Searched PATH plus:\n{searched}"
    )


def run_tool(args: list[str]) -> None:
    """Run one objcopy/strip step, folding output into a StagingError."""
    completed = run_captured(args, capture_output=True, text=True, check=False)
    if completed.returncode != 0:
        raise StagingError(
            f"{' '.join(args)} failed (exit {completed.returncode}):\n"
            f"{completed.stdout}{completed.stderr}"
        )


def strip_elf_binary_with_debuglink(
    binary: Path, debug_dest: Path, *, objcopy: str, strip_tool: str
) -> None:
    """Post-link symbol carve-out for one ELF binary (soldr#3038).

    See the module docstring for why this replaced
    `split-debuginfo = "packed"` on Linux. Three steps:

      1. `objcopy --only-keep-debug binary debug_dest` -- copy every debug
         section out to its own file. Operates on the COMPLETE linked
         binary, so it captures debug info from every source: soldr's own
         crates, the precompiled std, and every `cc`-built C dependency.
      2. `strip --strip-debug binary` -- remove the debug sections from the
         shipped binary in place. This is `--strip-debug`, not
         `--strip-all`: the GNU build-id note this release lane already
         injects (`-C link-arg=-Wl,--build-id`) lives in `.note.gnu.build-id`,
         not a `.debug_*` section, so it survives untouched -- a symbolizer
         can still cross-check binary and `.debug` file by build-id, not
         only by the debuglink CRC below.
      3. `objcopy --add-gnu-debuglink=debug_dest binary` -- embed a
         `.gnu_debuglink` section (the sidecar's basename plus a CRC32 of
         its contents) so a debugger/symbolizer that has both files locates
         one from the other without needing the build-id path at all.
    """
    run_tool([objcopy, "--only-keep-debug", str(binary), str(debug_dest)])
    run_tool([strip_tool, "--strip-debug", str(binary)])
    run_tool([objcopy, f"--add-gnu-debuglink={debug_dest}", str(binary)])


def verify_no_disallowed_dynamic_libs(paths: list[Path]) -> None:
    """Refuse to release a binary that links a non-system shared library.

    The static-liblzma bug (a dynamically-linked liblzma that auditwheel
    vendored into ``soldr.libs/`` with an RPATH soldr's relocated shim images
    cannot resolve) was host-luck: it only reproduced on a machine with a
    system liblzma for pkg-config to find. A release binary must never carry
    that risk regardless of which host built it. Non-ELF/non-Mach-O files in
    ``paths`` (a Windows PDB) are silently skipped by
    :func:`check_linked_libs.check_binary`.
    """
    for path in paths:
        result = check_linked_libs.check_binary(path)
        if result is None:
            continue
        kind, offenders = result.kind, list(result.offenders)
        if offenders:
            raise StagingError(
                f"{path} ({kind}) links disallowed shared librar{'y' if len(offenders) == 1 else 'ies'} "
                f"{offenders}: enable the offending dependency's static/bundled feature "
                '(e.g. xz2\'s "static" feature for liblzma) instead of letting it '
                "link a non-system shared library found on the build host."
            )


def stage_release_binaries(
    target: str, release_dir: Path, package_dir: Path
) -> list[Path]:
    """Populate ``package_dir`` with the executable, daemon, and (Windows) PDB.

    Never stages a macOS ``.dSYM`` and never carries an already-split Linux
    debug sidecar -- see the module docstring and :func:`stage_debug_symbols`,
    which strips the Linux binaries staged here in place as a second pass.
    """
    suffix = binary_suffix(target)
    source = release_dir / f"soldr{suffix}"
    if not source.is_file():
        raise StagingError(
            f"expected soldr{suffix} in target dir; observed release dir:\n"
            f"{release_contents(release_dir)}"
        )

    package_dir.mkdir(parents=True, exist_ok=True)
    staged: list[Path] = []
    soldr = package_dir / source.name
    shutil.copy2(source, soldr)
    mark_executable(soldr)
    staged.append(soldr)

    daemon = package_dir / f"soldr-daemon{suffix}"
    copy_or_link(soldr, daemon)
    mark_executable(daemon)
    staged.append(daemon)

    if target.endswith("-pc-windows-msvc"):
        pdb = first_file(release_dir, ["soldr.pdb", "soldr_cli.pdb"])
        if pdb is None:
            raise StagingError(
                f"expected a soldr PDB sidecar next to soldr{suffix}; observed release dir:\n"
                f"{release_contents(release_dir)}"
            )
        destination = package_dir / pdb.name
        shutil.copy2(pdb, destination)
        staged.append(destination)

    verify_no_disallowed_dynamic_libs(staged)

    print("--- staged release package ---")
    print(release_contents(package_dir))
    return staged


def stage_debug_symbols(  # noqa: C901
    target: str,
    release_dir: Path,
    package_dir: Path,
    symbols_dir: Path,
    *,
    objcopy: str | None = None,
    strip_tool: str | None = None,
    darwin_strip: str | None = None,
) -> list[Path]:
    """Carve debug symbols out of the ALREADY-STAGED binaries in
    ``package_dir`` into their own directory, SEPARATE from ``package_dir``
    itself (soldr#3038).

    This must never write a non-pdb sidecar into ``package_dir``:
    ``release_manifest.py`` reads that tree for ``soldr.debug_info``, and
    every ``setup-soldr`` consumer's
    ``zccache_contract.py::validate_release_manifest`` hard-rejects any entry
    there whose ``format`` is not ``"pdb"``. Instead this directory becomes
    its own opt-in ``-symbols.tar.zst`` release asset that ``setup-soldr``
    never downloads or parses.

    Requires :func:`stage_release_binaries` to have already populated
    ``package_dir`` -- the Linux path strips those staged binaries in place.

    Returns an empty list -- not an error -- when there is nothing to stage
    for this target (Windows always; a Unix build whose macOS dSYM never
    materialized).

    ``objcopy`` / ``strip_tool`` / ``darwin_strip`` default to ``None``,
    meaning "ask :func:`select_binutils` for tools that can read what this
    lane built" -- host GNU binutils for a host-native target, LLVM (or the
    managed cross toolchain's target-prefixed binutils) for a foreign one.
    Pass explicit names to override; nothing is ever skipped silently.
    """
    staged: list[Path] = []
    suffix = binary_suffix(target)
    overrides = {"objcopy": objcopy, "strip": strip_tool, "darwin_strip": darwin_strip}
    resolved: dict[str, str] = {}

    def tool(kind: str) -> str:
        """Resolve one tool: explicit override wins, else auto-select once.

        Lazy on purpose. A target with nothing to carve -- Windows, or a
        darwin build whose dSYM never materialized -- must not fail on
        discovering a tool it is never going to run. `strip -x` on macOS is
        the same tool family as the ELF `--strip-debug` pass, so one
        selection answers both.
        """
        explicit = overrides[kind]
        if explicit is not None:
            return explicit
        if not resolved:
            selected = select_binutils(target)
            selected_objcopy, selected_strip = selected.objcopy, selected.strip
            resolved.update(
                objcopy=selected_objcopy,
                strip=selected_strip,
                darwin_strip=selected_strip,
            )
            print(
                f"selected binutils for {target} (host "
                f"{sys.platform}/{platform.machine()}): "
                f"objcopy={selected_objcopy!r} strip={selected_strip!r}"
            )
        return resolved[kind]

    if "-unknown-linux-" in target:
        binary = package_dir / f"soldr{suffix}"
        if not binary.is_file():
            raise StagingError(
                f"stage_debug_symbols requires soldr{suffix} already staged in "
                f"{package_dir} -- call stage_release_binaries first"
            )
        symbols_dir.mkdir(parents=True, exist_ok=True)
        debug_dest = symbols_dir / "soldr.debug"
        elf_objcopy = tool("objcopy")
        elf_strip = tool("strip")
        strip_elf_binary_with_debuglink(
            binary, debug_dest, objcopy=elf_objcopy, strip_tool=elf_strip
        )
        staged.append(debug_dest)
        print(
            f"carved debug info out of {binary.name} into {debug_dest.name} "
            f"(objcopy={elf_objcopy!r}, strip={elf_strip!r})"
        )

        daemon = package_dir / f"soldr-daemon{suffix}"
        if daemon.is_file() and not files_equal(binary, daemon):
            copy_or_link(binary, daemon)
            mark_executable(daemon)
            print(
                f"{daemon.name} was not byte-identical to the stripped {binary.name}; re-derived it"
            )
        elif daemon.is_file():
            print(
                f"{daemon.name} is already byte-identical to {binary.name} "
                "(shared inode reflected the strip)"
            )
    elif target.endswith("-apple-darwin"):
        dsym = first_directory(release_dir, ["soldr.dSYM", "soldr_cli.dSYM"])
        if dsym is not None:
            symbols_dir.mkdir(parents=True, exist_ok=True)
            destination = symbols_dir / dsym.name
            shutil.copytree(dsym, destination)
            staged.append(destination)
            print(f"staged macOS dSYM bundle into separate symbols asset: {dsym.name}")

            # dsymutil (run during the build by `split-debuginfo = "packed"`,
            # injected for darwin targets only -- see auto-release.yml /
            # native_release_build.py) gathers DWARF from the intermediate
            # `.o` files via N_OSO stabs, not from the linked binary's own
            # embedded debug info, so the dSYM above is already complete.
            # `strip -x` removes the local symbol table entries (those same
            # N_OSO stabs) the shipped binary no longer needs them for.
            binary = package_dir / "soldr"
            daemon = package_dir / "soldr-daemon"
            if binary.is_file():
                run_tool([tool("darwin_strip"), "-x", str(binary)])
                if daemon.is_file() and not files_equal(binary, daemon):
                    copy_or_link(binary, daemon)
                    mark_executable(daemon)

    if staged:
        print("--- staged debug-symbols package ---")
        print(release_contents(symbols_dir))
    return staged


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--package-dir", type=Path, default=Path("dist/package"))
    parser.add_argument("--release-dir", type=Path)
    parser.add_argument(
        "--symbols-dir",
        type=Path,
        default=None,
        help=(
            "Optional. When given, also carve debug symbols out of the "
            "staged Linux binaries (or stage the macOS .dSYM) here -- NEVER "
            "into --package-dir. See stage_debug_symbols()."
        ),
    )
    parser.add_argument(
        "--objcopy",
        default=None,
        help=(
            "objcopy executable for the Linux symbol carve-out. Default: "
            "auto-selected per target -- host GNU objcopy when the target is "
            "native to this host, llvm-objcopy (or the managed cross "
            "toolchain's target-prefixed objcopy) when it is not."
        ),
    )
    parser.add_argument(
        "--strip-tool",
        default=None,
        help="strip executable for the Linux symbol carve-out. Default: auto-selected, as --objcopy.",
    )
    parser.add_argument(
        "--darwin-strip",
        default=None,
        help=(
            "strip executable for the macOS post-dsymutil `strip -x` pass. "
            "Default: auto-selected -- llvm-strip, since GNU strip cannot read "
            "Mach-O at all and these lanes now cross-build on Linux."
        ),
    )
    args = parser.parse_args(argv)
    release_dir = args.release_dir or Path("target") / args.target / "release"
    try:
        stage_release_binaries(args.target, release_dir, args.package_dir)
        if args.symbols_dir is not None:
            stage_debug_symbols(
                args.target,
                release_dir,
                args.package_dir,
                args.symbols_dir,
                objcopy=args.objcopy,
                strip_tool=args.strip_tool,
                darwin_strip=args.darwin_strip,
            )
    except (OSError, StagingError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
