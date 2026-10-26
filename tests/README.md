# soldr Python tests

`pytest`-driven tests for the Python surfaces that ship alongside the
Rust crates: the `setup-soldr` GitHub Action helpers, the exporter, and
the CI benchmark report. Rust crate tests live under `crates/*/tests/`
and `crates/*/src/**_tests.rs`.

## Running

```bash
uv run pytest tests/
```

Heavy Docker checks are opt-in:

```bash
uv run --no-sync pytest tests/test_nextest_archive_cacheability.py --cacheability-integration
```

That check runs `ci/assert_nextest_archive_cacheability.py`, which builds
the full `soldr cargo nextest archive --workspace` path twice in
Linux Docker, forcing the warm pass with `cargo clean` and a soldr cache
daemon restart.

**What it asserts (soldr#2937, phase 5 of soldr#2931).** On the warm rebuild,
*dependency* compilation units must hit the compiler cache. Test-harness
**link** products are reported with their miss counts and are explicitly not
required to be hits.

This replaced the soldr#1391 invariant, which required the full **linked** test
archive to be warm-cacheable at positive hits and zero misses. soldr#2931
inverted that: cache admission follows the stability of an artifact's identity
key relative to its size. A linked test product has the least stable key in
the build, so a warm run may legitimately re-link it; requiring zero misses
made the acceptance fail on ordinary re-links. Test harness compiles are
now admitted to the zccache store, so unchanged harnesses do hit.

The verdict itself lives in `evaluate_warm_result` and is unit-tested without
Docker in `test_nextest_archive_cacheability.py` — a 40-minute acceptance is
the worst possible place to discover a classification bug.

Repository-wide, the same policy is enforced statically by
`.github/scripts/check_cache_ownership.py` against the ownership manifest
`ci/cache-ownership.json`, which classifies every persisted artifact class in
this repo as `cook`, `zccache-unit`, `none`, or a named exception. That guard
runs in the `Lint` job on every PR; see `test_cache_ownership.py`.

The repository's Actions-cache budget (soldr#3047) is declared once, in
`ci.toml` `[cache.family]` (soldr#3618); the manifest's `family_groups` map
those families onto ownership groups and janitor `evict` policies. The budget
is enforced against the live `gh cache list` by `.github/scripts/check_cache_budget.py` in the `Cache Budget`
workflow. `test_cache_budget.py` covers it, including the RED acceptance
fixture `tests/fixtures/actions-cache/listing-2026-09-01.json` — the real
44.23 GiB / 143-entry snapshot that motivated the gate and must fail it.

## Layout

- `test_setup_soldr_action.py` — exercises `resolve_setup.py` end-to-end
  (cache key shapes, target-cache modes, native-cache policy).
- `test_setup_soldr_*.py` — additional unit tests for each
  `.github/actions/setup-soldr/*.py` helper.
- `test_cli.py`, `test_bootstrap_act_image.py` —
  Python-side glue for CLI smoke tests and the `nektos/act` smoke image.
- `fixtures/` — golden files (e.g. exporter expected outputs).
- `conftest.py` — shared fixtures and path bootstrap.
