# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> [!IMPORTANT]
> Ordinary PR and `main` CI runs lint plus Linux x64 `soldr ci-test`. Add the
> `ci-full` PR label for platform-sensitive or target/toolchain changes; release
> candidates require full CI on the exact commit SHA. Third-party PRs must not
> merge until `Full coverage` proves all required cells ran. See [CI modes](docs/CI_MODES.md).

> [!IMPORTANT]
> ## ⚡ Performance work → read [PERF.md](PERF.md) FIRST ⚡
>
> **The Perf Matrix GitHub Action is the MOST important workflow in this repo.**
> If you are testing, measuring, optimizing, or regressing soldr's performance
> — read **[PERF.md](PERF.md)** before doing anything else.
>
> Branch naming (`perf/<plat>-<fix>-<scen>`) controls exactly which cells run.
> Pushing to a wrong branch name silently runs the full sweep — costly and slow.
> **Do not guess.** PERF.md has the complete 48-pattern scope table.

## What is soldr

A Rust command suite whose main multicall binary has two user-facing jobs:
1. **Tool fetcher** — download and run pre-built Rust tool binaries (like npx/crgx)
2. **Compilation cache** — sit in the `RUSTC_WRAPPER` slot, hash rustc inputs,
   and cache artifacts (like sccache). When no explicit wrapper override is
   set, `RUSTC_WRAPPER` points at a compiler-named Soldr shim that sends work
   to the zccache service embedded in the `soldr-daemon` sidecar.

Mode is detected automatically from argv[1]: path-to-rustc → cache mode, built-in command → dispatch, anything else → tool fetch.

**soldr wraps rustc, NOT cargo.** This is the most important design decision: cargo owns build orchestration, soldr owns per-unit caching. See DESIGN.md "Why no `soldr build`" for rationale. As of #685 (phase 2 of #682) `soldr test` / `soldr clippy` / etc. are accepted as **dispatch shorthand** for `soldr cargo test` / `soldr cargo clippy` — they route through the cargo front door and do not become soldr-native verbs (the `Commands::Cargo` arm is still where the work happens).

## Two build paths — blessed vs legacy (soldr#1010)

`soldr build --target <triple>` is the **blessed-default surface**; `soldr cargo build --target <triple>` is the **explicit legacy passthrough**. Both invoke cargo under the hood — the difference is the toolchain stack each surface mounts and which behaviors evolve at each level:

| Verb | Surface intent | Underlying mechanism (today) | Evolution (soldr#1010) |
|---|---|---|---|
| **`soldr build --target X`** | **blessed default** | Uses cargo under the hood after preparing catalogue-driven sysroots, compiler/linker env, and shims for supported targets. | Resolves required libs/headers from `https://zackees.github.io/soldr-toolchain/catalogue.v1.json`, materializes under `~/.soldr/sdk/<triple>/`, exports env vars, and ships required compiler/linker shims so supported cross-compiles do not invoke the legacy `cargo xwin` or `cargo zigbuild` subcommands. GNU Linux currently uses managed Zig directly through soldr-generated wrappers; a Zig-free implementation is tracked by #2220. Zero-auth, zero-API-quota, pre-compressed `tar.zst`. |
| **`soldr cargo build --target X`** | **explicit legacy** | Passes through the cargo front door and preserves explicit cargo subcommands such as `cargo xwin build` or `cargo zigbuild`. | Stays as the documented fallback when users want the historical path or when blessed misses an asset. **There is no env-var opt-out on the blessed surface**: `SOLDR_USE_LEGACY_XWIN` / `SOLDR_USE_LEGACY_ZIGBUILD` were removed in soldr#2519 because they routed around the pinned, sha256-verified SDK to an unpinned third-party download. `soldr build` either prepares the blessed toolchain or fails. |

The split is **a surface contract, not an implementation contract**. `soldr build` may delegate to cargo internally; what matters is that callers asking for `soldr build` get the soldr-blessed toolchain story (with whatever extras land there over time), while callers asking for `soldr cargo build` get the legacy cargo-xwin/zigbuild path verbatim. Internal sharing of the dispatch is allowed and expected.

Friendly target aliases (`win-x64`, `mac-arm64`, etc.) are accepted by both verbs and resolve identically.

## Build Commands

```bash
# Dev environment setup (installs uv if needed)
./install

# Rust
soldr cargo build -p soldr-cli              # Build CLI binary
soldr cargo test --workspace                 # Run all Rust tests
soldr cargo clippy --workspace               # Lint Rust
soldr cargo fmt --all -- --check             # Check Rust formatting

# Python (linting/testing the PyPI wrapper)
./lint                                 # ruff, black, isort, flake8, pylint, mypy
./test                                 # full build + test pipeline

# Maturin (Python+Rust packaging)
uv run maturin develop                 # Build & install in venv
uv run maturin build --release         # Build wheel
```

## Architecture

**Internal workspace split (#1490).** The 2026-05 monocrate collapse is being reversed into `publish = false` internal crates (the zccache pattern, no amalgamation): all five phases have landed: `soldr-core`, `soldr-fetch`, `soldr-cache`, and `soldr-daemon` are extracted, with `soldr-cli` as the facade + `[[bin]]` crate. soldr publishes no crates, so workspace membership has no external surface — the old `monocrate_guard.rs` test was deleted with the split. `crates/soldr-cli/tests/guards/no_timed_test_guard.rs` walks every workspace crate under `crates/`.

- **`crates/soldr-core`** — foundation crate: shared types, config (`~/.soldr/config.toml`), target triple resolution (MSVC default on Windows at runtime), error types, the daemon wire schema (`core::wire`), Windows Defender exclusion plumbing (`defender`), and `self_relocate`. No I/O beyond config files. soldr-cli re-exports all of it at the old paths (`soldr_cli::core`, `soldr_cli::defender`, …), so consumers are unchanged.
- **`crates/soldr-fetch`** — Binary resolution (re-exported as `soldr_cli::fetch`; `build.rs` + `embed/` live with it since `OUT_DIR` is per-crate). Ships several sub-modules:
  - `known_tools` — registry of ecosystem tools with explicit GitHub `(owner, repo)`, cargo subcommand mapping, and optional monorepo tag prefix (e.g. `cargo-audit/v0.21.0`). Keeps dispatch off the crates.io round-trip and handles per-tool release quirks.
  - `trust` — SHA-256 computation + `SOLDR_TRUST_MODE` / `SOLDR_CHECKSUMS_FILE` enforcement. Every fetch emits a `trust: verified` or `trust: unverified` line and a pin mismatch is a hard error regardless of mode.
  - `rustup_init` — rustup auto-bootstrap. zccache is a vendored Rust
    dependency hosted in-process by `soldr-daemon`, not a fetched tool.
  - Resolution chain: local cache → registry-or-crates.io repo lookup → GitHub Releases asset download → crate `[package.metadata.binstall]` pkg-url (exact versions, strict template vars, https only, trust-unverified unless pinned, `binstall` token; #3726) → QuickInstall (exact versions, trust-unverified unless pinned, `quickinstall` token of `SOLDR_RESOLVER_ORDER`; #3700) → extract.
- **`crates/soldr-cache`** — `RUSTC_WRAPPER` logic (re-exported as `soldr_cli::cache_lib`): hash inputs (blake3), check `~/.soldr/cache/`, daemon IPC (Unix socket / Windows named pipe), LRU eviction, plus the `soldr save` / `soldr load` archive transport and the auto-GC orchestrator. The `[cook]` eviction pass lives in `cache_lib::cook_gc` (issue #589) and is kicked from `gc/auto.rs` on the same throttle as the disk-pressure tiers.
- **`crates/soldr-daemon`** — daemon runtime (re-exported as `soldr_cli::daemon` / `soldr_cli::zccache_embedded`): lifecycle (spawn/displacement/relocation), IPC server, wire codec, running-process v2 broker adoption, and the embedded zccache service the daemon hosts. Depends on soldr-core + soldr-cache.
- **`crates/soldr-cli/src/soldr_main.rs` + sibling CLI modules** — Mode detection in `run()` (`crates/soldr-cli/src/main.rs` is a 3-line binary shim calling `soldr_cli::run()` since #1490 Phase 1), clap for built-ins, exec for tool fetch. The cargo front door (`soldr cargo ...`) inspects the first positional arg; if it matches a `known_tools` `cargo_subcommand`, the corresponding `cargo-<sub>` binary is fetched and prepended to `PATH` before cargo runs.

`crates/soldr-cli/src/lib.rs` declares every module exactly once (#1490 Phase 1 removed the historical lib/bin double-declaration, which compiled ~40K LOC twice per build). The CLI entry logic lives in `crates/soldr-cli/src/soldr_main.rs`, glob-re-exported at the crate root so `crate::<item>` paths inside the tree resolve unchanged; integration tests keep their `use soldr_cli::core::*`-style imports.

Dependency flow: every module reaches into `crate::core::*` for shared types (resolved through the soldr-core re-export); `fetch` and `cache_lib` each consume `core`; `daemon` consumes `core` + `cache_lib`; the cli-side modules consume all four.

Python package (`src/soldr/`) wraps the CLI binary via Maturin as `soldr._native`.

## Supported Tools

Two categories, surfaced as first-class subcommands or via the generic fetch path:

**Rustup toolchain passthroughs** (resolved via `rustup which`):
`rustc`, `rustfmt`, `clippy-driver`, `rustdoc`, `rust-gdb`, `rust-lldb`, `rust-analyzer`.

**Rustup front door** (top-level, direct exec of `rustup` itself — `rustup which rustup` doesn't work):
- `soldr rustup <args>` forwards to the system `rustup` binary. When the first non-flag positional is `target` or `component` and `rust-toolchain.toml` declares a `channel`, soldr injects `--toolchain <channel>` after the verb so per-toolchain state mutations land on the pinned toolchain. Pass `--toolchain` explicitly, or set a non-empty `RUSTUP_TOOLCHAIN` (which rustup ranks above `rust-toolchain.toml`), to opt out of injection.
- `soldr toolchain install` reads `[toolchain].channel` from `rust-toolchain.toml` and runs `rustup toolchain install <channel> --profile minimal --no-self-update`.
- `soldr toolchain prepare` chains install + `component add` + `target add` for every declared component / target, then acquires every entry under `[soldr.plugins]` (prebuilt `known_tools` binary first, `cargo install` fallback — see below).
- `soldr toolchain ensure [--json]` (issue #407 Phase 2) auto-bootstraps rustup if missing, runs the same pipeline as `prepare`, then smoke-verifies the resolved toolchain with `cargo --version` / `rustc --version`. `--json` emits a stable `schema_version: 1` payload consumed by `setup-soldr#133`.
- `soldr toolchain link --shim-dir <path> [--json] [--force]` (issue #407 Phase 3) writes PATH shim files for `cargo`, `rustfmt`, `clippy-driver`, `rustc`, and `rustdoc` that re-exec the running soldr binary. Idempotent (existing-matches → skip); `--force` overwrites differing content. JSON payload uses the same `schema_version: 1` shape as `ensure`.
- `soldr toolchain doctor [--json]` (issue #407 Phase 4) runs env-detection probes (musl-cc availability, pre-populated `target/` warning) and emits either a human summary or the same `schema_version: 1` JSON shape as `ensure`/`link`. Namespaced under `toolchain` to avoid colliding with the top-level `soldr doctor` system check.
  - Manifest example:
    ```toml
    [toolchain]
    channel = "1.98.1"

    [soldr.plugins]
    cargo-nextest = "0.9"
    cargo-zigbuild = { version = "0.18", locked = true }
    cargo-deny = "*"          # any version — `--version` is omitted
    ```
  - Pre-built first (soldr#3699): an entry whose crate is a `known_tools` cargo subcommand (`cargo-nextest`, `cargo-deny`, `cargo-zigbuild`, ...) is first fetched as a prebuilt release through soldr-fetch (install lock, trust sha, staged promotion) into soldr's tool cache, where `soldr cargo <sub>` runs it. The version requirement is always honoured: the prebuilt is used only when the version the cargo front door would run (the registry pin, else the latest release) satisfies it. A requirement the prebuilt cannot satisfy, `features` / `no_default_features`, an unregistered crate, or a failed fetch falls back to `cargo install` with the declared requirement; a different version is never silently installed. A prebuilt-served plugin is not placed in `$CARGO_HOME/bin`, so invoke it as `soldr cargo <sub>`.
  - For the `cargo install` fallback, `prepare` invokes the cargo binary resolved via `resolve_toolchain_binary("cargo")` directly (NOT through the rustc wrapper) so installs land in soldr-managed `$CARGO_HOME`. The active cargo already obeys `rust-toolchain.toml`, so no channel is threaded through.

**Ecosystem fetches** (registered in `known_tools`, pulled from GitHub Releases):
- cargo subcommands invoked via `soldr cargo <sub>`: `nextest`, `deny`, `audit`, `llvm-cov`, `udeps`, `semver-checks`, `expand`, `watch`, `chef`, `zigbuild`, `xwin`, `binstall`, `machete`.
- top-level tools invoked directly via `soldr <tool>`: `cross`, `mdbook`, `cbindgen`, `wasm-pack`, `trunk`, `sccache`, `bacon`, `just`, `typos`.
- `cargo-chef` powers the `soldr cook` content-addressable dep-prebuild (issue #359). It is pinned to v0.1.73 — the most recent release that still ships pre-built archives for Windows MSVC and macOS in addition to the Linux assets the newer releases publish.

Anything not registered falls through the generic External subcommand, which resolves via crates.io → GitHub Releases.

## Key Design Rules

- **Frozen built-in commands**: `status`, `clean`, `config`, `cache`, `version`, `help`, `rustup`, `toolchain`, `doctor`, `optimize`, `cook`, `archive`, `build-from-source`, `wheel` (soldr#2139 abi3 wheel surface), **`build` (soldr#1010 blessed surface — see "Two build paths" above)** plus the toolchain passthroughs listed above. These are clap-captured and must NOT be repurposed. Bare cargo built-in verbs — `test`, `check`, `run`, `bench`, `doc`, `fmt`, `clippy`, `tree`, `update`, `fix`, `add`, `remove`, `metadata`, `pkgid`, `search`, `vendor`, `yank`, `owner`, `login`, `logout`, `init`, `new`, `generate-lockfile`, `verify-project`, `locate-project`, `report`, `install`, `uninstall`, `publish`, `package`, `info` — route to `cargo <verb>` via the External arm (see `CARGO_BUILTIN_VERBS` in `crates/soldr-cli/src/cli_args.rs` and the phase-2 hop in `Commands::External` of `crates/soldr-cli/src/soldr_main.rs`). They are NOT soldr-native verbs and may not be reused as such; their soldr meaning is "shorthand for `cargo <verb>`." `build` is the **exception** — it has been promoted to a soldr-native surface and is no longer a pure cargo-builtin alias.
- **`ci-test` is also a frozen native verb** (soldr#2867): it is the prescribed host-validation DAG, not shorthand for `test` and not an external tool. It maximizes sharing by compile domain: stable host Clippy subsumes `soldr cargo check`; after Clippy, Nextest test-profile compilation completes before Fresh Nextest execution overlaps the serial Dylint libraries -> workspace-analysis -> UI-test branch; doctests join both branches and retain their rustdoc family; and all eight Dylints use separate target trees keyed by the one exact nightly declared by every lint manifest. Unset `CARGO_BUILD_JOBS` and `SOLDR_JOBS` remain unset, explicit values are preserved byte-for-byte, and only `NEXTEST_TEST_THREADS` is chosen when unset -- measured from CPUs and available memory, with a hysteresis pressure gate and a per-test memory ceiling in the Nextest wrapper (soldr#2885); an explicit value stays authoritative and only warns when memory cannot hold it. Some Nextest test bodies intentionally launch nested Cargo/compiler fixtures; those compiles and Dylint share the daemon's canonical shared/exclusive admission instead of causing a global Cargo cap. Cgroup memory observations remain telemetry and never impose a global Cargo job cap. Dylint is allowed its own toolchain, but it must never reuse or invalidate the stable target tree. `--explain-plan --format json` is the versioned machine-readable contract.
- **Trap unexpected Cargo calls during Nextest** (soldr#2878): prescribed host validation uses a two-name Cargo capability. Resolve the real rustup Cargo once and keep its absolute path in `SOLDR_REAL_CARGO`, which is private input to the source-built Soldr front door. Set `CARGO` to a separately named CI shim (for example `soldr-ci-cargo`) that re-enters that source binary as `soldr cargo`, and prepend a different fail-closed `cargo` shim to `PATH`. Code that honors `$CARGO` therefore stays inside Soldr; code that hard-codes bare `cargo` hits the trap and fails with a named diagnostic. Cargo itself overwrites `CARGO` with its own executable path before it launches test binaries, so the workflow-level override alone is insufficient: `soldr ci-test` must install `SOLDR_CI_TEST_CARGO_RUNNER` as the host target runner for the `nextest` execution stage only. That runner restores `CARGO` to the allowed shim immediately before execing each test process; do not apply it to `nextest-compile` or unrelated stages. The guard therefore has three required runtime layers: the allowed named shim, the bare-name `PATH` trap, and the Nextest target runner that repairs Cargo's environment rewrite. Pair those layers with a Ripgrep (`rg`) source scan for literal bare-Cargo process launches in test helpers, scripts, and workflow code; `rg` is the fast static check, while the runtime trap remains authoritative for aliases, variables, generated commands, and other dynamically constructed invocations. RED -> GREEN coverage must prove `$CARGO` succeeds through Soldr, bare `cargo` exits through the trap, a Cargo-like parent overwrite is repaired for the test process, and the runner is scoped only to Nextest execution. Intentional compiler fixtures launched by Nextest remain allowed through the named shim and use normal shared/exclusive compiler admission; never make the trap a reason to set `CARGO_BUILD_JOBS`, `SOLDR_JOBS`, or another global cap. This is not an OS sandbox: a child that already captured an absolute real-Cargo path can bypass PATH and `$CARGO`, so retain the Ripgrep/static spawn-path guard and do not claim complete process confinement.
- **Standalone native compiler verbs are frozen**: `cc` and `c++` are clap-captured catalogue compiler surfaces (soldr#2335), and `ld` / `link` are the standalone target linkers (soldr#3806: `ld` speaks GNU ld syntax for ELF/Mach-O/win-gnu, `link` speaks MSVC link.exe syntax for `*-windows-msvc`). None of them may fall through to external-tool fetching.
- **PyO3 extensions are `abi3-py310`, always**: every PyO3 extension soldr builds or ships — soldr's own, and every downstream repo in the fleet (fbuild, zccache, running-process, bosn) — declares `pyo3 = { features = ["abi3-py310"] }`. Not `abi3-py311`/`py312` (narrows compatibility for nothing), not version-specific `cp3xx-cp3xx` wheels (one wheel per interpreter, and a cross build then needs a CPython built for the target). `abi3-py310` is the floor soldr supports directly: `soldr wheel --target <triple>` plans it as `Abi3NoPython` (`PYO3_NO_PYTHON=1`, no target-side interpreter), one `cp310-abi3-<platform>` wheel covers CPython 3.10+, and it matches soldr's own `Python >=3.10` requirement. When advising a downstream repo, or when the `soldr wheel` abi3 refusal fires, the answer is `abi3-py310` — never pick the ABI floor from that repo's `requires-python`. See docs/API.md "PyO3 ABI policy".
- **MSVC on Windows always**: Default to `x86_64-pc-windows-msvc` (or aarch64). Only use GNU if `rust-toolchain.toml` explicitly says so. Target resolved at runtime, not compile-time.
- **Pre-built first**: Try every binary source before `cargo install`. Resolution order matters.
- **The wrapper route stays inside Soldr**: If caching is enabled and no
  explicit `SOLDR_RUSTC_WRAPPER` override is set, `soldr cargo ...` installs a
  compiler-named Soldr shim in `RUSTC_WRAPPER`. Each compile re-enters Soldr
  and is sent over Soldr IPC to the zccache service embedded in
  `soldr-daemon`; Cargo never launches a standalone zccache wrapper/daemon on
  this path.
- **Soldr daemon auto-starts**: The first cacheable wrapper call starts
  `soldr-daemon`, which owns the embedded zccache service. No standalone
  zccache process is started and no manual `soldr start` is required.
- **Degenerate states fail loudly, never wait silently** (soldr#3815): a broken or unreachable daemon connection is an error condition and must never go unnoticed. Detect degenerate behaviour and alert the user -- a loud red error naming the condition and the recovery steps, exit 1 -- rather than silently self-correcting or absorbing it in a long timeout.
  - Every client wait on the daemon has a liveness deadline: 30 s (`CONNECTION_LIVENESS_DEADLINE`, `SOLDR_CONNECT_DEADLINE_SECS`) to connect or re-establish a lost connection. On expiry soldr prints a red error (`soldr daemon status`, `soldr daemon stop` + `start`, `soldr logs paths`) and exits 1. A live daemon actively compiling is not cut at 30 s.
  - One broker per socket path, enforced by identity: a broker records the file it bound (device, inode, change time), retires the path only while it is still that file, and an orphan whose path was rebound stands down without reaping (`broker_endpoint_owner.rs`). Orphaned brokers were reaping daemons the live broker's clients were using.
  - Never reap a busy daemon: the broker asks the daemon for its in-flight compile count before reaping a route whose requesters exited, and a failed query means no reap. Lost replies, orphaned in-flight compiles and reaped-while-busy daemons are bugs to detect and fix, not conditions to hide behind `SOLDR_COMPILE_REPLY_TIMEOUT_SECS`.
- **Recovery from a wedged cache**: `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=<n>` shortens the 30-min compile-reply backstop so the build fails quickly. Inspect `soldr doctor`, `soldr status`, and `soldr logs paths`; restart the broker-owned route with `soldr daemon stop` followed by `soldr daemon start`. Cacheable compiler work never silently bypasses the broker/daemon. See [docs/DAEMON_TIMEOUTS.md](docs/DAEMON_TIMEOUTS.md).
- **The broker is a stable singleton; Soldr never replaces it automatically** (soldr#2549): a package-version or image-digest mismatch between the running Soldr and the live broker is a loud front-door warning, never a lifecycle action — no stop, no kill, no staging over. The version+digest generation belongs to the *daemon*: the route service name is keyed on the daemon image hash, so the stable broker launches or adopts a matching daemon generation behind itself and the prior daemon drains under `displace_stale_daemon`. **One daemon owns a root at a time** (`root-owner.lock` is per root; route claims are per generation, soldr#3374): while a daemon of another generation is alive and busy, a second version's daemon cannot start and fails with an ownership error that names the holder (soldr#3456). The ways out are an isolated root (`SOLDR_CACHE_DIR=<scratch>`), stopping that daemon from the Soldr that started it, or the explicit `soldr broker remove`; Soldr never displaces a busy daemon on its own, since that would cut other sessions' in-flight compiles (soldr#3377). Operator recovery is the explicit `soldr broker remove`.
- **Embedded cache location is Soldr-owned**: By default the service receives
  `~/.soldr/cache/zccache/daemon-state/embedded-v1` as its top-level cache
  root and zccache versions persistent state beneath `v<VERSION>/`. Per-build
  history is stored under `~/.soldr/cache/zccache/history/`.
  `SOLDR_CACHE_DIR` moves that embedded compiler-store boundary.
  `ZCCACHE_CACHE_DIR` remains a front-door/session, rust-plan, and rustfmt
  compatibility override; it does not relocate the service hosted by
  `soldr-daemon`.
- **Parent-cache sharing is default-on**: For managed-zccache builds soldr seeds `ZCCACHE_PATH_REMAP=auto` on the child cargo (issue #352, Tier L1.x). zccache then normalizes absolute source paths inside compiled artifacts so two git worktrees of the same repo serve each other's cache hits via hardlinks. Escape hatch: `SOLDR_PATH_REMAP=off` suppresses the injection; setting `ZCCACHE_PATH_REMAP` yourself wins. Works for non-git checkouts too: since zccache#353, `ZCCACHE_PATH_REMAP=auto` with no `.git/` ancestor falls back to the cwd as the remap root and still injects `--remap-path-prefix=<cwd>=.`, so tarball/zip/git-archive checkouts produce path-independent artifacts and share hits (the `.git/` walk is only how the preferred worktree root is discovered).
- **Integrity is default**: every fetch records sha256. Pins are opt-in via `SOLDR_CHECKSUMS_FILE`; `SOLDR_TRUST_MODE=strict` refuses unpinned fetches.
- **Version independence, one pin**: Users install once and forget. The only soldr version pin is setup-soldr's own default (bumped by its ingest workflow on every release); every call site, bootstrap, Dockerfile, host install script and PEP 723 dependency takes that default or the latest release -- never `soldr==X.Y.Z`. A stale pin on a shared host downgrades every session's soldr, and an old soldr daemon can take over `~/.soldr` and lock newer daemons out (2026-10-16). `tests/test_setup_soldr_version_pins.py` guards the call sites.
- **Local zccache development**: Soldr consumes an exact released zccache
  crate. Test unreleased work upstream, publish it, then update Soldr's pin.
  `SOLDR_ZCCACHE_LOCAL_DIR` and `SOLDR_ZCCACHE_BIN` are legacy
  compatibility names and do not replace the embedded service on the normal
  path. To deliberately test an external compiler wrapper, set
  `SOLDR_RUSTC_WRAPPER=/path/to/zccache` (or another wrapper) explicitly.
- **The Dylint nightly is declared by the lint libraries, never derived** (soldr#2945): every `dylints/*/rust-toolchain.toml` names the channel, they must all agree, and that channel is the only one for which a `dylint-driver` is published. Do not infer it from the project's stable pin — lint libraries link `rustc-dev` and track the compiler API, so the right nightly is routinely several minor versions ahead of stable (today: stable `1.98.1`, lints `nightly-2026-05-28`). soldr used to derive it from stable and then demand a driver for the derived value, which made `soldr dylint` fail on **every** host while CI stayed green — because CI runs `soldr ci-test`, which read the manifests, and nothing in CI ran `soldr dylint`. **A green CI lane only proves the verb CI runs.** When a tool has more than one entry point, check that they resolve their inputs through one implementation, or expect them to drift.
- **One canonical toolchain-home pair per execution, chosen by where the binary lives** (soldr#1799/#1768): soldr keeps private managed `RUSTUP_HOME`/`CARGO_HOME` for dylint's nightly, and they are applied **only** when the resolved binary physically lives inside those managed homes (`binaries::apply_resolved_toolchain_homes`). A host-resolved `cargo`/`rustc`/`rustfmt`/`clippy` always executes under the caller's own homes — never by ambient env leakage. This matters because the failure is silent: flipping homes between runs changes which rustc is used, which invalidates cargo's fingerprints and zccache's keys, so a warm build recompiles the world and is merely 10-50x slower, indefinitely. Every build log records `home_origin` (`caller` | `managed` | `repo-local`) beside the resolved `binary`, and `.github/scripts/check_toolchain_homes.py` fails CI when a row claims `managed` for a binary outside a managed root.
- **All Rust toolchain commands go through soldr**: `cargo`, `rustup`, `rustc`, `rustfmt`, `clippy-driver`, `cargo-clippy`, `cargo-fmt`, `rustdoc`, `rust-gdb`, `rust-lldb`, and `rust-analyzer` must be invoked as `soldr <tool> ...` (or `uv run soldr <tool> ...`). This includes invocations with leading env-var assignments — `RUSTUP_TOOLCHAIN=... cargo build` is the same policy violation as `cargo build`. clud enforces this in agent shell tools (see Dogfooding below — the in-repo hook is no longer the enforcement point); the helper script `bench/build_local_zccache.sh` and any documented workflow must follow the same rule. Env-vars prefixed before `soldr` are fine — the policy is about routing the tool, not forbidding env overrides.

## Concurrency caps are a last resort — reach for exclusive access first (soldr#3138)

**When the answer looks like "set jobs/threads/concurrency to 1", the question to
ask instead is: which specific unit of work needs exclusive access to the build
system, and how do we grant it that?**

A global cap is the wrong shape of fix almost every time. `CARGO_BUILD_JOBS`,
`SOLDR_JOBS`, and `[jobs] max_parallel_compiles` all size the *same shared
ceiling* — Cargo's producer queue and the embedded zccache admission gate both
resolve through `soldr-core/src/core/jobs.rs`. Setting it to 1 to protect one
enormous compilation unit also serializes every small one, so a lane pays the
cost across its whole graph to fix a problem that lives in a single unit. It is
a whole-system setting used to express a per-unit property.

Exclusive admission expresses the actual constraint. The embedded service can
grant one compiler child sole access while everything else keeps running N-wide:
soldr's classifier is `SoldrHostAdmissionClassifier` in
`crates/soldr-daemon/src/amalgamation.rs`, combined with zccache's own built-in
C/C++ and Rust predicates before zccache takes its capacity semaphore and fair
resource lock. That is strictly better than a cap — same protection for the heavy
unit, no penalty for the rest.

### The ladder

Work down it. Stop at the first rung that actually fits the problem.

1. **Make the unit itself cheaper.** `codegen-units = 1`, `debug = false`,
   `lto = false`, a smaller profile. See `[profile.*.package.zccache]`.
2. **Size the unit's actual resource need and let admission schedule it.**
   Exclusive access is the blunt form of this (`SoldrHostAdmissionClassifier`).
   Since soldr#3152, admission also spends each unit's **measured** memory
   history (`crates/soldr-daemon/src/history_admission.rs`): a unit with at
   least two recorded measurements becomes exclusive when 1.5x its remembered
   tree peak exceeds half the memory available right now. The history can only
   add exclusivity. A fresh root (every CI run) has no history, so the name
   lists remain the cold-start fallback there. Prefer a measured predicate over
   a name either way (see below).
3. **Give the machine headroom.** `.github/scripts/setup_ci_swap.sh` — 14 GB of
   swap for <1 s. Note that soldr#2453's own workflow comment records which rung
   did the work: "Bounding CARGO_BUILD_JOBS/SOLDR_JOBS narrowed but did not close
   the signal-kill; the swap headroom is the safety valve that does."
4. **Cap concurrency.** Only when the rungs above genuinely cannot apply — and
   then say *why* they cannot, in a comment, next to the cap.

The one honest reason to land on rung 4 is that **exclusive admission requires
the wrapper**. A step running `RUSTC_WRAPPER=""`, `ZCCACHE_DISABLE=1`, or
`--no-cache` has no embedded service, so there is no gate to ask. Those steps
keep their cap and should carry it at *step* scope, never job scope — a
`GITHUB_ENV` write or a job-level `env:` block applies to every subsequent step
and silently serializes unrelated work.

### Recognise the shape, not the name

`amalgamation.rs` already states the right principle for C/C++, and its reasoning
generalises:

> Deliberately measures the file rather than trusting the name: the point is to
> recognise the *shape* of the work, and a private amalgamation nobody added to
> `KNOWN_AMALGAMATIONS` is exactly the case a name table misses.

A size threshold catches the next amalgamation automatically; a name list catches
only the ones someone remembered. Where a name list is unavoidable, treat every
addition to it as evidence the predicate is wrong, and expect the list to drift
silently — a missing entry produces no error, just a slow build or an OOM whose
cause is three layers away.

### Diagnosing before capping

An OOM is a scheduling defect, not a signal to lower global parallelism —
`crates/soldr-cli/src/cli_args.rs` already says so on the `--jobs` surface:

> An OOM is a scheduling/admission defect to diagnose from compiler and cgroup
> telemetry, not a signal for Soldr to silently lower Cargo's global parallelism.

Two traps when gathering that telemetry:

- **cgroup OOM counters are not sufficient.** `amalgamation.rs` records that the
  kills which motivated `SOLDR_HEAVY_TEST_LINKS` — `soldr_cli --test` dying twice
  above 5 GiB — did **not** increment them. Measure **peak RSS per compiler
  child**.
- **The diagnostics exist but are not visible.** The daemon logs its resolved
  limit (`compile_limit.rs` `resolve_and_announce`) and each exclusive-access
  request (`amalgamation.rs`) to its own stderr, which is redirected to the daemon
  log file and never reaches the CI job log. Absence of those lines from a job log
  is not evidence they did not happen. `soldr status` reports the applied limit
  and compares it to what the CLI would resolve now (soldr#2023).

### Current rung-4 exceptions, and why

| Site | Why a cap, not exclusive access |
|---|---|
| `_ci-cross-build-linux.yml` bootstrap steps | `RUSTC_WRAPPER: ""` (one also `ZCCACHE_DISABLE=1`). No service, no gate. Step-scoped. |
| `ci.yml` `wheel-cross-verify` | Bootstrap-shaped build (soldr#2469). |
| `cook-size-gate.yml` cook step | `ZCCACHE_DISABLE=1`: no admission gate. Step-scoped. |
| `setup-soldr-action.yml` "Test through soldr" | `--no-cache`: no admission gate. Step-scoped. |
| `macos-universal2-probe.yml` `build-slice` | Job-scoped, `workflow_dispatch` only; its job comment records the soldr-daemon `codegen-units=1` kill (soldr#2781). `tests/test_compile_caps_step_scoped.py` lists it as a named job-scope exception. |
| `auto-release.yml` build steps | Swap is in place (soldr#3148 step 1), so rung 3 exists. Since soldr#2469 3.1 a `dry_run=true` dispatch builds and smokes the full production matrix on an exact candidate and publishes nothing, so a lift can be canaried without a release; until one has been, the cap stays. A lift is verified from the job log's `soldr: daemon compile concurrency =` line (soldr#3206). |

Every entry names a reason. A cap with no reason beside it is a bug report
waiting to be written.

### Why "exclusive access" is the floor, not the ceiling

Exclusivity is still a whole-machine answer to a per-unit question — it just
scopes the blast radius to one compile instead of a whole lane. Applied too
widely it re-creates the problem it solves: granting every first-party `--test`
link exclusive access would serialize dozens of trivial test binaries and cost
more wall-clock than the OOMs it prevents. The real factors are continuous, not
categorical — LTO mode (`fat` holds the whole graph at once, `thin` is chunked,
absent is flattest), object count, total object bytes, and largest single object
all move peak memory independently. Measured over a real journal, `--extern`
counts are bimodal (42-43 for genuine test links versus 0-4 for trivial ones) and
rlib sizes span 65x between median and max, so the information needed to
discriminate is present on the command line and merely unused.

So the ladder's rung 2 has a floor and a ceiling: exclusivity, and memory-aware
scheduling from measurement (soldr#3152). The measurement now exists. zccache
1.13.24 journals `child_peak_rss_bytes` (the compiler alone) and
`tree_peak_rss_bytes` (the compiler plus its live descendants, so a linking
`rustc`'s `ld` counts) and returns both on each compile's response. soldr keeps
the last trusted peak per unit (`<crate name>/<cargo -C metadata>`) in
`state.sqlite3`. `.github/scripts/fit_memory_estimate.py` scores it from the
`build-logs-*` artifacts.

What soldr#3152 measured, so nobody re-derives it:

- A command-line-feature estimate does not work: R² 0.09 on `--extern` bytes
  across 1,161 CI units. The heaviest units are large first-party source trees
  the command line cannot see.
- A unit's own measured peak is stable. Every unit ≥ 256 MiB was within 1.35x of
  its remembered peak.
- Short compiles report a spawn-instant sample (tens of KiB). The history ignores
  readings under 8 MiB and trusts a unit only after two measurements.
- CI roots are fresh, so CI never reaches a trusted history. That is why the name
  lists stay as the cold-start fallback rather than being retired.

## Agent Development Environment Rule (issue #1105)

**For all changes, develop and debug in the local Docker Linux scripts.** Cross-compile to Windows/macOS later, do not let the cross-compile story block feature work.

- **Develop features under Linux first.** The fastest, most reproducible inner loop is `docker build` + `docker run` against the harnesses already in this repo: `examples/docker-cross-win/`, `ci/docker-aarch64-windows-msvc-cross/`, `ci/docker-darwin-cross/`, `ci/docker-aarch64-musl-cross/`, and `docker/cook-shared-cache/`. Use them for the feature build, the unit-test pass, and the smoke run before you ever need a Windows host.
- **Land the Linux-side behavior first, then handle the cross-compile.** Native Windows or macOS only behaviors (vswhere, link.exe, etc.) get test seams (env-var overrides, fixture-driven probes) so the pure logic is exercised under Linux. The host-specific end-to-end gates stay `cfg(target_os = "...")` integration tests and run on the per-PR matrix lanes — they do not block local iteration.
- **Default debugging flow.** For ordinary Linux builds, use `uv run --no-project python ci/perf_local.py cargo <args>` so every agent reuses the named `soldr-perf-local` runner and its persistent target/Cargo/soldr volumes. The runner mounts the shared checkout root, so repository worktrees use `docker exec -w` instead of creating per-agent containers. Use the target-specific harness scripts only for their cross-compile environments; do not create another general-purpose soldr development container. `ci/perf_local.py --stop` and `--reset-runner` preserve volumes, while `--wipe` is the explicit destructive reset. Reach for these Docker harnesses before spinning up a Windows VM or asking the user to run things on a Windows host.
- **When the cross-compile genuinely needs Windows tooling** (live MSVC SDK probe, registry reads, real `link.exe` resolution) gate that path with a runtime opt-out (`SOLDR_*_DISCOVERY=off`-style env vars) and a Linux-friendly fixture path so the same module still compiles + unit-tests on the Linux harness.

This rule was set during the #1105 fix: the `rust-lld` LIB-injection feature was developed and tested entirely under `examples/docker-cross-win/` before the Windows-only end-to-end gate was added.

## Working Location Rule

**Sister checkouts are allowed** (owner directive 2026-10-01, zackees/clud#1696;
supersedes the 2026-08-10 "no sibling clones, no worktrees" rule). Work in this
checkout, or in a git worktree under `../soldr-extern/<name>` when you need
isolation from another agent's work or a clean tree (`git worktree add
../soldr-extern/<name> -b <branch> origin/main`). Remove the worktree when the
branch is merged.

- **Know which runner sees which tree.** `ci/perf_local.py` mounts the
  repository's shared git root read-only at `/repo`; a worktree *below* that
  root is reachable with `docker exec -w`, a sister worktree under
  `../soldr-extern` is not -- use `bosn run --task <t>` from the worktree
  instead (bosn mounts the workspace it is run from).
- **Never share one worktree between agents**, and never edit a checkout or
  worktree another agent owns.

## Agent Completion Rules

- **Finish on a branch**: When an agent completes a task, it must put the work on a dedicated branch rather than leaving it only in the local worktree.
- **Push the branch**: The branch must be pushed to `origin` before the task is considered complete.
- **Open a pull request**: The agent must create a PR for the branch when permissions allow.
- **Always report the merge URL**: The final user-facing summary must include the PR URL the user should open to review and merge the work.
- **Fallback if PR creation is blocked**: If the GitHub integration cannot open the PR directly, the agent must still push the branch and provide the exact GitHub URL the user needs to open or complete the PR manually.

## Local Gate Rule (zackees/ci.yml#166, #168)

**Every PR head must pass the local gate before it is pushed.** CI's first job
(`ci-mode`) fails a PR whose head commit lacks a tree-bound `Local-Gate:`
trailer, and every other job needs `ci-mode`.

**Attested PRs skip the remote quick gate (zackees/ci.yml#190, GATE-008).**
When the head is attested and in policy, `ci-mode` outputs `trusted=true`
and the remote `Lint` and `Linux x64` jobs skip, because they would rerun
exactly what the gate just ran. The policy is `[gate.trust]` in
`local-gate.toml`, read from the PR's base. These still run remotely:
- a change to `local-gate.toml`, `ci/local_gate.py`, `ci.yml`,
  `_build-and-test.yml`, the actions they use, or a declared surface script;
- a `ci-full` label, a fork, or an author without write access;
- the 1-in-10 audit sample.

The `tests` lane attests the tests the change can reach (soldr#3731): for a
crate-source-only diff it runs the changed crates' reverse-dependency closure
plus `soldr-cli`, logged as `tests lane: SELECTED [...]`; any root, lockfile,
toolchain, test-infra, unknown path, dirty tree or git error runs the full
workspace. See [CI modes](docs/CI_MODES.md) "Local gate: proven lane skips".

Every push to `main` runs the jobs for real, and a release candidate runs
full CI on its exact SHA. A remote failure on an attested head is a gate
gap: fix the gate, don't just rerun.

```bash
git commit ...                       # the gate attests a committed, clean tree
uvx --from git+https://github.com/zackees/ci.yml@<CI_LINT_REF> ci-lint local-gate run
git push                             # (--force-with-lease if the branch was already pushed)
```

`<CI_LINT_REF>` is the SHA in `ci.toml`'s `linter` -- the one canonical pin (soldr#3616); `ci/local_gate.py` derives `CI_LINT_REF` from it and workflow `ref:` lines are guarded equal. `local-gate run` runs
`ci/local_gate.py` (lanes `lint` = the remote Lint job exactly, `rust` =
`soldr lint rust`, `tests` = the test suite in bosn), and on success amends
HEAD's message with the trailer. Install the pre-push hook once per clone with
`... ci-lint local-gate install-hook`.

- **The `wine` lane may attest by proof (soldr#3704).** When every changed path
  (vs `merge-base origin/main`) lies in a workspace crate outside the path-dependency
  closure of `ci/wine_lane.py` `CRATES`, the lane prints `wine lane: SKIPPED (soldr#3704)`
  and passes: the Wine-tested binaries are unchanged from the base. Any root file,
  lockfile, toolchain, script, unknown path or unreadable diff runs it in full. `cross`
  is not skipped this way: a cfg-free edit can still call a cfg-gated API.

- **Add Lint checks to `ci/local_gate.py`, never as a step in ci.yml's Lint
  job.** The Lint job runs `ci/local_gate.py --lane lint` and nothing else;
  `ci-lint local-gate lint` (a check in that lane) fails anything else.
- **Never run soldr's test suite on the host.** It starts soldr daemons and
  touches soldr state roots. A leaked fixture once claimed the real
  `~/.soldr` root and wedged every soldr build on the machine (soldr#3516).
  The nextest run-wrapper refuses unless `CI=true` or `SOLDR_TEST_ISOLATED=1`,
  which only the bosn image sets. Run the suite with
  `bosn run --task test` (the gate's `tests` lane). Do not set the marker on
  a host to get around the refusal.

## Agent Code-Smell Reporting Rule (issue #2741)

**Significant code smells get an issue, not a shrug.** When you find code where
the *same idea has several conflicting implementations*, or where the intended
behaviour cannot be determined from the code, file an issue — even when it is
outside the task you were given.

- **Title states the research request**, not just the observation:
  `research: N conflicting implementations of X — which is canonical?`
- **Body carries the evidence**: every implementation with `file:line`, how they
  differ (a table once there are more than two), and what the difference costs
  in observable behaviour.
- **Do not fix it inline as a side quest, and do not silently pick one.** File,
  cross-reference from the issue you were actually working, and continue.

### The bar — all three, or it is not an issue

1. **Multiplicity or ambiguity** — the same concept implemented more than once
   with differing behaviour, or behaviour you cannot determine without running it.
2. **Observable consequence** — the divergence changes what the program does for
   some real input.
3. **Not locally fixable** — resolving it needs a decision about which behaviour
   is correct, i.e. a design question rather than a typo.

Duplication that behaves identically is a refactor, not an issue. One ugly
function is not an issue. A `TODO` is not an issue. This rule dies the moment it
becomes noise.

### Triggers worth noticing while reading

Each of these was present in soldr#2740, the worked example, and each is cheap
to spot:

- N implementations of the same predicate or parser in different modules.
- A test that **re-implements** the logic it is testing instead of calling it —
  it validates a copy and cannot catch drift.
- A comment describing behaviour the code does not implement.
- Two names for the same concept with different semantics.

### Why this rule exists

soldr#2740 found five hand-rolled environment-variable truthy parsers plus two
inline spellings, mutually disagreeing. `SOLDR_USE_SYSTEM_CMAKE=false` *enabled*
a switch that routes around the pinned, sha256-verified SDK; `ZCCACHE_DISABLE=off`
*disabled* the cache. Every parser looked correct in isolation — the defect
existed only **between** them, so agents read adjacent code for months and moved
on. It surfaced only because a user asked a follow-up question that forced a
comparison. That is too fragile a mechanism for finding this class.

## Release Publishing Rules

- **Release PRs must bump the package version**: a release starts from a PR merged to `main` that bumps `[workspace.package].version` in `Cargo.toml` and the matching `"version"` in `package.json` to a version that is not already published. Merging it does **not** release anything by itself.
- **Releases are two explicit dispatches on one exact SHA** (soldr#3346): dispatch `CI` with `candidate_sha=<merged SHA>` and wait for `Full coverage` to succeed, then dispatch `Autonomous Release` (`auto-release.yml`) with the same `candidate_sha` and `full_ci_run_id=<that run>`. A dispatch whose version is already fully published sets `should_release=false` and skips every job. Re-dispatching the same SHA is the recovery path; completed surfaces are skipped. See [RELEASE.md](RELEASE.md).
- **Tags are release outputs, not normal agent inputs**: The release workflow derives `vX.Y.Z` from `Cargo.toml` and creates the matching GitHub tag and release when the tag does not already exist. Do not manually create or push `vX.Y.Z` tags unless the owner explicitly asks for a recovery operation.
- **Check release state before claiming a release is ready**: Verify `Cargo.toml`, `package.json`, PyPI `soldr`, npm `@zackees/soldr`, and `git ls-remote --tags origin vX.Y.Z`. If the candidate version or tag already exists, either bump to the next patch version or stop and report the conflict.

## Toolchain

- Rust 1.98.1 (rust-toolchain.toml), edition 2021, MSRV 1.98
  (`[workspace.package].rust-version`). The MSRV and the pinned toolchain are
  the same compiler train — soldr does not support building on an older
  compiler, so "will this still build on the MSRV?" is never a reason to avoid
  a newer std API. Guarded by
  `crates/soldr-cli/tests/guards/msrv_doc_matches_manifest.rs`.
- **The MSRV names a train, never a patch release** (soldr#3305). `soldr
  ci-test` compiles this workspace with *two* compilers: the stable pin, and
  the dated nightly that `dylints/*/rust-toolchain.toml` declare. A dated
  nightly reports the `.0` release of its train (`nightly-2026-05-28` is
  `1.98.0-nightly`), so `rust-version = "1.98.1"` makes Cargo refuse to build
  under it — `error: rustc 1.98.0-nightly is not supported`. Two components
  (`"1.98"`) mean `>=1.98.0`, which both compilers satisfy. Nothing local
  catches a patch-level MSRV: the nightly's rustc version is recorded nowhere
  in-tree, and `cargo build`, clippy and the whole test suite all run under the
  stable pin, so it fails only in the Linux `ci-test` lane.
- Python >=3.10 (for PyPI distribution via Maturin)
- uv for Python dependency management
- Workspace dependencies shared in root `Cargo.toml`

## Bumping soldr's own version (release PRs)

When opening a release PR, **three files must be bumped in lockstep** or every CI lane fails immediately with `error: cannot update the lock file because was passed to prevent this` (the v0.7.65 trap from #1024 / #1025):

1. `Cargo.toml` — `[workspace.package].version`.
2. `package.json` — top-level `"version"`.
3. `Cargo.lock` — the `soldr-cli` package's `version` field. Refresh with a no-op build (`soldr cargo build -p soldr-cli`) AFTER bumping `Cargo.toml`, then `git add Cargo.lock` so the new version is committed alongside the other two.

The regression guard at `crates/soldr-cli/tests/guards/version_lockstep.rs` reads all three files and asserts they match — `soldr cargo test -p soldr-cli --test guards version_lockstep::` (or any full `cargo test` run) fails the build if any of the three drifts. The same test runs on every PR via the `Lint` job.

A pre-merge `cargo metadata --frozen` would also catch the trap (it refuses to run when the lockfile is stale relative to manifests) but adds a separate workflow. The in-tree test covers the same ground with no CI plumbing.

### zccache is embedded (no managed-version pin)

soldr#1368 removed the externally-downloaded managed zccache binary: the
zccache CLI ships compiled into Soldr from the exact released crate version in
the lockfile. soldr#2765 retired the zccache and running-process submodules.

Release staging follows the same rule: archives contain Soldr's embedded
zccache implementation and never download or bundle standalone `zccache`,
`zccache-daemon`, or `zccache-fp` binaries.

## Published zccache amalgamation

zccache publishes its internal `publish = false` workspace as one amalgamated
`zccache` crate. Soldr consumes that crate through an exact crates.io pin; it
does not carry a zccache submodule or path override. Since zccache 1.13.11, the
embedded service grants known Rust amalgamation crates and large C/C++ units
exclusive compiler admission, so a setup-soldr 0.9.6+ bootstrap can build a
fresh Soldr checkout without parallel amalgamation compiles exhausting a GHA
worker.

Do not move the pin ahead of a published zccache crate.
`crates/soldr-cli/tests/guards/version_lockstep.rs` enforces manifest/lock agreement
and the single-codegen profile for the amalgamated crate.

## Updating the external zccache and running-process pins

Update every exact dependency requirement together and refresh the lockfile.
Release archives use the embedded API and must not introduce a standalone
zccache asset dependency.

## Reference Docs

- **`PERF.md` — Performance testing. Read this BEFORE running any perf work. See callout at the top of this file.**
- `DESIGN.md` — Authoritative implementation guide, architecture decisions, phase roadmap
- `docs/API.md` — Full CLI specification, environment variables, cache layout
- `docs/CONTRIBUTING_TESTS.md` — portable and native platform test conventions, including target-run archive coverage
- `docs/CROSS_COMPILE.md` — blessed cross-compile recipes, including managed Windows GNU and MSVC `cargo-xwin`
- `docs/DEBUG_SIDECARS.md` — debug-symbol sidecar policy for release archives (`.pdb` / `.dSYM` / `.dwp`, `manifest.json` `debug_info` contract)
- `docs/TRUST_BOUNDARIES.md` — Runtime fetch policy, what integrity is enforced, what remains follow-up
- `docs/DAEMON_TIMEOUTS.md` — Daemon timeout & stall runbook: failure mode → signal → bounded broker-owned recovery, plus `soldr doctor`/`soldr status` diagnostics
- `README.md` — User-facing motivation and prior art comparison

## GitHub Actions workflow conventions

- **Complex CI logic inline in workflow YAML is BANNED. If the logic can be moved to a `ci/*.py` (or `.github/scripts/*.py`) file, it must be.** GitHub Actions YAML is notoriously hard to test — the only way to exercise an inline `run:` block is to push a branch and watch a runner — while a Python file is unit-testable under `tests/`, smoke-runnable from a developer's shell, and reviewable as code. Workflow YAML stays orchestration-only: matrix definitions, `needs:` edges, env plumbing, artifact upload/download, and one-line invocations like `python3 ci/<name>.py ...` or `python3 .github/scripts/<name>.py ...`. Prefer `ci/*.py` for logic that is also useful outside Actions (local dev loops, e.g. `ci/perf_local.py`) and `.github/scripts/*.py` for workflow-only helpers; prefer extending an existing script over adding a new one, and never grow an inline bash/python block instead.
- The historical motivation: `cross-compile-all-targets.yml` used to inline curl + jq chains for every release-asset lookup, which (a) couples the workflow tightly to GitHub Actions' shell wrapper, (b) is hard to unit-test, (c) duplicates parsing logic between lanes, and (d) makes the YAML unreadable. Extracted examples: `build_manifest.py`, `tool_query.py`, `print_build_banner.sh`, `ts_step.py`, `run_with_ts.sh`. The scripts have docstrings and take CLI args, so debugging doesn't require pushing a branch. Existing workflows still carrying large inline blocks (e.g. `auto-release.yml`) are grandfathered but must shrink, not grow: any change touching an inline block should extract it rather than extend it.
- **Skipping CI never skips cheap validation.** An integrator may omit expensive `ci-test`, target-run, or performance lanes, but must still run `./lint` and the Lint workflow's static tripwires (including `.github/scripts/platform_cfg_boundary_ratchet.py`).
- **`soldr-platform` has two overlapping boundaries, and `ipc/` is the only directory that clears both.** The cfg ratchet (`ban_platform_cfg_outside_boundary`) rejects host selection anywhere outside `platform_{win,linux,macos}/`, and the Dylint `ban_raw_ipc_transport` rule independently rejects raw socket/pipe constructors outside `platform_{win,linux,macos}/ipc/`. The second is strictly narrower and is not discoverable from the first: a `UnixStream::connect` probe under `platform/*/fs/` satisfies the cfg ratchet and then fails the Dylint lane, so the rules must be cleared together. When platform-specific code touches IPC — a liveness probe, a bind, a connect — home it under `ipc/`, not the nearest-looking `fs/` module. Both boundaries are real; neither implies the other.
- **`uvx --with pyyaml` is required to run `ci-lint` locally.** Without PyYAML on the path, `local-gate lint` emits spurious `GATE-001`/`GATE-002` "cannot parse ci.yml" findings and reports violations that do not exist. It reads as a real config failure and is not one.

## Per-file line ceiling (soldr#1966)

`.github/scripts/loc_ratchet.py` runs in the `Lint` job on every PR and enforces
the 1,500-line ceiling as a **ratchet**, not a threshold:

- a file at or under the ceiling must stay at or under it;
- a file already over it may not get **bigger**;
- shrinking, and deleting (i.e. splitting), are always allowed;
- Rust `mod.rs` files are exempt because they are module aggregation surfaces,
  not standalone implementation units.

There is no grandfather list to maintain — the baseline is the file's size at
the merge base. Thirteen files are already over the ceiling, and blocking every
PR that touches them would have been worse than the drift, so the rule is
"don't make it worse" rather than "fix it before you may proceed".

If a PR fails this check, the addition belongs in a new module. The split the
check is asking for is a change that was already overdue — but note that
splitting a popular file is a **rename**, and renames conflict destructively
with in-flight branches (soldr#1962 became a modify/delete conflict where
taking the delete compiled, passed CI, and silently dropped the fix). Check
`git branch -a --contains` for live work before splitting a hot file.

## Dogfooding

The repo builds itself through soldr so every contributor populates and hits the same cache.

- `./test` routes every `cargo` step through `soldr cargo` when `soldr` is on `PATH`. On a fresh checkout without soldr the script prints a one-line warning to stderr and falls back to bare `cargo` (no caching).
- **Command policy is enforced by clud, not by an in-repo hook.** `.claude/settings.json` and `.codex/hooks.json` are both intentionally empty (`{}`); soldr#1634 delegated enforcement to clud's repo-scoped `.clud/settings.json`. `.codex/README.md` records the same arrangement. What is enforced is unchanged — bare `cargo`, `rustc`, `rustfmt`, `clippy-driver`, `cargo-clippy`, `cargo-fmt`, `python`, `python3`, `pip`, `pip3` are denied in agent shell tools; route through `soldr cargo ...` / `uv run ...` / `uv pip ...`.
- `.claude/hooks/tool_guard.py` is the **previous** implementation of that policy, kept in-tree and still unit-tested, but no longer wired to anything. Do not rely on it as the enforcement point, and do not assume a bare `cargo` is mechanically blocked by this repository alone.
- Unit tests for the hook live next to it: `uv run --no-project --directory .claude/hooks python -m unittest test_tool_guard`. The `--directory` flag puts `tool_guard.py` on `sys.path` so the sibling import resolves.

## Serialization (issues #580 + #603)

- **Binary transports and persisted-state metadata MUST use Protocol Buffers** (via `prost`), not `bincode` / `rmp-serde` / other schema-less formats. Each schema is paired with the module carrying its hand-written prost types, and the pairs are exactly `PAIRS` in `.github/scripts/check_proto_drift.py`: the daemon wire schema `crates/soldr-core/src/core/wire.proto` with `crates/soldr-core/src/core/wire_proto.rs` (conversions in `crates/soldr-daemon/src/daemon/wire.rs`), and the `soldr save` / `soldr load` archive manifest `crates/soldr-cache/src/cache_lib/manifest.proto` with `crates/soldr-cache/src/cache_lib/save.rs`. (The former rust-plan schema was removed with the target cache in soldr#2996.) The schema file is the source of truth; `.github/scripts/check_proto_drift.py` enforces that by comparing every (message, field, tag) triple in each `.proto` against the `#[prost(…)]` attributes on its Rust type, and runs in the `Lint` job.

  soldr runs no `prost-build`, so the Rust types are hand-written rather than generated. Until soldr#2753 nothing read the `.proto` files at all — the round-trip tests encode a Rust type and decode it back into the *same* Rust type, which catches prost attribute self-consistency but cannot detect divergence from a schema it never parses. Three drifts had accumulated, including `wire.proto` referencing a message it never defined (i.e. `protoc` would have rejected it). If a schema and its Rust type disagree, fix whichever is wrong; the checker's `CROSS_REPO_MESSAGES` allowlist is only for types implemented in a vendored repo, and each entry must name the file where the type actually lives.
- **Daemon IPC** (`crates/soldr-daemon/src/daemon/{protocol,ipc,wire}.rs`) carries prost-encoded `WireRequest` / `WireResponse` in the frame body. The header is unchanged from prior versions; `PROTOCOL_VERSION` is bumped on every body-format change so peers at different versions error cleanly rather than silently mis-decoding.
- **Persistent state-store rows** (SQLite `~/.soldr/state.sqlite3`, `cache_lib::state_store` — the redb store was retired because its exclusive whole-file lock caused silent write drops and lock storms) are `[0x01][prost body]` BLOBs (the `0x01` tag is reserved for future format extensions). Reads enforce the tag and refuse anything else. A legacy `state.redb` beside the SQLite file is deleted whole on first open; cook artifacts on disk are unaffected and re-index on the next `soldr cook`.
- **`bincode` is no longer a workspace dep.** As of #603 there are zero bincode call sites in the crate. Re-introduction is blocked by clippy: workspace-root `clippy.toml` lists `bincode::serialize` / `bincode::deserialize` under `disallowed-methods`, and `[workspace.lints.clippy]` sets `disallowed_methods = "deny"` (issue #602). `cargo clippy --workspace -- -D warnings` fails on any new call site.
- **Human-edited config (`config.toml`, `rust-toolchain.toml`) stays JSON/TOML.** Protobuf mandate applies only to binary transports + archived metadata.

## Test Infrastructure

See `docs/CONTRIBUTING_TESTS.md` for the portable/native test boundary and how
platform behavioral tests reach the target-run lanes.

- **Linked test products are never cached, and test-target fan-out is a real cost (soldr#2931)**: test executables invalidate on every workspace source edit and each top-level file under `crates/*/tests/` links the full soldr graph into its own binary — ~110 of them produced a 3.3 GB nextest archive that exhausted a CI runner. Never wire a workflow, cache layer, or guard that persists test binaries (or benches/examples/doctest products/test incremental state) into a cross-run store; the cache hierarchy is cook → per-unit zccache → nothing (see `docs/CI_CACHE.md` § Cache Ownership And Priority). Prefer adding new tests as modules of an existing test target over new top-level files; soldr#2934 consolidated the soldr-cli integration tests into eight category targets (`broker`, `daemon`, `cargo_front_door`, `cache_gc`, `cook_dylint`, `fetch_tools`, `toolchain_env`, `guards`), each a directory of module files behind one `main.rs`. Selecting a single module is `--test <category>` plus a filter — `-E 'test(/^<module>::/)'` under nextest, a positional `<module>::` under plain `cargo test`.
- **Tests are plain `#[test]`; timeouts belong to nextest (soldr#2493)**: Write ordinary `#[test]` functions. Per-test wall-clock budgets are configured in `.config/nextest.toml` as `slow-timeout = { period, terminate-after }` — the default is `60s × 2 = 120s`, and the twelve tests that need longer carry `[[profile.default.overrides]]` entries keyed by `test(=name)`. **Run the suite with `soldr cargo nextest run`, not `soldr cargo test`**: under plain `cargo test` nothing bounds a hung test at all.
- **Why the `timed_test!` watchdog was removed**: it ran each body on a worker thread and called `std::process::abort()` on expiry. Two failures no tuning fixes. (1) *The diagnostic was invisible.* The banner went through `eprintln!`, which libtest captures; a captured buffer is only printed when libtest reports a result, and `abort()` guarantees that never happens — CI saw a bare `signal: 6, SIGABRT`. The macro's own docs told you to re-run with `--nocapture`, useless for a failure that already happened. (2) *The blast radius was the whole binary.* `abort()` killed every other test in the process; in soldr#2517's Linux lane a 60 s test's expiry killed a 120 s test that was 60 s in and blameless. nextest runs each test in its own process, so a timeout names one test, preserves its captured output, and kills only that test. `crates/soldr-cli/tests/guards/no_timed_test_guard.rs` fails the build if `timed_test` reappears anywhere under `crates/`.
- **What nextest does *not* do**: there is no thread-stack dump on timeout. On Unix it sends `SIGTERM`, waits `grace-period`, then `SIGKILL`; on Windows it kills the job object with no grace window. If stack dumps are ever wanted, the `SIGTERM` window is the hook.
- **Triaging a red lane that shows a timeout**: nextest prints `TIMEOUT [> Ns] <crate>::<binary> <test_name>` and retains that test's output.
  1. Take the test name from the `TIMEOUT` line — it is the test that blew its budget, and no longer takes any other test down with it.
  2. Decide whether that named test can even reach your change. A tool-fetch-only or parser-only test cannot be affected by daemon/IPC changes.
  3. These lanes time out on `main` too under runner contention — **rerun the failed job before blaming the PR** (`gh run rerun <run-id> --failed`), and diff a red lane against `main` *by test name*, not by lane name.
  4. Only raise a budget if the test is *legitimately* long. A stub-driven test that suddenly takes 120 s is an environmental stall; raising the budget masks the hang the timeout exists to surface. Note that a broker-spawning test still pays a real cold start — process spawn, route acquisition and daemon bringup, recorded per phase in `daemon-bringup.jsonl` (soldr#3163) — which is measurable and not a hang. It no longer pays a full executable image hash each time: the broker's blake3 is memoized per inode (soldr#2521/#2996) and optimized (soldr#3166), and the daemon's per-start SHA-256 identity hash fell from ~2.9 s to ~0.13 s (soldr#3173).
