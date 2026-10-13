#!/usr/bin/env python3
"""Every direct third-party dependency must be in the inventory (soldr#2752).

soldr#2752 proposes routing third-party crates through a `soldr-deps` gateway,
enforced at two levels: a manifest rule (only the gateway may declare
third-party dependencies) and a source rule (a Dylint over `use` paths). Its
own recommendation is to land the manifest half first, because it needs no
facade, no source churn, and no lint -- and because, quoting the issue:

    Even without the facade, a manifest inventory that must be explicitly
    amended is a real ratchet.

That is what this is. It does not move any dependency and does not require the
gateway to exist. It makes *adding* one a deliberate, reviewed act rather than
a line in a manifest nobody diffs, which is the property the gateway is
ultimately after.

Two directions, like `loc_ratchet.py`:

  * a crate may not declare a third-party dependency the inventory does not
    list -- no new edges by accident;
  * the inventory may not list one the crate no longer declares -- so it
    shrinks with the surface instead of ossifying into folklore.

Scope: normal dependencies, including `[target.'cfg(...)'.dependencies]`.
Dev- and build-dependencies are deliberately out: soldr#2752's rule is about
*normal* third-party dependencies, which are what ship and what the gateway
would own. A test fixture pulling in `tempfile` is not the boundary this
guards.

Usage:
    python .github/scripts/check_dependency_inventory.py
    python .github/scripts/check_dependency_inventory.py --write
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path

try:
    import tomllib as _toml  # 3.11+
except ImportError:  # pragma: no cover -- older Pythons
    try:
        import tomli as _toml  # type: ignore[import,no-redef]
    except ImportError:
        sys.stderr.write(
            "check_dependency_inventory.py: needs Python 3.11+ (tomllib) "
            "or `pip install tomli`\n"
        )
        sys.exit(2)

REPO_ROOT = Path(__file__).resolve().parents[2]
CRATES_DIR = REPO_ROOT / "crates"
INVENTORY_PATH = REPO_ROOT / "ci" / "dependency-inventory.json"

# Workspace-internal crates. These are the edges the gateway would never own,
# so they are not third-party and are not inventoried.
INTERNAL_PREFIX = "soldr-"


def dependency_tables(manifest: dict) -> list[dict]:
    """Normal dependency tables, including per-target ones."""
    tables = [manifest.get("dependencies") or {}]
    for target_cfg in (manifest.get("target") or {}).values():
        tables.append(target_cfg.get("dependencies") or {})
    return tables


def third_party_dependencies(manifest_path: Path) -> list[str]:
    manifest = _toml.loads(manifest_path.read_text(encoding="utf-8"))
    names: set[str] = set()
    for table in dependency_tables(manifest):
        for name in table:
            if not name.startswith(INTERNAL_PREFIX):
                names.add(name)
    return sorted(names)


@dataclass(frozen=True)
class CrateDependencies:
    """The third-party dependencies one workspace crate declares."""

    crate: str
    dependencies: tuple[str, ...]


@dataclass(frozen=True)
class DependencyEdge:
    """One (crate, third-party dependency) pair."""

    crate: str
    dependency: str


@dataclass(frozen=True)
class InventoryDiff:
    """Edges the manifests declare but the inventory lacks, and vice versa."""

    added: tuple[DependencyEdge, ...]
    removed: tuple[DependencyEdge, ...]


def observed_surface() -> tuple[CrateDependencies, ...]:
    surface: list[CrateDependencies] = []
    for crate_dir in sorted(CRATES_DIR.iterdir()):
        manifest = crate_dir / "Cargo.toml"
        if manifest.is_file():
            surface.append(
                CrateDependencies(
                    crate_dir.name, tuple(third_party_dependencies(manifest))
                )
            )
    return tuple(surface)


def load_inventory() -> tuple[CrateDependencies, ...]:
    data = json.loads(INVENTORY_PATH.read_text(encoding="utf-8"))
    return tuple(
        CrateDependencies(crate, tuple(deps)) for crate, deps in data["crates"].items()
    )


def write_inventory(surface: tuple[CrateDependencies, ...]) -> None:
    payload = {
        "schema_version": 1,
        "comment": (
            "Direct third-party dependencies per workspace crate (soldr#2752 "
            "Rule A). Regenerate with "
            "`python .github/scripts/check_dependency_inventory.py --write`, "
            "and expect the diff to be reviewed: adding an entry is adding a "
            "dependency."
        ),
        "crates": {entry.crate: list(entry.dependencies) for entry in surface},
    }
    INVENTORY_PATH.write_text(
        json.dumps(payload, indent=2, sort_keys=False) + "\n", encoding="utf-8"
    )


def diff(
    surface: tuple[CrateDependencies, ...], inventory: tuple[CrateDependencies, ...]
) -> InventoryDiff:
    """The edges added to and removed from the manifests since the inventory."""
    observed = {entry.crate: set(entry.dependencies) for entry in surface}
    listed_by_crate = {entry.crate: set(entry.dependencies) for entry in inventory}
    added: list[DependencyEdge] = []
    removed: list[DependencyEdge] = []
    for crate in sorted(set(observed) | set(listed_by_crate)):
        have = observed.get(crate, set())
        listed = listed_by_crate.get(crate, set())
        added.extend(DependencyEdge(crate, dep) for dep in sorted(have - listed))
        removed.extend(DependencyEdge(crate, dep) for dep in sorted(listed - have))
    return InventoryDiff(tuple(added), tuple(removed))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--write",
        action="store_true",
        help="rewrite the inventory from the manifests",
    )
    args = parser.parse_args()

    surface = observed_surface()
    if args.write:
        write_inventory(surface)
        total = sum(len(entry.dependencies) for entry in surface)
        print(
            f"dependency inventory: wrote {total} third-party edge(s) across "
            f"{len(surface)} crate(s)"
        )
        return 0

    if not INVENTORY_PATH.is_file():
        print(f"dependency inventory: missing {INVENTORY_PATH}", file=sys.stderr)
        return 1

    changes = diff(surface, load_inventory())
    if not changes.added and not changes.removed:
        total = sum(len(entry.dependencies) for entry in surface)
        print(
            f"dependency inventory: {total} third-party edge(s) across "
            f"{len(surface)} crate(s), all accounted for"
        )
        return 0

    print("dependency inventory: FAIL", file=sys.stderr)
    for edge in changes.added:
        print(
            f"  + {edge.crate} declares `{edge.dependency}`, which the inventory does not list.",
            file=sys.stderr,
        )
    for edge in changes.removed:
        print(
            f"  - {edge.crate} no longer declares `{edge.dependency}`, but the inventory still "
            "lists it.",
            file=sys.stderr,
        )
    print(
        "\nsoldr#2752 Rule A: a new third-party dependency is a deliberate act, "
        "not a manifest line.\nIf the change is intended, run "
        "`python .github/scripts/check_dependency_inventory.py --write` and "
        "commit the\ninventory alongside it, so the addition is visible in "
        "review.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
