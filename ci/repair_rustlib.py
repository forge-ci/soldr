"""Repair an incomplete cached Windows GNU rustlib in the setup-action smoke job.

The action's Rustup home can report this target as installed while its rustlib
is absent. Keep the toolchain-doctor test strict: restore the target and verify
that the doctor succeeds before the dogfood test suite starts.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from dataclasses import dataclass

TARGET = "x86_64-pc-windows-gnu"


@dataclass(frozen=True)
class DoctorResult:
    status: int
    corrupted: tuple[str, ...]


def doctor() -> DoctorResult:
    with tempfile.TemporaryFile(mode="w+t") as stdout_file:
        with tempfile.TemporaryFile(mode="w+t") as stderr_file:
            process = subprocess.run(
                ["soldr", "toolchain", "doctor", "--json"],
                stdout=stdout_file,
                stderr=stderr_file,
                check=False,
            )
            stdout_file.seek(0)
            stderr_file.seek(0)
            stdout = stdout_file.read()
            stderr = stderr_file.read()
    try:
        report = json.loads(stdout)
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
            f"stdout: {stdout}\nstderr: {stderr}"
        ) from error
    return DoctorResult(process.returncode, tuple(corrupted))


def main() -> int:
    initial = doctor()
    if not initial.corrupted:
        if initial.status != 0:
            print(
                "soldr toolchain doctor failed for a reason other than rustlib integrity",
                file=sys.stderr,
            )
        return initial.status
    if initial.corrupted != (TARGET,):
        print(
            f"unexpected corrupt rustlib targets: {initial.corrupted}", file=sys.stderr
        )
        return 1

    print(f"repairing incomplete cached rustlib for {TARGET}", flush=True)
    subprocess.run(
        ["soldr", "rustup", "component", "remove", "rust-std", "--target", TARGET],
        check=True,
    )
    subprocess.run(["soldr", "rustup", "target", "add", TARGET], check=True)
    repaired = doctor()
    if repaired.status != 0 or repaired.corrupted:
        print(
            f"rustlib repair did not pass toolchain doctor: {repaired.corrupted}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
