"""Stage auxiliary Cargo ``[[bin]]`` targets into a soldr-built wheel.

soldr#3239. maturin builds one artifact kind per wheel: a PyO3 extension *or*
a ``bindings = "bin"`` executable. A project that ships both used to wrap
maturin in a bespoke ``delegate-backend`` whose only job was to build the bin
and copy it into maturin's data directory. ``bundle-bins`` moves that job into
the native backend::

    [tool.soldr.pep517]
    bundle-bins = [
      { bin = "bosn", package = "bosn" },                # -> <dist>.data/scripts/
      { bin = "helper", dest = "platlib/bosn/_bin" },    # -> site-packages/bosn/_bin/
    ]

After maturin writes the extension wheel, each entry is built through
``soldr build`` with the same prepared environment, target, and profile, and
the executable is added to the wheel with a regenerated ``RECORD``.

``dest`` is ``<scheme>[/<subdir>]`` using the wheel install schemes that
maturin's ``data`` directory also uses: ``scripts`` (the default, installed
onto ``PATH``), ``platlib``, ``purelib``, ``data``, and ``headers``.

This module has no dependency on the backend in ``__init__.py`` so it can be
unit-tested in isolation; the backend supplies the command environment.

It is also the one implementation behind ``soldr wheel`` (zackees/soldr#3468):
the native verb embeds this file and, after maturin writes the wheel, runs
``python _bundle_bins.py stage --wheel <whl> --pyproject <file> --soldr <exe>
[--manifest-path <p>] [--target <triple>] [--profile-arg=<arg>]...``, so the
wheel verb and the PEP 517 backend cannot drift apart.
"""

import argparse
import base64
import csv
import hashlib
import importlib
import io
import json
import os
import re
import stat
import struct
import sys
import tarfile
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional, Sequence

# The native wheel verb extracts this module and its capture helper together.
sys.path.insert(0, str(Path(__file__).resolve().parent))
if __package__ == "soldr":
    from ._process import run_captured
else:
    # The native CLI extracts these two standalone modules together.
    run_captured = importlib.import_module("_process").run_captured

BUNDLE_BINS_KEY = "bundle-bins"
DEFAULT_DEST = "scripts"
WHEEL_SCHEMES = ("scripts", "platlib", "purelib", "data", "headers")
_SECTION = "tool.soldr.pep517"
_ENTRY_KEYS = {"bin", "package", "dest", "features", "linux-libc", "rename"}
# `linux-libc = "musl"`: on a Linux wheel, build the bin for the same
# architecture's `-unknown-linux-musl` triple and require a static executable,
# so the CLI runs on any Linux while the PyO3 extension keeps the
# interpreter's glibc. Darwin and Windows wheels are unaffected.
LINUX_LIBCS = ("musl",)
_FEATURE_NAME = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_+./-]*")
_BIN_NAME = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.-]*")


class BundleBinsError(RuntimeError):
    """A ``bundle-bins`` configuration or staging failure."""


@dataclass(frozen=True)
class BundleBin:
    """One Cargo bin target to stage into the wheel."""

    bin: str
    package: Optional[str] = None
    dest: str = DEFAULT_DEST
    features: "tuple[str, ...]" = ()
    linux_libc: Optional[str] = None
    rename: Optional[str] = None

    def staged_name(self, executable: Path) -> str:
        """The file name inside the wheel: ``rename`` plus the executable's
        own suffix (``.exe`` on Windows), else Cargo's file name."""
        if not self.rename:
            return executable.name
        return self.rename + (".exe" if executable.suffix.lower() == ".exe" else "")


def read_bundle_bins(pyproject: Path) -> "list[BundleBin]":
    """Return the validated ``bundle-bins`` entries, or ``[]`` when absent."""
    try:
        text = pyproject.read_text(encoding="utf-8")
    except OSError:
        return []
    return [_parse_entry(index, raw) for index, raw in enumerate(_raw_entries(text))]


def _raw_entries(text: str) -> "list[Any]":
    try:
        # pylint: disable=import-outside-toplevel  # 3.10 has no tomllib
        import tomllib  # type: ignore[import-not-found]
    except ImportError:
        return _fallback_entries(text)
    try:
        value: Any = tomllib.loads(text)
    except ValueError:
        # maturin reports the authoritative TOML error during the build.
        return []
    for component in (*_SECTION.split("."), BUNDLE_BINS_KEY):
        if not isinstance(value, dict):
            return []
        value = value.get(component)
    if value is None:
        return []
    if not isinstance(value, list):
        raise BundleBinsError(
            f"[{_SECTION}] {BUNDLE_BINS_KEY} must be an array of tables"
        )
    return value


def _strip_comment(line: str) -> str:
    quote = None
    for index, char in enumerate(line):
        if quote:
            if char == quote:
                quote = None
        elif char in "\"'":
            quote = char
        elif char == "#":
            return line[:index]
    return line


def _bracket_depth(text: str) -> int:
    depth = 0
    quote = None
    for char in text:
        if quote:
            if char == quote:
                quote = None
        elif char in "\"'":
            quote = char
        elif char == "[":
            depth += 1
        elif char == "]":
            depth -= 1
    return depth


def _fallback_entries(text: str) -> "list[Any]":  # noqa: C901
    """Parse the one supported ``bundle-bins`` shape without tomllib.

    Python 3.10 has no tomllib and the backend must stay dependency-free, so
    this accepts exactly the documented form: an array of inline tables whose
    values are strings, optionally spread over several lines.
    """
    active = False
    collected: "list[str] | None" = None
    for line in text.splitlines():
        stripped = _strip_comment(line).strip()
        if collected is not None:
            collected.append(stripped)
            if _bracket_depth(" ".join(collected)) <= 0:
                break
            continue
        if stripped.startswith("["):
            active = re.sub(r"\s+", "", stripped) == f"[{_SECTION}]"
            continue
        key, separator, rest = stripped.partition("=")
        if active and separator and key.strip() == BUNDLE_BINS_KEY:
            collected = [rest.strip()]
            if _bracket_depth(rest) <= 0:
                break
    if collected is None:
        return []
    body = " ".join(collected).strip()
    if not (body.startswith("[") and body.endswith("]")):
        raise BundleBinsError(
            f"[{_SECTION}] {BUNDLE_BINS_KEY} must be an array of tables"
        )
    entries: "list[Any]" = []
    for table in re.findall(r"\{([^{}]*)\}", body):
        entry: "dict[str, Any]" = {}
        for match in re.finditer(
            r"([A-Za-z0-9_-]+)\s*=\s*(?:\"([^\"]*)\"|'([^']*)'|\[([^\]]*)\])", table
        ):
            if match.group(4) is not None:
                entry[match.group(1)] = [
                    a if a is not None else b
                    for a, b in re.findall(r"\"([^\"]*)\"|'([^']*)'", match.group(4))
                ]
                continue
            value = match.group(2) if match.group(2) is not None else match.group(3)
            entry[match.group(1)] = value
        entries.append(entry)
    return entries


def _parse_entry(index: int, raw: Any) -> BundleBin:
    where = f"[{_SECTION}] {BUNDLE_BINS_KEY}[{index}]"
    if not isinstance(raw, dict):
        raise BundleBinsError(f'{where} must be a table such as {{ bin = "name" }}')
    unknown = sorted(set(raw) - _ENTRY_KEYS)
    if unknown:
        raise BundleBinsError(
            f"{where} has unknown key(s) {', '.join(unknown)}; "
            f"supported keys are {', '.join(sorted(_ENTRY_KEYS))}"
        )
    name = raw.get("bin")
    if not isinstance(name, str) or not _BIN_NAME.fullmatch(name):
        raise BundleBinsError(f"{where} needs `bin`, the Cargo [[bin]] target name")
    package = raw.get("package")
    if package is not None and (not isinstance(package, str) or not package.strip()):
        raise BundleBinsError(f"{where} `package` must be a non-empty string")
    dest = raw.get("dest", DEFAULT_DEST)
    if not isinstance(dest, str):
        raise BundleBinsError(f"{where} `dest` must be a string")
    features = raw.get("features", [])
    if not isinstance(features, list) or not all(
        isinstance(feature, str) and _FEATURE_NAME.fullmatch(feature)
        for feature in features
    ):
        raise BundleBinsError(
            f"{where} `features` must be an array of Cargo feature names"
        )
    linux_libc = raw.get("linux-libc")
    if linux_libc is not None and linux_libc not in LINUX_LIBCS:
        raise BundleBinsError(
            f"{where} `linux-libc` must be one of {', '.join(LINUX_LIBCS)}; got {linux_libc!r}"
        )
    rename = raw.get("rename")
    if rename is not None and (
        not isinstance(rename, str) or not _BIN_NAME.fullmatch(rename)
    ):
        raise BundleBinsError(f"{where} `rename` must be a plain executable name")
    return BundleBin(
        bin=name,
        rename=rename,
        package=package.strip() if package else None,
        dest=_normalize_dest(where, dest),
        features=tuple(features),
        linux_libc=linux_libc,
    )


def _normalize_dest(where: str, dest: str) -> str:
    parts = dest.strip().split("/")
    if (
        "\\" in dest
        or any(part in ("", ".", "..") for part in parts)
        or parts[0] not in WHEEL_SCHEMES
    ):
        raise BundleBinsError(
            f"{where} `dest` must be <scheme>[/<subdir>] with scheme one of "
            f"{', '.join(WHEEL_SCHEMES)} and no empty, `.`, or `..` components; "
            f"got {dest!r}"
        )
    return "/".join(parts)


def cargo_build_command(
    entry: BundleBin,
    *,
    manifest_path: Optional[Path],
    profile_args: Sequence[str],
    target_args: Sequence[str],
    soldr: str = "soldr",
) -> "list[str]":
    """Build one bin through soldr's blessed build surface."""
    command = [
        soldr,
        "build",
        "--bin",
        entry.bin,
        "--message-format=json-render-diagnostics",
    ]
    if entry.package:
        command += ["--package", entry.package]
    if entry.features:
        command += ["--features", ",".join(entry.features)]
    if manifest_path is not None:
        command += ["--manifest-path", str(manifest_path)]
    return [*command, *profile_args, *entry_target_args(entry, target_args)]


def _host_linux_triple() -> Optional[str]:
    if not sys.platform.startswith("linux"):
        return None
    machine = os.uname().machine
    machine = {"arm64": "aarch64", "amd64": "x86_64"}.get(machine, machine)
    return f"{machine}-unknown-linux-gnu"


def entry_target_args(entry: BundleBin, target_args: Sequence[str]) -> "list[str]":
    """The ``--target`` for one bin: the wheel's, retargeted to musl for a
    ``linux-libc = "musl"`` entry on a Linux wheel."""
    if entry.linux_libc != "musl":
        return list(target_args)
    wheel_target = (
        target_args[1]
        if len(target_args) == 2 and target_args[0] == "--target"
        else _host_linux_triple()
        if not target_args
        else None
    )
    if wheel_target is None:
        return list(target_args)
    for gnu in ("-linux-gnu", "-linux-musl"):
        if wheel_target.endswith(gnu):
            return ["--target", wheel_target[: -len(gnu)] + "-linux-musl"]
    # A Darwin or Windows wheel: the bin keeps the wheel's own target.
    return list(target_args)


_ELF_PT_DYNAMIC = 2
_ELF_PT_INTERP = 3
_ELF_DT_NEEDED = 1


def assert_static_elf(entry: BundleBin, executable: Path) -> None:
    """Refuse a ``linux-libc = "musl"`` bin that needs an interpreter or a
    shared library. static-pie keeps PT_DYNAMIC for its own relocations, so
    the test is the absence of PT_INTERP and of every DT_NEEDED entry."""
    data = executable.read_bytes()
    if len(data) < 64 or data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise BundleBinsError(
            f"bundled bin `{entry.bin}` is not a little-endian ELF64 executable"
        )
    (phoff,) = struct.unpack_from("<Q", data, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    for index in range(phnum):
        kind, _flags, offset, _vaddr, _paddr, size = struct.unpack_from(
            "<IIQQQQ", data, phoff + index * phentsize
        )
        if kind == _ELF_PT_INTERP:
            raise BundleBinsError(
                f"bundled bin `{entry.bin}` (linux-libc = musl) is dynamically linked"
            )
        if kind == _ELF_PT_DYNAMIC:
            for cursor in range(offset, offset + size - 15, 16):
                tag, _value = struct.unpack_from("<qQ", data, cursor)
                if tag == 0:
                    break
                if tag == _ELF_DT_NEEDED:
                    raise BundleBinsError(
                        f"bundled bin `{entry.bin}` (linux-libc = musl) needs a shared library"
                    )


def executable_from_messages(stdout: str, bin_name: str) -> Path:
    """Find the bin's executable in Cargo's JSON message stream."""
    found: Optional[Path] = None
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if (
            not isinstance(message, dict)
            or message.get("reason") != "compiler-artifact"
        ):
            continue
        target = message.get("target")
        if not isinstance(target, dict) or target.get("name") != bin_name:
            continue
        if "bin" not in (target.get("kind") or []):
            continue
        executable = message.get("executable")
        if isinstance(executable, str) and executable:
            found = Path(executable)
    if found is None:
        raise BundleBinsError(
            f"cargo reported no executable for bundled bin `{bin_name}`"
        )
    return found


def build_bundle_bin(
    entry: BundleBin,
    command: Sequence[str],
    env: "dict[str, str]",
    run: Callable[..., Any] = run_captured,
) -> Path:
    """Run ``command`` and return the built executable for ``entry``."""
    result = run(
        list(command),
        env=env,
        capture_stdout=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    stdout = result.stdout or ""
    # Cargo's rendered diagnostics and progress already go to stderr; relay
    # any non-JSON stdout too so the frontend shows the whole build.
    for line in stdout.splitlines():
        if line.strip() and not line.lstrip().startswith("{"):
            print(line, file=sys.stderr)
    if result.returncode != 0:
        raise BundleBinsError(
            f"`{' '.join(command)}` exited with {result.returncode} while building "
            f"bundled bin `{entry.bin}`"
        )
    executable = executable_from_messages(stdout, entry.bin)
    if not executable.is_file():
        raise BundleBinsError(
            f"bundled bin `{entry.bin}` was reported at {executable}, which is not a file"
        )
    if entry.linux_libc == "musl" and "-linux-musl" in " ".join(command):
        assert_static_elf(entry, executable)
    return executable


def wheel_arcname(prefix: str, dest: str, filename: str, root_is_purelib: bool) -> str:
    """Map an install scheme destination to its path inside the wheel."""
    scheme, _, subdir = dest.partition("/")
    parts = [part for part in (subdir, filename) if part]
    if (scheme == "platlib" and not root_is_purelib) or (
        scheme == "purelib" and root_is_purelib
    ):
        return "/".join(parts)
    return "/".join([f"{prefix}.data", scheme, *parts])


def _record_row(name: str, data: bytes) -> "list[str]":
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return [name, f"sha256={digest.decode('ascii')}", str(len(data))]


def _dist_info_directory(names: "Sequence[str]") -> str:
    candidates = sorted(
        {
            name.split("/", 1)[0]
            for name in names
            if name.count("/") == 1
            and name.split("/", 1)[0].endswith(".dist-info")
            and name.endswith("/RECORD")
        }
    )
    if len(candidates) != 1:
        raise BundleBinsError(
            f"expected exactly one top-level *.dist-info/RECORD in the wheel, found {candidates}"
        )
    return candidates[0]


def _root_is_purelib(wheel_metadata: str) -> bool:
    for line in wheel_metadata.splitlines():
        key, separator, value = line.partition(":")
        if separator and key.strip().lower() == "root-is-purelib":
            return value.strip().lower() == "true"
    return False


def add_files_to_wheel(
    wheel: Path, files: "Sequence[tuple[BundleBin, Path]]"
) -> "list[str]":
    """Add executables to ``wheel`` in place and regenerate its ``RECORD``.

    Returns the wheel paths that were added.
    """
    with zipfile.ZipFile(wheel) as source:
        infos = source.infolist()
        names = [info.filename for info in infos]
        dist_info = _dist_info_directory(names)
        record_name = f"{dist_info}/RECORD"
        record_info = source.getinfo(record_name)
        root_is_purelib = _root_is_purelib(
            source.read(f"{dist_info}/WHEEL").decode("utf-8", errors="replace")
        )
        prefix = dist_info[: -len(".dist-info")]
        additions: "list[tuple[str, Path]]" = []
        taken = set(names)
        for entry, executable in files:
            arcname = wheel_arcname(
                prefix, entry.dest, entry.staged_name(executable), root_is_purelib
            )
            if arcname in taken:
                raise BundleBinsError(
                    f"bundled bin `{entry.bin}` would overwrite `{arcname}` already in the wheel"
                )
            taken.add(arcname)
            additions.append((arcname, executable))

        temporary = wheel.with_name(f".{wheel.name}.{os.getpid()}.tmp")
        rows: "list[list[str]]" = []
        try:
            with zipfile.ZipFile(
                temporary, "w", compression=zipfile.ZIP_DEFLATED
            ) as out:
                for info in infos:
                    if info.filename == record_name:
                        continue
                    data = source.read(info)
                    out.writestr(info, data)
                    if not info.is_dir():
                        rows.append(_record_row(info.filename, data))
                for arcname, executable in additions:
                    data = executable.read_bytes()
                    # Reuse RECORD's timestamp so the added entries are as
                    # reproducible as the wheel maturin produced.
                    info = zipfile.ZipInfo(arcname, date_time=record_info.date_time)
                    info.external_attr = (stat.S_IFREG | 0o755) << 16
                    info.compress_type = zipfile.ZIP_DEFLATED
                    out.writestr(info, data)
                    rows.append(_record_row(arcname, data))
                rows.append([record_name, "", ""])
                buffer = io.StringIO()
                csv.writer(buffer, lineterminator="\n").writerows(rows)
                out.writestr(record_info, buffer.getvalue())
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    os.replace(temporary, wheel)
    return [arcname for arcname, _ in additions]


def _sdist_top_level(names: "Sequence[str]") -> str:
    tops = {name.split("/", 1)[0] for name in names if "/" in name}
    if len(tops) != 1:
        raise BundleBinsError(
            f"expected exactly one top-level directory in the sdist, found {sorted(tops)}"
        )
    return next(iter(tops))


def _cargo_package_name(text: str) -> Optional[str]:
    try:
        # pylint: disable=import-outside-toplevel  # 3.10 has no tomllib
        import tomllib  # type: ignore[import-not-found]
    except ImportError:
        return None
    try:
        parsed: Any = tomllib.loads(text)
    except ValueError:
        return None
    package = parsed.get("package") if isinstance(parsed, dict) else None
    if isinstance(package, dict):
        name = package.get("name")
        if isinstance(name, str):
            return name
    return None


def _append_workspace_members(text: str, additions: "Sequence[str]") -> str:
    """Insert ``additions`` into a ``[workspace] members = [...]`` array.

    Text-level, not a TOML writer, so the file's existing formatting and
    comments survive untouched apart from the appended entries.
    """
    if not additions:
        return text
    match = re.search(r"members\s*=\s*\[(.*?)\]", text, re.DOTALL)
    if not match:
        raise BundleBinsError(
            "sdist root Cargo.toml has a [workspace] table but no `members` array to patch"
        )
    existing = match.group(1).rstrip()
    if existing and not existing.endswith(","):
        existing += ","
    new_items = "".join(f'\n    "{member}",' for member in additions)
    replacement = f"members = [{existing}{new_items}\n]"
    return text[: match.start()] + replacement + text[match.end() :]


def patch_sdist_workspace_members(  # noqa: C901
    sdist: Path, entries: "Sequence[BundleBin]"
) -> "list[str]":
    """Keep every ``bundle-bins`` package as an sdist workspace member.

    maturin's sdist builder regenerates the root ``Cargo.toml``'s
    ``[workspace] members`` array, trimmed to the Cargo dependency graph
    reachable from ``[tool.maturin] manifest-path``. A ``bundle-bins``
    package with no Cargo dependency edge to the extension crate is dropped
    from that array even though its files are still copied into the sdist
    (soldr#3239 / zackees/soldr#3444), so a later
    ``soldr build --package <it> --manifest-path <the trimmed Cargo.toml>``
    fails with "package ID specification did not match any packages".

    This patches the trimmed array back in place, in the sdist tarball
    itself, right after maturin writes it. Returns the workspace-relative
    directories that were added (empty if nothing needed patching).
    """
    wanted = sorted({entry.package for entry in entries if entry.package})
    if not wanted:
        return []

    with tarfile.open(sdist, "r:gz") as archive:
        members = archive.getmembers()
        names = [member.name for member in members]
        top = _sdist_top_level(names)
        root_cargo_name = f"{top}/Cargo.toml"
        package_dirs: "dict[str, str]" = {}
        root_text: Optional[str] = None
        for member in members:
            if not member.isfile() or not member.name.endswith("/Cargo.toml"):
                continue
            extracted = archive.extractfile(member)
            if extracted is None:
                continue
            text = extracted.read().decode("utf-8", errors="replace")
            if member.name == root_cargo_name:
                root_text = text
                continue
            name = _cargo_package_name(text)
            if name is not None:
                package_dirs[name] = member.name[len(top) + 1 : -len("/Cargo.toml")]

        if root_text is None:
            raise BundleBinsError(f"sdist {sdist} has no root {root_cargo_name}")
        workspace_name = _cargo_package_name(root_text)
        # The root manifest may itself be a package (a virtual manifest has
        # no [package] table); either way, a [workspace] table is what we
        # need to check for.
        del workspace_name
        try:
            # pylint: disable=import-outside-toplevel  # 3.10 has no tomllib
            import tomllib  # type: ignore[import-not-found]

            parsed_root: Any = tomllib.loads(root_text)
        except ImportError:
            parsed_root = None
        except ValueError:
            parsed_root = None
        workspace = (
            parsed_root.get("workspace") if isinstance(parsed_root, dict) else None
        )
        if not isinstance(workspace, dict):
            return []
        existing_members = set(workspace.get("members") or [])
        additions = [
            package_dirs[name]
            for name in wanted
            if name in package_dirs and package_dirs[name] not in existing_members
        ]
        if not additions:
            return []
        patched_root = _append_workspace_members(root_text, additions).encode("utf-8")

    temporary = sdist.with_name(f".{sdist.name}.{os.getpid()}.tmp")
    try:
        with (
            tarfile.open(sdist, "r:gz") as source,
            tarfile.open(temporary, "w:gz") as out,
        ):
            for member in source.getmembers():
                if member.name == root_cargo_name and member.isfile():
                    info = tarfile.TarInfo(member.name)
                    info.size = len(patched_root)
                    info.mode = member.mode
                    info.mtime = member.mtime
                    info.uid = member.uid
                    info.gid = member.gid
                    info.uname = member.uname
                    info.gname = member.gname
                    out.addfile(info, io.BytesIO(patched_root))
                elif member.isfile():
                    data = source.extractfile(member)
                    out.addfile(member, data)
                else:
                    out.addfile(member)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    os.replace(temporary, sdist)
    return additions


def bundle_into_wheel(
    wheel: Path,
    entries: "Sequence[BundleBin]",
    build: Callable[[BundleBin], Path],
) -> "list[str]":
    """Build every entry, then stage all of them into ``wheel`` at once."""
    built = [(entry, build(entry)) for entry in entries]
    return add_files_to_wheel(wheel, built)


def _parse_stage_args(argv: "Sequence[str]") -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="_bundle_bins.py",
        description="Stage [tool.soldr.pep517] bundle-bins into a built wheel.",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    stage = commands.add_parser("stage", help="build the bins and add them")
    stage.add_argument("--wheel", required=True, type=Path)
    stage.add_argument("--pyproject", required=True, type=Path)
    stage.add_argument("--soldr", default="soldr")
    stage.add_argument("--manifest-path", type=Path, default=None)
    stage.add_argument("--target", default=None)
    stage.add_argument("--profile-arg", action="append", default=[])
    return parser.parse_args(list(argv))


def main(argv: "Optional[Sequence[str]]" = None) -> int:
    """``soldr wheel``'s entry point (zackees/soldr#3468)."""
    args = _parse_stage_args(sys.argv[1:] if argv is None else argv)
    try:
        entries = read_bundle_bins(args.pyproject)
        if not entries:
            return 0
        target_args = ["--target", args.target] if args.target else []
        env = dict(os.environ)

        def build(entry: BundleBin) -> Path:
            command = cargo_build_command(
                entry,
                manifest_path=args.manifest_path,
                profile_args=args.profile_arg,
                target_args=target_args,
                soldr=args.soldr,
            )
            return build_bundle_bin(entry, command, env, run=run_captured)

        added = bundle_into_wheel(args.wheel, entries, build)
    except BundleBinsError as error:
        print(f"soldr wheel: bundle-bins: {error}", file=sys.stderr)
        return 1
    print(f"soldr wheel: bundled {', '.join(added)}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
