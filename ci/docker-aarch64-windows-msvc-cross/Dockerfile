# Simulate soldr's cross-compile to `aarch64-pc-windows-msvc` from a
# Linux x86_64 host in a vanilla Ubuntu 24.04 container.
#
# Mirrors `ci/docker-aarch64-musl-cross/` for the Windows MSVC target.
# Triggered by GHA run 28345283384 / job 83967287913 which hit a
# 30-minute hang on the same cross-compile path —
#   "soldr: cargo diagnostic capture timed out after 1800 seconds"
# This harness exists so the same path can be reproduced locally and
# diagnosed with per-line elapsed-second prefixes (run_with_ts.py).
#
# Toolchain stack:
#   - soldr v0.9.6 (a catalogue-v2-capable published bootstrap)
#   - clang on PATH (the blessed `soldr build` surface installs
#     soldr-managed clang shims ahead of system clang so ring 0.17.x's
#     hardcoded `c.compiler("clang")` routes to clang-cl for MSVC)
#   - cargo-xwin (the legacy `soldr cargo build` surface uses this
#     to download the MS CRT + SDK headers; still installed for
#     comparison runs)
#   - run_with_ts.py wraps every cargo / soldr invocation so each
#     output line carries an elapsed-seconds prefix, making the
#     "stuck step" pattern from GHA observable line-by-line
#
# Build:
#   docker build -f ci/docker-aarch64-windows-msvc-cross/Dockerfile \
#       -t soldr-aarch64-windows-msvc-cross .
#
# Run (from repo root):
#   docker run --rm -v "$PWD:/src" -w /src \
#       soldr-aarch64-windows-msvc-cross \
#       bash ci/docker-aarch64-windows-msvc-cross/build.sh

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Minimum apt set — mirrors what GHA's ubuntu-24.04 image provides
# plus the deps soldr / cargo-xwin / ring expect.
#   curl + ca-certificates  → toolchain + soldr release downloads
#   git                     → cargo fetch from git deps
#   xz-utils + bzip2 + zip + unzip → archive plumbing
#   build-essential         → host cc/ld for build scripts
#   pkg-config              → many *-sys crates poke pkg-config
#   file + jq               → diagnostics + catalogue parsing
#   python3-minimal         → run_with_ts.py
#   clang + lld + llvm + llvm-dev → ring's build.rs needs clang-cl
#                             routing; lld for cross-link; llvm-lib
#                             from llvm-dev satisfies cargo-xwin
#   cmake + perl            → zstd-sys + openssl-sys build scripts
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl git xz-utils bzip2 zip unzip \
        build-essential pkg-config file jq python3-minimal \
        clang lld llvm llvm-dev libclang-dev \
        cmake perl \
 && rm -rf /var/lib/apt/lists/*

COPY .github/scripts/download_large_asset.sh /usr/local/bin/download-large-asset
RUN chmod 0755 /usr/local/bin/download-large-asset

# Stock rustup — installs the pinned toolchain from rust-toolchain.toml
# on first cargo invocation in /src.
RUN curl -fsSL https://sh.rustup.rs | sh -s -- \
        --default-toolchain none -y \
 && /root/.cargo/bin/rustup --version

ENV PATH=/root/.cargo/bin:$PATH \
    RUSTUP_HOME=/root/.rustup \
    CARGO_HOME=/root/.cargo

# Install the latest published soldr. Only setup-soldr's default pins a soldr
# version; this image takes the current release. The blessed
# `soldr build --target aarch64-pc-windows-msvc` surface installs clang shim
# names so ring's hardcoded `clang` invocations route to clang-cl.
COPY --from=ghcr.io/astral-sh/uv:0.9.18 /uv /usr/local/bin/uv
RUN set -eu; \
    UV_TOOL_DIR=/opt/soldr-tool UV_TOOL_BIN_DIR=/opt/soldr \
        uv tool install --python-preference only-managed soldr; \
    /opt/soldr/soldr --version

ENV PATH=/opt/soldr:$PATH

# Pre-install cargo-xwin (the legacy cross-MSVC path) for comparison.
# `soldr build --target *-pc-windows-msvc` will resolve cargo-xwin via
# the catalogue; this baked-in copy avoids a per-build fetch.
RUN /root/.cargo/bin/rustup toolchain install stable --profile minimal --no-self-update \
 && /root/.cargo/bin/cargo +stable install cargo-xwin --version 0.18.6 \
 && cargo-xwin --version

# soldr#1036: pre-stage an xwin SDK splat at image-build time so the
# e2e `cargo nextest archive --target aarch64-pc-windows-msvc` step
# doesn't have to download MS CRT + SDK at every harness run. Mirrors
# the recipe used by `xwin-cache-windows-arm64` in zackees/soldr-
# toolchain (the splatted layout that cargo-xwin's XWIN_CACHE_DIR
# consumer expects). Uses xwin directly (not cargo-xwin) so we get
# just the splat without driving a per-crate build.
ENV XWIN_VERSION=0.6.5 \
    XWIN_CACHE_DIR=/opt/xwin-cache

RUN set -eu; \
    url="https://github.com/Jake-Shadle/xwin/releases/download/${XWIN_VERSION}/xwin-${XWIN_VERSION}-x86_64-unknown-linux-musl.tar.gz"; \
    echo "fetching xwin ${XWIN_VERSION} from ${url}"; \
    curl -fsSL --connect-timeout 10 --max-time 30 "${url}.sha256" -o /tmp/xwin.sha256; \
    xwin_sha=$(awk '{print $1}' /tmp/xwin.sha256); \
    download-large-asset --url "$url" --output /tmp/xwin.tgz --sha256 "$xwin_sha" --expected-size 3557511; \
    mkdir -p /tmp/xwin-extract; \
    tar -xzf /tmp/xwin.tgz -C /tmp/xwin-extract; \
    xwin_bin=$(find /tmp/xwin-extract -type f -name xwin | head -1); \
    chmod +x "$xwin_bin"; \
    "$xwin_bin" --accept-license \
        --arch aarch64 \
        splat \
        --output "$XWIN_CACHE_DIR" \
        --preserve-ms-arch-notation; \
    rm -rf /tmp/xwin.tgz /tmp/xwin.sha256 /tmp/xwin-extract; \
    # Sanity check — xwin's canonical splatted layout has crt/ + sdk/
    [ -d "$XWIN_CACHE_DIR/crt" ] && [ -d "$XWIN_CACHE_DIR/sdk" ] || { \
        echo "FATAL: xwin splat produced unexpected layout under $XWIN_CACHE_DIR" >&2; \
        ls -la "$XWIN_CACHE_DIR" >&2 || true; \
        exit 1; \
    }; \
    du -sh "$XWIN_CACHE_DIR"

# Kernel-independent 32-bit ELF guard — same paranoia as the
# aarch64-musl harness. Catches future toolchain vendor changes that
# might ship 32-bit binaries that locally work but ENOEXEC on GHA.
RUN echo "scanning /opt /usr/local/bin /root/.cargo/bin for 32-bit ELFs..." \
 && bad_files=$(find /opt /usr/local/bin /root/.cargo/bin -type f -executable -exec file {} + 2>/dev/null | awk '/ELF 32-bit/ {print $1}' | sed 's/:$//') \
 && if [ -n "$bad_files" ]; then \
        echo "FATAL: 32-bit ELF binaries found that would ENOEXEC on GHA azure kernel:" >&2; \
        echo "$bad_files" >&2; \
        exit 1; \
    fi \
 && echo "OK: no 32-bit ELF binaries found"
