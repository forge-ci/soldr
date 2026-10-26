"""Tests for the release-manifest validator.

`manifest.json` is the release describing itself, and setup-soldr, the npm
wrapper and ad-hoc tooling all read it. Nothing checked it, and it was wrong:
published v0.8.29 has `crgx.source_commit` holding

    "soldr-toolchain:v0.1.0\\nCARGO_CHEF_SOURCE_COMMIT=soldr-toolchain:v0.1.73\\n"

with `cargo_chef.source_commit` reading "unknown", because the step writing
$GITHUB_ENV terminated lines with a literal backslash-n. The digests were all
correct -- this is provenance, not integrity -- but provenance that silently
says "unknown" is worse than absent, because it looks answered.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "verify_release_manifest.py"

GOOD_SHA = "a" * 64


def _manifest(**overrides) -> dict:
    base = {
        "schema_version": 3,
        "soldr": {
            "version": "v0.8.29",
            "target": "x86_64-unknown-linux-musl",
            "binary": "soldr",
            "sha256": GOOD_SHA,
            "sidecars": [{"name": "soldr-daemon", "sha256": GOOD_SHA}],
        },
        "crgx": {
            "version": "0.1.0",
            "binary": "crgx",
            "sha256": GOOD_SHA,
            "source_commit": "soldr-toolchain:v0.1.0",
        },
        "cargo_chef": {
            "version": "0.1.73",
            "binary": "cargo-chef",
            "sha256": GOOD_SHA,
            "source_commit": "soldr-toolchain:v0.1.73",
        },
    }
    base.update(overrides)
    return base


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "verify_release_manifest")


def _write(tmp_path: Path, manifest: dict) -> str:
    path = tmp_path / "manifest.json"
    path.write_text(json.dumps(manifest), encoding="utf-8")
    return str(path)


# --- the actual published defect ------------------------------------------


def test_the_v0_8_29_env_leak_is_caught(mod):
    # Verbatim from the published artifact.
    leaked = _manifest()
    leaked["crgx"]["source_commit"] = (
        "soldr-toolchain:v0.1.0\nCARGO_CHEF_SOURCE_COMMIT=soldr-toolchain:v0.1.73\n"
    )
    problems = mod.env_leak_problems(leaked)
    assert len(problems) == 1
    assert "crgx.source_commit" in problems[0]


def test_an_env_assignment_without_a_newline_is_still_caught(mod):
    # If the leak ever arrives flattened, the KEY= shape is the giveaway.
    leaked = _manifest()
    leaked["crgx"]["source_commit"] = "v0.1.0 CARGO_CHEF_SOURCE_COMMIT=v0.1.73"
    assert len(mod.env_leak_problems(leaked)) == 1


def test_a_bare_trailing_newline_is_caught_on_its_own(mod):
    # The published value trips BOTH checks -- it has a newline and an
    # ENV= fragment -- so the tests above cannot tell them apart. A value
    # carrying only a stray newline isolates the newline check, and is
    # malformed in its own right: `$(...)` capture that keeps its trailing
    # newline produces exactly this.
    trailing = _manifest()
    trailing["crgx"]["source_commit"] = "soldr-toolchain:v0.1.0\n"
    problems = mod.env_leak_problems(trailing)
    assert len(problems) == 1
    assert "newline" in problems[0]


def test_a_clean_manifest_has_no_leak_problems(mod):
    assert mod.env_leak_problems(_manifest()) == []


@pytest.mark.parametrize(
    "value",
    [
        "https://example.com/x?ref=v1",  # lowercase key
        "https://example.com/x?REF=v1",  # uppercase, but mid-token
        "soldr-toolchain:v0.1.0",
        "a=b",  # too short to be a plausible env name
    ],
)
def test_ordinary_values_with_equals_are_not_flagged(mod, value):
    # A check that fires on legitimate values gets switched off rather than
    # fixed, so the rule is anchored at a word boundary: a real leak always
    # arrives at one, a URL query parameter does not.
    ok = _manifest()
    ok["crgx"]["source_commit"] = value
    assert mod.env_leak_problems(ok) == []


def test_leaks_are_found_at_any_depth(mod):
    nested = _manifest()
    nested["soldr"]["sidecars"][0]["name"] = "soldr-daemon\nFOO=bar"
    problems = mod.env_leak_problems(nested)
    assert len(problems) == 1
    assert "soldr.sidecars[0].name" in problems[0]


# --- digests --------------------------------------------------------------


def test_a_malformed_sha_is_reported(mod):
    bad = _manifest()
    bad["crgx"]["sha256"] = "not-a-digest"
    problems = mod.sha_problems(bad, None)
    assert len(problems) == 1
    assert "crgx.sha256" in problems[0]


def test_uppercase_digests_are_rejected(mod):
    bad = _manifest()
    bad["crgx"]["sha256"] = "A" * 64
    assert len(mod.sha_problems(bad, None)) == 1


def test_digests_are_checked_against_the_bytes(mod, tmp_path):
    (tmp_path / "soldr").write_bytes(b"soldr")
    (tmp_path / "soldr-daemon").write_bytes(b"soldr")
    (tmp_path / "crgx").write_bytes(b"crgx")
    (tmp_path / "cargo-chef").write_bytes(b"chef")
    manifest = _manifest()
    manifest["soldr"]["sha256"] = hashlib.sha256(b"soldr").hexdigest()
    manifest["soldr"]["sidecars"][0]["sha256"] = hashlib.sha256(b"soldr").hexdigest()
    manifest["crgx"]["sha256"] = hashlib.sha256(b"crgx").hexdigest()
    manifest["cargo_chef"]["sha256"] = hashlib.sha256(b"chef").hexdigest()
    assert mod.sha_problems(manifest, tmp_path) == []


def test_a_wrong_digest_is_caught_against_the_bytes(mod, tmp_path):
    for name in ("soldr", "soldr-daemon", "crgx", "cargo-chef"):
        (tmp_path / name).write_bytes(b"content")
    manifest = _manifest()
    real = hashlib.sha256(b"content").hexdigest()
    for tool in ("soldr", "crgx", "cargo_chef"):
        manifest[tool]["sha256"] = real
    manifest["soldr"]["sidecars"][0]["sha256"] = real
    manifest["crgx"]["sha256"] = "b" * 64  # declared, but wrong
    problems = mod.sha_problems(manifest, tmp_path)
    assert len(problems) == 1
    assert "mismatch" in problems[0]


def test_a_named_binary_missing_from_the_package_is_reported(mod, tmp_path):
    manifest = _manifest()
    problems = mod.sha_problems(manifest, tmp_path)
    # Nothing was written to tmp_path, so every named binary is absent.
    assert len(problems) == 4
    assert all("not in" in p for p in problems)


# --- structure ------------------------------------------------------------


def test_a_missing_tool_entry_is_reported(mod):
    incomplete = _manifest()
    del incomplete["crgx"]
    problems = mod.structure_problems(incomplete)
    assert any("crgx entry is missing" in p for p in problems)


def test_a_missing_schema_version_is_reported(mod):
    incomplete = _manifest()
    del incomplete["schema_version"]
    assert any("schema_version" in p for p in mod.structure_problems(incomplete))


def test_an_empty_version_is_reported(mod):
    incomplete = _manifest()
    incomplete["cargo_chef"]["version"] = "   "
    assert any("cargo_chef.version" in p for p in mod.structure_problems(incomplete))


def test_a_clean_manifest_has_no_structure_problems(mod):
    assert mod.structure_problems(_manifest()) == []


def test_schema_v4_requires_truthful_execution_provenance(mod):
    manifest = _manifest()
    manifest["schema_version"] = 4
    problems = mod.structure_problems(manifest)
    assert any("artifact_provenance" in problem for problem in problems)


def test_schema_v4_rejects_unknown_execution_status(mod):
    manifest = _manifest()
    manifest["schema_version"] = 4
    manifest["artifact_provenance"] = {
        "build": {"status": "cross-built", "runner": "ubuntu-24.04"},
        "execution": {
            "archive": {"status": "probably-ran"},
            "wheel": {"status": "not-executed", "issue": 3071},
        },
    }
    problems = mod.structure_problems(manifest)
    assert any("probably-ran" in problem for problem in problems)


# --- end to end -----------------------------------------------------------


def test_a_clean_manifest_passes(mod, tmp_path):
    assert mod.main([_write(tmp_path, _manifest())]) == 0


def test_the_published_shape_fails(mod, tmp_path):
    leaked = _manifest()
    leaked["crgx"]["source_commit"] = (
        "soldr-toolchain:v0.1.0\nCARGO_CHEF_SOURCE_COMMIT=soldr-toolchain:v0.1.73\n"
    )
    assert mod.main([_write(tmp_path, leaked)]) == 1


def test_unreadable_json_fails(mod, tmp_path):
    path = tmp_path / "manifest.json"
    path.write_text("{not json", encoding="utf-8")
    assert mod.main([str(path)]) == 1


def test_a_missing_file_fails(mod, tmp_path):
    assert mod.main([str(tmp_path / "nope.json")]) == 1


def test_a_json_array_is_rejected(mod, tmp_path):
    path = tmp_path / "manifest.json"
    path.write_text("[]", encoding="utf-8")
    assert mod.main([str(path)]) == 1


# --- debug-symbol sidecars (docs/DEBUG_SIDECARS.md) -----------------------
#
# Windows ships soldr.pdb on every release, recorded under
# soldr.debug_info with a sha256. Nothing re-derived that digest, so
# corrupting the .pdb produced byte-identical validator output -- verified
# against the real published v0.8.29 Windows bundle before this was fixed.


def _complete_package(tmp_path: Path) -> dict:
    """Stage all four binaries and return a manifest whose digests match.

    Everything staged, so the only problem any test below can produce is the
    debug_info one it is actually about. An earlier version filtered problems
    by substring instead, and matched every message -- pytest embeds the test
    name in tmp_path, so a test named ..._pdb_... made `"pdb" in problem`
    true for unrelated failures.
    """
    for name in ("soldr", "soldr-daemon", "crgx", "cargo-chef"):
        (tmp_path / name).write_bytes(b"x")
    digest = hashlib.sha256(b"x").hexdigest()
    manifest = _manifest()
    for tool in ("soldr", "crgx", "cargo_chef"):
        manifest[tool]["sha256"] = digest
    manifest["soldr"]["sidecars"][0]["sha256"] = digest
    return manifest


def _with_debug_info(manifest: dict, name: str, sha: str, fmt: str = "pdb") -> dict:
    manifest["soldr"]["debug_info"] = [{"name": name, "sha256": sha, "format": fmt}]
    return manifest


def test_a_correct_pdb_digest_passes(mod, tmp_path):
    manifest = _complete_package(tmp_path)
    (tmp_path / "soldr.pdb").write_bytes(b"symbols")
    _with_debug_info(manifest, "soldr.pdb", hashlib.sha256(b"symbols").hexdigest())
    assert mod.sha_problems(manifest, tmp_path) == []


def test_a_tampered_pdb_is_caught(mod, tmp_path):
    # The gap this closes: the digest was declared and never checked, so
    # corrupting the real published soldr.pdb produced byte-identical output.
    manifest = _complete_package(tmp_path)
    (tmp_path / "soldr.pdb").write_bytes(b"tampered")
    _with_debug_info(manifest, "soldr.pdb", hashlib.sha256(b"symbols").hexdigest())
    problems = mod.sha_problems(manifest, tmp_path)
    assert len(problems) == 1
    assert "debug_info[0]" in problems[0] and "mismatch" in problems[0]


def test_a_missing_pdb_is_caught(mod, tmp_path):
    manifest = _complete_package(tmp_path)
    _with_debug_info(manifest, "soldr.pdb", hashlib.sha256(b"symbols").hexdigest())
    problems = mod.sha_problems(manifest, tmp_path)
    assert len(problems) == 1
    assert "debug_info[0]" in problems[0] and "not in" in problems[0]


def test_a_dsym_directory_is_not_digest_checked(mod, tmp_path):
    # auto-release hashes a dSYM as a `tar -cf -` stream, which is not
    # reproducible here (member order, mtimes, uid/gid). Re-deriving it would
    # invent failures, so presence is all that is honestly checkable.
    manifest = _complete_package(tmp_path)
    (tmp_path / "soldr.dSYM").mkdir()
    _with_debug_info(manifest, "soldr.dSYM", "c" * 64, fmt="dsym")
    assert mod.sha_problems(manifest, tmp_path) == []


def test_an_empty_debug_info_is_valid(mod, tmp_path):
    # Documented as a valid state for Linux and macOS, not an error.
    manifest = _complete_package(tmp_path)
    manifest["soldr"]["debug_info"] = []
    assert mod.sha_problems(manifest, tmp_path) == []
