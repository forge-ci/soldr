#!/usr/bin/env bash
# Inside-the-container darwin cross-compile runner.
#
# Args:
#   $1 = target triple (x86_64-apple-darwin | aarch64-apple-darwin)
#   $@ = remaining args forwarded as cargo args (e.g. --release)
#
# Mirrors the env block in release-auto.yml's darwin Linux-host branch
# (post-v0.7.87). The whole point of this harness is that this same
# env, when applied locally inside a clean ubuntu-24.04 container,
# either:
#   (a) produces a Mach-O binary for soldr-cli → matches CI behavior
#       and proves the env is sufficient, OR
#   (b) hits the same compile/link error CI hits → reproducible bug we
#       can iterate on without waiting on a 30-min remote release run

set -euo pipefail

TARGET="${1:?usage: $0 <target-triple> [cargo args...]}"
shift

# Discover the extracted Apple SDK (top-level MacOSX*.sdk dir).
SDKROOT="$(ls -d /opt/apple-sdk/MacOSX*.sdk 2>/dev/null | head -1)"
if [ -z "$SDKROOT" ] || [ ! -f "$SDKROOT/usr/include/sys/syscall.h" ]; then
    echo "ERROR: no usable Apple SDK at /opt/apple-sdk/" >&2
    ls -la /opt/apple-sdk >&2 || true
    exit 1
fi
export SDKROOT
echo "Using SDKROOT=$SDKROOT"

# Diagnostic the v0.7.87 trace asked for: do we have the headers
# jemalloc needs?
for header in sys/syscall.h os/lock.h os/availability.h mach/mach.h; do
    if [ -f "$SDKROOT/usr/include/$header" ]; then
        echo "  ✓ $header"
    else
        echo "  ✗ $header (MISSING)"
    fi
done

case "$TARGET" in
    x86_64-apple-darwin)   CLANG_ARCH=x86_64-apple-darwin ;;
    aarch64-apple-darwin)  CLANG_ARCH=arm64-apple-darwin ;;
    *) echo "unsupported darwin target: $TARGET" >&2 ; exit 1 ;;
esac

TARGET_U="${TARGET//-/_}"
TARGET_U_UPPER="$(echo "$TARGET_U" | tr '[:lower:]' '[:upper:]')"

# Wrapper scripts that bake `--target=$CLANG_ARCH -isysroot $SDKROOT
# -mmacosx-version-min=11.0` into every clang invocation. Required
# because cc-rs splits compiler binary from flags when invoking
# configure (CC=clang, --target ends up in CFLAGS), and jemalloc's
# Makefile has a hidden `$(CC) -MM $(CPPFLAGS)` dep-gen invocation
# that uses CC only (no CFLAGS). Without the wrapper, that -MM call
# misses --target+isysroot, falls back to Linux system headers, and
# fails the `#include <os/lock.h>` resolution. The CC wrapper makes
# the flags survive ALL clang invocations.
WRAP_DIR="/tmp/clang-wrap-${TARGET}"
mkdir -p "$WRAP_DIR"

# Save the real clang on first run (idempotent if already saved).
if [ ! -f /usr/bin/clang.real ]; then
    cp /usr/bin/clang-18 /usr/bin/clang.real
    cp /usr/bin/clang++-18 /usr/bin/clang++.real
fi

cat > "$WRAP_DIR/clang" <<EOF
#!/bin/sh
exec /usr/bin/clang.real --target=${CLANG_ARCH} -isysroot ${SDKROOT} -mmacosx-version-min=11.0 -fuse-ld=lld "\$@"
EOF
cat > "$WRAP_DIR/clang++" <<EOF
#!/bin/sh
exec /usr/bin/clang++.real --target=${CLANG_ARCH} -isysroot ${SDKROOT} -mmacosx-version-min=11.0 -stdlib=libc++ -fuse-ld=lld "\$@"
EOF
chmod +x "$WRAP_DIR/clang" "$WRAP_DIR/clang++"

# Nuclear option: replace /usr/bin/clang with the wrapper so EVERY
# clang invocation (including cc-rs's hardcoded `CC=clang` for
# cross-compile detection of cross-toolchain naming) goes through
# the wrapper. cc-rs's CC_<triple> env var override has edge cases
# that didn't take effect for tikv-jemalloc-sys's configure
# invocation — config.log kept showing `CC=clang` despite our env.
# Symlinking the system binary is unambiguous: every bare `clang`
# invocation in the build resolves to our wrapper.
ln -sf "$WRAP_DIR/clang" /usr/bin/clang
ln -sf "$WRAP_DIR/clang++" /usr/bin/clang++
# Also redirect /usr/bin/clang-18 in case cc-rs bypasses the
# unversioned name. clang.real (saved above) is the real binary.
ln -sf "$WRAP_DIR/clang" /usr/bin/clang-18
ln -sf "$WRAP_DIR/clang++" /usr/bin/clang++-18

# v0.7.87's env block — keep in lockstep with release-auto.yml.
# -fuse-ld=lld in CFLAGS so cmake-rs sub-builds use lld for their
# test-compile-link step (else they call /usr/bin/ld which can't
# emit Mach-O).
export "CC_${TARGET_U}=$WRAP_DIR/clang"
export "CXX_${TARGET_U}=$WRAP_DIR/clang++"
export "AR_${TARGET_U}=llvm-ar"
export "RANLIB_${TARGET_U}=llvm-ranlib"
# Belt-and-suspenders: also set plain CC/CXX. Some build scripts
# (notably tikv-jemalloc-sys) use cc-rs to detect the compiler;
# cc-rs's per-target env lookup has edge cases. Setting plain CC
# is unambiguous since this script is darwin-only.
export CC="$WRAP_DIR/clang"
export CXX="$WRAP_DIR/clang++"
export AR=llvm-ar
export RANLIB=llvm-ranlib
# CFLAGS/CXXFLAGS still needed for cc-rs's own probe stage. Wrapper
# already adds --target+isysroot but cc-rs may add its own flags too;
# leaving CFLAGS explicit ensures cc-rs's downstream cargo invocations
# (rustc -C link-arg=...) also have the right view.
export "CFLAGS_${TARGET_U}=-isysroot ${SDKROOT} -mmacosx-version-min=11.0"
export "CXXFLAGS_${TARGET_U}=-isysroot ${SDKROOT} -mmacosx-version-min=11.0 -stdlib=libc++"
# Plain CPPFLAGS (no triple suffix) — load-bearing for jemalloc 5.3.1's
# autotools build. jemalloc's Makefile has TWO clang invocations per .c
# file:
#     $(CC) $(CFLAGS) -c $(CPPFLAGS) $(CTARGET) $<
#     @$(CC) -MM $(CPPFLAGS) -MT $@ -o $(@:%.$(O)=%.d) $<
# The second one (the -MM dep-tracker, suppressed from `make V=1`
# echo by the `@`) uses CPPFLAGS ONLY — no CFLAGS, no --target,
# no -isysroot. Without sysroot in CPPFLAGS, clang falls back to
# Linux system headers when generating dependencies and the
# `#include <os/lock.h>` in jemalloc_internal_decls.h fails to
# resolve.
#
# autoconf-style configure scripts (like jemalloc's) only honor the
# bare `CPPFLAGS` env var — not the cc-rs-style `CPPFLAGS_<triple>`
# suffix. So we have to set CPPFLAGS directly. This script is darwin-
# only (entry point gated on `*-apple-darwin` in release-auto.yml +
# in the docker harness's caller), so setting plain CPPFLAGS doesn't
# bleed into other targets.
export CPPFLAGS="--target=${CLANG_ARCH} -isysroot ${SDKROOT} -mmacosx-version-min=11.0"
export "CARGO_TARGET_${TARGET_U_UPPER}_LINKER=$WRAP_DIR/clang"
# `--cfg tokio_unstable` must be repeated here: this per-target RUSTFLAGS
# overrides (does not merge with) the `[build] rustflags` in
# .cargo/config.toml, so without it the macOS wheels would compile the
# tokio-console feature but leave console-subscriber inert. See the note
# in .cargo/config.toml on cargo's rustflags precedence.
export "CARGO_TARGET_${TARGET_U_UPPER}_RUSTFLAGS=-C link-arg=-fuse-ld=lld --cfg tokio_unstable"
# Drop the explicit CPPFLAGS — the wrapper takes care of it.
unset CPPFLAGS

echo "===== inline darwin env ====="
env | grep -E "^(SDKROOT|CC_|CXX_|AR_|RANLIB_|CFLAGS_|CXXFLAGS_|CARGO_TARGET_)" \
    | grep -E "${TARGET_U}|${TARGET_U_UPPER}|SDKROOT" \
    | sort
echo "============================="

cargo build --package soldr-cli --release --target "$TARGET" "$@"

OUT="target/$TARGET/release/soldr"
echo "===== post-build ====="
if [ -f "$OUT" ]; then
    echo "OK — $(file "$OUT")"
else
    echo "MISSING binary at $OUT"
    ls -la "target/$TARGET/release/" 2>&1 | head -20 || true
    exit 1
fi
