# Darwin cross-compile harness (issue: complete macOS sysroot for cargo)

Reproduces the release-auto.yml `macOS x64` / `macOS ARM64` build lane
in a vanilla ubuntu-24.04 container. The goal is **not** "patch jemalloc"
— it is to surface the **complete Apple SDK as the cross-compile
sysroot** so that any C/C++ library in any Rust crate's dep tree
compiles correctly when targeting `*-apple-darwin` from a Linux host.

## Why the current state fails

`cargo zigbuild --target x86_64-apple-darwin` uses zig's bundled
`lib/libc/include/x86_64-macos-none/` sysroot. That sysroot is
intentionally minimal — enough for typical Rust crates that only touch
`stdio.h` / `pthread.h`, but missing:

- Several `sys/syscall.h` macros (`SYS_getrandom`, etc.) → tikv-jemalloc-sys
  configure aborts
- Framework headers (CoreFoundation, Security, Foundation) → would break
  rustls / reqwest if anything links them
- A subset of `mach/` and `os/` headers other low-level libs probe

Patching each library that hits this is whack-a-mole. The right fix is
to point the C/C++ compiler at a **complete Apple SDK** (MacOSX11.3.sdk
or later — soldr already fetches this via `apple_sdk.rs`) and keep zig
(or ld64.lld) only as the Mach-O *linker*.

## What this harness does

1. Builds an `ubuntu-24.04` image with:
   - `rustup` 1.98.1 + the `x86_64-apple-darwin` rust target
   - System `clang-18` (Ubuntu's; has darwin codegen built in)
   - System `llvm-18` (for `llvm-ar`, `llvm-ranlib`)
   - `zig` 0.14.1 (used only as the linker, via `-fuse-ld=lld` or `zig cc`)
   - `MacOSX11.3.sdk` extracted to `/opt/apple-sdk/MacOSX11.3.sdk/`
     (sourced from the same soldr-toolchain catalogue `apple_sdk.rs`
     uses, SHA-pinned to match `MANAGED_APPLE_SDK_SHA256`)
2. Mounts the soldr source tree at `/src` as a bind mount (so iteration
   is in-place; `target/` survives across container runs).
3. Runs `cargo build --target x86_64-apple-darwin -p soldr-cli` with the
   sysroot-overriding env block:
   ```
   SDKROOT=/opt/apple-sdk/MacOSX11.3.sdk
   CC_x86_64_apple_darwin="clang --target=x86_64-apple-darwin -isysroot $SDKROOT -mmacosx-version-min=11.0"
   CXX_x86_64_apple_darwin="clang++ --target=x86_64-apple-darwin -isysroot $SDKROOT -mmacosx-version-min=11.0 -stdlib=libc++"
   AR_x86_64_apple_darwin="llvm-ar"
   RANLIB_x86_64_apple_darwin="llvm-ranlib"
   CFLAGS_x86_64_apple_darwin="-isysroot $SDKROOT -mmacosx-version-min=11.0"
   CXXFLAGS_x86_64_apple_darwin="-isysroot $SDKROOT -mmacosx-version-min=11.0 -stdlib=libc++"
   CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="clang"
   CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS="-C link-arg=--target=x86_64-apple-darwin -C link-arg=-isysroot -C link-arg=$SDKROOT -C link-arg=-mmacosx-version-min=11.0 -C link-arg=-fuse-ld=lld"
   ```
4. Success criterion: `file target/x86_64-apple-darwin/debug/soldr`
   reports `Mach-O 64-bit x86_64 executable`.

## Iteration loop

```bash
# One-time: build the image (5-10 min, fetches Apple SDK once)
./build.sh --setup

# Inner loop: code edit → run a single attempt (2-3 min)
./build.sh                              # x86_64-apple-darwin
./build.sh --target aarch64-apple-darwin

# Reproduce today's failure (BASELINE — uses cargo zigbuild):
./build.sh --baseline
```

## Crates this harness must successfully cross-compile

If the sysroot is truly complete, EVERY C/C++ dep in soldr's tree
compiles. The test crates are exactly the ones the GHA `macOS x64` lane
builds:

- `tikv-jemalloc-sys` (autotools probe of `sys/syscall.h` — today's canary)
- `libsqlite3-sys` (touches dirent, syscalls)
- `libz-ng-sys` (cmake; probes threads, atomics)
- `zstd-sys` (touches mmap, posix_madvise)
- `xz2` / `lzma-sys`
- `bzip2-sys`
- `libmimalloc-sys` (touches mmap, mprotect)
- `ring` 0.17 (asm + cc-rs)
- `rustls` / `rustls-webpki` (Security framework)
- `reqwest` (CoreFoundation, Security)

When ALL of these build without a `cargo build` re-run, the sysroot
contract is satisfied.

## What to codify after the harness shows it works

Extend `crates/soldr-cli/src/blessed_build.rs::prepare`'s apple-darwin
arm (lines 156-172, currently only sets `SDKROOT`) to inject the env
block above — mirror the Windows MSVC arm at lines 84-110. Then change
`release-auto.yml:371` from `cargo zigbuild` to plain `cargo build`
(via `soldr build --target $T` which calls into blessed_build) and drop
`continue-on-error` on the darwin lanes at line 176.
