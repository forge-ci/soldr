#!/usr/bin/env python3
"""Summarize Cargo JSON unit freshness and enforce first-party expectations."""

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class CompilerArtifact:
    """The two fields of a Cargo `compiler-artifact` message this script reads."""

    fresh: bool
    package_id: object


@dataclass(frozen=True)
class UnitSummary:
    """Fresh/dirty unit counts for one Cargo JSON log."""

    fresh_units: int
    dirty_units: int
    # Cargo emits one dirty compiler-artifact per compiler unit. This is
    # a JSON-level proxy, not a direct wrapper process counter.
    compiler_invocations: int
    first_party_dirty_units: int

    def to_dict(self) -> dict:
        """The emitted JSON object."""
        return {
            "fresh_units": self.fresh_units,
            "dirty_units": self.dirty_units,
            "compiler_invocations": self.compiler_invocations,
            "first_party_dirty_units": self.first_party_dirty_units,
        }


def summarize(path: Path, root_package_id: str) -> UnitSummary:
    units: list[CompilerArtifact] = []
    for line_number, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), 1
    ):
        try:
            message = json.loads(line)
        except json.JSONDecodeError as exc:
            raise ValueError(
                f"{path}:{line_number}: invalid Cargo JSON: {exc}"
            ) from exc
        if message.get("reason") == "compiler-artifact":
            units.append(
                CompilerArtifact(
                    fresh=message.get("fresh") is True,
                    package_id=message.get("package_id"),
                )
            )

    fresh = sum(unit.fresh for unit in units)
    dirty = len(units) - fresh
    first_party_dirty = sum(
        not unit.fresh and unit.package_id == root_package_id for unit in units
    )
    return UnitSummary(
        fresh_units=fresh,
        dirty_units=dirty,
        compiler_invocations=dirty,
        first_party_dirty_units=first_party_dirty,
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("log", type=Path)
    parser.add_argument("--root-package-id", required=True)
    parser.add_argument("--expect-first-party-dirty", type=int, required=True)
    args = parser.parse_args()

    try:
        result = summarize(args.log, args.root_package_id)
    except (OSError, ValueError) as exc:
        print(f"cargo_units: {exc}", file=sys.stderr)
        return 2

    actual = result.first_party_dirty_units
    if actual != args.expect_first_party_dirty:
        print(
            "cargo_units: expected exactly "
            f"{args.expect_first_party_dirty} dirty first-party unit(s); got {actual}",
            file=sys.stderr,
        )
        return 1
    print(json.dumps(result.to_dict(), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
