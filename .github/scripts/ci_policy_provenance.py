#!/usr/bin/env python3
"""Validate CI policy provenance without checking out or executing a candidate.

The caller must obtain commit and recursive-tree responses from the trusted
repository API and choose reviewed policy independently of the candidate.
This is one prerequisite of merge enforcement, not a check publisher or a
replacement for fresh PR metadata, run/attempt identity, and job coverage.
Policy updates deliberately require a separate reviewed bootstrap.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Any

SHA = re.compile(r"[0-9a-f]{40}\Z")
SCHEMA = "fleet-ci-policy-provenance/v1"


def _sha(value: Any) -> str:
    if not isinstance(value, str) or not SHA.fullmatch(value):
        raise ValueError("missing or invalid immutable Git object identity")
    return value


def _path(value: Any) -> str:
    if (
        not isinstance(value, str)
        or not value
        or value.startswith("/")
        or "\\" in value
        or any(part in ("", ".", "..") for part in value.split("/"))
        or any(ord(char) < 32 or ord(char) == 127 for char in value)
    ):
        raise ValueError("invalid repository path")
    return value


@dataclass(frozen=True)
class TreeEntry:
    """One Git tree row: path, object kind, file mode and object SHA."""

    path: str
    kind: str
    mode: str
    sha: str


def _entries(evidence: dict, expected_sha: str) -> list[TreeEntry]:
    commit, tree = evidence["commit"], evidence["tree"]
    if _sha(commit["sha"]) != _sha(expected_sha):
        raise ValueError("commit response does not identify the requested revision")
    if _sha(commit["commit"]["tree"]["sha"]) != _sha(tree["sha"]):
        raise ValueError("tree response is not bound to the requested commit")
    if tree.get("truncated") is not False:
        raise ValueError("complete recursive tree response is required")
    rows = tree["tree"]
    if not isinstance(rows, list) or len(rows) > 100_000:
        raise ValueError("invalid or oversized tree inventory")
    seen: set[str] = set()
    result: list[TreeEntry] = []
    for row in rows:
        path = _path(row["path"])
        if path in seen:
            raise ValueError("duplicate path in tree inventory")
        seen.add(path)
        kind, mode = row["type"], row["mode"]
        if (kind, mode) not in (
            ("tree", "040000"),
            ("blob", "100644"),
            ("blob", "100755"),
            ("blob", "120000"),
            ("commit", "160000"),
        ):
            raise ValueError("unrecognized Git tree entry")
        result.append(TreeEntry(path, kind, mode, _sha(row["sha"])))
    return result


def verify_policy_trees(  # noqa: C901
    reviewed: dict,
    candidate: dict,
    *,
    reviewed_sha: str,
    candidate_sha: str,
    protected_prefixes: list[str],
    protected_files: list[str],
) -> dict:
    """Require identical protected blobs, modes and inventory in both revisions.

    All paths under a protected prefix are included, including additions and
    deletions. Protected directory and ancestor hashes are compared as well:
    a nonrecursive inventory cannot hide changed policy descendants. Symlinks
    and submodules in policy refuse approval.
    """
    failures: list[str] = []
    changes: list[str] = []
    checked: list[str] = []
    try:
        if not isinstance(protected_prefixes, list) or not protected_prefixes:
            raise ValueError("nonempty reviewed policy prefixes are required")
        prefixes = []
        for prefix in protected_prefixes:
            if not isinstance(prefix, str) or not prefix.endswith("/"):
                raise ValueError("policy prefixes must end with a slash")
            prefixes.append(_path(prefix[:-1]) + "/")
        if not isinstance(protected_files, list):
            raise ValueError("policy file inventory must be a list")
        files = {_path(path) for path in protected_files}
        base = {entry.path: entry for entry in _entries(reviewed, reviewed_sha)}
        proposed = {entry.path: entry for entry in _entries(candidate, candidate_sha)}
        paths = {
            path
            for path in base.keys() | proposed.keys()
            if path in files
            or any(
                path == prefix[:-1]
                or path.startswith(prefix)
                or prefix.startswith(path + "/")
                for prefix in prefixes
            )
            or any(file.startswith(path + "/") for file in files)
        }
        for path in sorted(paths):
            old, new = base.get(path), proposed.get(path)
            if old == new and old and old.kind == "tree":
                continue
            checked.append(path)
            if any(
                entry and entry.mode in ("120000", "160000") for entry in (old, new)
            ):
                failures.append(f"{path}: policy symlink or submodule is unsupported")
            if old != new:
                changes.append(path)
                failures.append(f"{path}: candidate changes reviewed CI policy")
        if not checked:
            raise ValueError("reviewed policy inventory contains no files")
    except (KeyError, TypeError, ValueError, AttributeError) as error:
        failures.append(f"invalid policy provenance evidence: {error}")
    return {
        "schema": SCHEMA,
        "reviewed_sha": reviewed_sha,
        "candidate_sha": candidate_sha,
        "checked_paths": checked,
        "changed_paths": changes,
        "failures": sorted(set(failures)),
        "success": not failures,
    }
