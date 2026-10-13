#!/usr/bin/env python3
"""Resolve the Dylint nightly channel + pinned Dylint version for CI caching.

soldr#2349 adds three cache keys to `_build-and-test.yml` that all need to
agree on the exact Dylint nightly (`dylints/*/rust-toolchain.toml`'s
`[toolchain].channel`, e.g. `nightly-2026-05-28`) and the exact cargo-dylint
release (`known_tools.rs`'s `pinned_version`, e.g. `6.0.3`):

* the new `~/.rustup/toolchains/<channel>-<host>` cache,
* the dylint driver + prepared-plan-marker cache, and
* the `dylint-toolchain:` / `cargo-dylint-version:` / `dylint-link-version:`
  inputs already passed to `setup-soldr`.

CLAUDE.md is explicit that this value must never be hard-coded a second time:
"The Dylint nightly is declared by the lint libraries, never derived"
(soldr#2945) -- soldr used to derive it from the stable channel and shipped
broken on every host while CI stayed green, because nothing in CI actually ran
`soldr dylint`. A workflow YAML literal that silently drifted from the
libraries would be the exact same failure shape one level up the stack: CI
would keep restoring/installing whatever nightly this file names, the
`ci-test` Dylint plan would resolve a *different* one from the libraries
themselves, and the cache/pre-install would simply go to waste every run
without ever turning red.

This is a thin CLI wrapper around `check_dylint_driver_assets.library_nightly`
/ `pinned_dylint_version` -- those two functions already implement "read the
libraries, verify they agree, canonicalize" and "read known_tools.rs, verify
the two Dylint crate pins agree". Reusing them (rather than re-parsing the
same files a second way) is what CLAUDE.md's "N implementations of one idea"
rule is asking for.

Emits `channel` and `dylint_version` to `$GITHUB_OUTPUT` (for a step with an
`id:`) and to `$GITHUB_ENV` (for later steps that prefer an env var), when
those files are set. Always prints both to stdout, so it is runnable locally
with neither set.

Usage:
    python3 .github/scripts/dylint_toolchain_channel.py
"""

from __future__ import annotations

import os
import pathlib
import sys
from dataclasses import dataclass

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

# The sibling import must follow the `sys.path` insert above, so both flake8's
# E402 and pylint's wrong-import-position are suppressed deliberately here
# rather than by duplicating `check_dylint_driver_assets`'s readers (soldr#2740:
# one parser per concept).
from check_dylint_driver_assets import (  # noqa: E402  # pylint: disable=wrong-import-position
    KNOWN_TOOLS_RELATIVE,
    GuardError,
    dylint_library_manifests,
    library_nightly,
    pinned_dylint_version,
)


@dataclass(frozen=True)
class DylintToolchain:
    """The nightly channel and Dylint version the lint libraries pin."""

    channel: str
    dylint_version: str


def resolve(repo_root: pathlib.Path) -> DylintToolchain:
    """The channel and Dylint version, e.g. `nightly-2026-05-28` / `6.0.3`."""
    manifests = {
        path.relative_to(repo_root).as_posix(): path.read_text(encoding="utf-8")
        for path in dylint_library_manifests(repo_root)
    }
    channel = library_nightly(manifests)
    known_tools_text = (repo_root / KNOWN_TOOLS_RELATIVE).read_text(encoding="utf-8")
    dylint_version = pinned_dylint_version(known_tools_text)
    return DylintToolchain(channel, dylint_version)


def emit(name: str, value: str) -> None:
    """Print `name=value` and append it to `$GITHUB_OUTPUT` / `$GITHUB_ENV`
    when those files are set (both, so a workflow step can read either
    `steps.<id>.outputs.<name>` or `env.<NAME>` — see the module docstring)."""
    print(f"{name}={value}")
    for env_var_name in ("GITHUB_OUTPUT", "GITHUB_ENV"):
        target = os.environ.get(env_var_name)
        if not target:
            continue
        with pathlib.Path(target).open("a", encoding="utf-8") as handle:
            handle.write(f"{name}={value}\n")


def main(argv: list[str] | None = None) -> int:
    del argv
    try:
        toolchain = resolve(REPO_ROOT)
    except GuardError as error:
        print(f"dylint_toolchain_channel: {error}", file=sys.stderr)
        return 1
    except OSError as error:
        print(
            f"dylint_toolchain_channel: could not read a Dylint pin: {error}",
            file=sys.stderr,
        )
        return 1

    emit("channel", toolchain.channel)
    emit("dylint_version", toolchain.dylint_version)
    return 0


if __name__ == "__main__":
    sys.exit(main())
