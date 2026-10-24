"""Tests for the extracted release binary-staging gate (soldr#2469)."""

from __future__ import annotations

import shutil
import sys
from pathlib import Path

import pytest
from conftest import load_script_module
from test_check_linked_libs import build_elf64

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"

stage = load_script_module(
    SCRIPTS / "stage_release_binaries.py", "stage_release_binaries"
)


def write_file(directory: Path, name: str, contents: bytes = b"artifact") -> Path:
    path = directory / name
    path.write_bytes(contents)
    return path


def test_windows_requires_and_stages_a_pdb_sidecar(tmp_path: Path) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr.exe", b"soldr")
    write_file(release, "soldr_cli.pdb", b"symbols")

    staged = stage.stage_release_binaries("x86_64-pc-windows-msvc", release, package)

    assert [path.name for path in staged] == [
        "soldr.exe",
        "soldr-daemon.exe",
        "soldr_cli.pdb",
    ]
    assert (package / "soldr.exe").read_bytes() == b"soldr"
    assert (package / "soldr-daemon.exe").read_bytes() == b"soldr"
    assert (package / "soldr_cli.pdb").read_bytes() == b"symbols"


def test_windows_missing_pdb_is_a_named_release_failure(tmp_path: Path) -> None:
    release = tmp_path / "release"
    release.mkdir()
    write_file(release, "soldr.exe")

    with pytest.raises(stage.StagingError, match="PDB sidecar"):
        stage.stage_release_binaries(
            "x86_64-pc-windows-msvc", release, tmp_path / "package"
        )


def test_linux_release_with_split_dwarf_never_stages_it_into_package_dir(
    tmp_path: Path,
) -> None:
    """soldr#3038 regression guard: package_dir feeds manifest.json's
    soldr.debug_info, which every setup-soldr consumer requires to be
    pdb-only (zccache_contract.py::validate_release_manifest). A .dwp must
    never land here even when the release profile produced one.
    """
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr")
    write_file(release, "soldr_cli.dwp", b"debug")

    staged = stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon"]
    assert not (package / "soldr_cli.dwp").exists()


def test_linux_release_without_split_dwarf_still_stages_binary(tmp_path: Path) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr")

    staged = stage.stage_release_binaries("x86_64-unknown-linux-musl", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon"]


def test_linux_release_stages_a_compliant_elf(tmp_path: Path) -> None:
    """A binary whose NEEDED entries are all system libraries stages fine."""
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr", build_elf64(["libc.so.6", "libm.so.6"]))

    staged = stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon"]


def test_linux_release_refuses_a_vendored_dynamic_dependency(tmp_path: Path) -> None:
    """soldr's static-liblzma follow-up: a release binary that dynamically
    links a non-system shared library (the exact liblzma bug this guards
    against) must fail staging instead of shipping the risk to every
    installer's build host.
    """
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr", build_elf64(["libc.so.6", "liblzma.so.5"]))

    with pytest.raises(stage.StagingError, match="liblzma.so.5"):
        stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)


def test_macos_release_with_dsym_never_stages_it_into_package_dir(
    tmp_path: Path,
) -> None:
    """soldr#3038 regression guard, macOS half of the Linux .dwp test above."""
    release = tmp_path / "release"
    package = tmp_path / "package"
    dsym = release / "soldr.dSYM" / "Contents" / "Resources"
    dsym.mkdir(parents=True)
    write_file(release, "soldr")
    write_file(dsym, "DWARF", b"symbols")

    staged = stage.stage_release_binaries("aarch64-apple-darwin", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon"]
    assert not (package / "soldr.dSYM").exists()


def fake_objcopy_strip(calls: list[list[str]]):
    """A stand-in for GNU objcopy/strip that records invocations and mutates
    files the same shape the real tools would, without needing a real ELF
    binary or the tools themselves installed in the test environment.
    """

    def run_tool(args: list[str]) -> None:
        calls.append(list(args))
        program = Path(args[0]).name
        if program in {"objcopy", "llvm-objcopy"} and args[1] == "--only-keep-debug":
            # objcopy --only-keep-debug SRC DEST
            Path(args[3]).write_bytes(b"debug-section-bytes")
        elif program in {"strip", "llvm-strip"} and args[1] == "--strip-debug":
            # strip --strip-debug BINARY -- modifies BINARY's own inode in
            # place, exactly like the real tool: a hardlinked soldr-daemon
            # sees this too, for free.
            Path(args[2]).write_bytes(b"stripped-binary-bytes")
        elif program in {"objcopy", "llvm-objcopy"} and args[1].startswith(
            "--add-gnu-debuglink"
        ):
            pass  # in-place section add; no observable content change here
        elif program in {"strip", "llvm-strip"} and args[1] == "-x":
            # macOS: strip -x BINARY, same in-place-mutation shape.
            Path(args[2]).write_bytes(b"darwin-stripped-bytes")
        else:  # pragma: no cover - guards against an un-mocked call shape
            raise AssertionError(f"unexpected tool invocation: {args}")

    return run_tool


def test_stage_debug_symbols_carves_linux_debug_info_via_objcopy(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    symbols = tmp_path / "symbols"
    release.mkdir()
    write_file(release, "soldr", b"linked-binary-bytes")
    stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    calls: list[list[str]] = []
    monkeypatch.setattr(stage, "run_tool", fake_objcopy_strip(calls))

    staged = stage.stage_debug_symbols(
        "x86_64-unknown-linux-gnu",
        release,
        package,
        symbols,
        objcopy="objcopy",
        strip_tool="strip",
    )

    assert [path.name for path in staged] == ["soldr.debug"]
    assert (symbols / "soldr.debug").read_bytes() == b"debug-section-bytes"
    debug_dest = str(symbols / "soldr.debug")
    soldr = str(package / "soldr")
    assert calls == [
        ["objcopy", "--only-keep-debug", soldr, debug_dest],
        ["strip", "--strip-debug", soldr],
        ["objcopy", f"--add-gnu-debuglink={debug_dest}", soldr],
    ]
    # soldr-daemon was staged as a hardlink to soldr (same inode); the fake
    # strip's in-place write to soldr's inode is visible through it too, for
    # free -- matching the real tool's behavior. No re-derive needed.
    assert (package / "soldr-daemon").read_bytes() == b"stripped-binary-bytes"


def test_stage_debug_symbols_re_derives_a_daemon_that_was_not_hardlinked(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """soldr#3038: 'verify by hash rather than assuming.'

    If `soldr-daemon` ever reaches this point as an independent copy (the
    `copy_or_link` cross-device fallback) rather than a hardlink, stripping
    `soldr` in place does NOT touch it -- it would ship with stale,
    unstripped bytes unless this notices the mismatch and re-derives it.
    """
    release = tmp_path / "release"
    package = tmp_path / "package"
    symbols = tmp_path / "symbols"
    release.mkdir()
    write_file(release, "soldr", b"linked-binary-bytes")
    stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    # Simulate the copy fallback: replace the hardlinked daemon with an
    # independent file holding different (stale) bytes.
    (package / "soldr-daemon").unlink()
    (package / "soldr-daemon").write_bytes(b"stale-independent-copy")

    calls: list[list[str]] = []
    monkeypatch.setattr(stage, "run_tool", fake_objcopy_strip(calls))

    stage.stage_debug_symbols(
        "x86_64-unknown-linux-gnu",
        release,
        package,
        symbols,
        objcopy="objcopy",
        strip_tool="strip",
    )

    assert (package / "soldr-daemon").read_bytes() == (package / "soldr").read_bytes()
    assert (package / "soldr-daemon").read_bytes() == b"stripped-binary-bytes"


def test_stage_debug_symbols_requires_the_binary_already_staged(
    tmp_path: Path,
) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"  # deliberately never staged
    symbols = tmp_path / "symbols"
    release.mkdir()
    package.mkdir()

    with pytest.raises(stage.StagingError, match="already staged"):
        stage.stage_debug_symbols("x86_64-unknown-linux-gnu", release, package, symbols)


def test_run_tool_wraps_a_failing_command_in_a_staging_error() -> None:
    with pytest.raises(stage.StagingError, match="exit 3"):
        stage.run_tool([sys.executable, "-c", "import sys; sys.exit(3)"])


def test_stage_debug_symbols_stages_macos_dsym_and_strips_the_staged_binary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    symbols = tmp_path / "symbols"
    dsym = release / "soldr.dSYM" / "Contents" / "Resources"
    dsym.mkdir(parents=True)
    write_file(release, "soldr")
    write_file(dsym, "DWARF", b"symbols")
    stage.stage_release_binaries("aarch64-apple-darwin", release, package)

    calls: list[list[str]] = []
    monkeypatch.setattr(stage, "run_tool", fake_objcopy_strip(calls))

    staged = stage.stage_debug_symbols(
        "aarch64-apple-darwin", release, package, symbols, darwin_strip="strip"
    )

    assert [path.name for path in staged] == ["soldr.dSYM"]
    assert (
        symbols / "soldr.dSYM" / "Contents" / "Resources" / "DWARF"
    ).read_bytes() == b"symbols"
    # dsymutil already captured everything from the intermediate .o files
    # (see the module docstring); `strip -x` runs on the STAGED binary
    # afterward, and the hardlinked daemon reflects it for free.
    assert calls == [["strip", "-x", str(package / "soldr")]]
    assert (package / "soldr").read_bytes() == b"darwin-stripped-bytes"
    assert (package / "soldr-daemon").read_bytes() == b"darwin-stripped-bytes"


def test_stage_debug_symbols_is_a_clean_no_op_when_nothing_was_emitted(
    tmp_path: Path,
) -> None:
    """Windows always, and any Unix build whose profile emitted no sidecar."""
    release = tmp_path / "release"
    package = tmp_path / "package"
    symbols = tmp_path / "symbols"
    release.mkdir()
    write_file(release, "soldr.exe")
    write_file(release, "soldr_cli.pdb", b"pdb-bytes")
    stage.stage_release_binaries("x86_64-pc-windows-msvc", release, package)

    staged = stage.stage_debug_symbols(
        "x86_64-pc-windows-msvc", release, package, symbols
    )

    assert staged == []
    assert not symbols.exists()


def test_missing_main_binary_reports_observed_release_directory(tmp_path: Path) -> None:
    release = tmp_path / "release"
    release.mkdir()
    write_file(release, "another-file")

    with pytest.raises(stage.StagingError, match="expected soldr") as error:
        stage.stage_release_binaries(
            "x86_64-unknown-linux-gnu", release, tmp_path / "package"
        )

    assert "another-file" in str(error.value)


def test_workflow_invokes_the_script_instead_of_inlining_binary_staging() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/stage_release_binaries.py" in workflow
    assert "pdb_src=" not in workflow
    assert "staged Linux split-DWARF sidecar" not in workflow


def test_daemon_sidecar_links_through_a_portable_api() -> None:
    """`Path.hardlink_to` is Python 3.10+; the release runners are not all.

    A runner whose `python3` predated 3.10 raised AttributeError here and took
    down the v0.9.3 macOS ARM64 release lane (soldr#2763). A behavioral test
    cannot catch a reintroduction, because the interpreter running the suite
    has the newer API -- so assert the source-level choice instead.
    """
    source = (SCRIPTS / "stage_release_binaries.py").read_text(encoding="utf-8")
    assert "os.link(" in source
    # The call form, not the bare name -- the code comment above the fix
    # names the rejected API deliberately.
    assert ".hardlink_to(" not in source


def test_release_jobs_running_python_pin_the_interpreter() -> None:
    """Every release job that shells to python3 must install a known one."""
    workflow = WORKFLOW.read_text(encoding="utf-8")

    # The build matrix stages binaries; the macOS ARM smoke runs
    # ci/smoke_release_artifacts.py. Both are macOS-capable lanes that
    # previously inherited the image's interpreter.
    assert workflow.count('python-version: "3.13"') >= 3


def test_daemon_sidecar_is_staged_from_the_release_binary(tmp_path: Path) -> None:
    """Guard the link/copy fallback itself, not just which API it names."""
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr", b"soldr-bytes")

    stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    assert (package / "soldr-daemon").read_bytes() == b"soldr-bytes"


# ---------------------------------------------------------------------------
# Cross-target binutils selection (soldr#3085)
#
# Release run 33820395040 died in this step on three lanes because soldr#3038
# ran the HOST's GNU binutils over a foreign artifact. Reproduced by hand:
#
#   $ objcopy --only-keep-debug aarch64.elf out.debug
#   objcopy: Unable to recognise the architecture of the input file
#   $ strip -x arm64-macho
#   strip: arm64-macho: file format not recognized
#
# These tests are hermetic: they fake the host facts and a bin directory
# rather than needing a cross toolchain installed. `test_selected_tool_*`
# below is the one that touches real tools, and skips when they are absent.


@pytest.fixture
def no_managed_llvm(monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
    """Point every managed-LLVM search root at empty directories.

    Without this a developer machine that happens to have LLVM installed
    would pass the "nothing available" tests for the wrong reason.
    """
    empty = tmp_path / "empty-home"
    empty.mkdir()
    monkeypatch.delenv("SOLDR_LLVM_DIR", raising=False)
    monkeypatch.setenv("SOLDR_CACHE_DIR", str(empty))
    monkeypatch.setenv("RUSTUP_HOME", str(empty))
    monkeypatch.setattr(stage.Path, "home", classmethod(lambda cls: empty))
    monkeypatch.setenv("PATH", str(empty))
    return empty


def fake_tool_dir(directory: Path, *names: str) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    for name in names:
        tool = directory / name
        tool.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        tool.chmod(0o755)
    return directory


@pytest.mark.parametrize(
    ("machine", "target", "native"),
    [
        # The lanes that work today keep host GNU binutils, unchanged.
        ("x86_64", "x86_64-unknown-linux-gnu", True),
        ("x86_64", "x86_64-unknown-linux-musl", True),
        ("aarch64", "aarch64-unknown-linux-musl", True),
        # The three lanes that failed in run 33820395040.
        ("x86_64", "aarch64-unknown-linux-gnu", False),
        ("x86_64", "aarch64-apple-darwin", False),
        # The trap: matching architecture, foreign object format. An
        # arch-only comparison would wrongly hand this GNU strip, which is
        # exactly the "macOS x64 (Linux cross)" failure.
        ("x86_64", "x86_64-apple-darwin", False),
        ("x86_64", "x86_64-pc-windows-msvc", False),
    ],
)
def test_host_native_requires_linux_elf_and_a_matching_arch(
    monkeypatch: pytest.MonkeyPatch, machine: str, target: str, native: bool
) -> None:
    monkeypatch.setattr(stage.sys, "platform", "linux")
    monkeypatch.setattr(stage.platform, "machine", lambda: machine)

    assert stage.target_is_host_native(target) is native


def test_a_non_linux_host_is_never_treated_as_native(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(stage.sys, "platform", "darwin")
    monkeypatch.setattr(stage.platform, "machine", lambda: "arm64")

    assert stage.target_is_host_native("aarch64-unknown-linux-gnu") is False


def test_host_native_target_keeps_the_host_gnu_binutils(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(stage.sys, "platform", "linux")
    monkeypatch.setattr(stage.platform, "machine", lambda: "x86_64")

    assert stage.select_binutils("x86_64-unknown-linux-gnu") == stage.Binutils(
        "objcopy", "strip"
    )


def test_foreign_target_prefers_llvm_binutils_from_path(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, no_managed_llvm: Path
) -> None:
    """The macOS lanes' shape: soldr's managed LLVM is already first on PATH."""
    monkeypatch.setattr(stage.sys, "platform", "linux")
    monkeypatch.setattr(stage.platform, "machine", lambda: "x86_64")
    bin_dir = fake_tool_dir(tmp_path / "llvm" / "bin", "llvm-objcopy", "llvm-strip")
    monkeypatch.setenv("PATH", str(bin_dir))

    tools = stage.select_binutils("aarch64-apple-darwin")
    objcopy, strip_tool = tools.objcopy, tools.strip

    assert Path(objcopy).name == "llvm-objcopy"
    assert Path(strip_tool).name == "llvm-strip"


def test_foreign_target_finds_llvm_in_the_managed_soldr_cache(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, no_managed_llvm: Path
) -> None:
    """Not on PATH, but materialized by `soldr prepare` under SOLDR_CACHE_DIR."""
    monkeypatch.setattr(stage.sys, "platform", "linux")
    monkeypatch.setattr(stage.platform, "machine", lambda: "x86_64")
    cache = tmp_path / "setup-soldr-soldr"
    fake_tool_dir(
        cache
        / "bin"
        / "syslib"
        / "llvm-tools"
        / "20.1.7"
        / "linux-x64-gnu"
        / "package"
        / "bin",
        "llvm-objcopy",
        "llvm-strip",
    )
    monkeypatch.setenv("SOLDR_CACHE_DIR", str(cache))

    tools = stage.select_binutils("x86_64-apple-darwin")
    objcopy, strip_tool = tools.objcopy, tools.strip

    assert Path(objcopy).name == "llvm-objcopy"
    assert Path(strip_tool).name == "llvm-strip"


def test_foreign_target_finds_llvm_in_the_rustup_llvm_tools_component(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, no_managed_llvm: Path
) -> None:
    """The backstop auto-release.yml installs. RUSTUP_HOME is read from the
    environment because setup-soldr relocates it away from ~/.rustup."""
    monkeypatch.setattr(stage.sys, "platform", "linux")
    monkeypatch.setattr(stage.platform, "machine", lambda: "x86_64")
    rustup = tmp_path / "rustup-home"
    fake_tool_dir(
        rustup
        / "toolchains"
        / "1.95.0-x86_64-unknown-linux-gnu"
        / "lib"
        / "rustlib"
        / "x86_64-unknown-linux-gnu"
        / "bin",
        "llvm-objcopy",
        "llvm-strip",
    )
    monkeypatch.setenv("RUSTUP_HOME", str(rustup))

    tools = stage.select_binutils("aarch64-unknown-linux-gnu")
    objcopy, strip_tool = tools.objcopy, tools.strip

    assert Path(objcopy).name == "llvm-objcopy"
    assert Path(strip_tool).name == "llvm-strip"


def test_elf_target_falls_back_to_the_managed_cross_toolchain_binutils(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, no_managed_llvm: Path
) -> None:
    """No LLVM anywhere, but soldr's GNU/Linux bundle is prepared.

    It exports AR_<triple> as an absolute path to <bundle>/bin/<prefix>-ar, so
    the sibling objcopy/strip are derivable without hard-coding the vendor
    prefix soldr picked (`aarch64-conda-linux-gnu-`, today).
    """
    monkeypatch.setattr(stage.sys, "platform", "linux")
    monkeypatch.setattr(stage.platform, "machine", lambda: "x86_64")
    prefix = "aarch64-conda-linux-gnu"
    bundle = fake_tool_dir(
        tmp_path / "bundle" / "bin",
        f"{prefix}-ar",
        f"{prefix}-objcopy",
        f"{prefix}-strip",
    )
    monkeypatch.setenv("AR_aarch64_unknown_linux_gnu", str(bundle / f"{prefix}-ar"))

    tools = stage.select_binutils("aarch64-unknown-linux-gnu")
    objcopy, strip_tool = tools.objcopy, tools.strip

    assert Path(objcopy).name == f"{prefix}-objcopy"
    assert Path(strip_tool).name == f"{prefix}-strip"


@pytest.mark.parametrize(
    ("target", "ar_value"),
    [
        # A darwin lane's AR is a bare `llvm-ar`, not a cross bundle path --
        # deriving `llvm-objcopy` from it by string surgery would be an
        # accident, not a decision.
        ("aarch64-apple-darwin", "llvm-ar"),
        # The native musl lane exports a plain `ar`.
        ("aarch64-unknown-linux-musl", "ar"),
    ],
)
def test_cross_gnu_fallback_ignores_a_non_bundle_ar(
    monkeypatch: pytest.MonkeyPatch, target: str, ar_value: str
) -> None:
    monkeypatch.setenv(f"AR_{target.replace('-', '_')}", ar_value)

    assert stage.find_cross_gnu_tool(target, "objcopy") is None


def test_no_usable_tool_fails_loudly_naming_what_was_searched(
    monkeypatch: pytest.MonkeyPatch, no_managed_llvm: Path
) -> None:
    """Never a silent skip: a release that cannot carve symbols must stop and
    say why, not ship an unstripped binary with no sidecar beside it."""
    monkeypatch.setattr(stage.sys, "platform", "linux")
    monkeypatch.setattr(stage.platform, "machine", lambda: "x86_64")
    monkeypatch.delenv("AR_aarch64_unknown_linux_gnu", raising=False)

    with pytest.raises(stage.StagingError) as error:
        stage.select_binutils("aarch64-unknown-linux-gnu")

    message = str(error.value)
    assert "llvm-objcopy" in message
    assert "llvm-strip" in message
    assert "--objcopy/--strip-tool/--darwin-strip" in message


def test_stage_debug_symbols_auto_selects_when_no_tools_are_passed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The workflow passes no --objcopy/--strip-tool, so this is the real
    release path: the darwin lanes must reach for llvm-strip, not `strip`."""
    release = tmp_path / "release"
    package = tmp_path / "package"
    symbols = tmp_path / "symbols"
    dsym = release / "soldr.dSYM" / "Contents" / "Resources"
    dsym.mkdir(parents=True)
    write_file(release, "soldr")
    write_file(dsym, "DWARF", b"symbols")
    stage.stage_release_binaries("aarch64-apple-darwin", release, package)

    monkeypatch.setattr(
        stage,
        "select_binutils",
        lambda target: stage.Binutils("llvm-objcopy", "llvm-strip"),
    )
    calls: list[list[str]] = []
    monkeypatch.setattr(stage, "run_tool", fake_objcopy_strip(calls))

    stage.stage_debug_symbols("aarch64-apple-darwin", release, package, symbols)

    assert calls == [["llvm-strip", "-x", str(package / "soldr")]]


def test_windows_never_needs_a_tool_selection(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Selection is lazy: a target with nothing to carve must not fail on
    discovering a tool it will never run."""
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr.exe")
    write_file(release, "soldr_cli.pdb", b"pdb-bytes")
    stage.stage_release_binaries("x86_64-pc-windows-msvc", release, package)

    def explode(target: str):  # pragma: no cover - must never be reached
        raise AssertionError(f"select_binutils must not run for {target}")

    monkeypatch.setattr(stage, "select_binutils", explode)

    assert (
        stage.stage_debug_symbols(
            "x86_64-pc-windows-msvc", release, package, tmp_path / "symbols"
        )
        == []
    )


def test_a_darwin_build_without_a_dsym_never_needs_a_tool_selection(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr")
    stage.stage_release_binaries("aarch64-apple-darwin", release, package)

    def explode(target: str):  # pragma: no cover - must never be reached
        raise AssertionError(f"select_binutils must not run for {target}")

    monkeypatch.setattr(stage, "select_binutils", explode)

    assert (
        stage.stage_debug_symbols(
            "aarch64-apple-darwin", release, package, tmp_path / "symbols"
        )
        == []
    )


def build_foreign_binary(tmp_path: Path, clang_target: str, name: str) -> Path | None:
    """Compile one genuinely foreign object with clang, or None if it can't."""
    clang = shutil.which("clang")
    if clang is None:
        return None
    source = tmp_path / "probe.c"
    source.write_text("int probe(int x){return x*37+11;}\n", encoding="utf-8")
    artifact = tmp_path / name
    result = run_captured(
        [
            clang,
            f"--target={clang_target}",
            "-g",
            "-c",
            str(source),
            "-o",
            str(artifact),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    return artifact if result.returncode == 0 and artifact.is_file() else None


@pytest.mark.parametrize(
    ("clang_target", "name", "target"),
    [
        ("aarch64-linux-gnu", "foreign-aarch64.o", "aarch64-unknown-linux-gnu"),
        ("arm64-apple-macos11", "foreign-macho.o", "aarch64-apple-darwin"),
    ],
)
def test_selected_tool_reads_a_real_foreign_binary_the_host_one_rejects(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    clang_target: str,
    name: str,
    target: str,
) -> None:
    """The end-to-end claim, against real tools and a real foreign object.

    Asserts BOTH halves of soldr#3085: the host GNU tool this script used to
    hard-code genuinely cannot read the artifact, and the auto-selected one
    can. Skips rather than passes vacuously when the pieces are missing.
    """
    if shutil.which("objcopy") is None:
        pytest.skip("host GNU objcopy is required to demonstrate the failure it causes")
    artifact = build_foreign_binary(tmp_path, clang_target, name)
    if artifact is None:
        pytest.skip(f"clang cannot target {clang_target} here")

    host_attempt = run_captured(
        ["objcopy", "--only-keep-debug", str(artifact), str(tmp_path / "host.debug")],
        capture_output=True,
        text=True,
        check=False,
    )
    assert host_attempt.returncode != 0, (
        "host objcopy unexpectedly read a foreign artifact; this test can no "
        "longer demonstrate the release failure it guards"
    )

    monkeypatch.setattr(stage.sys, "platform", "linux")
    monkeypatch.setattr(stage.platform, "machine", lambda: "x86_64")
    try:
        tools = stage.select_binutils(target)
        objcopy, strip_tool = tools.objcopy, tools.strip
    except stage.StagingError:
        pytest.skip("no cross-capable binutils installed here")

    stage.run_tool(
        [objcopy, "--only-keep-debug", str(artifact), str(tmp_path / "out.debug")]
    )
    assert (tmp_path / "out.debug").is_file()
    stage.run_tool([strip_tool, "--strip-debug", str(artifact)])
