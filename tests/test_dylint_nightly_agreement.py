"""Every Dylint nightly pin in the tree must agree (soldr#2817).

soldr fetches a **prebuilt** Dylint driver keyed on `<dylint-version>-<nightly>`
and refuses to build one from source:

    Dylint v6.0.3 is not built for this machine (host: x86_64-unknown-linux-gnu;
    missing or unusable component: dylint-driver for nightly-2026-04-16).
    Soldr will not build Dylint from source.

So a file that pins its own nightly does not merely get a different compiler —
it gets *no driver at all*. `ci/fixtures/dylint-cache` was on
`nightly-2026-04-16` while every real dylint had moved to `nightly-2026-05-28`,
and the Dylint Cache and Cook acceptance lanes failed every scheduled run for
three weeks. Both are scheduled-only, so no PR ever ran them.

This is a source check rather than a lane check for that reason: the lanes that
would have caught it are exactly the ones nobody watches.
"""

from __future__ import annotations

import re
from pathlib import Path

from conftest import DYLINT_NIGHTLY, load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
CHANNEL = re.compile(r'^\s*channel\s*=\s*"([^"]+)"', re.MULTILINE)
TARGET_GUARD = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "verify_dylint_target_dirs.py",
    "verify_dylint_target_dirs",
)


def pinned_channel(path: Path) -> str | None:
    match = CHANNEL.search(path.read_text(encoding="utf-8"))
    return match.group(1) if match else None


def dylint_toolchain_files() -> list[Path]:
    """Every first-party Dylint crate's pin, plus the acceptance fixture.

    Vendored copies under a crate's own `.cargo/registry` are excluded — they
    are upstream dependencies' files, not pins this repo controls.
    """
    found = [
        path
        for path in sorted((REPO_ROOT / "dylints").glob("*/rust-toolchain.toml"))
        if ".cargo" not in path.parts
    ]
    fixture = REPO_ROOT / "ci" / "fixtures" / "dylint-cache" / "rust-toolchain.toml"
    if fixture.is_file():
        found.append(fixture)
    return found


def test_the_scan_finds_the_files_it_is_meant_to_guard():
    """A guard that scans nothing reports clean (soldr#2013)."""
    files = dylint_toolchain_files()
    assert len(files) >= 7, f"expected the six dylints plus the fixture, got {files}"
    assert any("fixtures" in str(p) for p in files), "the fixture must be covered"


def test_every_dylint_nightly_pin_agrees():
    pins = {path: pinned_channel(path) for path in dylint_toolchain_files()}
    missing = [str(p) for p, c in pins.items() if c is None]
    assert not missing, f"no channel pinned in: {missing}"

    # `missing` above already proved every value is a str, but the checker
    # cannot see that across the assert, and `sorted` on `str | None` is a
    # type error rather than a style nit.
    distinct = sorted({channel for channel in pins.values() if channel is not None})
    assert len(distinct) == 1, (
        "Dylint nightly pins disagree, so at least one has no prebuilt driver:\n"
        + "\n".join(
            f"  {path.relative_to(REPO_ROOT).as_posix()}: {channel}"
            for path, channel in sorted(pins.items())
        )
    )


def test_ci_test_reads_and_requires_one_exact_dylint_nightly():
    """soldr#2945 moved the read, not the requirement.

    The per-lint `rust-toolchain.toml` loop that used to live in `plan.rs` is
    now the one shared, glob-aware reader in `dylint_libraries.rs` — which is
    the whole point of that change, since `plan.rs` was the *only* caller that
    got the answer right. ci-test still consumes it and still refuses an env
    override that names a different nightly.
    """
    plan = (
        REPO_ROOT / "crates" / "soldr-cli" / "src" / "ci_test" / "plan.rs"
    ).read_text(encoding="utf-8")
    libraries = (
        REPO_ROOT / "crates" / "soldr-cli" / "src" / "dylint_libraries.rs"
    ).read_text(encoding="utf-8")
    host_workflow = (
        REPO_ROOT / ".github" / "workflows" / "_build-and-test.yml"
    ).read_text(encoding="utf-8")

    assert host_workflow.count("- name: Run prescribed host validation") == 1
    assert 'ci-test --target "${{ inputs.target }}"' in host_workflow
    assert 'join("rust-toolchain.toml")' in libraries
    assert "conflicting Dylint library toolchain pins" in libraries
    assert "dylint_libraries::toolchain_state" in plan
    assert "lint libraries resolved to" in plan


def test_ci_test_dylint_domains_share_nightly_keyed_target_directories():
    plan = (
        REPO_ROOT / "crates" / "soldr-cli" / "src" / "ci_test" / "plan.rs"
    ).read_text(encoding="utf-8")
    executor = (
        REPO_ROOT / "crates" / "soldr-cli" / "src" / "ci_test" / "execute.rs"
    ).read_text(encoding="utf-8")

    for target in ('join("libraries")', 'join("target")', 'join("tests")'):
        assert target in plan
    assert '"--target-dir"' in plan
    assert '"CARGO_TARGET_DIR"' in executor
    assert "verify_dylint_test_targets" in executor
    assert "verify_target_tree" in executor


def test_dylint_target_guard_allows_known_bookkeeping(tmp_path: Path):
    target = tmp_path / "dylints" / "one" / "target"
    (target / "debug").mkdir(parents=True)
    for relative in (
        ".rustc_info.json",
        "CACHEDIR.TAG",
        "debug/.cargo-lock",
    ):
        (target / relative).touch()

    assert TARGET_GUARD.local_dylint_target_artifacts(tmp_path) == []


def test_dylint_target_guard_reports_real_local_artifacts(tmp_path: Path):
    artifact = (
        tmp_path / "dylints" / "one" / "target" / "debug" / "deps" / "libone.rlib"
    )
    artifact.parent.mkdir(parents=True)
    artifact.touch()

    assert TARGET_GUARD.local_dylint_target_artifacts(tmp_path) == [artifact]


def test_dylint_target_guard_requires_materialized_shared_dependencies(tmp_path: Path):
    shared = tmp_path / "target" / "dylint" / "tests" / DYLINT_NIGHTLY
    (shared / "debug" / "deps").mkdir(parents=True)
    assert not TARGET_GUARD.has_materialized_shared_dependencies(shared)

    (shared / "debug" / "deps" / "empty-directory").mkdir()
    assert not TARGET_GUARD.has_materialized_shared_dependencies(shared)

    (shared / "debug" / "deps" / "dylint_testing.rlib").touch()
    assert TARGET_GUARD.has_materialized_shared_dependencies(shared)


def test_published_driver_nightly_constant_matches_the_lint_pins():
    """`known_tools::DYLINT_DRIVER_NIGHTLY` names the nightly the in-tree lints pin.

    A drift would send soldr's own lint lane down the unpublished-pin path.
    """
    known_tools = (REPO_ROOT / "crates/soldr-fetch/src/fetch/known_tools.rs").read_text(
        encoding="utf-8"
    )
    match = re.search(
        r'pub const DYLINT_DRIVER_NIGHTLY: &str = "([^"]+)";', known_tools
    )
    assert match, "DYLINT_DRIVER_NIGHTLY constant not found"
    assert DYLINT_NIGHTLY.startswith(match.group(1) + "-")
