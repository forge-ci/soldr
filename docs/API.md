# soldr API Reference

This file is the CLI reference for `soldr`.

For the product-level support contract about what counts as a supported external API, see [API_BOUNDARY.md](./API_BOUNDARY.md).

## Overview

soldr is a single front door for Rust tool execution and Rust builds.

It has four invocation modes:

1. `soldr cargo ...`
   Delegates to the real Cargo binary while routing cacheable compiler work to
   the zccache service embedded in `soldr-daemon`.
2. `soldr <tool> [args...]`
   Fetches and runs a Rust CLI tool binary.
3. `soldr rustc ...`
   Low-level passthrough wrapper mode for explicit `RUSTC_WRAPPER=soldr` usage.
4. `soldr cc ...` / `soldr c++ ...`
   Compile native source through a catalogue-backed compiler and sysroot.

The primary user experience is `soldr cargo ...`.

## Machine-Facing Support Level

Current support policy:

- the supported external integration surface is invoking the `soldr` executable through documented commands and flags
- the internal Rust crates are not a supported public API
- wrapper mode and internal environment variables are operational mechanics, not a general-purpose API contract
- human-oriented command output is not the stable machine-facing protocol
- the first stable machine-facing protocol is the JSON mode on selected commands documented below

---

## Invocation Modes

### Mode 1: Cargo Front Door

```bash
soldr cargo build --release
soldr cargo test
soldr cargo run -- --help
```

Behavior:

- Resolve Cargo and Rust tools through Soldr's front door. Host-owned binaries
  retain the caller's `CARGO_HOME` / `RUSTUP_HOME`; binaries from Soldr's
  managed toolchain receive the matching managed context.
- Fall back to `rustup which <tool>` when no matching binary is present in the
  selected toolchain location.
- Set `RUSTC_WRAPPER` to a compiler-named Soldr shim when caching is enabled.
- Register the exact `soldr-daemon` image and selected root with the singleton
  broker before Cargo starts. Only the broker places and starts that image.
- Route cacheable compiler work through the broker SESSION service to the zccache
  service compiled into `soldr-daemon`; no separate zccache binary or daemon
  is fetched.
- Start a per-build correlation session while keeping cache artifacts under
  the exact selected Soldr root.
- Delegate to Cargo with the exact flags the user passed.

Current cache behavior:

- caching is enabled by default for `soldr cargo ...`
- `--zccache=managed` and the legacy `--zccache=system` spelling are retained
  for CLI compatibility; both use the embedded runtime. To deliberately put an
  external compiler cache in Cargo's wrapper slot, use
  `SOLDR_RUSTC_WRAPPER=/path/to/zccache soldr cargo ...`.
- zccache integration currently targets Rust builds through the cargo front door
- session logs and archived reports live under
  `<soldr-root>/cache/zccache/history/`; the embedded service's versioned
  state and compile journal live under
  `<soldr-root>/cache/zccache/daemon-state/embedded-v1/v<VERSION>/`.
  `SOLDR_CACHE_DIR` moves this compiler-store boundary; `ZCCACHE_CACHE_DIR`
  affects auxiliary front-door/session and rustfmt state, not the
  service hosted by `soldr-daemon`. Soldr neither uses nor sweeps the
  standalone `~/.zccache` root
- toolchain binaries (`rustc`, `rustfmt`, `clippy-driver`, etc.) are resolved directly from `RUSTUP_HOME` / `CARGO_HOME` / `PATH` before any `rustup` call; `rustup which` is only used as a fallback when the direct probe fails. The sole exception is when `RUSTUP_TOOLCHAIN` is explicitly set to a non-empty value — in that case soldr skips the direct probe and asks `rustup` for the matching toolchain binary so the pinned channel always wins
- once a concrete Cargo or Rust tool has been resolved, Soldr propagates its managed `CARGO_HOME` / `RUSTUP_HOME` only when that binary came from the Soldr-managed toolchain. Host-owned binaries keep the caller's host toolchain context instead of being paired with Soldr's default-less managed Rustup home

This is the normal build entry point.

### Prefer a newer global Soldr

Projects can opt in to using an installed global Soldr when it is newer than
the checkout-local executable. Add this to the root manifest:

```toml
[workspace.metadata.soldr]
prefer_newer_global = true
```

Soldr resolves the first different `soldr` executable on `PATH`, probes its
`--version`, and delegates only when its SemVer version is strictly higher.
Failures to locate or probe a global executable leave the local invocation
unchanged. Explicit `soldr --as <version>` / `SOLDR_AS` pins remain
authoritative.

`[workspace.metadata.soldr]` (or `[package.metadata.soldr]`) also accepts a
`linker` field, which selects the linker the same way `SOLDR_LINKER` does (see
the `SOLDR_LINKER` entry in the environment variable table below for the
accepted values and the full precedence chain):

```toml
[workspace.metadata.soldr]
linker = "reld"
```

### Mode 2: Tool Fetcher

```bash
soldr <tool>[@<version>] [tool-args...]
```

Examples:

```bash
soldr maturin build --release
soldr cargo-dylint check
soldr rustfmt src/main.rs
soldr maturin@1.7.0 build
```

Rustfmt remains routed through Soldr. Because rustfmt recursively discovers
child modules that are absent from Cargo's explicit rustfmt argv, normal
recursive invocations always execute the real formatter. The embedded marker
shortcut is used only when the invocation explicitly sets
`skip_children=true`, making the source-file set complete and safe to cache.

Each cached rustfmt child holds one permit of the daemon's resident
compile-capacity lease for the duration of its run (soldr#2877), so a large
`cargo fmt` fan-out is bounded by the same effective `SOLDR_JOBS` /
`[jobs] max_parallel_compiles` capacity the compiler admission gate already
enforces, with no separate global job cap. The lease is best-effort: if the
daemon is unreachable, formatting proceeds unleased with a warning rather than
failing the format. Set `SOLDR_RUSTFMT_ADMISSION_TRACE=1` to print the
admission line for each lease acquire/release. Source files named explicitly
on the command line are snapshotted before the formatter runs and restored if
a failed formatter leaves any of them empty or unreadable.

Resolution order:

1. **Cargo verb shorthand** — if `<tool>` (with no `@<version>` suffix) is either a cargo subcommand soldr already prebuilds (`nextest`, `deny`, `audit`, ...) OR one of cargo's own first-party verbs (`build`, `test`, `clippy`, ...), the invocation is rewritten as `soldr cargo <tool> [args...]` and dispatched through the cargo front door. See [Cargo Verb Shorthand](#cargo-verb-shorthand) below for the full list.
2. Local cache in `~/.soldr/bin/`
3. crates.io repository lookup
4. GitHub Releases for that repository
5. The crate's own `[package.metadata.binstall]` metadata (#3726), for an exact version when the GitHub Releases hop misses or is excluded. soldr downloads `<base>/<crate>/<crate>-<version>.crate` (base `https://static.crates.io/crates`, override with `SOLDR_BINSTALL_CRATES_BASE_URL`), reads its `Cargo.toml`, applies `overrides.<target>`, and renders `pkg-url` / `bin-dir` strictly: only the documented binstall variables (`name`, `version`, `target`, `archive-format`/`format`, `archive-suffix`, `repo`, `binary-ext`, `target-family`, `target-arch`, `target-libc`, `target-vendor`, and `bin` in `bin-dir`) render; an unknown variable or malformed brace is an error. The `.crate` URL and rendered `pkg-url` must be `https`. `pkg-fmt` `tgz` (default), `txz`, `tzstd`, `tar`, `zip`, and `bin` are supported; `tbz2` is refused. A crate without a `pkg-url` is a miss (binstall's default URL guesses are not used). Binstall metadata gives soldr no checksum (minisign signatures are not verified), so these installs report `trust: unverified` unless `SOLDR_CHECKSUMS_FILE` pins the asset (keyed by the last URL segment); `SOLDR_TRUST_MODE=strict` refuses an unpinned asset, and a pin mismatch is always fatal.
6. QuickInstall (`https://github.com/cargo-bins/cargo-quickinstall/releases/download/<crate>-<version>/<crate>-<version>-<target>.tar.gz`) for an exact version when the GitHub Releases hop misses or is excluded (#3700). Override the base URL with `SOLDR_QUICKINSTALL_BASE_URL`. QuickInstall publishes no checksum, so these installs report `trust: unverified` unless `SOLDR_CHECKSUMS_FILE` pins the asset; `SOLDR_TRUST_MODE=strict` refuses an unpinned QuickInstall asset, and a pin mismatch is always fatal.

Current implementation note:

- a crate's own `[package.metadata.binstall]` `pkg-url` templates and the `cargo install` source-build fallback are planned behavior, not the current shipped fetch path (binstall metadata: #3726)

#### Cargo Verb Shorthand

When `soldr <verb> [args...]` is invoked and `<verb>` is not a frozen soldr built-in, the External arm rewrites the invocation as `soldr cargo <verb> [args...]` whenever `<verb>` matches one of two lists. The long form `soldr cargo <verb>` continues to work — the shorthand is purely additive.

**Routed cargo subcommands** (soldr prebuilds these via `KNOWN_TOOLS`):

`nextest`, `deny`, `audit`, `llvm-cov`, `udeps`, `semver-checks`, `expand`, `watch`, `chef`, `zigbuild`, `xwin`, `binstall`, `machete`

**Routed cargo built-in verbs** (cargo's first-party commands):

`build`, `test`, `check`, `run`, `bench`, `doc`, `fmt`, `clippy`, `tree`, `update`, `fix`, `add`, `remove`, `metadata`, `pkgid`, `search`, `vendor`, `yank`, `owner`, `login`, `logout`, `init`, `new`, `generate-lockfile`, `verify-project`, `locate-project`, `report`, `install`, `uninstall`, `publish`, `package`, `info`

**Collision policy.** These three verbs MUST stay anchored to their soldr-native meaning and are explicitly excluded from the shorthand:

| Verb       | soldr-native meaning                              | Cargo equivalent (use long form) |
|------------|---------------------------------------------------|-----------------------------------|
| `clean`    | Clear the embedded zccache build cache            | `soldr cargo clean`               |
| `config`   | Show or set soldr configuration                   | `soldr cargo config` (unstable)   |
| `version`  | Print soldr's version                             | `soldr cargo --version`           |

The borderline case `install`: bare `soldr install <crate>` routes to
`cargo install <crate>`. The former standalone `soldr install-zccache`
surface was removed when zccache became a compiled-in service.

**Prebuilt release assets (soldr#3697).** When `soldr install <github-url>`
selects a GitHub release (bare `--release`, `--release <tag|~N>`, a release
URL, or the smart default "latest release"), soldr looks up that release's
assets and picks the one matching the target triple with the same matcher the
tool fetcher uses. A match is downloaded, sha256-verified, extracted into the
locked tool cache (`~/.soldr/bin/install-<name>-<tag>/`), and placed on PATH
with no compile. Integrity: GitHub's published asset `digest` is a hard pin (a
mismatch refuses the install in every mode); without one,
`SOLDR_CHECKSUMS_FILE` / `SOLDR_TRUST_MODE` apply as for any fetch, so
`SOLDR_TRUST_MODE=strict` refuses an unpinned asset. When no asset matches, or
the lookup fails, the default form falls back to a source build; `--prebuilt`
turns both into an error and `--build` skips the lookup entirely. A
verification or download failure of a chosen asset is never silently replaced
by a source build.

**Version pinning skips the shorthand.** Cargo built-in verbs and registered cargo subcommands cannot be version-pinned via the bare `@<version>` form — the cargo front door has no per-invocation version knob. `soldr build@1.0` keeps the existing External fetch path (and errors with "no crate named build"), and so do the registered subcommands (`soldr nextest@0.9.x` falls through to External). For pinned cargo-subcommand versions, use the soldr registry (`KNOWN_TOOLS::pinned_version` in source); for pinned tool fetches, use the External path for crates that actually exist.

```bash
# Shorthand
soldr build --release          # == soldr cargo build --release
soldr test --workspace         # == soldr cargo test --workspace
soldr clippy -- -D warnings    # == soldr cargo clippy -- -D warnings
soldr fmt --all -- --check     # == soldr cargo fmt --all -- --check
soldr nextest run              # == soldr cargo nextest run
soldr zigbuild build --target ...  # == soldr cargo zigbuild build --target ...

# Long form (always works as the escape hatch)
soldr cargo clean              # explicitly route `clean` to cargo (NOT soldr's cache clean)
soldr cargo config get profile.dev
```

### PEP 517 Build Backend

soldr ships a PEP 517 build backend (`src/soldr/__init__.py` in the
wheel), so Rust+Python packages can build through soldr instead of
raw maturin:

```toml
[build-system]
requires = ["soldr"]
build-backend = "soldr"
```

The existing `[tool.maturin]` configuration is honored unchanged — the
backend delegates to `soldr maturin pep517 <hook>` with a pinned
maturin. The dispatch pins the child's toolchain before exec:

| Pin | Effect |
|---|---|
| `CARGO` / `RUSTC` | rustup-resolved cargo + its sibling rustc — `rust-toolchain.toml` wins over PATH-shadowing standalones (chocolatey/scoop GNU installs). |
| `CARGO_BUILD_TARGET` | runtime MSVC-default triple on Windows (same policy as the cargo front door). |
| `CMAKE` / `CMAKE_GENERATOR=Ninja` | managed cmake + ninja from the soldr toolchain archive for cmake-based `*-sys` crates. |
| `RUSTC_WRAPPER=soldr` | compilation caching. The Python backend sets this before calling `soldr maturin pep517`; direct `soldr maturin build` / `develop` auto-inject it when cache is enabled and `RUSTC_WRAPPER` is unset. |

For a project whose packaging logic is not maturin, soldr can wrap another
PEP 517/660 backend while keeping the same managed environment:

```toml
[build-system]
requires = ["soldr", "setuptools>=64"]
build-backend = "soldr"

[tool.soldr.pep517]
delegate-backend = "setuptools.build_meta"
```

All standard wheel, editable, metadata, sdist, and dynamic-requirements hooks
are forwarded to the delegate. The delegate runs with soldr's target/profile/
linker/cache environment, and the caller environment is restored after each
hook. Maturin remains the default when `delegate-backend` is absent; recursive
delegation back to `soldr` is rejected.

The `profile`, `--profile`, and (for editable hooks) `editable-profile` PEP
config settings are also applied to delegated builds. For example,
`pip install . --config-settings profile=release` selects an explicit release
profile; without an explicit setting, the fast local `dev` policy remains in
effect.

#### Bundling Cargo bins into a PyO3 wheel (soldr#3239)

maturin builds one artifact kind per wheel — a PyO3 extension or a
`bindings = "bin"` executable — so a project that needs both declares its
extra bins instead of writing a delegate:

```toml
[tool.soldr.pep517]
bundle-bins = [
  { bin = "mytool", package = "mytool" },
  { bin = "helper", dest = "platlib/mypkg/_bin" },
]
```

| Key | Required | Meaning |
|---|---|---|
| `bin` | yes | Cargo `[[bin]]` target name (`--bin`). |
| `package` | no | Cargo package that owns the bin (`--package`). |
| `dest` | no | `<scheme>[/<subdir>]`; scheme is `scripts` (default), `platlib`, `purelib`, `data`, or `headers`. |

After maturin writes the wheel (or editable wheel), each entry runs
`soldr build --bin <bin> --message-format=json-render-diagnostics` with the
backend's prepared environment, the `[tool.maturin] manifest-path`, the PEP
`--target`, and the profile maturin used (maturin's own default is `--release`
for wheels and `dev` for editables). Host builds also receive
`PYO3_PYTHON=<frontend interpreter>` unless the caller set it. The executable
Cargo reports is added with mode `0755` at its scheme path and `RECORD` is
regenerated; a path that already exists in the wheel is refused. Shared
libraries the bin links are not repaired. Combining `bundle-bins` with
`delegate-backend` is an error.

For local PEP 517 wheel and editable builds, the backend defaults to Cargo's
`dev` profile and sets `opt-level = 0`, `codegen-units = 256`,
`debug = "line-tables-only"`, `lto = false`, and `incremental = true` for
every field the project has not explicitly configured in `profile.dev`. This
keeps manual installs fast; release pipelines should continue to select an
explicit release profile. A project's
`[tool.maturin] profile` / `editable-profile`, PEP config settings, or the
`SOLDR_PEP517_PROFILE` environment variable are authoritative. Set
`SOLDR_PEP517_PROFILE=none` to preserve maturin's profile selection entirely.

On every successful wheel or editable build, the backend writes a concise
timing and cache summary to stderr. `SOLDR_PEP517_STATS=off` disables it;
`SOLDR_PEP517_STATS=full` prints the complete cache-session payload as a
second stderr line. Soldr also selects `full` when `PIP_VERBOSE` or
`UV_VERBOSE` is set by the frontend or caller.

### PEP 517 callers with a wall-clock timeout

The compile-reply backstop defaults to 30 minutes so a legitimate large
release compile is not cut off. A hook, editor, or CI harness with a shorter
wall-clock timeout should set `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` to a smaller
value than its own timeout before invoking the PEP 517 frontend. That makes
Soldr report an actionable daemon-timeout diagnostic before the harness kills
the backend; it does not change the default for ordinary `pip` or `uv` builds.

```bash
# A five-minute hook timeout: leave time for Soldr to report the failure.
SOLDR_COMPILE_REPLY_TIMEOUT_SECS=240 uv build
```

If a harness sends `SIGTERM`, `SIGBREAK`, or `SIGINT` first, the backend also
names the interrupted command and elapsed time. A hard process kill cannot be
intercepted, so the explicit timeout remains the reliable diagnostic boundary.

For wheel and editable hooks, soldr keeps the last successful artifact under
`<effective-soldr-root>/pep517/wheels/`. It performs a metadata-only recursive scan of the
project (relative path, size, and modification time), staged files included,
and also hashes supplied prepared metadata by content. If that fingerprint and
the build settings match, soldr hardlinks the cached wheel into the frontend's
output directory (copying only when hardlinks are unavailable) and skips the
backend's packaging/compression work. Set `SOLDR_PEP517_WHEEL_CACHE=off` to
disable reuse.

The local PEP 517 path also sets `SOLDR_PEP517_LINKER=auto` by default. Soldr
tries the fastest supported linker for the active target, retries once with
the platform linker only for a linker-availability failure, and records that
successful fallback in versioned state under the soldr cache. Equivalent later
builds skip the failed candidate and warn that the standard linker was
previously verified. Set `SOLDR_PEP517_LINKER=none` to disable the automatic
policy. A user-specified `SOLDR_LINKER=fast` remains explicit and never
silently falls back; it emits an actionable warning instead. Target-specific
linker or rustflags settings from the command environment or project
`.cargo/config.toml` retain precedence.
Every pin defers to a pre-set user env var. Maturin acquisition is a
ladder controlled by `SOLDR_MATURIN_PROVISIONER` (`auto` default:
pinned prebuilt binary from GitHub Releases, falling back to the PyPI
maturin wheel provisioned into an isolated uv-managed env under
`~/.soldr/bin/maturin-uv-<ver>/`; `binary` and `uv` force one rung).
Direct `soldr maturin ...` preserves caller-provided `CARGO`, `RUSTC`,
and `RUSTC_WRAPPER`; `SOLDR_RUSTC_WRAPPER` only controls Soldr's
auto-injected wrapper when `RUSTC_WRAPPER` is unset.

Target resolution is shared by direct maturin and the PEP 517 backend,
in this precedence order: an explicit `--target` argument (including
PEP 517 config settings named `target`, `--target`, or `build-target`),
`CARGO_BUILD_TARGET`, `[tool.maturin].target`, then the host triple.
Before maturin starts, soldr applies the same target OS SDK preparation
used by `soldr build`, target clippy, and target test compilation. The Python
backend and the child build also share the same `CARGO_TARGET_DIR`.

`soldr env --target <alias-or-triple> --json` includes an additive
`target_plan` object (`schema_version: 1`) for setup-soldr and other tooling.
It reports the canonical target and alias, stable toolchain family, concrete
compiler/linker/archiver programs, SDK/sysroot provider and cache identity,
sorted environment-key contract, and supported operations. Consumers should
invoke the soldr operation rather than reconstructing those environment
values themselves.

PyO3 configuration is resolved separately from the target OS SDK. Soldr
reads workspace dependency metadata and only injects
`PYO3_NO_PYTHON=1` for a proven cross-compiled ABI3 extension.
PyO3-free projects receive no Python variables; modern PyO3 Windows
extensions use raw-dylib without a Python import library; Unix/macOS
extensions keep PyO3/maturin's normal dynamic extension behavior.
Embedding, legacy, ambiguous, and non-ABI3 builds are never guessed to
be ABI3. Caller-provided `PYO3_*` values always win. `soldr env --target
... --json` includes the resolved `pyo3_plan`.

Set `SOLDR_PYO3_COMPATIBILITY=sysroot` to opt an older PyO3 or embedded
Python cross-build into managed target-Python assets. Only this plan mode
consults the Python rows in the toolchain catalogue. Soldr selects the
newest published version for the target, verifies its catalogue SHA-256,
and exports `PYO3_CROSS`, `PYO3_CROSS_LIB_DIR`,
`PYO3_CROSS_PYTHON_VERSION`, and `PYO3_CROSS_PYTHON_IMPLEMENTATION`.
`SOLDR_PYTHON_VERSION=X.Y.Z` requests an exact published version.
`SOLDR_SYSLIB_ASSET_ORIGIN` overrides the asset origin for mirrors and
controlled integration tests; the default remains the soldr-toolchain
assets branch.

This split follows PyO3's own cross-compilation contract: target Python is
optional for ABI3 extensions, `PYO3_CROSS_LIB_DIR` is only needed when the
output must link libpython or consume target interpreter configuration, and
Windows uses Rust `raw-dylib` linking without an import library. See the
[PyO3 0.29 cross-compilation guide](https://pyo3.rs/v0.29.0/building-and-distribution.html#cross-compiling)
and [raw-dylib configuration](https://pyo3.rs/main/building-and-distribution.html#the-pyo3_use_raw_dylib-environment-variable).

The backend also pins `CARGO_TARGET_DIR` to a stable per-project path under
`<effective-soldr-root>/cargo-target/pep517/<project-id>` so PEP 517 isolated builds
(pip/uv copy the sdist to a throwaway temp dir, discarding `target/`
after every build) keep Cargo's incremental cache hot across invocations.
The project ID is a content identity over build configuration (not Rust
source); Cargo remains authoritative for source freshness. This keeps
temporary source directories warm without sharing target state between
unrelated projects. `SOLDR_PEP517_PROJECT_ID` exposes the identity to soldr
diagnostics and cache keys. A caller-provided `CARGO_TARGET_DIR` always wins,
and `SOLDR_PEP517_STABLE_TARGET_DIR=0` disables the pin entirely. The effective
root is `SOLDR_CACHE_DIR` when set; otherwise the backend queries the selected
`soldr version --json` binary for its provenance-aware production (`.soldr`) or
development (`.soldr-dev`) root, then forwards that exact root to child hooks.
The owning daemon removes both PEP517 target namespaces and wheel-cache
namespaces after 30 days, or once they are older than 72 hours while the
root/volume is under pressure. Active build leases defer the pass.

### Mode 3: Internal Wrapper Mode

Wrapper mode is entered when Cargo invokes soldr as the configured `RUSTC_WRAPPER`.

Typical shape:

```text
soldr /path/to/rustc --crate-name foo ...
```

In this mode, soldr should act as the transparent build-assistance layer around `rustc`.

Current implementation status:

- Wrapper mode still transparently resolves the real `rustc`, preferring direct binaries before `rustup`
- The normal cache-enabled build path now runs through soldr wrapper mode and delegates into managed `zccache`
- If caching is disabled, wrapper mode falls through to real `rustc` without zccache involvement

---

## Mode Detection

When soldr starts, it decides its mode in this order:

1. If `argv[1]` looks like `rustc` or a path to `rustc`, enter wrapper mode.
2. Otherwise, parse CLI commands with Clap.
3. `cargo` is a first-class built-in subcommand.
4. Any other first argument falls through to the External arm, which itself tries dispatch in order: cargo verb shorthand (see [Cargo Verb Shorthand](#cargo-verb-shorthand)) first; if no match, treat the argument as a tool name to fetch and run.

---

## Built-in Commands

### `soldr cc` / `soldr c++` (soldr#2335)

Compile and link C or C++ with a verified compiler/sysroot bundle from the
soldr-toolchain catalogue:

```bash
soldr cc --target x86_64-linux-gnu.2.17 hello.c -o hello
soldr c++ --target linux-x64 main.cpp -o app
```

`--target` accepts the regular friendly aliases and Rust triples and defaults
to `host`. The supported standalone targets are GNU/Linux, musl/Linux, and
`x86_64-pc-windows-gnu`; unsupported host/target pairs fail explicitly.
GNU/Linux uses the catalogue's glibc 2.17 sysroot whether the suffix is
explicit or omitted.

Arguments not consumed by the front door are forwarded verbatim to the
compiler. Exactly one of these query flags may be used instead of compiler
arguments:

- `--print-cc`
- `--print-cxx`
- `--print-ar`
- `--print-linker`

The query output is only the prepared executable path. Invoke the `soldr cc`
or `soldr c++` wrapper when the target's required sysroot arguments must be
applied. CMake supports that shape through its `CC` and `CXX` environment
variables:

```bash
CC="soldr cc --target x86_64-linux-gnu.2.17" \
CXX="soldr c++ --target x86_64-linux-gnu.2.17" \
  cmake -S . -B build
cmake --build build
```

### `soldr wheel` (soldr#2139)

Build an **abi3** Python wheel through soldr's blessed toolchain:

```bash
soldr wheel                                        # quick DEV wheel, host target
soldr wheel --release                              # release wheel, host target (glibc 2.17 on *-linux-gnu)
soldr wheel --release --host-glibc                 # release wheel against THIS host's glibc
soldr wheel --release --target aarch64-unknown-linux-gnu   # release cross wheel
soldr wheel --release --target linux-arm64         # friendly aliases resolve identically
soldr wheel --release --target mac-arm64 --out dist # later arguments reach maturin
```

`--release` is **opt-in**, matching `cargo` and `soldr build`: a bare
`soldr wheel` builds the dev profile, which is what you want while iterating.
A release profile forwarded to maturin -- `-r`, `--release`, `--profile release`, or `--profile=release` -- gets exactly the same release policy as `soldr wheel --release` (glibc 2.17 sysroot and `manylinux_2_17` tag on `*-linux-gnu`, release-profile bundled bins), and combining one with `--debug` is refused (soldr#3636).
`--target` is optional and defaults to the host triple. `--target`,
`--release` and `--host-glibc` must come before any forwarded maturin
arguments.

`soldr wheel` resolves the target (same alias table as `soldr build`), prepares
the sysroot and toolchain environment, provisions maturin, and delegates to the
existing `soldr maturin build` execution path. Wheel *naming* is unchanged;
that contract is maturin's.

`soldr wheel` honours `[tool.soldr.pep517] bundle-bins` (zackees/soldr#3468)
through the same implementation as the PEP 517 backend: after maturin writes
the wheel, soldr runs its embedded `_bundle_bins.py stage` with a Python
interpreter (`$PYO3_PYTHON`, then `python3`, then `python`; needed only when
`bundle-bins` is declared). Each bin is built with `soldr build --bin <bin>`,
the `[tool.maturin] manifest-path`, the wheel's profile, and — whenever the
maturin build prepared its target (any cross build, or a host-target release
`*-linux-gnu` wheel) — `--target <triple>`, so a bundled CLI gets the same
catalogue toolchain and glibc 2.17 floor as the extension. A failed bin build
fails `soldr wheel`.

#### Platform tags: soldr only claims a floor it enforced

| invocation | target preparation | `--compatibility` passed to maturin |
| --- | --- | --- |
| `soldr wheel --release` for any `*-linux-gnu` target, **host target included** (soldr#3432) | always: catalogue glibc-2.17 toolchain + sysroot | `manylinux_2_17` |
| `soldr wheel --release --host-glibc` (host-target `*-linux-gnu` only) | none: links this host's glibc | `pypi` |
| `soldr wheel --release --target <cross *-linux-musl>` | catalogue musl toolchain | `musllinux_1_2` |
| `soldr wheel --release` for a host-target `*-linux-musl` build | none | `pypi` |
| any dev-profile build (no `--release`) | cross targets only | `pypi` |
| any non-Linux target | cross targets only | `pypi` |

`pypi` is maturin's pseudo-option for "derive the platform tag from the bytes,
then validate the filename for PyPI" — a description of the artifact rather
than a promise about it.

After soldr#3432, "release + linux-gnu" always means an enforced 2.17 floor.
Target preparation (`target_lifecycle::prepare_for_invocation`) is what mounts
the catalogue sysroot that *creates* that floor. The maturin path runs it on
its own only when the target differs from the host, so before soldr#3432 a
host-target release wheel linked the machine's own glibc — 2.39 on
ubuntu-24.04 — and was tagged from the bytes (`manylinux_2_34` or higher).
That was an honest tag but the wrong artifact: a wheel for PyPI must not
silently inherit the build machine's glibc floor. `soldr wheel --release` now
asks the maturin path to prepare the host target too, so on an x86_64 Linux
host `soldr wheel --release` produces
`*-manylinux_2_17_x86_64.manylinux2014_x86_64.whl`.

**The floor is never a silent choice.** Before the build starts, soldr prints
one `info` line on stderr, green on a terminal (plain text when `NO_COLOR` is
set or stderr is not a TTY; nothing is written to stdout):

```text
soldr: info: building release wheel against glibc 2.17 (manylinux_2_17) for maximum Linux compatibility; pass --host-glibc to link against this host's glibc instead
```

A cross `*-linux-gnu` release build prints the same line naming the target,
without the `--host-glibc` hint, because the flag is refused there.

**`--host-glibc` opts out.** For a host-target `*-linux-gnu` build it skips
the forced preparation, links against this host's glibc, and claims only what
that enforces: `--compatibility pypi`, so maturin tags the wheel from its
bytes. It prints its own `info` line with the version soldr read from
`gnu_get_libc_version(3)`:

```text
soldr: info: --host-glibc: building wheel against this host's glibc, not glibc 2.17; it will require glibc 2.39 or newer, and maturin tags it from its bytes instead of manylinux_2_17
```

It is never the default. It is refused with a cross target (the wheel is not
linked against this host's libraries at all) and with a non-glibc target
(macOS, Windows, musl).

**Native aarch64 Linux hosts.** Every catalogue GNU bundle is hosted on
`x86_64-unknown-linux-gnu` (soldr#2874), so a native aarch64 host cannot run
the toolchain that enforces the floor. A host-target release wheel there is
**refused**, naming both remedies: build on an x86_64 Linux host, where
`soldr wheel --release --target aarch64-unknown-linux-gnu` cross-builds the
wheel at glibc 2.17, or pass `--host-glibc` to accept this host's glibc floor
explicitly. Falling back silently would repeat the bug soldr#3432 fixed.

**musl is unchanged.** A host-target musl release wheel keeps the `pypi`
tag; soldr#3432's host-target preparation deliberately covers GNU only
(soldr#3435). The catalogue musl bundle's compilers are static i386 ELFs, so
they run on any x86_64 Linux host, musl included. The GNU bundle's compilers
are glibc-dynamic: on a host whose OS libc is musl, anything that needs the GNU
bundle is refused with a message naming the host libc and glibc's loader
(soldr#3435).

`.github/scripts/verify_wheel_glibc.py` checks the claim against the bytes —
both `*.data/scripts/*` executables and `*.so` extension modules — because pip
acts on tags: it installs a wheel tagged `manylinux_2_17` on an old host, and a
binary that really needs GLIBC_2.39 then dies with
``version `GLIBC_2.39' not found``. The `wheel-cross-verify` CI lane runs the
whole path end to end, for a cross target and for the x86_64 host target.

(maturin does not silently downgrade an explicit tag: with
`--compatibility manylinux_2_17` and an ELF needing GLIBC_2.39 its auditwheel
implementation fails the build with "Error ensuring manylinux_2_17
compliance". It auto-selects the highest satisfied policy only when no tag was
requested.)

Scope notes:

- **abi3 only — and specifically `abi3-py310`; see "PyO3 ABI policy" below.**
  A non-abi3 extension module needs a CPython built for the target, not just
  a sysroot. When soldr cannot place the build in an interpreter-free mode it
  refuses and names `soldr maturin build` as the escape hatch, rather than
  quietly building against the host's Python.
- **No glibc-floor suffix.** `--target <triple>.<major>.<minor>` (soldr#2202)
  is rejected here. A floor is a request to zig, never a guarantee — the
  effective floor is also bounded by every symbol the vendored C dependencies
  reference — so folding it into a manylinux tag would publish a promise soldr
  cannot keep.
- Pass `--target` once, before any forwarded maturin arguments. A second
  `--target` in the passthrough is refused rather than silently disagreeing
  with the sysroot soldr prepared.
- `--release` together with a forwarded `--debug` is refused: those are two
  different profiles, and picking one would build something you did not ask
  for.

#### PyO3 ABI policy: `abi3-py310`, always

Every PyO3 extension built through soldr — soldr's own `soldr._native` and
every downstream repo in the fleet (fbuild, zccache, running-process, bosn) —
declares exactly this:

```toml
[dependencies]
pyo3 = { version = "0.29", features = ["abi3-py310"] }
```

This is a fleet-wide policy, not a per-repo choice, and it is the only ABI
`soldr wheel` supports directly:

- **`abi3-py310` is the compatibility floor.** One `cp310-abi3-<platform>`
  wheel installs on CPython 3.10 and every later release. A higher floor
  (`abi3-py311`, `abi3-py312`) buys nothing soldr needs and drops every 3.10
  host. A version-specific extension (no `abi3-*` feature; tag
  `cp3xx-cp3xx-<platform>`) needs one wheel per interpreter *and* a CPython
  built for the target when cross-compiling — which is the exact case the
  abi3 refusal above exists to catch.
- **It is interpreter-free, so it cross-compiles.** With `abi3-py310` the
  PyO3 planner (`crates/soldr-cli/src/pyo3_detect.rs`) places the build in
  `Abi3NoPython` mode: `PYO3_NO_PYTHON=1`, no `PYO3_CROSS_LIB_DIR`, no
  `PYO3_CROSS_PYTHON_VERSION`. That is what lets `soldr wheel --release
  --target x86_64-apple-darwin` (or any other cross triple) run on a Linux
  host with only the sysroot/SDK soldr prepared. `ci/docker-pyo3-policy/`
  builds its fixture extension with this feature for both Apple triples to
  keep the mode honest.
- **It matches soldr's own floor.** soldr's PyPI wrapper requires Python
  >=3.10 (see `CLAUDE.md` "Toolchain"), so the ABI floor and the package
  floor agree.

Do not derive the ABI floor from a downstream repo's `requires-python`. A
repo that declares `requires-python = ">=3.11"` still builds `abi3-py310`
(a `cp310-abi3` wheel installs fine on 3.11+); the package floor and the
ABI floor are different knobs, and only the second one is soldr's concern.
Reference implementations: `fbuild/Cargo.toml` (`abi3-py310`, cross-built for
both Apple triples on Linux with `PYO3_NO_PYTHON=1 soldr build --target …`),
`zccache/crates/*-py/Cargo.toml`, `running-process/Cargo.toml`.

### `soldr cargo`

Run Cargo through soldr's front door.

### Stable `-Zthreads` fallback

When a stable compiler rejects exactly `-Zthreads=<N>` with rustc's nightly-only
diagnostic, the Cargo front door retries once without that flag. The retry is a
fresh `soldr cargo` invocation, so its effective flags and cache/session plan
are rebuilt; artifacts from the failed flag set are never treated as
equivalent. Soldr emits a warning containing the removed value and notes that
the build may be slower. GitHub Actions receives the warning in
`::warning::` form, while supported local terminals render it in yellow.

This fallback is intentionally limited to one removable flag. It applies only
when `-Zthreads=<N>` comes from `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, or a
supported `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` environment variable, with no other
`-Z` flags present. It never sets or changes `RUSTC_BOOTSTRAP`, never retries a
normal compilation failure, and never retries a nightly invocation. If the
flag comes only from Cargo configuration, Soldr leaves the original failure in
place and asks the caller to remove it or provide it through one of the
supported environment variables.

```bash
soldr cargo build --release
soldr cargo test --workspace
soldr cargo check -p soldr-cli
```

For cross-target builds (`soldr cargo --target ...`), the target's Rust standard library must be provisioned separately — see the [native vs cross targets](../README.md#native-vs-cross-targets) section of the README.

### `soldr lint`

Run the repository validation suites through Soldr. The default and `rust`
suites run formatting, Clippy, and Dylint with one canonical workspace scope:

```bash
soldr lint
soldr lint rust --package soldr-cli
soldr lint deps
soldr lint ci
soldr lint ci --format json
soldr lint all
soldr lint --target x86_64-pc-windows-msvc
soldr lint --target win-x64 --target mac-arm64
soldr lint --host-only
```

`deps` runs `deny check`, `audit`, and `machete` concurrently as cache-disabled
children because they do not compile Rust. `ci` runs the CI/build-surface policy
suite (see [`soldr lint ci`](#soldr-lint-ci) below). `all` runs the `ci` suite
first, then adds `--all-features`, `udeps`, and `semver-checks` after the standard
Rust and dependency suites. Compiler-bearing steps stay on the regular Soldr cache
lifecycle; `cargo-dylint` is fetched from its Linux GNU release asset or
source-built from the pinned registry version on Windows and macOS.

**Cross-target Clippy (soldr#3378).** `lint rust` and `lint all` run Clippy once
for the host, then once more per Rust target triple declared in
`[workspace.metadata.soldr].targets` (`[package.metadata.soldr]` fallback for
single-crate repos) — a declared target is treated as a claim that the workspace
supports that platform, so `#[cfg(windows)]` / `#[cfg(target_os = "macos")]` code
gets type-checked on every `soldr lint`, on any host, instead of only in a
platform-specific CI lane. A workspace that declares no targets keeps today's
host-only behavior unchanged. Each cross Clippy pass runs `cargo clippy --target
<triple>`, so it lands under Cargo's own `target/<triple>/` and never touches the
host tree's fingerprints; the target's standard library is installed on demand
through Soldr's managed `rustup target add` (idempotent, never a bare `rustup`).
Dylint runs once for the host by default. `--cross-dylint` adds a custom-lint
pass for each selected target using the lint libraries' exact pinned nightly.
Soldr installs that nightly's `rust-std` for each target on demand before the
workspace check; the catalogued driver remains prebuilt. A failed cross pass
names the target and tool (`clippy` or `dylint`) and returns its exit code.

`--target <triple>` (repeatable; friendly aliases such as `win-x64`/`mac-arm64`
are accepted) overrides the declared list for that invocation. `--host-only`
skips cross-target Clippy entirely, even when targets are declared. It cannot
be combined with `--cross-dylint`. All three flags
are valid only for the `rust`/`all` suites and are stripped from the cargo scope
before it reaches fmt/Clippy/Dylint. An unknown or unsupported triple — declared
or explicit — is a clear error raised before any compile starts, not a silent
skip; the host's own triple is never run twice.

### `soldr ci-test`

Run Soldr's prescribed validation DAG with maximum artifact sharing inside
each compatible compiler domain:

```bash
soldr ci-test
soldr ci-test --package soldr-cli --features feature-a,feature-b
soldr ci-test --explain-plan
soldr ci-test --explain-plan --format json
soldr ci-test --no-run --target aarch64-apple-darwin
```

By default, the stable host chain runs formatting and `soldr lint ci`, Clippy, exactly one
Nextest test-profile build/run, and doctests. After Clippy, Nextest overlaps the
serial Dylint libraries -> workspace analysis -> UI-test branch. Doctests start
only after both branches complete. `soldr cargo check` is not run because
Clippy subsumes the same workspace/all-targets host scope. Dependency policy
then fans out across `soldr cargo deny check bans`, `soldr cargo audit`, and
`soldr cargo machete`. Nextest runs from the workspace root, so
`.config/nextest.toml` keeps its test groups, slow-test budgets, timeout grace
period, and platform wrappers. Unset `CARGO_BUILD_JOBS` and `SOLDR_JOBS`
remain absent from the plan and stage environments, leaving Cargo's default
jobserver policy and Soldr's canonical compiler admission in control. Explicit
values are frozen into the plan byte-for-byte. `NEXTEST_TEST_THREADS` alone
is chosen by ci-test when unset (see "Memory-aware Nextest admission" below).
Nextest compilation completes before the fork,
but individual tests intentionally launch nested Cargo/compiler fixtures.
Those compiles and Dylint share Soldr's canonical shared/exclusive admission;
their dynamic child-jobserver count is not converted into a global Cargo cap.
Cgroup memory capacity remains diagnostic telemetry and is never applied as a
global `CARGO_BUILD_JOBS` cap.

The prescribed GitHub host lane also makes the Cargo front-door contract
fail-closed during this command. `CARGO` names a shim that re-enters the
source-built binary as `soldr cargo`, while a distinct bare `cargo` shim at the
front of `PATH` rejects accidental bypasses. Cargo replaces `CARGO` with its
own absolute path while launching build tooling, so the Nextest execution
stage installs a target runner that restores the allowed shim immediately
before each test process starts. Nested compiler fixtures remain supported
when they invoke `$CARGO`. An intentional absolute path to another Cargo
binary bypasses this name-based diagnostic; this guard is not an OS sandbox.

All six repository Dylints are retained. They intentionally use their exact
pinned nightly rather than the stable project toolchain. The command reads all
seven lint manifests, requires their pins to agree, and rejects an environment
override that selects another nightly. Before entering the Dylint domain it
self-provisions the catalogue-pinned `cargo-dylint`, `dylint-link`, and matching
prebuilt driver, so a caller does not need to install Dylint tooling separately.
Release-profile lint libraries share
`target/dylint/libraries/<nightly-host>`, workspace analysis uses
`target/dylint/target/<nightly-host>`, and UI tests share
`target/dylint/tests/<nightly-host>` through both `--target-dir` and
`CARGO_TARGET_DIR`. The command verifies all three trees and rejects material
artifacts in per-lint `dylints/*/target` directories. This exception preserves
Dylint correctness without causing stable Cargo fingerprints to flip between
toolchains.

Accepted scope selectors are `--package`/`-p`, `--features`,
`--all-features`, and `--no-default-features`. One explicit `--target` (including
a Soldr target alias) selects the stable compile domain. The exact host triple
runs tests by default; a target without a registered executor fails
closed with target-specific `--no-run` guidance. `--no-run` runs formatting,
Clippy, Dylint libraries and workspace analysis, and Nextest test compilation
for the selected target, then writes a Nextest archive at a stable path. It
skips Nextest execution, Dylint UI-test execution, and doctests. The archive
path is printed and included in the JSON plan. Target-directory, profile,
toolchain, manifest, and release overrides remain rejected.

`--explain-plan` performs no compiler work. Its schema-version-5 JSON freezes
workspace metadata identity, toolchain/compiler identity, host target, target
directories, profile, scope/features, Cargo configuration, Rust flags, wrapper
identity, stage dependencies, resource limits, the Nextest admission decision
(`test_admission`), and metric slots. Human output is the compact diagnostic
view of the same plan.

#### Memory-aware Nextest admission (soldr#2885)

An unset `NEXTEST_TEST_THREADS` is **measured**, not fixed: ci-test runs the
smaller of the logical CPU count and the number of per-test memory budgets
(`SOLDR_CI_TEST_PER_TEST_MEMORY_MIB`, default 1024) that fit in available
memory after a reserve (`SOLDR_CI_TEST_MEMORY_RESERVE_MIB`, default 2048),
never fewer than one. Available memory is the *tighter* of host
`MemAvailable` and finite cgroup headroom; a cgroup `memory.max` of `max`
(Docker Desktop) is never read as unlimited. Windows uses
`GlobalMemoryStatusEx` available physical memory and macOS `vm_stat`
reclaimable pages. When no reading is possible the historical one-test
fallback applies. `SOLDR_CI_TEST_MEMORY_AVAILABLE_MIB` and
`SOLDR_CI_TEST_LOGICAL_CPUS` replace the probed values at plan time -- for a
container that cannot see VM-wide pressure, and for reproducible plans. The
run-time monitor below always reads live memory.

An explicit `NEXTEST_TEST_THREADS` is authoritative and frozen verbatim
(including Nextest's `num-cpus` and `-N` spellings). When the measurement says
it cannot fit, planning prints an actionable warning and records it in
`test_admission.warning`; the value is never rewritten.

While Nextest executes, a monitor samples available memory. Below one per-test
budget it creates a pause flag; the Nextest wrapper
(`crates/soldr-nextest-wrapper`, every Unix test) then holds
tests that have not started yet while other tests drain. Admissions resume
once two budgets are available again, transitions are at least two seconds
apart, and paused tests are released one at a time. A paused test never waits
when nothing else is running, and waits at most
`SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS` (default 30) because the wait spends
its own Nextest timeout.

Each test's process tree also has a memory ceiling
(`SOLDR_CI_TEST_TEST_MEMORY_CEILING_MIB`, default 4096, `0` disables). On
Linux with a delegated cgroup v2 root named by `SOLDR_NEXTEST_CGROUP_ROOT` the
kernel enforces it (`memory.max`, `memory.oom.group`) on a per-test leaf;
otherwise the wrapper samples the tree's resident memory (procfs on Linux,
`ps` on macOS, so a fast allocation can overshoot between samples) and kills
only that tree. A test that crosses its ceiling, is OOM-killed in its cgroup,
cannot be started (`ENOMEM`/`EAGAIN`), or fails while printing an
allocation-failure signature is reported as an **infrastructure failure, not
an assertion**: the wrapper prints a `nextest memory:` diagnostic naming the
test, the requested/effective concurrency, the memory source and limit,
memory available at admission and at failure, the tree's peak RSS, and PID
pressure, then exits `75` (`EX_TEMPFAIL`). ci-test lists every such test after
Nextest exits. Windows does not run the wrapper: Nextest puts each Windows test
in its own Job Object, and no per-test memory ceiling is applied there.

### `soldr lint ci`

Statically validate the repository's executable CI/build surfaces against
Soldr's build policies (soldr#2038). This suite is a **pure filesystem scan**:
it requires no `Cargo.toml`, no Rust toolchain, and never starts the compiler
cache, so it runs in any repository — including non-Rust ones.

```bash
soldr lint ci                 # human-readable diagnostics (default)
soldr lint ci --format json   # stable machine-readable report
```

`ci` accepts only `--format json|human`; it does not take Cargo scope flags.
It exits `0` when there are no error-severity findings (warnings alone still
exit `0`) and non-zero when any error-severity finding remains after
suppressions.

**Scanned surfaces:** `.github/workflows/**/*.yml|yaml`,
`.github/actions/**/action.yml|yaml`, everything under `.github/scripts/**`,
and any `*.sh` / `*.py` / `*.ps1` helper referenced from a `run:`/`uses:` line
that exists on disk. Full-line and inline `#` comments are ignored, so prose
that mentions a legacy command is never flagged.

The suite is an extensible **registry of independent rules**. Adding a new CI
policy is a bounded module + one registration entry; the public command surface
does not change.

#### Rule: `cross-compile-surface`

Enforces that Apple Darwin (`*-apple-darwin`) and Windows MSVC
(`*-pc-windows-msvc`) builds go through the blessed `soldr build --target ...`
surface (`soldr prepare --target ...` is also accepted). It flags direct use of
a legacy cross wrapper or raw cross compiler for those targets:

- `cargo xwin` / `cargo-xwin` (and `cargo install cargo-xwin`),
- `cargo zigbuild` / `cargo-zigbuild`,
- raw `zig cc` / `zig c++` / `zig build-exe`,
- `*-w64-mingw32-*` (mingw), osxcross `o64-clang`, or a `clang`/`gcc`/`cc`
  carrying an Apple/Windows `--target`.

**Target-aware Zig exception:** Zig and `cargo-zigbuild` remain **allowed** for
`*-unknown-linux-*` and manylinux targets. The rule resolves each invocation's
`--target` (including matrix placeholders such as `${{ matrix.target }}`
against the workflow's declared matrix targets) and evaluates **per target**, so
a legitimate Linux-Zig matrix row never masks an invalid Apple/Windows row. Soldr's
own managed xwin / Apple-SDK / LLVM assets are allowed implementation details;
the rule governs the repository's public build entrypoint, not soldr internals.

A target that cannot be statically resolved on a surface that is otherwise
capable of Apple/Windows builds is reported as a lower-severity `warning`
(non-failing) rather than silently passing.

#### Inline suppression

A finding can be suppressed with a comment on the offending line or the line
immediately above it:

```yaml
soldr cargo xwin build --target x86_64-pc-windows-msvc  # soldr-lint-ci: allow cross-compile-surface -- intentional legacy-path regression test
```

`allow all` suppresses every rule on that line; `allow <rule-id>[,<id>...]`
suppresses only the named rules. Any text after `--` is a free-form reason.

#### JSON schema (`schema_version: 1`)

`--format json` emits a single object:

```json
{
  "schema_version": 1,
  "ok": true,
  "findings": [
    {
      "rule": "cross-compile-surface",
      "severity": "error",
      "file": ".github/workflows/release.yml",
      "line": 42,
      "tool": "cargo zigbuild",
      "target": "aarch64-apple-darwin",
      "recommendation": "use `soldr build --target aarch64-apple-darwin` (or `soldr prepare --target aarch64-apple-darwin`) instead of `cargo zigbuild`"
    }
  ]
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `schema_version` | integer | Schema version; bumped only on a breaking shape change. |
| `ok` | boolean | `true` when there are zero **error**-severity findings. |
| `findings[]` | array | Every finding (error and warning) after suppressions. |
| `findings[].rule` | string | Stable rule id, e.g. `cross-compile-surface`. |
| `findings[].severity` | string | `"error"` or `"warning"`. |
| `findings[].file` | string | Repo-root-relative path, `/`-separated. |
| `findings[].line` | integer | 1-based line where the command begins. |
| `findings[].tool` | string | Detected non-blessed tool, e.g. `cargo xwin`. |
| `findings[].target` | string | Resolved triple, a `*`-representative, or `<unresolved>`. |
| `findings[].recommendation` | string | The exact blessed replacement command. |

For Dylint builds, Soldr installs an absolute `soldr-dylint` compiler shim.
Ordinary dependency and lint-library compilation follows
`soldr-dylint -> rustc`; workspace analysis follows
`soldr-dylint -> dylint-driver -> rustc`. The compiler cache keys the driver,
loaded lint-library contents, and Dylint configuration. It caches individual
compiler outputs and diagnostics, never a command-level lint verdict: the real
Dylint pass executes on every invocation, so changed source is always analyzed.
Cargo incremental state accelerates repeated work in the same target tree,
while Soldr's object cache enables clean-target and sibling-worktree reuse.

### `soldr dylint cook`

Prepare external dependencies for a real Dylint pass without mixing its
nightly artifacts into the repository's ordinary stable build tree:

```bash
soldr dylint cook --workspace --all-targets
soldr dylint cook --plan-only --json
soldr dylint cook --tree tests --tests --target-root /path/to/repo/target
```

The command resolves one exact Dylint nightly from the verified
soldr-toolchain catalogue (or an explicit `--toolchain nightly-YYYY-MM-DD`),
then verifies the installed compiler's full release and commit identity. It
reconstructs a dependency skeleton and runs a dependency-only pass through
Soldr's normal compilation cache. `RUSTC_WORKSPACE_WRAPPER` and every
`DYLINT_*` library variable are removed for this phase, so custom lint
libraries are loaded only by the later real Dylint invocation.

Outputs live under `target/dylint/target/<nightly>-<host-triple>/` by default, matching
Dylint 6's own workspace-check directory; `--tree tests` selects the second
tree and changes the shape of the pass — see "Which tree" below. The pass is
check-shaped for the default `analysis` tree and build-shaped for `tests`.

The warm marker includes the observed compiler commit, manifests, lockfile,
selected target/profile/features/packages,
configuration, and wrapper identity. Workspace source contents are excluded,
so editing only a local source file preserves the external-dependency layer.
Conflicting nightly requirements from configured lint-library paths fail
instead of selecting one heuristically.

`--plan-only --json` does not install a missing toolchain. Its stable
`schema_version: 1` result includes `compiler`, `target_directory`,
`build_shape`, `cache_key`, and `outcome`. A plan-only result can report a
verified hit only when the installed compiler identity, marker, and target
payload all match. A normal invocation verifies the compiler again after any
restore/install and reports `miss` after cooking or `skip` when the complete
layer is already warm.

Shape options are `--tree <analysis|tests>`, `--target-root <DIR>`,
`--target`, `--release` / `--profile`, `--workspace`, repeatable `--package`,
`--features`, `--all-features`, `--no-default-features`, `--all-targets`,
`--tests`, `--benches`, `--examples`, repeatable `--config`, `--locked`,
`--frozen`, and `--offline`. `--tree` and `--target-root` are the only two
that are not forwarded to cargo: they select the target directory instead.
`--tree` enters the cook key directly; `--target-root` is checked through the
target directory the marker records. Ordinary `soldr cook` behavior is
unchanged.

#### Which tree: `--tree <analysis|tests>` (soldr#3042)

There are two Dylint target trees and they are not interchangeable.
`--tree analysis` (the default, and the only historical behavior) prepares
`target/dylint/target/<nightly>-<host-triple>/`, the tree `cargo dylint` analyses the
workspace in. `--tree tests` prepares
`target/dylint/tests/<nightly>-<host-triple>/`, the dependency layer the
Dylint UI-test stages of `soldr ci-test` compile their `trybuild`/UI
harnesses against (`dylint_testing` -> `compiletest_rs`, `git2`,
`libgit2-sys`, plus `dylint`'s own build script).

Three things change with `--tree tests`, and each is required for the cooked
artifacts to be *reused* rather than merely present:

* **Shape.** The pass is build-shaped (`cargo build`) rather than
  check-shaped. `cargo check` emits `.rmeta` under a different fingerprint
  mode than the `cargo test` the UI-test stage runs, so a check-shaped cook
  of this tree would be recompiled wholesale on first use.
* **Directory naming.** The channel segment carries the host triple, because
  that is how `soldr ci-test` names the tree it passes to `--target-dir`.
  The analysis tree keeps its bare `<nightly>` segment, because that is what
  `cargo dylint` picks for itself. The two rules genuinely disagree, and the
  cook deliberately follows each tree's own consumer.
* **Environment.** The cook applies the same `dylint`/`dylint-link`
  bootstrap `PATH`, `SOLDR_LINKER=default`, and `SOLDR_NO_GC_TARGET=1` that
  `soldr ci-test` gives every Dylint stage. `SOLDR_LINKER` is load-bearing:
  Soldr's linker injection rewrites `RUSTFLAGS`, and `RUSTFLAGS` is in every
  unit's fingerprint.

`--target <TRIPLE>` is rejected together with `--tree tests`: the UI-test
stages compile for the host only, so a cross-target cook would land its
artifacts one directory deeper than Cargo will look.

`--target-root <DIR>` overrides where `target/dylint/...` is rooted. Each
lint crate under `dylints/` is its own Cargo workspace, so a tests-tree cook
runs with the working directory inside `dylints/<lint>` while the tree it
must fill belongs to the *repository*. A relative path resolves against the
current directory. Without the flag, the tree would be created at
`dylints/<lint>/target/...`, which
`.github/scripts/verify_dylint_target_dirs.py` rejects.

The two trees never share a cook key: the tree name is folded into the cache
digest, so one tree's marker can never satisfy the other's.

`soldr ci-test` is the CI driver: its `dylint-cook-<lint>` stages run one
`--tree tests` cook per `dylints/*` crate inside the Dylint branch, after the
lint libraries and before workspace analysis, so none of these compiles
overlap running tests (soldr#3460).
Neither tree is uploaded anywhere: the cook fills an in-run target tree, and
cross-run reuse comes from the per-unit object store
(see `ci/cache-ownership.json`).

### `soldr cook`

Content-addressable dependency pre-build (issue #359). `cook` is a shim
around [`cargo-chef`](https://github.com/LukeMathWalker/cargo-chef) — soldr
ships no Rust reimplementation of the recipe logic; it fetches the pinned
`cargo-chef` binary (currently v0.1.73), drives `cargo chef prepare` to
synthesise a `recipe.json` derived only from `Cargo.toml` + `Cargo.lock`,
then drives `cargo chef cook` to compile a stub project containing those
deps. The output lands in `target/` with no project source code touched,
so any subsequent `soldr cargo build` reuses the compiled deps.

Both phases route through `soldr cargo` so they automatically pick up
zccache (`RUSTC_WRAPPER`), `ZCCACHE_PATH_REMAP=auto`, the soldr linker
selection, and the soldr-managed `CARGO_HOME` / `RUSTUP_HOME`.

```bash
soldr cook                                  # debug cook, ephemeral recipe
soldr cook --release                        # release cook, ephemeral recipe
soldr cook --release --target x86_64-unknown-linux-musl
soldr cook --release --recipe-path recipe.json --prepare-only   # Docker phase 1
soldr cook --release --recipe-path recipe.json --cook-only      # Docker phase 2
soldr cook --release --keep-recipe          # cook + leave recipe.json behind
soldr cook --release -- --features extra,fast --no-default-features
```

Recognised flags:

- `--release` — forwarded to `cargo chef cook --release`.
- `--target <triple>` — forwarded to `cargo chef cook --target <triple>`.
- `--workspace` (alias `--all`) — forwarded to `cargo chef cook --workspace`.
- `--profile <name>` — forwarded to `cargo chef cook --profile <name>`.
- `-p` / `--package <name>` — repeatable; forwarded as `--package <name>`.
- `--recipe-path <path>` — write/read the recipe at this absolute or
  manifest-relative path. Without this flag the recipe lives in a temp
  dir and is deleted on exit.
- `--keep-recipe` — retain the recipe on disk (at `--recipe-path` if
  supplied, else `<cwd>/recipe.json`) so you can inspect it.
- `--prepare-only` — run `cargo chef prepare` only and exit. Used for
  the Docker recipe-layer pattern below.
- `--cook-only` — skip `prepare`; requires `--recipe-path`. Used for the
  Docker cook-layer pattern below.
- `--no-trim` — skip the post-cook `target/` trim. By default (issue
  #459) cook removes cargo-recreatable noise — incremental state, the
  synthetic stub binary, build-script binaries, large stderr blobs,
  debug sidecars, and `examples/`/`doc/`/`tests/` — so the downstream
  tarball ships dramatically fewer bytes (~30–40% drop on a typical
  cook output). Use `--no-trim` only if you genuinely need the full
  raw `target/` tree.
- Anything after `--` is forwarded verbatim to `cargo chef cook` (e.g.
  `--features`, `--no-default-features`, `--all-features`, `--tests`,
  `--benches`).

**Docker recipe pattern** — the canonical use case the issue highlights:

```dockerfile
FROM rust:1 AS chef
RUN cargo install soldr
WORKDIR /app

FROM chef AS planner
COPY . .
RUN soldr cook --release --prepare-only --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Heavy step — cached as long as recipe.json (i.e. Cargo.lock) is stable.
RUN soldr cook --release --cook-only --recipe-path recipe.json
COPY . .
RUN soldr cargo build --release --bin myapp
```

**Local-dev pattern** — first-time setup on a fresh clone:

```bash
git clone <repo> && cd <repo>
soldr cook --release         # one-time dep prebuild; ~10 min cold, ~0s warm
soldr cargo build --release  # builds only the project on top
```

**Cargo.lock missing.** `soldr cook` continues with a warning if
`Cargo.lock` is absent — cargo-chef will derive the recipe from
`Cargo.toml` alone, which weakens content-addressability. For
deterministic builds, commit `Cargo.lock` (libraries should still ship
`Cargo.lock` for reproducibility under `soldr cook`, even though cargo
normally `.gitignore`s lockfiles in library crates).

**Companion automation.** [`zackees/setup-soldr#110`](https://github.com/zackees/setup-soldr/issues/110)
proposes a GitHub Action that keys the complete dependency closure by
`Cargo.lock`: the cooked `target/` plus Cargo registry/git sources. With
`soldr cook` available as a primitive, the action restores both archives and
runs cook only when the dependency base is absent.

### `soldr save` / `soldr hydrate` (`soldr load` alias)

Bundle a build-cache directory plus a content-verified snapshot of
source-file mtimes into a single `.tar.zst` archive (`save`), then
rematerialize it on a fresh checkout (`hydrate`). The historical `load`
spelling remains a compatibility alias. Intended for CI cache layers
that need stable Cargo fingerprints across `actions/checkout` runs
without resorting to mtime-rewrite tricks.

```bash
soldr save --cache-dir <dir> --workspace <dir> --out cache.tar.zst
soldr hydrate --archive cache.tar.zst --cache-dir <dir> --workspace <dir>
```

Recognised `soldr save` flags:

- `--cache-dir <DIR>` — cache directory to archive.
- `--workspace <DIR>` — workspace whose source-file mtimes get snapshotted.
- `--out <FILE>` — destination archive.
- `--threads <N>` — parallel hash / compression worker hint.
- `--mtimes-only` — write only the source-file mtime manifest.
- `--delta-from-manifest <FILE>` — save a delta against a previously
  restored base manifest.
- `--ci`, `--minimal` — CI/minimal payload profile. Excludes logs,
  sockets, lock files, runtime scratch, zccache runtime binaries, and
  soldr-managed binary/toolchain trees from the cache payload while
  preserving cache artifacts and manifest state needed for warm rustc hits.
- `SOLDR_SAVE_PROFILE=cook` — cook payload profile (soldr#2996). For a
  cargo target directory rather than a soldr cache dir. Keeps the
  dependency graph — `deps/*.rlib` / `*.rmeta` / shared libraries,
  `.fingerprint/`, and everything under `build/` including the
  extensionless `build-script-build` executables Cargo needs to
  rematerialize a closure — and drops what a build layers on top:
  linked binaries and test executables, `incremental/`, `examples/`,
  `doc/`, test trees, and debug sidecars.

  The motivating case is zackees/setup-soldr#499: the cook cache layer
  archives the target dir from a *post* step, by which point the real
  build has written its binaries into the same tree, turning an 83 MiB
  cook slice into a 1.62 GB entry. Selecting this profile fixes the
  payload without moving when the archive is written.
- `--json` — emit a JSON summary. Save JSON includes
  `profile`, `source_files`, `cache_files`, `excluded_files`,
  `excluded_bytes`, `archive_bytes`, and `elapsed_ms`.

When `--cache-dir` contains the active embedded zccache root, `soldr save`
first completes a cache checkpoint, gracefully shuts down the daemon, and
waits for the exact daemon generation that acknowledged shutdown before the
archive walk begins. A checkpoint alone is not a snapshot barrier because a
new publication could otherwise land between the checkpoint and tar traversal.
If quiescence cannot be proven, save fails instead of producing a racing
snapshot. This intentionally stops the daemon; the next compile-like command
starts a fresh generation. An unrelated `--cache-dir` does not flush, stop, or
otherwise depend on the ambient Soldr daemon.

Recognised `soldr hydrate` / `soldr load` flags (issue #575):

- `--archive <FILE>` — input archive produced by `soldr save`.
- `--cache-dir <DIR>` — destination cache directory; created if absent.
- `--workspace <DIR>` — workspace whose source-file mtimes get the
  content-verified replay. Optional when `--mtimes-only` is unset.
- `--threads <N>` — parallel-extract worker count. Defaults to
  rayon's `num_cpus`; capped at the value passed here.
- `--mtimes-only` — refuse cache entries; apply mtime snapshot only.
- `--manifest-out <FILE>` — write the archive's protobuf manifest for a
  later `soldr save --delta-from-manifest`.
- `--profile-extract` — emit a per-phase profile line to stderr after
  the load completes. Shape:
  ```text
  soldr load: profile: zstd_decode=4120ms tar_parse=890ms extract_total=10510ms \
    workers={0:n=12058, 1:n=12090, 2:n=12053, 3:n=12030} \
    per_file_p50_us=180 p95_us=450 p99_us=1200 cache_files=48231
  ```
  Also enabled when `SOLDR_PROFILE_EXTRACT=1` is set in the environment.
  Useful for tuning the parallel-extract worker count against real
  workloads (issue #575). The line lands on **stderr** and retains the
  historical `soldr load:` prefix for log-parser compatibility; the existing
  machine-readable status line on stdout is untouched.
- `--auto-defender-exclude` — on Windows + admin, briefly add
  `--cache-dir` to Defender's exclusion list for the duration of the
  load (issue #596). Never UAC-prompts: no-op on non-Windows or when
  the current process isn't elevated.
- `--json` — machine-readable status line on stdout instead of the
  human-readable summary.

`SOLDR_LOAD_WORKERS=<N>` overrides `--threads` and sizes the rayon
pool that runs both the per-file extract workers and the
mtime-replay walk.

### `soldr status`

Show cache and target information. The stable JSON shape retains the historical
compile-daemon `fallbacks` rollup for upgrade compatibility; the mandatory
broker route no longer appends new fallback records.

Stable machine-facing mode:

```bash
soldr status --json
```

### `soldr clean`

Clear the local embedded-zccache artifact cache and remove Soldr's zccache
session state directory.

### `soldr logs`

Inspect soldr's runtime activity (soldr#820, soldr#3698).

```
soldr logs list [--limit N] [--json]     # recent launches (daemon-owned history)
soldr logs show <launch-id> [--json]     # one launch summary + log paths
soldr logs paths [--json]                # every directory soldr writes logs into
soldr logs view <launch-id>              # stream a launch's archived JSONL journal(s)
soldr logs prune --keep N [--dry-run] [--json]
```

A *launch* is one per-build archive directory, named by its decimal session
id, under the `zccache-build-history` entry that `soldr logs paths` reports
(`<cache>/zccache/history/<session-id>/`). `view` and `prune` resolve that
directory through the same inventory; there is no second log location.

- `view` accepts an exact id or a unique decimal prefix and copies every
  `*.jsonl` file in the launch directory (sorted by name) to stdout. It fails
  when the id is unknown, ambiguous, or the launch has no journal.
- `prune --keep N` keeps the newest N launches (by directory mtime, session id
  as tie-break) and removes the rest. It only removes direct, non-symlink,
  all-digit children of the history directory, never touches any other file,
  and skips launches still being published (`.publishing-v2`). `--dry-run`
  reports without deleting. `--json` emits `schema_version: 1` with
  `history_root`, `keep`, `dry_run`, `kept`, `removed`, `skipped_active`, and
  `failed`. Exit code is 1 when any removal failed. Daemon-side build-history
  rows are left as-is; the daemon's own history GC reconciles them.

### `soldr config`

Show or set keys in Soldr's `config.toml` (`<soldr root>/config.toml`,
normally `~/.soldr/config.toml`; `SOLDR_CACHE_DIR` moves it). Keys are dotted
paths into the file (`linker`, `cook.max_total_gb`, `pins.zccache`).

```bash
soldr config                         # same as `soldr config list`
soldr config list                    # every key set in the file, `key = value`
soldr config get cook.max_total_gb   # one value; exit 1 when the key is unset
soldr config set cook.max_total_gb 12
soldr config set linker rust-lld
soldr config path                    # print the config file path
```

- `get` and `list` report only what the file sets; they do not print
  defaults.
- `set` parses the value as a TOML value (`12`, `false`, `"x"`) and falls
  back to a plain string (`rust-lld`, `1.12.5`). The top-level key must be
  one Soldr knows (`gc`, `auto_gc`, `linker`, `cook`, `pins`, `jobs`,
  `zccache`, `install`), and the edited file must load through the same
  `SoldrConfig` loader the rest of Soldr uses; otherwise nothing is written.
- Writes keep existing comments and layout and replace the file atomically.

### `soldr cache`

Inspect the embedded zccache service and its cache status.

Stable machine-facing mode:

```bash
soldr cache --json
```

#### `soldr cache flush`

Synchronously checkpoint pending embedded zccache publications and persistent
index state without stopping `soldr-daemon`. The command succeeds only when
all reported steps complete; `--json` exposes the detailed flush report.

```bash
soldr cache flush
soldr cache flush --json
```

#### `soldr cache shutdown`

Checkpoint the embedded cache, request graceful daemon shutdown, and by default
wait for the exact PID and generation returned in the shutdown
acknowledgement. The default wait is 300 seconds and can be changed with
`--shutdown-timeout-seconds <SECONDS>`. Once a daemon acknowledges shutdown,
Soldr never force-kills it: a timeout reports failure while the daemon
continues its durability work.

`--archive-logs <DIR>` copies the finalized session log, journal, and stats
only after the acknowledged generation is proven quiescent. It therefore
cannot be combined with `--no-wait`. `--no-depgraph-save` skips the explicit
pre-shutdown checkpoint for debugging, but graceful shutdown still completes
its own cache flush. `--json` emits the stable machine-facing result.

```bash
soldr cache shutdown
soldr cache shutdown --shutdown-timeout-seconds 600 --archive-logs ./soldr-logs
soldr cache shutdown --no-wait
```

#### `soldr cache prune-target <path>`

Prune stale per-prefix cargo build artifacts inside a given `target/`
directory. Scanned subdirectories under each profile (`debug/`,
`release/`, …) are `deps/`, `.fingerprint/`, `incremental/`, and
`build/`.

Two strategies are available:

- **Default — orphan siblings (issue #336).** Keep the newest entry per
  `(parent_dir, prefix)` bucket. Each subdirectory's set of
  hash-siblings is pruned independently. Conservative: cargo tolerates
  orphans across the four subdirs as long as the live entry inside
  each is current.
- **`--keep-latest` — aggressive (issue #316).** Bucket by `prefix`
  alone; keep only the **newest hash family** per logical artifact
  name, deleting every other hash's files across the four subdirs.
  Shrinks a heavily-rebuilt target/ from 16+ GB to ~2 GB on real
  workloads. Use when you don't need the per-subdir orphan retention.

**Recency ranking.** Both strategies prefer cargo's authoritative
`target/<profile>/.fingerprint/<prefix>-<hash>/invoked.timestamp`
mtime — the same signal cargo's unstable `-Zgc` uses — and fall back
to the entry's own filesystem mtime when the fingerprint file is
missing or unreadable. The JSON output reports per-decision
provenance via `keep_decisions_from_fingerprint` and
`keep_decisions_from_mtime`.

```bash
soldr cache prune-target ./target                  # dry-run, orphan-siblings
soldr cache prune-target ./target --keep-latest    # dry-run, aggressive
soldr cache prune-target ./target --force          # actually delete (orphan-siblings)
soldr cache prune-target ./target --keep-latest --force  # actually delete (aggressive)
soldr cache prune-target ./target --dry-run --json # machine-readable plan
```

Defaults to a dry run for safety; pass `--force` (or `--no-dry-run`)
to actually delete entries. If `target/.cargo-lock` or any
`target/<profile>/.cargo-lock` is present the command refuses with a
non-zero exit code (suggests a live build).

JSON schema (`--json`):

```json
{
  "schema_version": 1,
  "command": "cache prune-target",
  "target_dir": "<absolute path>",
  "dry_run": true,
  "keep_latest": false,
  "scanned": 3,
  "kept": 1,
  "deleted": 2,
  "reclaimed_bytes": 0,
  "reclaimed_human": "0 B",
  "keep_decisions_from_fingerprint": 1,
  "keep_decisions_from_mtime": 0,
  "entries": [
    {
      "path": "<absolute path>",
      "prefix": "libfoo",
      "hash": "abcdef1234567",
      "size_bytes": 0,
      "size_human": "0 B",
      "mtime_unix": 1700000500,
      "action": "keep"
    }
  ]
}
```

**Explicit target pruning (issues #485 and #1818).** Soldr no longer
prunes hash families automatically around build-like invocations.
Multiple families for one crate can all be live at once, so selecting
only the newest family can turn an unchanged warm build into a rebuild
loop.

Use `soldr cache prune-target <path>` when explicit target maintenance
is desired. The historical `--no-gc-target`,
`--no-gc-target-before`, and `--no-gc-target-after` flags remain
accepted as compatibility no-ops. `SOLDR_NO_GC_TARGET` is likewise
accepted but unnecessary now that no automatic pass runs.

### `soldr version`

Print soldr version.

Stable machine-facing mode:

```bash
soldr version --json
```

### `soldr toolchain`

Project-aware orchestrators around `rustup` and `cargo install` that
read `rust-toolchain.toml` so users (and CI) don't have to thread the
pinned channel through every command. Like rustup and Cargo, they use
the nearest `rust-toolchain.toml` in the current directory or any
ancestor, so they work from a subdirectory of a pinned repo (soldr#3633).

```bash
soldr toolchain install   # rustup toolchain install <channel> --profile minimal --no-self-update
soldr toolchain prepare   # install + component add + target add + cargo install for [soldr.plugins]
soldr toolchain ensure    # bootstrap rustup if missing, run `prepare`, smoke-verify (cargo/rustc --version)
soldr toolchain ensure --json  # same, but emit a stable JSON payload (schema_version: 1)
soldr toolchain link --shim-dir <path> [--json] [--force]  # write PATH shim files (issue #407 Phase 3)
soldr toolchain doctor [--json]   # run env-detection probes (musl-cc, shared target/) (issue #407 Phase 4)
```

`prepare` runs, in order:

1. `rustup toolchain install <channel> --profile minimal --no-self-update`
2. `rustup component add --toolchain <channel> <component>` for every entry in `[toolchain].components`
3. `rustup target add --toolchain <channel> <target>` for every entry in `[toolchain].targets`
4. a prebuilt `known_tools` fetch, else `cargo install <name> [--version V] [--locked] [--features ...] [--no-default-features]` for every entry in `[soldr.plugins]`

The Cargo front door performs the toolchain/component/target portion
automatically before launching the child build. A successful preparation is
memoized under the Soldr cache, so an unchanged warm invocation launches no
Rustup subprocesses. The memo is invalidated when the channel, profile,
component or target requirements, explicit `+toolchain`, effective Rustup
home/binary, or installed toolchain identity changes. That identity includes
Rustup's authoritative `lib/rustlib/multirust-channel-manifest.toml` and the
`lib/rustlib/components` manifest, in addition to the native compiler binary.
Failed preparation is never memoized. `soldr toolchain prepare` and `ensure`
remain explicit, unconditional orchestrators and also handle `[soldr.plugins]`.

#### Toolchain readiness and recovery

Soldr uses one filesystem contract before Dylint, blessed target setup, or a
Cargo preparation memo may reuse a selected toolchain directory:

| State | Directory | Channel manifest | Native `rustc` | Action |
|---|---|---|---|---|
| Missing | absent | n/a | n/a | install may create it |
| Partial | present | either/both absent | either/both absent | never reuse or memoize it |
| Ready | present | present | present | callers may perform their additional checks |

Cargo additionally requires `lib/rustlib/components` before it writes or
accepts its preparation memo. A partial directory under a caller-selected
`RUSTUP_HOME` is fail-closed: Soldr reports the missing evidence and exact
directory, but does not uninstall or reinstall shared user state. Recover with
`soldr rustup toolchain uninstall <channel>` followed by `soldr toolchain
install`, or repair the stated directory using the caller's toolchain manager.
Soldr-managed private homes may retain their existing automatic interrupted-
install recovery.

Plugin installs are bootstrap/dev-tool acquisition, not project compilation.
They invoke the directly resolved cargo binary and clear inherited
`RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER` so a setup step cannot
recursively re-enter Soldr's zccache wrapper while installing the tools
that future builds may use.

The first non-zero exit short-circuits the chain.

When the project selects `reld` as its linker (`SOLDR_LINKER=reld`/`fast`,
`~/.soldr/config.toml` `linker =`, a project `.cargo/config.toml`
`[target.<triple>] linker`/`rustflags`, or `[workspace|package.metadata.soldr]
linker`), `soldr prepare` and `soldr toolchain prepare`/`ensure` also fetch the
pinned, sha256-verified `reld` binary described under the `SOLDR_LINKER` entry
in the environment variable table below, so the linker is in place before the
child build starts.

`soldr toolchain prepare`/`ensure` also provision the build tools a native
`soldr cargo build --release` would fetch inside the build (soldr#3802): the
linker driver's managed LLVM and shim (exactly when the build would need them,
so a host whose rustc supplies lld fetches nothing), `reld` when selected, and
the managed cmake/ninja (skipped when `Cargo.lock` proves no `cmake` crate). It
runs the front door's own functions, so prepare and build cannot disagree, and
is idempotent. A report line says what was provisioned, or `nothing provisioned
(<reason>)`. `--json` adds an optional `build_tools` object (`provisioned`,
`linker`, `cmake`, `note`; `null` when it did not run); `schema_version` stays
1. Without a `rust-toolchain.toml` channel the rustup steps are skipped, and the
build tools are still provisioned under `--allow-unpinned` /
`RUSTUP_TOOLCHAIN`. The README benchmark runs this before its timed cold build
and reports it separately as `provision_ms`.

#### `soldr toolchain ensure`

One-shot "make sure this host can build" verb (issue #407 Phase 2):

1. Auto-bootstraps `rustup` into the soldr-managed bin dir if missing
   (reuses the same logic as `soldr bootstrap`). Respects
   `SOLDR_NO_BOOTSTRAP=1`.
2. Runs the same `install` + `component add` + `target add` + plugin-
   install pipeline as `prepare`.
3. Smoke-verifies the resolved toolchain by spawning `cargo --version`
   and `rustc --version`. Either failure marks the verify as failed and
   exits non-zero.

`ensure` is intended for one-stop bootstrap callers like
[setup-soldr#133](https://github.com/zackees/setup-soldr/issues/133)
that want to delegate every TS toolchain step to the soldr binary.
Existing `install` and `prepare` output formats are unchanged.

The `--json` payload (`schema_version: 1`) is the stable contract for
those callers — fields may be added in future schema versions but
existing field names and types will not change without a schema bump:

```json
{
  "schema_version": 1,
  "channel": "1.95.0",
  "rustup_bootstrapped": false,
  "components_added": ["rustfmt", "clippy"],
  "targets_added": ["aarch64-apple-darwin"],
  "plugins_installed": ["cargo-zigbuild@0.18"],
  "smoke_verify": {
    "cargo_version": "cargo 1.95.0 (abc1234 2026-04-15)",
    "rustc_version": "rustc 1.95.0 (def5678 2026-04-15)",
    "missing_std_targets": [],
    "ok": true
  },
  "elapsed_ms": 12345,
  "linker": "reld"
}
```

`linker` is an additive field reporting the resolved linker selection (see the
`SOLDR_LINKER` entry below); its presence and value follow the same
env/config/manifest precedence used for `soldr cargo ...` builds and do not
change any other field's meaning.

Notes on the schema:

- `channel` is `null` when no `rust-toolchain.toml` is present; the rest
  of the payload still serializes so consumers can parse unconditionally.
- `rustup_bootstrapped` is `true` only when `ensure` actually fetched
  rustup-init for this invocation. A pre-existing rustup or
  `SOLDR_NO_BOOTSTRAP=1` keeps it `false`.
- `components_added` / `targets_added` / `plugins_installed` mirror the
  manifest entries that the `prepare` pipeline attempted. Plugin labels
  are `name` for bare or `*` versions, otherwise `name@version`.
- `smoke_verify.ok` is `false` when either spawn fails or returns
  non-zero, or when `missing_std_targets` is not empty. The JSON payload is
  emitted in both success and failure cases; only the process exit code
  differs.
- `smoke_verify.missing_std_targets` (additive, soldr#3376; `schema_version`
  stays `1`) lists the host and declared-target triples whose
  `lib/rustlib/<triple>/lib/libcore-*.rlib` (and, for the host, `libstd-*.rlib`)
  are missing from the resolved toolchain even though rustup's `components`
  manifest may still claim them. It is always present and empty when all files
  exist. Only a `rustc` inside a real `lib/rustlib` layout is judged.
- `error` (additive, soldr#3693; `schema_version` stays `1`) is `null` on a
  normal run. When an early step fails before a result exists -- rustup
  bootstrap, reading `rust-toolchain.toml`, a `prepare` spawn failure (for
  example a plugin `cargo install` that cannot start), or the smoke-verify
  spawn -- `--json` still prints the schema-v1 payload with whatever fields
  were known, `error` set to the message, and `smoke_verify.ok = false`, and
  the process exits `1`. Previously stdout was empty in that case.


##### Build drivers that look up the toolchain themselves (soldr#3716)

`soldr cargo ...` never relies on rustup's lazy auto-install: before
Cargo runs, the front door installs the `rust-toolchain.toml` channel and
its declared components and targets (`ensure_cargo_toolchain`), so the
cost of a toolchain soldr has not seen is a slower first build, not a
failure.

A driver that locates Rust *before* any `soldr cargo` call does not get
that guarantee. CMake with [corrosion](https://github.com/corrosion-rs/corrosion)
is the common case: `FindRust` queries rustup during configure, and on a
fresh host or container the pinned channel is not installed yet, so
configure fails with `Could not find toolchain '<channel>'`. Run
`soldr toolchain ensure` first:

```bash
soldr toolchain ensure          # install the pinned channel + components + targets
cmake -S . -B build             # corrosion now finds the installed toolchain
cmake --build build
```

#### `soldr toolchain link`

Write PATH shim files into `--shim-dir` so a child process that
resolves `cargo` / `rustfmt` / `clippy-driver` / `rustc` / `rustdoc`
through PATH gets routed back through `soldr <tool>` (issue #407 Phase
3, ports setup-soldr's `ensure-shims.ts`).

Each shim is platform-aware:

- **Unix**: a `#!/bin/sh` script that `exec`s `<soldr-path> <tool>
  "$@"`. File mode `0o755`.
- **Windows**: a `.cmd` script with CRLF line endings that calls
  `"<soldr-path>" <tool> %*`.

The absolute path to the running soldr binary (resolved via
`std::env::current_exe()`) is baked into each shim at write time, so
the shim does not depend on PATH to find soldr itself.

Idempotency:

- Existing shim file whose contents equal the expected body → left
  alone, reported as `skip_reason: "existing-matches"`. Mtime is
  preserved.
- Existing shim file whose contents differ AND `--force` not passed →
  left alone, reported as `skip_reason: "existing-differs"`. The
  caller can detect this and decide whether to re-run with `--force`.
- Existing shim file whose contents differ AND `--force` passed →
  overwritten, reported as `created: true`.

The `--json` payload (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "shim_dir": "/runner/.setup-soldr/shims",
  "tools": [
    {"name": "cargo", "shim_path": "/runner/.setup-soldr/shims/cargo", "created": true},
    {"name": "rustfmt", "shim_path": "/runner/.setup-soldr/shims/rustfmt", "created": false, "skip_reason": "existing-matches"},
    {"name": "clippy-driver", "shim_path": "/runner/.setup-soldr/shims/clippy-driver", "created": true},
    {"name": "rustc", "shim_path": "/runner/.setup-soldr/shims/rustc", "created": true},
    {"name": "rustdoc", "shim_path": "/runner/.setup-soldr/shims/rustdoc", "created": true}
  ],
  "elapsed_ms": 12
}
```

Notes on the schema:

- `tools[].skip_reason` is omitted (not `null`) when `created: true`.
- The `tools` array order is stable: `cargo`, `rustfmt`,
  `clippy-driver`, `rustc`, `rustdoc`.
- `link` never adds the shim directory to the caller's `PATH` — that
  is the caller's responsibility (e.g. setup-soldr emits a GitHub
  Actions `addPath` call after consuming the JSON).

#### `soldr toolchain doctor`

Run env-detection probes that ship the diagnostic intel `setup-soldr`
used to compute in TypeScript (issue #407 Phase 4, ports the
env-detection halves of setup-soldr's `detect-musl-cc.ts`,
`detect-shared-target-warning.ts`, and `diagnostics.ts`).

Namespaced under `toolchain` to avoid colliding with the top-level
`soldr doctor` system check.

Probes (run in stable order):

1. **`musl-cc`** — scans `PATH` for `musl-gcc` / `musl-clang` and, if
   found, captures the first line of `--version` output. Auto-skipped on
   non-Linux hosts (`details.skipped = "not-linux"`). When the host is
   Linux but no musl C compiler is on `PATH`, the probe still reports
   `ok: true` with `details.found = false` — not all Linux workflows
   need musl tooling, so missing it is informational rather than fatal.
2. **`shared-target-warning`** — walks the current workspace's
   `target/` (up to 3 levels deep) looking for a populated
   cargo `.fingerprint/` directory. Mirrors the prepopulated-target
   detector landed in PR #508. Reports `would_warn: true` when at least
   one fingerprint dir is found, signalling that a subsequent
   `soldr cargo build` may collide with the cook hydrate path.

Exit code: `0` when every probe reports `ok: true`, `1` otherwise.

The `--json` payload (`schema_version: 1`) is the stable contract for
`setup-soldr#133` and similar consumers:

```json
{
  "schema_version": 1,
  "host": {"os": "linux", "arch": "x86_64", "libc": "gnu"},
  "probes": [
    {
      "name": "musl-cc",
      "ok": true,
      "details": {
        "musl_cc": "/usr/bin/musl-gcc",
        "tool": "musl-gcc",
        "version": "musl-tools 1.2.5-1"
      }
    },
    {
      "name": "shared-target-warning",
      "ok": true,
      "details": {
        "target_dir": "/workspace/target",
        "fingerprint_dirs_found": 0,
        "would_warn": false
      }
    }
  ],
  "elapsed_ms": 12
}
```

Notes on the schema:

- `host.os` is `std::env::consts::OS` (`linux` / `windows` / `macos`).
- `host.arch` is `std::env::consts::ARCH` (`x86_64` / `aarch64` / …).
- `host.libc` is `gnu` on Linux, `msvc` on Windows, `darwin` on macOS
  (CLAUDE.md mandates MSVC-default on Windows).
- The `probes` array preserves declaration order: `musl-cc` first,
  `shared-target-warning` second. Future probes will be appended.
- Each probe's `details` object is probe-specific. Consumers must
  switch on `probe.name` before reading nested keys.
- `probe.ok = false` is reserved for future probes; today both probes
  always set `ok: true` because a missing musl-cc or a clean target/
  is informational rather than blocking.

#### `[soldr.plugins]`

Top-level `[soldr]` section of `rust-toolchain.toml`. Currently
surfaces a `plugins` table keyed by cargo crate name. Each value is
either a bare version requirement or a detailed table.

```toml
[toolchain]
channel = "1.95.0"

[soldr.plugins]
cargo-nextest = "0.9"
cargo-zigbuild = { version = "0.18", locked = true }
cargo-deny    = "*"          # any version — `--version` is omitted
cargo-llvm-cov = { version = "0.6", features = ["no_cfg_coverage"], no_default_features = true }
```

Field semantics for the detailed shape:

| Field                 | Type           | Maps to                  |
|-----------------------|----------------|--------------------------|
| `version`             | string         | `--version <value>`. `"*"` or unset omits the flag entirely. |
| `locked`              | bool           | `--locked` when `true`.  |
| `features`            | list of string | `--features <a,b,c>` when non-empty. |
| `no_default_features` | bool           | `--no-default-features` when `true`. |

Pre-built first (soldr#3699): an entry whose crate is a `known_tools` cargo subcommand (`cargo-nextest`, `cargo-deny`, `cargo-zigbuild`, ...) is first fetched as a prebuilt release through soldr-fetch (install lock, trust sha, staged promotion) into soldr's tool cache, where `soldr cargo <sub>` runs it. The version requirement is always honoured: the prebuilt is used only when the version the cargo front door would run (the registry pin, else the latest release) satisfies it. A requirement the prebuilt cannot satisfy, `features` / `no_default_features`, an unregistered crate, or a failed fetch falls back to `cargo install` with the declared requirement; a different version is never silently installed. A prebuilt-served plugin is not placed in `$CARGO_HOME/bin`, so invoke it as `soldr cargo <sub>`.

`cargo install` fallbacks are dispatched to the cargo binary resolved by soldr's
toolchain probe (`resolve_toolchain_binary("cargo")`) and invoked
directly — **not** through the rustc wrapper. This routes installs
into soldr-managed `$CARGO_HOME` while letting the active cargo honor
`rust-toolchain.toml` at exec time, so no explicit channel is threaded
through. `cargo install` is idempotent by design, so a second
`prepare` after a successful one is a no-op for plugins.

### `soldr doctor`

Diagnose drift between `rust-toolchain.toml` and the rustup state
currently installed for the declared channel. Read-only — `doctor`
never invokes `rustup toolchain install`, `rustup component add`, or
`rustup target add`. Exits `1` when drift is detected, `0` otherwise.
When no `rust-toolchain.toml` exists in the current working directory
or any of its ancestors (the same nearest-pin search rustup and Cargo
use, soldr#3633) the command exits `0` and reports that no manifest was found.

```bash
soldr doctor
soldr doctor --json
soldr doctor --refresh-defender-probe       # Windows: force fresh probe of the cache dir
```

Flags:

- `--json` — emit the stable machine-facing JSON form.
- `--refresh-defender-probe` — ignore the cached Defender real-time-scan
  probe result and run a fresh probe of the soldr cache directory.
  No-op outside Windows. Issue #357.

Example human output (drift detected):

```text
manifest: /home/user/project/rust-toolchain.toml
toolchain: 1.95.0
  status: installed

components (declared 2):
  rustfmt   installed
  clippy    MISSING

targets (declared 1):
  x86_64-unknown-linux-musl   installed

result: drift detected (1 missing component)
hint: run `soldr toolchain prepare` to bring installed state in sync with manifest
```

Example JSON output (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "command": "doctor",
  "manifest_path": "/home/user/project/rust-toolchain.toml",
  "toolchain": {"channel": "1.95.0", "installed": true},
  "components": [
    {"name": "rustfmt", "installed": true},
    {"name": "clippy", "installed": false}
  ],
  "targets": [
    {"triple": "x86_64-unknown-linux-musl", "installed": true}
  ],
  "drift": true,
  "missing_components": ["clippy"],
  "missing_targets": [],
  "timeouts": [
    {"name": "compile reply", "env_var": "SOLDR_COMPILE_REPLY_TIMEOUT_SECS", "default_secs": 1800, "effective_secs": 1800, "source": "default", "override_ignored": false}
  ],
  "fallbacks": {"total": 0, "recent": []},
  "soldr_debug_info": {
    "binary_path": "C:\\Users\\user\\.soldr\\bin\\soldr.exe",
    "debug_info_found": 1,
    "debug_info_expected": 1,
    "symbol_path": "C:\\Users\\user\\.soldr\\bin"
  },
  "defender_probe": {
    "verdict": "scanned",
    "probed_path": "C:\\Users\\user\\.soldr\\cache",
    "median_write_ms": 412,
    "probed_at_unix": 1715000000,
    "refreshed_this_run": false
  }
}
```

**`timeouts`** (soldr#1838 Phase 3). One row per daemon/toolchain timeout that
has an env override, so a consumer can see the *effective* value and where it
came from without re-deriving it: `name`, `env_var`, `default_secs`,
`effective_secs`, `source` (`"default"`, `"override"`, or `"default (override
ignored: unparseable)"`), and `override_ignored` — `true` when a variable was
set but did not take effect
(the soldr#1837 malformed-value-falls-back-to-default rule), broken out so CI
can assert on it without string-matching `source`.

**`fallbacks`** (soldr#1838 Phase 4). Historical compile-daemon fallback
telemetry retained as `{ total, recent[] }` for JSON compatibility. The
mandatory broker route does not append new records.

**Defender probe** (issue #357, Windows-only). `soldr doctor` runs a
throttled empirical scan probe to detect whether the soldr cache
directory is being inspected by Windows Defender's real-time
protection. The probe writes a 1 MiB `.dll` file into the cache
directory, times the syscall, and classifies the median across 3
repeats: ≥80 ms means scanned, below means excluded (or running on a
trusted Dev Drive). State persists at `~/.soldr/defender-probe.json`
and refreshes only when the cache path changes, the soldr version
changes, the cached state is older than 7 days, or
`--refresh-defender-probe` is passed. The field is omitted from
output on non-Windows platforms.

Component installed-state matching is target-qualified: rustup's
`component list --installed` returns names like
`clippy-x86_64-unknown-linux-gnu`, so a declared `clippy` matches any
installed entry that either equals `clippy` exactly or starts with
`clippy-`.

### `soldr optimize`

Apply platform-specific hot-cache optimizations to remove
build-time penalties caused by antivirus / real-time scanners. On
Windows this adds soldr-owned cache paths (and optionally the current
project's `target/`) to Windows Defender's exclusion list. On macOS
and Linux it is a no-op with a clear message — soldr's workloads do
not require exclusions there.

```bash
soldr optimize                                # default scope: all
soldr optimize --scope global                 # only ~/.soldr/* paths
soldr optimize --scope project                # only <workspace>/target
soldr optimize --scope all                    # both
soldr optimize --dry-run --json               # plan-only output
soldr optimize --undo --scope global          # reverse soldr-added exclusions
```

Flags:

- `--scope {global|project|all}` — what to optimize. Defaults to
  `all`.
- `--undo` — reverse exclusions previously added by soldr. Only
  paths tracked in `~/.soldr/managed-defender-exclusions.json` are
  removed; entries the user added by hand are never touched.
- `--dry-run` — print the plan and exit without invoking PowerShell.
- `--json` — emit the stable machine-facing JSON form.
- `--manifest-path <PATH>` — explicit `Cargo.toml` for the `project`
  scope. When unset, soldr walks up from the current directory.

Platform behavior:

| Platform | Behavior |
|---|---|
| Windows 10 | Add Defender exclusions. UAC self-relaunch when not admin. |
| Windows 11 pre-22H2 | Same as Windows 10, plus an info note about Dev Drive in 22H2. |
| Windows 11 22H2+ | Same as Windows 10, plus a Dev Drive suggestion when `fsutil devdrv` is supported. |
| Windows + Defender disabled | Prints "Defender not active; no exclusions needed." Exits 0. |
| Windows + no PowerShell on PATH | Hard error; exits non-zero. |
| macOS / Linux | No-op; exits 0 with a message. |

The global scope covers:

- `~/.soldr/cache`
- `~/.soldr/bench`
- `~/.soldr/runtime`
- `~/.soldr/state.sqlite3`
- Soldr's embedded-zccache state and session/report owner directory
  (`~/.soldr/cache/zccache`).
  When `ZCCACHE_CACHE_DIR` is set, that caller-selected zccache cache root is
  excluded explicitly.

The project scope covers `<workspace_root>/target/`, where
`<workspace_root>` is the nearest ancestor of the current directory
containing a `Cargo.toml`. Without `--manifest-path` and without a
matching ancestor, the command exits with `no Rust project detected`.

UAC self-relaunch:

When the current process is not elevated, soldr re-launches itself
via `Start-Process powershell -Verb RunAs --as-elevated-helper`. The
elevated child writes its JSON status to a temp file referenced by
the `SOLDR_OPTIMIZE_HELPER_OUTPUT` env var; the parent reads,
prints, and deletes it. The same exit code is propagated. If UAC is
denied or unavailable, soldr prints instructions for running the
helper from an elevated PowerShell and exits non-zero — soldr will
never silently elevate via tokens or scheduled tasks.

CI auto-skip:

`soldr optimize` detects CI via `GITHUB_ACTIONS`, `CI`, `BUILDKITE`,
`CIRCLECI`, `TRAVIS` (truthy values), or a non-empty `JENKINS_URL`,
and exits 0 with a clear message. Ephemeral runners discard
Defender state at job end, so adding exclusions is a no-op there.

Example JSON output (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "command": "optimize",
  "platform": "Windows10",
  "scope": "all",
  "undo": false,
  "dry_run": true,
  "defender_present": true,
  "defender_active": true,
  "actions": [
    {
      "path": "C:\\Users\\you\\.soldr\\cache",
      "action": "add",
      "scope": "global",
      "status": "planned"
    }
  ],
  "note": "dry-run: would add 6 paths to Defender exclusions"
}
```

### `soldr gc`

Review reclaimable Cargo `target/` directories tracked in
`~/.soldr/state.sqlite3`. Implemented by issue #234 and made safe-by-default
by issue #289. The wrapper-mode hot path upserts each invocation's
resolved workspace `target/` path with the current timestamp; `soldr gc`
walks the registry, drops missing rows, applies safety guards, and
prints an info summary without prompting or deleting anything.

```bash
soldr gc                                      # info summary only
soldr gc --json                               # machine-readable summary
soldr gc --older-than 30d --larger-than 1GB   # summary with tunable filters
soldr gc purge                                # interactive deletion flow
soldr gc purge --all                          # delete every eligible candidate
soldr gc purge --older-than 30d --larger-than 1GB
soldr gc purge --json                         # machine-readable purge report
soldr gc list --kind cargo_target_incremental # list one taxonomy kind
soldr gc purge --target-incremental --all      # delete target/<profile>/incremental/
soldr gc purge --build-scripts --all           # delete build-script-build binaries
soldr gc purge --doc --all                     # delete target/doc/
soldr gc purge --subcommand-caches --all       # delete target/{criterion,nextest,...}/
soldr gc purge --registry-src --all            # delete extracted registry sources
soldr gc purge --git-checkouts --all           # delete git checkout worktrees
soldr gc purge --kind rustup_toolchain --dry-run --all  # list unprotected toolchains; deletes nothing
soldr gc purge --kind rustup_toolchain --all   # uninstall them via `rustup toolchain uninstall`
```

Defaults:

- `--older-than 10d`
- `--larger-than 256M`

Compatibility:

- `soldr gc --dry-run` is accepted as a temporary alias for `soldr gc`
- `soldr gc --all` is rejected with a pointer to `soldr gc purge --all`

Safety guards (from `docs/TARGET_GC_PROPOSAL.md`):

- skip a candidate whose workspace `Cargo.lock` was modified within
  the staleness window
- skip a candidate whose `target/.cargo-lock` exists (active build)
- only consider paths under `gc.allowlist_roots` (default: `~/dev`)

The summary includes the registry path, eligible candidate count, total
reclaimable size, skipped/dropped counts, and the largest eligible
target directories with size and last-used age.

`gc list --json` uses the issue #323 taxonomy. Derived kinds can be
purged through explicit opt-in flags or `--kind`; the cargo-owned primary
kinds (`cargo_registry_cache`, `cargo_git_db`, `cargo_installed_binaries`)
remain report-only and `gc purge --kind <primary>` is rejected before
deletion for them. `rustup_toolchain` is also primary but is purgeable
through rustup itself — see the
`soldr gc purge --kind rustup_toolchain` section below.

Derived purge kinds:

- `cargo_target` (default `gc purge` behavior)
- `cargo_target_incremental`
- `cargo_target_build_script_binaries`
- `cargo_target_doc`
- `cargo_target_subcommand_caches`
- `cargo_registry_src`
- `cargo_git_checkouts`

Report-only primary kinds (rejected before deletion):

- `cargo_registry_cache`
- `cargo_git_db`
- `cargo_installed_binaries`

Primary but purgeable through rustup (soldr#3507):

- `rustup_toolchain`

**Eviction order and `in_worktree` (issue #2134).** `cargo_target`
entries carry `in_worktree`, and `age_seconds` is the same **effective**
age eviction uses — the more recent of soldr's registry stamp and the
directory's mtime, because the stamp goes stale while a directory stays
hot (a repo built with bare `cargo` never updates it). Eviction orders
`worktree → coldest → size`, so those two fields together explain why
any given target was chosen. Size is the last key, not the first: it
only breaks ties between equally cold targets.

`in_worktree` is omitted for kinds that have no owning workspace.

**`cargo_registry_src` last-used provenance (issue #349).** When
`soldr gc list --kind cargo_registry_src` (or unfiltered `gc list`)
walks `$CARGO_HOME/registry/src/...`, each entry's `last_used_unix`
field is preferentially derived from cargo's own
`$CARGO_HOME/.global-cache` SQLite tracker — the same data cargo's
unstable `-Zgc` uses to evict least-recently-used crate sources. When
the tracker is missing, locked, schema-drifted, or has no row for a
particular crate, soldr falls back to the directory's filesystem
mtime. The provenance is exposed as `last_used_source` on each entry:
`"global_cache"` or `"fs_mtime"`. The field is omitted from
`cargo_target` entries (no comparable tracker exists).

Configure additional allowlist roots via `~/.soldr/config.toml`:

```toml
[gc]
allowlist_roots = ["~/dev", "/work/repos"]
```

During build-like `soldr cargo ...` invocations, soldr checks free
space on the relevant target/current filesystem before spawning Cargo.
When less than 2 GB is available, it emits a yellow stderr warning that
recommends `soldr gc`. Disk-space detection failures are ignored so they
never fail the build.

### `soldr gc purge --kind rustup_toolchain` (soldr#3507)

Real deletion for installed rustup toolchains. Until #3507 this kind was
report-only ("cargo/rustup own deletion"), nothing ever pruned
`$RUSTUP_HOME/toolchains/`, and a long-lived soldr home accumulated every
channel any pin had ever named. Deletion is still owned by rustup: soldr
never removes the directory itself, it runs
`rustup toolchain uninstall <name>` per candidate with `RUSTUP_HOME`
pinned to the home the candidate was enumerated from.

Two homes are enumerated — each report line says which (soldr#1799):

- the caller's `$RUSTUP_HOME`, or `~/.rustup` when unset (`origin:
  "caller"`);
- soldr's managed `<root>/rustup`, where `soldr toolchain prepare`
  installs (`origin: "managed"`). This is where the 20 GB of #3507
  lived, and it used to be invisible to `gc list`.

```bash
soldr gc purge --kind rustup_toolchain --dry-run --all   # what would go; deletes nothing, never spawns rustup
soldr gc purge --kind rustup_toolchain                   # prompt per candidate (closed stdin answers no)
soldr gc purge --kind rustup_toolchain --all             # no prompt
soldr gc purge --kind rustup_toolchain --all --json      # machine-readable report
```

Safety rails — a toolchain is never a candidate when it is:

- the enumerated home's default toolchain (`settings.toml`), or one of
  that home's `[overrides]` toolchains (rail: active/default);
- the toolchain this invocation resolves to: `$RUSTUP_TOOLCHAIN`, or the
  `rust-toolchain.toml` channel found in the working directory's
  ancestors — which also covers the repo's stable pin, e.g. `1.98.1`
  inside the soldr tree (rails: active pin + repo pin);
- anything at all, when that home's `settings.toml` exists but cannot be
  read or parsed — an unreadable default fails closed for the whole home
  and the report says so.

Matching is exact-or-triple-aware — a pin of `1.98.1` protects the
installed directory `1.98.1-x86_64-unknown-linux-gnu` — and
version-segment aware: `channel = "1.95"` protects the resolved
`1.95.0-...` directory. Both rules run one-directional (protected →
installed), so pins only ever over-protect; they can never match past a
different version. A bare `nightly` never covers a dated
`nightly-YYYY-MM-DD` snapshot, so old dailies stay eligible while the
current `nightly` stays protected.

`--dry-run` is only accepted for this kind; every other `gc purge` kind
rejects it rather than silently ignoring it. `--older-than` /
`--larger-than` are `target/` knobs and do not apply here: toolchain
eligibility is decided by the protection rails alone, not by directory
age (a toolchain's mtime is not a reliable last-used signal). Unlike the
other gc prompts, a closed stdin answers **no** — bulk deletion requires
`--all`.

JSON shape (`schema_version: 3`), one row per enumerated home:

```json
{
  "schema_version": 3,
  "command": "gc",
  "mode": "purge",
  "kind": "rustup_toolchain",
  "dry_run": true,
  "selected_count": 1,
  "uninstalled_count": 0,
  "failed_count": 0,
  "reclaimed_bytes": 0,
  "reclaimed_human": "0 B",
  "homes": [
    {
      "rustup_home": "/home/me/.rustup",
      "origin": "caller",
      "installed_count": 17,
      "default_toolchain": "stable-x86_64-unknown-linux-gnu",
      "settings_unreadable": false,
      "protected": ["1.98.1", "stable-x86_64-unknown-linux-gnu"],
      "candidates": [
        {"toolchain": "1.70-x86_64-unknown-linux-gnu", "size_bytes": 1200000000, "size_human": "1.2 GB"}
      ],
      "selected": [
        {"toolchain": "1.70-x86_64-unknown-linux-gnu", "size_bytes": 1200000000, "size_human": "1.2 GB"}
      ],
      "uninstalled": [],
      "failures": []
    }
  ]
}
```

`selected` mirrors `candidates` under `--dry-run` (a dry-run cannot
prompt, so it reports the `--all` shape); after a real purge it holds the
chosen subset. `failures` entries are `{toolchain, error}` and a
per-candidate rustup failure does not abort the rest of the run.

### `soldr gc cargo` (issue #323)

Shells out to nightly cargo's unstable `-Zgc clean gc` against
`$CARGO_HOME`. The CLI prepends `rustup run <toolchain>` so the workspace
`rust-toolchain.toml` is bypassed — `cargo` here always means "the cargo
shipped with the requested toolchain".

```bash
soldr gc cargo                                  # nightly, conservative defaults
soldr gc cargo --dry-run --json                 # plan + machine-readable report
soldr gc cargo --max-src-age 7days --max-crate-age 14days
soldr gc cargo --toolchain nightly-2026-01-01   # pin the nightly snapshot
```

Toolchain resolution: `--toolchain` flag → `$SOLDR_GC_CARGO_TOOLCHAIN`
→ `nightly` default. Missing toolchain is a hard error from explicit
`gc cargo`; the `gc sweep` orchestrator downgrades it to a skip so CI
runners without nightly still get the soldr target purge stage.

JSON shape (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "command": "gc",
  "mode": "cargo",
  "toolchain": "nightly",
  "exit_code": 0,
  "dry_run": false,
  "args": ["-Zgc", "clean", "gc", "--max-src-age=7days"],
  "stdout_bytes": 612,
  "stderr_bytes": 0,
  "skipped": false,
  "skipped_reason": null
}
```

### `soldr gc locations` (issue #323)

Read-only enumeration of every cache directory soldr cares about. No
deletion, no last-used derivation. Walks `$CARGO_HOME/{registry/{src,
cache,index},git/{db,checkouts},.global-cache}`,
`$RUSTUP_HOME/{toolchains,update-hashes}`, `~/.soldr/cache/`, and
`~/.soldr/state.sqlite3`. Missing paths are reported with `exists: false`
and zero size.

```bash
soldr gc locations
soldr gc locations --json
```

Per-entry JSON: `{kind, path, exists, size_bytes, size_human, file_count,
owner, purge_safety}`. `owner` is `cargo` / `rustup` / `soldr`;
`purge_safety` is `regenerable` (safe to delete; cargo will refetch) or
`user_action` (the user installed it on purpose — never auto-purge).

### `soldr gc sweep` (issue #323)

Orchestrator that combines `gc locations`, cargo's `clean gc`, and the
soldr target purge in one shot. Designed to be the user-facing "free
me some disk space" command.

```bash
soldr gc sweep                            # full pipeline, prompt for each target
soldr gc sweep --all --dry-run --json     # plan everything, delete nothing
soldr gc sweep --no-cargo-gc              # skip cargo (e.g. no nightly available)
soldr gc sweep --aggressive               # second cargo pass with tighter ages
```

Stages, in order:

1. `gc locations` (always — read-only).
2. cargo `clean gc` with conservative defaults (unless `--no-cargo-gc`
   or nightly is missing — auto-skipped).
3. soldr's target purge over registered workspaces. Respects `--all`
   (no prompt) and the configured `gc.allowlist_roots`.
4. `--aggressive` only: second cargo pass with
   `--max-src-age=7days --max-crate-age=14days --max-git-co-age=7days`,
   each clamped to `auto_gc.min_age_secs`.

**Target registration (soldr#3645).** The rustc wrapper resolves the workspace target directory for build-lifecycle records and GC registration from `CARGO_TARGET_DIR` (absolute, or relative resolved against the invocation's current directory, matching Cargo). Otherwise it walks up from rustc's `--out-dir` to the nearest ancestor named `target`, or failing that the nearest ancestor carrying Cargo's target-root markers (`CACHEDIR.TAG` / `.rustc_info.json`), so non-`target`-named directories such as `build/` or `out/` are registered. If nothing resolves, the wrapper silently skips registration.

### `soldr gc target` (issue #574)

Cross-repo `target/` reclamation: walks a configurable root, finds every
workspace with a sibling `target/` directory, and reports their sizes
(sorted descending). With `--purge` it deletes them after a single
confirmation prompt (or `--yes` for non-interactive automation).

```bash
soldr gc target                                    # report under ~/dev (default)
soldr gc target --root ~/code --max-depth 6        # custom root and walk depth
soldr gc target --dry-run --json                   # machine-readable report
soldr gc target --purge                            # interactive deletion
soldr gc target --purge --yes                      # non-interactive deletion
soldr gc target --purge --yes --json               # CI-friendly purge + JSON
```

Options:

- `--root <PATH>` — walk root. Falls back to `$SOLDR_GC_TARGET_ROOT` then
  `~/dev`.
- `--max-depth <N>` — maximum directory depth for the workspace scan
  (default `4`). Hidden directories (leading `.`) and any directory
  named `target` are skipped.
- `--dry-run` — report-only (default).
- `--purge` — delete every reported `target/` directory.
- `--yes` — skip the y/n confirmation prompt. Required when stdin is
  not a terminal (CI).
- `--json` — emit the stable machine-facing payload (`schema_version: 1`).

JSON shape (stable):

```json
{
  "schema_version": 1,
  "command": "gc target",
  "mode": "report" | "purge",
  "root": "/home/me/dev",
  "max_depth": 4,
  "entry_count": 0,
  "total_bytes": 0,
  "total_human": "0 B",
  "entries": [
    {
      "workspace_root": "/home/me/dev/foo",
      "target_dir": "/home/me/dev/foo/target",
      "size_bytes": 0,
      "size_human": "0 B",
      "file_count": 0,
      "last_modified_ms": 0
    }
  ],
  "purged_count": 0,
  "failed_count": 0,
  "purged_bytes": 0,
  "purged_human": "0 B",
  "failures": []
}
```

The walker is independent of the per-repo `gc list` taxonomy above —
it scans real filesystem trees rather than the soldr registry, so it
finds workspaces that have never gone through `soldr cargo ...`.

### Host-volume disk watchdog (issue #574)

Before every `soldr cargo ...` build, soldr probes free space on the
build volume (the disk hosting the project's `target/` dir, or CWD if
`target/` doesn't exist yet) and either warns or aborts:

- Above `SOLDR_TARGET_WARN_FREE_GB` (default 10 GiB) — silent.
- Between warn and `SOLDR_TARGET_BLOCK_FREE_GB` (default 5 GiB) —
  one-line stderr warning pointing at `soldr gc target`.
- Below the block threshold — cargo is refused with a clear error
  directing the user at `soldr gc target --purge`.

Set `SOLDR_TARGET_AUTO_PRUNE_ENABLED=0` to disable the watchdog
entirely. The watchdog is layered on top of the legacy 2 GiB low-disk
advisory (issue #289) and never replaces it.

### Daemon-owned cache maintenance (issues #1762–#1764)

Each `soldr-daemon` owns exactly the one root selected by `SoldrPaths`:
production `.soldr`, development `.soldr-dev`, or an explicit
`SOLDR_CACHE_DIR`. Embedded zccache remains below that root. Standalone
`.zccache` is owned only by a standalone zccache daemon; Soldr never discovers,
migrates, links into, or deletes it.

The daemon performs a lightweight pressure check every five minutes and a full
age pass every 24 hours, and exits after 30 minutes without work. The last
completed full pass and the latest structured report are stored under
`<root>/cache/soldr-daemon/`, so an overdue restart catches up immediately and
the schedule survives the exit. `soldr daemon start --idle-timeout <seconds>`
sets a different budget.

`soldr-daemon --owner-pid <PID>`, or `SOLDR_DAEMON_OWNER_PID`, ties a daemon's
lifetime to another process: it exits once that process is gone, however the
process ended. This is for callers that own a throwaway root, such as a test
fixture with its own `SOLDR_CACHE_DIR`. A broker route is keyed on the soldr
root, so each such caller gets a daemon of its own, and without an owner a
fixture that is killed rather than shut down leaves that daemon resident.
Liveness is polled at the idle interval, so a PID reused within one interval
can be mistaken for the original owner. Soldr does not install Task Scheduler jobs, systemd timers,
launchd agents, or any other OS scheduler.

The embedded artifact budget defaults to 5% of filesystem capacity, clamped to
40–200 GiB and reduced when needed to preserve recovery space. At 85% fill it
evicts LRU artifacts older than four days toward 70%; at 100% fill or critically
low free space it evicts regardless of age toward 80% plus recovery headroom.
A daily pass expires artifacts older than 30 days even below budget. Physical,
hardlink-deduplicated allocated bytes are used. Set exactly one of
`ZCCACHE_CACHE_SIZE_BYTES` or `ZCCACHE_CACHE_SIZE_PERCENT` to override the
dynamic budget.

The same coordinated pass bounds cook artifacts, trash buckets, registered
workspace targets, PEP517 targets and wheels, daemon events, stale embedded generations,
and `cache/zccache/history`. Build history defaults to 72 hours and 1 GiB per
root; active publishers survive, while abandoned unfinished rows do not block
retention forever. Expired database records retain their useful metrics with
archived paths marked unavailable. The first pass
after the zccache#1149 integration removes completed pre-redaction histories.
The daily 30-day absolute bound overrides cook's `keep_per_origin` protection,
so every abandoned cook origin eventually expires.

Builds hold a shared root-maintenance lease from before `BuildSessionStart`
through sanitized archive publication. Maintenance holds the exclusive side
for the complete pass, so a build cannot begin in the probe/delete gap. Daemon
shutdown lets an already-started pass finish. Soldr embeds zccache with
host-owned maintenance, so zccache does not also start its standalone periodic
maintenance scheduler inside the same process.

`soldr status`, `soldr cache`, and `soldr doctor` expose the owning root,
identity, embedded root, effective budget policy, measured usage/fill/free
space, last attempt/success, pressure tier, reclaimed bytes/items, and
component errors. For an orphaned root, the only manual surface is explicit:

```text
soldr gc maintain --root /absolute/path/to/the/root [--json]
```

It refuses relative paths, symlinks/junctions/reparse points, any version-blind
daemon endpoint occupant, and any root whose daemon-ownership lock is busy. It
holds that ownership lock for the full pass and never searches for sibling
roots. Deferred or partially failed manual passes print their structured status
but exit nonzero, so emergency cleanup scripts cannot mistake a no-op for
success.

### Invocation fallback and Cargo-volume GC (issue #323)

The daemon is primary for Soldr-owned state. The cargo front door retains its
post-build detached fallback for Cargo-owned global caches and registered
workspace targets when free
space on any soldr-relevant volume drops below the configured trigger.
Per volume (Windows: drive letter; Unix: device id):

1. tier 1 — cargo `clean gc` with conservative ages,
2. tier 2 — soldr target purge with `larger_than = 256M` and
   `older_than = max(1h, auto_gc.min_age_secs)`,
3. tier 3 — cargo `clean gc` with `--max-src-age=7d
   --max-crate-age=14d --max-git-co-age=7d`,
4. stop. Anything more aggressive requires explicit
   `soldr gc sweep --aggressive`.

Configure in `~/.soldr/config.toml`:

```toml
[auto_gc]
enabled = true          # opt-out; on by default
trigger_free_gb = 20    # start GC when free space < this
target_free_gb = 30     # stop GC when free space >= this
min_age_secs = 3600     # never touch anything modified within this window
```

Fallback behavior:

- Background-only. A detached `soldr gc auto-sweep` process survives the
  wrapper exit, so the build never blocks waiting for Cargo-volume GC.
- Throttled to ~once per 5 minutes via `~/.soldr/.auto_gc_marker`.
- Set `SOLDR_AUTO_GC_DISABLED=1` to disable for a single invocation
  without editing config.
- Volumes that already have plenty of free space are skipped, even
  when another volume on the same machine is below the trigger.
- Every check that crosses the trigger writes a structured log line to
  `~/.soldr/logs/auto-gc.log`. The file rotates to
  `auto-gc.log.old` once it exceeds 10 MiB.
- Cargo's `.package-cache` mutex serializes auto-GC against any
  in-flight `cargo build`, so concurrent builds and GC don't race.

---

## Per-Build XML Log (issue #1790)

Every managed `soldr cargo ...` build — success or failure, cache enabled or
disabled — writes one self-contained XML file:

```text
~/.soldr/logs/builds/<timestamp>-<sanitized-cwd>.xml
```

The directory is flat and the filename's compact UTC timestamp prefix
(`YYYYMMDDTHHMMSSZ`) sorts lexically = chronologically, so `ls` /
`Get-ChildItem` in that directory is already newest-last. The cwd suffix is a
lowercased, filename-safe slug (non `[a-z0-9]` bytes collapsed to `-`, capped
at 80 chars) so logs from different checkouts of the same repo (or different
repos entirely) don't collide.

The write is always-on and best-effort: a failure to write the log never
fails the build — soldr prints `soldr warning: failed to write build log:
<err>` to stderr and continues.

The format was originally JSON and was converted to XML by owner decision:
the log is dominated by repeated attribute-blocks inside group nodes (one
record per compiled crate, with the same handful of build-settings fields
stamped on every group), which XML's attribute-on-element shape expresses
more naturally than JSON's per-item object repetition. The emitter is
hand-rolled (no new dependency).

### Schema (`schema_version: 1`)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<build schema_version="1" soldr_version="0.8.21" cwd="C:\Users\niteris\dev\soldr2" started_at_ms="0" ended_at_ms="0" duration_ms="0" exit_code="0">
  <args>
    <arg>cargo</arg>
    <arg>build</arg>
    <arg>--release</arg>
  </args>
  <steps>
    <download wall_ms="0" cpu_ms="0">
      <item name="cargo-nextest" source="github-release" started_at_ms="0" duration_ms="0"/>
    </download>
    <compile wall_ms="0" cpu_ms="0" target="x86_64-pc-windows-msvc" profile="release" debug="false" opt_level="3" lto="off">
      <item crate="foo" duration_ms="0" cache="hit"/>
    </compile>
    <link wall_ms="0" cpu_ms="0" derived="true" target="x86_64-pc-windows-msvc" profile="release" debug="false" opt_level="3" lto="off">
      <item crate="foo" duration_ms="0"/>
    </link>
  </steps>
  <totals wall_ms="0" cpu_ms="0" crate_count="0" cache_hits="0" cache_misses="0"/>
</build>
```

- **`<build>` header attributes** — `cwd`, `started_at_ms` / `ended_at_ms` /
  `duration_ms`, `exit_code`. The full invoked argv is the child `<args>`
  element (one `<arg>` per token) rather than an attribute, since argv is
  variable-length.
- **build settings on `<compile>` and `<link>`** — there is no separate
  `<settings>` element. Instead, the derived `[profile.*]` metadata
  (`target`, `profile`, `debug`, `opt_level`, `lto`) — read from the invoked
  flags plus the target `Cargo.toml`'s `[profile.<name>]` table — is stamped
  as attributes on BOTH the `<compile>` and `<link>` group nodes, so each
  group is self-describing on its own. `RUSTFLAGS` / `CARGO_PROFILE_*`
  environment overrides are not accounted for in v1.
- **`<steps><download>`** — any tool/artifact fetches (e.g. `cargo-nextest`,
  `zccache`) that happened during the build, one self-closing `<item>` per
  fetch with `name`, `source` (`"github-release"`, `"crates-io"`,
  `"catalogue"`, etc.), `started_at_ms`, and `duration_ms`.
- **`<steps><compile>`** — one `<item>` per crate compiled during the
  session (`crate`, `duration_ms`, `cache`: `"hit"` / `"miss"` /
  `"unknown"`), sourced from the daemon's build-history DB and
  cross-referenced against the zccache compile journal for cache outcome.
- **`<steps><link>`** — v1 has no independently-measured link phase, so this
  section is *derived*: the compile event with the latest end timestamp is
  treated as the linking crate and echoed here with `derived="true"`. That
  crate's entry therefore also appears in `<compile>` — this is
  intentional, not a double-count bug.
- **`wall_ms` vs `cpu_ms`** — `wall_ms` is calendar time spanning the
  earliest start to the latest end within a group. `cpu_ms` is
  *aggregate busy time summed across (possibly parallel) units* — e.g. the
  sum of every compile's duration — **not** OS-reported CPU time. On a
  build with N-way parallelism, a group's `cpu_ms` can comfortably exceed
  its `wall_ms`. Note: `<totals>` `cpu_ms` excludes the derived `<link>`
  group's `cpu_ms` (it re-labels a slice already counted in `<compile>`, so
  adding it again would double-count).
- **`<totals>`** — build-wide `wall_ms` / `cpu_ms` rollup, `crate_count`,
  and `cache_hits` / `cache_misses` (falls back to the zccache session-stats
  summary when no per-crate cache outcome could be resolved from the
  compile journal).
- Empty groups (e.g. no fetches happened) render as a self-closing element
  with only the group's own attributes, e.g. `<download wall_ms="0"
  cpu_ms="0"/>`.
- All attribute values and `<arg>` text content are XML-escaped (`&`, `<`,
  `>`, `"`, `'`, and stray control characters below `0x20`).

### Retention

The same detached `soldr gc auto-sweep` process that runs the Cargo-volume
GC pass above (throttled to ~once per 5 minutes) also prunes
`~/.soldr/logs/builds/` down to the newest 100 files, sorted by filename
(so oldest-timestamp files are removed first). This matches both `*.xml`
files and any legacy `*.json` files left over from interim builds before the
JSON->XML conversion, so old logs in either format still get GC'd. No
separate throttle or opt-out exists for this prune — it rides the existing
auto-GC sweep unconditionally.

---

## Structured JSON Output

The supported JSON protocol currently exists on:

- `soldr status --json`
- `soldr cache --json`
- `soldr cache prune-target <path> --json`
- `soldr version --json`

The JSON response always includes:

- `schema_version`
- `command`

Current schema version:

- `schema_version: 1`

Compatibility rules for schema version `1`:

- existing fields keep their current meaning
- fields may be added in later releases without changing `schema_version`
- removing a field, renaming a field, or changing the meaning/type of an existing field requires a new schema version
- human-readable stdout for commands without `--json` is not covered by this compatibility promise

Example:

```json
{
  "schema_version": 1,
  "command": "version",
  "soldr_version": "0.7.4"
}
```

---

## Help Surface

```text
Usage:
  soldr <COMMAND>
  soldr <TOOL>[@version] [args...]

Commands:
  cargo    Run Cargo through soldr
  status   Show cache status and tool info
  clean    Clear caches
  config   Show or set configuration
  cache    Inspect the compilation cache
  version  Show version
  gc       Review reclaimable Cargo target/ directories; use gc purge to delete
```

---

## Environment Variables

| Variable | Purpose | Default |
|---|---|---|
| `RUSTC_WRAPPER` | Internal build hook used by `soldr cargo ...` | unset |
| `SOLDR_CACHE_ENABLED` | Internal toggle propagated from `soldr cargo ...` into wrapper mode | `1` |
| `SOLDR_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN` | Credentials for GitHub release lookups and `soldr install` codeload source-zip downloads. `SOLDR_GITHUB_TOKEN` is always sent. Inside GitHub Actions (`GITHUB_ACTIONS=true`), `GH_TOKEN`/`GITHUB_TOKEN` are sent only for the workflow's own repository (`GITHUB_REPOSITORY`); requests to other repositories go unauthenticated unless `SOLDR_GITHUB_TOKEN` is set (soldr#3639). | unset |
| `SOLDR_RUSTC_WRAPPER` | Replace Soldr's normal embedded-zccache route with another wrapper binary, or disable rustc wrapper injection with `none` / empty while leaving other Soldr front-door behavior intact | unset |
| `SOLDR_REAL_<TOOL>` (one per toolchain binary) | Internal real-tool path overrides used by setup-soldr PATH shims to avoid recursive tool lookup | unset |
| `SOLDR_ZCCACHE_BIN` | Legacy compatibility variable; it does not replace the embedded service on the normal `soldr cargo ...` path. Use `SOLDR_RUSTC_WRAPPER=/path/to/zccache` for an intentional external-wrapper experiment. | unset |
| `SOLDR_ZCCACHE_LOCAL_DIR` | Legacy compatibility variable from the removed downloaded-zccache flow; ignored by the normal embedded path. | unset |
| `SOLDR_CACHE_DIR` | Override the exact product root owned by this soldr daemon. **On Windows, keep this path short** — see the note below. | official builds: `~/.soldr`; development builds: `~/.soldr-dev` |
| `ZCCACHE_CACHE_SIZE_BYTES` | Exact embedded artifact budget in bytes. Mutually exclusive with `ZCCACHE_CACHE_SIZE_PERCENT`. | dynamic 5%, clamped 40–200 GiB |
| `ZCCACHE_CACHE_SIZE_PERCENT` | Embedded artifact budget as an integer percentage from 1 through 100. Mutually exclusive with `ZCCACHE_CACHE_SIZE_BYTES`. | dynamic 5%, clamped 40–200 GiB |
| `SOLDR_CACHE_LIFECYCLE` | Embedded-cache durability policy for `soldr cargo ...`. `job` leaves normal background persistence to the long-lived Soldr daemon. `command` requests an embedded cache flush before the command exits; it does not stop the root-owning daemon. | `job` |
| `SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS` | Positive compatibility timeout accepted with command-lifetime mode; empty, zero, or malformed values fall back to the default (malformed/zero warn on stderr). Embedded mode uses it to select the flush-on-command path; there is no separate zccache daemon to stop. | `300` |
| `SOLDR_TRUST_INHERITED_ENV` | Advanced escape hatch for CI/action workflows that intentionally inject soldr/zccache workspace-pinned env into `soldr cargo ...`. Truthy values are equivalent to `--trust-inherited-soldr-env`; unset/default means `soldr cargo ...` derives a fresh soldr workspace context from the current cwd/manifest while preserving normal OS, Cargo, Rust, proxy, cert, and CI env. | unset |
| `SOLDR_RELOCATED_EXE` | Internal recursion guard set after Windows self-relocation | unset |
| `SOLDR_ORIGINAL_EXE` | Internal path to the original executable when Windows self-relocation is active | unset |
| `SOLDR_ZCCACHE_SESSION_DIR` | Internal session/report directory passed from `soldr cargo ...` into wrapper mode | unset |
| `SOLDR_ZCCACHE_PRIVATE` | Opt-in private auxiliary session root. When truthy (`1`/`true`/`yes`/`on`), `soldr cargo ...` routes that state to `<cwd>/.zccache` and `soldr save`/`soldr hydrate` (`load` alias) default `--cache-dir` to the same path when omitted. It does **not** relocate the compiler-artifact service embedded in `soldr-daemon`; use `SOLDR_CACHE_DIR` for a fully isolated embedded compiler store. Explicit `ZCCACHE_CACHE_DIR` (front door) or `--cache-dir` (save/load) always wins. | unset |
| `SOLDR_SAVE_PROFILE` | Default payload profile for `soldr save` when `--ci` / `--minimal` is not passed. Values: `full`/`default`/`complete` for historical all-files archives; `ci`/`minimal` for the CI/minimal profile that excludes runtime-only files, zccache runtime binaries, and reports `excluded_files` / `excluded_bytes`; or `cook` for archiving a cargo **target directory's dependency graph** while dropping the linked products soldr#2931 classifies tier 3. CLI flags win over the env var. | `full` |
| `ZCCACHE_CACHE_DIR` | Auxiliary zccache front-door/session and direct-rustfmt cache-root override. It does not relocate the compiler service embedded in `soldr-daemon`; use `SOLDR_CACHE_DIR` for that. `soldr cargo ...` ignores inherited values by default so stale workspace state from setup/action wrappers cannot bleed across projects; pass `--trust-inherited-soldr-env` or set `SOLDR_TRUST_INHERITED_ENV=1` only when intentionally injecting this state. | unset |
| `ZCCACHE_SESSION_ID` | Per-build zccache session identifier set by soldr | unset |
| `SOLDR_RUSTFMT_ADMISSION_TRACE` | Truthy values print the daemon resident-permit lease acquire/release line for each cached `rustfmt` child (soldr#2877). | unset |
| `SOLDR_NATIVE_CACHE` | Native C/C++ compiler cache toggle. Falsy values (`0`/`false`/`no`/`off`) disable only cc-rs `CC`/`CXX` wrapper injection, leaving rustc-side zccache enabled. Useful when a target cross compiler, such as the managed MinGW `gcc.exe` / `g++.exe` path, must run directly while Rust compilation still uses the cache. | unset (on) |
| `SOLDR_CARGO_WAIT_TIMEOUT_SECS` | Opt-in wall-clock watchdog for the Cargo child. Normal `soldr Cargo ...` invocations have no Soldr-imposed wall-clock deadline and may run for hours. A timeout terminates the process tree, records abort diagnostics, and returns failure without changing compile topology. | unset (no deadline) |
| `SOLDR_NESTED_CARGO` | Response of the nested-Cargo self-lock guard (soldr#2924). The Cargo front door observes its Cargo child's process tree; a build-like Cargo started by a build script (or compiler) of a running Cargo, with no Soldr process in between and no explicit `--target-dir` distinct from that Cargo's target, would wait on the held build-directory lock forever. `enforce` (default) prints one bounded diagnostic, terminates the Cargo tree, and exits 1; `report` warns without terminating; `allow` is the explicit permit for a nested build isolated in a way the observer cannot prove (for example an env-only `CARGO_TARGET_DIR`). Every detection writes a redacted JSON record under `<soldr root>/logs/nested-cargo/`, naming the permit when one was used. The value is consumed by the front door and removed from Cargo's environment, so it covers exactly one outer run and never reaches a nested Soldr entry; it does not affect the `IN_SOLDR_PID` re-entrancy guard. Unrecognized values warn and enforce. Armed on Linux and macOS; not armed on Windows, whose descendant observer reports no parent pids. | unset (`enforce`) |
| `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` | Overrides the compile-dispatch reply timeout. Default is 30 min so a legitimate slow release compile is never cut off; set a small value (e.g. `30`) to fail fast instead of waiting out the backstop if the daemon stops responding. `0`/empty/unparseable falls back to the default. | 1800 |
| `SOLDR_COOK_NO_PROGRESS_SECS` | No-progress watchdog for `soldr cook`'s cargo-chef `prepare`/`cook`/exact phases (CI-silence follow-up to soldr#3043). Every 30s soldr samples the cook target directory for new build artifacts and resets the deadline on any change; a heartbeat line is logged every 5 minutes regardless. On expiry soldr writes a best-effort forensic dump (cook descendant process tree + any discoverable daemon/broker processes, plus native thread backtraces via `gdb`/`eu-stack` when present) under `<cache>/logs/cook-stall-<timestamp>/`, terminates the cook process tree, and fails with a message naming the dump path. `0` disables the watchdog entirely; unset/unparseable falls back to the default. | 900 |
| `ZCCACHE_PATH_REMAP` | zccache path-remap mode. soldr seeds `auto` on the child cargo for managed-zccache builds so multiple git worktrees of the same repo share cache hits (issue #352, Tier L1.x). Caller-supplied values are preserved. Works for non-git checkouts too: since zccache#353, `ZCCACHE_PATH_REMAP=auto` with no `.git/` ancestor falls back to the cwd as the remap root and still injects `--remap-path-prefix=<cwd>=.`, so tarball/zip/git-archive checkouts produce path-independent artifacts and share hits (the `.git/` walk is only how the preferred worktree root is discovered). | unset (soldr injects `auto`) |
| `SOLDR_PATH_REMAP` | Escape hatch for the default `ZCCACHE_PATH_REMAP=auto` injection. `off` (case-insensitive) suppresses the injection; any other value, or unset, keeps the default behavior. | unset (`auto`) |
| `SOLDR_ZCCACHE_MODE` | How the embedded zccache delivers a cache hit (zccache#1683, soldr#3407, zccache#1792): `link`, `copy` (always an independent writable copy), `reflink` (an independent clone, a copy where the volume cannot clone), or `reflink-or-link-or-copy` (clones, else hardlinks, else copies — the old meaning of `auto`, added in zccache 1.15.0); case-insensitive. `auto`, and leaving this and every other tier unset, means "soldr decides": zccache's own `AUTO` no longer hardlinks (zccache#1792), so soldr probes, once, whether the pair of directories a cache hit actually moves between (the zccache cache dir and the cargo target dir) supports a reflink or a hardlink, and injects the single explicit answer — `REFLINK`, `LINK`, or `COPY` — instead (soldr#3440). This also applies when your own `ZCCACHE_MODE` names `AUTO`. `--zccache-mode MODE` sets it. Precedence: `--zccache-mode` / `SOLDR_ZCCACHE_MODE` > `[zccache] mode` in `config.toml` > your own `ZCCACHE_MODE`. Soldr sets the winner as `ZCCACHE_MODE` on the cargo child; the embedded service reads it per compile, so no daemon restart is needed. An unknown value in any tier fails the build before cargo runs. | unset (soldr probes and injects `REFLINK`, `LINK`, or `COPY`) |
| `ZCCACHE_MODE` | zccache's own spelling of the delivery mode. An explicit `LINK`/`COPY`/`REFLINK`/`REFLINK_OR_LINK_OR_COPY` is passed through untouched when no higher-precedence soldr-level setting is present. `AUTO` (or an absent variable) is replaced with soldr's one-time probe result instead of reaching zccache unaided — see `SOLDR_ZCCACHE_MODE` (zccache#1792). `REFLINK`, `LINK`, and `COPY` already exist in the pinned zccache 1.14.14, so this needs no zccache version bump (soldr#3440). | unset (soldr probes and injects `REFLINK`, `LINK`, or `COPY`) |
| `SOLDR_TIMESTAMP_LINES` | Prefix each relayed output line with elapsed seconds since soldr start (`  12.34 `), color-preserving, so per-line cost is visible in a log. A single `# t0=<epoch-seconds>` header anchors absolute time. Applies to the cargo front door and the PEP 517 build backend; both stamp piped/non-TTY output by default and leave an interactive terminal unstamped. `1`/`true`/`yes`/`on` (case-insensitive) forces stamping on (e.g. on a TTY); `0`/`false`/`no`/`off` forces it off. Diagnostic capture, cargo-JSON parsing, and the archived build log always see the unstamped bytes. | unset (on for non-TTY, off for TTY) |
| `SCCACHE_DIR` | sccache cache-root override soldr injects when `SOLDR_RUSTC_WRAPPER=sccache` and the caller has not set it themselves | `~/.soldr/cache/sccache` |
| `SOLDR_PEP517_STABLE_TARGET_DIR` | PEP 517 backend only: set to `0` / `false` / `no` / `off` to skip pinning `CARGO_TARGET_DIR` to the content-identified `<effective-soldr-root>/cargo-target/pep517/<project-id>` namespace for isolated builds (see [PEP 517 Build Backend](#pep-517-build-backend)). A caller-provided `CARGO_TARGET_DIR` always wins regardless. | unset (pin enabled) |
| `SOLDR_PEP517_PROJECT_ID` | Read-only diagnostic/cache identity exported by the PEP 517 backend. It identifies manifests, lockfile, toolchain/configuration, maturin settings, and build-policy environment; source freshness remains Cargo's responsibility. | content-derived |
| `SOLDR_PEP517_STATS` | PEP 517 wheel/editable build diagnostics: `off` (also `0` / `false`) suppresses stderr statistics; `full` emits the cache-session JSON after the default one-line summary; any other nonempty value selects the one-line summary. When unset, verbose frontends detected through `PIP_VERBOSE` or `UV_VERBOSE` select `full`. | concise summary |
| `SOLDR_PEP517_WHEEL_CACHE` | PEP 517 wheel/editable hooks: reuse the last successful wheel when the metadata fingerprint of sources, staged artifacts, prepared metadata, and build settings matches. `off` (also `0` / `false` / `none`) disables reuse. | unset (on) |
| `SOLDR_PEP517_LINKER` | PEP 517 backend only: `auto` (default) tries the fastest supported linker and caches a verified platform-linker fallback after a linker-availability failure; `none` / `default` / `off` disables the automatic attempt. An explicit `SOLDR_LINKER=fast` remains non-fallbacking. | `auto` |
| `SOLDR_FETCH_OVERLAP` | Kill switch for the `soldr build --target <T>` dependency-prefetch overlap (issue #1543). By default the blessed build spawns a best-effort `cargo fetch --target <T>` concurrently with catalogue/SDK preparation so fresh-build time approaches max(fetch, prepare) instead of their sum; the prefetch is joined before the main cargo build spawns and any prefetch failure is logged and ignored. The overlap is automatically skipped for `--offline` / `--frozen` builds, truthy `CARGO_NET_OFFLINE`, or when no `Cargo.lock` exists. Set to `0` / `false` / `no` / `off` (case-insensitive) to disable. | unset (on) |
| `SOLDR_ALLOW_UNPINNED` | Opt out of the toolchain-pin guard (soldr#1766) that refuses a build in a workspace with no `rust-toolchain.toml` / `RUSTUP_TOOLCHAIN` pin. Also set by `--allow-unpinned`. Only `1`/`true`/`yes`/`on` enable it; any other value, including unrecognised ones like `disabled`, leaves the guard on (soldr#3634). | unset (guard on) |
| `SOLDR_DYLINT_PREPARE_TTL_SECS` | Freshness window (seconds) for the dylint prepared-toolchain marker under `<soldr root>/dylint/prepared/v1/`. A fresh, valid marker lets a warm top-level `soldr cargo dylint` skip the nightly-map HTTP fetch and the `rustup component list` / `rustc -vV` probes entirely. `0` means never trust the marker (every run pays the full cold path). | `86400` (24 h) |
| `SOLDR_DYLINT_REVERIFY` | Truthy (`1`/`true`) bypasses the dylint prepared-toolchain marker and always re-runs the full catalogue-fetch + rustup verification path. Use after manually mutating the nightly toolchain or when diagnosing identity mismatches. | unset |
| `SOLDR_DYLINT_LIBRARY_SKIP` | **Opt-in** (soldr#2349). `on`/`1`/`true`/`yes` lets `soldr ci-test` skip the six `dylint-library-*` build stages when an input-keyed marker under `target/dylint/libraries/<channel>-<host>/.soldr-dylint-library-marker-v1.json` matches AND the tree holds a built `release/` payload. The marker hashes each lint's `src/**`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` and `.cargo/config.toml`, the nightly compiler identity, the resolved driver's content hash, and the same `RUSTFLAGS`/`CARGO_PROFILE_*`/wrapper env slice `dylint_cook.rs` hashes; `dylints/*/ui/**` is excluded by construction. Unset or any other value keeps the six stages running. Stays off (soldr#3132): skipping the library builds moves the Dylint UI-test compiles into Fresh Nextest's resident-memory peak, and the builds it would skip now take 7-12 s in total on a warm `main` gate (measured over three runs), so the default is not worth that exposure. Revisit only if those stages regress back to minutes. The marker is recorded either way, so a run with the skip off still leaves an accurate marker for a later opt-in run. | unset (off) |
| `SOLDR_DYLINT_DRIVER_FALLBACK` | `off`/`0`/`false`/`no` disables the cached local `dylint-driver` build that `soldr` falls back to when the soldr-toolchain catalogue publishes no driver asset for `(pinned nightly, host triple)` — routinely the case on macOS and Windows. The fallback builds once per `(channel, compiler release, compiler commit, cargo-dylint version, host triple)` tuple under an `fs2` exclusive lock, records the result `trust: unverified`, and is refused outright under `SOLDR_TRUST_MODE=strict`. It is also skipped when the driver's bounded `-V` probe *timed out* (soldr#2436's bounded-failure invariant): a driver binary that exists and hangs is a wedged host, not a missing asset, so soldr fails immediately with the hung-probe diagnostic rather than spending a `rustup component add` plus a full `rustc_private` build to reach the same verdict. | unset (on) |
| `SOLDR_RESOLVER_ORDER` | Comma-separated subset of `embed`, `live`, `api`, `binstall`, `quickinstall` (case-insensitive) selecting which tool-fetch resolver hops may run. The order is fixed (embed -> live -> api -> binstall -> quickinstall) and cannot be reordered, only disabled. `binstall` (#3726) and `quickinstall` (#3700) run only for exact versions after the `api` hop misses or is excluded. Unknown tokens are ignored with a warning; if no token is recognised (e.g. `bogus`) soldr warns and uses all hops, so a typo never disables the sha-pinned manifest hops. When `api` is excluded, a manifest miss fails with `no resolver hop permitted` instead of doing an unpinned GitHub Releases API lookup (#3640). | unset/empty (all five) |
| `SOLDR_BINSTALL_CRATES_BASE_URL` | Base URL the `[package.metadata.binstall]` hop (#3726) downloads `<base>/<crate>/<crate>-<version>.crate` from to read the crate's binstall metadata. Must be `https`. | `https://static.crates.io/crates` |
| `SOLDR_QUICKINSTALL_BASE_URL` | Base URL for the QuickInstall resolver hop (#3700); assets are fetched from `<base>/<crate>-<version>/<crate>-<version>-<target>.tar.gz`. For mirrors and hermetic fixtures. | `https://github.com/cargo-bins/cargo-quickinstall/releases/download` |
| `SOLDR_SOURCE_BUILD_CACHE` | Falsy (`0`/`false`/`no`/`off`) restores the historical fully-uncached source-build spawn. By default `soldr build-from-source` and dylint source preparation route compiler work through Soldr so fresh machines can reuse cached objects. | unset (cached) |
| `SOLDR_TOOLCHAIN_BIN_CACHE` | `off` (case-insensitive) disables the in-process memo and on-disk cache (`<soldr root>/cache/toolchain-bins/v2/<rustup-home+host-scope>/<channel>/<tool>.path`) for channel-scoped `rustup which` binary resolution. The cache saves one `rustup which` subprocess spawn per tool per nested cargo-dylint re-entry; entries self-invalidate when the cached path no longer exists, and the v2 scope prevents one toolchain home or host architecture from reusing another's path. | unset (on) |
| `DYLINT_DRIVER_PATH` | Soldr sets this on the dylint child process tree to `<soldr root>/dylint/drivers` (a stable soldr-owned home for cargo-dylint's per-toolchain driver builds) **only when the caller has not already set it** — an explicit caller value always wins. A fixed path means warm runs reuse the already-built driver and CI caches have a deterministic path to restore. | soldr-injected |
| `SOLDR_LINKER` | Pick the linker injected for `soldr cargo ...` builds (issue #285). Accepted values: `default` (no injection — keep the rust-toolchain default), `ld` (system linker — also no injection on every supported platform), `mold` (Linux only; hard error elsewhere), `rust-lld` (Windows MSVC and Linux/MinGW via `clang -fuse-ld=lld`; **no-op on macOS** — see below), `reld` (the polylinker, on every platform: driven through `clang --ld-path=reld` on Linux, direct on Windows/macOS), `fast` (reld — the fastest available; same as `reld`). **Unset defaults to `fast`, i.e. reld** (soldr#3262). The choice resolves to `CARGO_TARGET_<TRIPLE>_LINKER` injected into the spawned cargo process; Linux clang driver arguments are carried by a content-addressed generated linker shim instead of `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, so a project's `[build] rustflags` remain active (soldr#3277). The active target is the same one Cargo would pick (`--target` flag, `CARGO_BUILD_TARGET`, or the host triple). soldr skips the injection when a `CARGO_TARGET_<TRIPLE>_LINKER` / `_RUSTFLAGS` env var already carries a non-empty value, and — when the choice is the *automatic* default, i.e. neither `SOLDR_LINKER` nor `~/.soldr/config.toml` `linker =` is set — also when the project's own `.cargo/config.toml` declares `[target.<triple>] linker` or `rustflags` (soldr#3277: Cargo gives the env vars precedence over the config file, so the default would otherwise silently replace a linker the project declared for itself). Only exact-triple sections are detected; cfg-spec sections such as `[target.'cfg(all())']` are not. An explicit `SOLDR_LINKER=` request is a user decision and still overrides project config. A `linker = "..."` field in `~/.soldr/config.toml` is honored when the env var is unset. On macOS targets `rust-lld` falls back silently to the platform default linker (issue #509): Apple clang only accepts `-fuse-ld=lld` when the toolchain wires up an `ld64.lld` shim, and stock macOS toolchains do not — injecting it would break even `cc-rs` build-script compilations. `reld`/`fast` are unaffected and inject reld on macOS. **Selection precedence** (highest wins): `SOLDR_LINKER` env var > project or `$CARGO_HOME` `.cargo/config.toml` `[target.<triple>] linker`/`rustflags` > `Cargo.toml` `[workspace.metadata.soldr]`/`[package.metadata.soldr]` `linker` > `~/.soldr/config.toml` `linker =` > the automatic default (`fast`, i.e. reld). **On-demand `reld` fetch** (soldr#3276): when the resolved choice is an explicit `reld` request — `SOLDR_LINKER=reld`, `~/.soldr/config.toml` `linker = "reld"`, `[workspace|package.metadata.soldr] linker = "reld"`, or a bare `linker = "reld"` in a `.cargo/config.toml` `[target.<triple>]` section — soldr downloads a pinned, sha256-verified `reld` v0.1.0 on demand from `github.com/zackees/reld/releases` into `~/.soldr/bin/reld-0.1.0/` before cargo starts (Linux x64 hosts use the static musl asset), injects its absolute path, and fails hard with no fallback if the fetch fails. A bare `linker = "reld"` in cargo config is satisfied the same way by the managed binary; `--ld-path=reld` / `-C linker=reld` supplied via `rustflags` instead gets the managed reld directory prepended to `PATH` so the bare name resolves; an already-absolute path to a linker in cargo config is left alone (no fetch, no rewrite). The automatic default (`fast`/unset) never triggers a network fetch — it only uses a `reld` already discoverable on `PATH`. `SOLDR_RELD_BIN` (see below) overrides the managed install for reld development. | unset (resolves to reld) |
| `SOLDR_RELD_BIN` | Absolute path to a `reld` executable that overrides the managed on-demand install described under `SOLDR_LINKER`. Intended for reld development — build reld locally and point soldr at it instead of fetching the pinned release. | unset (managed install) |
| `SOLDR_OPTIMIZE_HELPER_OUTPUT` | Internal: set by the parent soldr process when it re-launches itself elevated via `--as-elevated-helper`. The elevated child writes its JSON status to this path so the parent can read and propagate it. | unset |
| `SOLDR_TARGET_WARN_FREE_GB` | Free-space threshold (in GiB) below which the host-volume disk watchdog (issue #574) emits a one-line stderr warning before `soldr cargo ...` dispatches the build. The watchdog probes the disk hosting the project's `target/` dir, falling back to CWD when `target/` doesn't exist yet. | `10` |
| `SOLDR_TARGET_BLOCK_FREE_GB` | Free-space threshold (in GiB) below which the host-volume disk watchdog (issue #574) refuses to start the build, pointing the user at `soldr gc target --purge`. Always strictly less than `SOLDR_TARGET_WARN_FREE_GB`; inverted thresholds collapse to "block wins". | `5` |
| `SOLDR_TARGET_AUTO_PRUNE_ENABLED` | Master toggle for the host-volume disk watchdog (issue #574). Falsy values (`0`, `false`, `no`, `off`, empty — case-insensitive) disable the watchdog entirely. | `1` |
| `SOLDR_GC_TARGET_ROOT` | Default walk root for `soldr gc target` (issue #574). The `--root <PATH>` flag always takes precedence. | `~/dev` |
| `SOLDR_TEST_DISK_FREE_BYTES` | Test seam for the watchdog: when set to a `u64` byte count (or `error`), overrides the real `fs2::available_space` probe so tests can drive every threshold edge. Internal — never set this in production. | unset |
| `SOLDR_TEST_FORBID_SOURCE_BUILD` | Test-only tripwire (soldr#2436): truthy values make every source-build chokepoint (`build-from-source` cargo install, toolchain plugin install) error with a distinctive message instead of spawning cargo, so containment tests can prove no implicit compile path is reached. Never set in production. | unset |
| `SOLDR_TEST_FORBID_TOOLCHAIN_INSTALL` | Test-only tripwire (soldr#3195): truthy values make every soldr path that would download a Rust toolchain, component or target (`rustup toolchain install`, `component add`, `target add`, the pinned-toolchain install, the rustup bootstrap, and `soldr rustup` passthroughs of those verbs) fail with a named diagnostic instead of invoking the real rustup. A fake rustup supplied through `SOLDR_TEST_RUSTUP_BIN` is allowed through. The Nextest wrapper sets it, together with `RUSTUP_AUTO_INSTALL=0`, for every Unix test process. Never set in production. | unset |
| `SOLDR_TEST_FORBID_TARGET_CONTAINING` | Test-only tripwire (soldr#3203): the absolute path of the running test binary, set by the nextest wrapper for every test process. A `soldr cargo` build or unmediated compile whose resolved Cargo target directory contains that binary fails with a `test tripwire:` diagnostic before any target hook runs, because its hooks would modify the suite's own `target/`. Give the fixture its own `CARGO_TARGET_DIR` or a temporary workspace. Never set outside tests. | unset |
| `SOLDR_PROFILE_EXTRACT` | Env-var equivalent of `soldr hydrate --profile-extract` (`load` alias) (issue #575). Any non-empty value other than `0` enables the per-phase profile line on stderr after a load (`zstd_decode`, `tar_parse`, `extract_total`, per-worker job counts, per-file `p50`/`p95`/`p99`). Useful for tuning the parallel-extract worker count against real workloads. | unset |
| `SOLDR_LOAD_WORKERS` | Cap on the parallel-extract worker pool used by `soldr hydrate` (`load` alias) (issue #575). Positive integer; wins over the explicit `--threads` flag. When unset, `--threads` (or rayon's `num_cpus` default) is used. | unset |
| `SOLDR_DAEMON_RSS_CEILING_BYTES` (soldr#3038 / soldr#3057) | Optional, fail-fast ceiling on the resident set size of `soldr-daemon` *and* its `soldr-broker`, in bytes. Unset means no watchdog task is even spawned in either process, and the sampled mimalloc-pprof profiler is never started — zero behavior change for ordinary builds. When set to a positive integer, each process samples its own RSS every 2 s (`daemon::rss_ceiling::RSS_SAMPLE_INTERVAL`; deliberately not the 5-minute cache-maintenance cadence — see that module's docs), records the running peak and mimalloc's own exact `heap.committed`/`heap.detailed` counters alongside the OS RSS reading to `<SOLDR_CACHE_DIR>/cache/soldr-daemon/rss-ceiling-v1.json` (broker: under its own broker-owned root, e.g. `~/.config/running-process/soldr-broker/cache/soldr-daemon/` on Linux — `daemon::service_definition::broker_owned_paths`), and — as of soldr#3057 — on the first sample that exceeds the ceiling, writes a full memory dump to `<cache>/cache/soldr-daemon/memory-breach-<unix_millis>-<pid>/` (`heap.pprof` sampled profile, `mimalloc-stats.json` exact counters, `cgroup.json` cgroup-wide memory/swap/pids counters plus the identities of the compiles in flight at breach time, `proc-self-status.txt` / `proc-self-smaps_rollup.txt` on Linux, `summary.json` naming the pid/role/ceiling/peak/dump path and each artifact's path) and calls `std::process::exit(1)` immediately — no graceful drain. soldr#3053 added `cgroup.json` and bumped the breach schema to `schema_version: 2` (`summary.json` gained `cgroup_path`, written with `#[serde(default)]` so a version-1 dump still deserializes): cgroup OOM counters on their own are not sufficient evidence — CLAUDE.md's "Diagnosing before capping" records kills that never incremented them — so the dump carries the cgroup boundary and the per-unit picture together. Every cgroup field is optional and serializes as `null` when unreadable, never as `0`. The in-flight list is always empty for a `broker`-role dump, because the broker runs no compiles; `summary.json`'s `role` is what tells that apart from an idle daemon. The sampled profiler (`mimalloc_pprof::prof::start`) is started at 16 KiB between samples (`daemon::rss_ceiling::HEAP_PROFILE_SAMPLE_INTERVAL_BYTES`) rather than the crate's own ~512 KiB default, so a process that breaches within seconds of startup still records real samples instead of an empty profile. `heap.pprof`'s `MAPPED_LIBRARIES` section names the exact on-disk binary and its load address ranges, so `addr2line -e <that binary>` (or `pprof -http=:0 <that binary> heap.pprof`) resolves the recorded stacks to real function names — this was verified directly against both a daemon and a broker breach dump while building soldr#3057 (e.g. `mi_theap_malloc_zero_aligned_at_overalloc`, `tokio::runtime::task::raw::RawTask::new`). The broker watches its own RSS independently, using the same env var, rather than reacting to a specific daemon's breach (see `rss_ceiling`'s and `broker_server.rs`'s module docs for why). A caller driving a build through this must check for the breach dump *before* trusting a resulting transport error — see `crates/soldr-cli/tests/daemon/daemon_rss_ceiling.rs`'s `assert_build_succeeded_or_report_breach`. Intended for CI/test use, not for production ceilings. | unset (no ceiling, no watchdog, no sampled profiler) |
| `SOLDR_BROKER_AUTOSPAWN` (soldr#3193) | `0` stops the front door from spawning a broker. For callers that only need read-only surfaces (`soldr shims --json` under a throwaway `HOME` per test); a wrapper compile under it fails loudly with "soldr broker is unreachable" rather than running uncached. Any other value, or unset, keeps the default spawn. | unset |
| `SOLDR_SHUTDOWN_WATCHDOG_SECS` | Seconds the daemon's graceful teardown may run before the process exits regardless. Zero, empty, or malformed values use the default; the backstop cannot be disabled. See docs/DAEMON_TIMEOUTS.md. | `240` |
| `SOLDR_DISPLACEMENT_DRAIN_TIMEOUT_SECS` | Positive seconds an acknowledged stale-version daemon displacement may drain before the kill fallback. Zero or malformed uses the default. | `120` |
| `SOLDR_DAEMON_DISPLACE` | `off`/`0`/`false`/`no` disables displacement of a stale-version daemon (pre-soldr#1495 'first daemon wins'). Any other value or unset keeps displacement on. | unset |
| `SOLDR_ALLOW_EPHEMERAL_DISPLACE` | Internal/diagnostic: permits displacing a daemon running from an ephemeral (temp/build-output) image. | unset |
| `SOLDR_HEAVY_TEST_LINK_BYTES` | Byte threshold above which a first-party `--test` link is granted exclusive compiler admission by `SoldrHostAdmissionClassifier`. | built-in threshold |
| `SOLDR_WINDOWS_PIPE_LISTENER_POOL` | Windows only: positive number of pre-created named-pipe listener instances the daemon keeps for incoming IPC connections. | built-in pool size |
| `SOLDR_DAEMON_TOKIO_CONSOLE` | Diagnostic: enables the tokio-console instrumentation in the daemon. | unset |
| `SOLDR_DAEMON_TOKIO_CONSOLE_PUBLISH_INTERVAL_MS` | Diagnostic: tokio-console publish interval in milliseconds when the console is enabled. | built-in interval |
| `SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH` | Diagnostic: file path where tokio-console events are recorded when the console is enabled. | unset |
| `SOLDR_BROKER_SERVICE` | Internal: route service name handed from broker to daemon. | set by broker |
| `SOLDR_DAEMON_CONTROL_ENDPOINT_PATH` | Internal: control-socket/pipe path handed from broker to daemon. | set by broker |
| `SOLDR_SESSION_ENDPOINT_PATH` | Internal: SESSION socket/pipe path handed from broker to daemon. | set by broker |
| `SOLDR_INTERNAL_DAEMON_EXE` | Internal: path to the staged daemon executable used by the launcher. | set by soldr |
| `SOLDR_BROKER_IDLE_EXIT_SECS` (soldr#3193) | Seconds a broker may sit with no connection open and no route owned before it stands itself down; the front door respawns it on the next invocation and daemons it owns exit with it. `0` disables idle exit. | `1800` |
| `SOLDR_BROKER_LEAK_TOAST` (soldr#3193) | `0` silences the stderr notice that soldr-broker/soldr-daemon processes for other `HOME`s are running on this host; `always` emits it even when stderr is not a terminal. Unset: interactive stderr only, never with `--json`/`--github-env`/`--shell-export`, never on `soldr broker` verbs. `soldr doctor` always reports the inventory; `soldr broker purge` stops the processes. | unset |
| `SOLDR_BROKER_LEAK_TOAST_INTERVAL_SECS` (soldr#3193) | Minimum spacing between two leak notices for one `HOME` (stamp file in `~/.soldr/broker/`). The stamp is checked before the process scan, so a quiet interval costs one `stat`. | `3600` |

**Windows: keep `SOLDR_CACHE_DIR` short.** Under the cache root soldr appends a
fixed staging suffix of roughly 143 characters
(`cache/zccache/daemon-state/embedded-v1/v<VERSION>/staging/<session>/<compile>/`)
before the artifact's own filename. Windows' classic `MAX_PATH` is 260, so a deep
cache root leaves too little budget and the **linker** fails part-way through a
build with, for example:

```text
LINK : fatal error LNK1104: cannot open file '...\staging\...\<crate>-<hash>.dll.lib'
```

That error names a file inside soldr's own cache rather than anything in your
project, which is why soldr calls that out explicitly when it detects it
(soldr#1969). The fix is a shorter root — the default `~/.soldr` is well within
budget; a root nested inside a temp/scratch tree may not be.

`RUSTC_WRAPPER=soldr cargo build` remains a low-level passthrough path, but it
does not create infrastructure. A top-level Soldr front door must first
register the exact daemon image/root route and ensure the singleton broker is
available. Every cacheable wrapper invocation then uses that broker SESSION
route. Broker, daemon, transport, version-skew, retirement, initialization,
and protocol failures are hard failures; there is no direct-daemon or
direct-compiler fallback.

When `SOLDR_RUSTC_WRAPPER` is set to a non-empty value such as `sccache`, soldr puts that binary in the wrapper slot instead of itself, bypassing the embedded path entirely. If it is set to `none` or an empty string, soldr leaves `RUSTC_WRAPPER` unset for that build.

Release/LTO musl validation and daemon-failure diagnostics are recorded in
[`DATALAKE_RELEASE_MUSL.md`](DATALAKE_RELEASE_MUSL.md).

On the normal embedded path, `soldr cargo ...` resolves a fresh Soldr
workspace context by default. It preserves normal process environment used by
Cargo, Rust, proxies, certificates, CI, and platform SDKs, but ignores
inherited Soldr/zccache workspace-pinned state such as `ZCCACHE_CACHE_DIR`,
`SOLDR_TARGET_REGISTRY_RECORDED`, and `SETUP_SOLDR_*`. The `SOLDR_TARGET_CACHE_*`
family is still scrubbed defensively even though soldr#2996 removed the target
cache soldr used to read them for -- a pinned setup-soldr may still export them.
The setup-soldr action now matches this (soldr#3651): `target-cache` and
`target-cache-mode` are deprecated no-ops, and the action no longer exports
`SOLDR_TARGET_CACHE_*` or runs a `target/` cache step.
Pass `--trust-inherited-soldr-env` or set `SOLDR_TRUST_INHERITED_ENV=1` only
for advanced workflows that intentionally inject those values. Custom wrapper
modes leave caller-provided wrapper environment alone; when
`SOLDR_RUSTC_WRAPPER=sccache` and the caller has set `SCCACHE_DIR` themselves,
Soldr forwards their value rather than overriding it.

Compile-capable Soldr front doors register the exact daemon image, root, and
route service before starting the build tool. Non-compiling commands do not
request a daemon route.

Daemon recovery uses that registered service definition. Wrappers carry only
the route service name and never locate, place, or start a daemon image.

The singleton broker serializes creation per root/version/image route. It
places the image under its stable user-owned root, launches one child, watches
early exit, and returns only after the route-local endpoint answers an active
BackendHandle probe.

Bootstrap cargo-install paths are intentionally uncached. `soldr build-from-source ...` and `[soldr.plugins]` installs from `soldr toolchain prepare` / `ensure` invoke the directly resolved cargo binary and scrub inherited `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER`. Those commands install dev tools and cross-target helper binaries; routing them through Soldr's wrapper slot would make setup recursively depend on the cache layer it is preparing.

`rustdoc` is intentionally not a zccache driver route today. Direct `soldr rustdoc ...` invocations and `rustdoc` PATH shims resolve the toolchain `rustdoc` binary and run it directly. `soldr cargo doc`, `soldr doc`, and doc tests still run with `RUSTC_WRAPPER=soldr`, so rustc dependency compile units remain cached; only the rustdoc driver phase itself is uncached because the embedded zccache runtime has no rustdoc parser/route.


`rust-analyzer` is launched as the real toolchain language server, not as a cache driver. When caching is enabled, `soldr rust-analyzer ...` gives the server process Soldr's cache policy plus a scoped child PATH shim so language-server child builds can re-enter the broker-owned route. `SOLDR_DISABLE_CHILD_SHIMS=1` keeps the language server as a direct passthrough when an editor owns its build environment.


Set `SOLDR_CACHE_LIFECYCLE=command` for self-build jobs that need embedded
cache state flushed before a following archive or test step. Command mode
requests a structured checkpoint of the
embedded zccache artifact index, depgraph, metadata cache, compiler hash cache,
system-includes cache, and artifact store, then finalizes session statistics.
An incomplete or timed-out checkpoint is reported as a failure. Command mode
does not run `zccache stop`, does not terminate the root-owning Soldr daemon,
and never changes ownership based on `ZCCACHE_CACHE_DIR`. Use
`soldr cache shutdown` when the whole Soldr daemon must exit.

On Windows, soldr may copy the running `soldr.exe` into `SOLDR_CACHE_DIR/runtime/soldr-self/<version-and-hash>/soldr.exe` and re-run the command from that relocated copy before build orchestration starts. This keeps disposable worktree builds from repeatedly using the worktree-local `soldr.exe` as `RUSTC_WRAPPER`. The trampoline sets `SOLDR_RELOCATED_EXE=1` and `SOLDR_ORIGINAL_EXE=<original path>` as a recursion guard and preserves argv, inherited environment, stdio, and exit status. Stale relocated copies are purged by a best-effort runtime GC step that runs periodically and skips copies that cannot be removed because they are still locked.

## GitHub Actions

```yaml
- name: Build through soldr
  run: soldr cargo build --release
```

For bootstrap verification of another Rust project:

```yaml
- name: Build third-party project through soldr
  run: soldr cargo build --target ${{ matrix.target }}
```

---

## Broker/daemon failure output

A cacheable compile that cannot use its registered broker route fails with an
infrastructure-attributed diagnostic. `soldr doctor`, `soldr status`, and
`soldr logs paths` provide the route, process, timeout, and log evidence. See
[docs/DAEMON_TIMEOUTS.md](DAEMON_TIMEOUTS.md) for bounded recovery steps.

## A restored source file can run a stale binary (soldr#2773)

Restoring a source file so its modification time moves **backwards** makes
`soldr cargo` run the previously compiled binary instead of the source on
disk — no `Compiling` line, no warning, exit 0:

```bash
cp -p src/main.rs main.rs.bak    # backup keeps the OLD mtime
# ... edit src/main.rs, build, observe the new behaviour ...
mv main.rs.bak src/main.rs       # restore; mtime goes backwards
soldr cargo run                  # runs the artifact from the EDITED source
```

The build result and the source tree disagree, and nothing says so.

**Why.** Cargo decides freshness by comparing a source's mtime against its
artifact's. A source that becomes *older* than the artifact looks fresh, so
rustc is never invoked — which means soldr never sees the compile either.
Nothing in soldr's cache is wrong here; the unit is skipped above soldr.

**Which operations do this.** Anything that writes a file with a preserved
timestamp rather than the current time:

| Operation | Moves mtime backwards? |
|---|---|
| `mv` of a `cp -p` / `rsync -a` backup | **yes** |
| `tar -x` (preserves mtimes by default) | **yes** |
| `git checkout` of an older commit | no — sets mtime to now |
| an editor save | no |

So the exposure is concentrated in bisect-style work, stash/restore loops, and
comparing a fix against a backup — exactly the situations where a wrong result
is most likely to be believed.

**Recovering.** Either of these forces the unit to rebuild:

```bash
touch src/main.rs                # cheapest; restores a forward mtime
soldr cargo clean -p <package>   # discards the stale artifact
```

Disabling the compilation cache does **not** help, and it is worth knowing
why: `ZCCACHE_DISABLE=1` (and the deprecated `--no-cache`) govern what happens
*once cargo decides to compile a unit*. Here cargo decides not to, so the unit
never reaches the cache in the first place. Both commands above work because
they change cargo's freshness answer — one by making the source newer than the
artifact, the other by removing the artifact.

**Recognising it.** The tell is a build that prints *no* `Compiling` line when
you expect one, and results that match a version of the source you no longer
have. Diagnostics from the stale artifact are the strongest signal — a warning
naming an import that the file on disk uses, or a test failure that the current
source cannot produce, means the running binary corresponds to no file in the
tree.

Automatic detection is tracked by soldr#2773.

## Summary

The key design rule is simple:

- users build through `soldr cargo ...`
- soldr owns the wrapper slot on the common path
- soldr delegates cache-enabled wrapper invocations over Soldr IPC into the
  zccache service embedded in `soldr-daemon`
- users do not need to manually wire `RUSTC_WRAPPER` for the common path
