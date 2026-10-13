#!/usr/bin/env python3
"""Assert a native-host GNU preparation selects an executable compiler.

soldr#2874. On a native ARM64 Linux runner, `soldr prepare --target
aarch64-unknown-linux-gnu` selected the catalogue `linux-arm64-gnu` bundle and
exported its compilers. That slug names the *target* shape; every catalogue
bundle is x86_64-hosted, so the exported `cc` was an x86_64 ELF and the first
`-sys` crate died with `Exec format error (os error 8)` -- several hundred
megabytes and many minutes after the wrong decision was made.

The check that would have caught it is not "did prepare succeed" (it did) but
**"can the compiler it chose actually run here"**. So this runs the
preparation, then executes whatever compiler came out of it.

Usage:
    python .github/scripts/assert_native_gnu_prepare.py \
        --soldr /path/to/soldr --target aarch64-unknown-linux-gnu
"""

from __future__ import annotations

import argparse
import platform
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

# The env keys that name an executable the build will later invoke. A path
# exported under any of these has to run on this host.
COMPILER_KEYS_TEMPLATE = (
    "CC_{suffix}",
    "CXX_{suffix}",
    "AR_{suffix}",
    "RANLIB_{suffix}",
    "CARGO_TARGET_{upper}_LINKER",
)


def compiler_keys(target: str) -> tuple[str, ...]:
    suffix = target.replace("-", "_")
    return tuple(
        key.format(suffix=suffix, upper=suffix.upper())
        for key in COMPILER_KEYS_TEMPLATE
    )


def parse_env_file(text: str) -> MappingProxyType[str, str]:
    """`KEY=VALUE` lines, as `--github-env` writes them.

    Blank lines and lines without `=` are skipped rather than raising: this
    runs to produce a verdict about something else, and a malformed line is a
    fact to report, not a reason to crash before reporting anything.
    """
    env: dict[str, str] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or "=" not in line:
            continue
        key, _, value = line.partition("=")
        env[key.strip()] = value.strip()
    return MappingProxyType(env)


@dataclass(frozen=True)
class ExecutionProbe:
    """Whether the host could execute a binary, and the evidence."""

    runnable: bool
    detail: str


def is_executable_here(path: str) -> ExecutionProbe:
    """Run `<path> --version` and report whether the host could execute it.

    An `Exec format error` is `OSError` with `errno == 8` on Linux, which is
    the precise signature of soldr#2874. A non-zero *exit status* is a
    different thing entirely -- the binary ran -- so it is not a failure here.
    """
    try:
        completed = run_captured(
            [path, "--version"],
            capture_output=True,
            timeout=60,
            check=False,
        )
    except OSError as error:
        return ExecutionProbe(False, f"{type(error).__name__}: {error}")
    first_line = completed.stdout.decode("utf-8", "replace").splitlines()
    return ExecutionProbe(
        True, first_line[0] if first_line else f"exit {completed.returncode}"
    )


def check(soldr: str, target: str) -> int:
    with tempfile.TemporaryDirectory() as scratch:
        env_file = Path(scratch) / "prepared.env"
        env_file.touch()
        print(f"$ {soldr} prepare --target {target} --github-env {env_file}")
        completed = run_captured(
            [soldr, "prepare", "--target", target, "--github-env", str(env_file)],
            capture_output=False,
            check=False,
        )
        if completed.returncode != 0:
            print(
                f"FAIL: `soldr prepare --target {target}` exited "
                f"{completed.returncode} on this host",
                file=sys.stderr,
            )
            return 1
        exported = parse_env_file(env_file.read_text(encoding="utf-8"))

    print(f"## exported keys: {len(exported)}")
    failures: list[str] = []
    checked = 0
    for key in compiler_keys(target):
        value = exported.get(key)
        if not value:
            # Not exporting a compiler is a legitimate outcome on a native
            # host: soldr#2874's fix falls back to the host's own toolchain,
            # which cc-rs and rustc find without help.
            print(f"  {key}: (not exported -- host compiler)")
            continue
        checked += 1
        probe = is_executable_here(value)
        status = "runs" if probe.runnable else "CANNOT EXECUTE"
        print(f"  {key}={value} -> {status}: {probe.detail}")
        if not probe.runnable:
            failures.append(f"{key}={value}: {probe.detail}")

    if failures:
        print(
            "\nFAIL (soldr#2874): preparation exported a compiler this host "
            "cannot execute. The bundle slug names the target shape, not the "
            "host shape.",
            file=sys.stderr,
        )
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print(
        f"\nOK: {checked} exported compiler path(s) execute on this host "
        f"(host={platform.machine() or 'unknown'})"
    )
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", required=True, help="path to the soldr binary")
    parser.add_argument("--target", required=True, help="target triple to prepare")
    args = parser.parse_args(argv)
    return check(args.soldr, args.target)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
