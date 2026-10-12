"""soldr#2424 P1: keep the broker topology guarded.

The cutover made the stable broker the only way to a daemon: the first
cacheable compile starts it, and recovery is `soldr daemon stop` + the next
build. CI must not grow the workarounds the cutover retired -- a pre-start
(`soldr daemon start` / `soldr broker start` before a build, as if the
front door could not), or a `sleep` waiting for one to come up. Each one
hides a real cold-start defect behind a workflow line. A deliberate start
needs an entry in ALLOWED with the reason.
"""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCANNED = (".github/workflows", ".github/scripts", ".github/actions", "ci")
SUFFIXES = {".yml", ".yaml", ".sh", ".py", ".ps1"}
PRE_START = re.compile(r"\bsoldr(?:\.exe)?\s+(?:daemon|broker)\s+start\b")
SLEEP_WAIT = re.compile(
    r"\b(?:sleep|Start-Sleep)\b.*\b(?:daemon|broker)\b|\b(?:daemon|broker)\b.*\b(?:sleep|Start-Sleep)\s+\d",
    re.IGNORECASE,
)
ALLOWED = {
    # The tokio-console smoke must start the daemon with its console env
    # before the build it observes; the front door would start it without.
    ("ci/smoke_local.sh", "soldr daemon start"),
}


def offending_lines() -> list[str]:
    found = []
    for directory in SCANNED:
        for path in sorted((ROOT / directory).rglob("*")):
            if path.suffix not in SUFFIXES or not path.is_file():
                continue
            relative = path.relative_to(ROOT).as_posix()
            if relative == "tests/test_broker_topology_guard.py":
                continue
            for number, line in enumerate(
                path.read_text(encoding="utf-8").splitlines(), 1
            ):
                code = line.split("#", 1)[0] if path.suffix in {".sh", ".py"} else line
                for pattern in (PRE_START, SLEEP_WAIT):
                    match = pattern.search(code)
                    if match and (relative, match.group(0).strip()) not in ALLOWED:
                        found.append(f"{relative}:{number}: {line.strip()}")
    return found


def test_no_broker_or_daemon_pre_start_or_sleep_in_ci() -> None:
    offending = offending_lines()
    assert not offending, offending


def test_the_guard_catches_a_pre_start() -> None:
    assert PRE_START.search("  soldr daemon start && soldr cargo build")
    assert PRE_START.search("soldr.exe broker start")
    assert SLEEP_WAIT.search("sleep 5  # let the daemon come up")
    assert not PRE_START.search("soldr daemon stop")
