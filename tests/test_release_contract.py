"""Pure fixtures grant no GitHub, issue, or native execution authority."""

import copy
import importlib.util
import json
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]
SPEC = importlib.util.spec_from_file_location(
    "release_contract", ROOT / "ci/release_contract.py"
)
rc = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(rc)


@pytest.fixture
def fixture():
    contract = json.loads((ROOT / "ci/canonical-targets.json").read_text())
    targets = sorted(
        row["triple"]
        for row in contract["targets"]
        if row["release"]["status"] == "included"
    )
    value = {
        "schema_version": 1,
        "repository": "zackees/soldr",
        "issue": 3343,
        "attempt_id": "12345678-1234-4234-8234-123456789abc",
        "candidate_sha": "a" * 40,
        "version": "0.9.27",
        "tag": "v0.9.27",
        "mode": "full",
        "targets": targets,
        "destinations": ["github", "pypi", "npm"],
        "partial_reason": "",
        "expected_artifacts": rc._assets("v0.9.27", targets, ["github", "pypi", "npm"]),
        "contract_sha256": rc.canonical_digest(contract),
    }
    files = [
        {"name": x, "size": 12, "sha256": "b" * 64} for x in value["expected_artifacts"]
    ]
    required = sorted(
        {row["ci"]["run_job"] for row in contract["targets"]} | {"trusted-extra"}
    )
    proof = {
        "candidate_sha": value["candidate_sha"],
        "contract_sha256": value["contract_sha256"],
        "cells": [{"id": x, "status": "success", "native": True} for x in required],
    }
    return value, contract, files, required, proof


def test_full_contract(fixture):
    value, contract, files, _required, _proof = fixture
    assert len(value["targets"]) == 8
    assert len(value["expected_artifacts"]) == 17
    assert len(contract["targets"]) == 9
    info = rc.freeze_info(value, contract, list(reversed(files)))
    rc.require_frozen_info(info, value, contract, files)
    assert (
        rc.validate_transition("pending", "full-ci", directive=value, contract=contract)
        == "full-ci"
    )


@pytest.mark.parametrize(
    "key,bad",
    [
        ("schema_version", True),
        ("issue", True),
        ("issue", 0),
        ("attempt_id", "not-uuid"),
        ("candidate_sha", "a" * 39),
        ("candidate_sha", "A" * 40),
        ("repository", "../soldr"),
        ("version", "01.2.3"),
        ("tag", "v9.9.9"),
        ("targets", []),
        ("targets", ["foreign"]),
        ("destinations", []),
        ("destinations", ["crates.io"]),
        ("mode", "other"),
        ("partial_reason", "unexpected"),
        ("contract_sha256", "c" * 64),
        ("expected_artifacts", ["../escape"]),
    ],
)
def test_invalid_directives(fixture, key, bad):
    value, contract, *_ = fixture
    value[key] = bad
    with pytest.raises(rc.ContractError):
        rc.validate_directive(value, contract)


@pytest.mark.parametrize("key", ["targets", "destinations"])
def test_duplicates(fixture, key):
    value, contract, *_ = fixture
    value[key].append(value[key][0])
    with pytest.raises(rc.ContractError):
        rc.validate_directive(value, contract)


def test_unknown_field(fixture):
    value, contract, *_ = fixture
    value["authority"] = True
    with pytest.raises(rc.ContractError):
        rc.validate_directive(value, contract)


def test_partial_retains_all_native_cells(fixture):
    value, contract, _, required, proof = fixture
    value.update(
        mode="partial",
        partial_reason="Linux-only recovery",
        targets=["x86_64-unknown-linux-gnu"],
        destinations=["github"],
    )
    value["expected_artifacts"] = rc._assets(
        value["tag"], value["targets"], value["destinations"]
    )
    rc.validate_directive(value, contract)
    rc.validate_native_proof(proof, value, contract, required)
    proof["cells"] = proof["cells"][:1]
    with pytest.raises(rc.ContractError):
        rc.validate_native_proof(proof, value, contract, required)
    value["partial_reason"] = " "
    with pytest.raises(rc.ContractError):
        rc.validate_directive(value, contract)


@pytest.mark.parametrize(
    "mutation",
    [
        "foreign",
        "stale",
        "missing",
        "duplicate",
        "skipped",
        "advisory",
        "failed",
        "notnative",
        "boolnative",
        "omitgnu",
    ],
)
def test_native_blockers(fixture, mutation):
    value, contract, _, required, proof = fixture
    if mutation == "foreign":
        proof["candidate_sha"] = "c" * 40
    elif mutation == "stale":
        proof["contract_sha256"] = "c" * 64
    elif mutation == "missing":
        proof["cells"].pop()
    elif mutation == "duplicate":
        proof["cells"].append(copy.deepcopy(proof["cells"][0]))
    elif mutation in ("skipped", "advisory", "failed"):
        proof["cells"][0]["status"] = mutation
    elif mutation == "notnative":
        proof["cells"][0]["native"] = False
    elif mutation == "boolnative":
        proof["cells"][0]["native"] = 1
    else:
        required.remove(
            next(
                row["ci"]["run_job"]
                for row in contract["targets"]
                if row["triple"] == "x86_64-pc-windows-gnu"
            )
        )
    with pytest.raises(rc.ContractError):
        rc.validate_native_proof(proof, value, contract, required)


@pytest.mark.parametrize(
    "mutation", ["retarget", "scope", "hash", "size", "unsafe", "duplicate", "boolsize"]
)
def test_freeze_refuses_changes(fixture, mutation):
    value, contract, files, *_ = fixture
    info = rc.freeze_info(value, contract, files)
    if mutation == "retarget":
        value["candidate_sha"] = "c" * 40
    elif mutation == "scope":
        value["destinations"].pop()
    elif mutation == "hash":
        files[0]["sha256"] = "c" * 64
    elif mutation == "size":
        files[0]["size"] += 1
    elif mutation == "unsafe":
        files[0]["name"] = "../escape"
    elif mutation == "duplicate":
        files.append(files[0])
    else:
        files[0]["size"] = True
    with pytest.raises(rc.ContractError):
        rc.require_frozen_info(info, value, contract, files)


def test_no_publisher_without_proof(fixture):
    value, contract, files, required, proof = fixture
    info = rc.freeze_info(value, contract, files)
    with pytest.raises(rc.ContractError):
        rc.validate_transition(
            "artifacts-ready",
            "publishing",
            directive=value,
            contract=contract,
            info=info,
            files=files,
            required_cells=required,
        )
    with pytest.raises(rc.ContractError):
        rc.validate_transition(
            "pending", "publishing", directive=value, contract=contract
        )
    with pytest.raises(rc.ContractError):
        rc.validate_transition(
            "publishing",
            "partial-complete",
            directive=value,
            contract=contract,
            info=info,
            files=files,
            proof=proof,
            required_cells=required,
            outcomes={"github": "verified"},
        )


def test_crate_skip_explicit():
    assert (
        rc.crate_applicability([{"package": {"publish": False}}])["applicable"] is False
    )
    assert (
        rc.crate_applicability([{"package": {"name": "publishable"}}])["applicable"]
        is True
    )
    with pytest.raises(rc.ContractError):
        rc.crate_applicability([])


def test_frozen_bool_schema_is_not_integer(fixture):
    value, contract, files, *_ = fixture
    info = rc.freeze_info(value, contract, files)
    info["schema_version"] = True
    with pytest.raises(rc.ContractError):
        rc.require_frozen_info(info, value, contract, files)
    bad = copy.deepcopy(value)
    bad["issue"] = True
    with pytest.raises(rc.ContractError):
        rc.require_same_directive({"issue": 1}, {"issue": True})


def test_partial_completion_and_destination_failure(fixture):
    value, contract, _, required, proof = fixture
    value.update(
        mode="partial",
        partial_reason="explicit recovery",
        destinations=["github", "pypi"],
    )
    value["expected_artifacts"] = rc._assets(
        value["tag"], value["targets"], value["destinations"]
    )
    files = [
        {"name": name, "size": 1, "sha256": "b" * 64}
        for name in value["expected_artifacts"]
    ]
    info = rc.freeze_info(value, contract, files)
    args = {
        "directive": value,
        "contract": contract,
        "proof": proof,
        "required_cells": required,
        "info": info,
        "files": files,
    }
    assert (
        rc.validate_transition(
            "publishing",
            "partial-complete",
            outcomes={"github": "verified", "pypi": "verified"},
            **args,
        )
        == "partial-complete"
    )
    with pytest.raises(rc.ContractError):
        rc.validate_transition(
            "publishing",
            "complete",
            outcomes={"github": "verified", "pypi": "verified"},
            **args,
        )
    with pytest.raises(rc.ContractError):
        rc.validate_transition(
            "publishing",
            "partial-complete",
            outcomes={"github": "verified", "pypi": "failed"},
            **args,
        )
    with pytest.raises(rc.ContractError):
        rc.validate_transition("partial-complete", "publishing", **args)


@pytest.mark.parametrize("mode", ["full", "partial"])
@pytest.mark.parametrize(
    "before,after",
    [
        ("full-ci", "artifacts-ready"),
        ("artifacts-ready", "publishing"),
        ("publishing", "complete"),
        ("publishing", "partial-complete"),
    ],
)
def test_npm_unfrozen_blocks_publication(fixture, mode, before, after):
    value, contract, files, required, proof = fixture
    if mode == "partial":
        value.update(
            mode="partial",
            partial_reason="npm recovery",
            destinations=["npm"],
            expected_artifacts=[],
        )
        files = []
    rc.validate_directive(value, contract)
    info = rc.freeze_info(value, contract, files)
    with pytest.raises(rc.ContractError, match="npm package byte identity not frozen"):
        rc.validate_transition(
            before,
            after,
            directive=value,
            contract=contract,
            proof=proof,
            required_cells=required,
            info=info,
            files=files,
            outcomes=dict.fromkeys(value["destinations"], "verified"),
        )


def npm_fixture(version="0.9.27"):
    import io
    import tarfile

    source = {"name": "@zackees/soldr", "version": version, "files": ["bin/soldr.js"]}
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for name, data in [
            ("package/package.json", json.dumps(source).encode()),
            ("package/bin/soldr.js", b'console.log("soldr")'),
        ]:
            entry = tarfile.TarInfo(name)
            entry.size = len(data)
            archive.addfile(entry, io.BytesIO(data))
    return {"package_json": source, "archive": output.getvalue()}


@pytest.mark.parametrize("mode", ["full", "partial"])
def test_actual_npm_bytes_freeze(fixture, mode):
    value, contract, files, required, proof = fixture
    if mode == "partial":
        value.update(
            mode="partial",
            partial_reason="npm retry",
            destinations=["npm"],
            expected_artifacts=[],
        )
        files = []
    package = npm_fixture()
    info = rc.freeze_info(value, contract, files, npm_package=package)
    assert info["npm_package"]["filename"] == "zackees-soldr-0.9.27.tgz"
    assert info["npm_package"]["size"] == len(package["archive"])
    assert info["npm_package"]["integrity"].startswith("sha512-")
    for before, after in [
        ("full-ci", "artifacts-ready"),
        ("artifacts-ready", "publishing"),
        ("publishing", "complete" if mode == "full" else "partial-complete"),
    ]:
        assert (
            rc.validate_transition(
                before,
                after,
                directive=value,
                contract=contract,
                files=files,
                info=info,
                proof=proof,
                required_cells=required,
                npm_package=package,
                outcomes=dict.fromkeys(value["destinations"], "verified"),
            )
            == after
        )
    package["archive"] += b"changed"
    with pytest.raises(rc.ContractError):
        rc.require_frozen_info(info, value, contract, files, npm_package=package)


@pytest.mark.parametrize(
    "mutation",
    [
        "source-version",
        "source-name",
        "packed-version",
        "unsafe",
        "duplicate",
        "link",
        "missing",
        "size",
        "notbytes",
    ],
)
def test_npm_pack_refusals(mutation):
    import io
    import tarfile

    package = npm_fixture()
    if mutation == "source-version":
        package["package_json"]["version"] = "9.9.9"
    elif mutation == "source-name":
        package["package_json"]["name"] = "../escape"
    elif mutation == "notbytes":
        package["archive"] = "notbytes"
    else:
        out = io.BytesIO()
        with tarfile.open(fileobj=out, mode="w:gz") as archive:
            metadata = dict(package["package_json"])
            if mutation == "packed-version":
                metadata["version"] = "9.9.9"
            data = json.dumps(metadata).encode()
            entry = tarfile.TarInfo("package/package.json")
            entry.size = len(data)
            if mutation == "unsafe":
                entry.name = "package/../package.json"
            if mutation == "link":
                entry.type = tarfile.SYMTYPE
                entry.linkname = "/etc/passwd"
                entry.size = 0
            if mutation == "missing":
                entry.name = "package/other.json"
            if mutation == "size":
                entry.size = 16_000_001
            if mutation != "size":
                archive.addfile(entry, io.BytesIO(data))
                if mutation == "duplicate":
                    archive.addfile(entry, io.BytesIO(data))
            else:
                # Only the oversized header is needed: reject before any read.
                out.write(entry.tobuf())
        package["archive"] = out.getvalue()
    with pytest.raises(rc.ContractError):
        rc.freeze_npm_package(package, "0.9.27")


def test_npm_identity_matches_real_source_naming():
    source = json.loads((ROOT / "package.json").read_text())
    assert source["name"] == "@zackees/soldr"
    assert source["scripts"]["prepack"] == "node scripts/test-npm-package.js"
    package = npm_fixture(source["version"])
    package["package_json"] = source
    frozen = rc.freeze_npm_package(package, source["version"])
    assert frozen["filename"] == f"zackees-soldr-{source['version']}.tgz"


@pytest.mark.parametrize("kind", ["pax", "longname"])
def test_npm_metadata_expansion_bound(monkeypatch, kind):
    import gzip
    import tarfile

    monkeypatch.setattr(rc, "MAX_NPM_EXPANDED_BYTES", 2048, raising=False)
    header = tarfile.TarInfo("metadata")
    header.type = tarfile.XHDTYPE if kind == "pax" else tarfile.GNUTYPE_LONGNAME
    header.size = 4096
    raw = header.tobuf() + b"x" * 4096 + b"\0" * 1024
    package = npm_fixture()
    package["archive"] = gzip.compress(raw)
    with pytest.raises(rc.ContractError, match="npm expanded byte ceiling"):
        rc.freeze_npm_package(package, "0.9.27")
