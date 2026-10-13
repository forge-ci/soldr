#!/usr/bin/env python3
"""Fail CI when the repository's GitHub Actions cache exceeds its budget.

soldr#3047, Phase B of soldr#3039. GitHub evicts the oldest, least-recently-used
entries once a repository's Actions cache crosses the 10 GB it documents as the
per-repository ceiling, and it does so silently: no job goes red, the run whose
warm cache disappeared underneath it just gets slower. `total_max_bytes` is
the sum of the family allocations and `fail_total_bytes` is ci.toml's
`[cache].budget` (9.5 GiB), so a family briefly over its own allocation does
not fail the whole gate before the next `--prune` sweep catches up. Every
number lives in `ci.toml` `[cache.family]` (soldr#3618); this script reads it
through `cache_families.py` and hard-codes none.

`zackees/soldr`'s cache had grown to 44.23 GiB across 143 entries by
2026-09-01 (`tests/fixtures/actions-cache/listing-2026-09-01.json`, the RED
acceptance fixture for this guard) -- more than four times the ceiling -- and
nothing in CI could tell a reviewer that from a passing lane. The only signal
was slower builds nobody could attribute to a cause.

## What is checked

`cache_families.compose_budget` builds a `budget` object from `ci.toml`
`[cache.family]` and `ci/cache-ownership.json` `family_groups`: a
`total_max_bytes` allocation, a `fail_total_bytes` hard ceiling, and a
`families` map of ownership groups. Each group owns its member families'
`key_prefixes`, a `max_bytes` allocation (the sum of their ci.toml
footprints), and a rationale. Every live cache entry is assigned to the group whose
`key_prefixes` entry is the LONGEST match on that entry's key -- longest,
so a family that owns a specific sub-namespace is not shadowed by a sibling
family's shorter, more general prefix.

Three things fail the gate:

* **An unregistered key.** An entry matching no family's `key_prefixes` names
  a producer nobody declared. ci.toml is authoritative, not descriptive:
  a new cache-writing step must declare its family in the same PR that adds
  it, or this guard has no way to tell a reviewed addition from cache-key
  drift (a resurrected `v0-rust-cross-build-*` key, say).
* **A family over its allocation.** Bytes used under one family's prefixes
  exceed that family's declared `max_bytes`.
* **The total over `fail_total_bytes`.** Even when every family individually
  fits, the sum across all of them may not exceed the hard ceiling.

## Network policy

By default, SKIP, DO NOT FAIL, when the live source is unavailable: `gh`
missing, a non-zero exit, or a response that does not parse as JSON. This
keeps fork PRs with read-only/no tokens from going red for lack of API access.
The same-repository PR budget gate and main operational sweep pass
`--require-live`, so those required paths fail closed instead of showing a
false green. `--from-json` bypasses the network entirely (used for the
acceptance fixture) and its own read failures are real failures, not skips,
because the caller chose that exact path.

## Pruning

`--prune` lists (never deletes without `--apply`) safe classes of
reclaimable entry: keys under a `RETIRED_PREFIXES` namespace whose producer no
longer runs, entries on a ref other than `refs/heads/main` (a PR's caches are
never restored by another PR), and `v0-rust-*` entries on `refs/heads/main`
that have been superseded by a newer generation of the same shared-key
lineage. The old perf-matrix target-cache namespace is an exception: it is
kept until a registry-only replacement exists for the same platform. It also
retires old cook locks within the same target/feature shape
(across soldr versions) only when that shape has a base under the exact
current main Cargo.lock hash; see `cook_lineage_candidates`, and superseded
setup-action store generations (an older lock, or an older toolchain hash
than the platform's newest current-lock store) via
`action_store_lineage_candidates`. The report
prints raw usage and the projected effective-after-safe-prune usage
separately, and the exit code always follows raw usage.
Pruning needs cache ids to call `gh cache delete`, so it requires the live
source; `--from-json` fixtures carry no ids.

Usage:
    python .github/scripts/check_cache_budget.py [options]
Options:
    --manifest PATH   ownership manifest (default: ci/cache-ownership.json);
                      its budget is composed from ci.toml [cache.family]
    --from-json PATH  read a cache listing from this file instead of `gh`
    --repo OWNER/NAME repository to query (default: zackees/soldr)
    --require-live    fail rather than skip when the live cache listing is unavailable
    --prune           report deletion candidates (dry run)
    --apply           with --prune, actually delete the candidates
    --sweep-only      with --prune --apply, exit 0 after deleting (janitor)
    --event-name E    triggering event; decides who pays for an overage
    --ref REF         triggering ref (refs/pull/N/merge for a PR)
    --delete-ref REF  delete one closed PR's entries, then exit 0

## Convergence and enforcement (zackees/ci.yml#6 "Cache (live)", GEN-009)

The janitor (`--prune --apply --sweep-only`, run by `ci-pre.yml` before every
CI run and on a schedule) deletes, in order: every safe-prune candidate --
including every `refs/pull/*` entry, which no other ref can restore -- and
then, per family, whatever its declared `evict` policy (`lru`,
`newest-per-lineage`) needs to fit its allocation. Families without `evict`
are never evicted for budget. Nothing younger than `SWEEP_GRACE_SECONDS` is
deleted. See `plan_sweep`.

The verdict depends on the event (`enforcement_mode`): a pull request fails
only for entries it saved from its own ref or for a static manifest breach
(`forecast_problems`), and warns about everything else; a push to a branch
other than main warns; main, schedule, dispatch and local runs fail hard.
"""

from __future__ import annotations

import argparse
import base64
import datetime
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

# ci-pre.yml runs this with the runner image's own `python3` (no setup-python,
# no uv), so it must stay standard-library only and say so if the image's
# interpreter is ever too old, rather than failing on a syntax/API detail.
STDLIB_PYTHON_FLOOR = (3, 11)  # tomllib, for ci.toml
if sys.version_info < STDLIB_PYTHON_FLOOR:  # pragma: no cover - old runner image
    sys.exit(
        f"check_cache_budget.py needs Python >= "
        f"{'.'.join(map(str, STDLIB_PYTHON_FLOOR))}, got {sys.version.split()[0]}"
    )

sys.path.insert(0, str(Path(__file__).resolve().parent))
# pylint: disable-next=wrong-import-position,wrong-import-order
import cache_families  # noqa: E402 -- sibling-module bootstrap precedes this import

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"
DEFAULT_REPO = "zackees/soldr"

GIB = 1024**3

# Retired Swatinem/rust-cache shared-key namespaces: nothing writes these any
# more, so any entry still carrying one of these prefixes is pure waste.
RETIRED_PREFIXES: tuple[str, ...] = (
    # `dylint-nightly-v1-<target>-<channel>` in _build-and-test.yml. Retired by
    # soldr#3216: 452 MiB of the fixed budget for about 15 s per host run, and
    # it pushed dylint-foundation over an allocation no family could top up.
    "dylint-nightly-",
    # `cross-build-<target>-v7` in _ci-cross-build-linux.yml. Retired by
    # soldr#3047: soldr#2996 had already made Tier 1 `soldr cook` the surviving
    # implementation of the dependency-graph cache on exactly this lane, so the
    # rust-cache step beside it was a second implementation of one tier.
    "v0-rust-cross-build-",
    # `ws-dev-<target>` in _build-and-test.yml's ci-test plan. Retired by
    # soldr#3047 (2.15 GiB across 5 entries at a 0% hit rate).
    "v0-rust-ws-dev-",
    # `pep517-<name>-ci-release-v2` for the PEP 517 wheel build, in both
    # ci.yml and _ci-target-run.yml. Retired by soldr#3047.
    "v0-rust-pep517-",
    # ci.yml's bootstrap-driver shared-key was renamed from
    # `bootstrap-soldr-linux-gnu-ci-bootstrap-v2` to
    # `bootstrap-soldr-linux-gnu-dev-v1` (see the `shared-key:` step in
    # ci.yml). The `-v2` generation is orphaned: nothing writes it any more.
    "v0-rust-bootstrap-soldr-linux-gnu-ci-bootstrap-",
    # The same rename on the sibling *binary* cache, which is `actions/cache`
    # rather than rust-cache. ci.yml now writes
    # `bootstrap-soldr-blessed-linux-gnu-dev-v1-<sha>` and
    # `_ci-cross-build-linux.yml` writes
    # `bootstrap-soldr-blessed-linux-gnu-<sha>`; a `<sha>` is hex, so neither
    # can ever produce the `-ci-bootstrap-` segment. It was 117 MB of the
    # bootstrap-driver-binary family in the 2026-09-01 listing -- a family
    # whose key embeds `github.sha` and therefore cannot be main-gated, so
    # pruning dead generations is the only lever it has.
    "bootstrap-soldr-blessed-linux-gnu-ci-bootstrap-",
    # `bootstrap-e2e-<target>` in _bootstrap-e2e.yml, retired by soldr#3121
    # to fund cook on the MSVC/aarch64-gnu cross lanes.
    "v0-rust-bootstrap-e2e-",
    # `stable-cook-v2-<target>-<hash>` in _build-and-test.yml, retired by
    # soldr#3396: soldr#3043's prototype cook cache restored the previous
    # lockfile's archives through a restore-keys prefix and never pruned them
    # (one 2.8 GB entry). Nothing writes the prefix any more.
    "stable-cook-",
    # perf-matrix's registry-only rust-cache layer was removed after
    # measurements showed the exact-source binary cache retained same-source
    # perf reruns while the registry saved only cold-build fetch time. No
    # current workflow writes this namespace.
    "v0-rust-perf-registry-soldr-",
    # `wheel-cross-<target>-release-v1` in ci.yml's wheel-cross-verify lane.
    # Retired by zackees/ci.yml#209 (CACHE-025): the lane runs the shared
    # bootstrap soldr, so the bootstrap exception does not cover it.
    "v0-rust-wheel-cross-",
    # The last Swatinem/rust-cache producers, retired when CACHE-025 dropped
    # its exceptions (zackees/ci.yml, maintainer decision 2026-10-02):
    # ci.yml's bootstrap driver (`bootstrap-soldr-linux-gnu-dev-v1`, formerly
    # the rust-cache-residual family) and three experiment lanes --
    # baseline-zero-deps (`ws-release-<target>`), parent-cache-bench and
    # perf-cold-warm (`perf-cold-warm-build-soldr-linux`). Their prefixes stay
    # in experiment-lanes only so a leftover entry is retired, not unknown.
    "v0-rust-bootstrap-soldr-linux-gnu-dev-",
    "v0-rust-ws-release-",
    "v0-rust-parent-cache-bench",
    "v0-rust-perf-cold-warm-",
    # setup-soldr's `cook-delta-v2-` layer, retired fleet-wide by
    # setup-soldr#533; nothing here writes it (soldr#3618 dropped it from the
    # cook family, so a leftover is reclaimed rather than budgeted).
    "cook-delta-",
)


@dataclass(frozen=True)
class CacheEntry:
    """One `gh cache list` row, trimmed to the fields this guard reads."""

    key: str
    ref: str
    size_bytes: int
    id: str | None = None
    created_at: str | None = None
    last_accessed_at: str | None = None


# ---------------------------------------------------------------------------
# Loading a listing
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Listing:
    """A `--from-json` fixture: its raw entries and the reported usage bytes."""

    entries: list[object]
    usage_bytes: int | None


@dataclass(frozen=True)
class Family:
    """One manifest family; `spec` is None when the manifest value is no object."""

    family_id: str
    spec: dict | None


@dataclass(frozen=True)
class FamilyGroup:
    """A family and the cache entries its key prefixes own."""

    family: Family
    entries: list[CacheEntry]


@dataclass(frozen=True)
class FamilyGrouping:
    """Entries split across the manifest families, plus those matching none."""

    groups: list[FamilyGroup]
    unmatched: list[CacheEntry]

    def entries_of(self, family_id: str) -> list[CacheEntry]:
        """The entries owned by `family_id` (empty when it is not a family)."""
        for group in self.groups:
            if group.family.family_id == family_id:
                return group.entries
        return []


@dataclass(frozen=True)
class RecognizedEntry:
    """An action-store cache entry split into its retention-relevant parts."""

    entry: CacheEntry
    platform: str
    toolchain: str
    lock: str


@dataclass(frozen=True)
class SweepPlan:
    """One janitor sweep: what to delete now and what to defer."""

    delete: list[CacheEntry]
    deferred: list[CacheEntry]


def normalize_entries(raw: list[object]) -> list[CacheEntry]:
    """Turn raw JSON rows (fixture or live) into `CacheEntry` objects.

    A malformed row (missing/mistyped key, ref or size) is dropped rather than
    raising -- one bad row from `gh` should not take the whole budget check
    down with it.
    """
    entries: list[CacheEntry] = []
    for item in raw:
        if not isinstance(item, dict):
            continue
        key = item.get("key")
        ref = item.get("ref")
        size = item.get("sizeInBytes")
        if (
            not isinstance(key, str)
            or not isinstance(ref, str)
            or not isinstance(size, int)
        ):
            continue
        # `gh cache list --json id` emits the id as a JSON NUMBER, not a
        # string. Accepting only `str` here silently dropped every live id,
        # so `--prune --apply` reported "no cache id, cannot delete" for all
        # 21 candidates on 2026-09-04 and reclaimed nothing.
        entry_id = item.get("id")
        if isinstance(entry_id, bool) or not isinstance(entry_id, (int, str)):
            entry_id = None
        created_at = item.get("createdAt")
        last_accessed_at = item.get("lastAccessedAt")
        entries.append(
            CacheEntry(
                key=key,
                ref=ref,
                size_bytes=size,
                id=str(entry_id) if entry_id is not None else None,
                created_at=created_at if isinstance(created_at, str) else None,
                last_accessed_at=(
                    last_accessed_at if isinstance(last_accessed_at, str) else None
                ),
            )
        )
    return entries


def load_from_json(path: pathlib.Path) -> Listing:
    """Read the `{"usage_bytes", "entries": [...]}` fixture shape."""
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError(f"{path} must contain a JSON object")
    entries = payload.get("entries")
    if not isinstance(entries, list):
        raise ValueError(f"{path} has no 'entries' array")
    usage = payload.get("usage_bytes")
    return Listing(entries, usage if isinstance(usage, int) else None)


def run_gh(args: list[str]) -> str:
    """Run `gh <args>` and return stdout. Raises on any failure.

    Kept as its own function so tests can monkeypatch exactly this call to
    simulate a missing/broken `gh` without touching the network.
    """
    result = run_captured(["gh", *args], capture_output=True, text=True, check=True)
    return result.stdout


def fetch_live_entries(repo: str) -> list[object]:
    """`gh cache list`, paged high enough to see the whole repository cache.

    GitHub pages this API at 100 by default, so the explicit `--limit 1000`
    is required, not cosmetic -- a repository with more than 100 entries
    would otherwise silently see only the first page.
    """
    stdout = run_gh(
        [
            "cache",
            "list",
            "--repo",
            repo,
            "--limit",
            "1000",
            "--json",
            "id,key,ref,sizeInBytes,createdAt,lastAccessedAt",
        ]
    )
    payload = json.loads(stdout)
    if not isinstance(payload, list):
        raise ValueError("gh cache list did not return a JSON array")
    return payload


def fetch_live_usage_bytes(repo: str) -> int | None:
    """`actions/cache/usage`'s reported total, or `None` if it cannot be read.

    Best-effort and separate from `fetch_live_entries`: a broken usage call
    does not invalidate a listing that came back fine, it just means the
    table prints without the API's own cross-check number.
    """
    try:
        stdout = run_gh(["api", f"repos/{repo}/actions/cache/usage"])
        payload = json.loads(stdout)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError):
        return None
    if isinstance(payload, dict):
        value = payload.get("active_caches_size_in_bytes")
        if isinstance(value, int):
            return value
    return None


# ---------------------------------------------------------------------------
# Family assignment
# ---------------------------------------------------------------------------


def parse_families(raw: dict) -> list[Family]:
    """The manifest's `budget.families` object as `Family` records."""
    return [
        Family(family_id, spec if isinstance(spec, dict) else None)
        for family_id, spec in raw.items()
    ]


def family_for(key: str, families: list[Family]) -> str | None:
    """The family owning `key`: the LONGEST matching `key_prefixes` entry."""
    best_id: str | None = None
    best_len = -1
    for family in families:
        family_id, spec = family.family_id, family.spec
        if spec is None:
            continue
        for prefix in spec.get("key_prefixes") or []:
            if (
                isinstance(prefix, str)
                and key.startswith(prefix)
                and len(prefix) > best_len
            ):
                best_len = len(prefix)
                best_id = family_id
    return best_id


def group_by_family(
    entries: list[CacheEntry], families: list[Family]
) -> FamilyGrouping:
    """Each family's entries, plus the entries matching no family."""
    groups = [FamilyGroup(family, []) for family in families]
    by_id = {group.family.family_id: group for group in groups}
    unmatched: list[CacheEntry] = []
    for entry in entries:
        family_id = family_for(entry.key, families)
        if family_id is None:
            unmatched.append(entry)
        else:
            by_id[family_id].entries.append(entry)
    return FamilyGrouping(groups, unmatched)


# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------


def load_manifest(path: pathlib.Path) -> dict:
    """The ownership manifest, its `budget` composed from ci.toml (soldr#3618)."""
    return cache_families.load_manifest(path)


def budget_problems(  # noqa: C901
    manifest_path: pathlib.Path, manifest: dict, entries: list[CacheEntry]
) -> list[str]:
    """Every budget failure for `entries` under `manifest['budget']`."""
    if not isinstance(manifest, dict):
        return [f"{manifest_path} must contain a JSON object"]
    budget = manifest.get("budget")
    if not isinstance(budget, dict):
        return [f"{manifest_path} has no object-valued 'budget'"]

    problems: list[str] = []

    total_max_bytes = budget.get("total_max_bytes")
    if not isinstance(total_max_bytes, int):
        problems.append(f"{manifest_path} budget.total_max_bytes must be an integer")

    fail_total_bytes = budget.get("fail_total_bytes")
    if not isinstance(fail_total_bytes, int):
        problems.append(f"{manifest_path} budget.fail_total_bytes must be an integer")

    families = budget.get("families")
    if not isinstance(families, dict) or not families:
        problems.append(f"{manifest_path} budget.families must be a non-empty object")
        return problems

    parsed = parse_families(families)
    grouping = group_by_family(entries, parsed)

    for entry in sorted(grouping.unmatched, key=lambda e: (e.key, e.ref)):
        problems.append(
            f"unregistered cache entry key={entry.key!r} ref={entry.ref!r}: an "
            "unregistered producer may not appear; register its key prefix "
            f"under a family in {manifest_path} budget.families in the same "
            "PR that adds the producer."
        )

    for family in sorted(parsed, key=lambda f: f.family_id):
        family_id, spec = family.family_id, family.spec
        if spec is None:
            problems.append(f"family {family_id!r} is not an object")
            continue
        max_bytes = spec.get("max_bytes")
        if not isinstance(max_bytes, int):
            problems.append(f"family {family_id!r} has no integer 'max_bytes'")
            continue
        used = sum(e.size_bytes for e in grouping.entries_of(family_id))
        if used > max_bytes:
            problems.append(
                f"family {family_id!r} uses {used / GIB:.2f} GiB, over its "
                f"{max_bytes / GIB:.2f} GiB budget"
            )

    if isinstance(fail_total_bytes, int):
        total_bytes = sum(e.size_bytes for e in entries)
        if total_bytes > fail_total_bytes:
            problems.append(
                f"total repository Actions-cache usage {total_bytes / GIB:.2f} "
                f"GiB exceeds fail_total_bytes {fail_total_bytes / GIB:.2f} GiB"
            )

    return problems


def check(manifest_path: pathlib.Path, entries: list[CacheEntry]) -> list[str]:
    """Every budget failure, as actionable lines. Empty means it holds."""
    try:
        manifest = load_manifest(manifest_path)
    except (OSError, json.JSONDecodeError) as error:
        return [f"cannot read {manifest_path}: {error}"]
    return budget_problems(manifest_path, manifest, entries)


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------


def build_table(
    budget: dict, entries: list[CacheEntry], usage_bytes: int | None
) -> str:
    """The pass-or-fail table printed on every run."""
    families = budget.get("families")
    if not isinstance(families, dict):
        families = {}
    grouping = group_by_family(entries, parse_families(families))
    total_bytes = sum(e.size_bytes for e in entries)
    total_max_bytes = budget.get("total_max_bytes")

    rows = []
    for group in grouping.groups:
        spec = group.family.spec
        max_bytes = spec.get("max_bytes") if spec is not None else None
        used = sum(e.size_bytes for e in group.entries)
        rows.append((group.family.family_id, len(group.entries), used, max_bytes))
    rows.sort(key=lambda row: row[2], reverse=True)

    lines = [f"{'family':<42} {'count':>6} {'used GiB':>10} {'alloc GiB':>10} {'%':>7}"]
    for family_id, count, used, max_bytes in rows:
        alloc_gib = max_bytes / GIB if isinstance(max_bytes, int) else float("nan")
        pct = (
            (used / max_bytes * 100)
            if isinstance(max_bytes, int) and max_bytes
            else 0.0
        )
        lines.append(
            f"{family_id:<42} {count:>6} {used / GIB:>10.2f} {alloc_gib:>10.2f} {pct:>6.1f}%"
        )
    total_alloc_gib = (
        total_max_bytes / GIB if isinstance(total_max_bytes, int) else float("nan")
    )
    lines.append(
        f"{'TOTAL':<42} {len(entries):>6} {total_bytes / GIB:>10.2f} {total_alloc_gib:>10.2f}"
    )
    unmatched = grouping.unmatched
    if unmatched:
        noun = "entry" if len(unmatched) == 1 else "entries"
        lines.append(f"  ({len(unmatched)} unregistered {noun} not shown above)")
    if usage_bytes is not None:
        lines.append(f"actions/cache/usage reports {usage_bytes / GIB:.2f} GiB active")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Pruning
# ---------------------------------------------------------------------------


# Key families whose trailing `-<segment>` is a per-run generation of one
# shared lineage: `v0-rust-*` ends in a rust-cache hash, `zccache-unit-*`
# ends in the GitHub run id (soldr#3041 keys it that way so every host-lane
# run saves a fresh generation and restores the newest by prefix). Only the
# newest generation of a lineage is ever restored, so the rest is dead
# weight -- 1.3 GiB per host-lane run on main (soldr#3102).
# `bootstrap-soldr-blessed-*-<sha>` (ci.yml, _ci-cross-build-linux.yml) is
# keyed on the exact `github.sha` with no restore-keys, so only a re-run of
# that same commit can restore an older entry; every main merge wrote another
# ~23 MiB driver that nothing superseded (13 live on 2026-09-14, 0.30 GiB of
# a 0.15 GiB family). Its two lineages (`...-linux-gnu`, `...-linux-gnu-dev-v1`)
# now keep only their newest entry.
# `dylint-foundation-` (soldr#3216): the tree is keyed on a hash of the lint
# sources and saved only on main, so each lint change leaves the previous
# ~413 MiB generation behind; two of them alone exceed the family allocation.
# `setup-soldr-dogfood-zccache-` (soldr#3398): setup-soldr-action.yml keys the
# dogfood store `...-<os>-<arch>-<lockfile hash>-<github.sha>` and saves one
# per main commit, restoring the newest through the lockfile-hash prefix, so
# stripping the trailing sha leaves the lockfile prefix and only its newest
# generation is kept. A generation that is alone under its lockfile prefix is
# never touched (three generations were ~1.05 GiB of a 1.30 GiB family).
GENERATION_KEY_PREFIXES = (
    "v0-rust-",
    "zccache-unit-",
    "bootstrap-soldr-blessed-",
    "dylint-foundation-",
    "setup-soldr-dogfood-zccache-",
    # The toolchain archive is identified by toolchain + target set. The
    # setup-soldr version suffix is only the producer version, and otherwise
    # leaves one identical ~180 MiB toolchain per action release forever.
    "solo-toolchain-v3-",
)

SOLO_TOOLCHAIN_KEY = re.compile(r"^(solo-toolchain-v3-.+)-soldrv[0-9.]+$")
PERF_TARGET_KEY = re.compile(
    r"^v0-rust-perf-build-soldr-(?P<platform>[a-z0-9-]+)-"
    r"(?P<os>Linux|Windows|macOS)-(?P<arch>[A-Za-z0-9_]+)-"
    r"[0-9a-f]+-[0-9a-f]+$"
)
PERF_BINARY_KEY = re.compile(r"^soldr-bin-(?P<platform>[a-z0-9-]+)-[0-9a-f]{64}$")

COOK_KEY = re.compile(
    r"^cook-(base|delta)-v2-(.+-f[0-9a-f]+)-l([0-9a-f]+)-soldr([0-9.]+)(?:-s[0-9a-f]+-g[0-9a-f]+)?$"
)


def fetch_main_lock_hash(repo: str) -> str:
    """Hash the raw Cargo.lock bytes at the live main ref, not this checkout."""
    payload = json.loads(run_gh(["api", f"repos/{repo}/contents/Cargo.lock?ref=main"]))
    if not isinstance(payload, dict) or payload.get("encoding") != "base64":
        raise ValueError("main Cargo.lock response is not base64 content")
    encoded = payload.get("content")
    if not isinstance(encoded, str):
        raise ValueError("main Cargo.lock response has no content")
    return hashlib.sha256(base64.b64decode(encoded)).hexdigest()[:16]


def strip_shared_key_hash(key: str) -> str:
    """Drop a generation key's trailing `-<hash|run id>` segment."""
    solo_toolchain = SOLO_TOOLCHAIN_KEY.fullmatch(key)
    if solo_toolchain:
        return solo_toolchain.group(1)
    if not key.startswith(GENERATION_KEY_PREFIXES):
        return key
    index = key.rfind("-")
    return key[:index] if index != -1 else key


def cook_shape(match: re.Match[str]) -> str:
    """The target/toolchain/feature shape of a cook key, without lock or soldr."""
    return match.group(2)


def cook_lineage_candidates(
    on_main: list[CacheEntry], current_main_lock: str | None
) -> list[CacheEntry]:
    """Superseded main `cook-base-v2` / `cook-delta-v2` lock generations.

    The explicit retention policy (soldr#3347):

    * Soldr's workflows explicitly disable the optional cook delta layer, so
      every `cook-delta-v2-*` entry is dead state and is always retired. The
      base layer remains the reusable dependency cache. Re-enabling deltas
      requires changing this policy and its workflow guard together.
    * The current generation of a shape is its base(s) under the live main
      Cargo.lock hash. Every such base is kept, at every soldr version, so a
      soldr rollback still finds its archive.
    * An entry under any other lock hash is retired only when its own
      target/feature shape has a base under the current lock. The soldr
      version is deliberately NOT part of that match: a lockfile bump and a
      soldr bump usually land together, and keying the match on the version
      kept every prior-lock base alive forever (five prior shapes, ~0.8 GiB
      of a 2.33 GiB family on 2026-09-23).
    * A base with no current-lock base for its shape is unique and active: it
      is not retired merely because another shape is newer.
    * Without a known current lock (the main Cargo.lock fetch failed), no base
      is retired; keys this regex does not recognize are never touched.
    """
    # The workflows' explicit `cook-delta: false` contract makes these entries
    # unrestorable. Retire them even if the live main lock lookup is
    # unavailable; base lineage pruning below still fails closed without it.
    retired = [
        entry
        for entry in on_main
        if (match := COOK_KEY.fullmatch(entry.key)) and match.group(1) == "delta"
    ]
    if not current_main_lock:
        return retired
    current_shapes: set[str] = set()
    for entry in on_main:
        match = COOK_KEY.fullmatch(entry.key)
        if match and match.group(1) == "base" and match.group(3) == current_main_lock:
            current_shapes.add(cook_shape(match))
    for entry in on_main:
        match = COOK_KEY.fullmatch(entry.key)
        if (
            match
            and match.group(1) == "base"
            and match.group(3) != current_main_lock
            and cook_shape(match) in current_shapes
        ):
            retired.append(entry)
    return retired


ACTION_BUILD_KEY = re.compile(
    r"^(?P<shape>setup-soldr-buildcache-v2-[a-z0-9_]+-[a-z0-9_]+-[0-9a-f]{16})-"
    r"(?P<lock>[0-9a-f]{16})$"
)
ACTION_REGISTRY_KEY = re.compile(
    r"^(?P<platform>setup-soldr-cargoregistry-v1-[a-z0-9_]+-[a-z0-9_]+)-"
    r"(?P<lock>[0-9a-f]{16})-(?P<toolchain>[0-9a-f]{16})$"
)


def action_store_lineage_candidates(
    on_main: list[CacheEntry], current_main_lock: str | None
) -> list[CacheEntry]:
    """Retire superseded lock/toolchain generations of setup-action stores.

    A recognized key splits into a platform (`<os>-<arch>`), a 16-hex
    toolchain hash and a 16-hex lock hash. The retention policy:

    * A platform with no entry under the live main lock is unique and active:
      nothing on it is retired, however old its lock.
    * On a platform that HAS a current-lock entry, that newest entry's
      toolchain is the current one and every other entry on the platform is
      superseded: an older lock at any toolchain (soldr#3347), or the same
      lock under an older toolchain (soldr#3545 -- a toolchain bump changes
      the toolchain hash while Cargo.lock often does not, so the stale store
      shared the live entry's lock and was unreachable by both the lock rule
      and a shape match that included the toolchain hash; two such entries
      held 1.43 GiB of a 1.30 GiB family with no eviction rule that could
      ever select either).
    * Namespaced and unknown key formats are deliberately outside this
      policy, and without a known current lock nothing is retired.
    Re-running an older lock or toolchain can rebuild its store, as with cook
    lineage retention; it must not accumulate indefinitely at main's expense.
    """
    if not current_main_lock:
        return []
    # Both key formats end their shape in
    # the toolchain hash, so the platform is everything before its last '-'.
    recognized: list[RecognizedEntry] = []
    for entry in on_main:
        build = ACTION_BUILD_KEY.fullmatch(entry.key)
        registry = ACTION_REGISTRY_KEY.fullmatch(entry.key)
        if build:
            platform, _, toolchain = build["shape"].rpartition("-")
            recognized.append(
                RecognizedEntry(entry, platform, toolchain, build["lock"])
            )
        elif registry:
            recognized.append(
                RecognizedEntry(
                    entry,
                    registry["platform"],
                    registry["toolchain"],
                    registry["lock"],
                )
            )
    # The current toolchain of every platform that still has a current-lock
    # store: the newest such entry, ties broken on the toolchain hash so two
    # entries with identical timestamps resolve the same way in any order.
    # Two parallel str dicts rather than a dict-of-tuple record (PY-002).
    best_created: dict[str, str] = {}
    best_toolchain: dict[str, str] = {}
    for item in recognized:
        platform, toolchain = item.platform, item.toolchain
        if item.lock != current_main_lock:
            continue
        created = item.entry.created_at or ""
        if platform not in best_created or (created, toolchain) > (
            best_created[platform],
            best_toolchain[platform],
        ):
            best_created[platform] = created
            best_toolchain[platform] = toolchain
    return [
        item.entry
        for item in recognized
        if item.platform in best_toolchain
        and (
            item.lock != current_main_lock
            or item.toolchain != best_toolchain[item.platform]
        )
    ]


def perf_target_cache_candidates(on_main: list[CacheEntry]) -> list[CacheEntry]:
    """Retire obsolete perf target caches after the exact-source binary producer.

    The perf-matrix producer previously used `perf-build-soldr-<platform>`
    with rust-cache's default target caching. Those entries are still
    restorable, so they are not unconditional retired prefixes. A matching
    exact-source `soldr-bin-<platform>-<hashFiles>` entry saved on main after
    the old target archive proves the replacement producer has run on that
    platform. Missing timestamps fail closed.
    """
    replacements: dict[str, list[str]] = {}
    for entry in on_main:
        match = PERF_BINARY_KEY.fullmatch(entry.key)
        if match and entry.created_at:
            replacements.setdefault(match["platform"], []).append(entry.created_at)

    candidates: list[CacheEntry] = []
    for entry in on_main:
        match = PERF_TARGET_KEY.fullmatch(entry.key)
        if (
            match
            and entry.created_at
            and any(
                replacement_at > entry.created_at
                for replacement_at in replacements.get(match["platform"], [])
            )
        ):
            candidates.append(entry)
    return candidates


def prune_candidates(  # noqa: C901
    entries: list[CacheEntry],
    current_main_lock: str | None = None,
) -> list[CacheEntry]:
    """Entries safe to delete: retired prefix, non-main ref, or superseded.

    Each entry is classified into exactly the first rule it matches, so the
    reclaimed-bytes total never double-counts one entry.
    """
    candidates: list[CacheEntry] = []
    remaining: list[CacheEntry] = []
    for entry in entries:
        if entry.key.startswith(RETIRED_PREFIXES):
            candidates.append(entry)
        else:
            remaining.append(entry)

    on_main: list[CacheEntry] = []
    for entry in remaining:
        if entry.ref != "refs/heads/main":
            candidates.append(entry)
        else:
            on_main.append(entry)

    groups: dict[str, list[CacheEntry]] = {}
    for entry in on_main:
        # Old perf target archives remain useful until the exact-source binary
        # producer has saved a matching-platform main generation after them.
        # Do not let generic rust-cache generation pruning bypass that guard.
        if PERF_TARGET_KEY.fullmatch(entry.key):
            continue
        if not entry.key.startswith(GENERATION_KEY_PREFIXES):
            continue
        groups.setdefault(strip_shared_key_hash(entry.key), []).append(entry)

    for group_entries in groups.values():
        if len(group_entries) <= 1:
            continue
        newest = max(group_entries, key=lambda e: e.created_at or "")
        for entry in group_entries:
            if entry is not newest:
                candidates.append(entry)

    candidates.extend(cook_lineage_candidates(on_main, current_main_lock))
    candidates.extend(action_store_lineage_candidates(on_main, current_main_lock))
    candidates.extend(perf_target_cache_candidates(on_main))

    return candidates


def effective_verdict(effective_problems: list[str]) -> str:
    """One line for the projected post-prune state, never green while red.

    Raw usage decides the exit code; this line only says whether the safe
    candidate set would be enough. A family still over its cap after every
    safe deletion is a producer or retention defect, not a pruning backlog.
    """
    if effective_problems:
        return (
            f"effective after safe prune: STILL OVER BUDGET "
            f"({len(effective_problems)} problem(s)); pruning alone cannot fix this"
        )
    return "effective after safe prune: every family and the total fit"


def apply_prune(candidates: list[CacheEntry], repo: str) -> list[str]:
    """Delete every candidate with a cache id. Returns failure lines."""
    failures: list[str] = []
    for entry in candidates:
        if entry.id is None:
            failures.append(f"{entry.key}: no cache id, cannot delete")
            continue
        try:
            run_gh(["cache", "delete", entry.id, "--repo", repo])
        except (OSError, subprocess.CalledProcessError) as error:
            failures.append(f"{entry.key} ({entry.id}): {error}")
    return failures


# ---------------------------------------------------------------------------
# Convergent eviction (zackees/ci.yml#6 "Cache (live)", GEN-009)
# ---------------------------------------------------------------------------

# An entry younger than this is never deleted by a sweep. The janitor runs on
# every push, so without a grace window it could delete a cache a producer job
# saved seconds ago, before the consumer jobs of the same run restore it.
SWEEP_GRACE_SECONDS = 10 * 60

# `evict` values a family may declare in ci/cache-ownership.json. A family
# with no `evict` key is never evicted for budget; only the safe classes in
# `prune_candidates` (retired, non-main ref, superseded) ever touch it.
#
# * `lru` -- no required job restores this family, so the janitor deletes its
#   least-recently-used entries until the family fits its allocation.
# * `newest-per-lineage` -- the janitor keeps the newest entry of every
#   lineage (the key with each digit run normalized, so version bumps of one
#   download share a lineage), and deletes older generations least-recently
#   used first until the family fits. The newest entry, which is the one a
#   required job restores, is never a candidate.
# * `newest` -- as `newest-per-lineage`, but the whole family is one lineage:
#   only its single newest main entry is protected. For hash-keyed stores
#   (zccache-unit-*) whose hex keys defeat digit normalization, so each
#   generation would otherwise be its own protected lineage (soldr#3458).
EVICT_POLICIES = ("lru", "newest-per-lineage", "newest")

PR_REF = re.compile(r"^refs/pull/(?P<number>[0-9]+)/(?:merge|head)$")
# The `pr-<N>` component PR-context saves put in their key (zackees/ci.yml#6:
# "PRs save one small delta ... keyed by PR number"). Delimited on both sides
# so `pr-12` never matches `pr-123`, and never matches mid-word (`xpr-1`).
PR_KEY_TAG = re.compile(r"(?:^|[-_.])pr-(?P<number>[0-9]+)(?=$|[-_.])")


def pr_number_of(entry: CacheEntry) -> int | None:
    """The PR an entry belongs to: its `refs/pull/<N>/*` ref or `pr-<N>` key tag."""
    match = PR_REF.fullmatch(entry.ref) or PR_KEY_TAG.search(entry.key)
    return int(match["number"]) if match else None


def parse_timestamp(value: str | None) -> datetime.datetime | None:
    """GitHub's ISO-8601 timestamps (`...Z` or with fractional seconds)."""
    if not value:
        return None
    try:
        parsed = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=datetime.timezone.utc)
    return parsed


def within_grace(entry: CacheEntry, now: datetime.datetime) -> bool:
    """Whether `entry` is too young to delete. Unknown age fails closed."""
    created = parse_timestamp(entry.created_at)
    if created is None:
        return True
    return (now - created).total_seconds() < SWEEP_GRACE_SECONDS


def lineage_of(key: str) -> str:
    """A key with every digit run normalized: `rustup-1.97.0-x` ~ `rustup-1.98.1-x`."""
    return re.sub(r"[0-9]+", "#", key)


def recency(entry: CacheEntry) -> str:
    """LRU order: last access, else creation; unknown sorts oldest."""
    return entry.last_accessed_at or entry.created_at or ""


def eviction_candidates(
    entries: list[CacheEntry],
    families: list[Family],
    excluded: set[int],
    now: datetime.datetime,
) -> list[CacheEntry]:
    """Entries an `evict` family must lose to fit its allocation.

    `excluded` holds `id()`s already chosen for deletion by the safe prune, so
    their bytes are not counted twice. Families without an `evict` policy are
    never touched here.
    """
    grouping = group_by_family(entries, families)
    chosen: list[CacheEntry] = []
    for group in sorted(grouping.groups, key=lambda g: g.family.family_id):
        spec = group.family.spec
        family_entries = group.entries
        if spec is None:
            continue
        policy = spec.get("evict")
        max_bytes = spec.get("max_bytes")
        if policy not in EVICT_POLICIES or not isinstance(max_bytes, int):
            continue
        live = [e for e in family_entries if id(e) not in excluded]
        used = sum(e.size_bytes for e in live)
        if used <= max_bytes:
            continue
        protected: set[int] = set()
        if policy in ("newest-per-lineage", "newest"):
            newest: dict[str, CacheEntry] = {}
            for entry in (e for e in live if e.ref == "refs/heads/main"):
                lineage = lineage_of(entry.key) if policy != "newest" else ""
                current = newest.get(lineage)
                if current is None or (entry.created_at or "") > (
                    current.created_at or ""
                ):
                    newest[lineage] = entry
            protected = {id(entry) for entry in newest.values()}
        pool = sorted(
            (
                e
                for e in live
                if id(e) not in protected
                # An open PR's entries count toward the budget and are
                # evicted by the same policy as main's (zackees/ci.yml#6).
                and (e.ref == "refs/heads/main" or pr_number_of(e) is not None)
                and not within_grace(e, now)
            ),
            key=recency,
        )
        for entry in pool:
            if used <= max_bytes:
                break
            chosen.append(entry)
            used -= entry.size_bytes
    return chosen


def fetch_pr_state(repo: str, number: int) -> str | None:
    """`open` or `closed` (merged PRs are `closed` too); `None` if unknown."""
    try:
        state = run_gh(["api", f"repos/{repo}/pulls/{number}", "--jq", ".state"])
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"  PR #{number}: state lookup failed ({error}); keeping its entries")
        return None
    state = state.strip()
    return state if state in {"open", "closed"} else None


def closed_pr_candidates(
    entries: list[CacheEntry], pr_state: Callable[[int], str | None] | None
) -> list[CacheEntry]:
    """Every entry of a PR that is no longer open (merged or closed).

    GitHub scopes `refs/pull/<N>/*` entries to that PR, so once it is closed
    nothing can restore them. Open PRs keep theirs -- repeat pushes stay warm
    -- and those count toward the family budgets like any other entry. One
    state lookup per PR number; a failed lookup keeps the entries (fail safe).
    No grace window: a closed PR has no running jobs to race.
    """
    by_number: dict[int, list[CacheEntry]] = {}
    for entry in entries:
        number = pr_number_of(entry)
        if number is not None:
            by_number.setdefault(number, []).append(entry)
    closed: list[CacheEntry] = []
    for number, pr_entries in sorted(by_number.items()):
        state = pr_state(number) if pr_state is not None else None
        if state is None:
            print(f"  PR #{number}: state unknown; kept {len(pr_entries)} entr(ies)")
        elif state == "closed":
            closed.extend(pr_entries)
    return closed


def plan_sweep(
    entries: list[CacheEntry],
    manifest: dict,
    current_main_lock: str | None,
    now: datetime.datetime,
    pr_state: Callable[[int], str | None] | None = None,
) -> SweepPlan:
    """The `SweepPlan` (delete, deferred) for one janitor sweep.

    Order matters and is the whole convergence argument:

    1. Every entry of a PR that is no longer open (`closed_pr_candidates`).
    2. Every other safe-prune candidate (`prune_candidates`): retired
       prefixes, entries on non-main branch refs, superseded generations, and
       the cook/perf lineage rules. Open PRs' entries are not in this class.
    3. Only then the per-family `evict` policies, measured against what is
       left, so they delete no more than the family's overage.

    Anything but a closed PR's entry that is younger than
    `SWEEP_GRACE_SECONDS` is deferred, never deleted, so a running job's
    just-saved cache is not raced.
    """
    closed = closed_pr_candidates(entries, pr_state)
    closed_ids = {id(e) for e in closed}
    safe = [
        e
        for e in prune_candidates(entries, current_main_lock)
        if not PR_REF.fullmatch(e.ref) and id(e) not in closed_ids
    ]
    delete = closed + [e for e in safe if not within_grace(e, now)]
    deferred = [e for e in safe if within_grace(e, now)]
    budget = manifest.get("budget") if isinstance(manifest, dict) else None
    families = budget.get("families") if isinstance(budget, dict) else None
    if isinstance(families, dict):
        excluded = {id(e) for e in safe} | {id(e) for e in closed}
        delete.extend(
            eviction_candidates(entries, parse_families(families), excluded, now)
        )
    return SweepPlan(delete, deferred)


# ---------------------------------------------------------------------------
# Who pays for an over-budget cache
# ---------------------------------------------------------------------------


def enforcement_mode(event_name: str, ref: str) -> str:
    """`pr`, `warn` or `fail` for the triggering event.

    * `pull_request`: pre-existing repository state is not the PR's fault, so
      it is reported as a warning; only the PR's own contribution fails.
    * a push or workflow_dispatch on any branch but main: report, exit 0. A
      push there follows a janitor sweep; a dispatch there is a PR's
      exact-SHA "CI full" validation (zackees/ci.yml#166: #3510's full run
      failed on main's pre-existing overage, which that PR did not cause).
    * main, schedule, a dispatch on main, and local runs (no event): hard fail.
    """
    if event_name == "pull_request":
        return "pr"
    if event_name in ("push", "workflow_dispatch") and ref != "refs/heads/main":
        return "warn"
    return "fail"


def forecast_problems(manifest_path: pathlib.Path, manifest: dict) -> list[str]:
    """Static, listing-independent budget breaches in the manifest itself."""
    budget = manifest.get("budget") if isinstance(manifest, dict) else None
    if not isinstance(budget, dict):
        return [f"{manifest_path} has no object-valued 'budget'"]
    families = budget.get("families")
    if not isinstance(families, dict) or not families:
        return [f"{manifest_path} budget.families must be a non-empty object"]
    problems: list[str] = []
    total_max = budget.get("total_max_bytes")
    fail_total = budget.get("fail_total_bytes")
    allocated = 0
    for family_id, spec in sorted(families.items()):
        if not isinstance(spec, dict) or not isinstance(spec.get("max_bytes"), int):
            problems.append(f"family {family_id!r} has no integer 'max_bytes'")
            continue
        allocated += spec["max_bytes"]
        policy = spec.get("evict")
        if policy is not None and policy not in EVICT_POLICIES:
            problems.append(
                f"family {family_id!r} declares unknown evict policy {policy!r}; "
                f"expected one of {', '.join(EVICT_POLICIES)}"
            )
        steady = spec.get("steady_state_bytes")
        if policy is None and isinstance(steady, int) and steady > spec["max_bytes"]:
            problems.append(
                f"family {family_id!r} is not evictable and its declared steady "
                f"state {steady / GIB:.2f} GiB cannot fit its "
                f"{spec['max_bytes'] / GIB:.2f} GiB allocation"
            )
    if isinstance(total_max, int) and allocated > total_max:
        problems.append(
            f"family allocations sum to {allocated / GIB:.2f} GiB, over "
            f"total_max_bytes {total_max / GIB:.2f} GiB"
        )
    if isinstance(total_max, int) and isinstance(fail_total, int):
        if total_max > fail_total:
            problems.append("total_max_bytes exceeds fail_total_bytes")
    return problems


def pr_owned_problems(
    manifest: dict, entries: list[CacheEntry], pr_ref: str
) -> list[str]:
    """Budget breaches the PR at `pr_ref` itself contributes to."""
    match = PR_REF.fullmatch(pr_ref)
    number = int(match["number"]) if match else None

    def is_mine(e: CacheEntry) -> bool:
        return e.ref == pr_ref or (number is not None and pr_number_of(e) == number)

    own = [e for e in entries if is_mine(e)]
    if not own:
        return []
    budget = manifest.get("budget") if isinstance(manifest, dict) else None
    families = budget.get("families") if isinstance(budget, dict) else None
    if not isinstance(families, dict):
        return []
    grouping = group_by_family(entries, parse_families(families))
    problems = [
        f"this PR ({pr_ref}) saved unregistered cache entry key={e.key!r}"
        for e in grouping.unmatched
        if is_mine(e)
    ]
    for group in sorted(grouping.groups, key=lambda g: g.family.family_id):
        family_id, family_entries = group.family.family_id, group.entries
        spec = group.family.spec
        max_bytes = spec.get("max_bytes") if spec is not None else None
        if not isinstance(max_bytes, int):
            continue
        used = sum(e.size_bytes for e in family_entries)
        mine = sum(e.size_bytes for e in family_entries if is_mine(e))
        if used > max_bytes and mine:
            problems.append(
                f"family {family_id!r} uses {used / GIB:.2f} GiB, over its "
                f"{max_bytes / GIB:.2f} GiB budget, and {mine / GIB:.2f} GiB of "
                f"that was saved by this PR ({pr_ref})"
            )
    fail_total = budget.get("fail_total_bytes") if isinstance(budget, dict) else None
    total = sum(e.size_bytes for e in entries)
    if isinstance(fail_total, int) and total > fail_total:
        mine = sum(e.size_bytes for e in own)
        problems.append(
            f"total usage {total / GIB:.2f} GiB exceeds fail_total_bytes "
            f"{fail_total / GIB:.2f} GiB, and {mine / GIB:.2f} GiB of it was "
            f"saved by this PR ({pr_ref})"
        )
    return problems


def report_warnings(problems: list[str]) -> None:
    """Surface non-blocking problems as annotations and in the job summary."""
    for problem in problems:
        print(f"::warning title=Actions cache budget::{problem}")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary and problems:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write("### Actions cache budget (warning, not blocking)\n\n")
            for problem in problems:
                handle.write(f"- {problem}\n")
            handle.write("\n")


def delete_pr_ref(repo: str, ref: str, entries: list[CacheEntry]) -> int:
    """Delete one closed PR's entries (by ref or `pr-<N>` tag). Never fails.

    A fork PR's `pull_request` token is read-only; a refused delete is logged
    and the next push-triggered sweep catches the entry.
    """
    match = PR_REF.fullmatch(ref)
    number = int(match["number"]) if match else None
    mine = [e for e in entries if number is not None and pr_number_of(e) == number]
    freed = sum(e.size_bytes for e in mine)
    print(f"closed PR {ref}: {len(mine)} entr(ies), {freed / GIB:.3f} GiB")
    for failure in apply_prune(mine, repo):
        print(f"  could not delete {failure}; the next janitor sweep will")
    return 0


# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:  # noqa: C901
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        default=MANIFEST,
        help="budget manifest (default: ci/cache-ownership.json)",
    )
    parser.add_argument(
        "--from-json",
        type=pathlib.Path,
        default=None,
        help="read a cache listing from this file instead of calling gh",
    )
    parser.add_argument(
        "--repo",
        default=DEFAULT_REPO,
        help=f"repository to query (default: {DEFAULT_REPO})",
    )
    parser.add_argument(
        "--require-live",
        action="store_true",
        help="fail rather than skip when the live cache listing is unavailable",
    )
    parser.add_argument(
        "--prune",
        action="store_true",
        help="report deletion candidates (dry run unless --apply is also given)",
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="with --prune, actually delete the candidates",
    )
    parser.add_argument(
        "--event-name",
        default="",
        help="triggering GitHub event; pull_request and non-main pushes warn",
    )
    parser.add_argument(
        "--ref",
        default="",
        help="triggering ref (a PR's refs/pull/N/merge in pull_request mode)",
    )
    parser.add_argument(
        "--sweep-only",
        action="store_true",
        help="with --prune --apply: report and delete, then exit 0 (the janitor)",
    )
    parser.add_argument(
        "--delete-ref",
        default=None,
        help="delete every entry saved from this closed PR ref, then exit 0",
    )
    args = parser.parse_args(argv)

    if args.delete_ref is not None:
        if not PR_REF.fullmatch(args.delete_ref):
            print(
                f"error: --delete-ref must be refs/pull/<N>/merge, got {args.delete_ref!r}"
            )
            return 1
        try:
            live = normalize_entries(fetch_live_entries(args.repo))
        except (
            OSError,
            subprocess.CalledProcessError,
            json.JSONDecodeError,
            ValueError,
        ) as error:
            print(
                f"closed PR {args.delete_ref}: listing unavailable ({error}); skipped"
            )
            return 0
        return delete_pr_ref(args.repo, args.delete_ref, live)

    if args.prune and args.from_json is not None:
        print(
            "error: --prune requires the live source; --from-json fixtures "
            "carry no cache ids to delete"
        )
        return 1

    usage_bytes: int | None
    if args.from_json is not None:
        try:
            listing = load_from_json(args.from_json)
        except (OSError, json.JSONDecodeError, ValueError) as error:
            print(f"error: cannot read {args.from_json}: {error}")
            return 1
        usage_bytes = listing.usage_bytes
        entries = normalize_entries(listing.entries)
    else:
        try:
            raw_entries = fetch_live_entries(args.repo)
        except (
            OSError,
            subprocess.CalledProcessError,
            json.JSONDecodeError,
            ValueError,
        ) as error:
            if args.require_live:
                print(f"error: required live cache listing unavailable ({error})")
                return 1
            print(f"check_cache_budget: skipped ({error})")
            return 0
        entries = normalize_entries(raw_entries)
        usage_bytes = fetch_live_usage_bytes(args.repo)

    try:
        manifest = load_manifest(args.manifest)
    except (OSError, ValueError) as error:  # JSONDecodeError, TOMLDecodeError
        print(f"error: cannot read {args.manifest} / ci.toml: {error}")
        return 1

    problems = budget_problems(args.manifest, manifest, entries)

    budget = manifest.get("budget") if isinstance(manifest, dict) else None
    if isinstance(budget, dict) and isinstance(budget.get("families"), dict):
        print(build_table(budget, entries, usage_bytes))
        print()

    if args.prune:
        try:
            current_main_lock = fetch_main_lock_hash(args.repo)
        except (
            OSError,
            subprocess.CalledProcessError,
            json.JSONDecodeError,
            ValueError,
        ) as error:
            print(
                f"prune: main Cargo.lock unavailable; cook lock pruning disabled ({error})"
            )
            current_main_lock = None
        now = datetime.datetime.now(datetime.timezone.utc)
        plan = plan_sweep(
            entries,
            manifest,
            current_main_lock,
            now,
            lambda number: fetch_pr_state(args.repo, number),
        )
        candidates, deferred = plan.delete, plan.deferred
        reclaimed = sum(e.size_bytes for e in candidates)
        pr_bytes = sum(e.size_bytes for e in candidates if PR_REF.fullmatch(e.ref))
        print(
            f"prune: {len(candidates)} candidate(s), {reclaimed / GIB:.2f} GiB reclaimable "
            f"({pr_bytes / GIB:.2f} GiB from closed PRs and evicted open-PR entries)"
        )
        for entry in candidates:
            print(f"  {entry.key} ({entry.ref}) {entry.size_bytes / GIB:.3f} GiB")
        for entry in deferred:
            print(
                f"  deferred, younger than {SWEEP_GRACE_SECONDS}s: {entry.key} ({entry.ref})"
            )
        candidate_ids = {id(entry) for entry in candidates}
        effective_entries = [e for e in entries if id(e) not in candidate_ids]
        if isinstance(budget, dict) and isinstance(budget.get("families"), dict):
            print(
                "raw usage is the table above; effective after safe prune (projected):"
            )
            print(build_table(budget, effective_entries, None))
            effective_problems = budget_problems(
                args.manifest, manifest, effective_entries
            )
            print(effective_verdict(effective_problems))
            for problem in effective_problems:
                print(f"  still over budget: {problem}")
        if args.apply:
            failures = apply_prune(candidates, args.repo)
            for failure in failures:
                print(f"  failed to delete {failure}")
        print()
        if args.sweep_only:
            # The janitor's job is to delete; the verdict is a separate step
            # that re-lists after the deletions landed.
            return 0

    mode = enforcement_mode(args.event_name, args.ref)
    blocking = forecast_problems(args.manifest, manifest)
    if mode == "fail":
        blocking.extend(problems)
    elif mode == "pr":
        blocking.extend(pr_owned_problems(manifest, entries, args.ref))
        report_warnings(problems)
    else:
        report_warnings(problems)

    if blocking:
        print("error: repository Actions-cache budget exceeded (soldr#3047):")
        for problem in blocking:
            print(f"  {problem}")
        return 1

    if problems:
        print(
            f"check_cache_budget: over budget, reported as a warning ({mode} mode: "
            "this run did not contribute to it)."
        )
        return 0
    print("check_cache_budget: repository Actions-cache usage is within budget.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
