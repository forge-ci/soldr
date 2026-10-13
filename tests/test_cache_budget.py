"""The repository Actions-cache budget guard (soldr#3047 Phase B).

`check_cache_budget.py` groups every live GitHub Actions cache entry into the
family whose `key_prefixes` is the longest match, then fails when an entry
matches no family, when a family exceeds its own allocation, or when the
total exceeds the hard ceiling.

`test_captured_fixture_is_red` is the acceptance item for soldr#3047: the real
`gh cache list` snapshot that motivated this guard (44.23 GiB across 143
entries) must fail it. Everything else is built from small synthetic
manifests/listings so each failure mode is exercised in isolation, in the same
style as `tests/test_cache_ownership.py`.
"""

from __future__ import annotations

import base64
import hashlib
import json
from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
_SCRIPT = REPO_ROOT / ".github" / "scripts" / "check_cache_budget.py"
guard = load_script_module(_SCRIPT, "check_cache_budget")

MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"
FIXTURE = REPO_ROOT / "tests" / "fixtures" / "actions-cache" / "listing-2026-09-01.json"


def write_manifest(tmp_path: Path, budget: dict) -> Path:
    path = tmp_path / "cache-ownership.json"
    path.write_text(json.dumps({"budget": budget}), encoding="utf-8")
    return path


def write_listing(
    tmp_path: Path, entries: list[dict], name: str = "listing.json"
) -> Path:
    path = tmp_path / name
    path.write_text(
        json.dumps({"captured": "test", "usage_bytes": None, "entries": entries}),
        encoding="utf-8",
    )
    return path


def family(prefix: str, max_bytes: int) -> dict:
    return {
        "key_prefixes": [prefix],
        "max_bytes": max_bytes,
        "entries": [],
        "rationale": "fixture",
    }


def entry(key: str, size_bytes: int, ref: str = "refs/heads/main") -> dict:
    return {"key": key, "ref": ref, "sizeInBytes": size_bytes}


# --------------------------------------------------------------------------
# Acceptance: the real 2026-09-01 listing must be RED
# --------------------------------------------------------------------------


def test_captured_fixture_is_red(capsys: pytest.CaptureFixture[str]) -> None:
    code = guard.main(["--manifest", str(MANIFEST), "--from-json", str(FIXTURE)])
    assert code == 1
    out = capsys.readouterr().out
    assert "v0-rust-cross-build" in out


# --------------------------------------------------------------------------
# A synthetic listing built from the real manifest
# --------------------------------------------------------------------------


def test_half_budget_synthetic_listing_from_real_manifest_passes(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest = guard.load_manifest(MANIFEST)
    families = manifest["budget"]["families"]

    entries = []
    for spec in families.values():
        prefix = spec["key_prefixes"][0]
        entries.append(entry(f"{prefix}half-budget-fixture", spec["max_bytes"] // 2))

    listing_path = write_listing(tmp_path, entries)
    code = guard.main(["--manifest", str(MANIFEST), "--from-json", str(listing_path)])
    assert code == 0, capsys.readouterr().out


# --------------------------------------------------------------------------
# Synthetic failures, one rule at a time
# --------------------------------------------------------------------------


def test_unregistered_key_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest_path = write_manifest(
        tmp_path,
        {
            "total_max_bytes": 1000,
            "fail_total_bytes": 1000,
            "families": {"fam-a": family("a-", 500)},
        },
    )
    listing_path = write_listing(tmp_path, [entry("totally-unregistered-key-zzz", 10)])

    code = guard.main(
        ["--manifest", str(manifest_path), "--from-json", str(listing_path)]
    )
    assert code == 1
    out = capsys.readouterr().out
    assert "totally-unregistered-key-zzz" in out


def test_family_over_its_own_budget_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest_path = write_manifest(
        tmp_path,
        {
            "total_max_bytes": 1000,
            "fail_total_bytes": 1000,
            "families": {"fam-a": family("a-", 100)},
        },
    )
    listing_path = write_listing(tmp_path, [entry("a-1", 150)])

    code = guard.main(
        ["--manifest", str(manifest_path), "--from-json", str(listing_path)]
    )
    assert code == 1
    out = capsys.readouterr().out
    assert "fam-a" in out


def test_families_under_but_total_over_fail_total_bytes_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest_path = write_manifest(
        tmp_path,
        {
            "total_max_bytes": 2000,
            "fail_total_bytes": 1000,
            "families": {
                "fam-a": family("a-", 1000),
                "fam-b": family("b-", 1000),
            },
        },
    )
    listing_path = write_listing(
        tmp_path,
        [entry("a-1", 600), entry("b-1", 600)],
    )

    code = guard.main(
        ["--manifest", str(manifest_path), "--from-json", str(listing_path)]
    )
    assert code == 1
    out = capsys.readouterr().out
    assert "fail_total_bytes" in out


# --------------------------------------------------------------------------
# The real manifest's budget object must agree with itself
# --------------------------------------------------------------------------


def test_manifest_budget_is_self_consistent() -> None:
    manifest = guard.load_manifest(MANIFEST)
    budget = manifest["budget"]
    families = budget["families"]

    assert (
        sum(spec["max_bytes"] for spec in families.values())
        == budget["total_max_bytes"]
    )
    # soldr#3618: the allocations are ci.toml footprints. They may not loosen
    # past the 8.5 GiB `total_max_bytes` CACHE-025 left (zackees/ci.yml,
    # maintainer decision 2026-10-02), and the hard ceiling is ci.toml's
    # `[cache].budget`, the former `fail_total_bytes` of 9.5 GiB.
    assert budget["total_max_bytes"] <= 9126805504
    assert budget["fail_total_bytes"] == 10200547328

    owned_prefixes = [
        (prefix, family_id)
        for family_id, spec in families.items()
        for prefix in spec["key_prefixes"]
    ]
    for prefix_a, family_a in owned_prefixes:
        for prefix_b, family_b in owned_prefixes:
            if family_a == family_b:
                continue
            assert not prefix_b.startswith(prefix_a), (
                f"{prefix_a!r} ({family_a}) is a prefix of {prefix_b!r} ({family_b})"
            )


# --------------------------------------------------------------------------
# Network policy: skip, never fail, when gh is unavailable
# --------------------------------------------------------------------------


def test_live_mode_skips_when_gh_is_unavailable(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def boom(_args: list[str]) -> str:
        raise FileNotFoundError("gh")

    monkeypatch.setattr(guard, "run_gh", boom)

    code = guard.main(["--manifest", str(MANIFEST)])
    assert code == 0
    assert "skipped" in capsys.readouterr().out


def test_required_live_mode_fails_when_gh_is_unavailable(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def boom(_args: list[str]) -> str:
        raise FileNotFoundError("gh")

    monkeypatch.setattr(guard, "run_gh", boom)

    code = guard.main(["--manifest", str(MANIFEST), "--require-live"])
    assert code == 1
    assert "required live cache listing unavailable" in capsys.readouterr().out


def test_numeric_cache_ids_from_gh_survive_normalization() -> None:
    # `gh cache list --json id` returns numbers; a str-only check dropped
    # every id and made `--prune --apply` a no-op (2026-09-04).
    raw = [
        {**entry("v0-rust-x", 1, ref="refs/pull/1/merge"), "id": 7258560456},
        {**entry("v0-rust-y", 1, ref="refs/pull/1/merge"), "id": "42"},
        {**entry("v0-rust-z", 1, ref="refs/pull/1/merge"), "id": True},
    ]
    ids = [e.id for e in guard.normalize_entries(raw)]
    assert ids == ["7258560456", "42", None]


def test_apply_prune_deletes_every_candidate_by_id(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[list[str]] = []

    def record(args: list[str]) -> str:
        calls.append(args)
        return ""

    monkeypatch.setattr(guard, "run_gh", record)
    candidates = guard.normalize_entries(
        [{**entry("v0-rust-x", 1, ref="refs/pull/1/merge"), "id": 7258560456}]
    )
    assert guard.apply_prune(candidates, "zackees/soldr") == []
    assert calls == [["cache", "delete", "7258560456", "--repo", "zackees/soldr"]]


def test_prune_supersedes_older_zccache_unit_generations_on_main() -> None:
    # soldr#3102: every host-lane run on main saves a run-id keyed
    # generation of the Tier-2 store; only the newest is ever restored.
    base = "zccache-unit-v1-x86_64-unknown-linux-gnu-abc123"
    raw = [
        {**entry(f"{base}-111", 1000), "id": 1, "createdAt": "2026-09-04T01:00:00Z"},
        {**entry(f"{base}-222", 1000), "id": 2, "createdAt": "2026-09-05T01:00:00Z"},
        {**entry("stable-cook-v1-x86_64-unknown-linux-gnu-abc123", 10), "id": 3},
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw))
    # soldr#3396 retired stable-cook-*, so that entry is reclaimable too.
    assert [c.key for c in candidates] == [
        "stable-cook-v1-x86_64-unknown-linux-gnu-abc123",
        f"{base}-111",
    ]


def test_prune_supersedes_older_sha_keyed_bootstrap_drivers_on_main() -> None:
    # Exact-SHA keys with no restore-keys: an older commit's driver is only
    # restorable by re-running that commit, so each lineage keeps its newest.
    dev = "bootstrap-soldr-blessed-linux-gnu-dev-v1"
    release = "bootstrap-soldr-blessed-linux-gnu"
    raw = [
        {**entry(f"{dev}-aaa1", 23), "id": 1, "createdAt": "2026-09-13T13:45:00Z"},
        {**entry(f"{dev}-bbb2", 23), "id": 2, "createdAt": "2026-09-13T14:33:00Z"},
        {**entry(f"{dev}-ccc3", 23), "id": 3, "createdAt": "2026-09-14T01:04:00Z"},
        {**entry(f"{release}-ddd4", 19), "id": 4, "createdAt": "2026-09-12T10:00:00Z"},
        {**entry(f"{release}-eee5", 19), "id": 5, "createdAt": "2026-09-13T13:33:00Z"},
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw))
    assert sorted(c.key for c in candidates) == sorted(
        [f"{dev}-aaa1", f"{dev}-bbb2", f"{release}-ddd4"]
    )


def test_prune_keeps_only_newest_identical_solo_toolchain_version() -> None:
    # The action version is not part of the toolchain identity: all three
    # entries have the same Rust release and target set. Retain the latest
    # archive, not one ~180 MiB copy for every setup-soldr release.
    base = "solo-toolchain-v3-linux-x64-glibc-rustc1.98.1-cb6b9e404-tnone"
    raw = [
        {
            **entry(f"{base}-soldrv0.7.28", 100),
            "id": 1,
            "createdAt": "2026-09-20T01:00:00Z",
        },
        {
            **entry(f"{base}-soldrv0.9.22", 100),
            "id": 2,
            "createdAt": "2026-09-25T01:00:00Z",
        },
        {
            **entry(f"{base}-soldrv0.9.23", 100),
            "id": 3,
            "createdAt": "2026-09-26T01:00:00Z",
        },
    ]
    entries = guard.normalize_entries(raw)
    assert guard.strip_shared_key_hash(entries[0].key) == base
    assert [entry.key for entry in guard.prune_candidates(entries)] == [
        f"{base}-soldrv0.7.28",
        f"{base}-soldrv0.9.22",
    ]


def test_retired_dylint_nightly_entries_are_prune_candidates() -> None:
    # soldr#3216: the nightly toolchain cache was retired; any live entry is
    # reclaimable wherever it sits.
    raw = [
        {
            **entry(
                "dylint-nightly-v1-x86_64-unknown-linux-gnu-nightly-2026-05-28", 474
            ),
            "id": 1,
        },
        {
            **entry(
                "dylint-driver-v1-x86_64-unknown-linux-gnu-nightly-2026-05-28-6.0.3", 1
            ),
            "id": 2,
        },
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw))
    assert [c.key for c in candidates] == [
        "dylint-nightly-v1-x86_64-unknown-linux-gnu-nightly-2026-05-28"
    ]


def test_prune_supersedes_older_dylint_foundation_generations_on_main() -> None:
    # soldr#3216: the foundation key hashes the lint sources, so a lint change
    # saves a new ~413 MiB generation beside the old one.
    base = "dylint-foundation-v2-x86_64-unknown-linux-gnu"
    raw = [
        {
            **entry(f"{base}-c301e24a", 433),
            "id": 1,
            "createdAt": "2026-09-13T05:27:00Z",
        },
        {
            **entry(f"{base}-9f00aa11", 434),
            "id": 2,
            "createdAt": "2026-09-14T12:00:00Z",
        },
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw))
    assert [c.key for c in candidates] == [f"{base}-c301e24a"]


def test_the_dylint_nightly_cache_producer_stays_retired() -> None:
    # soldr#3216: the retirement is only real if nothing re-adds the producer
    # or quietly re-registers its prefix under a family.
    families = guard.load_manifest(MANIFEST)["budget"]["families"]
    assert "dylint-nightly-" in guard.RETIRED_PREFIXES
    assert not any(
        prefix.startswith("dylint-nightly-")
        for spec in families.values()
        for prefix in spec["key_prefixes"]
    )
    workflow = (REPO_ROOT / ".github" / "workflows" / "_build-and-test.yml").read_text(
        encoding="utf-8"
    )
    assert "key: dylint-nightly-" not in workflow
    assert "dylint_nightly_cache" not in workflow


def test_a_lone_bootstrap_driver_per_lineage_is_kept() -> None:
    raw = [
        {**entry("bootstrap-soldr-blessed-linux-gnu-dev-v1-abc", 23), "id": 1},
        {**entry("bootstrap-soldr-blessed-linux-gnu-def", 19), "id": 2},
    ]
    assert guard.prune_candidates(guard.normalize_entries(raw)) == []


def test_cook_lock_prune_preserves_unique_shapes_and_newest_lock() -> None:
    def cook(kind: str, shape: str, lock: str, when: str, suffix: str = "") -> dict:
        return {
            **entry(
                f"cook-{kind}-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l{lock}-soldr0.9.21{suffix}",
                100,
            ),
            "createdAt": when,
        }

    old = "4503d1780e10b133"
    new = "9506e5de4a14312c"
    raw = [
        cook("base", "bf1bfb42", old, "2026-09-22T01:00:00Z"),
        cook(
            "delta",
            "bf1bfb42",
            old,
            "2026-09-22T02:00:00Z",
            "-s4d428effa373-g1415998a4019694e",
        ),
        cook("base", "bf1bfb42", new, "2026-09-23T01:00:00Z"),
        cook("base", "aaa264c8", old, "2026-09-22T01:00:00Z"),
        cook("base", "aaa264c8", new, "2026-09-23T01:00:00Z"),
        cook("base", "c0f411d4", old, "2026-09-22T01:00:00Z"),  # unique shape
        cook("base", "baf623ad", new, "2026-09-23T01:00:00Z"),
        entry("unknown-cache-prefix", 100),
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw), new)
    assert {e.key for e in candidates} == {raw[i]["key"] for i in (0, 1, 3)}


def test_3347_active_generations_need_lineage_and_producer_shrink() -> None:
    # Approximate the 20:26 listing in MiB; five current shapes, two old
    # shapes, PR copies, two unit runs, two stable-cook hashes, and residual.
    mib = 1024**2
    rows: list[dict] = []
    shapes = ("bf1bfb42", "c0f411d4", "8a8107f7", "baf623ad", "aaa264c8")
    for shape in shapes:
        rows.append(
            {
                **entry(
                    f"cook-base-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l9506e5de4a14312c-soldr0.9.21",
                    430 * mib,
                ),
                "createdAt": "2026-09-23T14:00:00Z",
            }
        )
    for shape in shapes[:2]:
        rows.append(
            {
                **entry(
                    f"cook-base-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l4503d1780e10b133-soldr0.9.21",
                    390 * mib,
                ),
                "createdAt": "2026-09-22T14:00:00Z",
            }
        )
    for shape in shapes[:3]:
        rows.append(
            entry(
                f"cook-base-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l4503d1780e10b133-soldr0.9.21",
                400 * mib,
                "refs/pull/3346/merge",
            )
        )
    rows += [
        {
            **entry("zccache-unit-v1-linux-abc-111", 1300 * mib),
            "createdAt": "2026-09-22T01:00:00Z",
        },
        {
            **entry("zccache-unit-v1-linux-abc-222", 1300 * mib),
            "createdAt": "2026-09-23T01:00:00Z",
        },
        {
            **entry("stable-cook-v2-x86_64-unknown-linux-gnu-" + "a" * 64, 300 * mib),
            "createdAt": "2026-09-22T01:00:00Z",
        },
        {
            **entry("stable-cook-v2-x86_64-unknown-linux-gnu-" + "b" * 64, 650 * mib),
            "createdAt": "2026-09-23T01:00:00Z",
        },
        # A leftover entry of the retired rust-cache-residual family (its last
        # producer, the bootstrap driver build, went with CACHE-025's
        # exceptions on 2026-10-02): a retired prefix, so it is reclaimable.
        entry("v0-rust-bootstrap-soldr-linux-gnu-dev-abc", 700 * mib),
    ]
    entries = guard.normalize_entries(rows)
    manifest = guard.load_manifest(MANIFEST)
    assert guard.budget_problems(MANIFEST, manifest, entries)
    # The existing policy could only reclaim PR copies and the older unit run.
    old_candidates = [
        e for e in entries if e.ref != "refs/heads/main" or e.key.endswith("-111")
    ]
    assert guard.budget_problems(
        MANIFEST, manifest, [e for e in entries if e not in old_candidates]
    )
    candidates = guard.prune_candidates(entries, "9506e5de4a14312c")
    effective = [e for e in entries if e not in candidates]
    # PR bases, old cook locks, old unit run, both retired stable-cook entries,
    # and the retired rust-cache-residual entry.
    assert len(candidates) == 9
    assert any(
        e.key.startswith("v0-rust-bootstrap-soldr-linux-gnu-dev-") for e in candidates
    )
    assert guard.budget_problems(MANIFEST, manifest, effective) == []


def test_cook_rollback_uses_main_lock_not_creation_time() -> None:
    base = "cook-base-v2-linux-x64-glibc-rustc1.98.1-fbf1bfb42-l{}-soldr0.9.21"
    old = {
        **entry(base.format("4503d1780e10b133"), 100),
        "createdAt": "2026-09-24T01:00:00Z",
    }
    current = {
        **entry(base.format("9506e5de4a14312c"), 100),
        "createdAt": "2026-09-23T01:00:00Z",
    }
    entries = guard.normalize_entries([old, current])
    assert [e.key for e in guard.prune_candidates(entries, "9506e5de4a14312c")] == [
        old["key"]
    ]
    assert [e.key for e in guard.prune_candidates(entries, "4503d1780e10b133")] == [
        current["key"]
    ]
    assert guard.prune_candidates(entries) == []


def test_main_lock_hash_uses_remote_raw_bytes(monkeypatch: pytest.MonkeyPatch) -> None:
    lock_bytes = b"# exact source bytes\nversion = 4\n"
    response = json.dumps(
        {"encoding": "base64", "content": base64.b64encode(lock_bytes).decode()}
    )
    calls: list[list[str]] = []

    def fake_gh(args: list[str]) -> str:
        calls.append(args)
        return response

    monkeypatch.setattr(guard, "run_gh", fake_gh)
    assert (
        guard.fetch_main_lock_hash("zackees/soldr")
        == hashlib.sha256(lock_bytes).hexdigest()[:16]
    )
    assert calls == [["api", "repos/zackees/soldr/contents/Cargo.lock?ref=main"]]


def test_retired_stable_cook_entries_are_prune_candidates() -> None:
    # soldr#3396: nothing writes stable-cook-* any more, so every generation,
    # current or not, is reclaimable and none is an unregistered producer.
    raw = [
        entry("stable-cook-v2-x86_64-unknown-linux-gnu-" + "a" * 64, 100),
        entry("stable-cook-v2-aarch64-unknown-linux-gnu-" + "c" * 64, 100),
    ]
    entries = guard.normalize_entries(raw)
    assert [e.key for e in guard.prune_candidates(entries)] == [r["key"] for r in raw]


# --------------------------------------------------------------------------
# soldr#3347: main cook lock-generation lineage, RED -> GREEN
# --------------------------------------------------------------------------

LINEAGE_FIXTURE = (
    REPO_ROOT
    / "tests"
    / "fixtures"
    / "actions-cache"
    / "listing-3347-cook-lineage.json"
)
CURRENT_LOCK = "e8c3129b32c03b91"
PRIOR_LOCK = "9506e5de4a14312c"


def lineage_entries() -> list:
    raw = guard.load_from_json(LINEAGE_FIXTURE).entries
    return guard.normalize_entries(raw)


def legacy_cook_candidates(on_main: list, lock: str) -> list:
    """The pre-#3347 rule: a shape matched only at the SAME soldr version."""
    bases = set()
    for e in on_main:
        m = guard.COOK_KEY.fullmatch(e.key)
        if m and m.group(1) == "base" and m.group(3) == lock:
            bases.add((m.group(2), m.group(4)))
    return [
        e
        for e in on_main
        if (m := guard.COOK_KEY.fullmatch(e.key))
        and (m.group(2), m.group(4)) in bases
        and m.group(3) != lock
    ]


def without(entries: list, dropped: list) -> list:
    ids = {id(e) for e in dropped}
    return [e for e in entries if id(e) not in ids]


def test_3347_fixture_is_red_raw() -> None:
    entries = lineage_entries()
    manifest = guard.load_manifest(MANIFEST)
    problems = guard.budget_problems(MANIFEST, manifest, entries)
    assert any("'cook-layer'" in p for p in problems)
    assert any("'zccache-unit'" in p for p in problems)


def test_3347_legacy_policy_still_fails_after_its_reclaim() -> None:
    entries = lineage_entries()
    manifest = guard.load_manifest(MANIFEST)
    everything = guard.prune_candidates(entries, CURRENT_LOCK)
    new_cook = guard.cook_lineage_candidates(
        [e for e in entries if e.ref == "refs/heads/main"], CURRENT_LOCK
    )
    legacy = without(everything, new_cook) + legacy_cook_candidates(
        [e for e in entries if e.ref == "refs/heads/main"], CURRENT_LOCK
    )
    problems = guard.budget_problems(MANIFEST, manifest, without(entries, legacy))
    assert any("'cook-layer'" in p for p in problems)
    assert guard.effective_verdict(problems).startswith(
        "effective after safe prune: STILL OVER BUDGET"
    )


def test_3347_lineage_policy_fits_every_family_and_total() -> None:
    entries = lineage_entries()
    manifest = guard.load_manifest(MANIFEST)
    candidates = guard.prune_candidates(entries, CURRENT_LOCK)
    effective = without(entries, candidates)
    assert guard.budget_problems(MANIFEST, manifest, effective) == []
    assert guard.effective_verdict([]).endswith("every family and the total fit")
    kept_cook = {e.key for e in effective if e.key.startswith("cook-")}
    # Keep one current-lock base for each shape; deltas are disabled by every
    # setup-soldr call and are safely reclaimed as unreachable state.
    assert len(kept_cook) == 5
    assert all(k.startswith("cook-base-") for k in kept_cook)
    assert all(f"-l{CURRENT_LOCK}-" in k for k in kept_cook)
    assert len({id(e) for e in candidates}) == len(candidates)  # no double count


def test_3347_policy_is_not_green_when_a_family_truly_does_not_fit() -> None:
    # A newest zccache-unit generation above its allocation has no safe prune
    # candidate (the newest main generation is what required jobs restore), so
    # it must stay red. (This used the rust-cache-residual family until
    # CACHE-025 retired it on 2026-10-02.)
    entries = [
        (
            guard.CacheEntry(e.key, e.ref, 3_000_000_000, e.id, e.created_at)
            if e.key.startswith("zccache-unit-")
            else e
        )
        for e in lineage_entries()
    ]
    manifest = guard.load_manifest(MANIFEST)
    effective = without(entries, guard.prune_candidates(entries, CURRENT_LOCK))
    problems = guard.budget_problems(MANIFEST, manifest, effective)
    assert any("'zccache-unit'" in p for p in problems)
    assert "STILL OVER BUDGET" in guard.effective_verdict(problems)


def _cook(kind: str, shape: str, lock: str, soldr: str, suffix: str = "") -> dict:
    return entry(
        f"cook-{kind}-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l{lock}-soldr{soldr}{suffix}",
        100,
    )


def test_3347_never_retires_a_unique_active_shape() -> None:
    raw = [
        _cook("base", "aaa264c8", CURRENT_LOCK, "0.9.22"),
        _cook("base", "c0f411d4", PRIOR_LOCK, "0.9.21"),  # no current base
        _cook("delta", "c0f411d4", PRIOR_LOCK, "0.9.21", "-s1-g2"),
    ]
    assert [
        entry.key
        for entry in guard.prune_candidates(guard.normalize_entries(raw), CURRENT_LOCK)
    ] == [raw[2]["key"]]


def test_3347_never_retires_an_unknown_prefix() -> None:
    raw = [
        _cook("base", "aaa264c8", CURRENT_LOCK, "0.9.22"),
        entry(
            f"cook-base-v3-linux-x64-glibc-rustc1.98.1-faaa264c8-l{PRIOR_LOCK}-soldr0.9.21",
            100,
        ),
        entry("mystery-cache-faaa264c8-l" + PRIOR_LOCK, 100),
    ]
    assert guard.prune_candidates(guard.normalize_entries(raw), CURRENT_LOCK) == []


def test_3347_keeps_every_current_lock_base_generation() -> None:
    raw = [
        _cook("base", "aaa264c8", CURRENT_LOCK, "0.9.22"),
        _cook("delta", "aaa264c8", CURRENT_LOCK, "0.9.22", "-s1-g2"),
        # Same lock, older soldr: still required for a soldr rollback.
        _cook("base", "aaa264c8", CURRENT_LOCK, "0.9.21"),
        _cook("base", "aaa264c8", PRIOR_LOCK, "0.9.21"),
    ]
    entries = guard.normalize_entries(raw)
    assert [e.key for e in guard.prune_candidates(entries, CURRENT_LOCK)] == [
        raw[1]["key"],
        raw[3]["key"],
    ]
    # Unknown current lock: the required generation cannot be identified.
    assert [e.key for e in guard.prune_candidates(entries, None)] == [raw[1]["key"]]


# --------------------------------------------------------------------------
# soldr#3398: dogfood zccache generations per lockfile prefix, RED -> GREEN
# --------------------------------------------------------------------------

DOGFOOD_FIXTURE = (
    REPO_ROOT
    / "tests"
    / "fixtures"
    / "actions-cache"
    / "listing-3398-dogfood-generations.json"
)
DOGFOOD = "setup-soldr-dogfood-zccache-v1-Linux-X64-"
DOGFOOD_LOCK = "55449f335975c067e53a168bab22ae9b63036849458d34b93c50638df7addd13"


def dogfood_entries() -> list:
    raw = guard.load_from_json(DOGFOOD_FIXTURE).entries
    return guard.normalize_entries(raw)


def test_3398_live_listing_is_red_raw() -> None:
    manifest = guard.load_manifest(MANIFEST)
    problems = guard.budget_problems(MANIFEST, manifest, dogfood_entries())
    assert any("'setup-soldr-action-stores'" in p for p in problems)


def test_3398_prune_fits_setup_soldr_action_stores() -> None:
    entries = dogfood_entries()
    manifest = guard.load_manifest(MANIFEST)
    effective = without(entries, guard.prune_candidates(entries))
    problems = guard.budget_problems(MANIFEST, manifest, effective)
    assert not any("'setup-soldr-action-stores'" in p for p in problems)
    kept = [e for e in effective if e.key.startswith(DOGFOOD)]
    by_lock: dict[str, list] = {}
    for e in kept:
        by_lock.setdefault(guard.strip_shared_key_hash(e.key), []).append(e)
    assert all(len(group) == 1 for group in by_lock.values())
    # The unique older-lockfile generation stays live.
    assert len(by_lock) == 2


def test_3398_prune_never_deletes_the_newest_or_a_unique_generation() -> None:
    main = "refs/heads/main"
    newest = guard.CacheEntry(
        f"{DOGFOOD}{DOGFOOD_LOCK}-bbb", main, 1, "1", "2026-09-26T06:00:00Z"
    )
    older = guard.CacheEntry(
        f"{DOGFOOD}{DOGFOOD_LOCK}-aaa", main, 1, "2", "2026-09-26T04:00:00Z"
    )
    unique = guard.CacheEntry(
        f"{DOGFOOD}otherlock-ccc", main, 1, "3", "2026-09-25T00:00:00Z"
    )
    unknown = guard.CacheEntry(
        "setup-soldr-dogfood-other-v9-x-1", main, 1, "4", "2026-09-20T00:00:00Z"
    )
    unknown2 = guard.CacheEntry(
        "setup-soldr-dogfood-other-v9-x-2", main, 1, "5", "2026-09-21T00:00:00Z"
    )
    candidates = guard.prune_candidates([older, newest, unique, unknown, unknown2])
    assert [c.key for c in candidates] == [older.key]


# --------------------------------------------------------------------------
# soldr#3347: retire the oversized perf build cache only after its replacement
# --------------------------------------------------------------------------

PERF_TARGET_CACHE = "v0-rust-perf-build-soldr-linux-Linux-x64-079eeefc-1a449117"
PERF_BINARY_CACHE = (
    "soldr-bin-linux-6bb7426dc0c3e4b2d8ea592c9c5168879a1f293457c7a30f8ee65d399e318149"
)


def test_perf_target_cache_waits_for_newer_same_platform_binary_replacement() -> None:
    old = guard.CacheEntry(
        PERF_TARGET_CACHE,
        "refs/heads/main",
        508_702_303,
        "1",
        "2026-09-26T09:31:38Z",
    )
    old_prior = guard.CacheEntry(
        PERF_TARGET_CACHE.rsplit("-", 1)[0] + "-1a449116",
        "refs/heads/main",
        500_000_000,
        "1a",
        "2026-09-26T08:00:00Z",
    )

    # This ~509 MB rust-cache entry is still restorable through the active
    # perf-build shared-key; a newer, unrelated platform cache is not proof
    # that Linux's binary producer has replaced it.
    wrong_platform = guard.CacheEntry(
        PERF_BINARY_CACHE.replace("linux-", "win-"),
        "refs/heads/main",
        1,
        "2",
        "2026-09-27T00:00:00Z",
    )
    assert old.key not in {
        entry.key for entry in guard.prune_candidates([old, old_prior, wrong_platform])
    }

    pull_request = guard.CacheEntry(
        PERF_BINARY_CACHE,
        "refs/pull/5/merge",
        1,
        "2b",
        "2026-09-27T00:00:00Z",
    )
    assert old.key not in {
        entry.key for entry in guard.prune_candidates([old, old_prior, pull_request])
    }

    replacement = guard.CacheEntry(
        PERF_BINARY_CACHE,
        "refs/heads/main",
        34_085_000,
        "3",
        "2026-09-27T00:56:54Z",
    )
    assert [
        entry.key for entry in guard.prune_candidates([old, old_prior, replacement])
    ] == [
        PERF_TARGET_CACHE,
        old_prior.key,
    ]

    # A same-platform binary that predates the legacy target cache is not proof
    # that the replacement workflow ran before the old archive is retired.
    older_replacement = guard.CacheEntry(
        PERF_BINARY_CACHE,
        "refs/heads/main",
        34_085_000,
        "4",
        "2026-09-26T09:00:00Z",
    )
    assert old.key not in {
        entry.key for entry in guard.prune_candidates([old, older_replacement])
    }

    # Exact-source binary keys must be well-formed; a prefix-only or malformed
    # key does not unlock deletion of the still-restorable target archive.
    malformed = guard.CacheEntry(
        "soldr-bin-linux-not-a-source-hash",
        "refs/heads/main",
        34_085_000,
        "5",
        "2026-09-27T01:00:00Z",
    )
    assert old.key not in {
        entry.key for entry in guard.prune_candidates([old, malformed])
    }


def test_retired_perf_registry_generation_is_prunable() -> None:
    registry = guard.CacheEntry(
        "v0-rust-perf-registry-soldr-linux-Linux-x64-079eeefc-1a449117",
        "refs/heads/main",
        119_579_384,
        "6",
    )
    assert guard.prune_candidates([registry]) == [registry]


# --------------------------------------------------------------------------
# zackees/ci.yml#6 "Cache (live)" / GEN-009: who pays, and a janitor that
# converges instead of leaving a permanently red cron.
# --------------------------------------------------------------------------

NOW = __import__("datetime").datetime(
    2026, 9, 29, 12, 0, tzinfo=__import__("datetime").timezone.utc
)
OLD = "2026-09-20T00:00:00Z"
FRESH = "2026-09-29T11:55:00Z"  # 5 minutes before NOW: inside the grace window


def aged(
    key: str,
    size: int,
    *,
    ref: str = "refs/heads/main",
    created: str = OLD,
    accessed: str | None = None,
) -> dict:
    row = {**entry(key, size, ref=ref), "createdAt": created}
    if accessed is not None:
        row["lastAccessedAt"] = accessed
    return row


def budget_with(**families: dict) -> dict:
    total = sum(spec["max_bytes"] for spec in families.values())
    return {
        "total_max_bytes": total,
        "fail_total_bytes": total * 2,
        "families": families,
    }


@dataclass(frozen=True)
class OverBudgetFiles:
    manifest: Path
    listing: Path


def over_budget_listing(
    tmp_path: Path, ref: str = "refs/heads/main"
) -> OverBudgetFiles:
    manifest = write_manifest(tmp_path, budget_with(fam=family("exp-", 100)))
    listing = write_listing(tmp_path, [entry("exp-a", 80), entry("exp-b", 80, ref=ref)])
    return OverBudgetFiles(manifest, listing)


def run_mode(manifest: Path, listing: Path, *extra: str) -> int:
    return guard.main(
        ["--manifest", str(manifest), "--from-json", str(listing), *extra]
    )


def test_pr_run_with_preexisting_overage_warns_and_passes(tmp_path, capsys) -> None:
    files = over_budget_listing(tmp_path)
    manifest, listing = files.manifest, files.listing
    code = run_mode(
        manifest, listing, "--event-name", "pull_request", "--ref", "refs/pull/7/merge"
    )
    out = capsys.readouterr().out
    assert code == 0, out
    assert "::warning title=Actions cache budget::family 'fam'" in out


def test_pr_run_writes_the_warning_to_the_job_summary(tmp_path, monkeypatch) -> None:
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    files = over_budget_listing(tmp_path)
    manifest, listing = files.manifest, files.listing
    assert (
        run_mode(
            manifest,
            listing,
            "--event-name",
            "pull_request",
            "--ref",
            "refs/pull/7/merge",
        )
        == 0
    )
    assert "over its" in summary.read_text(encoding="utf-8")


def test_pr_that_saved_over_budget_from_its_own_ref_fails(tmp_path, capsys) -> None:
    files = over_budget_listing(tmp_path, ref="refs/pull/7/merge")
    manifest, listing = files.manifest, files.listing
    code = run_mode(
        manifest, listing, "--event-name", "pull_request", "--ref", "refs/pull/7/merge"
    )
    out = capsys.readouterr().out
    assert code == 1, out
    assert "saved by this PR (refs/pull/7/merge)" in out


def test_pr_saving_an_unregistered_key_from_its_own_ref_fails(tmp_path) -> None:
    manifest = write_manifest(tmp_path, budget_with(fam=family("exp-", 100)))
    listing = write_listing(tmp_path, [entry("rogue-x", 1, ref="refs/pull/7/merge")])
    assert (
        run_mode(
            manifest,
            listing,
            "--event-name",
            "pull_request",
            "--ref",
            "refs/pull/7/merge",
        )
        == 1
    )


@pytest.mark.parametrize(
    ("event", "ref"),
    [
        ("push", "refs/heads/main"),
        ("schedule", "refs/heads/main"),
        ("workflow_dispatch", "refs/heads/main"),
        ("", ""),
    ],
)
def test_main_schedule_dispatch_and_local_runs_fail_when_over_budget(
    tmp_path, event, ref
) -> None:
    files = over_budget_listing(tmp_path)
    manifest, listing = files.manifest, files.listing
    assert run_mode(manifest, listing, "--event-name", event, "--ref", ref) == 1


@pytest.mark.parametrize("event", ["push", "workflow_dispatch"])
def test_a_push_or_dispatch_on_another_branch_warns(tmp_path, capsys, event) -> None:
    # workflow_dispatch: a PR's exact-SHA "CI full" run on its feature branch
    # (zackees/ci.yml#166, soldr#3510) must not fail on main's overage.
    files = over_budget_listing(tmp_path)
    manifest, listing = files.manifest, files.listing
    assert (
        run_mode(manifest, listing, "--event-name", event, "--ref", "refs/heads/topic")
        == 0
    )
    assert "::warning" in capsys.readouterr().out


def test_a_static_forecast_breach_fails_even_in_pr_mode(tmp_path) -> None:
    budget = budget_with(fam=family("exp-", 100))
    budget["families"]["fam"]["evict"] = "sometimes"
    manifest = write_manifest(tmp_path, budget)
    listing = write_listing(tmp_path, [entry("exp-a", 1)])
    assert (
        run_mode(
            manifest,
            listing,
            "--event-name",
            "pull_request",
            "--ref",
            "refs/pull/7/merge",
        )
        == 1
    )


def test_a_non_evictable_steady_state_that_cannot_fit_is_a_static_finding(
    tmp_path,
) -> None:
    spec = {**family("exp-", 100), "steady_state_bytes": 150}
    problems = guard.forecast_problems(
        tmp_path / "m.json", {"budget": budget_with(fam=spec)}
    )
    assert any("cannot fit" in p for p in problems)
    evictable = {**spec, "evict": "lru"}
    assert (
        guard.forecast_problems(
            tmp_path / "m.json", {"budget": budget_with(fam=evictable)}
        )
        == []
    )


def test_the_real_manifest_has_no_forecast_problems() -> None:
    manifest = guard.load_manifest(MANIFEST)
    assert guard.forecast_problems(MANIFEST, manifest) == []


def _plan(rows: list[dict], families: dict, pr_state=None):
    manifest = {"budget": budget_with(**families)}
    return guard.plan_sweep(
        guard.normalize_entries(rows), manifest, None, NOW, pr_state
    )


def test_lru_janitor_plan_reaches_the_family_budget() -> None:
    lru = {**family("exp-", 100), "evict": "lru"}
    rows = [
        aged("exp-a", 60, accessed="2026-09-21T00:00:00Z"),
        aged("exp-b", 60, accessed="2026-09-28T00:00:00Z"),
        aged("exp-c", 60, accessed="2026-09-25T00:00:00Z"),
    ]
    delete = _plan(rows, {"fam": lru}).delete
    # Least recently used first, and only as much as the overage needs.
    assert [e.key for e in delete] == ["exp-a", "exp-c"]
    remaining = sum(r["sizeInBytes"] for r in rows) - sum(e.size_bytes for e in delete)
    assert remaining <= lru["max_bytes"]


def test_a_non_evictable_family_is_never_touched_for_budget() -> None:
    rows = [aged("keep-a", 90), aged("keep-b", 90)]
    delete = _plan(rows, {"fam": family("keep-", 100)}).delete
    assert delete == []


def test_newest_per_lineage_never_evicts_the_newest_download() -> None:
    pinned = {**family("dl-", 100), "evict": "newest-per-lineage"}
    rows = [
        aged("dl-tool-v1.0", 60, created="2026-09-01T00:00:00Z"),
        aged("dl-tool-v1.1", 60, created="2026-09-10T00:00:00Z"),
        aged("dl-tool-v1.2", 60, created="2026-09-20T00:00:00Z"),
    ]
    delete = _plan(rows, {"fam": pinned}).delete
    assert "dl-tool-v1.2" not in [e.key for e in delete]
    assert [e.key for e in delete] == ["dl-tool-v1.0", "dl-tool-v1.1"]


def test_newest_per_lineage_leaves_a_lone_lineage_even_over_budget() -> None:
    pinned = {**family("dl-", 100), "evict": "newest-per-lineage"}
    delete = _plan([aged("dl-only-v1", 150)], {"fam": pinned}).delete
    assert delete == []


def test_newest_keeps_only_the_familys_newest_hash_keyed_generation() -> None:
    # zccache-unit keys end in a lockfile hash whose letters defeat
    # newest-per-lineage (every generation is its own lineage), so the Tier-2
    # store declares `newest`: one protected entry for the whole family.
    store = {**family("zu-", 100), "evict": "newest"}
    rows = [
        aged("zu-13acf6", 60, created="2026-09-20T00:00:00Z"),
        aged("zu-1dec4d", 60, created="2026-09-10T00:00:00Z"),
        aged("zu-ff1637", 60, created="2026-09-01T00:00:00Z"),
    ]
    delete = _plan(rows, {"fam": store}).delete
    assert "zu-13acf6" not in [e.key for e in delete]
    assert sorted(e.key for e in delete) == ["zu-1dec4d", "zu-ff1637"]


def test_newest_leaves_a_lone_generation_even_over_budget() -> None:
    store = {**family("zu-", 100), "evict": "newest"}
    delete = _plan([aged("zu-only", 150)], {"fam": store}).delete
    assert delete == []


STATES = {1: "open", 2: "closed", 3: "closed"}  # 2 merged, 3 closed unmerged


def test_an_open_prs_entries_are_kept() -> None:
    rows = [aged("keep-a", 1, ref="refs/pull/1/merge", created=FRESH)]
    delete = _plan(rows, {"fam": family("keep-", 1000)}, STATES.get).delete
    assert delete == []


def test_merged_and_closed_unmerged_prs_entries_are_deleted() -> None:
    rows = [
        aged("keep-open", 1, ref="refs/pull/1/merge"),
        aged("keep-merged", 1, ref="refs/pull/2/merge"),
        # Closed PRs have no running jobs, so the grace window does not apply.
        aged("keep-closed", 1, ref="refs/pull/3/merge", created=FRESH),
        aged("keep-main", 1),
    ]
    looked_up: list[int] = []

    def state(number: int) -> str | None:
        looked_up.append(number)
        return STATES.get(number)

    delete = _plan(rows, {"fam": family("keep-", 1000)}, state).delete
    assert sorted(e.key for e in delete) == ["keep-closed", "keep-merged"]
    assert looked_up == [1, 2, 3], "one lookup per PR number"


def test_a_failed_pr_state_lookup_keeps_the_entries(capsys) -> None:
    rows = [aged("keep-a", 1, ref="refs/pull/9/merge")]
    delete = _plan(rows, {"fam": family("keep-", 1000)}, lambda _n: None).delete
    assert delete == []
    assert "PR #9: state unknown; kept 1" in capsys.readouterr().out


def test_fetch_pr_state_logs_and_returns_none_on_failure(monkeypatch, capsys) -> None:
    def boom(_args: list[str]) -> str:
        raise guard.subprocess.CalledProcessError(1, ["gh"], "HTTP 502")

    monkeypatch.setattr(guard, "run_gh", boom)
    assert guard.fetch_pr_state("zackees/soldr", 5) is None
    assert "state lookup failed" in capsys.readouterr().out


def test_the_janitor_matches_pr_caches_by_key_tag_and_by_ref() -> None:
    rows = [
        aged("keep-x-pr-2-linux", 1, ref="refs/heads/main"),
        aged("keep-y", 1, ref="refs/pull/2/merge"),
        aged("keep-z-pr-1", 1, ref="refs/heads/main"),
    ]
    delete = _plan(rows, {"fam": family("keep-", 1000)}, STATES.get).delete
    assert sorted(e.key for e in delete) == ["keep-x-pr-2-linux", "keep-y"]


def test_an_untagged_key_on_a_branch_ref_is_not_a_pr_cache() -> None:
    untagged = guard.CacheEntry("keep-sprint-2", "refs/heads/feature-pr-2", 1)
    assert guard.pr_number_of(untagged) is None


def test_pr_12_does_not_match_pr_123() -> None:
    def number(key: str) -> int | None:
        return guard.pr_number_of(guard.CacheEntry(key, "refs/heads/main", 1))

    assert number("cook-pr-123-linux") == 123
    assert number("cook-pr-12-linux") == 12
    assert number("cook-pr-12") == 12
    assert number("pr-12_x") == 12
    assert number("cook-xpr-12") is None
    assert number("cook-pr-12a") is None


def test_the_plan_skips_entries_younger_than_the_window() -> None:
    lru = {**family("exp-", 10), "evict": "lru"}
    rows = [aged("exp-new", 60, created=FRESH), aged("exp-old", 60)]
    delete = _plan(rows, {"fam": lru}).delete
    assert [e.key for e in delete] == ["exp-old"]
    assert guard.SWEEP_GRACE_SECONDS == 600


def test_an_entry_with_no_creation_time_is_never_swept() -> None:
    rows = [entry("keep-a", 1, ref="refs/heads/topic")]
    plan = _plan(rows, {"fam": family("keep-", 1000)})
    delete, deferred = plan.delete, plan.deferred
    assert delete == [] and len(deferred) == 1


def test_delete_ref_removes_only_that_closed_prs_entries(monkeypatch, capsys) -> None:
    listing = [
        {**aged("keep-a", 1, ref="refs/pull/7/merge"), "id": 1},
        {**aged("keep-b", 1, ref="refs/pull/70/merge"), "id": 2},
        {**aged("keep-c", 1), "id": 3},
        {**aged("keep-d-pr-7", 1), "id": 4},
        {**aged("keep-e-pr-70", 1), "id": 5},
    ]
    calls: list[list[str]] = []

    def fake_gh(args: list[str]) -> str:
        calls.append(args)
        return json.dumps(listing) if args[:2] == ["cache", "list"] else ""

    monkeypatch.setattr(guard, "run_gh", fake_gh)
    assert guard.main(["--delete-ref", "refs/pull/7/merge"]) == 0
    deletes = [c for c in calls if c[:2] == ["cache", "delete"]]
    assert deletes == [
        ["cache", "delete", "1", "--repo", "zackees/soldr"],
        ["cache", "delete", "4", "--repo", "zackees/soldr"],
    ]


def test_delete_ref_refusal_is_logged_and_exits_zero(monkeypatch, capsys) -> None:
    listing = [{**aged("keep-a", 1, ref="refs/pull/7/merge"), "id": 1}]

    def fake_gh(args: list[str]) -> str:
        if args[:2] == ["cache", "list"]:
            return json.dumps(listing)
        raise guard.subprocess.CalledProcessError(1, ["gh"], "HTTP 403")

    monkeypatch.setattr(guard, "run_gh", fake_gh)
    assert guard.main(["--delete-ref", "refs/pull/7/merge"]) == 0
    assert "next janitor sweep" in capsys.readouterr().out


def test_delete_ref_rejects_anything_but_a_pr_ref() -> None:
    assert guard.main(["--delete-ref", "refs/heads/main"]) == 1


def test_real_manifest_declares_evict_only_for_the_safe_families() -> None:
    families = guard.load_manifest(MANIFEST)["budget"]["families"]
    evictable = {
        name: spec["evict"] for name, spec in families.items() if "evict" in spec
    }
    assert evictable == {
        # zackees/ci.yml#198: tiny attestation evidence; an evicted entry only
        # makes hydration pick an older (or no) attested ancestor.
        "attestation-evidence": "lru",
        "experiment-lanes": "lru",
        "pinned-immutable-download": "newest-per-lineage",
        # soldr#3545: superseded lock/toolchain generations are safe-pruned
        # first, so this only bites on an overage safe-prune cannot explain.
        "setup-soldr-action-stores": "newest",
        # soldr#3458: only the newest main store generation is ever restored.
        "zccache-unit": "newest",
    }


@pytest.mark.parametrize("kind", ["buildcache", "cargoregistry"])
def test_action_store_retires_superseded_lock_and_toolchain(kind):
    current, old, toolchain = "a" * 16, "b" * 16, "c" * 16

    def key(lock, shape="linux-x64"):
        if kind == "buildcache":
            return f"setup-soldr-buildcache-v2-{shape}-{toolchain}-{lock}"
        return f"setup-soldr-cargoregistry-v1-{shape}-{lock}-{toolchain}"

    entries = guard.normalize_entries(
        [
            entry(key(old), 400),
            entry(key(current), 500),
            entry(key(old, "windows-x64"), 600),
            entry("setup-soldr-buildcache-v99-unknown", 700),
            entry(key(old).replace(toolchain, "d" * 16), 800),
            entry(key(old) + "-job-namespace", 900),
        ]
    )
    # Retired: the replaced lock generation (entries[0]) and, soldr#3545, the
    # superseded toolchain generation on the same os/arch (entries[4]) -- it
    # differs from the live store only by the toolchain hash, so no current
    # job can ever restore it.
    assert guard.prune_candidates(entries, current) == [entries[0], entries[4]]
    # Protected: the current-lock, current-toolchain store (entries[1]), the
    # windows platform with no current-lock store, unknown formats, and the
    # namespaced key.
    assert entries[1] not in guard.prune_candidates(entries, current)
    assert guard.prune_candidates(entries, None) == []
    assert guard.prune_candidates(entries[:1], current) == []


@pytest.mark.parametrize("kind", ["buildcache", "cargoregistry"])
def test_3545_sweep_reclaims_the_older_toolchain_under_one_lock(kind):
    """soldr#3545: two toolchain generations sharing one lock hash.

    The reported shape: both entries carried the live main lock and differed
    only in toolchain hash (a toolchain bump that did not touch Cargo.lock),
    so the lock rule never fired and a shape match that included the
    toolchain hash never matched -- the stale store was unreachable by every
    eviction rule while the family sat permanently over its allocation.
    """
    lock, stale_toolchain, live_toolchain = "e" * 16, "c" * 16, "d" * 16

    def key(toolchain):
        if kind == "buildcache":
            return f"setup-soldr-buildcache-v2-linux-x64-{toolchain}-{lock}"
        return f"setup-soldr-cargoregistry-v1-linux-x64-{lock}-{toolchain}"

    rows = [
        aged(key(stale_toolchain), 600, created="2026-09-15T00:00:00Z"),
        aged(key(live_toolchain), 500, created="2026-09-25T00:00:00Z"),
    ]
    spec = {
        "key_prefixes": ["setup-soldr-buildcache-", "setup-soldr-cargoregistry-"],
        "max_bytes": 1_000,
        "entries": [],
        "rationale": "fixture",
    }
    plan = guard.plan_sweep(
        guard.normalize_entries(rows), {"budget": budget_with(fam=spec)}, lock, NOW
    )
    delete, deferred = plan.delete, plan.deferred
    assert [e.key for e in delete] == [key(stale_toolchain)]
    assert deferred == []
    # The current-toolchain generation is genuinely in use and stays live.
    assert key(live_toolchain) not in {e.key for e in delete}


# --------------------------------------------------------------------------
# soldr#3618: ci.toml is the one cache-family declaration
# --------------------------------------------------------------------------


def test_the_ownership_manifest_carries_no_byte_numbers() -> None:
    """Sizes live only in ci.toml; the JSON maps families onto groups."""
    raw = json.loads(MANIFEST.read_text(encoding="utf-8"))
    assert "budget" not in raw, "the budget is composed from ci.toml, not declared"
    for group_id, group in raw["family_groups"].items():
        assert not {"max_bytes", "key_prefixes", "total_max_bytes"} & set(group), (
            group_id
        )


def test_every_ci_toml_family_belongs_to_exactly_one_group() -> None:
    ci_toml = guard.cache_families.load_ci_toml()
    raw = json.loads(MANIFEST.read_text(encoding="utf-8"))
    members = [f for g in raw["family_groups"].values() for f in g["ci_toml_families"]]
    assert sorted(members) == sorted(ci_toml["cache"]["family"])


def test_a_group_allocation_is_the_sum_of_its_ci_toml_footprints() -> None:
    ci_toml = {
        "platforms": {
            "a": {"group": "linux"},
            "b": {"group": "linux"},
            "c": {"group": "win"},
        },
        "cache": {
            "budget": "1GB",
            "family": {
                "one": {"prefix": "one-", "max": "10MB", "per": "platform"},
                "two": {"via": "setup-uv", "max": "1MB", "per": "os"},
                "three": {
                    "prefix": "three-",
                    "max": "9MB",
                    "shapes": {"x": {"max": "4MB"}, "y": {"max": "5MB"}},
                },
            },
        },
    }
    groups = {
        "g": {"ci_toml_families": ["one", "two"], "evict": "lru"},
        "h": {"ci_toml_families": ["three"]},
    }
    budget = guard.cache_families.compose_budget(ci_toml, groups)
    mib = 1024**2
    assert budget["families"]["g"]["max_bytes"] == 30 * mib + 2 * mib
    assert budget["families"]["g"]["key_prefixes"] == [
        "one-",
        "setup-uv-2-",
        "setup-uv-1-",
    ]
    assert budget["families"]["g"]["evict"] == "lru"
    assert budget["families"]["h"]["max_bytes"] == 9 * mib
    assert budget["total_max_bytes"] == 41 * mib
    assert budget["fail_total_bytes"] == 1024**3


@pytest.mark.parametrize(
    "groups, message",
    [
        ({"g": {"ci_toml_families": ["one"]}}, "belong to no"),
        (
            {
                "g": {"ci_toml_families": ["one", "two"]},
                "h": {"ci_toml_families": ["two"]},
            },
            "two groups",
        ),
        ({"g": {"ci_toml_families": ["one", "two", "ghost"]}}, "does not declare"),
    ],
)
def test_a_family_outside_exactly_one_group_is_an_error(
    groups: dict, message: str
) -> None:
    ci_toml = {
        "cache": {
            "budget": "1GB",
            "family": {
                "one": {"prefix": "one-", "max": "1MB"},
                "two": {"prefix": "two-", "max": "1MB"},
            },
        }
    }
    with pytest.raises(guard.cache_families.CacheFamilyError, match=message):
        guard.cache_families.compose_budget(ci_toml, groups)


def test_an_unmapped_via_is_an_error_not_a_guess() -> None:
    ci_toml = {
        "cache": {
            "budget": "1GB",
            "family": {"x": {"via": "setup-soldr:nope", "max": "1MB"}},
        }
    }
    with pytest.raises(guard.cache_families.CacheFamilyError, match="VIA_PREFIXES"):
        guard.cache_families.compose_budget(ci_toml, {"g": {"ci_toml_families": ["x"]}})
