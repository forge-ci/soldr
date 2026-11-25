# soldr Implementation Guide

This document defines the implementation direction for soldr.

For user-facing command behavior, see [docs/API.md](docs/API.md).

---

## What soldr is

One binary. Two visible jobs. One internal job.

1. Build front door
   `soldr cargo ...` is the primary user experience for Rust builds.
2. Tool fetcher
   `soldr <tool> ...` fetches and runs Rust CLI tools.
3. Internal wrapper
   soldr participates in builds by sitting in the `RUSTC_WRAPPER` slot after `soldr cargo ...` wires it up.

The important product rule is that users should think in terms of `soldr cargo ...`, not in terms of manually exporting `RUSTC_WRAPPER`.

## What soldr is not

- Not a separate build language
- Not a project scaffolder
- Not a Rust toolchain manager *(under active reconsideration — see [docs/RUSTUP_REPLACEMENT_ANALYSIS.md](docs/RUSTUP_REPLACEMENT_ANALYSIS.md) and issue #235; the current recommendation is a hybrid native dist fetcher with rustup as a fallback, but until that work lands the position remains as stated)*

soldr can delegate to Cargo, but it should not try to replace Cargo's flags, profiles, or dependency model.

### The `ci-test` orchestration exception

`soldr ci-test` is a deliberately narrow exception to the usual rule that
Cargo owns orchestration. It freezes one prescribed host-validation DAG so CI
cannot accidentally compile the same test profile twice under superficially
different commands. Cargo remains the freshness authority and Soldr remains
the per-rustc cache; `ci-test` only fixes the order, scope, target directories,
and compatibility boundaries between them.

Compatible stable host work shares one target tree. Clippy subsumes a separate
`soldr cargo check`, and Nextest performs the sole test-profile build through
`--no-run` before the Dylint branch begins. The corresponding Fresh Nextest
execution then overlaps the serial Dylint libraries -> workspace analysis ->
UI-test branch; doctests are the explicit join and dependency policy follows
them. Unset `CARGO_BUILD_JOBS` and `SOLDR_JOBS` remain unset so Cargo and the
canonical Soldr compiler admission gate retain their normal parallelism.
Explicit values are preserved byte-for-byte. Only `NEXTEST_TEST_THREADS` is
chosen by ci-test when unset: the smaller of the CPU count and the per-test
memory budgets that fit in measured available memory (soldr#2885), with a
run-time pressure gate and per-test memory ceiling around each test.
Nextest compilation completes before the fork,
but individual tests intentionally launch nested Cargo/compiler fixtures.
Those compiles and Dylint share the daemon's canonical shared/exclusive
admission; their dynamic child-jobserver count is not converted into a global
Cargo cap. Host/cgroup memory remains observational telemetry, never an
applied global Cargo job cap. Doctests remain a rustdoc execution family.
Dylint is the
explicit exception: its exact pinned nightly uses separate nightly-keyed
library, workspace-analysis, and UI-test trees and never contaminates the
stable project target. The Dylint domain self-provisions the catalogue-pinned
frontend, linker, and matching prebuilt driver before any lint stage starts.
Dependency-policy tools do not execute the compiler.

The command rejects target, toolchain, profile, target-directory, and manifest
overrides that would create an undeclared compile domain. Use
`soldr ci-test --explain-plan --format json` to inspect the versioned plan, or
the explicit `soldr cargo ...` surface when a different domain is intentional.

---

## Core Principles

### 1. Front-door UX first

The normal build path is:

```bash
soldr cargo build
soldr cargo test
soldr cargo check
```

If the user has to understand `RUSTC_WRAPPER` just to get value from soldr, the product shape is wrong.

### 2. Cargo compatibility

The front door must preserve normal Cargo arguments. soldr should delegate to real Cargo, not reimplement Cargo semantics.

### 3. Wrapper mode is an implementation detail

Wrapper mode still matters, but it exists to support the front door. It is not the primary mental model.

### 4. Pre-built tools first

When users run `soldr <tool> ...`, prefer pre-built binaries before any source build path.

### 5. MSVC by default on Windows

On Windows, soldr should prefer MSVC targets unless the project explicitly requires GNU.

### 6. Bootstrapper mindset

soldr should prove it can build:

- itself
- other Rust software

That bootstrap story is a first-class requirement, not a side effect.

---

## Command Model

### Primary commands

```text
soldr cargo <cargo-args...>
soldr <tool>[@version] [tool-args...]
soldr status
soldr clean
soldr config
soldr cache
soldr version
```

### Internal execution model

For `soldr cargo ...`:

1. Resolve Cargo through Soldr's front door while retaining the caller's host
   toolchain context for host-owned binaries and the managed context for
   Soldr-owned binaries
2. Resolve rustc from the same selected toolchain context
3. Set `RUSTC_WRAPPER` to a compiler-named Soldr shim
4. Auto-start `soldr-daemon`, which owns the in-process zccache service, and
   pass only Soldr/session correlation state through the Cargo environment
5. Delegate to Cargo with unchanged user flags

For wrapper mode:

1. Detect the compiler-wrapper invocation shape
2. Resolve the real compiler from the selected toolchain context
3. Send cache-enabled compiler work over Soldr IPC to the zccache service
   embedded in `soldr-daemon`
4. Run the compiler directly only when caching is disabled or the Soldr daemon
   is unavailable and fallback policy permits it

### One daemon: embedded zccache (soldr#1467)

soldr runs exactly one long-lived process: soldr-daemon. The zccache build
cache is hosted *inside* it as an embedded service — wrapper invocations
ferry each compile to the daemon over the broker SESSION route. No
standalone `zccache-daemon` or `zccache-download-daemon` process is ever
spawned, and nothing in soldr may reach the upstream lazy-spawn entry
points (enforced by `crates/soldr-cli/tests/guards/no_standalone_spawn_lint.rs`).

Compiler resource admission has one owner (soldr#2932): embedded zccache owns
both the capacity semaphore and the fair shared/exclusive gate, in that order.
Admission happens only after cache lookup, so a cache hit never drains active
compiler work or waits for an oversized-unit barrier. zccache owns the generic
C/C++ size/name predicate and its published Rust amalgamation names; Soldr adds
only product-specific Rust classification (currently `kernal_api`) through the
embedded host-admission classifier. Soldr must not put another general compiler
gate around `ZccacheService::compile`, because that would move admission ahead
of hit/miss knowledge and recreate two schedulers with different predicates.

**One shipped executable (argv[0] multicall).** Every shipped executable file
is the same `soldr` image, either the binary itself or a hardlink, symlink, or
copied alias dispatched by `crates/soldr-cli/src/multicall.rs`. Aliases may be
created at staging, install, or runtime. New functionality is a library plus a
`soldr` subcommand, with an argv[0] identity when a distinct process name is
needed, never a separate shipped binary target/package. The repository-wide
`crates/soldr-cli/tests/guards/multicall_bin_layout.rs` guard enforces this layout;
see `CLAUDE.md` "Key Design Rules" for named non-shipped exceptions, fixture
exclusions, and the owner-approval requirement for new exceptions.

Compiler shims are named `rustc`, `clippy-driver`, or `zccache-soldr`, but a
long-lived daemon process must never inherit one of those executable
identities. The Cargo front door passes a canonical `soldr-daemon` multicall
alias to compiler children; a standalone compiler shim lazily materializes the
same alias before recovery. Lifecycle refuses a compiler-named fallback rather
than publishing a PID that its own recycled-PID safety check cannot trust.

Daemon startup has two distinct locks: the short-lived `.spawn.lock` suppresses
wrapper herds, while `root-owner.lock` is held by the daemon for its full
lifetime and shared with explicit orphan-root maintenance. On Unix, the child
binds the socket before publishing its PID/version. Retirement deliberately
leaves the PID, version, and socket claims in place; the next root owner
validates liveness, reclaims the stale socket, and overwrites the claims during
startup. Successor-owned cleanup avoids a check-then-unlink race in which an
older or idle-timed-out daemon could remove a live successor's endpoint.

`soldr zccache <args>` is a Soldr-owned compatibility surface implemented by
`crates/soldr-cli/src/zccache_compat.rs`, never an upstream CLI pass-through.
It adapts `cache-root`, `session-end`, and `stop` to Soldr's cache root,
session-end command, and daemon stop command; `rust-plan` is retired because
Soldr owns artifact-plan save/restore around `soldr cargo <verb>`. Version
selectors and unknown subcommands are refused. No upstream zccache CLI or
standalone zccache executable is resolved, downloaded, or invoked.

The daemon is also the primary cache-retention owner. One daemon maps to one
exact `SoldrPaths` root and persists its five-minute pressure / 24-hour age
schedule beneath that root. Embedded zccache runs the same bounded retention
engine as standalone zccache but receives the soldr-owned child root and never
the standalone `~/.zccache` default. The host coordinates history, PEP517,
cook, trash, target, and stale-generation cleanup around active build leases.
Builds hold a shared root-maintenance lock through sanitized history
publication; a pass holds the exclusive side from its first decision through
its last deletion. Daemon startup and explicit orphan-root maintenance also
share a version-blind root-owner lock. Shutdown waits for a pass that already
started before publishing the root as unowned.
Default daemon lifetime is bounded at 30 minutes of inactivity. Retention does
not depend on residency: the maintenance loop runs its first tick immediately
and treats an absent or overdue full marker as a catch-up, and the markers are
persisted under the root, so a restarted daemon resumes the schedule. An
unbounded default was tried (soldr#1782) and reverted, because a broker route
is keyed on the soldr root and so every throwaway root earned a daemon that
never exited. A caller may still pass an explicit idle timeout, and a caller
that owns a throwaway root may name itself with `--owner-pid` so its daemon
exits when it does. No operating-system scheduler is installed.

`soldr doctor` reports leftovers from pre-embedded installs or direct
zccache CLI use: running `zccache-daemon*` processes and stale per-launch
copies under `<zccache-root>/*/runtime-binaries/`, with a cleanup hint.

---

## Architecture

```text
crates/
|-- soldr-core
|-- soldr-fetch
|-- soldr-cache
`-- soldr-cli
```

### soldr-core

Owns:

- configuration
- target detection
- cache paths
- shared error types

### soldr-fetch

Owns:

- tool resolution
- archive download and extraction
- tool cache management

### soldr-cache

Owns:

- wrapper behavior around `rustc`
- cache keying
- artifact storage
- daemon and IPC work

### soldr-cli

Owns:

- mode detection
- command dispatch
- Cargo delegation
- fetched-tool process execution

---

## Implementation Phases

### Phase 1: Cargo Front Door

Done when:

- `soldr cargo build` works
- `soldr cargo test` works
- wrapper mode is wired automatically
- users no longer need manual `RUSTC_WRAPPER` setup for the common case

### Phase 2: Tool Fetching

Done when:

- `soldr maturin build`
- `soldr cargo-dylint check`
- `soldr rustfmt ...`

all resolve quickly from cache or pre-built binaries.

Rustfmt resolution is cached, but recursive formatting itself is not skipped:
Cargo passes crate roots while rustfmt discovers child modules dynamically.
Only an invocation that explicitly sets `skip_children=true` may use zccache's
content-marker shortcut.

### Phase 3: Build Cache

Done when:

- `soldr cargo ...` enables the zccache service embedded in `soldr-daemon` by default
- `soldr --no-cache cargo ...` cleanly bypasses the cache path
- wrapper mode routes cache-enabled builds over Soldr IPC into the embedded
  service instead of spawning an external zccache process
- cache commands report and manage real zccache state

### Phase 4: Bootstrap Validation

Done when:

- soldr can build itself per target
- soldr can build a pinned third-party Rust project per target
- CI exposes one workflow per badge target

---

## CI Expectations

The repository should verify two things independently:

1. soldr builds on each supported target
2. soldr can bootstrap and build another Rust project on each supported target

Badge visibility matters, so these should be separate workflow entry points rather than a single hidden matrix.

Reusable workflow templates are fine, but the public workflows should remain one file per badge target.

---

## Design Guardrails

- Do not regress to a `RUSTC_WRAPPER`-first UX in docs or examples.
- Do not proxy Cargo by reimplementing Cargo flags.
- Do not require users to learn internal wrapper mechanics for the happy path.
- Keep the wrapper contract compatible with normal Cargo execution.

---

## References

- [README.md](README.md)
- [docs/API.md](docs/API.md)
