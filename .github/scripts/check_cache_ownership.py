#!/usr/bin/env python3
"""Fail CI when a normal workflow persists a linked test product (soldr#2931).

soldr#2937 is phase 5 of soldr#2931. The policy it enforces is that cache
admission follows the stability of an artifact's identity key relative to its
size:

* **Tier 1 `cook`** -- external dependency compilation plus build-script /
  native outputs. Durable.
* **Tier 2 `zccache-unit`** -- per-compilation-unit, content-addressed
  compiler outputs. Durable, and safe by construction because the key *is*
  the hash of the compiler inputs.
* **Tier 3 `none`** -- everything else. In particular **linked test products
  are never cacheable**: test binaries, benches, examples built for tests,
  doctest products, test debug sidecars and test-specific incremental state
  may not enter cook, an rlib cache, a broad `target/` cache, a
  compiler-cache archive, or any other cross-run reusable store.

## The failure mode this prevents

A linked test binary is enormous and its identity key is the least stable
thing in the build: it moves with every source edit in the workspace. Storing
it buys a hit rate near zero while paying full upload, download and eviction
cost on every run -- and worse, it *displaces* entries whose keys are stable
(dependency rlibs, cc-rs outputs), so the store gets slower at the job it is
good at. soldr#1391 is the worked example in the other direction: a guard was
built that *required* the full linked nextest archive to be warm-cacheable at
zero misses. That invariant is now inverted, and this guard is what stops it
from creeping back in via a workflow edit.

The second rule covers the same damage arriving by a different route. Cook
restores a dependency-scoped tree. Restoring a **broad `target/` snapshot
after** it overwrites that tree with a bulk copy whose key is a whole-workspace
hash -- test binaries and `incremental/` state included. The cook restore then
bought nothing, and the store now carries exactly what tier 3 forbids. Order
matters: a broad restore *before* cook is fine, which is the shape
`_ci-cross-build-linux.yml` already uses.

## What is NOT flagged

Content-addressed per-unit caching stays live. soldr#2931 bans persisting
*linked products* and *bulk `target/` snapshots*; it does not disable zccache
dogfooding, and the soldr#1698 Windows cache-roundtrip probes depend on the
embedded wrapper being active in normal lanes. `zccache-unit` stores are
therefore never flagged by this guard.

`upload-artifact` / `download-artifact` are not cross-run reusable stores:
they are keyed by run id, never restored into a later run, and expire on
retention. A same-run transport bundle carrying cross-built test products to a
native runner is legal -- that is how `_ci-cross-build-linux.yml` reaches
`_ci-target-run.yml`. Its size budget is owned by those lanes, not here.

## Why a budget, not just a ceiling

soldr#3047 (Phase B of soldr#3039) found the repository's GitHub Actions cache
at 44.23 GiB against a 10 GB total-cache ceiling, evicted by LRU whenever it is
exceeded. Under LRU eviction against a hard ceiling, a persisted store is
only as reliable as the budget disciplining what lands in it -- a cache that
is full starts evicting whatever it feels like, including entries a healthy
lane depends on. A budget any producer can grow just by writing to a new key
is not a budget, it is a suggestion. `budget` in the manifest is the
enforced allocation: every family that may persist something declares its
own byte ceiling and the ids it owns, the family ceilings must not exceed
the total, and nothing may be added without being registered under a
family. That is what stops an unregistered producer from quietly consuming
another family's allocation.

## Rules

R1  The manifest (`ci/cache-ownership.json`) parses, declares
    `schema_version: 1`, has unique ids, uses only declared tiers and
    exceptions, and its `experiment_workflows` list matches
    `EXPERIMENT_WORKFLOWS` below exactly.
R2  Every workflow that persists something is covered by a manifest entry,
    and every manifest entry names a workflow that still persists something.
R3  In a non-exempt workflow, no durable cross-run store may name a test
    product (test/bench/example/doctest outputs, test debug sidecars,
    `incremental/` state, a nextest test archive).
R4  In a non-exempt workflow, no job may restore a broad `target/` snapshot
    *after* a cook restore in the same job.
R5  A manifest entry tiered `same-run-transport` must use an artifact
    mechanism, and a durable-cache mechanism may not claim that tier.
R6  The manifest declares a 'budget' whose per-family max_bytes sum to at
    most total_max_bytes, whose key_prefixes are disjoint across families,
    and whose fields are well typed.
R7  Every durable manifest entry in a non-experiment workflow is claimed
    by exactly one budget family, and every id a family claims exists.
R8  In a non-exempt workflow, every `actions/cache` or
    `actions/cache/restore` step with a `restore-keys` prefix fallback is an
    appending cache (the fallback restores another generation into the
    directory the job then writes to, and the save keeps both) and must be
    registered in `PRUNED_PREFIX_CACHES` with a prune/trim step that runs in
    the same job after the restore and before any explicit save of the same
    path (soldr#3396: the stable-cook cache grew to 2.8 GB this way). The
    `UNPRUNED_PREFIX_CACHES_RATCHET` list only shrinks: a stale entry fails.

Usage:
    python .github/scripts/check_cache_ownership.py
Options:
    --manifest PATH       policy manifest (default: ci/cache-ownership.json)
    --workflow-dir PATH   workflows to scan (default: .github/workflows)
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
from dataclasses import dataclass

import yaml

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
# pylint: disable-next=wrong-import-position,wrong-import-order
import cache_families  # noqa: E402 -- sibling-module bootstrap precedes this import

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW_DIR = REPO_ROOT / ".github" / "workflows"
MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"

SCHEMA_VERSION = 1

# The cache IS the subject under test in these workflows, so constraining what
# they persist would destroy the measurement. Frozen here rather than read from
# the manifest alone: a guard whose exemption list can be edited in the same
# file it guards is a guard that can be silently switched off. R1 requires the
# manifest to agree with this list, so the two can only change together.
EXPERIMENT_WORKFLOWS: frozenset[str] = frozenset(
    {
        "baseline-zero-deps.yml",
        "cache-delta-experiment.yml",
        "cook-size-gate.yml",
        "dylint-cache-acceptance.yml",
        "dylint-cook-acceptance.yml",
        "nextest-cacheability.yml",
        "parent-cache-bench.yml",
        "perf-cold-warm.yml",
        "perf-matrix.yml",
    }
)


@dataclass(frozen=True, order=True)
class PrefixCacheKey:
    """Identity of an appending prefix-fallback restore step."""

    workflow: str
    restore_step: str


@dataclass(frozen=True)
class PrefixCache:
    """A registered prefix-fallback cache and the step that prunes it.

    `note` is the prune step's name for `PRUNED_PREFIX_CACHES`; for the
    ratchet it names the issue that adds a prune step.
    """

    key: PrefixCacheKey
    note: str


# R8 (soldr#3396): appending prefix-fallback caches, keyed by
# (workflow file, restore step name), mapped to the name of the step in the
# same job that prunes or trims the restored directory before it is saved.
PRUNED_PREFIX_CACHES: tuple[PrefixCache, ...] = (
    PrefixCache(
        PrefixCacheKey(
            "_build-and-test.yml",
            "Restore Tier-2 zccache object store (soldr#3039)",
        ),
        "Measure, prune and trim the Tier-2 zccache store (soldr#3120, soldr#3252)",
    ),
    PrefixCache(
        PrefixCacheKey("setup-soldr-action.yml", "Restore dogfood build cache"),
        "Measure, prune and trim the dogfood zccache store (soldr#3398)",
    ),
)

# R8 ratchet: appending prefix-fallback caches that predate the rule and have
# no prune step yet, each with the issue that adds one. Entries may only be
# removed; one whose restore step no longer exists is itself a failure.
UNPRUNED_PREFIX_CACHES_RATCHET: tuple[PrefixCache, ...] = ()

PREFIX_CACHE_ACTIONS: frozenset[str] = frozenset(
    {"actions/cache", "actions/cache/restore"}
)

# `uses:` values (SHA stripped, lowercased) that persist something, mapped to
# the mechanism name the manifest uses.
PERSISTING_ACTIONS: dict[str, str] = {
    "actions/cache": "actions/cache",
    "actions/cache/restore": "actions/cache",
    "actions/cache/save": "actions/cache",
    "actions/upload-artifact": "actions/upload-artifact",
    "actions/download-artifact": "actions/download-artifact",
    "zackees/setup-soldr/cook": "setup-soldr/cook",
}

# setup-soldr gates each reuse layer on its own input, NOT on the `cache`
# umbrella (soldr#2451), so a layer counts as persisting only when a workflow
# turns it on explicitly. Every self-build lane in this repo spells out
# `false` for all of them; those spellings deliberately produce no entry.
SETUP_SOLDR_ACTION = "zackees/setup-soldr"
SETUP_SOLDR_LAYERS: tuple[str, ...] = (
    "cache",
    "build-cache",
    "target-cache",
    "cargo-registry-cache",
    "soldr-mini-cache",
    "solo-toolchain-cache",
    "ci-tests",
)

# Mechanisms that persist across runs and can therefore be *reused* by a later
# build. Artifact upload/download is excluded on purpose: it is run-keyed.
DURABLE_MECHANISMS: frozenset[str] = frozenset({"actions/cache", "setup-soldr/cook"})

ARTIFACT_MECHANISMS: frozenset[str] = frozenset(
    {"actions/upload-artifact", "actions/download-artifact"}
)


# Tier 3 shapes, named so the error message says what was found rather than
# which regex fired. Matched against a step's `path` / `flags` / `key` text.
@dataclass(frozen=True)
class ProductShape:
    """A named Tier 3 shape and the regex that recognises it."""

    label: str
    pattern: re.Pattern[str]


TEST_PRODUCT_PATTERNS: tuple[ProductShape, ...] = (
    ProductShape("a nextest test archive", re.compile(r"tests?\.tar", re.IGNORECASE)),
    ProductShape(
        "a nextest archive", re.compile(r"nextest[-_ ]archive", re.IGNORECASE)
    ),
    ProductShape(
        "compiled test/bench/example unit outputs",
        re.compile(r"target/[^\s'\"]*/(deps|examples|benches)(/|\b)", re.IGNORECASE),
    ),
    ProductShape(
        "cargo incremental state", re.compile(r"\bincremental\b", re.IGNORECASE)
    ),
    ProductShape("doctest products", re.compile(r"doctest", re.IGNORECASE)),
    ProductShape(
        "test debug sidecars", re.compile(r"\.(pdb|dsym|dwp)\b", re.IGNORECASE)
    ),
    ProductShape(
        "linked test binaries",
        re.compile(r"test[-_]?(bin|binaries|harness|exe)", re.IGNORECASE),
    ),
)

# A path that names the whole cargo target directory rather than a slice of it.
BROAD_TARGET_PATHS: frozenset[str] = frozenset(
    {"target", "target/", "./target", "./target/", "target/**", "./target/**"}
)


@dataclass(frozen=True)
class PersistedStep:
    """One workflow step that writes to or reads from a persistent store."""

    workflow: str
    job: str
    index: int
    name: str
    mechanism: str
    text: str

    @property
    def durable(self) -> bool:
        """Whether a later run can reuse what this step persists.

        Every enabled `setup-soldr:<layer>` counts. The zccache layers are
        content-addressed per unit, so tier 3 cannot normally pollute them --
        a unit key is the hash of that unit's compiler inputs. They are still
        checked here because a *path* naming a test product would mean the
        layer had been pointed somewhere it does not belong.
        """
        return self.mechanism in DURABLE_MECHANISMS or self.mechanism.startswith(
            "setup-soldr:"
        )

    def describe(self) -> str:
        label = self.name or f"(unnamed step #{self.index})"
        return f"{self.workflow}: step '{label}' ({self.mechanism})"


def action_id(uses: str) -> str:
    """`owner/repo/sub@sha` -> `owner/repo/sub`, lowercased."""
    return uses.split("@", 1)[0].strip().rstrip("/").lower()


def _truthy(value: object) -> bool:
    """Whether a `with:` input turns a layer on.

    YAML gives `true` as a bool and `'true'` as a string, and workflows in
    this repo use both spellings; an expression like `${{ ... && 'false' ||
    'true' }}` is neither, and is treated as on because it *can* be on.
    """
    if isinstance(value, bool):
        return value
    text = str(value).strip().lower()
    if text in {"false", "off", "no", "0", "", "none"}:
        return False
    return True


def step_text(step: dict) -> str:
    """The step inputs a path/key rule needs to inspect, joined."""
    with_block = step.get("with")
    if not isinstance(with_block, dict):
        return ""
    keys = (
        "path",
        "flags",
        "key",
        "restore-keys",
        "shared-key",
        "target-dir",
        "target-cache-mode",
        "cache-dir",
    )
    return "\n".join(str(with_block[k]) for k in keys if with_block.get(k) is not None)


def collect_steps(workflow_dir: pathlib.Path) -> list[PersistedStep]:
    """Every persisting step in every workflow, in file and step order."""
    found: list[PersistedStep] = []
    paths = sorted(workflow_dir.glob("*.yml")) + sorted(workflow_dir.glob("*.yaml"))
    for path in paths:
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
        if not isinstance(document, dict):
            continue
        for job_id, job in (document.get("jobs") or {}).items():
            if not isinstance(job, dict):
                continue
            for index, step in enumerate(job.get("steps") or []):
                if not isinstance(step, dict):
                    continue
                found.extend(
                    _classify(path.name, str(job_id), index, step),
                )
    return found


def _classify(workflow: str, job: str, index: int, step: dict) -> list[PersistedStep]:
    uses = step.get("uses")
    if not isinstance(uses, str):
        return []
    identifier = action_id(uses)
    name = str(step.get("name") or "")
    text = step_text(step)

    mechanism = PERSISTING_ACTIONS.get(identifier)
    if mechanism is not None:
        return [PersistedStep(workflow, job, index, name, mechanism, text)]

    if identifier != SETUP_SOLDR_ACTION:
        return []

    with_block = step.get("with")
    if not isinstance(with_block, dict):
        return []
    enabled = [
        layer
        for layer in SETUP_SOLDR_LAYERS
        if layer in with_block and _truthy(with_block[layer])
    ]
    # `prebuild-deps: soldr-cook` runs setup-soldr's early cook. No workflow
    # selects it today (both callers pass `none`), but it is the same store as
    # the `setup-soldr/cook` subaction and must be classified if it returns.
    prebuild = str(with_block.get("prebuild-deps", "none")).strip().lower()
    if prebuild not in {"none", "false", ""}:
        enabled.append("cook")
    return [
        PersistedStep(workflow, job, index, name, f"setup-soldr:{layer}", text)
        for layer in enabled
    ]


# --------------------------------------------------------------------------
# R1 / R5 -- the manifest itself
# --------------------------------------------------------------------------


def manifest_problems(manifest: dict) -> list[str]:  # noqa: C901
    """Schema, vocabulary and exemption-agreement failures (R1, R5)."""
    problems: list[str] = []

    version = manifest.get("schema_version")
    if version != SCHEMA_VERSION:
        problems.append(
            f"R1 manifest schema_version is {version!r}, expected {SCHEMA_VERSION}"
        )

    tiers = manifest.get("tiers")
    exceptions = manifest.get("exceptions")
    if not isinstance(tiers, dict) or not isinstance(exceptions, dict):
        problems.append(
            "R1 manifest must declare object-valued 'tiers' and 'exceptions'"
        )
        return problems
    vocabulary = set(tiers) | set(exceptions)

    declared = manifest.get("experiment_workflows")
    if not isinstance(declared, list) or set(declared) != set(EXPERIMENT_WORKFLOWS):
        problems.append(
            "R1 manifest 'experiment_workflows' must match EXPERIMENT_WORKFLOWS in "
            f"{pathlib.Path(__file__).name} exactly.\n"
            f"     manifest: {sorted(declared) if isinstance(declared, list) else declared}\n"
            f"     guard:    {sorted(EXPERIMENT_WORKFLOWS)}\n"
            "     Both must change together, so the exemption list cannot be "
            "widened by editing only the file being guarded."
        )

    layers = manifest.get("setup_soldr_layers")
    if isinstance(layers, dict):
        for layer, spec in layers.items():
            if not isinstance(spec, dict):
                continue  # the free-text 'comment' key
            if spec.get("tier") not in vocabulary:
                problems.append(
                    f"R1 setup_soldr_layers[{layer!r}] declares unknown tier "
                    f"{spec.get('tier')!r}"
                )

    entries = manifest.get("entries")
    if not isinstance(entries, list):
        problems.append("R1 manifest must declare a list of 'entries'")
        return problems

    seen: set[str] = set()
    for entry in entries:
        if not isinstance(entry, dict):
            problems.append(f"R1 manifest entry is not an object: {entry!r}")
            continue
        entry_id = str(entry.get("id", ""))
        if not entry_id:
            problems.append(f"R1 manifest entry has no id: {entry!r}")
            continue
        if entry_id in seen:
            problems.append(f"R1 duplicate manifest entry id {entry_id!r}")
        seen.add(entry_id)
        for field in ("workflow", "step", "mechanism", "tier", "rationale"):
            if not str(entry.get(field, "")).strip():
                problems.append(f"R1 entry {entry_id!r} is missing '{field}'")
        tier = entry.get("tier")
        if tier not in vocabulary:
            problems.append(
                f"R1 entry {entry_id!r} declares unknown tier {tier!r}; "
                f"declare it under 'tiers' or 'exceptions' first"
            )
        mechanism = str(entry.get("mechanism", ""))
        if tier == "same-run-transport" and mechanism not in ARTIFACT_MECHANISMS:
            problems.append(
                f"R5 entry {entry_id!r} claims 'same-run-transport' with mechanism "
                f"{mechanism!r}. Transport is run-keyed upload/download-artifact; a "
                "cross-run cache cannot be transport."
            )
        if mechanism in DURABLE_MECHANISMS and tier == "same-run-transport":
            problems.append(
                f"R5 entry {entry_id!r} uses the cross-run mechanism {mechanism!r} "
                "and may not claim 'same-run-transport'."
            )
    return problems


# --------------------------------------------------------------------------
# R6 -- the budget: schema, per-family typing, disjoint key_prefixes
# --------------------------------------------------------------------------


def budget_problems(manifest: dict) -> list[str]:  # noqa: C901
    """Schema and typing failures in `manifest['budget']` (R6).

    A family with a bad shape is still recorded (with an empty key_prefixes
    / omitted max_bytes) so the disjointness and sum checks below can run
    against whatever the other families declare, rather than bailing out on
    the first malformed entry.
    """
    problems: list[str] = []

    budget = manifest.get("budget")
    if not isinstance(budget, dict):
        problems.append("R6 manifest must declare an object-valued 'budget'")
        return problems

    total_max_bytes = budget.get("total_max_bytes")
    if not isinstance(total_max_bytes, int) or isinstance(total_max_bytes, bool):
        problems.append(
            f"R6 budget.total_max_bytes must be an int, got {total_max_bytes!r}"
        )
        total_max_bytes = None

    fail_total_bytes = budget.get("fail_total_bytes")
    if not isinstance(fail_total_bytes, int) or isinstance(fail_total_bytes, bool):
        problems.append(
            f"R6 budget.fail_total_bytes must be an int, got {fail_total_bytes!r}"
        )
        fail_total_bytes = None

    if (
        total_max_bytes is not None
        and fail_total_bytes is not None
        and fail_total_bytes < total_max_bytes
    ):
        problems.append(
            f"R6 budget.fail_total_bytes ({fail_total_bytes}) must be >= "
            f"budget.total_max_bytes ({total_max_bytes}); fail is the hard "
            "ceiling that must sit at or above the soft one."
        )

    families = budget.get("families")
    if not isinstance(families, dict) or not families:
        problems.append("R6 budget.families must be a non-empty object")
        return problems

    max_bytes_by_family: dict[str, int] = {}
    prefixes_by_family: dict[str, list[str]] = {}
    for family_id, family in families.items():
        if not isinstance(family, dict):
            problems.append(f"R6 budget.families[{family_id!r}] is not an object")
            continue

        key_prefixes = family.get("key_prefixes")
        if (
            not isinstance(key_prefixes, list)
            or not key_prefixes
            or not all(isinstance(p, str) for p in key_prefixes)
        ):
            problems.append(
                f"R6 budget.families[{family_id!r}].key_prefixes must be a "
                "non-empty list of strings"
            )
            key_prefixes = []
        else:
            seen_prefixes: set[str] = set()
            for prefix in key_prefixes:
                if prefix in seen_prefixes:
                    problems.append(
                        f"R6 budget.families[{family_id!r}] declares duplicate "
                        f"key_prefix {prefix!r}"
                    )
                seen_prefixes.add(prefix)
        prefixes_by_family[family_id] = key_prefixes

        max_bytes = family.get("max_bytes")
        if not isinstance(max_bytes, int) or isinstance(max_bytes, bool):
            problems.append(
                f"R6 budget.families[{family_id!r}].max_bytes must be an int, "
                f"got {max_bytes!r}"
            )
        else:
            max_bytes_by_family[family_id] = max_bytes

        entries = family.get("entries")
        if not isinstance(entries, list) or not all(
            isinstance(e, str) for e in entries
        ):
            problems.append(
                f"R6 budget.families[{family_id!r}].entries must be a list of strings"
            )

        if not str(family.get("rationale", "")).strip():
            problems.append(
                f"R6 budget.families[{family_id!r}] is missing a 'rationale'"
            )

    if max_bytes_by_family and total_max_bytes is not None:
        total = sum(max_bytes_by_family.values())
        if total > total_max_bytes:
            problems.append(
                f"R6 budget family max_bytes sum to {total}, over "
                f"budget.total_max_bytes ({total_max_bytes}).\n"
                "     Lower a family's max_bytes or raise total_max_bytes -- the "
                "sum is the enforceable ceiling, not an aspiration."
            )

    family_ids = sorted(prefixes_by_family)
    for i, family_a in enumerate(family_ids):
        for family_b in family_ids[i + 1 :]:
            for prefix_a in prefixes_by_family[family_a]:
                for prefix_b in prefixes_by_family[family_b]:
                    if prefix_a.startswith(prefix_b) or prefix_b.startswith(prefix_a):
                        problems.append(
                            f"R6 key_prefix {prefix_a!r} (family {family_a!r}) "
                            f"overlaps {prefix_b!r} (family {family_b!r}).\n"
                            "     Grouping is longest-match, so overlapping "
                            "prefixes across families make attribution "
                            "ambiguous. Narrow one prefix or merge the families."
                        )
    return problems


# --------------------------------------------------------------------------
# R7 -- the budget: every claim resolves, every durable entry is claimed once
# --------------------------------------------------------------------------


def budget_coverage_problems(manifest: dict) -> list[str]:  # noqa: C901
    """Family/entry claim resolution and durable-entry coverage (R7).

    Skipped when `budget.families` itself is malformed -- `budget_problems`
    already reported that, and there is nothing coherent to cross-check
    against here.
    """
    problems: list[str] = []

    entries = [e for e in manifest.get("entries") or [] if isinstance(e, dict)]
    entries_by_id = {str(e.get("id", "")): e for e in entries}

    budget = manifest.get("budget")
    families = budget.get("families") if isinstance(budget, dict) else None
    if not isinstance(families, dict):
        return problems

    claims: dict[str, list[str]] = {}
    for family_id, family in families.items():
        if not isinstance(family, dict):
            continue
        claimed = family.get("entries")
        if not isinstance(claimed, list):
            continue
        if not claimed and not family.get("reserved"):
            problems.append(
                f"R7 budget.families[{family_id!r}] claims no entries and is "
                "not marked 'reserved'.\n"
                "     An empty family is only legal as a deliberate future "
                'allocation -- set "reserved": true, or claim at least one '
                "entry."
            )
        for entry_id in claimed:
            if not isinstance(entry_id, str):
                continue
            if entry_id not in entries_by_id:
                problems.append(
                    f"R7 budget.families[{family_id!r}] claims {entry_id!r}, "
                    "which no manifest entry defines."
                )
                continue
            claims.setdefault(entry_id, []).append(family_id)

    multiply_claimed: set[str] = set()
    for entry_id, claiming_families in claims.items():
        if len(claiming_families) > 1:
            multiply_claimed.add(entry_id)
            problems.append(
                f"R7 entry {entry_id!r} is claimed by more than one budget "
                f"family: {sorted(claiming_families)}.\n"
                "     Each id may belong to exactly one family, or its bytes "
                "would be double-counted against the budget."
            )

    for entry in entries:
        entry_id = str(entry.get("id", ""))
        if entry_id in multiply_claimed:
            continue
        workflow = str(entry.get("workflow", ""))
        mechanism = str(entry.get("mechanism", ""))
        durable = mechanism in DURABLE_MECHANISMS or mechanism.startswith(
            "setup-soldr:"
        )
        if not durable or workflow in EXPERIMENT_WORKFLOWS:
            continue
        claimants = claims.get(entry_id, [])
        if len(claimants) != 1:
            problems.append(
                f"R7 entry {entry_id!r} (mechanism {mechanism!r}) is a durable "
                "store in a non-experiment workflow but is claimed by "
                f"{len(claimants)} budget families; it must be claimed by "
                "exactly one.\n"
                "     Add its id to the entries list of the family that owns "
                "its bytes."
            )
    return problems


# --------------------------------------------------------------------------
# R2 -- coverage, in both directions
# --------------------------------------------------------------------------


def coverage_problems(manifest: dict, steps: list[PersistedStep]) -> list[str]:
    """Workflows persisting without an entry, and entries that have rotted."""
    problems: list[str] = []
    entries = [e for e in manifest.get("entries") or [] if isinstance(e, dict)]
    covered = {str(e.get("workflow", "")) for e in entries}
    persisting = {step.workflow for step in steps}

    for workflow in sorted(persisting - covered):
        problems.append(
            f"R2 {workflow} persists something but has no entry in the cache "
            "ownership manifest.\n"
            "     Add one naming the step, its tier (cook | zccache-unit | none | "
            "a declared exception) and a one-line rationale. Classification is by "
            "CONTENTS, not by whether GitHub calls the store a cache."
        )
    for workflow in sorted(covered - persisting):
        problems.append(
            f"R2 the manifest has entries for {workflow}, which no longer persists "
            "anything. Remove them -- a manifest that records stores that are gone "
            "cannot be read to find the ones that are real."
        )
    return problems


# --------------------------------------------------------------------------
# R3 -- no test products in a durable store
# --------------------------------------------------------------------------


def banned_product_problems(steps: list[PersistedStep]) -> list[str]:
    """Durable stores in normal workflows that name a linked test product.

    Named `banned_product_problems` rather than `test_product_problems` so
    pytest never mistakes the guard's own helper for a test case.
    """
    problems: list[str] = []
    for step in steps:
        if step.workflow in EXPERIMENT_WORKFLOWS or not step.durable:
            continue
        for shape in TEST_PRODUCT_PATTERNS:
            label = shape.label
            match = re.search(shape.pattern, step.text)
            if match is None:
                continue
            problems.append(
                f"R3 {step.describe()} writes {label} into a cross-run reusable "
                f"store (matched {match.group(0)!r}).\n"
                "     Linked test products are tier 3 (soldr#2931): test binaries, "
                "benches, examples built for tests, doctest products, test debug "
                "sidecars and test incremental state may not enter cook, an rlib "
                "cache, a broad target/ cache, or a compiler-cache archive.\n"
                "     If this is a same-run handoff to another job, move it to "
                "upload-artifact/download-artifact and classify it as "
                "'same-run-transport' in ci/cache-ownership.json."
            )
            break
    return problems


# --------------------------------------------------------------------------
# R4 -- no broad target/ restore downstream of cook
# --------------------------------------------------------------------------


def is_broad_target_restore(step: PersistedStep) -> bool:
    """Does this step restore the whole cargo target directory?

    `actions/cache` does when its path names the target directory itself
    rather than a slice of it. setup-soldr's
    `target-cache` in `full` mode does too -- `thin` is the dependency-scoped
    slice and is fine.
    """
    if step.mechanism == "setup-soldr:target-cache":
        return "full" in step.text.lower()
    if step.mechanism != "actions/cache":
        return False
    return any(line.strip() in BROAD_TARGET_PATHS for line in step.text.splitlines())


@dataclass(frozen=True, order=True)
class JobKey:
    """A job within a workflow file."""

    workflow: str
    job: str


def cook_ordering_problems(steps: list[PersistedStep]) -> list[str]:
    """Broad `target/` restores that land after a cook restore in the same job."""
    problems: list[str] = []
    jobs: dict[JobKey, list[PersistedStep]] = {}
    for step in steps:
        if step.workflow in EXPERIMENT_WORKFLOWS:
            continue
        jobs.setdefault(JobKey(step.workflow, step.job), []).append(step)

    for job_key, job_steps in sorted(jobs.items()):
        workflow, job = job_key.workflow, job_key.job
        ordered = sorted(job_steps, key=lambda s: s.index)
        cook = next(
            (
                s
                for s in ordered
                if s.mechanism in {"setup-soldr/cook", "setup-soldr:cook"}
            ),
            None,
        )
        if cook is None:
            continue
        for step in ordered:
            if step.index <= cook.index or not is_broad_target_restore(step):
                continue
            problems.append(
                f"R4 {workflow}: job '{job}' restores a broad target/ snapshot in "
                f"step '{step.name or step.index}' ({step.mechanism}) AFTER the cook "
                f"restore in step '{cook.name or cook.index}'.\n"
                "     The bulk restore overwrites cook's dependency-scoped tree with "
                "a snapshot keyed on a whole-workspace hash -- test binaries and "
                "incremental state included -- so cook bought nothing and the store "
                "now carries what tier 3 forbids (soldr#2931).\n"
                "     Move the broad restore BEFORE the cook step, or narrow it."
            )
    return problems


# --------------------------------------------------------------------------
# R8 -- appending prefix-fallback caches need a registered prune step
# --------------------------------------------------------------------------


def _path_lines(step: dict) -> frozenset[str]:
    with_block = step.get("with")
    raw = with_block.get("path") if isinstance(with_block, dict) else None
    return frozenset(
        line.strip() for line in str(raw or "").splitlines() if line.strip()
    )


def prefix_cache_problems(  # noqa: C901
    workflow_dir: pathlib.Path,
    pruned: tuple[PrefixCache, ...] | None = None,
    ratchet: tuple[PrefixCache, ...] | None = None,
) -> list[str]:
    """Appending `restore-keys` caches without a registered prune step (R8)."""
    pruned_by_key = {
        entry.key: entry.note
        for entry in (PRUNED_PREFIX_CACHES if pruned is None else pruned)
    }
    ratchet_keys = {
        entry.key
        for entry in (UNPRUNED_PREFIX_CACHES_RATCHET if ratchet is None else ratchet)
    }
    problems: list[str] = []
    seen: set[PrefixCacheKey] = set()
    paths = sorted(workflow_dir.glob("*.yml")) + sorted(workflow_dir.glob("*.yaml"))
    for path in paths:
        if path.name in EXPERIMENT_WORKFLOWS:
            continue
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
        if not isinstance(document, dict):
            continue
        for job_id, job in (document.get("jobs") or {}).items():
            if not isinstance(job, dict):
                continue
            steps = [s for s in (job.get("steps") or []) if isinstance(s, dict)]
            for index, step in enumerate(steps):
                uses = step.get("uses")
                if not isinstance(uses, str):
                    continue
                if action_id(uses) not in PREFIX_CACHE_ACTIONS:
                    continue
                with_block = step.get("with")
                if not isinstance(with_block, dict):
                    continue
                if not str(with_block.get("restore-keys") or "").strip():
                    continue
                name = str(step.get("name") or "")
                key = PrefixCacheKey(path.name, name)
                seen.add(key)
                where = f"{path.name}: job '{job_id}' step '{name or index}'"
                if key in ratchet_keys:
                    continue
                prune_name = pruned_by_key.get(key)
                if prune_name is None:
                    problems.append(
                        f"R8 {where} restores through a `restore-keys` prefix "
                        "into a directory the job appends to, and no prune step "
                        "is registered for it.\n"
                        "     A prefix hit restores a superseded generation; the "
                        "save then keeps old and new together, so the entry grows "
                        "with every key change (soldr#3396: stable-cook reached "
                        "2.8 GB).\n"
                        "     Drop the prefix fallback, or add a step that prunes "
                        "what the current key does not use and register it in "
                        "PRUNED_PREFIX_CACHES in check_cache_ownership.py."
                    )
                    continue
                prune_index = next(
                    (
                        i
                        for i, other in enumerate(steps)
                        if i > index and str(other.get("name") or "") == prune_name
                    ),
                    None,
                )
                if prune_index is None:
                    problems.append(
                        f"R8 {where} is registered with prune step "
                        f"'{prune_name}', but no step of that name runs after it "
                        "in the same job."
                    )
                    continue
                restored = _path_lines(step)
                for later in steps[index + 1 : prune_index]:
                    later_uses = later.get("uses")
                    if (
                        isinstance(later_uses, str)
                        and action_id(later_uses) == "actions/cache/save"
                        and _path_lines(later) & restored
                    ):
                        problems.append(
                            f"R8 {where}: '{later.get('name') or 'save'}' saves "
                            f"the restored path before the prune step "
                            f"'{prune_name}' runs."
                        )
    registered = set(pruned_by_key) | ratchet_keys
    for missing in sorted(registered - seen):
        workflow, name = missing.workflow, missing.restore_step
        # A registration for a workflow outside this tree is not stale here.
        if not (workflow_dir / workflow).is_file():
            continue
        problems.append(
            f"R8 {workflow}: registered prefix-fallback cache step '{name}' no "
            "longer exists; remove it from PRUNED_PREFIX_CACHES / "
            "UNPRUNED_PREFIX_CACHES_RATCHET (the ratchet only shrinks)."
        )
    return problems


# --------------------------------------------------------------------------


def load_manifest(path: pathlib.Path) -> dict:
    """The manifest, its `budget` composed from ci.toml [cache.family] (soldr#3618)."""
    return cache_families.load_manifest(path)


def _budget_families_gib(manifest: dict) -> float:
    """Sum of every family's `max_bytes`, in GiB.

    Only called once `budget_problems` has already come back clean, so every
    family present is well typed and `max_bytes` is safe to sum directly.
    """
    budget = manifest.get("budget")
    families = budget.get("families") if isinstance(budget, dict) else None
    if not isinstance(families, dict):
        return 0.0
    total_bytes = sum(
        family.get("max_bytes", 0)
        for family in families.values()
        if isinstance(family, dict)
    )
    return total_bytes / (1024**3)


def check(manifest_path: pathlib.Path, workflow_dir: pathlib.Path) -> list[str]:
    """Every policy failure, as actionable lines. Empty means the tree is clean."""
    try:
        manifest = load_manifest(manifest_path)
    except (OSError, ValueError) as error:  # JSON/TOML decode, CacheFamilyError
        return [f"R1 cannot read {manifest_path} / ci.toml: {error}"]
    if not isinstance(manifest, dict):
        return [f"R1 {manifest_path} must contain a JSON object"]

    problems = manifest_problems(manifest)
    problems += budget_problems(manifest)
    problems += budget_coverage_problems(manifest)
    steps = collect_steps(workflow_dir)
    problems += coverage_problems(manifest, steps)
    problems += banned_product_problems(steps)
    problems += cook_ordering_problems(steps)
    problems += prefix_cache_problems(workflow_dir)
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        default=MANIFEST,
        help="cache ownership manifest (default: ci/cache-ownership.json)",
    )
    parser.add_argument(
        "--workflow-dir",
        type=pathlib.Path,
        default=WORKFLOW_DIR,
        help="directory of workflow YAML to scan (default: .github/workflows)",
    )
    args = parser.parse_args(argv)

    problems = check(args.manifest, args.workflow_dir)
    if problems:
        print("error: cache ownership policy violated (soldr#2931 / soldr#2937):")
        for problem in problems:
            print(f"  {problem}")
        print()
        print(
            "Cache admission follows the stability of an artifact's identity key\n"
            "relative to its size. Linked test products have the least stable key\n"
            "in the build and the largest size, so they are never cacheable.\n"
            "See ci/cache-ownership.json for the tier definitions and the list of\n"
            "named exceptions."
        )
        return 1

    manifest = load_manifest(args.manifest)
    print(
        "check_cache_ownership: every persisted store is classified; no test "
        "products in cross-run stores; budget families sum to "
        f"{_budget_families_gib(manifest):.2f} GiB."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
