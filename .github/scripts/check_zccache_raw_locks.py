"""Reject raw locks and raw blocking waits in the vendored zccache.

Locks must go through `zccache::sync`; every other blocking wait (channel
`recv`, thread `join`, child `wait`, `Command` output/status, file locks,
`block_on`) through the timeout-only `zccache::wait`.

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
    # Raw blocking waits (the timeout-only replacements live in crate::wait).
    r"|\.recv\(\)(?!\.await)"
    r"|\.join\(\)"
    r"|\.wait\(\)(?!\.await)"
    r"|\.wait_with_output\(\)(?!\.await)"
    r"|\bblock_on\("
    r"|platform::fs::lock_(?:exclusive|shared)(?:_owned)?\("
    r"|\.lock_(?:exclusive|shared)\(\)"
)

# `.status()` / `.output()` are also HTTP-response methods (reqwest), so they
# count only when the same statement builds a process command.
PROCESS_WAIT = re.compile(r"\.(?:status|output)\(\)(?!\.await)")
PROCESS_CONTEXT = re.compile(r"\bCommand\b|\bStdio\b|\bcmd\b|\bcommand\b")

# A deliberate long-lived gate carries this marker on its own line or the
# line above it, followed by the reason.
MARKER = "raw-lock-ok:"

# relative path -> reason. Paths are relative to ROOT.
ALLOWED: dict[str, str] = {
    "sync.rs": "the bounded-wait boundary itself",
    "wait.rs": "the timeout-only wait boundary itself",
    "daemon_core/daemon/server/persist/staged_store/hook.rs": (
        "test-only pause hooks (declared under #[cfg(test)])"
    ),
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
        or "/mod_tests/" in f"/{rel}"
    )


def production_text(text: str) -> str:
    """Drop a trailing inline `#[cfg(test)] mod ...` block."""
    match = re.search(
        r"^#\[cfg\(test\)\]\s*\n\s*(?:pub(?:\([^)]*\))?\s+)?mod\s", text, re.M
    )
    return text[: match.start()] if match else text


def statement_so_far(lines: list[str], index: int) -> str:
    """The current statement up to and including `lines[index]` (bounded)."""
    parts = [lines[index]]
    cursor = index - 1
    while cursor >= 0 and index - cursor <= 12:
        text = lines[cursor].rstrip()
        if text.endswith((";", "{", "}")):
            break
        parts.append(text)
        cursor -= 1
    return "\n".join(reversed(parts))


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
            previous = lines[number - 2] if number > 1 else ""
            if (
                MARKER in line
                or MARKER in previous
                or MARKER in comment_block_above(lines, number - 1)
            ):
                continue
            if BANNED.search(line) or (
                PROCESS_WAIT.search(line)
                and PROCESS_CONTEXT.search(statement_so_far(lines, number - 1))
            ):
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
            "Use crate::sync for locks and the timeout-only crate::wait for every "
            "other blocking wait -- soldr#3836.",
            file=sys.stderr,
        )
        return 1
    print("zccache raw-lock check: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
