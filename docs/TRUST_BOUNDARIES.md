# Trust Boundaries

This document describes the current external trust boundaries for `soldr`.

It records both the factual inventory of what the project trusts today and the current repo policy for which of those trust edges are acceptable on the `0.5.x` line.

## Controlled Inputs

The repository directly controls:

- the source tree at the released commit SHA
- the Cargo dependency graph recorded in `Cargo.lock`
- the workflow definitions committed in `.github/workflows/`
- the action revisions pinned by full commit SHA in those workflows
- the exact pinned fixture commit used by the current bootstrap e2e workflow

## Release-Time External Dependencies

Even with a validated release workflow, the release path still depends on external systems:

- GitHub-hosted runners
  - The repo trusts the runner images provided by GitHub Actions.
- Rust toolchains from `rustup`
  - The workflows install Rust toolchains during CI and release.
- crates.io
  - The build downloads crate metadata and crate archives from the crates.io ecosystem.
  - Published crate versions are immutable, but the transport and index are still external.
- GitHub APIs and GitHub Releases
  - The release workflow uses GitHub services to create releases, publish assets, and store attestations.
- PyPI
  - Optional hardened wheel publication relies on PyPI's Trusted Publishing and package hosting.
  - The project already has an existing `soldr` PyPI record whose ownership and publisher settings live outside this repository.

## Audit: What The Published Release Artifacts Depend On

Based on the committed release workflow in `.github/workflows/auto-release.yml`:

- the published `soldr` release archives are built from the repository source tree plus Rust dependencies resolved through Cargo
- the workflow does not explicitly download and repackage third-party release binaries into the published `soldr` archives
- the release path still depends on external services and package sources such as GitHub-hosted runners, `rustup`, crates.io, and GitHub APIs
- the live `apt` install and pinned bootstrap fixture checkout are part of release-gating validation, not packaged release contents
- zccache is an exact crates.io dependency compiled into `soldr-daemon`; there is no managed zccache download or standalone zccache release asset

## E2E Validation External Dependencies

The bootstrap e2e jobs currently trust additional external systems:

- Ubuntu package repositories
  - musl validation jobs install `musl-tools` from live `apt` repositories
  - the install command uses `--no-install-recommends`, a retry loop, and logs the resolved `musl-tools` version for auditability; the set of installed packages is therefore the declared set, not an open-ended recommended expansion
- GitHub repository hosting for the bootstrap fixture
  - the e2e workflow checks out a pinned commit of `zackees/soldr` and builds `crates/soldr-cli/tests/fixtures/windows-msvc-default`
- third-party Rust toolchain installation
  - the e2e workflow reads the third-party project's `rust-toolchain.toml` and installs that toolchain through `rustup`

These inputs are pinned where possible, but they are not yet mirrored or fully hermetic.

## Hermetic Inputs: Final `0.5.x` Stance

Concrete narrowings in place:

- the `musl-tools` install uses `--no-install-recommends` so the Ubuntu package surface is exactly what the release path declares
- the `musl-tools` install logs its resolved version so the exact package contents are visible in workflow logs
- the `musl-tools` install retries on transient apt failures rather than either flapping or pulling unrelated extras

Explicitly out of scope for `0.5.x` (not planned; any revisit is scoped to a future `1.0.0-rc` hardening milestone):

- a `cargo vendor` or mirrored-registry strategy for crates.io resolution during release
- a mirrored or prebuilt-image strategy for Rust toolchain acquisition during release
- a mirror for the pinned bootstrap fixture checkout
- replacement of the live `apt` repository with a prebuilt image or pinned-package snapshot

The attested release artifact - built from the reviewed `main` commit that bumped the workspace version, published through the `release` environment, with GitHub build-provenance attestation and a `SHA256SUMS` manifest - is the user-facing trust guarantee for `0.5.x`. Narrowing the live CI input surface below that attestation-based guarantee is not a project goal on this line.

## Runtime Tool-Fetch Trust Boundaries

The current `soldr-fetch` implementation resolves tools using this live network chain:

1. local cache in `~/.soldr/bin`
2. crates.io API lookup for the crate's repository URL
3. GitHub release metadata for the crate's repository
4. GitHub release asset download for the selected platform archive
5. the crate's own `[package.metadata.binstall]` `pkg-url` (or, when it declares none, binstall's default `{ repo }/releases/download/...` guesses, #3735) for an exact version when step 3/4 misses or `SOLDR_RESOLVER_ORDER` excludes `api` (#3726)
6. QuickInstall (`cargo-bins/cargo-quickinstall` release assets) for an exact version when step 3/4 misses or `SOLDR_RESOLVER_ORDER` excludes `api` (#3700)

That means the runtime tool-fetch path currently trusts:

- crates.io metadata for crate existence and repository linkage
- GitHub repository ownership and release contents for the target tool
- HTTPS transport to those services
- the crate publisher's `[package.metadata.binstall]` metadata (read from the crates.io `.crate`) and whatever `https` host its `pkg-url` (or a default guess under its `package.repository`) names, for tools resolved through the binstall hop; soldr honours no publisher checksum there, so these installs are `trust: unverified` unless the user pins the asset. A declared minisign `signing` key is verified (see below), but that key comes from the same crate metadata, so it is not an independent trust root
- the cargo-quickinstall project's build and release pipeline, for tools resolved through the QuickInstall hop; QuickInstall publishes no checksum, so these installs are always `trust: unverified` unless the user pins the asset

Integrity enforcement that is in place today:

- SHA-256 is always computed on the downloaded archive and printed to stderr
- user-supplied per-asset SHA-256 pins from `SOLDR_CHECKSUMS_FILE` are enforced as hard errors on mismatch
- `SOLDR_TRUST_MODE=strict` refuses any fetch without a matching pin

It does not yet enforce:

- maintainer allowlists
- repo-committed default checksums for tools shipped in the `known_tools` registry
- upstream signatures
- upstream artifact attestations
- mirrored internal copies of third-party tool binaries

## Current Policy Decisions

Current repo policy is:

- pin what we can inside the git tree
- make external trust edges explicit in documentation
- treat floating workflow refs as unacceptable
- prefer exact commit or version selection where possible
- `0.5.x` does not claim hermetic builds
- the documented release-time dependencies on GitHub-hosted runners, `rustup`, crates.io, GitHub APIs/Releases, live `apt` in the bootstrap e2e path, and the pinned bootstrap fixture checkout are acceptable for `0.5.x`
- Cargo vendoring, toolchain mirroring, OS-package mirroring, and bootstrap fixture source mirroring are explicitly out of scope for `0.5.x`; any revisit is scoped to a future `1.0.0-rc` hardening milestone
- the pinned bootstrap fixture checkout is acceptable for current validation, but it must not be described as mirrored or hermetic
- release verification for `soldr` covers the published `soldr` artifacts and their provenance, not every external input used during CI

## Current Runtime Fetch Policy

As of the `0.6.x` line, `soldr` enforces integrity on every third-party fetch:

- every downloaded archive has its SHA-256 computed before extraction; the digest is printed to stderr so a human or CI log grep can audit what actually installed
- users can pin per-asset SHA-256 values in a TOML file at the path named by `SOLDR_CHECKSUMS_FILE`; any pin mismatch is a hard error regardless of mode
- `SOLDR_TRUST_MODE=strict` refuses to install any tool that does not have a matching pin
- `SOLDR_TRUST_MODE=permissive` (the default) installs and emits a `trust: unverified` warning when no pin is available; this preserves the convenience/bootstrap path while making the trust state legible
- Any unrecognised `SOLDR_TRUST_MODE` value (for example a typo such as `strcit`, or `1`) fails closed to `strict` with a warning (soldr#3681); only `permissive`, an empty value, or unset select permissive mode
- zccache is not fetched by the runtime tool resolver; Cargo verifies and builds the exact dependency recorded in `Cargo.lock`, and release archives contain only the embedded implementation
- Managed Zig (the linker driver behind blessed GNU-Linux `soldr build` and `soldr cargo zigbuild`) is pinned to `MANAGED_ZIG_VERSION` (0.14.1) and verified against built-in SHA-256 pins for every host asset, sourced from ziglang.org's `download/index.json`; a `SOLDR_CHECKSUMS_FILE` pin overrides the built-in one, a mismatch is always fatal, and `SOLDR_TRUST_MODE=strict` passes without a user pin file. Installs extract into a sibling staging directory and are promoted under a per-version install lock (soldr#3682).
- The QuickInstall hop (#3700) is a fallback after the GitHub Releases hop, for exact versions only, and is gated by the `quickinstall` token of `SOLDR_RESOLVER_ORDER`. QuickInstall publishes no checksum, so an install is `trust: unverified` unless `SOLDR_CHECKSUMS_FILE` pins its asset (`<crate>-<version>-<target>.tar.gz`); `SOLDR_TRUST_MODE=strict` refuses an unpinned QuickInstall asset, and a pin mismatch is always fatal, so the hop never bypasses a pin. The `[package.metadata.binstall]` hop (#3726) runs just before QuickInstall under the same conditions, gated by the `binstall` token. It renders `pkg-url` with only the documented binstall variables (unknown variables are refused), refuses any non-`https` `.crate` or `pkg-url` URL, and installs through the same pin/strict-mode path: `trust: unverified` unless `SOLDR_CHECKSUMS_FILE` pins the asset, refused under `SOLDR_TRUST_MODE=strict` without a pin, and fatal on a pin mismatch. When a crate declares no `pkg-url`, binstall's nine default `{ repo }/releases/download/...` URL guesses are HEAD-probed in order (every `pkg-fmt` unless one is declared, `tbz2` included), under the same `https`-only and pin/strict rules; a crate with no `package.repository` is a miss (#3735).

binstall minisign signing (#3735): when `[package.metadata.binstall.signing]` is declared, soldr enforces it fail-closed. The algorithm must be `minisign`, the `pubkey` must parse, the signature (from `signing.file`, a strict template that may use `{ url }`, else `<asset-url>.sig`, `https` only) must exist and decode, and the prehashed minisign signature must verify over the exact downloaded asset bytes before the pin/trust check and before extraction; legacy (non-prehashed) signatures are refused. Any of those failing aborts the install, even when `SOLDR_CHECKSUMS_FILE` pins the asset. A pass is logged as `trust: minisign-verified` and the install still goes through the normal pin/trust policy. **The public key is read from the crate's own published `Cargo.toml`, the same `.crate` that names the asset URL, so it is not an independent trust root**: a verified signature proves the asset was signed by whoever controls the crate's crates.io publication (it detects a tampered, swapped or compromised asset host), not that the publisher is trustworthy. For that reason a minisign pass is **not** a pin: the install is still reported `trust: unverified` unless `SOLDR_CHECKSUMS_FILE` pins it, and `SOLDR_TRUST_MODE=strict` still refuses it without a pin. Crates that declare no `signing` table are unaffected.
- `reld` (the polylinker) is fetched only when a project or `SOLDR_LINKER` selects it (`reld` or the `fast` default); the fetch is pinned to v0.1.0 (the static musl asset on Linux x64 hosts) and verified against built-in SHA-256 pins sourced from reld's own `SHA256SUMS`, going through the same trust verification as every other runtime fetch — `SOLDR_CHECKSUMS_FILE` and `SOLDR_TRUST_MODE=strict` both apply

Example pin file layout:

```toml
[[tool]]
tool = "cargo-nextest"
version = "0.9.100"
asset = "cargo-nextest-0.9.100-x86_64-pc-windows-msvc.zip"
sha256 = "0123...cdef"   # 64-char lowercase hex
```

What is still deferred:

- maintainer allowlists (limiting which crate/repo pairs `soldr` will even attempt to fetch)
- upstream signature or attestation verification
- mirrored internal copies of third-party tool binaries

Those remain acceptable future hardening work and are tracked on the issues linked below.

Open follow-up implementation issues:

- [#11](https://github.com/zackees/soldr/issues/11) for repository and release-governance settings

Previously tracked, now closed: [#41](https://github.com/zackees/soldr/issues/41) (reducing live external release inputs — closed as out of scope for `0.5.x`), [#42](https://github.com/zackees/soldr/issues/42) (fetched-binary trust enforcement — checksum enforcement landed in `0.6.x`).

## Practical Reading Of This Document

If you are deciding whether to trust a published `soldr` release:

- trust the validated GitHub workflow run and artifact attestation for the released commit
- separately evaluate whether the remaining external build inputs are acceptable for your threat model
- separately evaluate whether `soldr` fetching third-party tool binaries at runtime is acceptable for your threat model, because that remains an upstream trust decision on `0.5.x`

Those are related but distinct trust decisions.
