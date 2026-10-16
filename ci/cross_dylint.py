"""Per-target Dylint for the local gate's `cross` lane (soldr#3787).

Per-target Dylint is ~90% of the `cross` lane and is redundant for most
diffs: `.github/scripts/platform_cfg_boundary_ratchet.py` (Lint lane) bans
host-platform selection outside `crates/soldr-platform/`, so a `.rs` change in
any other workspace crate compiles identically for every target and the host
Dylint in the `rust` lane already covers it. Cross Clippy is NOT skipped; it
is what catches a target-agnostic edit calling a target-gated API.

Usage: `python ci/cross_dylint.py <target-triple>`. It skips only on proof
(same shape as `ci/wine_lane.py`, soldr#3704) and otherwise runs the exact
command the gate ran before the skip existed.
"""

from __future__ import annotations

import subprocess
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
# pylint: disable-next=wrong-import-position
from crate_graph import (  # noqa: E402 -- script-relative import precedes this
    changed_paths,
    workspace_crates,
)

ROOT = Path(__file__).resolve().parent.parent
# Files whose content can differ per target: target-specific dependency
# tables, and build scripts branching on CARGO_CFG_TARGET_*.
TARGET_SENSITIVE_FILES = ("Cargo.toml", "build.rs")
PLATFORM_CRATE = "soldr-platform"


def skip_reason(root: Path, changed: list[str] | None) -> str | None:
    """A logged reason to skip, or None to run (the default)."""
    if not changed:
        return None
    try:
        crates = {c.name for c in workspace_crates(root)}
    except (OSError, ValueError, tomllib.TOMLDecodeError):
        return None
    touched: set[str] = set()
    for path in changed:
        parts = path.split("/")
        if len(parts) < 3 or parts[0] != "crates" or parts[1] not in crates:
            return None
        if parts[1] == PLATFORM_CRATE or parts[-1] in TARGET_SENSITIVE_FILES:
            return None
        touched.add(parts[1])
    return (
        f"cross dylint: SKIPPED (soldr#3787) -- changes touch only {sorted(touched)}; "
        "the platform-cfg boundary keeps them target-independent, "
        "so host dylint (rust lane) covers them"
    )


def command(target: str) -> list[str]:
    return [
        "soldr",
        "cargo",
        "dylint",
        "--all",
        "--",
        "--workspace",
        "--all-targets",
        "--target",
        target,
    ]


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: cross_dylint.py <target-triple>", file=sys.stderr)
        return 2
    target = argv[1]
    reason = skip_reason(ROOT, changed_paths(ROOT))
    if reason is not None:
        print(f"{reason} [{target}]", flush=True)
        return 0
    # Inherit stdout/stderr: no pipe capture (zackees/ci.yml PY-003).
    return subprocess.run(command(target), cwd=ROOT, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
