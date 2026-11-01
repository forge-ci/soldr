"""RED fixtures for the all-triples Dylint driver catalogue guard (soldr#2945).

soldr#2945 made the lint libraries authoritative for the Dylint nightly, which
removed a derived nightly that had no published driver. Nothing then checked
that the channel the libraries *do* declare has drivers published — editing a
`dylints/*/rust-toolchain.toml` to an undriven nightly compiles, tests, lints
and merges, and breaks Dylint on every host silently.
`.github/scripts/check_dylint_driver_assets.py` closes that, and these fixtures
are what prove it is closed: every one of them is a way the check could pass
vacuously or fail unhelpfully.
"""

from __future__ import annotations

import copy
import json
import re
from pathlib import Path

import pytest
from conftest import DYLINT_NIGHTLY, RELEASE_INCLUDED_TRIPLES, load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
CONTRACT = REPO_ROOT / "ci" / "canonical-targets.json"

# The guard imports two siblings by bare name, which only resolves because a
# script's own directory is `sys.path[0]` when CI runs it as a file. Under the
# importlib loader there is no such directory, so they are registered first —
# `catalogue_http` before `toolchain_asset_query`, which imports it.
load_script_module(SCRIPTS / "catalogue_http.py", "catalogue_http")
load_script_module(SCRIPTS / "toolchain_asset_query.py", "toolchain_asset_query")
GUARD = load_script_module(
    SCRIPTS / "check_dylint_driver_assets.py", "check_dylint_driver_assets"
)
RELEASE_COMPLETENESS = load_script_module(
    SCRIPTS / "release_completeness.py", "release_completeness"
)

# Spelled out rather than derived from the contract, so a contract edit that
# silently drops one from the release set fails here instead of quietly
# shrinking the guard's coverage. Shared with the target-removal guard
# (soldr#2469) via conftest so both have to answer for a change.
EXPECTED_TRIPLES = RELEASE_INCLUDED_TRIPLES

VERSION = "6.0.3"
CHANNEL = "nightly-2026-05-28"
DIGEST = "0" * 64


def direct_row(asset: str, **overrides: object) -> dict:
    """One catalogue row in the v2 direct-transport shape."""
    row: dict = {
        "owner": "zackees",
        "repo": "soldr-toolchain",
        "tag": "dylint-drivers",
        "asset": asset,
        "urls": [f"https://zackees.github.io/soldr-toolchain/assets/{asset}"],
        "sha256": DIGEST,
        "size_bytes": 4096,
    }
    row.update(overrides)
    return row


def catalogue(triples: list[str], **overrides: object) -> dict:
    """A catalogue publishing a driver for exactly `triples`."""
    return {
        "schema_version": 2,
        "entries": [
            direct_row(GUARD.driver_asset_name(VERSION, CHANNEL, triple), **overrides)
            for triple in triples
        ],
    }


def contract_payload() -> dict:
    return json.loads(CONTRACT.read_text(encoding="utf-8"))


# ---------------------------------------------------------------------------
# The guard must not derive an empty set (soldr#2013)
# ---------------------------------------------------------------------------


def test_triple_derivation_returns_the_expected_non_empty_set():
    """A guard that derives zero triples passes vacuously, which is the bug.

    `test_rust_validation_command_targets_exist` and soldr#2013 are the two
    previous times a check in this repo scanned nothing and reported clean.
    """
    triples = GUARD.release_included_triples(contract_payload())
    assert triples, "the driver guard would check nothing"
    assert sorted(triples) == sorted(EXPECTED_TRIPLES)
    assert "x86_64-pc-windows-gnu" not in triples, (
        "the documented-exclusion target must not be demanded of the catalogue"
    )


def test_release_selection_matches_the_release_staging_selector():
    """One definition of `release-included`, checked against its owner.

    Release staging selects on `release.status == "included"` in
    `release_completeness.included_triples`. This guard must select the same
    set or it would demand drivers for targets soldr does not ship (or, worse,
    skip ones it does).
    """
    assert GUARD.release_included_triples(contract_payload()) == list(
        RELEASE_COMPLETENESS.included_triples()
    )


# ---------------------------------------------------------------------------
# Malformed `ci/canonical-targets.json` — a parse error, never a traceback
# ---------------------------------------------------------------------------


def test_target_missing_the_triple_field_is_a_named_parse_error():
    payload = contract_payload()
    del payload["targets"][1]["triple"]
    with pytest.raises(GUARD.GuardError) as caught:
        GUARD.release_included_triples(payload)
    assert "targets[1]" in str(caught.value)
    assert "triple" in str(caught.value)


def test_target_with_a_non_string_triple_is_a_named_parse_error():
    payload = contract_payload()
    payload["targets"][3]["triple"] = 42
    with pytest.raises(GUARD.GuardError) as caught:
        GUARD.release_included_triples(payload)
    assert "targets[3]" in str(caught.value)
    assert "42" in str(caught.value)


def test_target_that_is_not_an_object_is_a_named_parse_error():
    payload = contract_payload()
    payload["targets"][0] = "x86_64-pc-windows-msvc"
    with pytest.raises(GUARD.GuardError) as caught:
        GUARD.release_included_triples(payload)
    assert "targets[0]" in str(caught.value)
    assert "str" in str(caught.value)


def test_target_without_a_release_block_is_a_named_parse_error():
    payload = contract_payload()
    del payload["targets"][2]["release"]
    with pytest.raises(GUARD.GuardError) as caught:
        GUARD.release_included_triples(payload)
    assert "targets[2]" in str(caught.value)
    assert "release" in str(caught.value)


def test_target_with_a_non_string_status_is_a_named_parse_error():
    payload = contract_payload()
    payload["targets"][4]["release"]["status"] = None
    with pytest.raises(GUARD.GuardError) as caught:
        GUARD.release_included_triples(payload)
    assert "release.status" in str(caught.value)


def test_contract_without_targets_is_a_parse_error():
    with pytest.raises(GUARD.GuardError, match="targets"):
        GUARD.release_included_triples({"schema_version": 1})


def test_contract_with_no_included_targets_fails_loudly():
    payload = contract_payload()
    for entry in payload["targets"]:
        entry["release"]["status"] = "documented-exclusion"
    with pytest.raises(GUARD.GuardError, match="soldr#2013"):
        GUARD.release_included_triples(payload)


# ---------------------------------------------------------------------------
# The asset identity
# ---------------------------------------------------------------------------


def test_asset_name_matches_the_rust_builder():
    """Byte-for-byte the string `toolchain_packaged::asset_name` produces.

    Copied from that module's own unit test rather than guessed: an asset name
    that is merely plausible would make this guard fail for every triple on a
    perfectly healthy catalogue.
    """
    assert (
        GUARD.driver_asset_name(VERSION, CHANNEL, "x86_64-pc-windows-msvc")
        == "dylint-driver-6.0.3-nightly-2026-05-28-x86_64-pc-windows-msvc.tar.gz"
    )
    assert GUARD.driver_asset_name("v6.0.3", CHANNEL, "aarch64-apple-darwin") == (
        "dylint-driver-6.0.3-nightly-2026-05-28-aarch64-apple-darwin.tar.gz"
    )


# ---------------------------------------------------------------------------
# Catalogue coverage
# ---------------------------------------------------------------------------


def test_happy_path_every_triple_present():
    payload = catalogue(EXPECTED_TRIPLES)
    assert GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES) == []


def test_one_missing_triple_is_reported_by_name():
    payload = catalogue([t for t in EXPECTED_TRIPLES if t != "aarch64-apple-darwin"])
    failures = GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES)
    assert [f.triple for f in failures] == ["aarch64-apple-darwin"]
    assert failures[0].asset_name == GUARD.driver_asset_name(
        VERSION, CHANNEL, "aarch64-apple-darwin"
    )
    assert failures[0].reason == "no catalogue row"


def test_several_missing_triples_are_all_reported():
    """Not just the first: a Windows-and-musl gap is one edit, not three PRs."""
    absent = {
        "aarch64-pc-windows-msvc",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    }
    payload = catalogue([t for t in EXPECTED_TRIPLES if t not in absent])
    failures = GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES)
    assert {f.triple for f in failures} == absent


def test_a_driver_for_a_different_nightly_does_not_satisfy_the_check():
    """The soldr#2945 defect itself: drivers exist, just not for this channel."""
    payload = catalogue(EXPECTED_TRIPLES)
    failures = GUARD.missing_drivers(
        payload, VERSION, "nightly-2026-02-28", EXPECTED_TRIPLES
    )
    assert len(failures) == len(EXPECTED_TRIPLES)


def test_a_driver_for_a_different_dylint_version_does_not_satisfy_the_check():
    payload = catalogue(EXPECTED_TRIPLES)
    failures = GUARD.missing_drivers(payload, "6.0.2", CHANNEL, EXPECTED_TRIPLES)
    assert len(failures) == len(EXPECTED_TRIPLES)


# ---------------------------------------------------------------------------
# Transport validation — the Rust runtime's v2 union
# ---------------------------------------------------------------------------


def test_row_declaring_both_transports_fails():
    """`urls` XOR `parts`, per `catalogue_model::entry_from_v2_wire`."""
    payload = catalogue(
        EXPECTED_TRIPLES,
        parts=[
            {
                "number": 1,
                "size_bytes": 4096,
                "sha256": DIGEST,
                "urls": ["https://zackees.github.io/soldr-toolchain/p/1"],
            }
        ],
    )
    failures = GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES)
    assert len(failures) == len(EXPECTED_TRIPLES)
    assert "exactly one transport shape" in failures[0].reason


def test_row_declaring_no_transport_at_all_fails():
    payload = catalogue(EXPECTED_TRIPLES, urls=[])
    failures = GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES)
    assert len(failures) == len(EXPECTED_TRIPLES)
    assert "invalid transport" in failures[0].reason


def test_row_with_an_unknown_transport_key_fails():
    """An unrecognised transport is a miss, not a silent pass."""
    payload = catalogue(EXPECTED_TRIPLES)
    for row in payload["entries"]:
        del row["urls"]
        row["transport"] = "torrent"
    failures = GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES)
    assert len(failures) == len(EXPECTED_TRIPLES)


def test_row_without_a_valid_sha256_fails():
    payload = catalogue(EXPECTED_TRIPLES, sha256="not-a-digest")
    failures = GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES)
    assert len(failures) == len(EXPECTED_TRIPLES)


def test_row_demanding_a_newer_client_capability_fails():
    """`supports_min_client_version`: present, but unusable by this soldr."""
    payload = catalogue(EXPECTED_TRIPLES, min_client_version=99)
    failures = GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES)
    assert len(failures) == len(EXPECTED_TRIPLES)
    assert "capability" in failures[0].reason


def test_duplicate_rows_for_one_asset_fail():
    """`toolchain_packaged::try_binary` hard-errors rather than picking one."""
    payload = catalogue(EXPECTED_TRIPLES)
    payload["entries"].append(copy.deepcopy(payload["entries"][0]))
    failures = GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES)
    assert len(failures) == 1
    assert "2 catalogue rows" in failures[0].reason


def test_legacy_v1_single_url_rows_are_accepted():
    """v1 rows carry one `url` string; `entry_from_v1_wire` lifts it to a list."""
    payload = {
        "entries": [
            direct_row(GUARD.driver_asset_name(VERSION, CHANNEL, triple))
            for triple in EXPECTED_TRIPLES
        ]
    }
    for row in payload["entries"]:
        row["url"] = row.pop("urls")[0]
    assert GUARD.missing_drivers(payload, VERSION, CHANNEL, EXPECTED_TRIPLES) == []


def test_catalogue_without_entries_is_reported_as_malformed_not_missing():
    with pytest.raises(GUARD.MalformedCatalogue):
        GUARD.missing_drivers({"schema_version": 2}, VERSION, CHANNEL, ["x"])


# ---------------------------------------------------------------------------
# The two repo-side pins
# ---------------------------------------------------------------------------


def test_the_nightly_comes_from_the_lint_libraries():
    manifests = GUARD.dylint_library_manifests(REPO_ROOT)
    assert len(manifests) >= 6, f"expected the six dylints, got {manifests}"
    channel = GUARD.library_nightly(
        {
            path.relative_to(REPO_ROOT).as_posix(): path.read_text(encoding="utf-8")
            for path in manifests
        }
    )
    # `DYLINT_NIGHTLY` is the fully-qualified rustup key the CI policy uses;
    # the driver asset is keyed on its dated prefix.
    assert channel == GUARD.canonical_channel(DYLINT_NIGHTLY)


def test_library_nightly_rejects_an_empty_library_set():
    with pytest.raises(GUARD.GuardError, match="soldr#2013"):
        GUARD.library_nightly({})


def test_library_nightly_rejects_disagreeing_pins():
    with pytest.raises(GUARD.GuardError, match="disagree") as caught:
        GUARD.library_nightly(
            {
                "dylints/a/rust-toolchain.toml": '[toolchain]\nchannel = "nightly-2026-05-28"\n',
                "dylints/b/rust-toolchain.toml": '[toolchain]\nchannel = "nightly-2026-04-16"\n',
            }
        )
    assert "dylints/b/rust-toolchain.toml" in str(caught.value)


def test_library_nightly_rejects_a_channel_that_is_not_a_dated_nightly():
    with pytest.raises(GUARD.GuardError, match="dated"):
        GUARD.library_nightly(
            {"dylints/a/rust-toolchain.toml": '[toolchain]\nchannel = "1.95.0"\n'}
        )


def test_library_nightly_rejects_a_manifest_with_no_channel():
    with pytest.raises(GUARD.GuardError, match=r"no \[toolchain\]\.channel"):
        GUARD.library_nightly({"dylints/a/rust-toolchain.toml": "[toolchain]\n"})


def test_canonical_channel_drops_only_a_nightly_host_suffix():
    assert GUARD.canonical_channel(DYLINT_NIGHTLY) == "nightly-2026-08-15"
    assert GUARD.canonical_channel("nightly-2026-05-28") == "nightly-2026-05-28"
    assert GUARD.canonical_channel("1.95.0") == "1.95.0"


def test_the_dylint_version_comes_from_known_tools():
    source = (REPO_ROOT / GUARD.KNOWN_TOOLS_RELATIVE).read_text(encoding="utf-8")
    version = GUARD.pinned_dylint_version(source)
    assert re.fullmatch(r"\d+\.\d+\.\d+", version), version


def tool_spec(crate_name: str, version: str | None) -> str:
    """One `ToolSpec { .. }` literal in `known_tools.rs` layout."""
    pin = f'Some("{version}")' if version else "None"
    return (
        "    ToolSpec {\n"
        f'        crate_name: "{crate_name}",\n'
        f"        pinned_version: {pin},\n"
        "    },\n"
    )


def test_pinned_dylint_version_rejects_disagreement_between_the_two_entries():
    source = tool_spec("cargo-dylint", "6.0.3") + tool_spec("dylint-link", "6.0.2")
    with pytest.raises(GUARD.GuardError, match="conflicting Dylint versions"):
        GUARD.pinned_dylint_version(source)


def test_an_unpinned_entry_does_not_borrow_the_next_entrys_version():
    """The block regex is bounded to its own literal, not to the next pin.

    An unbounded `.*?pinned_version:` would read dylint-link's 6.0.3 here and
    report a version cargo-dylint does not actually pin.
    """
    source = tool_spec("cargo-dylint", None) + tool_spec("dylint-link", "6.0.3")
    with pytest.raises(GUARD.GuardError, match="cargo-dylint has no"):
        GUARD.pinned_dylint_version(source)


def test_pinned_dylint_version_reports_a_missing_entry():
    with pytest.raises(GUARD.GuardError, match="no ToolSpec entry for cargo-dylint"):
        GUARD.pinned_dylint_version("// no ToolSpec table here\n")


# ---------------------------------------------------------------------------
# End to end through `main()`
# ---------------------------------------------------------------------------


def test_main_skips_when_the_catalogue_cannot_be_resolved(monkeypatch, capsys):
    """A Pages blip must not fail every PR."""

    def unreachable(_origin: str):
        raise GUARD.CatalogueUnavailable("network error")

    monkeypatch.setattr(GUARD, "load_catalogue", unreachable)
    assert GUARD.main([]) == 0
    assert "skipped" in capsys.readouterr().out


def test_main_skips_when_the_catalogue_is_malformed(monkeypatch, capsys):
    monkeypatch.setattr(
        GUARD,
        "load_catalogue",
        lambda _origin: GUARD.LoadedCatalogue("https://example.test/c", {}),
    )
    assert GUARD.main([]) == 0
    assert "skipped" in capsys.readouterr().out


def test_main_passes_when_every_triple_has_a_driver(monkeypatch, capsys):
    version = GUARD.pinned_dylint_version(
        (REPO_ROOT / GUARD.KNOWN_TOOLS_RELATIVE).read_text(encoding="utf-8")
    )
    channel = GUARD.canonical_channel(DYLINT_NIGHTLY)
    payload = {
        "schema_version": 2,
        "entries": [
            direct_row(GUARD.driver_asset_name(version, channel, triple))
            for triple in EXPECTED_TRIPLES
        ],
    }
    monkeypatch.setattr(
        GUARD,
        "load_catalogue",
        lambda _origin: GUARD.LoadedCatalogue("https://example.test/c", payload),
    )
    assert GUARD.main([]) == 0
    assert "all 8 release-included triples" in capsys.readouterr().out


def test_main_failure_names_every_gap_and_where_each_pin_came_from(monkeypatch, capsys):
    """The message must be actionable without opening the script."""
    version = GUARD.pinned_dylint_version(
        (REPO_ROOT / GUARD.KNOWN_TOOLS_RELATIVE).read_text(encoding="utf-8")
    )
    channel = GUARD.canonical_channel(DYLINT_NIGHTLY)
    absent = {"aarch64-pc-windows-msvc", "aarch64-unknown-linux-musl"}
    payload = {
        "schema_version": 2,
        "entries": [
            direct_row(GUARD.driver_asset_name(version, channel, triple))
            for triple in EXPECTED_TRIPLES
            if triple not in absent
        ],
    }
    monkeypatch.setattr(
        GUARD,
        "load_catalogue",
        lambda _origin: GUARD.LoadedCatalogue("https://example.test/c", payload),
    )

    assert GUARD.main([]) == 1
    out = capsys.readouterr().out
    for triple in absent:
        assert triple in out
        assert GUARD.driver_asset_name(version, channel, triple) in out
    assert channel in out
    assert version in out
    assert "dylints/ban_raw_env_flag/rust-toolchain.toml" in out
    assert GUARD.KNOWN_TOOLS_RELATIVE in out
    assert GUARD.CONTRACT_RELATIVE in out
    assert "soldr#2945" in out
