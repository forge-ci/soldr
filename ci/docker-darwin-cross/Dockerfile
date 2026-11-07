# Darwin cross-compile harness for soldr's release-auto.yml apple-darwin lanes.
#
# Goal: prove a Linux host can produce a Mach-O `soldr` binary for
# x86_64-apple-darwin (and aarch64-apple-darwin) using plain `cargo
# build` + a complete Apple SDK sysroot, NOT `cargo zigbuild`. Once the
# env block in this harness produces a working binary, the same env is
# already codified into `.github/workflows/release-auto.yml` (the
# darwin Linux-host branch of the build step).
#
# Image contents:
#   - rustup 1.98.1 + cross-compile targets x86_64-apple-darwin + aarch64-apple-darwin
#   - clang-18 / clang++-18 + lld-18 (Mach-O linker)
#   - llvm-18 (llvm-ar / llvm-ranlib)
#   - MacOSX11.3.sdk extracted under /opt/apple-sdk/MacOSX*.sdk — same
#     SHA-pinned bundle apple_sdk.rs fetches in production
#
# Inner-loop speed:
#   - source is bind-mounted from $PWD (no rebuild for source edits)
#   - cargo registry + target dir live in named volumes — first invocation
#     populates them (~5-10 min cold), subsequent iterations are 30s-2 min
#
# Build the image (one-time, ~5-10 min):
#   docker build -f ci/docker-darwin-cross/Dockerfile -t soldr-darwin-cross .
#
# Run a darwin x64 build (iteration, ~30s-5 min depending on warm state):
#   ./ci/docker-darwin-cross/build.sh                       # x86_64-apple-darwin
#   ./ci/docker-darwin-cross/build.sh aarch64-apple-darwin
#
# Interactive shell:
#   docker run --rm -it \
#     -v "$PWD:/src" -w /src \
#     -v soldr-darwin-cargo-registry:/root/.cargo/registry \
#     -v soldr-darwin-target:/src/target \
#     soldr-darwin-cross bash
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Single apt invocation for the whole toolchain (faster image build).
# lld-18 is the load-bearing add over the v0.7.86 docker harness — cmake
# crates (libz-ng-sys, etc.) need lld in PATH for the Mach-O test link.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        xz-utils \
        zstd \
        tar \
        clang-18 \
        clang++-18 \
        lld-18 \
        llvm-18 \
        cmake \
        make \
        pkg-config \
        git \
        build-essential \
    && ln -sf /usr/bin/clang-18    /usr/bin/clang \
    && ln -sf /usr/bin/clang++-18  /usr/bin/clang++ \
    && ln -sf /usr/bin/llvm-ar-18  /usr/bin/llvm-ar \
    && ln -sf /usr/bin/llvm-ranlib-18 /usr/bin/llvm-ranlib \
    && ln -sf /usr/bin/lld-18      /usr/bin/ld.lld \
    && ln -sf /usr/bin/lld-18      /usr/bin/lld \
    && rm -rf /var/lib/apt/lists/*

# --- Apple SDK (MacOSX*.sdk) ---
# Same URL apple_sdk.rs uses in production (MANAGED_APPLE_SDK_URL):
# `https://media.githubusercontent.com/media/zackees/soldr/manifest/deps/mac/sdk.tar.zstd`
# .tar.zstd (zstd compression), NOT .tar.xz — v0.7.85 docker tried the
# wrong URL + extension and 404'd. The bundle contains a top-level
# `MacOSX<version>.sdk/` directory.
ARG APPLE_SDK_URL=https://media.githubusercontent.com/media/zackees/soldr/manifest/deps/mac/sdk.tar.zstd
RUN mkdir -p /opt/apple-sdk \
 && curl -fsSL "$APPLE_SDK_URL" -o /tmp/sdk.tar.zstd \
 && zstd -dc /tmp/sdk.tar.zstd | tar -xf - -C /opt/apple-sdk \
 && rm /tmp/sdk.tar.zstd \
 && SDK_DIR="$(ls -d /opt/apple-sdk/MacOSX*.sdk | head -1)" \
 && test -f "$SDK_DIR/usr/include/sys/syscall.h" \
 || (echo "ERROR: Apple SDK extraction did not yield usr/include/sys/syscall.h" >&2 \
     && ls -la /opt/apple-sdk >&2 && exit 2) \
 && echo "Apple SDK at: $SDK_DIR" \
 && echo "  has sys/syscall.h: $(test -f $SDK_DIR/usr/include/sys/syscall.h && echo YES || echo NO)" \
 && echo "  has os/lock.h:     $(test -f $SDK_DIR/usr/include/os/lock.h && echo YES || echo NO)"

# --- rustup + rust 1.98.1 + cross targets ---
ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    PATH=/root/.cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.98.1 --profile minimal \
 && rustup target add x86_64-apple-darwin aarch64-apple-darwin \
 && rustc --version

COPY ci/docker-darwin-cross/build-soldr-for-darwin.sh /opt/build-soldr-for-darwin.sh
RUN chmod +x /opt/build-soldr-for-darwin.sh

WORKDIR /src
CMD ["/opt/build-soldr-for-darwin.sh", "x86_64-apple-darwin"]
