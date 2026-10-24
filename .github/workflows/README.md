# CI/CD Workflows

GitHub Actions workflow definitions.

- **ci.yml** - The only pull-request workflow entry point. It owns the
  required `Lint` context (including a cheap Markdown-only path), calls
  `ci-pre.yml` first, and makes path-selected calls to the setup-soldr,
  cook-size, and labeled macOS Recovery workflows.
- **ci-pre.yml** - "CI Pre" (zackees/ci.yml#6): the Actions-cache janitor and
  the budget verdict. Reusable only; stdlib `python3`, no installs, and no
  `ci.yml` job waits for it. `cache-budget.yml` calls it for non-main pushes,
  the schedule and dispatch, and deletes a closed PR's caches on
  `pull_request: closed`. See docs/CI_CACHE.md.
- **_build-and-test.yml** - Reusable per-platform workspace build and test template.
- **_bootstrap-e2e.yml** - Reusable bootstrap test that builds soldr, then uses that binary to build a third-party fixture.
- **setup-soldr-action.yml** - Reusable dogfood smoke test for setup-soldr;
  it retains push-to-main and manual operation while ci.yml invokes it for its
  former PR path set.

Normal build/test workflows use `zackees/setup-soldr` for Rust build acceleration, excluding `auto-release.yml`. Jobs that build soldr before running soldr self-tests or bootstrap tests stop the setup-soldr builder daemon before the test phase, run the test phase with a fresh `SOLDR_CACHE_DIR` / `ZCCACHE_CACHE_DIR`, and request `SOLDR_CACHE_LIFECYCLE=command` for the isolated test cache when supported by soldr.

Only `ci.yml` may declare `pull_request` or `pull_request_target`, except a
`pull_request` trigger limited to `types: [closed]`, which runs only after a PR
closes and so can never gate a merge (cache-budget.yml's cleanup). The
structural guard `.github/scripts/check_pr_workflow_triggers.py` scans both
`*.yml` and `*.yaml`; reusable `workflow_call` files are allowed. Docs-only
PRs do trigger canonical CI so the required `Lint` context reports, but its
path selection avoids the expensive platform fan-out.

`macos-recovery-replay.yml`, `setup-soldr-action.yml`, and
`cook-size-gate.yml` retain schedule, main-push, or manual operational surfaces
as applicable and are called from ci.yml for PRs.
`build-all-from-linux.yml` is manual-only because ci.yml already covers its
canonical target validation; `lint-docs-shim.yml` was removed when ci.yml took
over Markdown-only `Lint` reporting.

Exceptions:

- **auto-release.yml** remains conservative and keeps its existing release artifact build path.
- **parent-cache-bench.yml**, **perf-cold-warm.yml**, **perf-matrix.yml**, **cache-delta-experiment.yml** intentionally compare cache strategies or preserve experiment topology, so setup-soldr is not forced onto the control rows. The third-party comparison surface (`cache-benchmark.yml` and friends) lives in `zackees/setup-soldr` now — see soldr#674.
