"""`install.sh` must actually run (soldr#2131).

The installer is the one script users are told to execute, and no workflow ever
ran it -- it appears in the CI config only inside `paths:` triggers. So a bash
4.0 builtin sat in it undetected: `readarray`, which macOS does not have,
because macOS ships bash 3.2 and always will. On a stock Mac the installer
failed with `readarray: command not found` and installed nothing, while every
check stayed green (soldr#2130).

The obvious cheap test does NOT cover this. `--help` exits at the argument
parser, roughly a hundred lines above where the bug was, and `bash -n` cannot
see it either: an unavailable builtin is a runtime lookup failure, not a syntax
error. A test built on those would have gone green while the installer stayed
broken, which is worse than no test.

So these stub `curl` on PATH and let the script run its real code path all the
way through parsing the release JSON, extracting the archive and installing the
binary. No network, no GitHub, and nothing installed outside a temp directory.
On a macOS runner it executes under the real /bin/bash 3.2, which is the
environment that matters.
"""

from __future__ import annotations

import json
import os
import shutil
import stat
import sys
import tarfile
import zipfile
from dataclasses import dataclass
from pathlib import Path

import pytest

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
INSTALL_SH = REPO_ROOT / "install.sh"

# Every target `detect_target()` can produce. Offering all of them means the
# fixture matches whichever runner this lands on without the test duplicating
# the uname mapping -- duplicated detection logic would drift from the script
# it is meant to be testing.
TARGETS = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
]

TAG = "v9.9.9"


def _usable_bash() -> "str | None":
    """A bash that can actually run a script, not merely one on PATH.

    On Windows `shutil.which("bash")` finds WSL's bash, which fails with
    `execvpe(/bin/bash): No such file or directory` when no distro is
    installed. Probing beats presence: the point is to skip cleanly where the
    installer cannot run, not to fail there.
    """

    candidates = [shutil.which("bash"), "/bin/bash", "/usr/bin/bash"]
    for candidate in candidates:
        if not candidate:
            continue
        try:
            probe = run_captured(
                [candidate, "-c", "printf ok"],
                capture_output=True,
                text=True,
                timeout=30,
            )
        except OSError:
            continue
        if probe.returncode == 0 and probe.stdout.strip() == "ok":
            return candidate
    return None


BASH = _usable_bash()

needs_bash = pytest.mark.skipif(
    BASH is None, reason="no bash that can execute a script (install.sh needs one)"
)


def _write_executable(path: Path, body: str) -> None:
    path.write_text(body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


@dataclass(frozen=True)
class Archives:
    """The stand-in release archives, one per supported extension."""

    targz: Path
    zip: Path


def _make_archives(tmp: Path) -> Archives:
    """A tar.gz and a zip, each holding a stand-in `soldr` binary.

    Both, because `detect_target()` picks `zip` on Windows and `tar.gz`
    elsewhere, and the same fixture has to serve whichever runner it lands on.
    """
    payload = tmp / "payload"
    payload.mkdir()
    for name in ("soldr", "soldr.exe"):
        _write_executable(payload / name, "#!/bin/sh\necho soldr 9.9.9\n")

    targz = tmp / "asset.tar.gz"
    with tarfile.open(targz, "w:gz") as archive:
        for name in ("soldr", "soldr.exe"):
            archive.add(payload / name, arcname=name)

    zipped = tmp / "asset.zip"
    with zipfile.ZipFile(zipped, "w") as archive:
        for name in ("soldr", "soldr.exe"):
            archive.write(payload / name, arcname=name)

    return Archives(targz, zipped)


def _release_json(with_assets: bool) -> str:
    assets = []
    if with_assets:
        for target in TARGETS:
            ext = "zip" if "windows" in target else "tar.gz"
            assets.append(
                {
                    "name": f"soldr-{target}.{ext}",
                    "browser_download_url": f"https://example.invalid/soldr-{target}.{ext}",
                }
            )
    return json.dumps({"tag_name": TAG, "assets": assets})


def _stub_curl(bin_dir: Path, release_json: str, archives: Archives) -> None:
    """Serve the API call and the asset download from disk.

    The script calls curl exactly twice: once for release metadata, once for
    the asset. Dispatching on the URL keeps the stub honest about which is
    which rather than counting invocations.
    """
    script = f"""#!/bin/sh
set -eu
# Dispatch on the -o destination, not the URL: the download call is the only
# one that writes a file, and the extension there is exactly what install.sh
# decided it needs. Matching on the URL made a wrong-archive copy look like a
# corrupt zip instead of a stub bug.
out=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-o" ]; then out="$arg"; fi
  prev="$arg"
done

if [ -z "$out" ]; then
  cat <<'RELEASE_JSON_EOF'
{release_json}
RELEASE_JSON_EOF
  exit 0
fi

case "$out" in
  *.zip) src={archives.zip.as_posix()!r} ;;
  *)     src={archives.targz.as_posix()!r} ;;
esac
cp "$src" "$out"
"""
    _write_executable(bin_dir / "curl", script)


def _run_install(tmp: Path, *, with_assets: bool, extra_args: list[str] | None = None):
    stub_bin = tmp / "stub-bin"
    stub_bin.mkdir()
    archives = _make_archives(tmp)
    _stub_curl(stub_bin, _release_json(with_assets), archives)

    # `needs_bash` guarantees this; stated so the type checker sees it too.
    assert BASH is not None

    install_dir = tmp / "target-bin"
    env = dict(os.environ)
    env["PATH"] = f"{stub_bin}{os.pathsep}{env['PATH']}"
    env.pop("SOLDR_INSTALL_DIR", None)

    return (
        run_captured(
            [BASH, str(INSTALL_SH), "--bin-dir", str(install_dir)] + (extra_args or []),
            capture_output=True,
            text=True,
            env=env,
            timeout=120,
        ),
        install_dir,
    )


@needs_bash
def test_installs_the_binary_from_a_release_asset(tmp_path: Path) -> None:
    """The path the bug was on: parse the release, extract, install.

    This is the assertion that would have failed on macOS before soldr#2130 --
    the read of the release JSON into an array sits on this path.
    """
    result, install_dir = _run_install(tmp_path, with_assets=True)

    assert result.returncode == 0, (
        f"install.sh failed\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    )
    installed = [p.name for p in install_dir.iterdir()] if install_dir.exists() else []
    assert installed, f"nothing installed into {install_dir}\nstdout:\n{result.stdout}"
    assert TAG in result.stdout, (
        f"install should report the tag it used: {result.stdout}"
    )


@needs_bash
def test_reports_when_no_asset_matches(tmp_path: Path) -> None:
    """The branch immediately after the one that was broken.

    With no matching asset the script falls back to pip. The fallback itself is
    not exercised -- installing from PyPI in a unit test would be absurd -- but
    reaching the message proves the empty-array case is handled rather than
    crashing on an unbound array, which is exactly what a careless conversion
    away from `readarray` would cause.
    """
    result, _ = _run_install(tmp_path, with_assets=False)

    assert "no release asset found" in result.stderr, (
        f"expected the fallback message\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    )


@needs_bash
def test_help_exits_zero(tmp_path: Path) -> None:
    """Cheap, and deliberately labelled as not covering much.

    `--help` returns at the argument parser, so it proves the script parses and
    starts -- nothing about the download path. It is recorded here so nobody
    mistakes a green `--help` check for installer coverage, which is the
    mistake this file exists to correct.
    """
    assert BASH is not None
    result = run_captured(
        [BASH, str(INSTALL_SH), "--help"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    assert "SOLDR_INSTALL_DIR" in result.stdout + result.stderr
