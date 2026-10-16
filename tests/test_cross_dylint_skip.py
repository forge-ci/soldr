"""Per-target Dylint in the `cross` lane skips only on proof (soldr#3787).

A change confined to workspace crates other than soldr-platform, with no
Cargo.toml or build.rs, compiles identically for every target (the cfg
boundary ratchet), so host Dylint covers it. Anything else runs."""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
CROSS = load_script_module(ROOT / "ci" / "cross_dylint.py", "soldr_cross_dylint")


def test_non_platform_crate_change_skips() -> None:
    reason = CROSS.skip_reason(
        ROOT, ["crates/soldr-cli/src/lib.rs", "crates/soldr-core/src/a.rs"]
    )
    assert reason is not None
    assert "soldr#3787" in reason and "soldr-cli" in reason


def test_platform_crate_change_runs() -> None:
    assert CROSS.skip_reason(ROOT, ["crates/soldr-platform/src/lib.rs"]) is None
    assert (
        CROSS.skip_reason(
            ROOT, ["crates/soldr-cli/src/lib.rs", "crates/soldr-platform/src/x.rs"]
        )
        is None
    )


def test_manifest_and_build_script_run() -> None:
    assert CROSS.skip_reason(ROOT, ["crates/soldr-cli/Cargo.toml"]) is None
    assert CROSS.skip_reason(ROOT, ["crates/soldr-cli/build.rs"]) is None
    assert CROSS.skip_reason(ROOT, ["crates/soldr-fetch/src/build.rs"]) is None


def test_root_inputs_run() -> None:
    for path in ("Cargo.lock", "Cargo.toml", "rust-toolchain.toml", "ci/x.py"):
        assert CROSS.skip_reason(ROOT, [path]) is None, path


def test_unknown_crate_runs() -> None:
    assert CROSS.skip_reason(ROOT, ["crates/nonexistent/src/a.rs"]) is None


def test_empty_or_unknown_change_set_runs() -> None:
    assert CROSS.skip_reason(ROOT, None) is None
    assert CROSS.skip_reason(ROOT, []) is None


def test_command_is_the_gates_prior_command() -> None:
    assert CROSS.command("t") == [
        "soldr", "cargo", "dylint", "--all", "--",
        "--workspace", "--all-targets", "--target", "t",
    ]  # fmt: skip
