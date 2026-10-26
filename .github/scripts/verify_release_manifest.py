#!/usr/bin/env python3
"""Validate the manifest.json shipped inside a release archive.

`manifest.json` is the release's own description of itself: versions, target
triples, per-binary sha256s, provenance. setup-soldr, the npm install wrapper
and ad-hoc tooling all read it, and nothing checked it was right.

It was not. Published v0.8.29 carries:

    "crgx":       {"source_commit": "soldr-toolchain:v0.1.0\\n"
                                    "CARGO_CHEF_SOURCE_COMMIT=soldr-toolchain:v0.1.73\\n"}
    "cargo_chef": {"source_commit": "unknown"}

because the step writing `$GITHUB_ENV` terminated its lines with a literal
backslash-n instead of a newline, so both assignments landed on one line:
crgx's value swallowed the cargo-chef assignment and cargo-chef's own variable
was never set. The sha256s were all correct -- this is provenance metadata,
not integrity -- but provenance that silently reads "unknown" is worse than
absent, because it looks answered.

The checks are deliberately about *shape*, not values, so this keeps working
as versions move:

  1. every declared sha256 is 64 hex characters, and matches the file on disk
     when the archive contents are available;
  2. no string field contains a newline or a `KEY=value` fragment -- that is
     the signature of environment-variable leakage;
  3. required tool entries are present and carry a non-empty version.

Usage:
    python3 .github/scripts/verify_release_manifest.py <manifest.json> \\
        [--package-dir dist/package]

Exit codes:
  0 - the manifest is well formed
  1 - a problem was found, or the manifest could not be read
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path

SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
# An UPPER_SNAKE assignment starting a word -- how a leaked $GITHUB_ENV
# assignment looks once it has been pasted into a JSON string.
#
# Anchored on start-of-string or whitespace rather than allowed anywhere: a
# legitimate value may well contain `=` mid-token (`?REF=v1` in a URL), and a
# check that fires on those would be turned off rather than fixed. The real
# leak always arrives at a word boundary, whether the newline survived or not.
ENV_LEAK_RE = re.compile(r"(?:^|\s)[A-Z][A-Z0-9_]{2,}=")

REQUIRED_TOOLS = ("soldr", "crgx", "cargo_chef")
BUILD_STATUSES = {"native-built", "cross-built"}
EXECUTION_STATUSES = {
    "executed-in-build-job",
    "required-before-publication",
    "not-executed",
}


@dataclass(frozen=True)
class StringField:
    path: str
    value: str


def find_string_fields(value, path: str = "") -> "list[StringField]":
    """Every string leaf in the document, with its dotted path."""
    found: list[StringField] = []
    if isinstance(value, str):
        found.append(StringField(path, value))
    elif isinstance(value, dict):
        for key, sub in value.items():
            found.extend(find_string_fields(sub, f"{path}.{key}" if path else str(key)))
    elif isinstance(value, list):
        for index, sub in enumerate(value):
            found.extend(find_string_fields(sub, f"{path}[{index}]"))
    return found


def env_leak_problems(manifest: dict) -> "list[str]":
    """Fields that look like they captured shell environment text."""
    problems: list[str] = []
    for field in find_string_fields(manifest):
        path, value = field.path, field.value
        if "\n" in value or "\r" in value:
            problems.append(
                f"{path} contains a newline, which no manifest field should: {value!r}"
            )
        elif ENV_LEAK_RE.search(value):
            problems.append(
                f"{path} looks like a leaked environment assignment: {value!r}"
            )
    return problems


def sha_problems(manifest: dict, package_dir: "Path | None") -> "list[str]":  # noqa: C901
    """Malformed digests, and mismatches when the files are available."""
    problems: list[str] = []
    for field in find_string_fields(manifest):
        path, value = field.path, field.value
        if not path.endswith("sha256"):
            continue
        if not SHA256_RE.match(value):
            problems.append(f"{path} is not a lowercase 64-char sha256: {value!r}")

    if package_dir is None:
        return problems

    def check(name: str, expected: str, where: str) -> None:
        candidate = package_dir / name
        if not candidate.is_file():
            problems.append(f"{where} names {name}, which is not in {package_dir}")
            return
        actual = hashlib.sha256(candidate.read_bytes()).hexdigest()
        if actual != expected:
            problems.append(
                f"{where} sha256 mismatch for {name}: manifest {expected}, actual {actual}"
            )

    for tool in REQUIRED_TOOLS:
        entry = manifest.get(tool)
        if not isinstance(entry, dict):
            continue
        binary, sha = entry.get("binary"), entry.get("sha256")
        if isinstance(binary, str) and isinstance(sha, str) and SHA256_RE.match(sha):
            check(binary, sha, tool)
        for index, sidecar in enumerate(entry.get("sidecars") or []):
            if not isinstance(sidecar, dict):
                continue
            name, sidecar_sha = sidecar.get("name"), sidecar.get("sha256")
            if (
                isinstance(name, str)
                and isinstance(sidecar_sha, str)
                and SHA256_RE.match(sidecar_sha)
            ):
                check(name, sidecar_sha, f"{tool}.sidecars[{index}]")

        # Debug-symbol sidecars (docs/DEBUG_SIDECARS.md). Declared with a
        # digest that nothing re-derived, so a corrupted or swapped .pdb
        # passed the check whose whole job is "the manifest describes what is
        # staged". Windows ships one on every release.
        for index, sidecar in enumerate(entry.get("debug_info") or []):
            if not isinstance(sidecar, dict):
                continue
            name = sidecar.get("name")
            sidecar_sha = sidecar.get("sha256")
            where = f"{tool}.debug_info[{index}]"
            if not isinstance(name, str) or not isinstance(sidecar_sha, str):
                continue
            candidate = package_dir / name
            if candidate.is_dir():
                # A macOS dSYM is a directory, and auto-release hashes it as a
                # `tar -cf -` stream. That is not reproducible here (member
                # order, mtimes, uid/gid), so re-deriving it would invent
                # failures. Presence is what can honestly be checked.
                continue
            if not SHA256_RE.match(sidecar_sha):
                continue
            check(name, sidecar_sha, where)
    return problems


def structure_problems(manifest: dict) -> "list[str]":
    problems: list[str] = []
    if not isinstance(manifest.get("schema_version"), int):
        problems.append("schema_version is missing or not an integer")
    for tool in REQUIRED_TOOLS:
        entry = manifest.get(tool)
        if not isinstance(entry, dict):
            problems.append(f"{tool} entry is missing")
            continue
        version = entry.get("version")
        if not isinstance(version, str) or not version.strip():
            problems.append(f"{tool}.version is missing or empty")
    if manifest.get("schema_version", 0) >= 4:
        provenance = manifest.get("artifact_provenance")
        if not isinstance(provenance, dict):
            problems.append("artifact_provenance is missing or not an object")
            return problems
        build = provenance.get("build")
        build_status = build.get("status") if isinstance(build, dict) else None
        if build_status not in BUILD_STATUSES:
            problems.append(
                "artifact_provenance.build.status must be one of "
                f"{sorted(BUILD_STATUSES)}, got {build_status!r}"
            )
        execution = provenance.get("execution")
        for artifact in ("archive", "wheel"):
            record = execution.get(artifact) if isinstance(execution, dict) else None
            status = record.get("status") if isinstance(record, dict) else None
            if status not in EXECUTION_STATUSES:
                problems.append(
                    f"artifact_provenance.execution.{artifact}.status must be one of "
                    f"{sorted(EXECUTION_STATUSES)}, got {status!r}"
                )
    return problems


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", help="path to manifest.json")
    parser.add_argument(
        "--package-dir",
        type=Path,
        default=None,
        help="directory holding the binaries, to verify digests against bytes",
    )
    args = parser.parse_args(argv)

    try:
        manifest = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(
            f"verify_release_manifest: cannot read {args.manifest}: {error}",
            file=sys.stderr,
        )
        return 1

    if not isinstance(manifest, dict):
        print("verify_release_manifest: manifest is not a JSON object", file=sys.stderr)
        return 1

    problems = (
        structure_problems(manifest)
        + sha_problems(manifest, args.package_dir)
        + env_leak_problems(manifest)
    )

    if problems:
        print(
            f"verify_release_manifest: {args.manifest} has {len(problems)} problem(s):",
            file=sys.stderr,
        )
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1

    print(f"verify_release_manifest: {args.manifest} is well formed - OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
