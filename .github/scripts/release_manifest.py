#!/usr/bin/env python3
"""Write the release archive's `manifest.json` (soldr#2469 step 2.2).

The manifest is the release describing itself — versions, target triples,
per-binary sha256s, provenance — and setup-soldr, the npm install wrapper and
ad-hoc tooling all read it. It was assembled by 127 lines of inline bash that
built JSON with a heredoc, which is how published v0.8.29 shipped
`crgx.source_commit` holding a two-line value with `cargo_chef.source_commit`
reading `"unknown"`: a `$GITHUB_ENV` writer had terminated lines with a
literal backslash-n and the heredoc happily interpolated it.

`json.dumps` makes that class of corruption structurally impossible — a
newline inside a value is escaped rather than able to end a field — which is
the main reason this belongs in Python rather than in a heredoc.

Deliberately preserved from the bash, byte for byte:

* Windows `.pdb` is REQUIRED and its absence fails the build; it is the only
  sidecar this manifest (and therefore `soldr.debug_info`) ever describes.
* format tags must match `release_sidecar.rs::DebugSidecarFormat::as_manifest_str`.

soldr#3038: Linux `.dwp` and macOS `.dSYM` are deliberately NEVER collected
here, even though the release profile now emits them. `soldr.debug_info` is
read by every `setup-soldr` consumer via the vendored
`.github/actions/setup-soldr/zccache_contract.py::validate_release_manifest`,
which hard-rejects any entry whose `format` is not `"pdb"` -- turning on
split-debuginfo without this exclusion would have broken `setup-soldr` for
every Linux/macOS user on the next release. `collect_debug_info` below only
ever looks at `package_dir`, and `stage_release_binaries.py` deliberately
never stages a `.dwp`/`.dSYM` there (see its `stage_debug_symbols`) -- the
sidecar instead ships as its own opt-in `-symbols.tar.zst` release asset that
setup-soldr never downloads or parses. See docs/DEBUG_SIDECARS.md.

Usage (CI):
    python3 .github/scripts/release_manifest.py \
        --version v0.9.2 --commit-sha <sha> --target x86_64-unknown-linux-gnu \
        --package-dir dist/package
"""

from __future__ import annotations

import argparse
import copy
import datetime
import hashlib
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path

from release_artifacts import binary_suffix

REPO_ROOT = Path(__file__).resolve().parents[2]

SCHEMA_VERSION = 4
ARCHIVE_FORMAT = "tar.zst"
ARCHIVE_COMPRESSION_LEVEL = 19


@dataclass(frozen=True)
class PinnedVersionSpec:
    """Where one pinned version lives: source file, regex, and human name."""

    relative: str
    pattern: str
    label: str


# One spec for each pinned version the manifest reports. Read from the tree
# rather than passed in, so the manifest cannot disagree with the source it
# was built from.
ZCCACHE_VERSION = PinnedVersionSpec(
    "Cargo.lock",
    r'(?ms)^\[\[package\]\]\nname = "zccache"\nversion = "([^"]+)"',
    "zccache",
)
CRGX_VERSION = PinnedVersionSpec(
    "crates/soldr-fetch/src/fetch/mod.rs",
    r'MANAGED_CRGX_VERSION: &str = "(.*)";',
    "MANAGED_CRGX_VERSION",
)
CARGO_CHEF_VERSION = PinnedVersionSpec(
    "crates/soldr-fetch/src/fetch/known_tools.rs",
    r'CARGO_CHEF_PINNED_VERSION: &str = "(.*)";',
    "CARGO_CHEF_PINNED_VERSION",
)


class ManifestError(RuntimeError):
    """A precondition the manifest cannot be written without."""


def load_artifact_provenance(root: Path, target: str) -> dict:
    """Return the reviewed build/execution truth for one release target.

    This is deliberately sourced from the canonical target contract rather
    than inferred from the build host.  Cross-built bytes and target-executed
    bytes are different claims, and a missing declaration must stop manifest
    creation instead of silently looking equivalent to execution.
    """
    contract_path = root / "ci" / "canonical-targets.json"
    try:
        contract = json.loads(contract_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ManifestError(f"cannot read {contract_path}: {error}") from error
    for entry in contract.get("targets", []):
        if entry.get("triple") != target:
            continue
        release = entry.get("release")
        provenance = (
            release.get("artifact_provenance") if isinstance(release, dict) else None
        )
        if not isinstance(provenance, dict):
            raise ManifestError(
                f"{target}: release artifact_provenance is missing from {contract_path}"
            )
        return copy.deepcopy(provenance)
    raise ManifestError(f"{target}: target is missing from {contract_path}")


def read_pinned_version(root: Path, spec: PinnedVersionSpec) -> str:
    """First capture of `spec`'s regex in its file.

    Empty is an error rather than a default: a manifest that reports `""` for
    a pinned version looks answered, which the v0.8.29 provenance incident
    showed is worse than absent.
    """
    relative, pattern, label = spec.relative, spec.pattern, spec.label
    text = (root / relative).read_text(encoding="utf-8")
    match = re.search(pattern, text, re.MULTILINE)
    if not match or not match.group(1):
        raise ManifestError(f"could not read {label} from {relative}")
    return match.group(1)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def find_sidecar(
    package_dir: Path, names: list[str], directory: bool = False
) -> Path | None:
    for name in names:
        candidate = package_dir / name
        if candidate.is_dir() if directory else candidate.is_file():
            return candidate
    return None


@dataclass(frozen=True)
class DebugSidecar:
    """One debug-symbol sidecar recorded in the manifest's `debug_info`."""

    name: str
    sha256: str
    format: str

    def to_dict(self) -> dict:
        return {"name": self.name, "sha256": self.sha256, "format": self.format}


def collect_debug_info(package_dir: Path, suffix: str) -> list[DebugSidecar]:
    """The Windows PDB sidecar, if this is a Windows release -- and nothing else.

    soldr#3038: this used to also look for `soldr.dwp` / `soldr.dSYM` in
    `package_dir` and record them here. That path was dormant (the release
    profile emitted no split-debug info at all) until soldr#786's follow-up
    turned `split-debuginfo` on, and turning it on would have made this
    function start recording `format: "dwp"` / `"dsym"` entries that
    `zccache_contract.py::validate_release_manifest` -- the real code every
    `setup-soldr` consumer runs -- hard-rejects (`format` must be `"pdb"`).
    `package_dir` never receives a `.dwp`/`.dSYM` now (see
    `stage_release_binaries.py::stage_debug_symbols`), so there is nothing
    left here to intentionally not-collect; the docstring says so anyway
    because the failure mode is silent and a future edit could easily wire a
    sidecar back into `package_dir` without knowing why that is dangerous.
    """
    entries: list[DebugSidecar] = []
    if suffix:
        pdb = find_sidecar(package_dir, ["soldr.pdb", "soldr_cli.pdb"])
        if pdb is None:
            listing = "\n".join(sorted(p.name for p in package_dir.iterdir()))
            raise ManifestError(
                "missing soldr PDB sidecar in "
                f"{package_dir} for Windows release; contents:\n{listing}"
            )
        entries.append(DebugSidecar(pdb.name, sha256_file(pdb), "pdb"))
    return entries


def build_manifest(
    *,
    version: str,
    commit_sha: str,
    target: str,
    suffix: str,
    digests: dict[str, str],
    debug_info: list[dict[str, str]],
    versions: dict[str, str],
    commits: dict[str, str],
    built_at: str,
    artifact_provenance: dict,
) -> dict:
    """Pure assembly, so the shape is unit-testable without a staged tree."""
    return {
        "schema_version": SCHEMA_VERSION,
        "soldr": {
            "version": version,
            "target": target,
            "binary": f"soldr{suffix}",
            "sha256": digests["soldr"],
            "sidecars": [
                {"name": f"soldr-daemon{suffix}", "sha256": digests["soldr-daemon"]}
            ],
            "debug_info": debug_info,
            "commit_sha": commit_sha,
        },
        "zccache": {
            "version": versions["zccache"],
            "target": target,
            "embedded": True,
        },
        "crgx": {
            "version": versions["crgx"],
            "target": target,
            "binary": f"crgx{suffix}",
            "sha256": digests["crgx"],
            "source_commit": commits["crgx"],
        },
        "cargo_chef": {
            "version": versions["cargo_chef"],
            "target": target,
            "binary": f"cargo-chef{suffix}",
            "sha256": digests["cargo-chef"],
            "source_commit": commits["cargo_chef"],
        },
        "archive": {
            "format": ARCHIVE_FORMAT,
            "compression_level": ARCHIVE_COMPRESSION_LEVEL,
        },
        "artifact_provenance": artifact_provenance,
        "built_at": built_at,
    }


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit-sha", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--package-dir", type=Path, default=Path("dist/package"))
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--crgx-source-commit", default="unknown")
    parser.add_argument("--cargo-chef-source-commit", default="unknown")
    args = parser.parse_args(argv)

    suffix = binary_suffix(args.target)
    package_dir: Path = args.package_dir
    try:
        digests = {
            name: sha256_file(package_dir / f"{name}{suffix}")
            for name in ("soldr", "soldr-daemon", "crgx", "cargo-chef")
        }
        manifest = build_manifest(
            version=args.version,
            commit_sha=args.commit_sha,
            target=args.target,
            suffix=suffix,
            digests=digests,
            debug_info=[
                sidecar.to_dict() for sidecar in collect_debug_info(package_dir, suffix)
            ],
            versions={
                "zccache": read_pinned_version(args.repo_root, ZCCACHE_VERSION),
                "crgx": read_pinned_version(args.repo_root, CRGX_VERSION),
                "cargo_chef": read_pinned_version(args.repo_root, CARGO_CHEF_VERSION),
            },
            commits={
                "crgx": args.crgx_source_commit or "unknown",
                "cargo_chef": args.cargo_chef_source_commit or "unknown",
            },
            built_at=utc_now(),
            artifact_provenance=load_artifact_provenance(args.repo_root, args.target),
        )
    except (ManifestError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 1

    rendered = json.dumps(manifest, indent=2) + "\n"
    (package_dir / "manifest.json").write_text(rendered, encoding="utf-8")
    print("--- manifest.json ---")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
