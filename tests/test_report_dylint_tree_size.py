"""Regression locks for the Dylint tree-size reporter.

soldr#2996 Phase 6 gates a cache carve-out on this number, and the script
runs inside the 41-minute host lane. It therefore has to be accurate about
the three trees separately and incapable of failing the lane.
"""

import os
from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"


def _module():
    return load_script_module(
        SCRIPTS / "report_dylint_tree_size.py", "report_dylint_tree_size"
    )


def _tree(root: Path, name: str, payload: bytes) -> None:
    directory = root / "dylint" / name / "nightly-2026-05-28"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "artifact.bin").write_bytes(payload)


def test_absent_dylint_root_is_reported_not_an_error(tmp_path) -> None:
    module = _module()
    assert module.main(["--target-root", str(tmp_path)]) == 0
    body = "\n".join(module.report(tmp_path))
    assert "did not run" in body


def test_each_tree_is_reported_separately(tmp_path) -> None:
    """`soldr dylint cook` prewarms only the analysis tree, so a single
    total would hide the part that can come back only from an archive."""
    module = _module()
    _tree(tmp_path, "libraries", b"a" * 2048)
    _tree(tmp_path, "target", b"b" * 1024)
    _tree(tmp_path, "tests", b"c" * 4096)
    body = "\n".join(module.report(tmp_path))
    for name in ("libraries", "target", "tests"):
        assert f"`{name}`" in body
    assert "**total**" in body


def test_sizes_and_counts_are_accurate(tmp_path) -> None:
    module = _module()
    _tree(tmp_path, "libraries", b"x" * 5000)
    tree = module.tree_bytes(tmp_path / "dylint" / "libraries")
    assert tree.total_bytes == 5000
    assert tree.file_count == 1


def test_a_missing_tree_is_labelled_rather_than_skipped(tmp_path) -> None:
    module = _module()
    _tree(tmp_path, "libraries", b"x" * 10)
    body = "\n".join(module.report(tmp_path))
    assert "absent" in body


def test_summary_write_failure_never_fails_the_lane(tmp_path) -> None:
    module = _module()
    _tree(tmp_path, "target", b"x" * 10)
    os.environ["GITHUB_STEP_SUMMARY"] = str(tmp_path / "nope" / "summary.md")
    try:
        assert module.main(["--target-root", str(tmp_path)]) == 0
    finally:
        del os.environ["GITHUB_STEP_SUMMARY"]


def test_human_units_are_readable() -> None:
    module = _module()
    assert module.human(512) == "512 B"
    assert module.human(2048) == "2.0 KiB"
    assert module.human(5 * 1024 * 1024) == "5.0 MiB"
