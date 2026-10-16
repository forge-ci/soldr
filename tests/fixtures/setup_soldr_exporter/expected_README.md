# setup-soldr

Public GitHub Action for installing one released `soldr` binary, provisioning the resolved Rust toolchain with `rustup`, and restoring a cacheable runner-local root for Soldr, Cargo, and rustup state.

This repository is intended to be generated from `zackees/soldr`. The source-of-truth contract and release process still live in `soldr` issue #137, `docs/SETUP_SOLDR_PUBLIC_ACTION.md`, and `contracts/zccache-runtime.v1.json`.

## Usage

### Linux

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build-linux:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: zackees/setup-soldr@v0
        with:
          cache: true
      - run: soldr cargo build --release
      - run: soldr cargo test
```

### macOS

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build-macos:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - uses: zackees/setup-soldr@v0
        with:
          cache: true
      - run: soldr cargo build --release
      - run: soldr cargo test
```

### Windows

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build-windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: zackees/setup-soldr@v0
        with:
          cache: true
      - run: soldr cargo build --release
      - run: soldr cargo test
```

## Inputs

| Input | Meaning |
|---|---|
| `version` | Soldr release tag or version to install. Empty means latest release. |
| `cache` | Restore and save the action-managed cache/state root. |
| `cache-dir` | Override the runner-local cache/state root. |
| `cache-key-suffix` | Optional escape hatch appended to the cache key. |
| `toolchain` | Explicit Rust toolchain channel override. |
| `toolchain-file` | Alternate toolchain file path when `toolchain` is empty. |
| `trust-mode` | Optional `SOLDR_TRUST_MODE` value. |
| `build-cache` | Restore and save the Soldr-owned zccache compilation artifact cache across runs. Default `true`; set to `false` to opt out. |
| `target-cache` | Deprecated no-op (soldr#2996). The target cache was removed; accepted for compatibility and ignored (a warning is printed when enabled). Default `false`. |
| `target-cache-mode` | Deprecated no-op (soldr#2996). Accepted for compatibility and ignored. Default `off`. |
| `target-dir` | Cargo target directory (retained for compatibility). |
| `tool-shims` | Optional PATH shim mode. Set to `cargo` to make later `cargo ...` steps run through `soldr cargo ...`; default `false`. |
| `native-cache` | Default-on native C/C++ compiler caching for build-script work (bundled SQLite, etc.). When `true` (the default), soldr wraps `CC` / `CXX` with zccache so cc-rs invocations hit the same cache as rustc. Set to `false` to write `SOLDR_NATIVE_CACHE=0` to the job env and skip native wrapping for later `soldr cargo ...` steps. `soldr --no-cache cargo ...` is the command-time kill-switch and overrides this input. |

## Outputs

| Output | Meaning |
|---|---|
| `soldr-path` | Installed Soldr binary path added to `PATH`. |
| `soldr-version` | Installed Soldr version reported by `soldr version --json`. |
| `cache-dir` | Action-managed runner-local cache/state root. |
| `cache-hit` | Whether the action restored an exact cache hit. |
| `build-cache-hit` | Whether the Soldr-owned zccache compilation cache was restored. Empty only when `build-cache` is disabled. |
| `target-cache-hit` | Deprecated no-op (soldr#2996). Always empty. |
| `target-cache-mode` | Deprecated no-op (soldr#2996). Always `off`. |
| `native-cache-enabled` | Effective native C/C++ compiler cache policy. `true` when soldr will wrap `CC` / `CXX` with zccache for later `soldr cargo ...` steps; `false` when the action wrote `SOLDR_NATIVE_CACHE=0` to the job env. Does not reflect command-time `soldr --no-cache cargo ...`. |
| `toolchain` | Exact Rust toolchain channel configured for the action. |
| `tool-shims-dir` | Directory containing generated tool shims when enabled. |

## Notes

- The action installs exactly one released `soldr` binary for the active runner target.
- The normal path provisions Rust with `rustup`, bootstrapping `rustup` when it is absent.
- The action rehydrates `SOLDR_CACHE_DIR`, `CARGO_HOME`, and `RUSTUP_HOME` under the selected cache root.
- The action restores the Soldr-owned zccache cache root by default so child branches can reuse parent-branch build state.
- Released archives are validated against the versioned zccache runtime contract before setup-soldr exports bundled-tool paths.
- The target cache was removed in soldr#2996; `target-cache` and `target-cache-mode` are accepted but ignored. `soldr cook` plus zccache own compiler caching, including `--test` harness compiles.
- The action exports `ZCCACHE_CACHE_DIR` to keep managed zccache artifact storage under `SOLDR_CACHE_DIR`.
- Native C/C++ compiler caching is on by default. Build-script work (bundled SQLite, ring, etc.) runs through zccache without any extra wiring. Set `native-cache: false` to opt out of just the native wrapping while keeping Rust caching intact, or use `soldr --no-cache cargo ...` at command time to disable both layers.
- `tool-shims: cargo` prepends a Cargo shim for existing workflows that cannot rewrite every `cargo ...` command to `soldr cargo ...`.
- A restored target directory is a Cargo fast path, not a guarantee: build scripts without precise `cargo:rerun-if-*` inputs can still be dirty on fresh checkouts because source mtimes differ.

## Development

Regenerate this repository bundle from the source repository with the exporter in `zackees/soldr`.
