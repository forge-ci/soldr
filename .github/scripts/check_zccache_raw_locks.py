"""Reject raw lock primitives in the vendored zccache outside `zccache::sync`.

soldr#3836: a raw lock wait never ends and never names itself, and on
2026-10-16 one wedged the daemon for half an hour. soldr's own crates are
held to `soldr_platform::sync` by the `ban_raw_sync_primitive` Dylint, but
`_vender/zccache` is a `[patch.crates-io]` path dependency, not a workspace
member, so no Dylint or clippy pass ever sees it. This source scan is its
gate: production code must use `crate::sync` (bounded waits, watchdogged
holds). Test code is out of scope -- a test lock is held for a whole body.

Each deliberate exception carries its reason: whole files in ALLOWED, single
long-lived gates with a `raw-lock-ok: <reason>` comment on the line or in the
comment block directly above it.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path("_vender/zccache/src")

BANNED = re.compile(
    r"std::sync::(?:Mutex|RwLock|Condvar)\b"
    r"|use std::sync::\{[^}]*\b(?:Mutex|RwLock|Condvar)\b[^}]*\}"
    r"|\bparking_lot::"
    r"|async_engine::(?:Mutex|RwLock|RwLockReadGuard|RwLockWriteGuard)\b"
    r"|use kernal_api::async_engine::\{[^}]*\b(?:Mutex|RwLock)\b[^}]*\}"
    r"|\.blocking_(?:read|write)\(\)"
)

# A deliberate long-lived gate carries this marker on its own line or the
# line above it, followed by the reason.
MARKER = "raw-lock-ok:"

# relative path -> reason. Paths are relative to ROOT.
ALLOWED: dict[str, str] = {
    "sync.rs": "the bounded-wait boundary itself",
    "daemon_core/daemon/spawn_exclusion.rs": (
        "the spawn/materialization lock carries its own hold watchdog"
    ),
    "daemon_core/daemon/server/mod.rs": (
        "StdMutex alias for depgraph_persistence (held across a whole depgraph "
        "save) and staged_materialization_lock (held across a shared store "
        "flock); reasons beside each field in state.rs"
    ),
}


def is_test_file(rel: str) -> bool:
    return (
        rel.endswith("_tests.rs")
        or rel.endswith("/tests.rs")
        or rel == "tests.rs"
        or "/tests/" in f"/{rel}"
        or "test_support" in rel
    )


def production_text(text: str) -> str:
    """Drop a trailing inline `#[cfg(test)] mod ...` block."""
    match = re.search(
        r"^#\[cfg\(test\)\]\s*\n\s*(?:pub(?:\([^)]*\))?\s+)?mod\s", text, re.M
    )
    return text[: match.start()] if match else text


def comment_block_above(lines: list[str], index: int) -> str:
    """The contiguous `//` comment lines directly above `lines[index]`."""
    block = []
    cursor = index - 1
    while cursor >= 0 and lines[cursor].strip().startswith("//"):
        block.append(lines[cursor])
        cursor -= 1
    return "\n".join(block)


def violations(root: Path) -> list[str]:
    found = []
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(root).as_posix()
        if is_test_file(rel) or rel in ALLOWED:
            continue
        text = production_text(path.read_text(encoding="utf-8"))
        lines = text.splitlines()
        for number, line in enumerate(lines, start=1):
            stripped = line.strip()
            if stripped.startswith("//"):
                continue
            if MARKER in line or MARKER in comment_block_above(lines, number - 1):
                continue
            if BANNED.search(line):
                found.append(f"{root.as_posix()}/{rel}:{number}: {stripped}")
    return found


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args(argv)
    found = violations(args.root)
    for line in found:
        print(line)
    if found:
        print(
            f"\n{len(found)} raw lock use(s) in vendored zccache production code. "
            "Use crate::sync (Mutex/RwLock/Condvar/AsyncMutex/AsyncRwLock) -- "
            "soldr#3836.",
            file=sys.stderr,
        )
        return 1
    print("zccache raw-lock check: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
