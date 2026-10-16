# Docker image used to build and test the cross-repo shared `soldr cook`
# artifact cache feature (issues #576, #577, #578 under meta #579) and
# the general perf_local convenience CLI (issue #593).
#
# Why Docker is mandatory for cook-shared-cache work: PRs 1-3 mutate
# the soldr daemon protocol, the `~/.soldr/state.redb` schema, and the
# cargo-front-door hot path. soldr installs as a per-user singleton —
# running `cargo build` or `cargo test` for these PRs on the host would
# corrupt the host developer's running daemon and persistent state.
# Mounting `~/.soldr/` as a container-local volume (tmpfs or named
# volume — NEVER a bind-mount of the host path) keeps the host
# singleton intact.
#
# Volume layout (issues #593 and #1553): see `bench/cook_in_docker.sh`
# and `ci/perf_local.py`. The latter creates one named runner and reuses it
# through `docker exec` instead of creating a container per invocation.
# `perf_local.py` mounts its target volume at `/target`; the cook harness
# retains its historical `/work/target` mount. Both use `/root/.cargo`.
# These are NAMED VOLUMES (not host bind mounts), so cargo's mtime-based
# fingerprint check survives container restarts. Without this, Windows + Docker Desktop's WSL2
# 9P translation layer rewrites mtimes per container start and cargo
# rebuilds everything (~6 min on a fresh 21-crate workspace, vs ~1 s
# when warm).
#
# Usage (via bench/cook_in_docker.sh):
#   docker build -f docker/cook-shared-cache/Dockerfile -t soldr-cook-dev .
#   docker run --rm -v "$(pwd):/work" \
#       -v cook-soldr-home:/root/.soldr \
#       -v soldr-perf-target:/work/target \
#       -v soldr-perf-cargo-home:/root/.cargo \
#       -e SOLDR_COOK_DOCKER_HARNESS=1 -w /work soldr-cook-dev \
#       cargo test --workspace -- --include-ignored cook_index

# Dylint 6.0.1's official GNU/Linux binary requires glibc 2.39 and embeds
# its CI checkout path, so soldr rejects it as non-relocatable and builds
# cargo-dylint from source. Trixie keeps the runner compatible with both
# that upstream probe and the resulting host-local binary.
FROM rust:1.98.1-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546

# Keep the local Python half on the same pinned uv used by the native-wheel
# Docker harness. It lives in the image rather than the persistent venv.
COPY --from=ghcr.io/astral-sh/uv:0.9.18@sha256:5713fa8217f92b80223bc83aac7db36ec80a84437dbc0d04bbc659cae030d8c9 /uv /uvx /usr/local/bin/

# Build tools needed by the soldr-cli workspace and the integration
# tests (linker, pkg-config, git for origin-URL probes in PRs 2/3).
# clang and lld are required because soldr's blessed default
# LinkerChoice::Fast, absent `reld` on PATH, resolves to a driver shim
# that execs clang (`exec clang -fuse-ld=lld "$@"`) (soldr#2930); the
# sibling harness images (ci/docker-darwin-cross,
# ci/docker-aarch64-windows-msvc-cross) already install this pair.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       build-essential \
       pkg-config \
       libssl-dev \
       git \
       ca-certificates \
       curl \
       cmake \
       ninja-build \
       jq \
       gdb \
       procps \
       python3 \
       python3-venv \
       zstd \
       clang \
       lld \
    && rm -rf /var/lib/apt/lists/*

# soldr#2424 P1: bake a stable machine identity. Minimal container images
# ship no /etc/machine-id, so every broker SID / host-identity consumer in
# the vendored running-process suite exercised only its fallback here —
# and before running-process#1024 added one, 26 SID-dependent tests could
# not run in this image at all. A fixed random id (not the host's) keeps
# the identity container-scoped while giving each container generation a
# stable value across its lifetime.
RUN { dbus-uuidgen 2>/dev/null \
      || head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n'; } > /etc/machine-id \
    && echo >> /etc/machine-id \
    && grep -qE '^[0-9a-f]{32}$' /etc/machine-id

# soldr's `cargo test` jobs check the system rustc version; pin it here
# to match `rust-toolchain.toml` (Rust 1.98.1).
RUN rustup default 1.98.1 \
    && rustup component add rustfmt clippy

# Bootstrap current source with the latest published Soldr. Keep
# this isolated from the warm runner volumes as immutable image state. The
# bootstrap must include zccache's amalgamation admission gate (Soldr 0.9.6+):
# this image invokes it to compile current Soldr before the current source can
# provide that protection itself. It must also carry the PDEATHSIG fix
# (soldr#3211), which landed after 0.9.15, so a cold bootstrap compile cannot
# be SIGTERMed mid-run (soldr#2930).
RUN python3 -m venv /opt/soldr-bootstrap \
    && /opt/soldr-bootstrap/bin/pip install --disable-pip-version-check soldr \
    && /opt/soldr-bootstrap/bin/soldr --version

# The bootstrap is the latest published soldr (only setup-soldr's default pins
# a soldr version). Its daemon and embedded-cache state live on the persistent
# `/root/.soldr` volume under a bootstrap root of their own. This used to be
# keyed by the pinned bootstrap version so a daemon restored from an older image
# could not service a newer bootstrap; since soldr#3568 daemon routes, their
# private cache stores and root claims are keyed per daemon-image generation,
# and a container's daemons die with it, so no older live daemon can be left
# behind in the volume to take the root over.
ENV SOLDR_CACHE_DIR="/root/.soldr/bootstrap"
ENV PATH="/opt/soldr-bootstrap/bin:${PATH}"
# bosn runs every task as `sh -lc`, and a Debian login shell's /etc/profile
# overwrites PATH, dropping the ENV above and the base image's
# /usr/local/cargo/bin (rustup). soldr's tests then failed 59 times with
# "failed to invoke rustup which cargo" (zackees/ci.yml#172). Login shells
# source /etc/profile.d, so restate the image's PATH there.
RUN printf 'export PATH="/opt/soldr-bootstrap/bin:/usr/local/cargo/bin:$PATH"\n' \
        > /etc/profile.d/soldr-image-path.sh

# CARGO_HOME explicit so the `soldr-perf-cargo-home` named volume mount
# point is unambiguous. Without this the path depends on the base
# image's $HOME inheritance — `/root/.cargo` happens to be the default
# for the official `rust` image but pinning it makes future image
# migrations safe.
ENV CARGO_HOME=/root/.cargo
# Docker Desktop exposes many host CPUs but this development VM has only
# about 8 GiB of RAM. Two concurrent rustc processes keep large crates below
# the cgroup limit; higher automatic fan-out repeatedly OOM-kills cold test
# graph builds and is slower overall because Cargo must start over.
ENV CARGO_BUILD_JOBS=2 \
    SOLDR_JOBS=2
# This is a development/smoke image: make the effective Cargo profiles
# explicit so a future workspace-profile change cannot turn the local loop
# into optimized or LTO codegen. High codegen-unit counts and incremental
# state minimize edit-to-test latency. Debug assertions stay enabled through
# the normal dev/test defaults, but this speed-focused image emits no DWARF.
ENV CARGO_PROFILE_DEV_OPT_LEVEL=0 \
    CARGO_PROFILE_DEV_LTO=false \
    CARGO_PROFILE_DEV_CODEGEN_UNITS=256 \
    CARGO_PROFILE_DEV_INCREMENTAL=true \
    CARGO_PROFILE_DEV_DEBUG=0 \
    CARGO_PROFILE_TEST_OPT_LEVEL=0 \
    CARGO_PROFILE_TEST_LTO=false \
    CARGO_PROFILE_TEST_CODEGEN_UNITS=256 \
    CARGO_PROFILE_TEST_INCREMENTAL=true \
    CARGO_PROFILE_TEST_DEBUG=0 \
    CARGO_PROFILE_RELEASE_OPT_LEVEL=0 \
    CARGO_PROFILE_RELEASE_LTO=false \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=256 \
    CARGO_PROFILE_RELEASE_INCREMENTAL=true \
    CARGO_PROFILE_RELEASE_DEBUG=0 \
    CARGO_PROFILE_BENCH_OPT_LEVEL=0 \
    CARGO_PROFILE_BENCH_LTO=false \
    CARGO_PROFILE_BENCH_CODEGEN_UNITS=256 \
    CARGO_PROFILE_BENCH_INCREMENTAL=true \
    CARGO_PROFILE_BENCH_DEBUG=0 \
    CARGO_PROFILE_CI_BOOTSTRAP_OPT_LEVEL=0 \
    CARGO_PROFILE_CI_BOOTSTRAP_LTO=false \
    CARGO_PROFILE_CI_BOOTSTRAP_CODEGEN_UNITS=256 \
    CARGO_PROFILE_CI_BOOTSTRAP_INCREMENTAL=true \
    CARGO_PROFILE_CI_BOOTSTRAP_DEBUG=0 \
    CARGO_PROFILE_CI_RELEASE_OPT_LEVEL=0 \
    CARGO_PROFILE_CI_RELEASE_LTO=false \
    CARGO_PROFILE_CI_RELEASE_CODEGEN_UNITS=256 \
    CARGO_PROFILE_CI_RELEASE_INCREMENTAL=true \
    CARGO_PROFILE_CI_RELEASE_DEBUG=0 \
    CARGO_PROFILE_CI_NEXTEST_OPT_LEVEL=0 \
    CARGO_PROFILE_CI_NEXTEST_LTO=false \
    CARGO_PROFILE_CI_NEXTEST_CODEGEN_UNITS=256 \
    CARGO_PROFILE_CI_NEXTEST_INCREMENTAL=true \
    CARGO_PROFILE_CI_NEXTEST_DEBUG=0
ENV UV_CACHE_DIR=/root/.cache/uv
ENV UV_PROJECT_ENVIRONMENT=/venv

# Source tree is mounted by the runner at /repo; no COPY here so each
# `docker exec` operates on the live worktree.
WORKDIR /work

# Marker env var read by tests gated on the Docker harness. Bare-host
# `cargo test` runs are no-ops for these gated tests; only `docker run`
# with this image enables them.
ENV SOLDR_COOK_DOCKER_HARNESS=1

# zackees/ci.yml#168 (GATE-005), soldr#3516: soldr's test suite refuses to run
# on a developer host (.github/scripts/nextest_wrapper.sh), where a leaked test
# daemon once claimed the real ~/.soldr root and wedged every soldr build on
# the machine. This image is the isolated environment the suite may run in.
ENV SOLDR_TEST_ISOLATED=1

# Default command is a help message; the runner overrides with the
# actual `cargo test` invocation.
CMD ["echo", "soldr-cook-dev image — invoke via bench/cook_in_docker.sh or ci/perf_local.py"]
