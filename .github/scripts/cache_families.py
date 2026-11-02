"""ci.toml's `[cache.family]` declarations, as the budget the janitor enforces.

soldr#3618: `ci.toml` is the ONE place a cache family's key prefix and size
are declared. ci-lint reads it directly; `check_cache_budget.py` (the janitor
that deletes, and the budget verdict), `check_cache_ownership.py` (R6/R7),
`windows_guest_probe.py` and `measure_zccache_working_set.py` read it through
this module, so no second copy of a per-family byte number exists.

What `ci/cache-ownership.json` still contributes is not a size. Its
`family_groups` object maps every ci.toml family onto exactly one ownership
group, and per group records:

* `ci_toml_families` -- the member families (each must exist in ci.toml, and
  every ci.toml family must be a member of exactly one group);
* `entries` / `reserved` / `rationale` -- the ownership claims R7 checks;
* `evict` -- the janitor's eviction policy (`lru`, `newest`,
  `newest-per-lineage`, or absent for "never evict for budget"). ci.toml's
  own `evict` only accepts `"lru"`, so the policies this janitor needs beyond
  it cannot be spelled there;
* `store_cap_bytes` (zccache-unit only) -- the ON-DISK trim cap of the
  zccache store, a different quantity from its compressed Actions footprint.

`compose_budget` returns the janitor's historical `budget` shape: a group's
`key_prefixes` are its members' prefixes and its `max_bytes` is the SUM of
its members' footprints, computed exactly as ci-lint's CACHE-004 computes a
family footprint (`max x cardinality(per)`, or the sum of `shapes`).
`total_max_bytes` is the sum over all families and `fail_total_bytes` is
`[cache].budget`.

Standard library only (`tomllib`, Python >= 3.11): ci-pre.yml runs the
janitor with the runner image's own `python3`.
"""

from __future__ import annotations

import json
import pathlib
import re
import tomllib

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
CI_TOML = REPO_ROOT / "ci.toml"
OWNERSHIP_MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"

# ci-lint's size grammar (ci_lint/rules/cache_static.py `parse_size`): binary
# units, so "1GB" is 1 GiB. The janitor must parse sizes exactly as CACHE-004
# does, or the two would disagree about the same declaration.
SIZE_RE = re.compile(r"^(\d+(?:\.\d+)?)\s*(B|KB|MB|GB)$", re.IGNORECASE)
SIZE_UNITS = {"B": 1, "KB": 1024, "MB": 1024**2, "GB": 1024**3}

# `via` -> the key prefixes that producer writes, current first then legacy
# generations. Mirrors `ci_lint.cache.families.EXTERNAL_FAMILY_SHAPES` at the
# linter pinned by ci.toml's `linter`; only the producers this repository
# declares are listed. An unlisted `via` is an error, never a guess -- and a
# producer that bumps its prefix shows up as an unregistered key in the
# budget verdict, loudly.
VIA_PREFIXES: dict[str, tuple[str, ...]] = {
    "setup-soldr:build-cache": ("setup-soldr-buildcache-v2-",),
    "setup-soldr:cargo-registry": ("setup-soldr-cargoregistry-v1-",),
    "setup-soldr:cook": ("cook-base-v3-", "cook-base-v2-"),
    "setup-soldr:soldr-mini": ("soldr-mini-v2-",),
    "setup-soldr:solo-toolchain": ("solo-toolchain-v3-",),
    "setup-uv": ("setup-uv-2-", "setup-uv-1-"),
}


class CacheFamilyError(ValueError):
    """ci.toml and the ownership groups cannot be composed into a budget."""


def parse_size(text: object) -> int | None:
    """`"2400MB"` -> bytes, as ci-lint parses it; `None` if unparseable."""
    if not isinstance(text, str):
        return None
    match = SIZE_RE.match(text.strip())
    if not match:
        return None
    return int(float(match.group(1)) * SIZE_UNITS[match.group(2).upper()])


def cardinality(ci_toml: dict, per: object) -> int:
    """`per`'s multiplier, as ci-lint's `_cardinality` computes it."""
    platforms = ci_toml.get("platforms") or {}
    if per == "platform":
        return len(platforms)
    if per == "cross-platform":
        return max(len(platforms) - 1, 0)
    if per == "os":
        return len({spec.get("group") for spec in platforms.values()})
    return 1


def family_footprint(ci_toml: dict, family_id: str, spec: dict) -> int:
    """A family's steady bytes: the sum of its `shapes`, else `max x per`."""
    shapes = spec.get("shapes") or {}
    if shapes:
        sizes = [parse_size(shape.get("max")) for shape in shapes.values()]
        if any(size is None for size in sizes):
            raise CacheFamilyError(
                f"ci.toml [cache.family.{family_id}].shapes has an unparseable max"
            )
        return sum(size for size in sizes if size is not None)
    max_bytes = parse_size(spec.get("max"))
    if max_bytes is None:
        raise CacheFamilyError(
            f"ci.toml [cache.family.{family_id}].max {spec.get('max')!r} is not a size"
        )
    return max_bytes * cardinality(ci_toml, spec.get("per"))


def family_prefixes(family_id: str, spec: dict) -> tuple[str, ...]:
    """The key prefixes a family owns: its literal `prefix`, or its `via`'s."""
    literal = spec.get("prefix")
    if isinstance(literal, str) and literal:
        return (literal,)
    via = spec.get("via")
    if via in VIA_PREFIXES:
        return VIA_PREFIXES[via]
    raise CacheFamilyError(
        f"ci.toml [cache.family.{family_id}] has via = {via!r}, which "
        "cache_families.VIA_PREFIXES does not map to a key prefix; add it "
        "from ci_lint.cache.families at the pinned linter, or use `prefix`"
    )


def compose_budget(ci_toml: dict, groups: dict) -> dict:
    """The janitor's `budget` object, derived from ci.toml + ownership groups."""
    cache = ci_toml.get("cache") or {}
    families = cache.get("family") or {}
    if not families:
        raise CacheFamilyError("ci.toml declares no [cache.family]")
    fail_total = parse_size(cache.get("budget"))
    if fail_total is None:
        raise CacheFamilyError(
            f"ci.toml [cache].budget {cache.get('budget')!r} is not a size"
        )
    if not isinstance(groups, dict) or not groups:
        raise CacheFamilyError("ci/cache-ownership.json has no 'family_groups' object")

    owner: dict[str, str] = {}
    composed: dict[str, dict] = {}
    for group_id, group in groups.items():
        members = group.get("ci_toml_families") if isinstance(group, dict) else None
        if not isinstance(members, list) or not members:
            raise CacheFamilyError(
                f"family_groups[{group_id!r}].ci_toml_families must be a non-empty list"
            )
        prefixes: list[str] = []
        max_bytes = 0
        for family_id in members:
            if family_id not in families:
                raise CacheFamilyError(
                    f"family_groups[{group_id!r}] names {family_id!r}, which ci.toml "
                    "[cache.family] does not declare"
                )
            if family_id in owner:
                raise CacheFamilyError(
                    f"ci.toml family {family_id!r} is in two groups: "
                    f"{owner[family_id]!r} and {group_id!r}"
                )
            owner[family_id] = group_id
            prefixes.extend(family_prefixes(family_id, families[family_id]))
            max_bytes += family_footprint(ci_toml, family_id, families[family_id])
        spec = {k: v for k, v in group.items() if k != "ci_toml_families"}
        spec.update(
            key_prefixes=prefixes, max_bytes=max_bytes, ci_toml_families=members
        )
        composed[group_id] = spec

    orphans = sorted(set(families) - set(owner))
    if orphans:
        raise CacheFamilyError(
            f"ci.toml families {orphans} belong to no ci/cache-ownership.json "
            "family_groups entry; add each to the group that owns its entries"
        )
    return {
        "total_max_bytes": sum(spec["max_bytes"] for spec in composed.values()),
        "fail_total_bytes": fail_total,
        "families": composed,
    }


def load_ci_toml(path: pathlib.Path = CI_TOML) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def with_budget(manifest: dict, ci_toml_path: pathlib.Path = CI_TOML) -> dict:
    """`manifest` with its `budget` composed from ci.toml.

    A manifest that already carries a literal `budget` (a test's synthetic
    fixture) is returned unchanged; the repository's manifest carries
    `family_groups` instead, and a guard test keeps it that way.
    """
    if "budget" in manifest or "family_groups" not in manifest:
        return manifest
    return {
        **manifest,
        "budget": compose_budget(load_ci_toml(ci_toml_path), manifest["family_groups"]),
    }


def load_manifest(
    path: pathlib.Path = OWNERSHIP_MANIFEST, ci_toml_path: pathlib.Path = CI_TOML
) -> dict:
    """Read the ownership manifest and compose its budget from ci.toml."""
    manifest = json.loads(path.read_text(encoding="utf-8"))
    return with_budget(manifest, ci_toml_path)
