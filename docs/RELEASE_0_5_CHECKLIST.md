# Release 0.5 Checklist

This document is the concrete execution list for the first attested secure `soldr 0.5` release.

It assumes the current repository state on April 19, 2026:

- `.github/workflows/release-auto.yml` is the only release workflow
- the workflow derives the version directly from `Cargo.toml` on `main`
- publication uses a dedicated GitHub App
- `v*.*.*` tags are protected by repository rulesets
- immutable GitHub Releases are enabled
- `main` is protected
- final publication happens through the `release` environment

## 1. Freeze The `0.5` Product Surface

Decide what `0.5` means for the user-facing CLI.

Required decision:

- the `0.5` release line is the front-door build and tool-fetch line
- built-in zccache-backed compilation caching is part of the front-door `0.5.x` line
- `status`, `clean`, `config`, and `cache` must not be presented as complete features unless implemented

Do not ship `0.5` with placeholder commands presented as finished behavior.

## 2. Freeze The `0.5` Security Claim

Confirm the documented policy decisions in:

- issue [#12](https://github.com/zackees/soldr/issues/12) for verification, SBOM, and reproducibility policy
- issue [#13](https://github.com/zackees/soldr/issues/13) for hermeticity and runtime trust policy, with implementation follow-up in [#41](https://github.com/zackees/soldr/issues/41) and [#42](https://github.com/zackees/soldr/issues/42)

Current decisions for `0.5`:

- checksum plus `gh attestation verify` is the official user-facing verification story
- SBOM generation is not required for `0.5`
- reproducible-build claims are out of scope for `0.5`
- `0.5.x` does not claim hermetic builds; vendoring or mirroring Cargo, toolchain, OS-package, and pinned bootstrap-test inputs is deferred hardening tracked in [#41](https://github.com/zackees/soldr/issues/41)
- third-party binaries fetched by `soldr` at runtime remain an upstream trust decision on `0.5.x`, not a repository-side trust guarantee; stronger enforcement is tracked in [#42](https://github.com/zackees/soldr/issues/42)

## 3. Rehearse The Release Path

Before the first public `0.5` tag, rehearse with a reviewed release-candidate version bump on `main`.

Inputs to choose:

- candidate version such as `0.5.0-rc1`
- the exact PR that bumps `[workspace.package].version`

Required checks:

- the PR passes lint, test, and all supported e2e bootstrap jobs
- after merge, `release-auto.yml` starts automatically from the `main` push
- the workflow reaches the `release` environment approval gate
- the GitHub App token step succeeds
- checksums and provenance attestation steps succeed

## 4. Verify The Governance Controls Manually

Before the first real release, verify the controls that live outside the git tree.

```bash
gh api repos/zackees/soldr/rulesets
gh api repos/zackees/soldr/environments
gh api repos/zackees/soldr/branches/main/protection
gh api repos/zackees/soldr/immutable-releases
```

Success criteria:

- the `v*.*.*` tag ruleset is active
- only the release GitHub App can bypass that ruleset
- `release` still requires approval from `@zackees`
- `main` still requires the expected checks
- immutable releases remain enabled

## 5. Cut A Real Release Candidate

After the rehearsal passes, merge the first real release-candidate version bump.

Recommended first tag:

- `v0.5.0-rc1`

Required checks after publication:

- the GitHub Release contains all expected platform archives
- the `SHA256SUMS` manifest is present
- `gh attestation verify` succeeds for at least one downloaded archive
- a human cannot create, move, or delete the release tag manually
- the release cannot be mutated after publication because immutable releases are enabled

## 6. Enable Trusted PyPI Publishing If `0.5.0` Will Ship Wheels

If PyPI is part of `0.5.0`, configure the existing `soldr` project for Trusted Publishing before the final release.

Required setup:

- add the GitHub Actions trusted publisher on PyPI for:
  - owner: `zackees`
  - repository: `soldr`
  - workflow: `.github/workflows/release-auto.yml`
  - environment: `release`
- confirm the existing `soldr` PyPI project is the correct project and that stale pre-Trusted-Publishing metadata will be superseded by the next upload

Recommended GitHub-release verification commands:

```bash
gh release view v0.5.0-rc1 --repo zackees/soldr
gh attestation verify soldr-v0.5.0-rc1-x86_64-unknown-linux-gnu.tar.gz \
  --repo zackees/soldr \
  --signer-workflow zackees/soldr/.github/workflows/release-auto.yml
```

## 7. Audit The First Published Release

After `v0.5.0-rc1`, confirm that reality matches the docs:

- [RELEASE.md](../RELEASE.md)
- [RELEASE_VERIFICATION.md](./RELEASE_VERIFICATION.md)
- [RELEASE_GOVERNANCE_CHECKLIST.md](./RELEASE_GOVERNANCE_CHECKLIST.md)
- [PYPI_TRUSTED_PUBLISHING.md](./PYPI_TRUSTED_PUBLISHING.md)
- [TRUST_BOUNDARIES.md](./TRUST_BOUNDARIES.md)

Anything discovered during the RC must become either:

- a code fix
- a GitHub settings fix
- an explicit documented limitation

## 8. Decide Go Or No-Go For `v0.5.0`

Ship `v0.5.0` only when all of these are true:

- the intended `0.5` CLI surface is clearly scoped and honestly documented
- the release workflow has passed from a reviewed version-bump merge
- the first RC was verified with checksums and attestations
- the protected-tag and immutable-release controls behaved as expected
- if PyPI is in scope, Trusted Publishing was exercised successfully with the hardened wheel set
- the remaining policy questions are closed or explicitly deferred in writing

## 9. Gate `1.0.0-rc`

Do not cut `1.0.0-rc` until:

- the cache-enabled wrapper path delegates through managed zccache instead of acting as pure rustc pass-through
- the public cache commands describe and manage real behavior instead of placeholders

If those are not true, stay on the `0.5.x` line.
