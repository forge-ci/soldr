"""The workspace crate graph and the gate's change set, shared by every lane
that skips or narrows on proof (soldr#3704 wine lane, soldr#3731 tests lane).

One implementation on purpose: a second closure walker would drift from this
one silently, and a lane would then skip work its inputs can reach.
"""

from __future__ import annotations

import subprocess
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path


def _path_deps(manifest: dict) -> set[str]:
    tables = [manifest]
    tables += [t for t in manifest.get("target", {}).values() if isinstance(t, dict)]
    names: set[str] = set()
    for table in tables:
        for key in ("dependencies", "dev-dependencies", "build-dependencies"):
            for name, spec in table.get(key, {}).items():
                if isinstance(spec, dict) and ("path" in spec or spec.get("workspace")):
                    names.add(spec.get("package", name))
    return names


@dataclass(frozen=True)
class Crate:
    name: str
    deps: frozenset[str]


def workspace_crates(root: Path) -> list[Crate]:
    """Each crates/<dir> package and the names it depends on (normal, dev and
    build deps, every target table)."""
    crates: list[Crate] = []
    for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
        name = data.get("package", {}).get("name")
        if name != manifest.parent.name:
            raise ValueError(f"{manifest}: package name {name!r} != directory")
        crates.append(Crate(name, frozenset(_path_deps(data))))
    return crates


def dependency_closure(root: Path, seeds: tuple[str, ...]) -> set[str]:
    """The seeds and every workspace crate they depend on, transitively."""
    crates = {c.name: c.deps for c in workspace_crates(root)}
    closure: set[str] = set()
    todo = list(seeds)
    while todo:
        name = todo.pop()
        if name in closure:
            continue
        closure.add(name)
        todo += [d for d in crates.get(name, ()) if d in crates]
    return closure


def reverse_dependency_closure(root: Path, seeds: tuple[str, ...]) -> set[str]:
    """The seeds and every workspace crate whose dependency closure holds one."""
    names = [c.name for c in workspace_crates(root)]
    return {n for n in names if dependency_closure(root, (n,)) & set(seeds)}


def _git(root: Path, *args: str) -> str:
    """git stdout via a temporary file (PY-003: no pipe capture).

    `safe.directory` lets the bosn container (root) read the host user's
    checkout; `--no-optional-locks` keeps `status` from rewriting the index
    as root.
    """
    argv = ["git", "-c", "safe.directory=*", "--no-optional-locks", *args]
    with tempfile.TemporaryFile() as out:
        subprocess.run(argv, cwd=root, stdout=out, check=True)
        out.seek(0)
        return out.read().decode("utf-8")


def changed_paths(root: Path) -> list[str] | None:
    """Paths changed between merge-base(origin/main, HEAD) and HEAD, or None
    when that is undeterminable or the tree is dirty."""
    try:
        base = _git(root, "merge-base", "origin/main", "HEAD").strip()
        out = _git(root, "diff", "--name-only", "--no-renames", base, "HEAD")
        dirty = _git(root, "status", "--porcelain")
    except (OSError, subprocess.CalledProcessError):
        return None
    if dirty.strip():
        return None
    return [ln for ln in out.splitlines() if ln]
