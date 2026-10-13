#!/usr/bin/env python3
"""Every triple soldr ships must have a published Dylint driver (soldr#2945).

## The failure this prevents

Soldr fetches a **prebuilt** `dylint-driver` and refuses to build one from
source (binary-or-exit, soldr#2432/#2484). The driver asset is keyed on two
pins that live in two different files:

* the **Dylint version**, pinned on the `cargo-dylint` / `dylint-link` entries
  in `crates/soldr-fetch/src/fetch/known_tools.rs`;
* the **nightly**, declared by the lint libraries themselves — every
  `dylints/*/rust-toolchain.toml` `[toolchain].channel`.

soldr#2945 fixed *which* nightly soldr asks for: it used to derive one from the
stable channel, and the derived `nightly-2026-02-28` had no driver published
anywhere while the libraries' `nightly-2026-05-28` had one for all eight
release-included triples. The libraries are now the authority.

That fix makes the libraries authoritative but leaves them **unchecked**.
Editing a `dylints/*/rust-toolchain.toml` to a nightly nobody built a driver
for compiles, tests, lints and merges perfectly happily — and then Dylint is
broken on every host, silently, until somebody actually runs it. The signal
only exists at fetch time, on a machine, after the pin is already on `main`.

## What is checked

For **every** release-included canonical triple in `ci/canonical-targets.json`
(the same `release.status == "included"` selection
`.github/scripts/release_completeness.py::included_triples` uses for release
staging), the published soldr-toolchain catalogue must carry exactly one row
whose asset filename is the exact driver identity

    dylint-driver-<version>-<nightly>-<triple>.tar.gz

built the way `soldr-fetch`'s `toolchain_packaged::asset_name` builds it, and
that row must satisfy the Rust runtime's v2 transport union — exactly one of
`urls` / `parts`, a safe filename, and a valid SHA-256 pin. Checking only the
host triple would let a Windows-only or musl-only gap through, which is exactly
the shape of gap soldr#2945 was about.

## Network policy

An unreachable catalogue is **not** a failure. This guard runs on every PR,
and failing them all on a GitHub Pages blip would train people to ignore it.
A fetched catalogue
that is missing a driver *is* a failure — that is the defect, and it is not
transient.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
from dataclasses import dataclass

# Reuse the catalogue client and the transport validator rather than writing a
# second one (the soldr#2740/#2741 "N implementations of one idea" rule): a
# divergent copy could accept a row the runtime rejects, which would make this
# guard worse than nothing. Resolvable because Python puts a script's own
# directory on sys.path, which is how the sibling scripts here import it too.
from toolchain_asset_query import (
    DEFAULT_ORIGIN,
    NetworkFetchError,
    decode_json,
    fetch_bytes,
    normalize_asset,
)

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]

# Provenance strings. Every failure message names the file that pinned the
# value it is complaining about, so the reader never has to open this script.
CONTRACT_RELATIVE = "ci/canonical-targets.json"
KNOWN_TOOLS_RELATIVE = "crates/soldr-fetch/src/fetch/known_tools.rs"
LIBRARY_GLOB = "dylints/*/rust-toolchain.toml"

# The runtime prefers the multipart-aware v2 document and falls back to v1
# (`catalogue_lookup.rs`: CATALOGUE_V2_DOC_NAME is "tried before v1"). Checking
# them in the same order means this guard reads the document the fetch would.
CATALOGUE_DOCUMENTS = ("catalogue.v2.json", "catalogue.v1.json")

# Highest catalogue transport capability soldr implements — the Rust constant
# is `catalogue_lookup::CATALOGUE_CAPABILITY`. A row demanding more than this
# is unusable by the shipping client even though it is present.
CATALOGUE_CAPABILITY = 2

CHANNEL_PATTERN = re.compile(r'^\s*channel\s*=\s*"([^"]+)"', re.MULTILINE)
DATED_NIGHTLY = re.compile(r"^nightly-\d{4}-\d{2}-\d{2}$")

# The two `known_tools.rs` entries that carry the Dylint release version. The
# driver is keyed on cargo-dylint's version; dylint-link ships from the same
# upstream release, so a disagreement means one of the two is wrong.
DYLINT_CRATES = ("cargo-dylint", "dylint-link")


class GuardError(RuntimeError):
    """An actionable defect in the repo's pins or in the published catalogue."""


class CatalogueUnavailable(RuntimeError):
    """The catalogue could not be resolved. Skipped, never failed."""


class MalformedCatalogue(RuntimeError):
    """The fetched catalogue is unreadable. Skipped, like an unreachable one."""


# ---------------------------------------------------------------------------
# Input 1 — the nightly, from the lint libraries themselves
# ---------------------------------------------------------------------------


def dylint_library_manifests(repo_root: pathlib.Path) -> list[pathlib.Path]:
    """Every first-party lint library's `rust-toolchain.toml`.

    Vendored copies under a crate's own `.cargo/registry` are excluded — they
    are upstream dependencies' files, not pins this repo controls. Same rule as
    `tests/test_dylint_nightly_agreement.py::dylint_toolchain_files`, which
    also covers the acceptance fixture; the fixture is deliberately *not* read
    here because it is not one of the libraries the driver lookup consults.
    """
    return [
        path
        for path in sorted((repo_root / "dylints").glob("*/rust-toolchain.toml"))
        if ".cargo" not in path.parts
    ]


def pinned_channel(text: str) -> str | None:
    """The `[toolchain].channel` value in one `rust-toolchain.toml` body."""
    match = CHANNEL_PATTERN.search(text)
    return match.group(1) if match else None


def canonical_channel(channel: str) -> str:
    """Reduce a channel to the identity the driver asset is keyed on.

    Mirrors `dylint_libraries::canonical_channel` /
    `toolchain_packaged::dated_nightly_prefix`: `nightly-2026-05-28` and
    `nightly-2026-05-28-x86_64-pc-windows-msvc` name the same driver, and
    anything that is not a dated nightly is returned unchanged so the caller
    can reject it by name.
    """
    prefix = channel[:18]
    if len(channel) >= 18 and DATED_NIGHTLY.match(prefix):
        return prefix
    return channel


def library_nightly(manifests: dict[str, str]) -> str:
    """The one dated nightly every lint library declares.

    `manifests` maps a display path to that file's text, so the caller owns all
    the I/O and every disagreement can be reported with the path that caused it.
    """
    if not manifests:
        raise GuardError(
            f"no lint libraries found under {LIBRARY_GLOB}. This guard would "
            "then check nothing and report clean, which is the vacuous-guard "
            "failure soldr#2013 already cost us twice."
        )

    pins: dict[str, str | None] = {
        path: pinned_channel(text) for path, text in sorted(manifests.items())
    }
    unpinned = [path for path, channel in pins.items() if channel is None]
    if unpinned:
        raise GuardError("no [toolchain].channel pinned in: " + ", ".join(unpinned))

    distinct = sorted({channel for channel in pins.values() if channel})
    if len(distinct) != 1:
        detail = "\n".join(f"  {path}: {channel}" for path, channel in pins.items())
        raise GuardError(
            "Dylint nightly pins disagree, so at most one of them can have a "
            f"published driver:\n{detail}"
        )

    channel = canonical_channel(distinct[0])
    if not DATED_NIGHTLY.match(channel):
        raise GuardError(
            f"the lint libraries pin `{distinct[0]}`, which is not a dated "
            "nightly. The Dylint driver catalogue lookup is keyed on "
            "`nightly-YYYY-MM-DD`, so no asset name can be built from it."
        )
    return channel


# ---------------------------------------------------------------------------
# Input 2 — the Dylint version, from the fetch registry
# ---------------------------------------------------------------------------


def tool_spec_body(source: str, crate_name: str) -> str:
    """The body of one `ToolSpec { .. }` literal in `known_tools.rs`.

    Bounded to the literal's own closing brace rather than scanning to the next
    `pinned_version:` anywhere in the file — an entry that lost its pin would
    otherwise silently borrow the next entry's.
    """
    match = re.search(
        rf'crate_name:\s*"{re.escape(crate_name)}",(?P<body>.*?)\n    \}},',
        source,
        re.DOTALL,
    )
    if match is None:
        raise GuardError(
            f"{KNOWN_TOOLS_RELATIVE} has no ToolSpec entry for {crate_name}"
        )
    return match.group("body")


def pinned_dylint_version(source: str) -> str:
    """The exact Dylint release both `known_tools.rs` entries pin."""
    pins: dict[str, str] = {}
    for crate_name in DYLINT_CRATES:
        body = tool_spec_body(source, crate_name)
        match = re.search(r'pinned_version:\s*Some\("([^"]+)"\)', body)
        if match is None:
            raise GuardError(
                f"{KNOWN_TOOLS_RELATIVE}: {crate_name} has no `pinned_version: "
                "Some(..)`. The driver asset name is keyed on that exact "
                "version, so an unpinned entry has no asset to look for."
            )
        pins[crate_name] = match.group(1)

    distinct = sorted(set(pins.values()))
    if len(distinct) != 1:
        detail = ", ".join(f"{crate}={version}" for crate, version in pins.items())
        raise GuardError(
            f"{KNOWN_TOOLS_RELATIVE} pins conflicting Dylint versions "
            f"({detail}); they ship from one upstream release and the driver "
            "is keyed on cargo-dylint's."
        )
    return distinct[0].lstrip("v")


# ---------------------------------------------------------------------------
# Input 3 — the triples soldr actually ships
# ---------------------------------------------------------------------------


def release_included_triples(payload: object) -> list[str]:  # noqa: C901
    """Canonical triples whose `release.status` is `included`.

    Same field and same value as
    `.github/scripts/release_completeness.py::included_triples`, which is what
    release staging selects on; `tests/test_dylint_driver_assets.py` pins the
    two to the same answer so they cannot drift apart silently. The validation
    is the part that is new here: a malformed contract must name the entry it
    could not read rather than surfacing a `KeyError` traceback.
    """
    if not isinstance(payload, dict):
        raise GuardError(f"{CONTRACT_RELATIVE} is not a JSON object")
    targets = payload.get("targets")
    if not isinstance(targets, list) or not targets:
        raise GuardError(f"{CONTRACT_RELATIVE} has no non-empty `targets` array")

    triples: list[str] = []
    for index, entry in enumerate(targets):
        where = f"{CONTRACT_RELATIVE} targets[{index}]"
        if not isinstance(entry, dict):
            raise GuardError(f"{where} is a {type(entry).__name__}, expected an object")
        triple = entry.get("triple")
        if not isinstance(triple, str) or not triple:
            raise GuardError(
                f"{where} has no non-empty string `triple` field (got {triple!r})"
            )
        release = entry.get("release")
        if not isinstance(release, dict):
            raise GuardError(
                f"{where} ({triple}) has no `release` object (got {release!r})"
            )
        status = release.get("status")
        if not isinstance(status, str) or not status:
            raise GuardError(
                f"{where} ({triple}) has no non-empty string `release.status` "
                f"(got {status!r})"
            )
        if status == "included":
            triples.append(triple)

    duplicates = sorted({t for t in triples if triples.count(t) > 1})
    if duplicates:
        raise GuardError(
            f"{CONTRACT_RELATIVE} lists duplicate release-included triples: "
            + ", ".join(duplicates)
        )
    if not triples:
        raise GuardError(
            f"{CONTRACT_RELATIVE} declares no release-included targets, so "
            "this guard would check nothing and report clean. A guard that "
            "scans nothing is the soldr#2013 failure, not a pass."
        )
    return triples


# ---------------------------------------------------------------------------
# The catalogue
# ---------------------------------------------------------------------------


def driver_asset_name(version: str, channel: str, triple: str) -> str:
    """The exact catalogue filename soldr looks up for one triple.

    Byte-for-byte the string `toolchain_packaged::asset_name` builds for
    `cache_name = "dylint-driver"` and `version = "<dylint>-<nightly>"`, e.g.
    `dylint-driver-6.0.3-nightly-2026-05-28-x86_64-pc-windows-msvc.tar.gz`.
    """
    return f"dylint-driver-{version.lstrip('v')}-{channel}-{triple}.tar.gz"


def catalogue_entries(payload: dict) -> list[dict]:
    """The flat `entries` array both catalogue schema versions publish."""
    entries = payload.get("entries")
    if not isinstance(entries, list):
        raise MalformedCatalogue("has no `entries` array")
    return [entry for entry in entries if isinstance(entry, dict)]


def transport_shape(row: dict) -> dict:
    """Map a flat catalogue row onto the asset dict `normalize_asset` checks.

    v2 rows carry the transport union directly (`urls` XOR `parts`). Legacy v1
    rows carry a single `url` string, which `catalogue_model::entry_from_v1_wire`
    lifts into a one-element `urls` list; doing the same here means one union
    rule covers both documents.
    """
    urls = row.get("urls")
    if urls is None and isinstance(row.get("url"), str):
        urls = [row["url"]]
    return {
        "filename": row.get("asset"),
        "urls": urls or [],
        "parts": row.get("parts") or [],
        "sha256": row.get("sha256"),
        "size_bytes": row.get("size_bytes"),
    }


def driver_row_problem(rows: list[dict], asset_name: str) -> str | None:
    """Why this catalogue row cannot serve as the driver, or `None` if it can.

    The transport rule is not invented here: `normalize_asset` is the same
    validator `download_catalogued_asset.py` runs before materializing an
    asset, and it mirrors `catalogue_model::entry_from_v2_wire` — exactly one
    transport field, a filename that is a safe bare name, a lowercase 64-hex
    SHA-256, and contiguous, size-consistent parts when multipart.
    """
    if not rows:
        return "no catalogue row"
    if len(rows) != 1:
        # `toolchain_packaged::try_binary` hard-errors on an ambiguous exact
        # asset rather than picking one, so more than one row is a defect.
        return f"{len(rows)} catalogue rows for one exact asset"
    row = rows[0]
    required = row.get("min_client_version")
    if required is not None and required != CATALOGUE_CAPABILITY:
        return (
            f"row requires client capability {required!r}, but soldr "
            f"implements {CATALOGUE_CAPABILITY}"
        )
    try:
        normalized = normalize_asset({}, transport_shape(row), require_sha256=True)
    except SystemExit as exc:
        return f"invalid transport: {exc}"
    if normalized["filename"] != asset_name:
        return f"row filename is {normalized['filename']!r}"
    return None


@dataclass(frozen=True)
class DriverFailure:
    """One triple whose Dylint driver the catalogue cannot serve."""

    triple: str
    asset_name: str
    reason: str


@dataclass(frozen=True)
class LoadedCatalogue:
    """A fetched catalogue document and the URL it came from."""

    url: str
    payload: dict  # raw JSON object, as decoded from the wire


def missing_drivers(
    payload: dict, version: str, channel: str, triples: list[str]
) -> list[DriverFailure]:
    """A `DriverFailure` for every triple the catalogue fails."""
    rows_by_asset: dict[str, list[dict]] = {}
    for row in catalogue_entries(payload):
        asset = row.get("asset")
        if isinstance(asset, str):
            rows_by_asset.setdefault(asset, []).append(row)

    failures: list[DriverFailure] = []
    for triple in triples:
        asset_name = driver_asset_name(version, channel, triple)
        problem = driver_row_problem(rows_by_asset.get(asset_name, []), asset_name)
        if problem is not None:
            failures.append(DriverFailure(triple, asset_name, problem))
    return failures


def load_catalogue(origin: str) -> LoadedCatalogue:
    """Fetch the published catalogue, preferring v2 exactly as the runtime does.

    Raises `CatalogueUnavailable` when neither document can be resolved; the
    caller turns that into a skip, never a failure. See the network policy in
    this module's docstring.
    """
    reasons = []
    for name in CATALOGUE_DOCUMENTS:
        url = f"{origin.rstrip('/')}/{name}"
        try:
            payload = decode_json(fetch_bytes(url), url)
        except (OSError, NetworkFetchError) as exc:
            reasons.append(f"{name}: {exc}")
            continue
        except SystemExit as exc:  # decode_json: not UTF-8 JSON
            reasons.append(f"{name}: {exc}")
            continue
        if not isinstance(payload, dict):
            reasons.append(f"{name}: not a JSON object")
            continue
        return LoadedCatalogue(url, payload)
    raise CatalogueUnavailable("; ".join(reasons))


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------


def failure_report(
    failures: list[DriverFailure],
    *,
    version: str,
    channel: str,
    catalogue_url: str,
    library_paths: list[str],
    total_triples: int,
) -> str:
    """The whole diagnosis, so the reader never has to open this script."""
    rows = "\n".join(
        f"  {failure.triple}\n    expected asset: {failure.asset_name}\n"
        f"    catalogue says: {failure.reason}"
        for failure in failures
    )
    libraries = ", ".join(library_paths)
    return (
        f"error: {len(failures)} of {total_triples} release-included triples "
        f"have no usable Dylint driver in {catalogue_url}.\n\n"
        f"{rows}\n\n"
        f"Inputs and where they came from:\n"
        f"  nightly  {channel}\n"
        f"    pinned by {libraries}\n"
        f"  Dylint   {version}\n"
        f"    pinned by {KNOWN_TOOLS_RELATIVE} "
        f"({' / '.join(DYLINT_CRATES)} pinned_version)\n"
        f"  triples  {CONTRACT_RELATIVE} entries with "
        f'release.status == "included"\n\n'
        "soldr fetches a prebuilt driver and refuses to build one from source "
        "(binary-or-exit,\nsoldr#2432/#2484), so a triple listed above has "
        "Dylint broken outright on that host.\nEither re-pin every "
        f"`{LIBRARY_GLOB}` to a nightly the catalogue publishes a "
        f"v{version}\ndriver for, or publish the missing assets on "
        "zackees/soldr-toolchain first (soldr#2945)."
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=pathlib.Path, default=REPO_ROOT)
    parser.add_argument("--origin", default=DEFAULT_ORIGIN)
    args = parser.parse_args(argv)

    repo_root: pathlib.Path = args.repo_root
    try:
        manifests = dylint_library_manifests(repo_root)
        channel = library_nightly(
            {
                path.relative_to(repo_root).as_posix(): path.read_text(encoding="utf-8")
                for path in manifests
            }
        )
        version = pinned_dylint_version(
            (repo_root / KNOWN_TOOLS_RELATIVE).read_text(encoding="utf-8")
        )
        triples = release_included_triples(
            json.loads((repo_root / CONTRACT_RELATIVE).read_text(encoding="utf-8"))
        )
    except GuardError as exc:
        print(f"error: {exc}")
        return 1
    except (OSError, json.JSONDecodeError) as exc:
        print(f"error: could not read a Dylint driver pin: {exc}")
        return 1

    try:
        catalogue = load_catalogue(args.origin)
    except CatalogueUnavailable as exc:
        # Not a failure: see the network policy in this module's docstring.
        print(f"check_dylint_driver_assets: skipped, cannot resolve catalogue ({exc})")
        return 0

    try:
        failures = missing_drivers(catalogue.payload, version, channel, triples)
    except MalformedCatalogue as exc:
        # Same reasoning as an unreachable catalogue: a remote document this
        # guard cannot read is not evidence that a driver is missing.
        print(f"check_dylint_driver_assets: skipped, {catalogue.url} {exc}")
        return 0

    if failures:
        print(
            failure_report(
                failures,
                version=version,
                channel=channel,
                catalogue_url=catalogue.url,
                library_paths=[
                    path.relative_to(repo_root).as_posix() for path in manifests
                ],
                total_triples=len(triples),
            )
        )
        return 1

    print(
        f"check_dylint_driver_assets: dylint-driver {version}-{channel} is "
        f"published for all {len(triples)} release-included triples."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
