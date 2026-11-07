# Security

## Scope

`soldr` is a binary bootstrap/build tool. Security for this repo is therefore about:

- release integrity
- supply-chain trust boundaries
- workflow and dependency immutability
- user verification of published artifacts

This document describes the current hardening posture for the `0.5.x` line and the remaining policy work before the first attested secure release.

Related documentation:

- [docs/RELEASE_VERIFICATION.md](./docs/RELEASE_VERIFICATION.md)
- [docs/RELEASE_GOVERNANCE_CHECKLIST.md](./docs/RELEASE_GOVERNANCE_CHECKLIST.md)
- [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md)

## Current hardening

The repository currently enforces several baseline controls:

- Rust dependencies are locked in `Cargo.lock`.
- CI and release builds use `cargo ...`.
- CI now enforces `cargo fmt --check` and `cargo clippy -D warnings`.
- No Cargo `git` dependencies are currently used; dependencies resolve from crates.io.
- Third-party GitHub Actions in the repository workflows are pinned to full commit SHAs.
- The e2e bootstrap fixture input is pinned to an exact Git commit in the workflow inputs.
- Releases are promoted from reviewed version bumps on `main` through `.github/workflows/release-auto.yml`.
- The release workflow refuses to publish unless the workspace version is strictly greater than the latest version on PyPI.
- PyPI publication uses OIDC Trusted Publishing bound to `release-auto.yml`; no long-lived PyPI tokens exist in the repo or in any environment.
- Release assets are attested in GitHub Actions before publication.
- Release tags are minted by the workflow's built-in `GITHUB_TOKEN`; no GitHub App or PAT is involved.
- Immutable GitHub Releases are enabled.

These controls reduce drift, but they do not make the full release pipeline hermetic.

The current `0.5.x` line does not claim hermetic builds, and it does not claim that third-party binaries fetched later by `soldr` are repository-verified just because `soldr` downloaded them.

## What is pinned

The repo aims to pin security-relevant inputs wherever practical:

- Cargo dependency graph: pinned by `Cargo.lock`
- GitHub Actions: pinned by full commit SHA
- E2E third-party test source: pinned by exact commit SHA
- Rust toolchain action implementation: pinned by full commit SHA

For release integrity, pinning by floating tag or branch is treated as insufficient for third-party workflow code.

## What is still external or mutable

Even with the current pinning, some inputs still come from external systems at build or runtime:

- rustup toolchain downloads during CI/release
- crates.io index and crate downloads unless dependencies are vendored or mirrored
- OS package repositories such as `apt` in musl jobs
- GitHub Releases as the publication surface, even though immutable releases are enabled
- third-party binary artifacts fetched by soldr at runtime

These boundaries are intentional or transitional. They must be documented so users understand what is and is not covered by our own release assurances.

## Versioning and update policy

Security-relevant versioning and pinning should follow these rules:

- Cargo dependency changes must update `Cargo.lock`.
- Workflow action upgrades should be explicit pull requests that update SHAs, not floating major tags.
- Bootstrap fixture inputs should remain pinned to exact commits.
- Release toolchain changes should be explicit and reviewed.

When a pinned dependency, action, or external input is updated, the change should be visible in git history and reviewed like code.

## Release trust model

Current state:

- releases are validated in GitHub Actions from the merged `main` commit that bumped the workspace version
- `.github/workflows/release-auto.yml` is the single release path; it derives the version from `Cargo.toml` and only proceeds when that version is strictly greater than the latest version published on PyPI
- the release workflow does **not** currently re-run lint, build, test,
  integration, or e2e checks before publishing; the only pre-publication
  validation is what ran on the version-bump PR (restoring release-time
  gates is tracked in soldr#2469)
- release tags are minted with the workflow's built-in `GITHUB_TOKEN`; no GitHub App or PAT is involved
- PyPI wheels are uploaded through OIDC Trusted Publishing bound to `release-auto.yml`
- release assets are published to GitHub Releases with a generated checksum manifest
- release assets are attested in GitHub Actions prior to publication
- the intentional authorization step is the reviewed version-bump merge to `main` — note that `main` is not currently branch-protected (soldr#2469) and there is no environment approval gate at release time
- immutable releases and any tag-protection settings still depend on repository configuration outside the git tree
- current user-facing verification guidance is checksum verification plus `gh attestation verify`

Current verification policy:

- checksum verification plus `gh attestation verify` is the official user-facing verification story
- GitHub CLI is the primary documented attestation-verification tool
- offline attestation bundles may be archived, but separate Sigstore tooling is not required for the normal verification path
- SBOM publication is not currently required for the release line
- reproducible-build claims are not currently made for `soldr`
- no extra signed release metadata is currently published beyond `SHA256SUMS` and GitHub provenance attestations

Hermeticity and runtime-trust policy (final for `0.5`):

- `0.5.x` release verification covers published `soldr` artifacts, not every external input used during CI
- `0.5.x` does not claim hermetic builds; documented dependencies on GitHub-hosted runners, `rustup`, crates.io, GitHub APIs/Releases, live `apt`, and the pinned bootstrap test repository are accepted as the final input set for this line
- Cargo vendoring, toolchain mirroring, OS-package mirroring, and bootstrap fixture source mirroring are explicitly out of scope for `0.5`; any revisit is scoped to a future `1.0.0-rc` hardening milestone
- SBOMs are not required for `0.5`; GitHub provenance attestations plus `SHA256SUMS` are the verification story
- reproducible-build claims are not made for `0.5`
- runtime third-party binary trust is enforced as of `0.6.x` via SHA-256 pinning and `SOLDR_TRUST_MODE=strict` (originally tracked in [#42](https://github.com/zackees/soldr/issues/42), closed)

`1.0.0-rc` remains gated on actual compilation-cache integration rather than release hardening alone.

## Reporting a security issue

If you discover a vulnerability or supply-chain concern, open a GitHub issue unless the issue is sensitive enough that public disclosure would create immediate risk.

For high-risk undisclosed issues, contact the maintainers privately before opening a public issue.
