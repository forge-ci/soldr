"""The winvm lane selects native Windows coverage only for platform
implementation changes (soldr#3556). Anything else exits NOT_APPLICABLE --
not run, not attested -- rather than spending a VM run or claiming a pass."""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
WINVM = load_script_module(ROOT / "ci" / "winvm_lane.py", "soldr_winvm_lane")


def test_ci_only_change_is_not_selected() -> None:
    reason = WINVM.selection_reason(
        [".github/workflows/ci.yml", "ci.toml", "ci/local_gate.py"]
    )
    assert reason is not None and "not selected" in reason


def test_shared_logic_change_is_not_selected() -> None:
    assert (
        WINVM.selection_reason(["crates/soldr-cli/src/lib.rs", "docs/API.md"])
        is not None
    )


def test_platform_implementation_change_is_selected() -> None:
    changed = [
        "crates/soldr-cli/src/lib.rs",
        "crates/soldr-platform/src/platform_win/fs.rs",
    ]
    assert WINVM.selection_reason(changed) is None


def test_toolchain_lockfile_and_lane_inputs_are_selected() -> None:
    for path in (
        "Cargo.lock",
        "rust-toolchain.toml",
        "ci/winvm_guest.ps1",
        ".cargo/config.toml",
    ):
        assert WINVM.selection_reason([path]) is None, path


def test_unknown_change_set_runs() -> None:
    assert WINVM.selection_reason(None) is None
    assert WINVM.selection_reason([]) is None
