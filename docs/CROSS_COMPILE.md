# Cross-compiling with soldr

This guide documents the currently-working recipes for cross-compiling
between Linux and Windows targets through soldr. It addresses the
exploration tracked in [issue #329][issue-329].

soldr owns the complete target lifecycle. The consumer selects a target;
soldr installs Rust std, prepares the compiler/linker and SDK/sysroot, and
merges required target flags with project `RUSTFLAGS`, `CFLAGS`, and
`CXXFLAGS`. The same preparation is used by build, clippy, test compilation,
nextest archives, and PEP 517 wheels.

If you want a more permissive, CI-side experience that *can* install system
packages, see [`zackees/setup-soldr`][setup-soldr]'s `cross-targets:` input
instead.

**Windows hosts:** the bash-flavored docker-prebuilt + cross-compile recipes
on this page assume Git Bash / MSYS2 / Cygwin / WSL on PATH. soldr does NOT
bootstrap the POSIX shell layer — see [`docs/WINDOWS_PREREQS.md`](WINDOWS_PREREQS.md)
for the install matrix + the common `cygpath: command not found` / docker
bind-mount error → fix mapping ([soldr#885](https://github.com/zackees/soldr/issues/885)).

[issue-329]: https://github.com/zackees/soldr/issues/329
[setup-soldr]: https://github.com/zackees/setup-soldr

---

## TL;DR

| You want | Use |
|---|---|
| Any canonical target | `soldr build --target <alias-or-triple>` |
| Windows x64 → Windows GNU | managed MinGW-w64 GCC + GNU syslibs ([Section 1](#1-windows-x64--windows-gnu-via-managed-mingw-w64-gcc)) |
| Linux → Windows MSVC | `soldr build` ([Section 2](#2-linux--windows-msvc-via-soldr-build)) |
| **Windows/Linux -> Linux** | `soldr build --target <linux-triple>` |
| **Windows/Linux -> Mac** | `soldr build` + target-shaped Apple SDK ([Section 1a](#1a-canonical-linux-and-macos-targets-through-soldr-build)) |
| TLS for Windows MSVC without OpenSSL | nothing to install ([Section 3.6](#36-tls-on-windows-msvc-needs-no-openssl)) |
| Declare cross targets up-front | `[toolchain].targets` + `[soldr.plugins]` ([Section 3](#3-pinned-host-triples-per-project-current-state)) |

## Canonical target aliases

`ci/canonical-targets.json` is the machine-readable authority for the target,
alias, CI, release, and catalogue contract. Raw Rust triples remain accepted.

<!-- canonical-target-contract:start -->
| Alias | Rust target | CI validation | Release |
|---|---|---|---|
| `win-x64` | `x86_64-pc-windows-msvc` | Cross-build + native run | Shipped |
| `win-x64-gnu` | `x86_64-pc-windows-gnu` | Cross-build + native run | Uses shipped Windows x64 host artifact |
| `win-arm64` | `aarch64-pc-windows-msvc` | Cross-build + native run | Shipped |
| `mac-x64` | `x86_64-apple-darwin` | Cross-build + native run | Shipped |
| `mac-arm64` | `aarch64-apple-darwin` | Cross-build + native run | Shipped |
| `linux-x64` | `x86_64-unknown-linux-gnu` | Native build + run | Shipped |
| `linux-arm64` | `aarch64-unknown-linux-gnu` | Cross-build + native run | Shipped |
| `linux-x64-musl` | `x86_64-unknown-linux-musl` | Cross-build only (host-identical runner) | Shipped |
| `linux-arm64-musl` | `aarch64-unknown-linux-musl` | Cross-build + native run | Shipped |
<!-- canonical-target-contract:end -->

Aliases follow `<os>-<arch>[-<abi>][-glibc_<major>_<minor>]` (soldr#3806), so a
few predictable spellings are accepted beyond the table:

- `win-arm` / `mac-arm` mean `win-arm64` / `mac-arm64` (neither OS has a
  supported ARM32 target). `linux-arm` stays ambiguous because armv7 Linux
  is real; use `linux-arm64`.
- `x86_64` / `amd64` and `aarch64` are accepted for `x64` and `arm64`.
- `linux-x64-glibc_2_17` (also `glibc2.17`) is the alias form of
  `x86_64-unknown-linux-gnu.2.17` and selects the same catalogue glibc floor.
  A glibc suffix on a non-GNU alias (`mac-x64-glibc_2_17`,
  `linux-x64-musl-glibc_2_17`) or an unbacked floor is an error.

Every `--target` surface (`build`, `prepare`, `env`, `wheel`, `cc`, `c++`,
`ld`, `link`, `toolchain prepare|ensure`) resolves through the same resolver.

The target is the only toolchain choice in normal builds:

```bash
soldr prepare --target linux-arm64
soldr build --target linux-arm64 --release
soldr cargo clippy --target linux-arm64 --all-targets
soldr cargo test --no-run --target linux-arm64
soldr env --target linux-arm64 --json
```

Legacy wrapper commands are retained only for diagnostic comparisons and are
never the default CI or release recipe.

---

## 1. Windows x64 → Windows GNU via managed MinGW-w64 GCC

On Windows x64 hosts, `soldr prepare --target x86_64-pc-windows-gnu`
downloads a pinned WinLibs MinGW-w64 GCC bundle from the soldr-toolchain
catalogue, prepends its `bin/` directory to `PATH`, and exports the
target-scoped Cargo/cc-rs variables needed by build scripts:
`CC_x86_64_pc_windows_gnu`, `CXX_x86_64_pc_windows_gnu`,
`AR_x86_64_pc_windows_gnu`, `RANLIB_x86_64_pc_windows_gnu`,
`WINDRES_x86_64_pc_windows_gnu`, and
`CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER`.

The same prepare/build path materializes GNU-shaped managed syslib rows
(`windows-x64-gnu`) for the C dependencies soldr injects: `zstd`,
`sqlite`, `mimalloc`, `zlib-ng`, `lzma`, and `bzip2`. A Windows GNU build
must not consume `windows-x64` MSVC syslibs.

### Recipe

```powershell
rustup target add x86_64-pc-windows-gnu

soldr prepare --target x86_64-pc-windows-gnu
where gcc
gcc --version

soldr build --target x86_64-pc-windows-gnu --release
```

In GitHub Actions, pass `--github-env $env:GITHUB_ENV` so later steps
inherit the same compiler/linker/pkg-config environment:

```powershell
soldr prepare --target x86_64-pc-windows-gnu --github-env $env:GITHUB_ENV
```

Scope is intentionally narrow: first-class managed MinGW provisioning
currently supports only `x86_64-pc-windows-gnu` on Windows x64 hosts.
`i686-pc-windows-gnu`, `aarch64-pc-windows-gnullvm`, and other Windows
GNU-family targets are follow-ups. Linux hosts are not a blessed Windows
GNU path in soldr; do not use `cargo-zigbuild` as a substitute for this
target.

---

## 1a. Canonical Linux and macOS targets through `soldr build`

Windows contributors can produce Linux and macOS binaries locally instead of
pushing a branch and waiting on CI, but the two target families now use
one blessed build surface:

- GNU Linux targets use catalogue-backed GCC/binutils/glibc-2.17 sysroot
  bundles internally through `soldr build`. The published baseline is glibc
  2.17; `--target <triple>.2.17` selects it explicitly, while an unsupported
  floor is rejected rather than silently weakened. Soldr selects the correct x86_64
  or aarch64 bundle from the requested triple, verifies the catalogue SHA-256,
  and exports the target compiler/linker, CMake, and pkg-config sysroot
  environment. Neither `zig`, `cargo-zigbuild`, nor `ziglang` is on this
  blessed GNU path.
- musl Linux targets use separate catalogue-backed GCC/binutils/musl static
  CRT bundles for `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl`. The compiler, linker, CMake, and pkg-config
  environment all resolve under the verified managed root; normal musl
  builds never download or execute Zig, `cargo-zigbuild`, or `ziglang`.
  `SOLDR_USE_LEGACY_ZIGBUILD` is removed (soldr#2519); there is no Zig route
  to fall back to.
- macOS targets use `soldr build --target <apple-triple>`. That path resolves
  the target-aware Apple SDK row and injects clang/SDK env internally. Direct
  `soldr cargo zigbuild --target *-apple-darwin` is a legacy/diagnostic path,
  not the default macOS recipe.

`x86_64-apple-darwin` is supported for local, CI, and release cross-builds.
Official Intel and Apple Silicon macOS archives and PyPI wheels are built on
Linux through this blessed path. Opt-in `ci-full` and exact-SHA release
validation replay them on hosted `macos-15-intel` and `macos-15` runners,
including installation and import of the shipped wheels. Ordinary PR/main
validation does not allocate hosted macOS runners. The separate Intel macOS
Recovery-guest replay remains available as an advisory diagnostic lane.

PyPI wheels cross-built this way must be `abi3-py310` PyO3 extensions — the
fleet-wide policy, and the only ABI `soldr wheel` supports without a
target-side CPython. See [API.md](API.md#pyo3-abi-policy-abi3-py310-always).

### Recipe

```powershell
# Pin the cross targets in rust-toolchain.toml (same shape as Section 1):
#   [toolchain]
#   targets = ["x86_64-unknown-linux-gnu", "x86_64-apple-darwin", "aarch64-apple-darwin"]
#
# Linux target: soldr selects and prepares the managed toolchain.
soldr prepare --target x86_64-unknown-linux-gnu
soldr build --target x86_64-unknown-linux-gnu --release -p soldr-cli

# musl target: the same managed lifecycle, with a static musl CRT.
soldr prepare --target x86_64-unknown-linux-musl
soldr build --target x86_64-unknown-linux-musl --release -p soldr-cli

# macOS targets: soldr build is the blessed path. `prepare` is optional and
# useful only when a later external/legacy command needs SDKROOT exported.
soldr prepare --target x86_64-apple-darwin
soldr build --target x86_64-apple-darwin --release -p soldr-cli
soldr build --target aarch64-apple-darwin --release -p soldr-cli
```

### What soldr handles automatically

- Catalogue-backed GCC/binutils/glibc-2.17 sysroot provisioning for GNU Linux
  targets.
- Catalogue-backed GCC/binutils/musl sysroot provisioning for canonical musl
  Linux targets, including startup objects and static runtime libraries.
- Apple SDK fetch for `*-apple-darwin` targets. Auto shape maps
  `x86_64-apple-darwin` to `darwin-x86_64` and `aarch64-apple-darwin` to
  `darwin-aarch64`; `soldr build` applies the SDK env internally.

### Apple SDK version + shape (soldr-toolchain#14)

The Apple SDK soldr fetches is pinned per-target via two env vars
(both optional):

| Env var | Values | Default | Effect |
|---|---|---|---|
| `SOLDR_APPLE_SDK_VERSION` | `11.3`, `13.3`, `14.5`, `15.2` | `14.5` | Which macOS SDK to vendor (catalogue row selection). |
| `SOLDR_APPLE_SDK_SHAPE` | `universal2`, `thin-x86_64`, `thin-aarch64`, `auto` | `auto` | Whether to fetch the fat universal2 artifact or a lipo-thinned per-arch slice. |

`auto` (the default) picks the **thin variant matching the target
triple's arch** when cross-compiling for one Apple arch, falling
back to `universal2` otherwise. Examples:

```powershell
# Project targets only Apple Silicon: fetch the thin SDK slice
$env:SOLDR_APPLE_SDK_VERSION = "14.5"
$env:SOLDR_APPLE_SDK_SHAPE   = "thin-aarch64"
soldr build --target aarch64-apple-darwin --release -p soldr-cli

# Project targets both Apple archs from one cache: one fat artifact
$env:SOLDR_APPLE_SDK_SHAPE = "universal2"
soldr build --target x86_64-apple-darwin  --release -p soldr-cli
soldr build --target aarch64-apple-darwin --release -p soldr-cli
```

The available `(version, shape)` rows live in the soldr-toolchain
catalogue at `https://zackees.github.io/soldr-toolchain/catalogue.v1.json`
under the URL pattern `/apple-sdk/<version>/<shape-slug>/`. The historical
11.3 row keeps its legacy `/apple-sdk/MacOSX11.3/darwin-universal2/` layout;
modern rows use the bare version plus shape slug.

### Local executable-only symbol check

After installing `soldr` and an LLVM containing `llvm-nm` and
`llvm-symbolizer`, run the
self-cleaning probe from the repository root:

```sh
uv run --no-project python bench/test_darwin_symbols.py
DARWIN_TARGETS=x86_64-apple-darwin,aarch64-apple-darwin \
  uv run --no-project python bench/test_darwin_symbols.py
```

The probe builds a temporary crate with `line-tables-only` plus packed
debuginfo through `soldr build`, removes every generated dSYM/object sidecar,
and verifies that `llvm-symbolizer` still maps `symbol_probe` to `main.rs` in
the Mach-O executable. Set `LLVM_NM`, `LLVM_SYMBOLIZER`, or `SOLDR_LLVM_DIR`
when LLVM is not already on `PATH`.

### CI

The Linux cross-build workflow `.github/workflows/_ci-cross-build-linux.yml`
proves the MSVC targets from a Linux host, and the matching
`e2e-windows-x64` / `e2e-windows-arm64` jobs in `ci.yml` execute the produced
archives on native Windows runners.

The Windows GNU target has **no CI coverage**. A dedicated
`windows-gnu-mingw-validation.yml` workflow used to validate it on a native
`windows-2025` runner, but it path-triggered on nearly every code PR and did
two full release builds to exercise a target that is not in
`ci/canonical-targets.json` and is never shipped — removed in soldr#1982. The managed
MinGW path below is still supported and still works; it is simply verified by
hand rather than on every PR. Re-add a scheduled-only workflow if Windows GNU
regressions start reaching users.

Windows GNU is intentionally handled by the managed MinGW path in
[Section 1](#1-windows-x64--windows-gnu-via-managed-mingw-w64-gcc), not by
the Linux-hosted zigbuild flow.

---

## 2. Linux → Windows MSVC via blessed `soldr build`

The blessed `soldr build --target <triple>` path provisions the catalogued
Microsoft CRT/Windows SDK inputs and sets up the link step so you can produce
MSVC-ABI binaries from a Linux host. `soldr cargo xwin` remains an explicit
legacy passthrough for projects that need the historical cargo-xwin behavior.

### `rust-toolchain.toml`

```toml
[toolchain]
channel = "1.95.0"
targets = ["x86_64-pc-windows-msvc"]

[soldr.plugins]
cargo-xwin = { version = "0.18", locked = true }
```

### Bootstrap and build

```sh
soldr prepare --target x86_64-pc-windows-msvc
soldr build --release --target x86_64-pc-windows-msvc
```

Legacy fallback (explicitly bypasses the blessed surface):

```sh
soldr cargo xwin build --release --target x86_64-pc-windows-msvc
```

### MSVC EULA

`cargo-xwin` auto-downloads the MSVC CRT and Windows SDK from Microsoft's
servers on first run. By using it, **you accept Microsoft's MSVC
license** implicitly. soldr does not display the EULA; if that matters
for your distribution, read upstream's
[license discussion][xwin-license] before adopting the recipe.

[xwin-license]: https://github.com/rust-cross/cargo-xwin#license

### Useful env vars (cargo-xwin)

| Var | Purpose |
|---|---|
| `XWIN_ARCH` | Target architecture set to download (`x86_64`, `aarch64`, `x86`) |
| `XWIN_CACHE_DIR` | Where to cache the downloaded CRT/SDK |
| `XWIN_INCLUDE_ATL` | Include ATL headers (off by default) |
| `XWIN_VARIANT` | Pick a specific SDK variant when multiple match |

See [`rust-cross/cargo-xwin`][cargo-xwin] for the complete and current list.

[cargo-xwin]: https://github.com/rust-cross/cargo-xwin

---

## 3. Pinned host triples per project (current state)

Today, declaring a cross target involves two `rust-toolchain.toml` blocks:

1. **`[toolchain].targets`** — tells `soldr toolchain prepare` to
   `rustup target add` the rust-std for that triple.
2. **`[soldr.plugins]`** — tells `soldr toolchain prepare` to install the
   cargo extension that drives the cross link (`cargo-zigbuild`,
   `cargo-xwin`, etc.).

Putting both in `rust-toolchain.toml` means a fresh clone needs only:

```sh
soldr toolchain prepare
```

…to be ready to cross-compile, with the exact same versions as every other
developer on the project.

### Future enhancement (out of scope for #329)

A unified `[soldr.cross-targets]` block could collapse the two declarations
into one — `soldr toolchain prepare` would derive both the `rust-std`
install and the matching cargo-extension install from a single entry. That
work is intentionally **not** part of this exploration; if you need it,
file a follow-up issue referencing #329.

---

## 3.5. ring + `aarch64-pc-windows-msvc` requires `soldr build`

Crates depending on `ring 0.17.x` (rustls, reqwest's TLS, …) include
hand-written ARM assembly at `pregenerated/sha256-armv8-win64.S` etc.
ring's `build.rs` hardcodes `c.compiler("clang")` for windows-msvc and
shells out to `clang` directly. On `aarch64-pc-windows-msvc` plain
`clang` rejects this — soldr installs a multicall `clang` shim
(hardlink/copy of `soldr`) that intercepts the call and re-execs
`clang-cl` with the same argv.

**Use the blessed surface**: `soldr build --target
aarch64-pc-windows-msvc` (soldr#1012, #882). It installs the shim at
`~/.soldr/bin/clang-shim/` ahead of system clang on `PATH` and uses
the managed MSVC SDK cache when that target's cache row is available.
The managed ARM64 cache row is now available, so `soldr build` uses the same
catalogue-backed SDK/linker path for `aarch64-pc-windows-msvc`; the explicit
legacy cargo-xwin command remains available as a diagnostic fallback.

### Direct cargo-xwin path is unsupported

Bypassing `soldr build` (e.g. inside `messense/cargo-xwin:0.23.0`
docker, or a workflow that doesn't go through soldr) means the shim
isn't on `PATH` and the build fails:

```
error occurred in cc-rs: command did not execute successfully
LC_ALL=C clang ... --target=aarch64-pc-windows-msvc ...
  -c ring-0.17.14/pregenerated/sha256-armv8-win64.S
```

To use the shim outside soldr-managed shells: install soldr
(`pip install soldr` / `npm install -g @zackees/soldr`), then run
`soldr build --target aarch64-pc-windows-msvc --help` once to trigger
shim install. After that, add `$HOME/.soldr/bin/clang-shim` to `PATH`
manually and any downstream tool's clang invocation resolves to the
shim.

### Why not upstream?

Upstream-able in principle, but ring's `c.compiler("clang")` is
intentional and cargo-xwin's `CC_*=clang-cl` gets overridden by
cc-rs's `compiler_family()` probe. The shim is the surgical fix that
lives in soldr's toolchain story. See soldr#886.

---

## 3.6. TLS on Windows MSVC needs no OpenSSL

`native-tls` (and therefore reqwest's `default-tls`) uses SChannel on every
Windows target, and `rustls` is pure Rust. Neither puts `openssl-sys` in a
`*-pc-windows-msvc` dependency graph, so a Windows consumer of those stacks has
nothing to install. Check before adding anything to CI:

```sh
soldr cargo tree --target x86_64-pc-windows-msvc -e features -i openssl-sys
# "warning: nothing to print." means the graph does not use OpenSSL.
```

soldr ships no vcpkg OpenSSL bundle: the vcpkg producer workflow was retired in
soldr#2814. A per-job `vcpkg install openssl` rebuilds the port from source;
Bosn's Windows wheel lane spent 7m47s in it for a graph that never contained
`openssl-sys` (soldr#3231).

When the tree is not empty, a crate requests OpenSSL on Windows explicitly.
Prefer fixing it at that dependency by selecting its native-tls/SChannel or
rustls feature. If OpenSSL is genuinely required, `soldr build` and
`soldr prepare` provide it (soldr#3246). Whenever `openssl-sys` is the crate
that provides `links = "openssl"` for the target, they fetch the catalogue's
static, source-built OpenSSL 3.5.8 (sha256-verified) and export three
target-scoped variables that `openssl-sys` reads before the unscoped ones:

```sh
X86_64_PC_WINDOWS_MSVC_OPENSSL_DIR=~/.soldr/bin/syslib/openssl/3.5.8/windows-x64/package
X86_64_PC_WINDOWS_MSVC_OPENSSL_NO_VENDOR=1   # overrides `openssl/vendored`
X86_64_PC_WINDOWS_MSVC_OPENSSL_STATIC=1
```

This works for all nine syslib shapes. For targets other than MSVC,
`PKG_CONFIG_PATH_<triple>` also gets the bundle's `lib/pkgconfig`. No unscoped
`OPENSSL_*` variable is set, so host build scripts are unaffected. As a result,
crates that use `openssl = { features = ["vendored"] }` build for
`*-pc-windows-msvc` from Linux without manifest changes. `openssl-src` cannot
do that itself, because it looks up `nmake.exe` in the Windows registry.

soldr skips the managed OpenSSL in these cases:

- `SOLDR_USE_LEGACY_VENDORED_SYS` is set.
- The caller already set `OPENSSL_DIR`, `OPENSSL_LIB_DIR`,
  `OPENSSL_INCLUDE_DIR`, or `OPENSSL_NO_VENDOR`, either scoped to the target or
  unscoped.
- Another crate claims `links = "openssl"`.
- The bundle is unavailable. soldr logs why and the build continues.

When cargo's output is captured (CI logs, pipes), soldr prints this guidance
after an `openssl-sys` "Could not find directory of OpenSSL installation"
failure for a `*-windows-msvc` target.

---

## 4. What soldr deliberately does NOT do

The #329 exploration is explicit about the lines soldr will not cross:

- **No silent `RUSTFLAGS` injection.** soldr does not append linker flags
  or `-C link-arg=...` based on guesswork about your target.
- **No hidden `CC` / `CXX` / linker mutation.** `soldr prepare` may export
  target-scoped env for a supported target, such as managed MinGW-w64 GCC
  for `x86_64-pc-windows-gnu`, but only on an explicit `--target`.
- **No system-package installs** (`mingw-w64`, `binutils-mingw-w64-x86-64`,
  etc.) from inside soldr. Managed toolchains are downloaded into soldr's
  cache instead of being installed into the host OS.

For the CI side, [`zackees/setup-soldr`][setup-soldr] takes liberties that
soldr itself will not — including a `cross-targets:` input that can install
the system bits on a GitHub-hosted runner.

---

## 5. Status of issue #329 sub-items

| Sub-item | Status |
|---|---|
| 1. Windows GNU managed MinGW path | Works via `soldr prepare` / `soldr build` on Windows x64 hosts ([Section 1](#1-windows-x64--windows-gnu-via-managed-mingw-w64-gcc)). |
| 2. Documented `xwin` recipe | This file ([Section 2](#2-linux--windows-msvc-via-cargo-xwin)). |
| 3. Pinned host triples per project | Works via `[toolchain].targets` + `[soldr.plugins]` ([Section 3](#3-pinned-host-triples-per-project-current-state)). A unified `[soldr.cross-targets]` block is deferred. |

---

## See also

- [`README.md`](../README.md) — "Native vs cross targets" covers same-ABI
  cross targets that only need a rust-std install.
- [`docs/API.md`](API.md) — full CLI reference, including `soldr toolchain
  prepare`.
- [`CLAUDE.md`](../CLAUDE.md) — `[soldr.plugins]` manifest example and
  contributor guidance.
