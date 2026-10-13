"""The Tier-2 zccache working-set measurement (soldr#3120)."""

from __future__ import annotations

import json
import os
import stat
import sys
import time
from pathlib import Path
from types import SimpleNamespace

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
measure = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "measure_zccache_working_set.py",
    "measure_zccache_working_set",
)


def _file(path: Path, size: int) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"x" * size)
    return path


def _store(tmp_path: Path) -> Path:
    store = tmp_path / "daemon-state"
    _file(store / "embedded-v1" / "v1.13.21" / "artifacts" / "old", 300)
    _file(store / "embedded-v1" / "v1.13.22" / "artifacts" / "hit", 100)
    _file(store / "embedded-v1" / "v1.13.22" / "artifacts" / "cold", 50)
    return store


def _after_marker() -> float:
    since = time.time()
    time.sleep(0.05)
    return since


def test_only_files_touched_after_the_marker_count(tmp_path: Path) -> None:
    store = _store(tmp_path)
    since = _after_marker()
    hit = store / "embedded-v1" / "v1.13.22" / "artifacts" / "hit"
    # A hardlink materialization: the link count change bumps ctime.
    os.link(hit, tmp_path / "target-copy")

    walk = measure.walk_store(store, since)

    assert (walk.total_files, walk.total_bytes) == (3, 450)
    assert (walk.touched_files, walk.touched_bytes) == (1, 100)
    assert {
        stats.name: (stats.total_bytes, stats.touched_bytes) for stats in walk.versions
    } == {"v1.13.21": (300, 0), "v1.13.22": (150, 100)}


def test_a_hardlinked_inode_inside_the_store_counts_once(tmp_path: Path) -> None:
    store = _store(tmp_path)
    os.link(store / "embedded-v1" / "v1.13.22" / "artifacts" / "hit", store / "dup")
    assert measure.walk_store(store, time.time() + 60).total_bytes == 450


def test_verdicts_refuse_unmeasurable_walks(tmp_path: Path) -> None:
    cap = 120
    empty = measure.Walk()
    none = measure.Walk(total_files=3, total_bytes=450)
    everything = measure.Walk(
        total_files=3, total_bytes=450, touched_files=3, touched_bytes=450
    )
    fits = measure.Walk(
        total_files=3, total_bytes=450, touched_files=1, touched_bytes=100
    )
    over = measure.Walk(
        total_files=3, total_bytes=450, touched_files=2, touched_bytes=150
    )

    assert "empty" in measure.working_set_verdict(empty, cap)
    assert "unmeasurable" in measure.working_set_verdict(none, cap)
    assert "predates the restore" in measure.working_set_verdict(everything, cap)
    assert "fits under" in measure.working_set_verdict(fits, cap)
    assert "exceeds" in measure.working_set_verdict(over, cap)


def test_the_default_cap_is_the_uncompressed_store_cap_minus_the_reserve() -> None:
    # The trim cap bounds the on-disk store, which the Actions save compresses
    # about 4x; the family's max_bytes is the compressed Actions budget.
    cache_families = load_script_module(
        REPO_ROOT / ".github" / "scripts" / "cache_families.py", "cache_families"
    )
    manifest = cache_families.load_manifest()
    family = manifest["budget"]["families"]["zccache-unit"]
    assert family["store_cap_bytes"] > family["max_bytes"]
    assert (
        measure.default_cap_bytes(REPO_ROOT / "ci" / "cache-ownership.json", 7)
        == family["store_cap_bytes"] - 7
    )


def test_the_store_cap_holds_the_driver_and_ci_test_working_set() -> None:
    # soldr#3458: the driver build's ~381 dependency units plus ci-test's
    # measured 2.92 GiB working set must survive the trim, or every run's
    # driver build starts cold.
    manifest = json.loads(
        (REPO_ROOT / "ci" / "cache-ownership.json").read_text(encoding="utf-8")
    )
    family = manifest["family_groups"]["zccache-unit"]
    assert family["store_cap_bytes"] >= int(2.92 * (1 << 30))
    assert family["evict"] == "newest"
    # soldr#3396: the stable-cook co-tenant is retired, so by default the
    # Tier-2 store is trimmed to the whole family allocation.
    assert measure.DEFAULT_RESERVE_BYTES == 0


def _fake_soldr(tmp_path: Path, source_file: Path) -> Path:
    """A `soldr` that checks it was handed a real copy, then damages that copy."""
    record = tmp_path / "fake-soldr-record.json"
    script = tmp_path / "soldr"
    script.write_text(
        f"""#!{sys.executable}
import json, os, pathlib, sys
root = pathlib.Path(sys.argv[sys.argv.index("--root") + 1])
copy = root / "cache" / "zccache" / "daemon-state" / "embedded-v1" / "v1.13.22" / "artifacts" / "hit"
source = pathlib.Path({str(source_file)!r})
pathlib.Path({str(record)!r}).write_text(json.dumps({{
    "argv": sys.argv[1:],
    "cap": os.environ.get("ZCCACHE_CACHE_SIZE_BYTES"),
    "percent": os.environ.get("ZCCACHE_CACHE_SIZE_PERCENT"),
    "soldr_cache_dir": os.environ.get("SOLDR_CACHE_DIR"),
    "root": str(root),
    "real_copy": copy.stat().st_ino != source.stat().st_ino,
}}))
copy.unlink()
print("noise before json")
print(json.dumps({{"deferred_reason": None, "zccache": {{"pressure": "hard", "usage_after_bytes": 7}}}}))
""",
        encoding="utf-8",
    )
    script.chmod(script.stat().st_mode | stat.S_IXUSR)
    return script


def test_the_trial_trims_a_real_copy_and_leaves_the_store_alone(
    tmp_path: Path, monkeypatch
) -> None:
    store = _store(tmp_path)
    source_hit = store / "embedded-v1" / "v1.13.22" / "artifacts" / "hit"
    scratch = tmp_path / "scratch"
    scratch.mkdir()
    monkeypatch.setenv("ZCCACHE_CACHE_SIZE_PERCENT", "50")
    soldr = _fake_soldr(tmp_path, source_hit)

    trial = measure.trial_trim(store, 450, soldr, 1234, scratch)

    record = json.loads(
        (tmp_path / "fake-soldr-record.json").read_text(encoding="utf-8")
    )
    assert (
        record["argv"][:3] == ["gc", "maintain", "--root"]
        and record["argv"][-1] == "--json"
    )
    assert record["real_copy"] is True
    assert record["cap"] == "1234" and record["percent"] is None
    assert record["soldr_cache_dir"] == record["root"]
    assert trial["report"] == {"pressure": "hard", "usage_after_bytes": 7}
    assert source_hit.read_bytes() == b"x" * 100, "the saved store must not change"
    assert not any(scratch.iterdir()), "the copy must be removed"


def test_the_trial_skips_when_the_copy_would_not_fit(
    tmp_path: Path, monkeypatch
) -> None:
    store = _store(tmp_path)

    monkeypatch.setattr(
        measure.shutil, "disk_usage", lambda _path: SimpleNamespace(free=10)
    )
    trial = measure.trial_trim(store, 450, tmp_path / "missing-soldr", 1, tmp_path)
    assert "skipped" in trial and "need" in trial["skipped"]


def _fake_trim_soldr(tmp_path: Path) -> Path:
    """A `soldr` that records the root it was asked to maintain in place."""
    record = tmp_path / "fake-trim-record.json"
    script = tmp_path / "soldr-trim"
    script.write_text(
        f"""#!{sys.executable}
import json, os, pathlib, sys
root = pathlib.Path(sys.argv[sys.argv.index("--root") + 1])
pathlib.Path({str(record)!r}).write_text(json.dumps({{
    "argv": sys.argv[1:],
    "cap": os.environ.get("ZCCACHE_CACHE_SIZE_BYTES"),
    "percent": os.environ.get("ZCCACHE_CACHE_SIZE_PERCENT"),
    "soldr_cache_dir": os.environ.get("SOLDR_CACHE_DIR"),
    "root": str(root),
}}))
print("noise before json")
print(json.dumps({{"deferred_reason": None, "zccache": {{"pressure": "hard", "usage_after_bytes": 7}}}}))
""",
        encoding="utf-8",
    )
    script.chmod(script.stat().st_mode | stat.S_IXUSR)
    return script


def test_the_trim_maintains_the_real_store_in_place(
    tmp_path: Path, monkeypatch
) -> None:
    # The store sits at <root>/cache/zccache/daemon-state, so the maintain pass
    # must be driven at <root>, in place — never on a copy.
    root = tmp_path / "root"
    store = root / "cache" / "zccache" / "daemon-state"
    hit = _file(store / "embedded-v1" / "v1.13.22" / "artifacts" / "hit", 100)
    monkeypatch.setenv("ZCCACHE_CACHE_SIZE_PERCENT", "50")
    soldr = _fake_trim_soldr(tmp_path)

    trim = measure.trim_store(store, soldr, 1234)

    record = json.loads(
        (tmp_path / "fake-trim-record.json").read_text(encoding="utf-8")
    )
    assert (
        record["argv"][:3] == ["gc", "maintain", "--root"]
        and record["argv"][-1] == "--json"
    )
    assert record["root"] == str(root)
    assert record["cap"] == "1234" and record["percent"] is None
    assert record["soldr_cache_dir"] == str(root)
    assert trim["report"] == {"pressure": "hard", "usage_after_bytes": 7}
    assert hit.read_bytes() == b"x" * 100, "the store is maintained in place"


def test_main_trim_requires_soldr(tmp_path: Path, capsys) -> None:
    store = _store(tmp_path)
    code = measure.main(
        [
            "--store",
            str(store),
            "--since-file",
            str(tmp_path / "absent"),
            "--cap-bytes",
            "100",
            "--trim",
        ]
    )
    assert code == 0
    assert "--trim needs --soldr" in capsys.readouterr().out


def test_main_reports_without_a_marker_and_writes_the_summary(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    store = _store(tmp_path)
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))

    code = measure.main(
        [
            "--store",
            str(store),
            "--since-file",
            str(tmp_path / "absent"),
            "--cap-bytes",
            "100",
        ]
    )

    assert code == 0
    assert "no restore marker" in capsys.readouterr().out
    assert "soldr#3120" in summary.read_text(encoding="utf-8")


def test_main_is_quiet_about_a_missing_store(tmp_path: Path, capsys) -> None:
    code = measure.main(
        [
            "--store",
            str(tmp_path / "nope"),
            "--since-file",
            str(tmp_path / "x"),
            "--cap-bytes",
            "1",
        ]
    )
    assert code == 0
    assert "nothing to measure" in capsys.readouterr().out


def test_trial_false_skips_the_copy_but_still_walks(tmp_path: Path, capsys) -> None:
    store = _store(tmp_path)
    since = tmp_path / "since"
    since.write_text(str(time.time() + 60), encoding="utf-8")
    scratch = tmp_path / "scratch"
    scratch.mkdir()

    code = measure.main(
        [
            "--store",
            str(store),
            "--since-file",
            str(since),
            "--soldr",
            str(tmp_path / "never-run"),
            "--scratch-root",
            str(scratch),
            "--trial",
            "false",
            "--cap-bytes",
            "100",
        ]
    )

    out = capsys.readouterr().out
    assert code == 0
    assert "Working set:" in out and "Trial trim" not in out
    assert not any(scratch.iterdir())


def _touch_after_marker(tmp_path: Path, store: Path) -> float:
    """Mark, then hardlink v1.13.22's `hit` so only that tree is touched."""
    since = _after_marker()
    os.link(
        store / "embedded-v1" / "v1.13.22" / "artifacts" / "hit",
        tmp_path / "materialized",
    )
    return since


def test_prune_removes_only_version_trees_this_run_never_touched(
    tmp_path: Path,
) -> None:
    store = _store(tmp_path)
    since = _touch_after_marker(tmp_path, store)

    pruned = measure.prune_dead_version_dirs(measure.walk_store(store, since))

    assert pruned == [measure.PrunedTree("v1.13.21", 300)]
    assert not (store / "embedded-v1" / "v1.13.21").exists()
    assert (store / "embedded-v1" / "v1.13.22" / "artifacts" / "cold").exists()


def test_prune_refuses_an_invalid_measurement(tmp_path: Path) -> None:
    store = _store(tmp_path)
    nothing_touched = measure.walk_store(store, time.time() + 60)
    everything_touched = measure.walk_store(store, 0.0)

    assert not measure.prune_dead_version_dirs(nothing_touched)
    assert not measure.prune_dead_version_dirs(everything_touched)
    assert (store / "embedded-v1" / "v1.13.21").exists()


def test_a_touched_empty_file_keeps_its_version_tree(tmp_path: Path) -> None:
    store = _store(tmp_path)
    marker = store / "embedded-v1" / "v1.13.21" / "compiler_hash.bin"
    _file(marker, 0)
    since = _after_marker()
    os.chmod(marker, 0o600)  # bumps ctime, 0 bytes touched
    os.link(
        store / "embedded-v1" / "v1.13.22" / "artifacts" / "hit",
        tmp_path / "materialized",
    )

    assert not measure.prune_dead_version_dirs(measure.walk_store(store, since))
    assert (store / "embedded-v1" / "v1.13.21").exists()


def test_prune_never_removes_unversioned_files(tmp_path: Path) -> None:
    store = _store(tmp_path)
    _file(store / "embedded-v1" / "instance.json", 5)
    since = _touch_after_marker(tmp_path, store)

    measure.prune_dead_version_dirs(measure.walk_store(store, since))

    assert (store / "embedded-v1" / "instance.json").exists()


def test_main_prunes_only_when_asked(tmp_path: Path, capsys) -> None:
    store = _store(tmp_path)
    since_file = tmp_path / "since"
    since_file.write_text(str(_touch_after_marker(tmp_path, store)), encoding="utf-8")
    common = ["--store", str(store), "--since-file", str(since_file)]

    measure.main([*common, "--cap-bytes", "1000"])
    assert (store / "embedded-v1" / "v1.13.21").exists()
    assert "Pruned" not in capsys.readouterr().out

    measure.main([*common, "--cap-bytes", "1000", "--prune-dead-version-dirs"])
    assert not (store / "embedded-v1" / "v1.13.21").exists()
    assert "Pruned dead version trees before the save: `v1.13.21`" in (
        capsys.readouterr().out
    )
