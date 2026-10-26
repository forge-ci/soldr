#!/usr/bin/env python3
from __future__ import annotations

import argparse
import shutil
from pathlib import Path

HELPER_SCRIPT_PATHS = (
    Path(".github/actions/setup-soldr/resolve_setup.py"),
    Path(".github/actions/setup-soldr/ensure_rust_toolchain.py"),
    Path(".github/actions/setup-soldr/ensure_soldr.py"),
    Path(".github/actions/setup-soldr/install_tool_shims.py"),
    Path(".github/actions/setup-soldr/verify_soldr.py"),
    Path(".github/actions/setup-soldr/zccache_contract.py"),
    Path("contracts/zccache-runtime.v1.json"),
)

PUBLIC_README = """# setup-soldr

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
"""


def _module_repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def _write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _copy_file(source_root: Path, destination_root: Path, relative_path: Path) -> None:
    source_path = source_root / relative_path
    if not source_path.is_file():
        raise FileNotFoundError(f"Missing source file for export: {source_path}")
    destination_path = destination_root / relative_path
    destination_path.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source_path, destination_path)


def _strip_repo_input(action_text: str) -> str:
    output_lines: list[str] = []
    skipping_repo_input = False

    for line in action_text.splitlines():
        if skipping_repo_input:
            if line.startswith("  ") and not line.startswith("    "):
                skipping_repo_input = False
            else:
                continue

        if line == "  repo:":
            skipping_repo_input = True
            continue

        if "INPUT_REPO:" in line:
            continue

        output_lines.append(line)

    return "\n".join(output_lines) + "\n"


def render_public_action_yaml(source_root: Path) -> str:
    return _strip_repo_input((source_root / "action.yml").read_text(encoding="utf-8"))


def render_public_readme() -> str:
    return PUBLIC_README


def export_setup_soldr_bundle(source_root: Path, destination_root: Path) -> Path:
    source_root = source_root.resolve()
    destination_root = destination_root.expanduser().resolve()

    if destination_root == source_root:
        raise ValueError("Destination must not be the source repository root")

    destination_root.mkdir(parents=True, exist_ok=True)
    _write_text(destination_root / "action.yml", render_public_action_yaml(source_root))
    _write_text(destination_root / "README.md", render_public_readme())
    _copy_file(source_root, destination_root, Path("LICENSE"))

    for relative_path in HELPER_SCRIPT_PATHS:
        _copy_file(source_root, destination_root, relative_path)

    return destination_root


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Export the standalone public setup-soldr action bundle."
    )
    parser.add_argument(
        "destination",
        help="Directory to materialize as the future public action repository.",
    )
    parser.add_argument(
        "--source-root",
        default=str(_module_repo_root()),
        help="Source soldr repository root. Defaults to the current checkout.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    destination = export_setup_soldr_bundle(
        Path(args.source_root), Path(args.destination)
    )
    print(f"Exported setup-soldr bundle to {destination}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
