# Release Guide

This document records the release model `soldr` uses today.

It is written for two audiences:

- the repository owner, who configures the GitHub-side and PyPI-side controls
- a future agent, who needs to understand what the release workflow enforces

## Goal

The release model satisfies these properties:

- the version to release is `workspace.package.version` in `Cargo.toml` on `main`
- a release is an explicit `workflow_dispatch` of `auto-release.yml` naming one
  exact merged commit (`candidate_sha`)
- a successful **full** CI run on that exact SHA (`full_ci_run_id`) is required
  before any release artifact is built
- the matching `vX.Y.Z` tag and GitHub Release are minted by the workflow itself
- wheels are published to PyPI through OIDC Trusted Publishing
- no environment approval and no GitHub App credentials are required at release time
- published release assets are immutable, attestable, and verifiable

## Current State

These controls are in place:

- `main` is **not currently branch-protected**: there are no required
  checks, no rulesets, and merges are not mechanically gated (verified
  2026-08-11; restoring protection is tracked in soldr#2469)
- immutable GitHub Releases are enabled
- GitHub Actions requires full-SHA pinning for third-party actions
- `.github/workflows/auto-release.yml` is the only release workflow
- PyPI publication uses OIDC Trusted Publishing bound to `auto-release.yml`

crates.io publication is not part of the current release direction. `soldr` is being released as a hardened binary tool, not as a promised Rust library API surface. The completeness gate still accounts for it: `release_completeness.py` derives the publishable crates from the manifests and prints an explicit skip while every crate is `publish = false`; a crate that drops it joins the gate (soldr#3343).

Every release also publishes `soldr-<tag>-info.json`: the version, tag, candidate SHA, target-contract revision, targets, and each artifact's size and SHA-256 (soldr#3343). The publish job checks the full contract-derived asset set on the **draft** before making it public, because an immutable release can no longer gain assets (soldr#2469 4.2).

## Front door: `ci/release.py` (soldr#3343)

`ci/release.py` drives the two dispatches below for one exact merged SHA:

```bash
uv run --no-project python ci/release.py status --sha <40-hex merged SHA>
uv run --no-project python ci/release.py start  --sha <40-hex merged SHA>
```

`start` does one step per run, and re-running it is the resume path:
- It refuses a SHA that isn't on `origin/main`.
- It refuses when `vX.Y.Z` already tags a different commit; a released version is never moved.
- It dispatches full CI when the SHA has no `CI full <sha>` run, and waits while one is running.
- Once that run has succeeded, it dispatches `auto-release.yml` with the SHA and the run's id.

It never bumps a version or creates a tag.

## Dry run (soldr#2469 3.1)

Dispatch `auto-release.yml` with `candidate_sha`, `full_ci_run_id` and
`dry_run=true` to build, smoke and verify the full production matrix on that
exact SHA without writing anything. Every artifact is built, and the macOS,
Windows and darwin smokes and `release_execution_contract` all run. No tag,
GitHub Release, PyPI upload or npm publish is made, and the post-publish
verifiers are skipped. Use it to canary a release-workflow change, or a
lifted concurrency cap, before a real release.

## Promotion: publish a dry run's exact artifacts (soldr#2469 4.1)

A green dry run for a candidate can be promoted instead of rebuilt. Dispatch
`auto-release.yml` again with the same `candidate_sha` and `full_ci_run_id`,
plus `promote_run_id=<the dry run's id>`:
- `build` is skipped, so nothing is compiled.
- Every artifact download reads from the promoted run.
- `release_promotion_gate.py` first proves that run is this candidate's
  `auto-release.yml` dispatch (its run-name names the SHA), with every
  production build and release-artifact smoke green.

The smokes then run again against those exact artifacts before anything is
published. A promotion combined with `dry_run=true` exercises the whole
promotion path without publishing.

## Current Release Model

Since soldr#3346 (2026-09-24) a release is a deliberate two-dispatch act on
one exact commit. Merging a version bump no longer releases anything by
itself: `auto-release.yml` has only a `workflow_dispatch` trigger.

1. A reviewed PR bumps the version in lockstep: `[workspace.package].version`
   in `Cargo.toml`, `"version"` in `package.json`, and the workspace crates in
   `Cargo.lock` (see CLAUDE.md "Bumping soldr's own version"). The PR is
   merged to `main` (not branch-protected — see Current State above).
2. **Full CI on the exact SHA.** Dispatch the `CI` workflow on `main` with
   `candidate_sha=<full 40-char merged SHA>`. Its run-name becomes
   `CI full <sha>`, the `CI mode` job checks out and verifies that SHA, and
   every target in `ci/canonical-targets.json` plus the platform smokes run.
   It must finish with `CI mode` and `Full coverage` both successful. See
   [docs/CI_MODES.md](./docs/CI_MODES.md).
3. **Release dispatch.** Dispatch `Autonomous Release` (`auto-release.yml`)
   with `candidate_sha=<same SHA>` and `full_ci_run_id=<that CI run's ID>`.
4. `prepare` (`Detect explicit release candidate`) checks out the candidate
   and runs `release_full_ci_gate.py --verify-candidate`, which requires HEAD to
   equal `candidate_sha` and the SHA to be an ancestor of `origin/main`. Then
   `release_detect.py` derives `vX.Y.Z` from `Cargo.toml` (and requires
   `package.json` to agree) and computes one flag per public surface:
   - `should_publish_github_release`: the release is missing or incomplete
     *and* not immutable (an immutable release is never touched again);
   - `should_publish_pypi`: PyPI has no files for the version, or
     `force_pypi_publish` is set;
   - `should_publish_npm`: npm does not have the version;
   - `should_release`: any of the three.
5. `full_ci_gate` (`Verify exact-SHA full CI`) runs
   `release_full_ci_gate.py` against `full_ci_run_id`. The run must be a
   `workflow_dispatch` of `.github/workflows/ci.yml` from `main` in this
   repository, titled exactly `CI full <candidate_sha>`, completed
   successfully, with `CI mode` and `Full coverage` jobs both successful.
   Nothing is built otherwise.
6. The `build` matrix (generated from `ci/canonical-targets.json` by
   `release_completeness.py --build-matrix`) produces the platform archives
   and the hardened wheel set. The macOS Intel/ARM64 and Windows smokes, the
   darwin replay, and `release_execution_contract` exercise those exact
   artifacts before anything is published.
7. `publish` uses the built-in `GITHUB_TOKEN` to create the `vX.Y.Z` tag and
   GitHub Release with `SHA256SUMS.txt` and a build provenance attestation.
8. `verify_github_release` checks the published asset set against the target
   contract before any other registry is touched.
9. `publish-pypi` uploads the wheel set through OIDC Trusted Publishing;
   `smoke-published-dylint` installs the published wheel on Windows x64;
   `publish-npm` publishes `@zackees/soldr`.
10. `release-completeness` fails the run if any public surface ended up
    incomplete, so a partial release cannot report green (soldr#2469 step 1.1).

The ordering is `needs:` edges, not convention: **exact-SHA full CI → build →
smokes → GitHub Release → verification → PyPI → npm → completeness gate.**

Re-dispatching is safe and is the recovery path. Every surface flag is
recomputed from live state, so a second dispatch for the same SHA only does
what is still missing. v0.9.22 needed exactly this: the second dispatch
published everything and then failed only in the post-publish Dylint smoke;
the third went green without republishing anything.

Recovery inputs on the release dispatch:

- `force_pypi_publish` — rebuild and upload wheels for the current version
  even if PyPI lists files (only useful after a PyPI-side deletion/yank).
- `npm_release_ref` — publish only the npm package from a given ref; the
  `prepare` job and every other surface are skipped.

The intentional authorization steps are the reviewed version-bump merge and
the two dispatches. PyPI remains the source of truth for "have we already
shipped this".

## Owner Setup

These are the one-time controls that make the unattended flow work.

### 1. Protect `main` — not configured today

**This control is not in place.** `main` has no branch protection and the
repository has no rulesets (re-verified 2026-08-21: the protection API returns
404 and `/rulesets` returns `[]`). Restoring it is tracked in soldr#2469.

The target branch policy:

- no direct human pushes
- pull-request-only updates
- linear history enabled
- force pushes disabled
- deletions disabled
- required checks enabled

Until that is configured, nothing mechanically prevents a version bump from
reaching `main` without passing its PR checks. The release itself is gated:
`full_ci_gate` refuses to build unless an explicit full CI run on the exact
candidate SHA succeeded, so a bump merged past red checks still cannot ship
until full CI on the merged commit is green.

### 2. Register PyPI Trusted Publisher

Register a GitHub publisher on https://pypi.org/manage/project/soldr/settings/publishing/ as a maintainer of the `soldr` project:

- Owner: `zackees`
- Repository: `soldr`
- Workflow filename: `auto-release.yml`
- Environment: leave blank

There is no environment approval boundary in the unattended model.

### 3. Tag Protection

If `refs/tags/v*.*.*` is protected by a repository ruleset, the built-in `github-actions` integration must be an allowed bypass actor — otherwise the workflow cannot mint the tag with `GITHUB_TOKEN`. If tag protection is not required for this model, the ruleset can be disabled.

The previous GitHub App tag-creation path is not used. Any `RELEASE_APP_ID` variable, `RELEASE_APP_PRIVATE_KEY` secret, and the standalone release App can be removed.

## Future Agent Instructions

Before changing the release workflow:

1. Read this file.
2. Audit the live GitHub-side controls before trusting the checked-in docs.
3. Confirm the PyPI Trusted Publisher for `auto-release.yml` is still registered.
4. Check whether `main` is branch-protected. It is **not** today, so the
   expected result is a 404 from the protection API and an empty ruleset
   list — finding protection in place is the change worth reporting, and
   means §1 above and the claims in SECURITY.md and
   docs/RELEASE_VERIFICATION.md all need updating together.
5. Confirm tag protection (if any) still permits `github-actions` to mint `v*.*.*` tags.
6. Confirm the release is still dispatch-only with the exact-SHA full-CI
   gate (`auto-release.yml` `on:` and `full_ci_gate`). Do not reintroduce a
   push trigger, remove the full-CI gate, or add environment approval
   boundaries without explicit owner instruction.

If the live GitHub-side or PyPI-side controls drift from the documented flow, stop and report the drift instead of assuming the release posture is intact.

When preparing a normal release:

1. Pick `X.Y.Z` strictly greater than the latest PyPI `soldr` version, and
   confirm it is absent from PyPI, from npm `@zackees/soldr`, and from
   `git ls-remote --tags origin vX.Y.Z`.
2. Bump `Cargo.toml`, `package.json`, and refresh `Cargo.lock` (a no-op
   `soldr cargo build -p soldr-cli`); `version_lockstep` guards the trio.
3. Merge the bump PR to `main` with its normal CI green. Take the resulting
   merge commit's full SHA.
4. `gh workflow run ci.yml --repo zackees/soldr --ref main -f candidate_sha=<SHA>`
   and wait for it to succeed (it runs the whole target matrix; plan on
   roughly an hour).
5. `gh workflow run auto-release.yml --repo zackees/soldr --ref main
   -f candidate_sha=<SHA> -f full_ci_run_id=<CI run ID>`.
6. If a lane fails after publication started, fix the cause and re-dispatch
   step 5 for the same SHA; completed surfaces are skipped.

A release dispatch whose version is already fully published sets
`should_release=false` and skips every job. That is expected, not a failure;
a new release needs a new version bump.

## Verification Checklist

After an autonomous release:

- the `CI full <sha>` run for the candidate is green with `Full coverage` successful
- the `Autonomous Release` dispatch reports `should_release=true`, the expected `version=vX.Y.Z`, and a green `Release surface completeness gate`
- npm `@zackees/soldr` reports `X.Y.Z`
- a non-draft GitHub Release exists at `vX.Y.Z` with the platform archives and `SHA256SUMS.txt`
- `https://pypi.org/project/soldr/X.Y.Z/` shows `Uploaded using Trusted Publishing? Yes`
- `gh attestation verify dist/soldr-vX.Y.Z-*-SHA256SUMS.txt --repo zackees/soldr` succeeds against a downloaded archive

## Related Documents

- [README.md](./README.md)
- [SECURITY.md](./SECURITY.md)
- [docs/PYPI_TRUSTED_PUBLISHING.md](./docs/PYPI_TRUSTED_PUBLISHING.md)
- [docs/RELEASE_VERIFICATION.md](./docs/RELEASE_VERIFICATION.md)
- [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md)
