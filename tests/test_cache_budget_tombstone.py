"""setup-soldr poison markers have bounded ownership separate from live layers."""

import datetime

import pytest
from test_cache_budget_v3 import MANIFEST, entry, guard, write_listing

LIVE_KEY = "solo-tombstone-v1-solo-toolchain-v3-linux-x64-glibc-rustc1.98.1-cb6b9e404-tnone-soldrv0.9.47"


def test_cache_budget_tombstone_ownership() -> None:
    families = guard.parse_families(guard.load_manifest(MANIFEST)["budget"]["families"])
    assert guard.family_for(LIVE_KEY, families) == "solo-tombstone"
    assert guard.family_for(LIVE_KEY.removeprefix("solo-tombstone-v1-"), families) == (
        "pinned-immutable-download"
    )
    assert guard.family_for("unknown-producer-v1-layer", families) is None
    assert guard.family_for("solo-tombstone-v99-layer", families) is None


def test_cache_budget_tombstone_listing(tmp_path, capsys: pytest.CaptureFixture[str]):
    listing = write_listing(tmp_path, [entry(LIVE_KEY, 252)])
    code = guard.main(["--manifest", str(MANIFEST), "--from-json", str(listing)])
    out = capsys.readouterr().out
    assert code == 0, out
    assert "unregistered" not in out


def test_cache_budget_tombstone_lru_reclaims_stale_markers() -> None:
    families = guard.parse_families(guard.load_manifest(MANIFEST)["budget"]["families"])
    # Fill the actual allocation with tiny markers, then exceed it by one.
    count = 1024**2 // 252 + 1
    entries = guard.normalize_entries(
        [entry(f"solo-tombstone-v1-layer-{i}", 252) for i in range(count)]
        + [entry(LIVE_KEY.removeprefix("solo-tombstone-v1-"), 252)]
    )
    victims = guard.eviction_candidates(
        entries, families, set(), datetime.datetime(2026, 11, 7, tzinfo=datetime.UTC)
    )
    assert victims == [entries[0]]
    assert (
        guard.eviction_candidates(
            entries[1:],
            families,
            set(),
            datetime.datetime(2026, 11, 7, tzinfo=datetime.UTC),
        )
        == []
    )
