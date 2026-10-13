"""The native-GNU preparation assertion (soldr#2874).

The bug it guards is invisible to "did prepare succeed": preparation DID
succeed on the ARM64 runner, exported an x86_64 compiler, and the failure
surfaced hundreds of megabytes later inside a `-sys` crate. So the assertion
executes whatever compiler preparation chose, and these cover the parsing and
verdict logic without needing an ARM64 host.
"""

from __future__ import annotations

import shutil
import stat
import sys
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
_SCRIPT = REPO_ROOT / ".github" / "scripts" / "assert_native_gnu_prepare.py"
assert_native = load_script_module(_SCRIPT, "assert_native_gnu_prepare")

parse_env_file = assert_native.parse_env_file
compiler_keys = assert_native.compiler_keys
is_executable_here = assert_native.is_executable_here

ARM64 = "aarch64-unknown-linux-gnu"


def test_the_compiler_keys_are_target_scoped() -> None:
    keys = compiler_keys(ARM64)
    assert "CC_aarch64_unknown_linux_gnu" in keys
    assert "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER" in keys


def test_env_lines_are_parsed_into_pairs() -> None:
    parsed = parse_env_file("CC_x=/bin/true\nCXX_x=/bin/false\n")
    assert parsed == {"CC_x": "/bin/true", "CXX_x": "/bin/false"}


def test_a_value_containing_equals_keeps_its_tail() -> None:
    # RUSTFLAGS-shaped values carry `=` inside them; a naive split would
    # truncate a path or a flag and then "check" something that was never
    # exported.
    parsed = parse_env_file("CARGO_TARGET_X_RUSTFLAGS=-C link-arg=--sysroot=/s\n")
    assert parsed["CARGO_TARGET_X_RUSTFLAGS"] == "-C link-arg=--sysroot=/s"


def test_malformed_lines_are_skipped_not_raised() -> None:
    # This runs to produce a verdict about something else; crashing on a
    # stray line would replace the answer with a traceback.
    assert parse_env_file("\nnot an assignment\n\nCC_x=/bin/true\n") == {
        "CC_x": "/bin/true"
    }


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX exec semantics")
def test_a_missing_binary_is_reported_as_not_executable() -> None:
    probe = is_executable_here("/definitely/not/here/cc")
    assert not probe.runnable
    assert "Error" in probe.detail or "error" in probe.detail


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX exec semantics")
def test_a_file_that_is_not_a_valid_executable_is_reported_as_not_executable(
    tmp_path: Path,
) -> None:
    """The soldr#2874 shape: present, executable bit set, wrong machine.

    A real ARM64 host running an x86_64 ELF raises `OSError` with `errno == 8`
    (`Exec format error`). A file whose contents are not a valid executable at
    all reaches the same branch, which is the branch under test -- the point
    is that an unrunnable compiler is a FAILURE and not a pass.
    """
    fake = tmp_path / "cc"
    fake.write_bytes(b"\x7fELF this is not a loadable image for this machine")
    fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
    assert not is_executable_here(str(fake)).runnable


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX exec semantics")
def test_a_runnable_binary_passes_even_when_it_exits_non_zero() -> None:
    # `--version` is not universally supported. The question is whether the
    # host could EXECUTE the file, not whether it liked the argument, and
    # conflating those would fail hosts that are perfectly fine.
    false_binary = shutil.which("false")
    assert false_binary is not None
    assert is_executable_here(false_binary).runnable


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX exec semantics")
def test_a_runnable_binary_reports_its_first_output_line() -> None:
    probe = is_executable_here("/bin/echo")
    assert probe.runnable
    assert probe.detail is not None
