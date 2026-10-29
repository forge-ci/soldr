# PyPI Trusted Publishing

This document describes the repo-side and owner-side steps needed to publish `soldr` wheels to PyPI using GitHub Actions OIDC Trusted Publishing.

It is intentionally PyPI-only. crates.io publication is not part of the current release direction.

## Current State

- `.github/workflows/auto-release.yml` is the only release workflow
- that workflow always builds the hardened `soldr` wheel set as part of a real release
- each platform job builds `soldr-cli` once, then packages that same target
  binary into both the GitHub Release archive and the PyPI wheel
- the workflow publishes those wheels through `pypa/gh-action-pypi-publish` in a dedicated OIDC job
- the existing PyPI project `soldr` already exists
- PyPI is the source of truth used by the workflow's "should we publish?" gate

## Why This Uses Trusted Publishing

PyPI Trusted Publishing avoids long-lived API tokens.

The workflow receives a short-lived upload credential from PyPI using the GitHub Actions OIDC identity for a specific repository and workflow file.

For `soldr`, the trusted identity is:

- owner: `zackees`
- repository: `soldr`
- workflow: `auto-release.yml`
- environment: blank

There is no environment gate in the unattended model.

## Owner Setup On PyPI

These steps must be performed in the PyPI web UI by a maintainer of the `soldr` project.

1. Open https://pypi.org/manage/project/soldr/settings/publishing/.
2. Add a new GitHub Actions publisher with:
   - repository owner: `zackees`
   - repository name: `soldr`
   - workflow filename: `auto-release.yml`
   - environment name: leave blank

Do not register a publisher for any other workflow filename. Only `auto-release.yml` is authorized to publish.

## Repo-Side Workflow Behavior

The release workflow runs whenever a reviewed version bump lands on `main` (it filters to `paths: Cargo.toml`):

1. Derives the release version from `[workspace.package]` in `Cargo.toml`.
2. Refuses to publish unless that version is strictly greater than the latest version on PyPI.
3. Reruns the full lint, test, packaging, and e2e gate on the merged commit.
4. Builds each platform binary once, packages it into both the GitHub Release
   archive and the hardened wheel, and verifies the wheel's embedded `soldr`
   binary SHA-256 matches the target release binary.
5. Creates the `vX.Y.Z` GitHub Release with checksums and a build provenance attestation (skipped if the git tag already exists, e.g. PyPI catch-up).
6. Publishes the wheel set to PyPI from a dedicated Linux job with `id-token: write`.

No source distribution is published in this path. The current design is wheel-only because the project is prioritizing hardened binary distribution rather than source release through package registries.

## Expected Manual Stops

The only manual dependency is the PyPI Trusted Publisher registration above.

Until the publisher is registered, the PyPI publish job will not be able to mint a trusted upload token.

## Verification

After a successful publish:

- https://pypi.org/project/soldr/X.Y.Z/ shows `Uploaded using Trusted Publishing? Yes`
- the wheel set on PyPI contains the same per-platform `soldr` binaries as the
  artifacts attached to the corresponding `vX.Y.Z` GitHub Release
