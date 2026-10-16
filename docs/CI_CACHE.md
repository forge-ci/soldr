# CI Cache Guide For External Repos

This is a usage guide for anyone wiring `zackees/setup-soldr@v0` into their own GitHub Actions CI. It explains what you get automatically, what the minimum config looks like, and how to confirm warm builds on feature branches are actually restoring from `main`.

If you want the background on why this repository wires its own workflows the way it does, skip to [Why This Repo Uses This Model](#why-this-repo-uses-this-model) at the bottom.

## TL;DR

Add `zackees/setup-soldr@v0` to a normal `push`-triggered workflow:

```yaml
- uses: actions/checkout@v4
- uses: zackees/setup-soldr@v0
- run: soldr cargo build
```

You get, for free:

- branch-agnostic cache keys the action produces on its own
- automatic restore on feature branches from the latest `main` cache on a miss
- no separate `actions/cache` step; the action already runs the setup-state cache internally and also restores and saves the Soldr-owned zccache cache root by default
- `cache-hit` and `build-cache-hit` outputs (`target-cache-hit` is a deprecated output that is always empty) you can read to confirm warm vs cold runs

The rest of this document explains how and why that works.

> **Deprecated (soldr#2996).** soldr no longer implements a target cache. The
> `target-cache*` inputs and outputs described below are inert on the soldr
> side; `soldr cook` is the only durable compiler cache. Retiring the inputs
> themselves is an upstream change to the action. As of soldr#3651 the
> setup-soldr action matches: `target-cache` and `target-cache-mode` are
> deprecated no-ops (default off), no `target/` cache step runs, no
> `SOLDR_TARGET_CACHE_*` is exported, and a warning is printed if a caller
> enables them.

## Cache Ownership And Priority

Cache admission is decided by **the stability of the artifact's identity key
relative to its size**. Cache what invalidates rarely; never persist what
invalidates on every edit. The gradient, from most to least cacheable:

| Artifact class | Invalidates when | Owner |
|---|---|---|
| Toolchain / SDK / catalogue downloads | pinned version bumps (rare, explicit) | setup-state cache (content-pinned + sha256) |
| External dependency compilation | `Cargo.lock` / recipe changes | **soldr cook** (Tier 1) |
| Per-compilation-unit compiler outputs | that unit's input hash changes | **zccache store** (Tier 2, content-addressed) |
| Workspace linked products — final binaries and **test executables** (`--test` harnesses) | any edit to a crate they link | **zccache store** (Tier 2; harnesses admitted explicitly, see below) |

**Test harnesses are cached.** Soldr admits every rustc `--test` compile —
the harness link and the metadata-only Clippy/Dylint check of a test target —
to the zccache store with zccache's `TestHarnessAdmission::All`
(`soldr_compile_options` in `crates/soldr-daemon/src/zccache_embedded.rs`).
zccache refuses them by default (zccache#1525) because a harness re-links on
every edit to anything it links and can be large; Soldr overrides that on
purpose. `soldr cook` already partitions the long-lived dependency cache
(Tier 1) from large end-state products, and consolidating test targets keeps
harnesses few and small. zccache still skips storing any single harness above
512 MiB (`MAX_ADMITTED_TEST_HARNESS_BYTES`).

What stays costly is **test-target fan-out**. Lesson learned (soldr#2931): this
repo's suite reached ~110 linked test binaries and a 3.3 GB compressed nextest
archive that exhausted a hosted runner's disk, while a guard *required* that
archive to be warm-cacheable at zero misses. The fix was fewer test targets
(soldr#2934 consolidated the soldr-cli integration tests into eight), not a
refusal. A same-run transport bundle used to execute cross-built tests on
their target must still stay compact, single-extraction, and budgeted. When
adding a cache layer, name its tier first.

## How GitHub Actions Cache Scoping Actually Works

A workflow run in GitHub Actions can restore caches from a limited set of scopes, and **not from arbitrary sibling branches**. For any given run, GitHub will consider caches in this order:

1. The run's own branch
2. For `pull_request` events, the PR base branch
3. The repository's default branch (usually `main`)

That means two feature branches cannot share a cache entry directly. The only way to get a shared lineage is to treat the default branch as a shared parent: `main` writes caches, feature branches read them on miss.

Authoritative reference: [Caching dependencies to speed up workflows](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/cache-dependencies).

Two consequences of that scoping rule matter for soldr:

- **`main` is the canonical warm source.** Keep `main` passing so it refreshes its cache entries on every push. A broken `main` pipeline means cold feature-branch builds.
- **Saves are own-branch only.** A run on `feature/x` cannot write into `main`'s cache scope, and it cannot write into `feature/y`'s cache scope. It saves into its own branch scope, and later runs on that same branch restore it first.

## What setup-soldr Does For You Automatically

The `zackees/setup-soldr@v0` action (generated from [`action.yml`](../action.yml)) runs internal cache steps keyed so that the parent-to-child restore works correctly without you configuring anything:

- **Branch-agnostic state-cache keys.** The setup-state cache key is derived from runner OS, runner architecture, the resolved Rust toolchain channel, and the requested soldr version. No branch name is in the key. Two branches with the same toolchain pin produce the same key, so a cache written by `main` is a valid candidate for a run on any feature branch.
- **Restore-keys prefix for partial-match fallback.** The action registers a restore prefix (`setup-soldr-v0-{os}-{arch}-`) so that even if a future toolchain bump changes the exact key, GitHub can still fall back to the most recent compatible cache for the same OS and architecture.
- **Push-only save semantics come for free.** GitHub's cache scoping already prevents feature-branch runs from overwriting `main`'s cache. You do not need to gate `save-if` yourself the way internal Rust caching wrappers usually make you do.
- **Rehydrated state.** On a cache hit, the action restores the soldr root, `CARGO_HOME`, and `RUSTUP_HOME` under the runner-local cache/state root. The resolved Rust toolchain and the `soldr` binary are then provisioned on top of whatever was restored.
- **Build-artifact cache enabled by default.** The action also restores the Soldr-owned zccache cache root with a toolchain-scoped key and saves it at end-of-job, so zccache compilation artifacts survive across runs unless you opt out with `build-cache: false`.
- **No target cache (soldr#3651).** `target-cache` and `target-cache-mode` (`thin`/`full`) are deprecated no-ops: the action restores and saves no `target/` or Rust artifact plan cache, and enabling them only prints a warning.

Release/LTO musl validation and daemon-failure diagnostics live in
[`docs/DATALAKE_RELEASE_MUSL.md`](DATALAKE_RELEASE_MUSL.md).

## Minimum Config For An External Repo

This is the complete workflow. Copy-paste into `.github/workflows/ci.yml` and adjust the job matrix if you need more than Linux:

```yaml
name: CI

on:
  push:
    branches: ['**']

permissions:
  contents: read

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: zackees/setup-soldr@v0
        with:
          cache: true

      - run: soldr cargo build
      - run: soldr cargo test
```

That is enough. No separate `actions/cache` step, no `Swatinem/rust-cache`, no manual `save-if` gating. The action handles the cache internally with the key shapes described above.

A slightly fuller example that also demonstrates reading the action's outputs lives in [`examples/ci-minimal.yml`](../examples/ci-minimal.yml).

## Triggering On `push` Vs `pull_request`

Prefer `push` on all branches as your default trigger:

```yaml
on:
  push:
    branches: ['**']
```

Why:

- A `push` run on `feature/x` writes its cache into the `feature/x` scope. The next push on that branch restores it first. PRs from `feature/x` implicitly see those checks because they are tied to the branch head.
- A `pull_request` run builds the synthetic merge commit, writes into a PR-specific cache lineage, and duplicates the work that the branch push already did. You end up with two lineages competing for the same build time.
- Adding `pull_request` on top of `push` roughly doubles CI minutes and creates a second cache entry that does not help feature-branch restore from `main`.

Add `pull_request` only if you explicitly need CI on the PR merge commit (for example, a required merge-ref signal that a forked-branch push cannot produce). Most repos do not need this.

## Verifying The Cache Is Working

After two pushes to the same branch, you should be able to confirm the cache lineage is healthy.

1. **Check the `cache-hit` and `build-cache-hit` outputs (`target-cache-hit` is deprecated and always empty) of the setup step.** Reference them from a later step like this:

   ```yaml
   - id: soldr
     uses: zackees/setup-soldr@v0
     with:
       cache: true
   - run: echo "cache-hit=${{ steps.soldr.outputs.cache-hit }}"
   - run: echo "build-cache-hit=${{ steps.soldr.outputs.build-cache-hit }}"
   - run: echo "target-cache-hit=${{ steps.soldr.outputs.target-cache-hit }}"
   ```

   `true` means the key matched exactly. `false` means either a fresh key (cold) or a restore-keys fallback match (partial). Both `false` cases show the same literal `false`; distinguish them using the raw log.

2. **Open the raw log of the setup step.** Expand the internal cache steps inside the composite action. You want to see either:
   - `Cache restored from key: setup-soldr-v0-...` for an exact setup-state cache hit, or
   - `Cache restored successfully` followed by a key that matches the restore prefix `setup-soldr-v0-{os}-{arch}-` for a partial setup-state restore.

   A line that says no cache was found at all, with no restore match, indicates a cold miss.

   For the build-artifact layer, inspect the `build-cache-restore` step. Its exact keys are `setup-soldr-buildcache-v1-{os}-{arch}-{toolchain-digest}-{github.sha}` and its restore-keys fall back first to the same toolchain lineage, then to any cache for the same OS and architecture.

   There is no Rust artifact plan layer: the `target-cache` step was removed (soldr#3651).

3. **Compare wall-clock.** A warm feature-branch run should not rebuild the toolchain or re-download soldr. A warm build-artifact restore should also reduce downstream compile time once zccache has artifacts to reuse. If you see `rustup` installing, soldr downloading from GitHub Releases, or full recompiles on every run, one of the restore layers is not hitting and something below is wrong.

4. **Inspect zccache stats after the build.** Add a post-build status step when validating a new cache lineage:

   ```yaml
   - run: soldr cache
   ```

   The output includes the embedded zccache cache root and daemon-reported
   counters. For a healthy warm build, look for non-zero cached compilations
   or hit counts, plus the `zccache rust-plan restore/save` JSON summaries
   emitted by `soldr cargo ...`. If `build-cache-hit=true` but zccache still
   reports zero cached compilations, the build-artifact cache restored but did
   not produce compiler-cache reuse; check whether Cargo invalidated fingerprints before zccache could
   hit.

## Debugging Cold Misses

If feature branches keep rebuilding from scratch, check these in order:

- **Has `main` run successfully recently?** The restore fallback only works if the default branch has written a cache. If the main-branch pipeline is red or was never run on this workflow file, there is no parent to restore from. Fix `main` first.
- **Is `Cargo.lock` churning on every push?** Lockfile changes do not change the setup-soldr state-cache key, but they do invalidate the Rust artifact plan cache and can reduce downstream zccache reuse. Check whether your workflow keeps regenerating `Cargo.lock` (for example, because `Cargo.lock` is gitignored in an application repo where it should be committed).
- **Did `rust-toolchain.toml` change?** The resolved toolchain channel is part of both cache key families. Bumping the toolchain channel or the components/targets list invalidates every existing entry. That is expected behavior; the next push to `main` will write a fresh canonical entry.
- **Did you pass a `cache-key-suffix` input?** That value is appended to both cache key families (see `action.yml`). A different suffix on a feature branch produces a different key than `main` writes, and the restore will only succeed through the prefix fallback. Make sure the same suffix is used (or omitted) on every branch you want to share a lineage.
- **Mixed runner OS/arch.** Cache keys are scoped by runner OS and architecture. A cache written on `ubuntu-24.04` will not restore on `macos-15` and vice versa. Each combination needs its own warm lineage from `main`.
- **Did someone opt out of build caching?** If `build-cache: false` is set in the workflow, `build-cache-hit` will be empty and the Soldr-owned zccache cache root will not be restored or saved.
- **Is `target-cache` set in the workflow?** It is a deprecated no-op; `target-cache-hit` is always empty and nothing restores or saves Rust target artifacts.

---

## Why This Repo Uses This Model

The rest of this document is ancillary context about how this repository's own CI is wired. External consumers do not need any of this.

GitHub Actions caches are not shared across arbitrary sibling branches. A run can restore from:

- its own branch
- the default branch
- for pull requests, the PR base branch

So the right model is not "share caches between feature branches". The right model is:

1. `main` stays warm and acts as the shared parent lineage.
2. A feature branch restores from its own cache if it already has one.
3. Otherwise the feature branch restores from `main`.
4. A feature-branch push may then save a better branch-local cache for later runs of that same branch.

## How This Repo Is Wired

In [`.github/workflows/ci.yml`](../.github/workflows/ci.yml):

- `push` runs on `main` only. Feature branches are covered by the
  `pull_request` trigger instead — soldr#1985 narrowed the push trigger
  because `branches-ignore` matched every PR branch, so each push to an open
  PR ran the whole sweep twice.
- The heavy cache-producing CI workflow therefore *does* run on
  `pull_request`; that is the only way a feature branch gets coverage.
- `Swatinem/rust-cache` is banned fleet-wide (zackees/ci.yml#209, ci-lint
  `CACHE-025`) with **no exceptions** (zackees/ci.yml, maintainer decision
  2026-10-02): no same-line allow comment, no ci.toml `[[exceptions]]` entry.
  The last uses -- `ci.yml`'s bootstrap driver build and the bare-cargo soldr
  builds in `baseline-zero-deps.yml`, `parent-cache-bench.yml` and
  `perf-cold-warm.yml` -- were removed; those builds now run uncached on a
  binary-cache miss. `tests/test_ci_cache_key_scheme.py` pins the absence.
  `save-if: ${{ github.ref == 'refs/heads/main' }}`, so a PR run restores
  whatever `main` last wrote and never saves its own branch-scoped copy — the
  opposite of the `shared-key:`-only, no-`save-if:` posture this repo used to
  have, and deliberate: every open PR used to carry its own copy of the
  entry, which is what pushed the repository over its cache budget (see
  [Repository cache budget](#repository-cache-budget-soldr3047) below).
  `actions/cache` itself has no `save-if` input, so the two main-gated
  `actions/cache` stores — the Dylint foundation trees and the dogfood
  zccache store — get the same effect by splitting into an unconditional
  `actions/cache/restore` followed by an `actions/cache/save` step gated on
  `github.ref == 'refs/heads/main'`.

### The Tier-2 object store is persisted, not just isolated (soldr#3041)

In [`.github/workflows/_build-and-test.yml`](../.github/workflows/_build-and-test.yml)
the host validation lane points `SOLDR_CACHE_DIR` at a per-target directory
under `runner.temp` so every step in the lane shares one wrapper and one
compiler cache. That choice is about *isolation*; `runner.temp` also threw the
store away at the end of every run, so the lane's only per-unit cache was cold
every time. An `actions/cache` step now persists it:

- **Path is selected positively**: `…/cache/zccache/daemon-state` — the
  embedded service's object store
  (`daemon-state/embedded-v1/v<VERSION>/`). The compile journals
  (`cache/zccache/history/`) and session logs (`cache/zccache/logs/`) are
  siblings *outside* that directory and stay run-scoped, shipped by the
  build-logs artifact. Do not switch this to `cache/zccache` plus `!…/history`
  negations: `actions/cache` globs `path` with `implicitDescendants: false`
  and hands the matched directory to `tar`, which recurses, so the negation
  removes nothing. What the positive selection does *not* exclude is the
  service's own versioned log directory
  (`daemon-state/embedded-v1/v<VERSION>/logs/`), which is inside the store and
  rides along in the entry; zccache owns its rotation, and a `path` negation
  could not drop it either.
- **The key rolls**: it ends in `${{ github.run_id }}`, because
  `actions/cache` never re-saves on an exact-key hit — a stable key would
  freeze the store after its first save. Two `restore-keys` fall back to the
  same dependency graph first, then to any earlier entry.
- **Runtime coordination state is scrubbed first**: a step after
  `Stop canonical host CI cache` deletes `staging/` plus every
  `.lock` / `.sock` / `.socket` / `.pid` file under `daemon-state`, mirroring
  `archive_always_excludes_cache_path` in
  `crates/soldr-cache/src/cache_lib/save_inventory.rs`. Soldr's own archive
  transport already refuses to collect those files: the daemon deletes its
  `.active.lock` between walk and stat, which killed a real `soldr save`, and
  `tar` shares that race. A failed cache save is only a *warning*, so without
  the scrub the entry could silently never be written while the lane stayed
  green — and a restored stale lock or socket is documented in that module as
  preventing the compile daemon from starting.
- **Tier**: `zccache-unit`, already declared durable in
  `ci/cache-ownership.json`. This is not a new cache family; content-addressed
  per-unit caching was never what soldr#2931 banned.

### Cache key scheme (soldr#1978 item 6)

No rust-cache namespace survives (CACHE-025, no exceptions since
2026-10-02). While they existed they were keyed on **(profile, target)**, not
on the job:

```
shared-key: ws-<profile>-<target>
```

The point is that lanes compiling the same dependency graph, at the same
profile, for the same triple should share one namespace instead of each
owning a private one.

The dev-profile instance of that scheme is **gone**, and the reason is worth
keeping: `ws-dev-<target>` promised one `target/` restore for the native
validation lane, but soldr#2996 measured the `Swatinem/rust-cache` step
serving it at a 0% hit rate — the key's environment hash covers every
installed toolchain, so it flipped with the Dylint nightly on every run.
soldr#3047 deleted the step rather than re-key it; the successor is the
Tier-2 per-unit object store (soldr#3041), not another shared-key namespace
(soldr#3043's workflow-level `soldr cook` step was retired by soldr#3396), and
`tests/test_ci_cache_key_scheme.py` now pins the namespace's *absence*. The
last `ws-release-*` namespace, in `baseline-zero-deps.yml`, went with the
CACHE-025 no-exceptions decision.

Two constraints make this less mechanical than it looks:

- **A shared key is worthless unless the sharers write the same directory.**
  Cargo puts a bare build in `target/debug` and a `--target`-qualified build
  in `target/<triple>/debug`. `lint` had no `--target`, so it shared nothing
  with `build-linux-x64` no matter what the key said. Both now pass
  `--target`, and `tests/test_ci_cache_key_scheme.py` pins the flag and the
  key together.
- **Performance and cache-strategy lanes stay off the scheme.**
  `perf-matrix.yml`, `perf-cold-warm.yml`, `parent-cache-bench.yml`, and
  `cache-delta-experiment.yml` keep private, workflow-scoped namespaces. They
  either measure timings that an unrelated lane's writes would contaminate,
  or compare cache strategies. `perf-cold-warm.yml` additionally purges every
  repo cache whose key *contains* `perf-cold-warm`, so its key must keep that
  substring and no shared key may contain it — both pinned by the same test.

Note that `rust-cache`'s key does not include Cargo profile *definitions*, so
a change to a `[profile.*]` table does not invalidate these caches on its own;
that is why the profile is spelled out in the key text.

This repository itself is the reference implementation of that pattern.

## Repository cache budget (soldr#3047)

GitHub's Actions cache has a hard 10 GB ceiling per repository. Nothing asks
permission before that ceiling is reached — GitHub silently evicts the
least-recently-used entries to make room, so a producer that writes a lot but
is rarely read can quietly starve a producer that is small but read on every
run. That makes the ceiling a shared-resource problem, not a per-workflow one:
a persisted object store (soldr#3041) and cook archives (soldr#3042/#3043) are
worthless if an unrelated lane's growth evicts them before they are ever
restored.

Measured on 2026-09-01, before this PR: 47,489,917,871 bytes (44.23 GiB)
across 143 entries — more than four times the ceiling. 23.79 GiB of that was
`Swatinem/rust-cache` alone, and the reason was structural rather than a size
problem with any one entry: nothing restricted saves to `main`, so every
open PR wrote its own branch-scoped copy of every rust-cache producer, and
those copies never got evicted fast enough to matter.

The fix has two parts. First, stop writing what does not need to exist:
`cross-build-rust-cache`, `build-and-test-rust-cache`, `ci-pep517-rust-cache`,
and `target-run-pep517-rust-cache` are retired outright (their `setup-soldr
cook` or plain "run uncached" replacements are cheaper than a rust-cache
entry every PR pays for and few ever hit). The one surviving
`cook-unreachable-lane` exception — `ci.yml`'s bootstrap driver build, which
runs bare `cargo build` with `RUSTC_WRAPPER` emptied and has no setup-soldr
step — was retired too when CACHE-025 dropped its exceptions (zackees/ci.yml,
maintainer decision 2026-10-02): that build now runs uncached on a
binary-cache miss, and the `cook-unreachable-lane` tier is gone.
Second, gate what remains to `main`-only saves (see [How This Repo Is
Wired](#how-this-repo-is-wired) above) so a PR restores but never writes.

What survives that sweep is budgeted, not merely trimmed, because "smaller"
is not the same guarantee as "under the ceiling forever."

**`ci.toml` is the one declaration (soldr#3618).** Every cache family — its
key prefix (`prefix`, or a `via` producer ci-lint knows) and its size (`max`
x `per`, or the sum of its `shapes`) — is declared once, under
`[cache.family]` in `ci.toml`. ci-lint reads it directly (CACHE-001/004/029,
`ci-lint cache janitor`), and the janitor that actually deletes
(`.github/scripts/check_cache_budget.py`, run by `ci-pre.yml` with
`actions: write`) reads the same declaration through
`.github/scripts/cache_families.py`, which computes each family's footprint
with CACHE-004's arithmetic. No other file carries a per-family byte number.

`ci/cache-ownership.json` keeps only what `ci.toml`'s schema cannot say. Its
`family_groups` object assigns every `ci.toml` family to exactly one
ownership group, and per group records the ownership `entries` that
`check_cache_ownership.py` R7 checks and the janitor's `evict` policy
(`lru`, `newest`, `newest-per-lineage`, or none = never evicted for budget);
`ci.toml`'s own `evict` accepts only `"lru"`. `zccache-unit` also keeps
`store_cap_bytes`, the uncompressed on-disk trim cap of the zccache store,
which is a different quantity from its compressed Actions footprint. A
group's allocation is the sum of its members' footprints, so the janitor
still pools, for example, the nine experiment-lane prefixes under one
allocation.

The janitor's `total_max_bytes` is the sum of every family's footprint, and
its hard ceiling `fail_total_bytes` is `ci.toml`'s `[cache].budget` (9.5 GiB;
ci-lint sizes are binary, so `"1GB"` is 1 GiB). CACHE-004 requires the
families plus `[cache.pr].budget` (1 GiB) to fit that ceiling, which holds the
family total at or below the 8.5 GiB `total_max_bytes` CACHE-025 left when it
retired the last Swatinem producer. GitHub does not publish which
byte-multiple its documented "10 GB" is: 9.5 GiB sits under the binary
reading (10,737,418,240) but 2% over the decimal one (10,000,000,000), which
is why every family is sized against the family total, not the ceiling.

| Group | ci.toml families |
|---|---|
| `attestation-evidence` | `att1` |
| `pinned-immutable-download` | `rustup`, `xwin`, `apple-sdk`, `toolchain`, `soldr-mini`, `uv` |
| `bootstrap-driver-binary` | `bootstrap-soldr` |
| `cook-layer` | `cook` |
| `dylint-foundation` | `dylint-foundation`, `dylint-driver` |
| `setup-soldr-action-stores` | `compile`, `registry`, `dogfood-zccache` |
| `experiment-lanes` | `soldr-bin`, `perf-build`, `cache-delta-exp`, `bootstrap-release`, `zigbuild-toolchain`, `perf-cold-warm-zccache`, `soldr-baseline`, `soldr-cook`, `soldr-delta` |
| `zccache-unit` | `zccache-unit` |

`check_cache_budget.py` fails the build if a cache key matches no declared
family — a new producer cannot appear without being declared in `ci.toml` in
the same PR that adds it, and cannot be silently folded into an existing
family's headroom by accident. Composing the budget also fails if a `ci.toml`
family belongs to no group or to two, or names a `via` that
`cache_families.VIA_PREFIXES` cannot map to a prefix. Never list a live
prefix in `[cache].retired` to silence ci-lint: that is CACHE-009 and tells
every janitor to delete warm caches. Raising the family total is not one of
the available levers; the only way to make room for a new producer is to
retire or shrink an existing one.

GitHub's own LRU eviction is not under this repo's control and does not
consult the family allocations above, so the budget guard pairs with a
`--prune` sweep that brings the live store back inside those allocations
directly instead of waiting on GitHub's schedule; see the script's own
docstring for the selection policy. The sizes in `ci.toml` record the live
listing they were measured from.

### The janitor converges; the verdict charges whoever caused it (zackees/ci.yml#6)

The budget is enforced by `.github/workflows/ci-pre.yml` ("CI Pre"), the first
job of `ci.yml`, which `cache-budget.yml` also calls for non-main pushes, the
six-hourly schedule and `workflow_dispatch`. It installs nothing: stdlib
`python3` on `ubuntu-24.04`, a sparse checkout, 2-minute timeouts, and no job
in `ci.yml` `needs:` it. It has two independent jobs:

- **Cache janitor** (`--prune --apply --sweep-only`, job-level
  `concurrency: cache-janitor`, never cancelled). It deletes, in this order:
  every entry of a PR that is no longer open (merged or closed; matched by
  `refs/pull/<N>/*` ref or `pr-<N>` key tag, one state lookup per PR, kept
  when the lookup fails); the existing safe-prune classes; then, for families
  whose group declares an `evict` policy in `ci/cache-ownership.json`, whatever that
  policy needs to fit the family's allocation. `experiment-lanes` is `lru`;
  `pinned-immutable-download` is `newest-per-lineage` (the newest version of
  each download is never evicted). Families without `evict` are never evicted
  for budget. Apart from closed PRs' entries, nothing younger than
  `SWEEP_GRACE_SECONDS` (10 minutes) is deleted. Open PRs keep their entries,
  which count toward the budget.
- **Repository Actions cache budget** (the verdict). It depends on the event:
  a pull request fails only for what it saved itself (its own ref or `pr-<N>`
  tag) or for a static manifest breach, and warns about everything else; a push
  to a non-main branch warns; main, schedule and dispatch fail hard.

`cache-budget.yml` also runs on `pull_request: closed` (the one PR trigger a
non-`ci.yml` workflow may declare) to delete that PR's entries at once.
Every cache a PR-reachable workflow can save in PR context carries
`${{ env.PR_CACHE_TAG }}` (`-pr-<N>`, empty elsewhere), or does not save on PRs
at all. The immutable downloads (rustup, xwin, Apple SDK, Dylint driver) are
restore/save splits that PRs restore but never save.
`.github/scripts/check_pr_cache_keys.py` enforces this in Lint.
