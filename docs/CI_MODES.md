# CI modes

The `CI` workflow selects one mode for each event. An ordinary maintainer pull request or
push to `main` runs `Lint` and the Linux x64 prescribed host validation
(`soldr ci-test`). The Linux host job is not the whole platform matrix.


A PR author without effective `write`, `maintain`, or `admin` permission on
the base repository receives full tests without a label. Permission is queried
anew on each run; unavailable, malformed, stale, or mismatched responses choose
full. Fork origin, association strings, prior contributions, and bot identity
do not grant trust. Label removal cannot downgrade an external author. The
selector reports `author_permission` and `selector_schema=fleet-ci-mode/v1`,
and executes from the trusted base SHA rather than the PR helper. The shared
selector interface and remaining adoption work are described in
[the fleet contract](FLEET_CI_CONTRACT.md). GitHub maps
maintain to the API's write permission; custom role names alone grant nothing.
See [GitHub's permission API](https://docs.github.com/en/rest/collaborators/collaborators#get-repository-permissions-for-a-user).

External PRs must not merge until `Full coverage` proves every required cell
succeeded on their candidate. Live external-PR proof, stable summary branch
protection, and shared fleet adoption are tracked in
[setup-soldr#523](https://github.com/zackees/setup-soldr/issues/523). External
full runs are reported separately from the trusted routine compute budget.

Add the `ci-test` label to run the extended Linux x64 target E2E cell and
the macOS ARM64 archive replay on a hosted `macos-15` runner. Remove it to
return to minimal coverage on the same PR head SHA. This does not run the
complete platform matrix.

Add the `ci-full` label to a pull request when it changes platform-sensitive
code, cross-target behavior, the toolchain, sysroots, wheels, target execution,
or release packaging. Label add/remove and new commits recompute the mode for
the PR head SHA. `ci-full` takes precedence over the older `fast-build` label
and `ci-test` and path-based Windows/wheel policies. Full mode runs every CI
target in `ci/canonical-targets.json` and the platform smoke jobs. `Full
coverage` fails when a required job is skipped, failed, cancelled, or missing.
It also rejects a supported target with no execution job. Full mode replays
the macOS x64 archive on hosted `macos-15-intel` and the ARM64 archive on
hosted `macos-15`. These runner allocations occur for explicit labels, external-author full
validation, or exact-SHA release validation. Trusted unlabeled PRs and main
runs remain Mac-free.

For a release candidate, dispatch `CI` with `candidate_sha` set to its full
40-character commit SHA. The mode job checks out and verifies that SHA, then
passes it to every checkout and cross-build caller. A release process must wait
for a successful `Full coverage` result on that same SHA before publication.

The GNU/Linux wheel verification cells canary the release wheel helper with
`soldr wheel --release --target <triple> --locked --strip --target-dir target
--out dist`. Both x64 host and ARM64 cross wheels must pass the manylinux tag
and GLIBC 2.17 byte checks; the host wheel also passes the release installation
smoke test. This validates the release build path without publishing. Wheel
provisioning belongs to Soldr; the helper does not source-build Maturin or use
setup-soldr's `target-wheel-hook` for GNU/Linux. The project's `auditwheel =
"check"` policy remains authoritative, and these steps build wheels only, not
sdists. Native ARM64 musl retains its explicit native builder: the catalogue
compiler is x86_64-hosted and cannot execute on that runner (see
[soldr#3435](https://github.com/zackees/soldr/issues/3435)).

The normal `Lint` and `Linux x64` status names remain stable for branch
protection. A full run additionally reports `Full coverage`. The runner-minute
budget in [soldr#3344](https://github.com/zackees/soldr/issues/3344) is for all
workflows caused by the event; it needs Actions job-duration measurements and
cannot be inferred from this workflow's job count.

Manual npm recovery is partial publication and requires `candidate_sha` and
`full_ci_run_id` too. A completed main-workflow full run must prove that exact
candidate, and the immutable recovery tag must resolve to the tested SHA before
npm publication. A missing, skipped, failed, or stale full gate refuses recovery.

The cost collector groups direct PR/main runs by SHA and explicit event-wave
anchor, then follows versioned `ci-cost-parent-v1=<run-id>` receipts in chained
workflow run titles. These parent IDs come from GitHub's `workflow_run` payload;
matching SHAs or timestamps alone do not establish attribution. Chained jobs
can finish much later or run a newer default-branch SHA and still belong to the
original event. Missing receipts or pending selected runs refuse a cost report,
so older runs without causal receipts cannot silently count as a complete
whole-event measurement. Direct and chained jobs include every attempt; skipped
jobs consume zero runner time. Run the collector after the expected workflow
graph has completed and review its included/excluded run inventory.

The collector also checks API `total_count` against its paginated inventory and
refuses searches at [GitHub's 1,000-result limit](https://docs.github.com/en/rest/actions/workflow-runs#list-workflow-runs-for-a-repository).
A truncated or changing inventory cannot support a budget claim.

Setup Soldr Action smoke and Cook Size Gate are required full-mode jobs. They
check out the same candidate SHA as the target matrix and no longer run on
ordinary PR or main events. Explicit manual smoke dispatches remain available;
full validation, rather than each main push, seeds their base-branch caches.


The `Full coverage` job emits a `fleet-ci-coverage/v1` JSON artifact for each
run attempt, with the candidate SHA, selected SHA, manifest SHA-256, required
job IDs, outcomes, and failures. Wrong or missing candidate identity refuses
coverage even if every supplied job state says success. Failed coverage reports
remain diagnostic artifacts; they do not authorize publication.

Other fleet repositories can pin and execute the same `ci_full_coverage.py`
helper with `--contract <adapter.json> --expected-sha <candidate>
--selected-sha <selector-output> --report <coverage.json>`, and supply their
GitHub `needs` object in `CI_NEEDS_JSON`. A portable adapter declares
`schema_version: 1`, a nonempty unique `required_jobs` list, and `targets` with
`triple` (an opaque target identity) plus `ci.build_job` and `ci.run_job`. A
declared target without an execution job fails, including documented exceptions.
The adapter replaces Soldr's smoke-job defaults; its complete test inventory
must be reviewed and proven by live runs. Consumer rollout remains under
[soldr#3345](https://github.com/zackees/soldr/issues/3345).

`CI summary` is the stable merge context and runs with `always()` after the
selector and required cells. It checks `ci/summary-contract.json` for minimal,
documentation-only, and extended-test jobs; full mode additionally checks the
canonical target contract and `Full coverage`. Every selected job must finish
successfully on the selected candidate identity. An unresolved/external PR
author cannot use a minimal or test summary, and a dispatch requires full mode.
The summary fetches fresh current PR labels/head and repeats the permission
lookup, including failed-job reruns that reuse earlier selector outputs.
A former head, missing current metadata, or coverage below current labels fails
closed, even when the old jobs passed. Revoked or unresolved
write access refuses a minimal/test merge result; rerun the complete workflow
to schedule full validation. Reports retain both permission observations.
The summary uploads an attempt-specific `fleet-ci-summary/v1` diagnostic report
with both contract digests, identity, permission class, outcomes, and failures.

Both `ci-test` and `ci-full` override documentation-only host/lint skips, so the
extended tier can actually execute its declared extra tests. Minimal docs-only
PRs require the documentation lint and a successful path-selection decision.
Informational reld probes remain advisory and are not declared merge gates.

Branch protection must require `CI summary` only after a successful default-
branch run proves the context exists. The repository currently has no required
checks; shipping the summary alone does not prove protected enforcement or the
fleet's external-author security and live-run acceptance criteria.

Cache-control jobs check out the reviewed PR base revision, rather than the
candidate's helper/configuration. This prevents a same-repository author whose
write access was revoked from executing PR-authored cache-control code with
the janitor's write token. Fork janitor skips and read-only test tokens remain
in force; this does not replace the remaining full external-graph security audit.

Path classification, full-coverage enforcement, and the merge summary execute
helpers and contracts checked out from the reviewed PR base SHA. Candidate
source is checked out separately for compilation and path diffs. Missing trusted
helpers fail; there is no candidate-policy fallback. A changed coverage contract
must be reviewed and deployed into the trusted policy before it becomes the
mandatory support contract.

This protects policy execution within the reviewed workflow. A stable status
name alone does not prove trusted execution when the workflow itself is PR
controlled. Trusted workflow enforcement and live external-author security
proof remain required before claiming protected merge enforcement.

## Local gate: proven lane skips

**The `wine` lane may attest by proof (soldr#3704).** When every changed path
  (vs `merge-base origin/main`) lies in a workspace crate outside the path-dependency
  closure of `ci/wine_lane.py` `CRATES`, the lane prints `wine lane: SKIPPED (soldr#3704)`
  and passes: the Wine-tested binaries are unchanged from the base. Any root file,
  lockfile, toolchain, script, unknown path or unreadable diff runs it in full. `cross`
  is not skipped this way: a cfg-free edit can still call a cfg-gated API.

**The `tests` lane may narrow by proof (soldr#3731).** When every changed path is
  crate source -- under `crates/<known crate>/`, but not that crate's `Cargo.toml` or
  `tests/` -- `ci/workspace_test.py` runs doctests and nextest with `-p` for the
  reverse-dependency closure of the changed crates plus `soldr-cli` (whose guards read
  the whole tree), and prints `tests lane: SELECTED [...] (soldr#3731)` with the skipped
  crates. A skipped crate's test binaries are built only from crates outside the change,
  against the base's `Cargo.lock`, so they are the base's binaries. Anything else --
  a root manifest, `Cargo.lock`, toolchain, `.config/nextest.toml`, `tests/`, a crate's
  `tests/` or manifest, `ci/`, `docs/`, an unknown path, a dirty tree or a git error --
  prints `tests lane: FULL workspace` and runs everything. The `tests` attestation then
  means "every test the change can reach passed"; the attested PR skips remote Linux x64,
  and every push to `main` still runs the full suite. The graph and change set are one
  implementation, `ci/crate_graph.py`, shared with the `wine` lane.


## Local workflow parity: the real PR job under bosn -> act2 (soldr#3557)

The local gate's lanes are fast custom tasks. When a change needs proof that the
*actual* `ci.yml` Linux PR job passes -- its setup-soldr bootstrap, cache
foundations, guarded Cargo route and `soldr ci-test` DAG -- replay the job
through the published bosn -> pinned act2 engine:

```bash
bosn ci run --workflow .github/workflows/ci.yml --job build-linux-x64 --trigger pr --wait
bosn ci report <run>          # first failing step with its log tail
```

Measured on 2026-10-12 (bosn 0.1.21, act `0.2.89-act2.15`, 4 jobs, tree
`6707c250`): cold run 26.8 min, identical-tree warm run 19.8 min, all jobs green.
The run conclusion is `incomplete`, not `success`, because `publish-npm` needs
an OIDC issuer and is reported `remote_only` (GATE-012). The
`ci-attestations` resolve step fails without a GitHub token and is
`continue-on-error`. That is several times the gate's own `tests` lane, so the
replay is an on-demand parity check, not a gate lane: speed of the PR push
loop wins, and remote CI still runs the job.

Running under a non-GitHub image found two places where the workflow relied on
the runner image instead of declaring its needs. Both are fixed: the rustup
pre-seed is skipped when no rustup is installed, because setup-soldr
bootstraps it, and `.github/scripts/ensure_linux_build_tools.sh` installs
clang/lld when they are absent. Missing clang made about 95 linker-driver tests
fail under act2.
