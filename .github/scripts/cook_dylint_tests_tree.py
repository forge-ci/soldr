#!/usr/bin/env python3
"""Lockfile-closure guard for the Dylint UI-test dependency layer (soldr#3159).

`soldr ci-test` cooks the `target/dylint/tests` third-party layer
(`dylint_testing` -> `compiletest_rs` / `git2` / `libgit2-sys`, plus
`dylint`'s own build script) as its `dylint-cook-*` stages (soldr#3460;
before that, this script drove the cook as a serial workflow step,
soldr#3042). All seven lint crates share that one tree, so it only stays one
layer while their lockfiles resolve the same third-party closure. This module
owns that definition; `tests/test_cook_dylint_tests_tree.py` enforces it.
"""

from __future__ import annotations

import tomllib
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class LockedPackage:
    """One resolved `[[package]]` entry of a lint crate's lockfile."""

    name: str
    version: str
    checksum: str | None


def lint_roots(repo_root: Path, lints_dir: str = "dylints") -> list[Path]:
    """Every immediate subdirectory of `<repo_root>/<lints_dir>` with a `Cargo.toml`.

    Each lint crate is its OWN cargo workspace with its own `Cargo.lock` (see
    `dylints/*/Cargo.toml`, which carries a bare `[workspace]`), which is why
    the cook runs once per crate rather than once at the repo root.
    """
    base = repo_root / lints_dir
    if not base.is_dir():
        return []
    return sorted(
        entry
        for entry in base.iterdir()
        if entry.is_dir() and (entry / "Cargo.toml").is_file()
    )


def dependency_closure(lint_root: Path) -> frozenset[LockedPackage]:
    """The `(name, version, checksum)` set a lint crate's lockfile resolves to.

    The crate's own entry is excluded: it is the one package guaranteed to
    differ between the six, and it contributes nothing to the third-party
    layer this script cooks.
    """
    lock = tomllib.loads((lint_root / "Cargo.lock").read_text(encoding="utf-8"))
    return frozenset(
        LockedPackage(package["name"], package["version"], package.get("checksum"))
        for package in lock.get("package", ())
        if package["name"] != lint_root.name
    )


def diverging_closures(roots: Sequence[Path]) -> list[str]:
    """Lint crates whose dependency closure differs from the first root's.

    The six crates are separate cargo workspaces with separate lockfiles and
    nothing keeping them in step, so they drift apart silently: before
    soldr#3159 twelve shared deps had diverged (`cc` alone at 1.4.0, 1.4.1,
    1.4.2 and 1.4.4), which made the shared tests tree compile four copies of
    `cc` and two of `thiserror` for no reason.

    `tests/test_cook_dylint_tests_tree.py` calls THIS function rather than
    re-deriving the closure, so the guard and the definition cannot drift
    apart the way the lockfiles did.
    """
    if not roots:
        return []
    reference = dependency_closure(roots[0])
    return [root.name for root in roots[1:] if dependency_closure(root) != reference]
