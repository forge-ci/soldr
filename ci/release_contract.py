"""Pure release planning contracts; no function authenticates or executes anything.

Callers must authenticate issue directives, canonical source and native receipts
independently before using these results. A supplied proof is data, not authority.
"""

import gzip
import base64
import io
import tarfile
import copy
import hashlib
import importlib.util
import json
import re
import uuid
import zlib
from pathlib import Path


class ContractError(ValueError):
    """Invalid or conflicting declarative release state."""


def canonical_digest(value):
    return hashlib.sha256(
        json.dumps(
            value, sort_keys=True, separators=(",", ":"), allow_nan=False
        ).encode()
    ).hexdigest()


def _require(condition, message):
    if not condition:
        raise ContractError(message)


def _digest(value, length=64):
    return isinstance(value, str) and re.fullmatch(rf"[0-9a-f]{{{length}}}", value)


def _set(value, label):
    _require(isinstance(value, list) and value, f"{label} must be a nonempty list")
    _require(all(isinstance(x, str) and x for x in value), f"invalid {label}")
    _require(len(set(value)) == len(value), f"duplicate {label}")
    return set(value)


def _assets(tag, targets, destinations):
    # Keep filenames in the existing canonical release implementation.
    path = Path(__file__).parents[1] / ".github/scripts/release_completeness.py"
    spec = importlib.util.spec_from_file_location("release_completeness_contract", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    names = set()
    if "github" in destinations:
        names.update(module.expected_github_assets(tag, sorted(targets)))
    if "pypi" in destinations:
        names.update(module.expected_pypi_files(tag, sorted(targets)))
    return sorted(names)


def validate_directive(value, contract):
    """Validate untrusted data against caller-authenticated canonical source."""
    keys = {
        "schema_version",
        "repository",
        "issue",
        "attempt_id",
        "candidate_sha",
        "version",
        "tag",
        "mode",
        "targets",
        "destinations",
        "partial_reason",
        "expected_artifacts",
        "contract_sha256",
    }
    _require(isinstance(value, dict) and set(value) == keys, "directive fields differ")
    _require(
        type(value["schema_version"]) is int and value["schema_version"] == 1, "schema"
    )
    _require(
        isinstance(value["repository"], str)
        and re.fullmatch(
            r"[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*",
            value["repository"],
        ),
        "repository",
    )
    _require(type(value["issue"]) is int and value["issue"] > 0, "issue")
    try:
        _require(
            str(uuid.UUID(value["attempt_id"])) == value["attempt_id"], "canonical UUID"
        )
    except (ValueError, TypeError, AttributeError) as error:
        raise ContractError("attempt UUID") from error
    _require(_digest(value["candidate_sha"], 40), "candidate SHA")
    _require(
        isinstance(value["version"], str)
        and re.fullmatch(
            r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value["version"]
        ),
        "version",
    )
    _require(value["tag"] == "v" + value["version"], "tag/version mismatch")
    _require(
        value["contract_sha256"] == canonical_digest(contract),
        "canonical contract drift",
    )
    rows = contract["targets"]
    included = {row["triple"] for row in rows if row["release"]["status"] == "included"}
    _require(
        len({row["triple"] for row in rows}) == len(rows), "duplicate canonical targets"
    )
    selected = _set(value["targets"], "targets")
    _require(selected <= included, "unknown distribution target")
    destinations = _set(value["destinations"], "destinations")
    _require(
        destinations <= {"github", "pypi", "npm"},
        "unsupported destination; crates.io requires separate applicability",
    )
    _require(value["mode"] in ("full", "partial"), "mode")
    _require(isinstance(value["partial_reason"], str), "partial reason")
    if value["mode"] == "full":
        _require(
            selected == included and destinations == {"github", "pypi", "npm"},
            "full scope incomplete",
        )
        _require(not value["partial_reason"], "full cannot have partial reason")
    else:
        _require(bool(value["partial_reason"].strip()), "partial reason required")
    expected = _assets(value["tag"], selected, destinations)
    _require(value["expected_artifacts"] == expected, "expected artifact set mismatch")
    return copy.deepcopy(value)


def crate_applicability(manifests):
    """Manifests are caller-authenticated parsed Cargo documents, not authority."""
    _require(isinstance(manifests, list) and manifests, "crate inventory required")
    _require(
        all(
            isinstance(x, dict) and isinstance(x.get("package"), dict)
            for x in manifests
        ),
        "invalid crate inventory",
    )
    if all(x["package"].get("publish") is False for x in manifests):
        return {
            "applicable": False,
            "reason": "all inventoried workspace crates declare publish = false",
        }
    return {
        "applicable": True,
        "reason": "publishable crate requires registry preflight and soldr cargo publish --dry-run",
    }


def _files(files, expected):
    _require(isinstance(files, list), "files list required")
    names = []
    for row in files:
        _require(
            isinstance(row, dict) and set(row) == {"name", "size", "sha256"},
            "file fields",
        )
        name = row["name"]
        _require(
            isinstance(name, str)
            and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.+-]*", name)
            and ".." not in name,
            "unsafe filename",
        )
        _require(type(row["size"]) is int and row["size"] > 0, "file size")
        _require(_digest(row["sha256"]), "file digest")
        names.append(name)
    _require(
        len(set(names)) == len(names) and sorted(names) == expected, "file set mismatch"
    )
    return sorted(copy.deepcopy(files), key=lambda row: row["name"])


MAX_NPM_EXPANDED_BYTES = 64_000_000


def freeze_npm_package(package, version):
    """Inspect supplied pack bytes, never run npm or authenticate their source.

    Producer must bind source package.json and tarball to the exact candidate.
    It must publish these frozen bytes, not rebuild with npm publish(source).
    """
    _require(
        isinstance(package, dict) and set(package) == {"package_json", "archive"},
        "npm package fields",
    )
    source, data = package["package_json"], package["archive"]
    _require(isinstance(source, dict), "npm source metadata")
    name = source.get("name")
    _require(
        isinstance(name, str)
        and re.fullmatch(r"(?:@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*", name),
        "npm package name",
    )
    _require(source.get("version") == version, "npm source version mismatch")
    _require(
        isinstance(data, bytes) and 0 < len(data) <= 100_000_000,
        "npm archive byte ceiling",
    )
    # Bound every decompressed byte before tarfile can process hidden PAX/GNU
    # metadata. Member-only bounds cannot protect tarfile's metadata parser.
    expanded = io.BytesIO()
    try:
        with gzip.GzipFile(fileobj=io.BytesIO(data)) as compressed:
            while True:
                chunk = compressed.read(
                    min(65536, MAX_NPM_EXPANDED_BYTES - expanded.tell() + 1)
                )
                if not chunk:
                    break
                _require(
                    expanded.tell() + len(chunk) <= MAX_NPM_EXPANDED_BYTES,
                    "npm expanded byte ceiling",
                )
                expanded.write(chunk)
    except (OSError, EOFError, zlib.error) as error:
        raise ContractError("invalid npm gzip archive") from error
    expanded.seek(0)
    rows, metadata, total = [], None, 0
    try:
        with tarfile.open(fileobj=expanded, mode="r:") as archive:
            seen = set()
            for entry in archive:
                _require(len(seen) < 1024, "npm archive member ceiling")
                path = entry.name
                _require(
                    path.startswith("package/")
                    and all(part not in ("", ".", "..") for part in path.split("/"))
                    and "\\" not in path
                    and path not in seen,
                    "unsafe or duplicate npm member",
                )
                seen.add(path)
                _require(entry.isfile(), "npm member must be regular file")
                _require(0 <= entry.size <= 16_000_000, "npm member byte ceiling")
                total += entry.size
                _require(total <= MAX_NPM_EXPANDED_BYTES, "npm expanded byte ceiling")
                stream = archive.extractfile(entry)
                content = stream.read(entry.size + 1)
                _require(len(content) == entry.size, "npm member truncated")
                rows.append(
                    {
                        "name": path,
                        "size": entry.size,
                        "sha256": hashlib.sha256(content).hexdigest(),
                    }
                )
                if path == "package/package.json":
                    metadata = json.loads(content)
    except (tarfile.TarError, OSError, ValueError) as error:
        raise ContractError("invalid npm archive") from error
    _require(
        isinstance(metadata, dict)
        and metadata.get("name") == name
        and metadata.get("version") == version,
        "npm packed metadata mismatch",
    )
    return {
        "name": name,
        "version": version,
        "filename": name.removeprefix("@").replace("/", "-") + "-" + version + ".tgz",
        "size": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
        "integrity": "sha512-"
        + base64.b64encode(hashlib.sha512(data).digest()).decode(),
        "source_metadata_sha256": canonical_digest(source),
        "files": sorted(rows, key=lambda row: row["name"]),
    }


def freeze_info(directive, contract, files, *, npm_package=None):
    value = validate_directive(directive, contract)
    _require(
        npm_package is None or "npm" in value["destinations"], "undeclared npm artifact"
    )
    return {
        "schema_version": 1,
        "issue": value["issue"],
        "attempt_id": value["attempt_id"],
        "directive_sha256": canonical_digest(value),
        "contract_sha256": value["contract_sha256"],
        "directive": value,
        "files": _files(files, value["expected_artifacts"]),
        "npm_package": (
            freeze_npm_package(npm_package, value["version"])
            if npm_package is not None and "npm" in value["destinations"]
            else None
        ),
    }


def require_same_directive(existing, desired):
    _require(
        canonical_digest(existing) == canonical_digest(desired),
        "frozen directive identity or scope changed",
    )


def require_frozen_info(frozen, directive, contract, files, *, npm_package=None):
    _require(
        canonical_digest(frozen)
        == canonical_digest(
            freeze_info(directive, contract, files, npm_package=npm_package)
        ),
        "frozen info or artifact bytes changed",
    )


def validate_native_proof(proof, directive, contract, required_cells):
    """Check receipt shape only; caller MUST independently authenticate its producer."""
    validate_directive(directive, contract)
    canonical = {row["ci"]["run_job"] for row in contract["targets"]}
    required = _set(required_cells, "trusted required cells")
    _require(canonical <= required, "required cells omit canonical native execution")
    _require(
        isinstance(proof, dict)
        and set(proof) == {"candidate_sha", "contract_sha256", "cells"},
        "proof fields",
    )
    _require(
        proof["candidate_sha"] == directive["candidate_sha"]
        and proof["contract_sha256"] == directive["contract_sha256"],
        "stale or foreign proof",
    )
    _require(isinstance(proof["cells"], list), "proof cells")
    seen = set()
    for cell in proof["cells"]:
        _require(
            isinstance(cell, dict) and set(cell) == {"id", "status", "native"},
            "cell fields",
        )
        _require(
            isinstance(cell["id"], str)
            and cell["id"] in required
            and cell["id"] not in seen,
            "unknown or duplicate cell",
        )
        _require(
            cell["status"] == "success" and cell["native"] is True,
            "native cell not passed",
        )
        seen.add(cell["id"])
    _require(seen == required, "missing native cell")


_TRANSITIONS = {
    "pending": {"full-ci"},
    "full-ci": {"artifacts-ready", "incomplete"},
    "artifacts-ready": {"publishing", "incomplete"},
    "publishing": {"incomplete", "complete", "partial-complete"},
    "incomplete": {"full-ci"},
    "complete": set(),
    "partial-complete": set(),
}


def validate_transition(
    current,
    following,
    *,
    directive,
    contract,
    proof=None,
    required_cells=None,
    info=None,
    files=None,
    outcomes=None,
    npm_package=None,
):
    """Nonexecuting state validator. Inputs do not grant publication authority."""
    validate_directive(directive, contract)
    _require(
        isinstance(current, str)
        and current in _TRANSITIONS
        and following in _TRANSITIONS[current],
        "invalid transition",
    )
    if following in {"artifacts-ready", "publishing", "complete", "partial-complete"}:
        _require(
            "npm" not in directive["destinations"] or npm_package is not None,
            "npm package byte identity not frozen",
        )
        validate_native_proof(proof, directive, contract, required_cells)
        require_frozen_info(info, directive, contract, files, npm_package=npm_package)
    if following in {"complete", "partial-complete"}:
        _require(
            (following == "complete") == (directive["mode"] == "full"),
            "partial cannot satisfy full completion",
        )
        _require(
            isinstance(outcomes, dict)
            and set(outcomes) == set(directive["destinations"])
            and all(x == "verified" for x in outcomes.values()),
            "destination incomplete",
        )
    return following
