# Release Verification

This document describes how to verify a published `soldr` release today.

It is intentionally limited to what the repository currently implements.

## Verification Policy

The current `soldr` verification policy is:

- the official user-facing verification path is checksum verification plus `gh attestation verify`
- GitHub CLI is the primary documented tool for attestation verification
- Sigstore-compatible offline verification remains possible through downloaded attestation bundles, but it is not the primary documented path
- `soldr` does not currently require or publish SBOMs for the release line
- `soldr` does not currently claim independently reproducible builds
- `soldr` does not currently publish extra signed metadata beyond the checksum manifest and GitHub provenance attestations

Those positions are deliberate. They may be revisited later, but they are the current release policy rather than open questions.

## What The Current Release Flow Guarantees

For a normal `soldr` release:

- a reviewed version bump is merged to `main` (not currently
  branch-protected — no required checks or rulesets; soldr#2469 tracks
  restoring protection)
- `.github/workflows/auto-release.yml` derives the tag directly from `Cargo.toml` at that merged commit
- the workflow does **not** currently re-run lint, build, test,
  integration, or e2e jobs for that commit before publishing; the only
  validation is what ran on the PR (target state: soldr#2469 Phase 1)
- release archives are built from that exact commit
- only the **npm** publication runs in the `release` environment; the
  GitHub Release and PyPI publications do not declare an environment, and
  the `release` environment itself has no protection rules or deployment
  branch policy today, so it is not an approval gate for any surface
  (verified 2026-08-21; soldr#2469 Phase 4 is the target state)
- the version tag is created through the release workflow path; no
  repository rulesets protect tags today (soldr#2469)
- the published GitHub Release is immutable once published
- a SHA-256 checksum manifest is published with the release assets
- GitHub build provenance attestations are generated for the published assets
- PyPI wheels are built in the same platform jobs as the GitHub Release
  archives, and the workflow checks that each wheel embeds the same `soldr`
  binary bytes as the matching release build output
- the npm package is a JavaScript launcher that downloads and verifies the
  matching GitHub Release archive, so npm installations use those same release
  binaries rather than a separate npm-specific build

## What It Does Not Guarantee Yet

The current release flow does not yet claim all of the following:

- SBOM publication
- independently reproduced builds by a second builder
- fully hermetic inputs for rustup, crates.io, OS packages, or third-party test inputs

The release-governance and hermetic-input follow-up items remain tracked in issues [#11](https://github.com/zackees/soldr/issues/11), [#41](https://github.com/zackees/soldr/issues/41), and [#42](https://github.com/zackees/soldr/issues/42). SBOM publication and independently reproduced builds are intentionally outside the current release claim.

## Release Asset Names

Current release assets follow this shape:

- `soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.zst`
- `soldr-vX.Y.Z-aarch64-unknown-linux-gnu.tar.zst`
- `soldr-vX.Y.Z-x86_64-unknown-linux-musl.tar.zst`
- `soldr-vX.Y.Z-aarch64-unknown-linux-musl.tar.zst`
- `soldr-vX.Y.Z-x86_64-apple-darwin.tar.zst`
- `soldr-vX.Y.Z-aarch64-apple-darwin.tar.zst`
- `soldr-vX.Y.Z-x86_64-pc-windows-msvc.tar.zst`
- `soldr-vX.Y.Z-aarch64-pc-windows-msvc.tar.zst`
- `soldr-vX.Y.Z-SHA256SUMS.txt`

Both macOS archives and wheels are cross-built on Linux through the blessed
Apple SDK path. Publication is gated on `smoke_macos_x64` and
`smoke_macos_arm64`, running on hosted `macos-15-intel` and `macos-15`.
Those jobs download the exact release artifacts, execute the shipped binaries,
install and import the shipped wheels, and smoke the daemon/cache route on
their matching native architectures. The Intel job also verifies the archive's
Mach-O x86_64 identity; the ARM64 job checks its corresponding architecture.
A Linux-side METADATA read is only a
preliminary check, not execution evidence.

`e2e_macos_x64_build` / `e2e_macos_x64_replay` (soldr#3078) also run at
release time as a separate Recovery-guest diagnostic. They cross-build
`x86_64-apple-darwin` at the release commit and replay the positively-owned
nextest partition with toolchain provisioning. The dedicated
`macos-recovery-replay.yml` workflow runs nightly against `main`, on
`workflow_dispatch`, and on pull requests labelled `macos-replay`; soldr#3116
moved it out of `ci.yml`, where it had set the run's wall clock (34-40 min of
a wedged guest) without a green result in 25 runs.

The Recovery replay is **advisory, not a publication gate** (soldr#3088).
Both native macOS artifact-smoke jobs and `smoke_windows` gate publication;
the exact-SHA full-CI coverage gate is separate. A red Recovery replay
should be investigated, but it cannot make a release unpublishable.
soldr#3088 tracks restoring the gate once the lane can stay green.

## Step 1: Verify The Checksum

Download the release artifact you want and the matching `SHA256SUMS` file.

On Linux or macOS:

```bash
sha256sum -c soldr-vX.Y.Z-SHA256SUMS.txt --ignore-missing
```

On Windows PowerShell:

```powershell
$expected = Select-String -Path soldr-vX.Y.Z-SHA256SUMS.txt -Pattern 'soldr-vX.Y.Z-x86_64-pc-windows-msvc.tar.zst' |
  ForEach-Object { ($_ -split '\s+')[0] }
$actual = (Get-FileHash .\soldr-vX.Y.Z-x86_64-pc-windows-msvc.tar.zst -Algorithm SHA256).Hash.ToLower()
if ($expected -ne $actual) { throw "checksum mismatch" }
```

The checksum step tells you the file you downloaded matches the checksum manifest attached to the release.

## Step 2: Verify The Artifact Attestation

Use GitHub CLI's attestation support to verify the artifact provenance.

This is the primary documented verification path for `soldr`:

```bash
gh attestation verify soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.zst \
  --repo zackees/soldr
```

For stricter identity validation, also pin the signer workflow:

```bash
gh attestation verify soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.zst \
  --repo zackees/soldr \
  --signer-workflow zackees/soldr/.github/workflows/auto-release.yml
```

This validates that GitHub has a matching attestation for the artifact and that the attestation was produced by the expected repository and workflow.

## Step 3: Understand What Was Verified

`gh attestation verify` validates:

- the artifact digest
- the GitHub repository identity
- the workflow identity if you provide `--signer-workflow`
- the provenance attestation type

It does not, by itself, prove that every external input used during the build was mirrored or hermetic. For the current trust boundary inventory, see [TRUST_BOUNDARIES.md](./TRUST_BOUNDARIES.md).

## Optional: Offline Verification And Sigstore-Compatible Bundles

GitHub CLI also supports downloading attestation bundles and verifying them offline.

Relevant commands:

```bash
gh attestation download soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.zst --repo zackees/soldr
gh attestation trusted-root
```

Offline verification is not the primary documented path for `soldr`, but it remains available if you want to archive bundles and trusted roots alongside release artifacts.

This is also the nearest current equivalent to a Sigstore-style workflow for this repository. We do not require users to install separate Sigstore tooling as part of the normal `soldr` verification story.

## About `gh release verify`

GitHub CLI also has `gh release verify`, which verifies release-level attestations.

Immutable releases are enabled for this repository (see `SECURITY.md`), but release-level attestation is not yet the documented path: the release attests the `SHA256SUMS` subject set, not the release object. Today, the repository's official verification path is:

1. checksum verification
2. artifact attestation verification with `gh attestation verify`
