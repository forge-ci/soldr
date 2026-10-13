"""Docker-based integration test that reproduces issue #406.

Spawns `catthehacker/ubuntu:act-24.04` (the default medium image nektos/act
uses), mounts a locally-built linux soldr binary, and verifies that

    SOLDR_CACHE_DIR=/tmp/soldr soldr bootstrap

successfully installs rustup into the soldr-managed bin dir on an image that
has no preinstalled toolchain manager. Without the fix from issue #406, the
soldr CLI would exit 127 with `rustup: command not found` the moment it tried
to resolve a toolchain binary.

The test is opt-in: it requires docker, pulls a ~1 GB image, and downloads
rustup-init over the network. Run with::

    uv run pytest tests/test_bootstrap_act_image.py --act-integration

or::

    uv run pytest -m act_integration

The test is skipped by default via `tests/conftest.py`.

The mounted binary must be a Linux ELF — building soldr on Windows produces an
`.exe` that the test will refuse to use, so this test only meaningfully runs on
linux x86_64 hosts (or any host that has produced a linux x86_64 soldr binary
at `target/x86_64-unknown-linux-gnu/{debug,release}/soldr`).
"""

from __future__ import annotations

import sys
from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import docker_available

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

ACT_IMAGE = "catthehacker/ubuntu:act-24.04"
REPO_ROOT = Path(__file__).resolve().parents[1]

# ELF identification, per elf(5). Only the leading e_ident bytes and the
# e_machine field are needed to tell a linux x86-64 executable from a Mach-O,
# a PE, or an ELF built for another architecture.
_ELF_MAGIC = b"\x7fELF"
_ELFCLASS64 = 2
_ELFDATA2LSB = 1
_EM_X86_64 = 0x3E
# e_machine is a 2-byte little-endian field at offset 18.
_E_MACHINE_END = 20


def describe_binary_incompatibility(path: Path) -> str | None:
    """Return ``None`` if ``path`` is a linux x86-64 ELF, else why it is not.

    The two untargeted candidates below (``target/{release,debug}/soldr``) are
    whatever the host last built. On a macOS host that is a Mach-O; on a linux
    aarch64 host it is an ELF for the wrong machine. Docker mounts any of them
    happily and only fails deep inside the container with an opaque
    ``exec format error``, so the header is checked here to turn that into a
    readable skip (#1666).
    """
    try:
        header = path.read_bytes()[:_E_MACHINE_END]
    except OSError as exc:  # unreadable candidate is simply not usable
        return f"unreadable ({exc})"

    if len(header) < _E_MACHINE_END:
        return f"too small to hold an ELF header ({len(header)} bytes)"
    if header[:4] != _ELF_MAGIC:
        return f"not an ELF — magic {header[:4]!r} (a Mach-O or PE build?)"
    if header[4] != _ELFCLASS64:
        return f"not 64-bit — EI_CLASS {header[4]}, want {_ELFCLASS64}"
    if header[5] != _ELFDATA2LSB:
        return f"not little-endian — EI_DATA {header[5]}, want {_ELFDATA2LSB}"

    machine = int.from_bytes(header[18:_E_MACHINE_END], "little")
    if machine != _EM_X86_64:
        return f"wrong architecture — e_machine 0x{machine:02x}, want 0x3e (x86-64)"
    return None


@dataclass(frozen=True)
class BinaryLookup:
    """The compatible binary (if any) and a line per rejected candidate."""

    binary: Path | None
    rejected: tuple[str, ...]


def _locate_linux_soldr_binary() -> BinaryLookup:
    """Find a locally-built linux x86-64 soldr binary, if any.

    Returns the first *compatible* candidate together with a diagnostic line
    for every candidate that existed but was the wrong format, so the skip
    message can say "found one, but it was a Mach-O" instead of the much less
    useful "no binary found".
    """
    candidates = [
        REPO_ROOT / "target" / "x86_64-unknown-linux-gnu" / "release" / "soldr",
        REPO_ROOT / "target" / "x86_64-unknown-linux-gnu" / "debug" / "soldr",
        REPO_ROOT / "target" / "release" / "soldr",
        REPO_ROOT / "target" / "debug" / "soldr",
    ]
    rejected: list[str] = []
    for candidate in candidates:
        if not candidate.is_file():
            continue
        reason = describe_binary_incompatibility(candidate)
        if reason is None:
            return BinaryLookup(candidate, tuple(rejected))
        rejected.append(f"  {candidate.relative_to(REPO_ROOT)}: {reason}")
    return BinaryLookup(None, tuple(rejected))


@pytest.mark.act_integration
def test_soldr_bootstrap_installs_rustup_on_act_image(tmp_path: Path) -> None:
    if not docker_available():
        pytest.skip("docker daemon not reachable")

    lookup = _locate_linux_soldr_binary()
    soldr_bin, rejected = lookup.binary, lookup.rejected
    if soldr_bin is None:
        detail = (
            "\nrejected incompatible candidates:\n" + "\n".join(rejected)
            if rejected
            else ""
        )
        pytest.skip(
            "no linux x86-64 soldr binary found at "
            "target/x86_64-unknown-linux-gnu/{release,debug}/soldr or "
            "target/{release,debug}/soldr — build with `cargo build "
            "--release --target x86_64-unknown-linux-gnu -p soldr-cli` "
            "first (on a linux host, or via `docker run rust:1.94.1-slim` "
            "from a Windows host with Docker Desktop)." + detail
        )

    soldr_cache = tmp_path / "soldr-cache"
    soldr_cache.mkdir()

    cmd = [
        "docker",
        "run",
        "--rm",
        "--network=bridge",
        "-v",
        f"{soldr_bin}:/usr/local/bin/soldr:ro",
        "-v",
        f"{soldr_cache}:/soldr",
        "-e",
        "SOLDR_CACHE_DIR=/soldr",
        # Belt-and-braces: explicitly disable the opt-out so the test exercises
        # the bootstrap path even if a future default flips.
        "-e",
        "SOLDR_NO_BOOTSTRAP=0",
        ACT_IMAGE,
        "bash",
        "-c",
        # Repro the exact failure surface from issue #406: image has no rustup
        # preinstalled, soldr must bootstrap one. We then verify the managed
        # rustup binary exists and is executable.
        "set -euxo pipefail; "
        "if command -v rustup >/dev/null; then "
        '  echo "FAIL: act image unexpectedly ships rustup" >&2; exit 99; '
        "fi; "
        "soldr bootstrap --json; "
        "test -x /soldr/bin/rustup; "
        "/soldr/bin/rustup --version",
    ]

    result = run_captured(cmd, capture_output=True, text=True, timeout=600, check=False)
    assert result.returncode == 0, (
        f"docker run failed (exit {result.returncode})\n"
        f"stdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}"
    )
    # JSON line should report a real install, not the idempotent already_installed=true.
    assert '"already_installed": false' in result.stdout, (
        "expected first-run install report; stdout was:\n" + result.stdout
    )
    assert "rustup" in result.stdout, (
        "expected `rustup --version` output to mention rustup"
    )


def _elf64_header(machine: int, *, elf_class: int = _ELFCLASS64) -> bytes:
    """Minimal 64-bit little-endian ELF identification block.

    Only the bytes `describe_binary_incompatibility` inspects need to be real;
    the rest is padding to reach the e_machine field at offset 18.
    """
    header = bytearray(_E_MACHINE_END)
    header[0:4] = _ELF_MAGIC
    header[4] = elf_class
    header[5] = _ELFDATA2LSB
    header[6] = 1  # EI_VERSION
    header[16:18] = (2).to_bytes(2, "little")  # e_type = ET_EXEC
    header[18:20] = machine.to_bytes(2, "little")
    return bytes(header)


def test_linux_x86_64_elf_is_accepted(tmp_path: Path) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(_elf64_header(_EM_X86_64) + b"\x00" * 64)

    assert describe_binary_incompatibility(binary) is None


def test_mach_o_binary_is_rejected_with_a_clear_reason(tmp_path: Path) -> None:
    """A macOS build must be refused, not handed to Docker (#1666).

    0xfeedfacf is the 64-bit Mach-O magic — exactly what `target/release/soldr`
    contains on a macOS host, which is how an incompatible binary used to reach
    `docker run` and surface as an opaque exec-format error.
    """
    binary = tmp_path / "soldr"
    binary.write_bytes((0xFEEDFACF).to_bytes(4, "little") + b"\x00" * 64)

    reason = describe_binary_incompatibility(binary)
    assert reason is not None
    assert "not an ELF" in reason


def test_wrong_architecture_elf_is_rejected(tmp_path: Path) -> None:
    """An aarch64 ELF is still an ELF — the machine type has to be checked."""
    binary = tmp_path / "soldr"
    binary.write_bytes(_elf64_header(0xB7) + b"\x00" * 64)  # EM_AARCH64

    reason = describe_binary_incompatibility(binary)
    assert reason is not None
    assert "wrong architecture" in reason
    assert "0xb7" in reason


def test_32_bit_elf_is_rejected(tmp_path: Path) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(_elf64_header(_EM_X86_64, elf_class=1) + b"\x00" * 64)

    reason = describe_binary_incompatibility(binary)
    assert reason is not None
    assert "not 64-bit" in reason


def test_truncated_file_is_rejected(tmp_path: Path) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(_ELF_MAGIC)

    reason = describe_binary_incompatibility(binary)
    assert reason is not None
    assert "too small" in reason
