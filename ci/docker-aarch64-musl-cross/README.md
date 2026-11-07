# `ci/docker-aarch64-musl-cross/`

Local validation harness for soldr's `aarch64-unknown-linux-musl`
cross-compile lane. Mirrors what `release-auto.yml`'s Linux ARM64
(musl) job runs on `ubuntu-latest`.

**Per the 2026-06-28 user directive**: every soldr cross-compile
change that touches the release pipeline MUST be validated locally
in this docker before being pushed to GitHub. Catches issues like
the jemalloc atomics-detection failure that previously blocked
v0.7.60+ from shipping.

## Build + run

```bash
# Build the image (once)
docker build -f ci/docker-aarch64-musl-cross/Dockerfile \
    -t soldr-aarch64-musl-cross .

# Cross-compile soldr
docker run --rm -v "$PWD:/src" -w /src soldr-aarch64-musl-cross \
    bash ci/docker-aarch64-musl-cross/build.sh
```

`build.sh` produces `staging/soldr` and asserts via `file(1)` that
it's a valid `ELF 64-bit LSB ... aarch64` executable. The NO CHEATING
gate that proves the cross-compile actually worked.

## What this validates

* `aarch64-linux-musl-cross` from musl.cc is installed correctly
* `CC_aarch64_unknown_linux_musl` env var is honored by cc-rs
* `tikv-jemalloc-sys`'s build script can compile its bundled C source
  for the musl target (the v0.7.60+ release blocker)
* The pinned rustup toolchain (1.98.1) builds the soldr binary clean
* The output is a real arm64 ELF, not silently a host-arch binary

## When to invoke this

Before pushing ANY change to:

* `.github/workflows/release-auto.yml`'s Linux ARM64 (musl) lane
* `.github/workflows/_bootstrap-e2e.yml` musl env vars
* Crate dependencies that pull `tikv-jemalloc-sys` (including zccache)
* `Cargo.toml` workspace lints / profile settings

A green run here means the GHA equivalent should also pass.
