#!/usr/bin/env python3
"""Prove the PyPI ``soldr`` Dylint path works on a fresh Windows host.

The source tree can be correct while the last released wheel is still broken
(soldr#2972).  This deliberately installs PyPI's exact binary distribution
into a fresh virtualenv and gives Soldr, Cargo, and Rustup private homes before
asking the published executable to prepare and run one Dylint library against
a disposable one-crate package.  The nightly comes from the same lint-library
helper that validates published driver assets; it is not derived from the
stable compiler.

Without ``--expected-version`` the script monitors PyPI's latest release.
Release automation supplies its just-published exact version instead.
"""

from __future__ import annotations

import argparse
import importlib
import json
import os
import subprocess
import sys
import time
import urllib.request
from collections.abc import Callable, Mapping
from pathlib import Path
from types import MappingProxyType
from typing import Protocol, cast

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))


class DriverAssetHelpers(Protocol):
    """The narrow, shared driver-asset API this smoke is allowed to use."""

    def dylint_library_manifests(self, repo_root: Path) -> list[Path]: ...

    def library_nightly(self, manifests: dict[str, str]) -> str: ...


driver_assets = cast(
    DriverAssetHelpers, importlib.import_module("check_dylint_driver_assets")
)

PYPI_PROJECT = "soldr"
PYPI_JSON = f"https://pypi.org/pypi/{PYPI_PROJECT}/json"
PRESERVED_SOLDR_ENV = {"SOLDR_GITHUB_TOKEN"}
SCRUB_PREFIXES = ("CARGO_", "DYLINT_", "RUSTUP_", "RUSTC_", "ZCCACHE_")
SCRUB_EXACT = {"CARGO", "RUSTC", "RUSTDOC", "RUSTFLAGS", "RUSTDOCFLAGS"}
PROBE_LINT = "ban_raw_env_flag"


class PublishedDylintSmokeError(RuntimeError):
    """The installed PyPI package failed its released-Dylint contract."""


def normalized_version(version: str) -> str:
    return version.strip().removeprefix("v")


def fetch_pypi_bytes(url: str) -> bytes:
    """Fetch one PyPI JSON document without exposing urllib's handle type."""

    with urllib.request.urlopen(url, timeout=30) as response:
        return response.read()


def latest_pypi_version(fetch: Callable[[str], bytes] = fetch_pypi_bytes) -> str:
    """Return PyPI's authoritative current version, not a local package pin."""

    try:
        payload = json.loads(fetch(PYPI_JSON).decode("utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PublishedDylintSmokeError(
            f"could not query latest {PYPI_PROJECT} on PyPI: {exc}"
        ) from exc
    version = (
        (payload.get("info") or {}).get("version")
        if isinstance(payload, dict)
        else None
    )
    if not isinstance(version, str) or not normalized_version(version):
        raise PublishedDylintSmokeError(
            f"PyPI response has no latest {PYPI_PROJECT} version"
        )
    return normalized_version(version)


def selected_version(
    expected_version: str | None, fetch: Callable[[str], bytes] = fetch_pypi_bytes
) -> str:
    """Use an explicit release version, or intentionally monitor PyPI latest."""

    if expected_version and expected_version.strip():
        return normalized_version(expected_version)
    return latest_pypi_version(fetch)


def authoritative_channel(repo_root: Path) -> str:
    """Read the one Dylint nightly from the established driver-asset helper."""

    manifests = driver_assets.dylint_library_manifests(repo_root)
    return driver_assets.library_nightly(
        {
            path.relative_to(repo_root).as_posix(): path.read_text(encoding="utf-8")
            for path in manifests
        }
    )


def published_console_script(venv: Path) -> Path:
    """The smoke must execute the entry point installed in its own venv."""

    candidates = [venv / "Scripts" / "soldr.exe", venv / "bin" / "soldr"]
    found = next((candidate for candidate in candidates if candidate.is_file()), None)
    if found is None:
        raise PublishedDylintSmokeError(f"PyPI install did not create soldr in {venv}")
    return found


def isolated_environment(root: Path) -> MappingProxyType[str, str]:
    """Fresh, inspectable state for this one published-binary proof."""

    home = root / "home"
    inherited_cargo_home = Path(
        os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))
    ).resolve()
    inherited_cargo_bin = str(inherited_cargo_home / "bin").casefold()
    environment = os.environ.copy()
    # This must inspect the *published* binary's own resolution. Scrub every
    # Soldr setting except its authentication token, every Dylint override and
    # prepared marker, and every cargo/rustup/wrapper steering variable before
    # assigning this probe's private roots below. In particular, an inherited
    # `SOLDR_DYLINT_PREPARED_IDENTITY` or `DYLINT_DRIVER_PATH` could otherwise
    # convert this fresh-host test into a false green.
    for name in list(environment):
        if (
            (name.startswith("SOLDR_") and name not in PRESERVED_SOLDR_ENV)
            or name.startswith(SCRUB_PREFIXES)
            or name in SCRUB_EXACT
        ):
            environment.pop(name, None)
    environment.update(
        {
            "HOME": str(home),
            "USERPROFILE": str(home),
            "CARGO_HOME": str(root / "cargo-home"),
            "RUSTUP_HOME": str(root / "rustup-home"),
            "SOLDR_CACHE_DIR": str(root / "soldr-cache"),
            "UV_CACHE_DIR": str(root / "uv-cache"),
        }
    )
    # CARGO_HOME controls where cargo looks, but PATH wins for fetched tools.
    # Keep the runner's ordinary system paths (including a rustup bootstrap if
    # it is installed there) while removing its *old* cargo bin. Otherwise an
    # ambient dylint-link.exe can turn this supposed fresh-wheel proof into a
    # probe of a stale developer install.
    environment["PATH"] = os.pathsep.join(
        entry
        for entry in environment.get("PATH", "").split(os.pathsep)
        if str(Path(entry).resolve()).casefold() != inherited_cargo_bin
    )
    return MappingProxyType(environment)


def run(
    command: list[str],
    *,
    env: Mapping[str, str],
    cwd: Path,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a probe with output retained in the failure explaining it.

    ``timeout`` bounds the command so a stall surfaces as a named failure
    instead of a silent job-level timeout. The job cap (60 min) minus the
    healthy baseline (~23 min on a cold Windows host) is the budget the
    per-phase bounds below are sized from.
    """

    try:
        completed = run_captured(
            command,
            cwd=cwd,
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        output = "\n".join(
            part
            for part in (
                (
                    exc.stdout.decode(errors="replace").strip()
                    if isinstance(exc.stdout, bytes)
                    else (exc.stdout or "").strip()
                ),
                (
                    exc.stderr.decode(errors="replace").strip()
                    if isinstance(exc.stderr, bytes)
                    else (exc.stderr or "").strip()
                ),
            )
            if part
        )
        raise PublishedDylintSmokeError(
            f"published Dylint probe timed out after {timeout}s "
            f"({' '.join(command)}):\n{output or 'no output'}"
        ) from exc
    if completed.returncode:
        output = "\n".join(
            part
            for part in (completed.stdout.strip(), completed.stderr.strip())
            if part
        )
        raise PublishedDylintSmokeError(
            f"published Dylint probe failed ({' '.join(command)}):\n{output or 'no output'}"
        )
    return completed


def installed_version(soldr: Path, *, env: Mapping[str, str], cwd: Path) -> str:
    output = run(
        [str(soldr), "version", "--json"], env=env, cwd=cwd, timeout=120
    ).stdout
    try:
        payload = json.loads(output)
    except json.JSONDecodeError as exc:
        raise PublishedDylintSmokeError(
            f"published soldr version --json was invalid: {output!r}"
        ) from exc
    version = payload.get("soldr_version") if isinstance(payload, dict) else None
    if not isinstance(version, str):
        raise PublishedDylintSmokeError(
            "published soldr version --json omitted soldr_version"
        )
    return normalized_version(version)


def minimal_probe_manifest(state_root: Path) -> Path:
    """Create the tiny checked crate so the smoke never builds Soldr itself."""

    probe = state_root / "driver-probe"
    source = probe / "src"
    source.mkdir(parents=True, exist_ok=True)
    manifest = probe / "Cargo.toml"
    manifest.write_text(
        "[package]\n"
        'name = "soldr_published_dylint_probe"\n'
        'version = "0.0.0"\n'
        'edition = "2021"\n'
        "\n[workspace]\n",
        encoding="utf-8",
    )
    (source / "lib.rs").write_text("pub fn probe() {}\n", encoding="utf-8")
    return manifest


def driver_probe_command(soldr: Path, *, repo_root: Path, manifest: Path) -> list[str]:
    """Run one lint on the disposable crate through the published front door.

    cargo-dylint 6 removed the old ``--list`` option.  Its ``list`` subcommand
    only discovers libraries (and reports them ``<unbuilt>``), so it cannot
    prove that Soldr's prepared driver loaded.  A selected library plus an
    explicit small manifest both executes that driver and avoids checking this
    repository's workspace.
    """

    return [
        str(soldr),
        "dylint",
        "--manifest-path",
        str(manifest),
        "--path",
        str(repo_root / "dylints"),
        "--pattern",
        PROBE_LINT,
    ]


PYPI_SIMPLE_INDEX = "https://pypi.org/simple"

# soldr#3834: how long to keep asking PyPI for a version that was uploaded
# moments ago before calling the release broken. The smoke starts seconds
# after the upload job, and the simple index can lag the upload.
INSTALL_ATTEMPTS = 6
INSTALL_RETRY_SECONDS = 30.0

# uv's wording for "this exact version is not (yet) resolvable".
_NOT_YET_RESOLVABLE = ("No solution found", "there is no version of")


def published_install_command(venv: Path, version: str) -> list[str]:
    """The exact ``uv pip install`` of one published PyPI version.

    soldr#3834: ``--no-config`` keeps uv from discovering any ``uv.toml`` /
    ``pyproject.toml`` (the repository's, or a runner's user-level config)
    that could change resolution, ``--refresh`` bypasses cached index data,
    and the index is named explicitly: this smoke proves what PyPI serves.
    """

    return [
        "uv",
        "pip",
        "install",
        "--python",
        str(venv),
        "--no-config",
        "--refresh",
        "--index-url",
        PYPI_SIMPLE_INDEX,
        "--only-binary=:all:",
        f"soldr=={version}",
    ]


def install_published(
    *,
    venv: Path,
    version: str,
    env: Mapping[str, str],
    cwd: Path,
    sleep: Callable[[float], None] = time.sleep,
) -> None:
    """Install ``soldr==version`` from PyPI, waiting out index propagation.

    Only the "not resolvable yet" failure is retried; any other install error
    is reported on the first attempt.
    """

    command = published_install_command(venv, version)
    for attempt in range(1, INSTALL_ATTEMPTS + 1):
        try:
            run(command, env=env, cwd=cwd, timeout=600)
            return
        except PublishedDylintSmokeError as error:
            message = str(error)
            if attempt == INSTALL_ATTEMPTS or not any(
                marker in message for marker in _NOT_YET_RESOLVABLE
            ):
                raise
            print(
                f"soldr=={version} not resolvable from PyPI yet "
                f"(attempt {attempt}/{INSTALL_ATTEMPTS}); retrying in "
                f"{INSTALL_RETRY_SECONDS:.0f}s",
                file=sys.stderr,
            )
            sleep(INSTALL_RETRY_SECONDS)


def smoke(*, version: str, repo_root: Path, venv: Path, state_root: Path) -> None:
    """Install exactly one PyPI version and exercise its real Dylint boundary."""

    expected_channel = authoritative_channel(repo_root)
    env = isolated_environment(state_root)
    run(["uv", "venv", "--clear", str(venv)], env=env, cwd=repo_root, timeout=300)
    install_published(venv=venv, version=version, env=env, cwd=repo_root)
    soldr = published_console_script(venv)
    try:
        _exercise(
            soldr,
            version=version,
            expected_channel=expected_channel,
            env=env,
            repo_root=repo_root,
            state_root=state_root,
        )
    finally:
        stop_private_services(soldr, env=env, cwd=repo_root)


def teardown_commands(soldr: Path) -> list[list[str]]:
    """Stop the daemon route, then the broker, the smoke started (soldr#3868)."""

    return [[str(soldr), "daemon", "stop"], [str(soldr), "broker", "stop"]]


def stop_private_services(soldr: Path, *, env: Mapping[str, str], cwd: Path) -> None:
    """Best-effort: never leave this probe's private broker/daemon running.

    soldr#3868: a passing run left both alive under the private HOME. On an
    ephemeral runner that is only waste, but a lingering child that inherited
    the step's handles is a candidate for a Windows wait beyond every phase
    bound. A failed stop is reported, never raised: teardown must not mask the
    probe's own verdict.
    """

    for command in teardown_commands(soldr):
        try:
            completed = run_captured(
                command,
                cwd=cwd,
                env=env,
                text=True,
                capture_output=True,
                check=False,
                timeout=120,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            print(f"teardown {' '.join(command[1:])} failed: {exc}", file=sys.stderr)
            continue
        if completed.returncode:
            print(
                f"teardown {' '.join(command[1:])} exited {completed.returncode}: "
                f"{(completed.stderr or completed.stdout).strip()}",
                file=sys.stderr,
            )


def _exercise(
    soldr: Path,
    *,
    version: str,
    expected_channel: str,
    env: Mapping[str, str],
    repo_root: Path,
    state_root: Path,
) -> None:
    """Version, prepare and probe checks against the installed binary."""

    observed_version = installed_version(soldr, env=env, cwd=repo_root)
    if observed_version != version:
        raise PublishedDylintSmokeError(
            f"published binary provenance mismatch: requested soldr=={version}, got {observed_version} at {soldr}"
        )

    prepared = run(
        [str(soldr), "dylint", "prepare"], env=env, cwd=repo_root, timeout=900
    )
    prepared_output = prepared.stdout + prepared.stderr
    if expected_channel not in prepared_output:
        raise PublishedDylintSmokeError(
            f"published soldr dylint prepare did not report authoritative channel {expected_channel}:\n{prepared_output}"
        )
    manifest = minimal_probe_manifest(state_root)
    run(
        driver_probe_command(soldr, repo_root=repo_root, manifest=manifest),
        env=env,
        cwd=repo_root,
        timeout=1500,
    )
    print(
        f"published Dylint smoke passed: soldr {version}, channel {expected_channel}, binary {soldr}"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--expected-version",
        default="",
        help="exact PyPI version; empty monitors latest",
    )
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--venv", type=Path, default=Path(".published-dylint-venv"))
    parser.add_argument(
        "--state-root", type=Path, default=Path(".published-dylint-state")
    )
    args = parser.parse_args(argv)
    try:
        smoke(
            version=selected_version(args.expected_version),
            repo_root=args.repo_root.resolve(),
            venv=args.venv.resolve(),
            state_root=args.state_root.resolve(),
        )
    except (
        OSError,
        subprocess.SubprocessError,
        PublishedDylintSmokeError,
        RuntimeError,
    ) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
