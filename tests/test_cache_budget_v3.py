"""setup-soldr buildcache-v3 ownership and lineage (merge-group run 41474544217).

zackees/setup-soldr#625 moved the build-cache key to v3 while soldr registered
only v2, so the cache-budget gate failed on a live producer's entry.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
guard = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "check_cache_budget.py", "check_cache_budget"
)
MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"


def entry(key: str, size_bytes: int, created: str = "2026-09-20T00:00:00Z") -> dict:
    return {
        "key": key,
        "ref": "refs/heads/main",
        "sizeInBytes": size_bytes,
        "createdAt": created,
    }


def write_listing(tmp_path: Path, entries: list[dict]) -> Path:
    path = tmp_path / "listing.json"
    path.write_text(
        json.dumps({"captured": "test", "usage_bytes": None, "entries": entries}),
        encoding="utf-8",
    )
    return path


# setup-soldr buildcache-v3 (merge-group run 41474544217)
# --------------------------------------------------------------------------

# The exact key that turned merge-group run 41474544217 red: zackees/setup-soldr
# moved to buildcache-v3 (setup-soldr#625) while soldr registered only v2.
LIVE_V3_KEY = "setup-soldr-buildcache-v3-linux-x64-757e4514c0b6b6ed-12872e99232c5faf-sdbbf770dacb4d26f"


@pytest.mark.parametrize(
    "key",
    [
        LIVE_V3_KEY,
        "setup-soldr-buildcache-v2-linux-x64-" + "a" * 16 + "-" + "b" * 16,
    ],
)
def test_live_setup_soldr_buildcache_generations_are_registered(key: str) -> None:
    families = guard.parse_families(guard.load_manifest(MANIFEST)["budget"]["families"])
    assert guard.family_for(key, families) == "setup-soldr-action-stores"


def test_unknown_buildcache_generation_is_still_unregistered() -> None:
    families = guard.parse_families(guard.load_manifest(MANIFEST)["budget"]["families"])
    assert guard.family_for("setup-soldr-buildcache-v99-unknown", families) is None


def test_live_v3_listing_passes_the_real_budget(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    listing_path = write_listing(tmp_path, [entry(LIVE_V3_KEY, 10)])
    code = guard.main(["--manifest", str(MANIFEST), "--from-json", str(listing_path)])
    out = capsys.readouterr().out
    assert "unregistered" not in out
    assert code == 0, out


def test_v3_lineage_is_per_job_and_retires_superseded_generations() -> None:
    lock, old_lock, tc, old_tc, sd = "a" * 16, "b" * 16, "c" * 16, "d" * 16, "e" * 16

    def key(lock_hash, toolchain=tc, job=""):
        job_part = f"{job}-" if job else ""
        return f"setup-soldr-buildcache-v3-linux-x64-{toolchain}-{job_part}{lock_hash}-s{sd}"

    rows = [
        entry(key(old_lock), 400, created="2026-10-01T00:00:00Z"),
        entry(key(lock), 500, created="2026-10-02T00:00:00Z"),
        entry(key(lock, old_tc), 600, created="2026-09-01T00:00:00Z"),
        # A different job's store on the same platform is its own lineage.
        entry(key(lock, job="clippy"), 700, created="2026-09-01T00:00:00Z"),
        entry(key(old_lock, job="docs"), 800, created="2026-09-01T00:00:00Z"),
    ]
    entries = guard.normalize_entries(rows)
    assert guard.prune_candidates(entries, lock) == [entries[0], entries[2]]


def test_queue_zccache_footprint_rebalance() -> None:
    """Run 41942905548's 2.54 GiB row fits without raising the repo ceiling."""
    manifest = guard.load_manifest(MANIFEST)
    budget = manifest["budget"]
    # Rounded log measurement, represented as three synthetic generations.
    used = int(2.54 * 1024**3)
    entries = guard.normalize_entries(
        [
            entry(f"zccache-unit-v1-linux-lock-m{i}-hash", size)
            for i, size in enumerate((used // 3, used // 3, used - 2 * (used // 3)))
        ]
    )
    old = json.loads(json.dumps(manifest))
    old["budget"]["families"]["zccache-unit"]["max_bytes"] = 2091 * 1024**2
    assert any(
        "family 'zccache-unit'" in problem
        for problem in guard.budget_problems(MANIFEST, old, entries)
    )
    assert guard.budget_problems(MANIFEST, manifest, entries) == []
    assert budget["total_max_bytes"] == 8438 * 1024**2
    assert budget["fail_total_bytes"] == int(9.5 * 1024**3)
    # A single growing archive still trips the unchanged per-family guard.
    oversized = guard.normalize_entries([entry("zccache-unit-growth", 3054 * 1024**2)])
    assert any(
        "family 'zccache-unit'" in problem
        for problem in guard.budget_problems(MANIFEST, manifest, oversized)
    )
