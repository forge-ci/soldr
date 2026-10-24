#!/usr/bin/env python3
"""Native smoke for a release archive + wheel pair (soldr#2294).

Release binaries are cross-built on Linux; EXECUTION still needs the
target's native OS. The `smoke_macos_x64`, `smoke_macos_arm64`, and
`smoke_windows` jobs in
`auto-release.yml` download the lane's artifacts into `dist/` and run
this script. It:

1. checks the wheel's declared version (soldr#1202);
2. extracts the `.tar.zst` archive (host-side, via `tar` -- soldr ships as a
   maturin *binary* wheel (`crates/soldr-cli` has no pyo3 library, so
   `[tool.maturin] module-name` builds no extension module): the wheel
   carries the pure-Python `soldr` package plus the native `soldr` script,
   which must be installed on its own platform to run; the *archive*, by
   contrast, is a plain compressed tarball and needs nothing but `tar` to
   open);
3. asserts the required bundle members are present (including the
   required `.pdb` sidecar on Windows, docs/DEBUG_SIDECARS.md);
4. on macOS, asserts the Mach-O architecture matches the target;
5. executes every bundled binary from the archive (soldr#1140 stub guard,
   soldr#1202 dispatch-arm guard, then a smoke pass over the rest).

Runnable locally: `python3 ci/smoke_release_artifacts.py
--target aarch64-pc-windows-msvc --expected-version v0.8.40 --dist dist`.

The release workflow now executes both macOS targets on hosted native
runners. The older `zackees/docker-mac-x64` Recovery path remains available
through the guest-script flags for diagnostic callers.

That means step 5 (execution) for `x86_64-apple-darwin` splits into two
script invocations instead of running in-process:

- `--emit-guest-script <path> --share-dir <dir>` does everything that stays
  Linux-side (wheel version, archive extraction, required members, the
  Mach-O magic-byte check) and then, instead of executing anything itself,
  copies the binaries it wants run into `<dir>` and writes a
  bash-3.2-compatible script to `<path>` that fetches each one from
  `http://10.0.2.2:8000/<name>` (the guest's slirp view of the driving
  container -- see the action's README) and performs the same checks this
  script runs natively elsewhere, then leaves a flat `key=value` results file
  under `/tmp/results/` for `--collect` to tar back out.
- `--verify-collected <dir>` reads that results file back out of the action's
  `collect` tarball (extracted to `<dir>` by the workflow) plus the action's
  own `exit-code` output, and fails with a named diagnostic naming exactly
  which guest-side check did not pass.

The Recovery image itself ships no Python.  For the x64 release gate this
script therefore stages a pinned, SHA-256-verified portable CPython beside
the exact uploaded wheel.  The guest installs that wheel without an index,
imports its abi3 native module, and exercises both CLI version surfaces.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

MIN_SOLDR_BYTES = 2 * 1024 * 1024  # soldr#1140 stub floor

# soldr#3076: `lipo`/`file` need Xcode CLT, which neither a bare Linux host
# nor the Recovery guest has. A 64-bit little-endian Mach-O starts with the
# MH_MAGIC_64 magic number, then a mach/machine.h `cputype` (also
# little-endian here since the magic identifies this as the native-endian
# form): CPU_TYPE_X86_64 = 0x01000007, CPU_TYPE_ARM64 = 0x0100000C.
MACHO_MAGIC_64 = b"\xcf\xfa\xed\xfe"
_MACHO_CPUTYPE = {
    "x86_64": b"\x07\x00\x00\x01",
    "arm64": b"\x0c\x00\x00\x01",
}


# soldr#3076: the binaries copied into --share-dir and executed inside the
# Recovery guest for x86_64-apple-darwin, plus the flag run on each one.
# `soldr` itself (the only binary whose output format soldr controls) gets
# the strict check: stdout must start with "soldr " and `version --json`
# must report the expected version, matching `check_version_output` above.
# Every other binary -- soldr does not control cargo-chef's or crgx's
# `--version` text -- only needs to exit 0, matching what the native `run()`
# calls below check for them; "help"/"version" here select which flag to
# pass, not a stricter check.
@dataclass(frozen=True)
class GuestBinaryCheck:
    """A guest binary and which flag (`version` / `help`) is probed."""

    name: str
    check: str


GUEST_BINARY_CHECKS: tuple[GuestBinaryCheck, ...] = (
    GuestBinaryCheck("soldr", "version"),
    GuestBinaryCheck("soldr-daemon", "help"),
    GuestBinaryCheck("crgx", "version"),
    GuestBinaryCheck("cargo-chef", "version"),
)

RESULTS_FILE = "summary.txt"
GUEST_HTTP_BASE = "http://10.0.2.2:8000"
MACOS_X64_PYTHON_ARCHIVE = "cpython-macos-x64.tar.gz"
MACOS_X64_PYTHON_URL = (
    "https://github.com/astral-sh/python-build-standalone/releases/download/"
    "20260901/cpython-3.13.15%2B20260901-x86_64-apple-darwin-"
    "install_only_stripped.tar.gz"
)
MACOS_X64_PYTHON_SHA256 = (
    "f712a9143c8a5d248438ec7921a0b48d548bca4f1337d33c690d28c2d0504137"
)


def exe_suffix(target: str) -> str:
    return ".exe" if target.endswith("-pc-windows-msvc") else ""


def required_members(target: str) -> list[str]:
    """Bundle members whose absence fails the smoke."""
    suffix = exe_suffix(target)
    return [
        f"soldr{suffix}",
        f"soldr-daemon{suffix}",
        f"crgx{suffix}",
        f"cargo-chef{suffix}",
        "manifest.json",
    ]


def macho_arch(target: str) -> str | None:
    """Expected Mach-O `cputype` label for darwin targets, else None."""
    if target == "aarch64-apple-darwin":
        return "arm64"
    if target == "x86_64-apple-darwin":
        return "x86_64"
    return None


def check_macho_architecture(binary: Path, expected_arch: str) -> None:
    """Read the Mach-O header directly rather than shelling out to `lipo`."""
    header = binary.read_bytes()[:16]
    if header[:4] != MACHO_MAGIC_64:
        sys.exit(f"ERROR: {binary}: not a 64-bit Mach-O binary (magic={header[:4]!r})")
    expected_bytes = _MACHO_CPUTYPE.get(expected_arch)
    if expected_bytes is None:
        sys.exit(f"ERROR: no known Mach-O cputype for {expected_arch!r}")
    cputype = header[4:8]
    if cputype != expected_bytes:
        sys.exit(
            f"ERROR: {binary}: expected Mach-O cputype for {expected_arch}, got {cputype!r}"
        )
    print(f"Mach-O architecture OK: {binary} is {expected_arch}")


def wheel_version(wheel: Path) -> str:
    """Read the `Version:` field from the wheel's own METADATA, unzipped.

    This preliminary check needs nothing about the host or the wheel's target
    platform.  The x64 Recovery path additionally installs and imports the
    wheel with the staged portable CPython; this metadata read is not treated
    as runtime evidence.
    """
    with zipfile.ZipFile(wheel) as archive:
        metadata_names = [
            name for name in archive.namelist() if name.endswith(".dist-info/METADATA")
        ]
        if len(metadata_names) != 1:
            raise RuntimeError(
                f"expected exactly one *.dist-info/METADATA in {wheel}, found {metadata_names}"
            )
        metadata = archive.read(metadata_names[0]).decode("utf-8", errors="replace")
    match = re.search(r"^Version:\s*(\S+)\s*$", metadata, re.MULTILINE)
    if not match:
        raise RuntimeError(f"no Version: field in {wheel}'s METADATA")
    return match.group(1)


def extract_archive(archive: Path, dest: Path) -> None:
    """Unpack the release `.tar.zst` with `tar`, not the packaged soldr.

    Modern GNU tar and bsdtar (both ship on the ubuntu-24.04 and
    windows-2025 hosted runners) auto-detect zstd compression from the file
    itself, so no `--zstd` flag or external `unzstd` pipe is required.
    """
    dest.mkdir(parents=True, exist_ok=True)
    subprocess.run(["tar", "-xf", str(archive), "-C", str(dest)], check=True)


def run(cmd: list[str | Path]) -> subprocess.CompletedProcess[str]:
    argv = [str(part) for part in cmd]
    print(f"+ {' '.join(argv)}", flush=True)
    return run_captured(argv, check=True, capture_output=True, text=True)


# The module a shipped wheel must import. The wheel is a maturin binary
# wheel with no extension module, so this is the pure-Python package; the
# native coverage is running the installed `soldr` script below.
WHEEL_IMPORT_MODULE = "soldr"


def check_native_wheel_import(wheel: Path, expected: str) -> None:
    """Install the shipped wheel offline in an isolated pinned-Python venv."""
    with tempfile.TemporaryDirectory(prefix="soldr-release-wheel-") as root:
        env_dir = Path(root) / "venv"
        run(["uv", "venv", "--python", sys.executable, env_dir])
        python = env_dir / "bin" / "python"
        run(
            [
                "uv",
                "pip",
                "install",
                "--python",
                python,
                "--no-index",
                "--no-deps",
                wheel,
            ]
        )
        imported = run(
            [
                python,
                "-c",
                f"import {WHEEL_IMPORT_MODULE}; print({WHEEL_IMPORT_MODULE}.__file__)",
            ]
        )
        if not imported.stdout.strip():
            sys.exit(
                f"ERROR: installed release wheel did not expose {WHEEL_IMPORT_MODULE}"
            )
        check_version_output(env_dir / "bin" / "soldr", expected, "installed wheel")


def check_version_output(binary: Path, expected: str, label: str) -> None:
    out = run([binary, "--version"]).stdout.strip()
    print(f"{label} — soldr --version: {out}")
    if not out.startswith("soldr "):
        sys.exit(
            f"ERROR: {label}: 'soldr --version' output {out!r} does not start "
            "with 'soldr ' — likely a stub binary (soldr#1140)."
        )
    json_out = run([binary, "version", "--json"]).stdout.strip()
    if not json_out:
        sys.exit(
            f"ERROR: {label}: 'soldr version --json' produced empty stdout (soldr#1202)."
        )
    print(f"{label} — soldr version --json: {json_out}")
    reported = json.loads(json_out).get("soldr_version")
    if reported != expected:
        sys.exit(
            f"ERROR: {label}: soldr_version {reported!r} != expected {expected!r} (soldr#1202)."
        )


# ---------------------------------------------------------------------------
# soldr#3076: Recovery guest-script generation and result verification.
# ---------------------------------------------------------------------------


def copy_into_share_dir(extract: Path, share_dir: Path, suffix: str) -> None:
    """Copy every binary `GUEST_BINARY_CHECKS` names into `share_dir`.

    The guest fetches by basename over HTTP, so the destination name always
    drops the host suffix (there is none on darwin) -- copying rather than
    symlinking because the action's composite step chmods the whole
    `share-dir` tree world-readable, which a symlink target outside it would
    not get.
    """
    share_dir.mkdir(parents=True, exist_ok=True)
    for binary in GUEST_BINARY_CHECKS:
        name = binary.name
        src = extract / f"{name}{suffix}"
        if not src.is_file():
            sys.exit(f"ERROR: cannot stage {src} into the guest share-dir: missing")
        shutil.copy2(src, share_dir / name)


def download_verified(url: str, destination: Path, expected_sha256: str) -> None:
    """Download one immutable guest input and reject any byte drift."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256()
    with (
        urllib.request.urlopen(url, timeout=120) as response,
        destination.open("wb") as output,
    ):
        while chunk := response.read(1024 * 1024):
            output.write(chunk)
            digest.update(chunk)
    actual = digest.hexdigest()
    if actual != expected_sha256:
        destination.unlink(missing_ok=True)
        sys.exit(
            "ERROR: portable macOS Python SHA-256 mismatch: "
            f"expected {expected_sha256}, got {actual}"
        )


def stage_macos_wheel_runtime(wheel: Path, share_dir: Path) -> str:
    """Stage pinned CPython and the exact uploaded wheel for guest execution."""
    if not re.fullmatch(r"[A-Za-z0-9_.+-]+\.whl", wheel.name):
        sys.exit(f"ERROR: unsafe wheel basename for Recovery guest: {wheel.name!r}")
    share_dir.mkdir(parents=True, exist_ok=True)
    download_verified(
        MACOS_X64_PYTHON_URL,
        share_dir / MACOS_X64_PYTHON_ARCHIVE,
        MACOS_X64_PYTHON_SHA256,
    )
    shutil.copy2(wheel, share_dir / wheel.name)
    return wheel.name


def build_release_guest_script(expected_version: str, wheel_name: str) -> str:
    """The bash-3.2 script the Recovery guest runs (soldr#3076).

    Fetches every `GUEST_BINARY_CHECKS` binary from the driving container's
    HTTP server, runs the same checks `check_version_output`/`run` perform
    natively for every other target, and leaves one `key=value` line per
    check in `/tmp/results/summary.txt` -- the file `--collect` tars back
    out to the host. Never raises on a failed check: every check runs and is
    recorded, and the script's own exit code (0 only if every check passed)
    is the second, coarser signal `--verify-collected` checks against the
    action's `exit-code` output.
    """
    lines = [
        "#!/bin/sh",
        "set +e",
        "mkdir -p /tmp/results",
        f"SUMMARY=/tmp/results/{RESULTS_FILE}",
        ': > "$SUMMARY"',
        "FAIL=0",
        "",
        "ARCH=$(uname -m)",
        'if [ "$ARCH" = "x86_64" ]; then',
        '  echo "arch=pass:$ARCH" >> "$SUMMARY"',
        "else",
        '  echo "arch=fail:unexpected uname -m $ARCH" >> "$SUMMARY"',
        "  FAIL=1",
        "fi",
        "",
    ]
    for binary in GUEST_BINARY_CHECKS:
        name, check = binary.name, binary.check
        dest = f"/tmp/{name}"
        lines += [
            f"curl -fsS -o {dest} {GUEST_HTTP_BASE}/{name}",
            "if [ $? -ne 0 ]; then",
            f'  echo "fetch_{name}=fail:curl could not reach {GUEST_HTTP_BASE}/{name}" >> "$SUMMARY"',
            "  FAIL=1",
            "else",
            f"  chmod +x {dest}",
            f'  echo "fetch_{name}=pass" >> "$SUMMARY"',
            "fi",
            "",
        ]
        if name == "soldr":
            # `soldr` is the one binary whose `--version` output format soldr
            # itself controls, so it gets the strict "starts with 'soldr '"
            # + JSON version-match check `check_version_output` runs
            # natively for every other target.
            lines += [
                f"VOUT=$({dest} --version 2>&1)",
                'case "$VOUT" in',
                '  "soldr "*)',
                f'    echo "{name}_version=pass:$VOUT" >> "$SUMMARY" ;;',
                "  *)",
                f'    echo "{name}_version=fail:$VOUT" >> "$SUMMARY"',
                "    FAIL=1 ;;",
                "esac",
                "",
                "JOUT=$(/tmp/soldr version --json 2>&1)",
                'case "$JOUT" in',
                f'  *\'"soldr_version": "{expected_version}"\'*)',
                '    echo "soldr_version_json=pass:$JOUT" >> "$SUMMARY" ;;',
                "  *)",
                '    echo "soldr_version_json=fail:$JOUT" >> "$SUMMARY"',
                "    FAIL=1 ;;",
                "esac",
                "",
            ]
        else:
            # `soldr-daemon`, `crgx`, `cargo-chef` are tools soldr does not
            # control the version-string format of (matching what `run()`
            # checks for them natively elsewhere: exit 0, nothing about
            # stdout content).
            flag = "--help" if check == "help" else "--version"
            lines += [
                f"{dest} {flag} >/tmp/{name}.out 2>&1",
                "RC=$?",
                'if [ "$RC" -eq 0 ]; then',
                f'  echo "{name}_{check}=pass" >> "$SUMMARY"',
                "else",
                f'  echo "{name}_{check}=fail:exit $RC" >> "$SUMMARY"',
                "  FAIL=1",
                "fi",
                "",
            ]
    lines += [
        f"curl -fsS -o /tmp/{MACOS_X64_PYTHON_ARCHIVE} "
        f"{GUEST_HTTP_BASE}/{MACOS_X64_PYTHON_ARCHIVE}",
        "if [ $? -eq 0 ]; then",
        '  echo "fetch_python_runtime=pass" >> "$SUMMARY"',
        "  PYTHON_FETCH_OK=1",
        "else",
        '  echo "fetch_python_runtime=fail:portable CPython was unreachable" >> "$SUMMARY"',
        "  FAIL=1",
        "  PYTHON_FETCH_OK=0",
        "fi",
        f"curl -fsS -o /tmp/{wheel_name} {GUEST_HTTP_BASE}/{wheel_name}",
        "if [ $? -eq 0 ]; then",
        '  echo "fetch_wheel=pass" >> "$SUMMARY"',
        "  WHEEL_FETCH_OK=1",
        "else",
        '  echo "fetch_wheel=fail:shipped wheel was unreachable" >> "$SUMMARY"',
        "  FAIL=1",
        "  WHEEL_FETCH_OK=0",
        "fi",
        "",
        'if [ "$PYTHON_FETCH_OK" -eq 1 ]; then',
        "  mkdir -p /tmp/python-runtime",
        f"  tar -xzf /tmp/{MACOS_X64_PYTHON_ARCHIVE} -C /tmp/python-runtime",
        "  PY_RC=$?",
        "else",
        "  PY_RC=1",
        "fi",
        "PYTHON=/tmp/python-runtime/python/bin/python3",
        'if [ "$PY_RC" -eq 0 ] && [ -x "$PYTHON" ]; then',
        '  PY_VERSION=$("$PYTHON" --version 2>&1)',
        '  echo "python_runtime=pass:$PY_VERSION" >> "$SUMMARY"',
        "  PYTHON_OK=1",
        "else",
        '  echo "python_runtime=fail:portable CPython extraction failed" >> "$SUMMARY"',
        "  FAIL=1",
        "  PYTHON_OK=0",
        "fi",
        "",
        'if [ "$PYTHON_OK" -eq 1 ] && [ "$WHEEL_FETCH_OK" -eq 1 ]; then',
        '  "$PYTHON" -m venv /tmp/soldr-wheel-venv >/tmp/wheel-venv.out 2>&1',
        "  VENV_RC=$?",
        "else",
        "  VENV_RC=1",
        "fi",
        "VPYTHON=/tmp/soldr-wheel-venv/bin/python",
        "WHEEL_SOLDR=/tmp/soldr-wheel-venv/bin/soldr",
        'if [ "$VENV_RC" -eq 0 ]; then',
        f'  "$VPYTHON" -m pip install --no-index --no-deps /tmp/{wheel_name} '
        ">/tmp/wheel-install.out 2>&1",
        "  INSTALL_RC=$?",
        "else",
        "  INSTALL_RC=1",
        "fi",
        'if [ "$INSTALL_RC" -eq 0 ]; then',
        '  echo "wheel_install=pass" >> "$SUMMARY"',
        "else",
        '  echo "wheel_install=fail:$(tail -c 300 /tmp/wheel-install.out 2>/dev/null)" >> "$SUMMARY"',
        "  FAIL=1",
        "fi",
        "",
        'if [ "$INSTALL_RC" -eq 0 ]; then',
        f'  IMPORT_OUT=$("$VPYTHON" -c \'import {WHEEL_IMPORT_MODULE}; '
        f"print({WHEEL_IMPORT_MODULE}.__file__)' 2>&1)",
        "  IMPORT_RC=$?",
        "else",
        "  IMPORT_RC=1",
        '  IMPORT_OUT="wheel install failed"',
        "fi",
        'if [ "$IMPORT_RC" -eq 0 ]; then',
        '  echo "wheel_import=pass:$IMPORT_OUT" >> "$SUMMARY"',
        "else",
        '  echo "wheel_import=fail:$IMPORT_OUT" >> "$SUMMARY"',
        "  FAIL=1",
        "fi",
        "",
        'if [ "$INSTALL_RC" -eq 0 ]; then',
        '  WVOUT=$("$WHEEL_SOLDR" --version 2>&1)',
        "else",
        '  WVOUT="wheel install failed"',
        "fi",
        f'if [ "$WVOUT" = "soldr {expected_version}" ]; then',
        '  echo "wheel_version=pass:$WVOUT" >> "$SUMMARY"',
        "else",
        '  echo "wheel_version=fail:$WVOUT" >> "$SUMMARY"',
        "  FAIL=1",
        "fi",
        'if [ "$INSTALL_RC" -eq 0 ]; then',
        '  WJOUT=$("$WHEEL_SOLDR" version --json 2>&1)',
        "else",
        '  WJOUT="wheel install failed"',
        "fi",
        'case "$WJOUT" in',
        f'  *\'"soldr_version": "{expected_version}"\'*)',
        '    echo "wheel_version_json=pass:$WJOUT" >> "$SUMMARY" ;;',
        "  *)",
        '    echo "wheel_version_json=fail:$WJOUT" >> "$SUMMARY"',
        "    FAIL=1 ;;",
        "esac",
        "",
        'exit "$FAIL"',
    ]
    return "\n".join(lines) + "\n"


def append_github_output_multiline(path: Path, name: str, value: str) -> None:
    """Append a multi-line `name<<EOF / value / EOF` block to `$GITHUB_OUTPUT`.

    Doing this in Python instead of a separate bash heredoc step keeps the
    workflow's inline `run:` footprint down (soldr#2469 step 2.2's ratchet
    in tests/test_release_yaml_ratchet.py) and makes the delimiter handling
    testable instead of hand-typed YAML.
    """
    delimiter = f"GITHUB_OUTPUT_{name.upper()}_EOF"
    with path.open("a", encoding="utf-8") as handle:
        handle.write(f"{name}<<{delimiter}\n{value}{delimiter}\n")


@dataclass(frozen=True)
class CheckResult:
    """One guest check: its name, whether it passed, and its detail text."""

    name: str
    ok: bool
    detail: str


def find_result(results: list[CheckResult], name: str) -> CheckResult | None:
    """The recorded result for `name`, if the guest wrote one."""
    return next((result for result in results if result.name == name), None)


def parse_summary(text: str) -> list[CheckResult]:
    """Parse the guest's flat `key=value` results file.

    Each line is `name=pass[:detail]` or `name=fail[:detail]`; a malformed
    or truncated line (a wedged guest can be killed mid-write) is not
    silently dropped -- it fails as its own diagnostic. A repeated name keeps
    its first position and its last value.
    """
    results: list[CheckResult] = []

    def record(result: CheckResult) -> None:
        for index, existing in enumerate(results):
            if existing.name == result.name:
                results[index] = result
                return
        results.append(result)

    for lineno, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line:
            continue
        name, sep, rest = line.partition("=")
        if not sep:
            record(
                CheckResult(f"summary_line_{lineno}", False, f"malformed line: {raw!r}")
            )
            continue
        status, _, detail = rest.partition(":")
        record(CheckResult(name, status == "pass", detail))
    return results


def expected_check_names() -> list[str]:
    names = ["arch"]
    for binary in GUEST_BINARY_CHECKS:
        name, check = binary.name, binary.check
        names.append(f"fetch_{name}")
        names.append(f"{name}_{check}")
    names.append("soldr_version_json")
    names.extend(
        [
            "fetch_python_runtime",
            "fetch_wheel",
            "python_runtime",
            "wheel_install",
            "wheel_import",
            "wheel_version",
            "wheel_version_json",
        ]
    )
    return names


def verify_collected(collected_dir: Path, *, guest_exit_code: str) -> int:
    """Read the collected Recovery results and fail with a named diagnostic.

    `guest_exit_code` is the action's own `exit-code` output -- the guest
    script's process exit status. It is checked in addition to, not instead
    of, the per-check summary: a script that dies before writing every line
    (a killed heartbeat, an unhandled signal) must not read as a pass just
    because the lines it did write all say `pass`.
    """
    summary_path = collected_dir / RESULTS_FILE
    if not summary_path.is_file():
        sys.exit(
            f"ERROR: {summary_path} is missing — the Recovery guest never wrote "
            "results (did the script even reach a shell? check the action's "
            "workdir/results/*.ppm screendumps)."
        )
    results = parse_summary(summary_path.read_text(encoding="utf-8", errors="replace"))

    failures: list[str] = []
    for name in expected_check_names():
        result = find_result(results, name)
        if result is None:
            failures.append(f"{name}: no result recorded (guest script exited early?)")
            continue
        if not result.ok:
            failures.append(f"{name}: {result.detail}")

    if guest_exit_code.strip() != "0":
        failures.append(
            f"guest script exit code {guest_exit_code!r} != '0' "
            "(see the per-check results above for which check failed)"
        )

    if failures:
        joined = "\n  - ".join(failures)
        sys.exit(f"ERROR: Recovery guest smoke failed:\n  - {joined}")

    print(f"Recovery guest smoke OK: {len(results)} checks recorded, all passed")
    return 0


def main() -> int:  # noqa: C901
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, help="rust target triple")
    parser.add_argument(
        "--expected-version", required=True, help="release version, vX.Y.Z"
    )
    parser.add_argument("--dist", default="dist", type=Path)
    parser.add_argument(
        "--emit-guest-script",
        default=None,
        type=Path,
        help=(
            "soldr#3076: write a bash-3.2 script to this path that runs the "
            "guest-side checks inside a zackees/docker-mac-x64 Recovery "
            "guest, and copy the binaries it fetches into --share-dir. When "
            "given, this invocation does the Linux-side checks and then "
            "exits WITHOUT executing any binary itself."
        ),
    )
    parser.add_argument(
        "--share-dir",
        default=None,
        type=Path,
        help="directory the guest binaries are copied into, paired with --emit-guest-script",
    )
    parser.add_argument(
        "--verify-collected",
        default=None,
        type=Path,
        help=(
            "soldr#3076: skip every Linux-side check and instead read the "
            "Recovery guest's collected results directory (the action's "
            "`collect` tarball, already extracted by the workflow) and fail "
            "with a named diagnostic per check. Pair with --guest-exit-code."
        ),
    )
    parser.add_argument(
        "--guest-exit-code",
        default=None,
        help="the docker-mac-x64 action's `exit-code` output; required with --verify-collected",
    )
    parser.add_argument(
        "--github-output",
        default=None,
        type=Path,
        help=(
            "soldr#3076: with --emit-guest-script, also append the guest "
            "script to this $GITHUB_OUTPUT file as a 'script' multi-line "
            "output, so the workflow needs no separate heredoc step to hand "
            "it to the docker-mac-x64 action's `run:` input."
        ),
    )
    parser.add_argument(
        "--require-wheel-import",
        action="store_true",
        help=(
            "require the shipped wheel to install and import on the native "
            "runner or inside the Recovery guest"
        ),
    )
    parser.add_argument(
        "--require-daemon-cache-smoke",
        action="store_true",
        help="start the shipped daemon and require its native status and cache report",
    )
    args = parser.parse_args()

    target: str = args.target
    version: str = args.expected_version
    expected = version.removeprefix("v")
    suffix = exe_suffix(target)

    if args.verify_collected is not None:
        if args.guest_exit_code is None:
            parser.error("--verify-collected requires --guest-exit-code")
        return verify_collected(
            args.verify_collected, guest_exit_code=args.guest_exit_code
        )

    if args.emit_guest_script is not None and args.share_dir is None:
        parser.error("--emit-guest-script requires --share-dir")

    wheels = sorted(args.dist.glob("*.whl"))
    if len(wheels) != 1:
        sys.exit(f"expected exactly one wheel in {args.dist}, found {wheels}")

    reported_wheel_version = wheel_version(wheels[0])
    if reported_wheel_version != expected:
        sys.exit(
            f"ERROR: wheel {wheels[0]}: METADATA Version {reported_wheel_version!r} "
            f"!= expected {expected!r} (soldr#1202)."
        )
    print(f"{target} wheel — METADATA Version: {reported_wheel_version}")

    archive = args.dist / f"soldr-{version}-{target}.tar.zst"
    if not archive.is_file():
        sys.exit(f"missing archive: {archive}")
    extract = Path("extracted")
    extract_archive(archive, extract)

    for member in required_members(target):
        if not (extract / member).is_file():
            sys.exit(f"missing {member} in {archive}")
    if suffix:
        # docs/DEBUG_SIDECARS.md: the PDB sidecar is REQUIRED on Windows.
        pdbs = [p for p in ("soldr.pdb", "soldr_cli.pdb") if (extract / p).is_file()]
        if not pdbs:
            sys.exit(f"missing soldr PDB sidecar in {archive}")
        print(f"PDB sidecar present: {pdbs}")

    soldr_bin = extract / f"soldr{suffix}"
    size = soldr_bin.stat().st_size
    if size < MIN_SOLDR_BYTES:
        sys.exit(
            f"ERROR: {soldr_bin} is {size} bytes, expected >= {MIN_SOLDR_BYTES} "
            "(soldr#1140 / soldr#1202 stub-binary floor)."
        )

    arch = macho_arch(target)
    if arch is not None:
        check_macho_architecture(soldr_bin, arch)

    if args.emit_guest_script is not None:
        # Recovery has no built-in Python/Xcode CLT, so execution moves into
        # a generated guest script. A pinned portable CPython makes the wheel
        # runtime check possible without pretending the Recovery image grew a
        # compiler or SDK.
        copy_into_share_dir(extract, args.share_dir, suffix)
        if not args.require_wheel_import:
            parser.error(
                "macOS Recovery smoke must pass --require-wheel-import; "
                "host-side wheel metadata is not runtime coverage"
            )
        wheel_name = stage_macos_wheel_runtime(wheels[0], args.share_dir)
        script_text = build_release_guest_script(expected, wheel_name)
        args.emit_guest_script.parent.mkdir(parents=True, exist_ok=True)
        args.emit_guest_script.write_text(script_text, encoding="utf-8")
        print(
            f"wrote Recovery guest script to {args.emit_guest_script} "
            f"({len(GUEST_BINARY_CHECKS)} binaries staged in {args.share_dir})"
        )
        if args.github_output is not None:
            append_github_output_multiline(args.github_output, "script", script_text)
            print(f"appended 'script' output to {args.github_output}")
        return 0

    check_version_output(soldr_bin, expected, f"{target} archive")
    run([extract / f"soldr-daemon{suffix}", "--help"])
    run([extract / f"crgx{suffix}", "--version"])
    run([extract / f"cargo-chef{suffix}", "--version"])
    if args.require_daemon_cache_smoke:
        run([soldr_bin, "daemon", "start"])
        status = json.loads(run([soldr_bin, "daemon", "status", "--json"]).stdout)
        if status.get("running") is not True:
            sys.exit(f"ERROR: shipped daemon did not report running: {status!r}")
        run([soldr_bin, "cache", "report", "--json"])
    if args.require_wheel_import:
        check_native_wheel_import(wheels[0], expected)
    print(f"native smoke OK: {target} {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
