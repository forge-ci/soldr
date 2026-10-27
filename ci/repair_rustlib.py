"""Repair an incomplete cached Windows GNU rustlib in the setup-action smoke job.

The action's Rustup home can report this target as installed while its rustlib
is absent. Keep the toolchain-doctor test strict: restore the target and verify
that the doctor succeeds before the dogfood test suite starts.
"""

from __future__ import annotations

import json
import subprocess
import sys
from typing import Any

TARGET = "x86_64-pc-windows-gnu"


def doctor() -> tuple[int, list[str]]:
    result = subprocess.run(
        ["soldr", "toolchain", "doctor", "--json"],
        capture_output=True,
        text=True,
        check=False,
    )
    try:
        report: dict[str, Any] = json.loads(result.stdout)
        probes = report["probes"]
        rustlib = next(
            probe for probe in probes if probe["name"] == "rustlib-integrity"
        )
        corrupted = rustlib["details"].get("corrupted_targets", [])
        if not isinstance(corrupted, list) or not all(
            isinstance(item, str) for item in corrupted
        ):
            raise ValueError("invalid corrupted_targets in toolchain doctor report")
    except (
        json.JSONDecodeError,
        KeyError,
        StopIteration,
        TypeError,
        ValueError,
    ) as error:
        raise RuntimeError(
            f"could not read soldr toolchain doctor report: {error}\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        ) from error
    return result.returncode, corrupted


def main() -> int:
    status, corrupted = doctor()
    if not corrupted:
        if status != 0:
            print(
                "soldr toolchain doctor failed for a reason other than rustlib integrity",
                file=sys.stderr,
            )
        return status
    if corrupted != [TARGET]:
        print(f"unexpected corrupt rustlib targets: {corrupted}", file=sys.stderr)
        return 1

    print(f"repairing incomplete cached rustlib for {TARGET}", flush=True)
    subprocess.run(
        ["soldr", "rustup", "component", "remove", "rust-std", "--target", TARGET],
        check=True,
    )
    subprocess.run(["soldr", "rustup", "target", "add", TARGET], check=True)
    status, corrupted = doctor()
    if status != 0 or corrupted:
        print(
            f"rustlib repair did not pass toolchain doctor: {corrupted}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
