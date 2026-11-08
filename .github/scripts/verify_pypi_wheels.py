#!/usr/bin/env python3
"""Wait for a release's full wheel set to become visible on PyPI.

The PyPI half of the release completeness gate, extracted from
auto-release.yml (soldr#2469 step 2.2). PyPI's JSON index is
read-through-cached and lags an upload by seconds to minutes, so npm must not
publish until the wheels are actually resolvable -- otherwise a user's first
`pip install soldr==X.Y.Z` can miss its platform wheel and a resolver caches
the "no compatible wheel" result.

Two things change in the move out of YAML.

**The expected set comes from the contract, not a literal.** The inline block
carried `expected=8` with a comment explaining which matrix rows it counted.
Step 2.1's whole point is that the target contract is the single source, so
this asks `release_completeness` instead; adding or removing a target updates
the gate for free.

**Filenames are checked, not just the count.** The inline block compared
`len(urls)` against 8, so eight *wrong* wheels satisfied it. That is the
0.9.0 failure class -- PR #2455 shrank the matrix, the canonical targets, the
npm selector, and the tests together, and nothing noticed five targets
vanishing because every consumer agreed with the smaller number. Comparing
names against the contract is what makes the gate independent of the thing it
is checking.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable, cast

PYPI_JSON = "https://pypi.org/pypi/soldr/{version}/json"


def load_release_completeness() -> Any:
    """Load the sibling helper in direct-exec and file-loaded test modes."""
    path = Path(__file__).with_name("release_completeness.py")
    spec = importlib.util.spec_from_file_location("release_completeness", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load release completeness helper: {path}")
    module = importlib.util.module_from_spec(spec)
    # Registered before exec so the helper's @dataclass classes can resolve
    # their own module namespace.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return cast(Any, module)


RELEASE_COMPLETENESS = load_release_completeness()


def expected_wheels(version: str) -> list[str]:
    """Contract-derived wheel filenames for `version` (with or without `v`)."""
    tag = version if version.startswith("v") else f"v{version}"
    return RELEASE_COMPLETENESS.expected_pypi_files(
        tag, RELEASE_COMPLETENESS.included_triples()
    )


def missing_wheels(version: str, published: list[str]) -> list[str]:
    """Contract wheels absent from `published`, in contract order.

    Pure so the gate's decision can be driven from fixtures rather than from
    a live PyPI response.
    """
    present = set(published)
    return [name for name in expected_wheels(version) if name not in present]


def published_filenames(payload: object) -> list[str]:
    """Filenames from a decoded PyPI project-version JSON payload."""
    if not isinstance(payload, dict):
        return []
    urls = payload.get("urls")
    if not isinstance(urls, list):
        return []
    return [
        entry["filename"]
        for entry in urls
        if isinstance(entry, dict) and isinstance(entry.get("filename"), str)
    ]


def fetch_published(version: str) -> list[str]:
    """Read the version's file list; an unreachable index reads as empty.

    A 404 is the ordinary state while PyPI's cache catches up, so it must be
    a retry rather than a failure -- the caller's deadline decides when to
    give up.
    """
    url = PYPI_JSON.format(version=version)
    try:
        with urllib.request.urlopen(url, timeout=30) as response:
            return published_filenames(json.load(response))
    except (urllib.error.URLError, json.JSONDecodeError, TimeoutError, OSError):
        return []


def wait_for_wheels(
    version: str,
    *,
    max_attempts: int,
    deadline_seconds: float,
    poll_seconds: float,
    fetch: Callable[[str], list[str]] = fetch_published,
    sleep: Callable[[float], None] = time.sleep,
    now: Callable[[], float] = time.monotonic,
    log: Callable[[str], None] = print,
) -> list[str]:
    """Poll until every contract wheel is visible; return what is still missing.

    An empty list means complete. Both bounds are kept from the inline block:
    an attempt cap and a wall-clock deadline, so neither a fast-failing index
    nor a slow-but-responsive one can spin past the job's own timeout.
    """
    normalized = version.lstrip("v")
    expected = len(expected_wheels(version))
    missing = expected_wheels(version)
    for attempt in range(1, max_attempts + 1):
        published = fetch(normalized)
        missing = missing_wheels(version, published)
        log(
            f"PyPI reports {expected - len(missing)}/{expected} expected files "
            f"for soldr=={normalized} (attempt {attempt}/{max_attempts})"
        )
        if not missing:
            return []
        if attempt >= max_attempts or now() >= deadline_seconds:
            return missing
        sleep(poll_seconds)
    return missing


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="release version, e.g. v0.9.1")
    parser.add_argument("--max-attempts", type=int, default=60)
    parser.add_argument("--timeout-seconds", type=float, default=1800.0)
    parser.add_argument("--poll-seconds", type=float, default=15.0)
    args = parser.parse_args(argv)

    missing = wait_for_wheels(
        args.version,
        max_attempts=args.max_attempts,
        deadline_seconds=time.monotonic() + args.timeout_seconds,
        poll_seconds=args.poll_seconds,
    )
    if missing:
        print(
            f"timed out waiting for {len(missing)} wheel(s) for "
            f"soldr=={args.version.lstrip('v')}:",
            file=sys.stderr,
        )
        for name in missing:
            print(f"  missing: {name}", file=sys.stderr)
        return 1
    print(
        f"All {len(expected_wheels(args.version))} expected wheels visible on "
        f"PyPI for soldr=={args.version.lstrip('v')}; verification complete."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
