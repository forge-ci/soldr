# Contributing tests

Soldr separates portable logic tests from contracts that require a real host
operating system. This keeps the Linux development loop fast without replacing
filesystem, process, or platform behavior with mocks that cannot prove the
assumption in question.

## Portable logic first

Keep platform-independent parsing, planning, and policy logic in ordinary unit
or integration tests that run in the Linux workspace suite. Host-specific code
should expose fixture- or environment-driven seams where its decision logic can
be tested without the host facility.

When the behavior itself belongs to the operating system, use a native
behavioral integration test instead. Examples include filesystem attributes,
path handling, mapped executables, reparse points, and process lifetime rules.
Put each contract in a dedicated **module**, prefer adding it to an existing
test target under `crates/<crate>/tests/` rather than creating a new top-level
file, gate the module (`#[cfg(windows)]`, for example — or the whole binary
with `#![cfg(windows)]` when the target is host-specific end to end), and
explain the assumption and its failure mode in module documentation. Every
top-level file in `tests/` is compiled as its own executable statically
linking the full soldr graph; that per-file fan-out is how the suite reached
~110 linked binaries and a 3.3 GB CI archive (soldr#2931). soldr#2934 collapsed
the `crates/soldr-cli` half of that fan-out into eight category test binaries —
`broker`, `daemon`, `cargo_front_door`, `cache_gc`, `cook_dylint`,
`fetch_tools`, `toolchain_env`, `guards` — each a `tests/<category>/main.rs`
that declares its sibling files as modules. Add a new contract as a module in
the category it belongs to; reserve a new category (and therefore a new linked
binary) for a genuinely new domain. Use deterministic fixtures where
possible and print an explicit skip reason when a test genuinely depends on
host-installed tooling.

`crates/soldr-cli/tests/cache_gc/windows_delete_semantics.rs` is the reference
example: it pins Windows deletion behavior that had previously been inferred
from a different API. Executable Rust tests are plain `#[test]` functions;
per-test timeouts come from cargo-nextest (`.config/nextest.toml`), and the
workspace guard in `crates/soldr-cli/tests/guards/no_timed_test_guard.rs` keeps
the removed `timed_test!` watchdog from returning (soldr#2493).

### Broker/runtime ownership boundary

Generic process substrate conformance belongs to the exact `running-process`
dependency, not to Soldr's host suite. At running-process 4.10.9 the canonical
coverage is:

| Generic contract | Authoritative running-process test |
| --- | --- |
| singleton refusal and real concurrent starters | `broker::server::singleton_bind::tests::bind_singleton_binds_once_and_refuses_a_second_bind`; `broker::broker_v2_scaffold_accepts_connection::concurrent_starters_yield_exactly_one_singleton_survivor` |
| serialized stale-endpoint recovery | `broker::server::singleton_bind::tests::stale_endpoint_n_way_recovery_has_exactly_one_winner` |
| broker restart and live-route adoption | `broker::lifecycle_process_conformance::broker_restart_re_adopts_live_backend_and_serves_next_client` |
| dead-route replacement, single flight, and other-route isolation | `broker::lifecycle_process_conformance::backend_crash_concurrent_reconnects_launch_one_replacement_without_disturbing_other_instance` |
| child and descendant termination | `core::containment_test::test_contained_group_kills_grandchildren`; `core::containment_test::test_local_kill_tree_kills_root_and_grandchildren` |

Soldr retains adapter acceptance only where its own contract is observable:
the already-bound CLI diagnostic/exit code; Soldr route claims and daemon-image
replacement; cache warmth/durability across daemon restart; wire/session
bridging; service-definition generation; and root/config isolation. The
`cli_kill_matrix` source inventory guard prevents generic multi-route and
restart-storm cases from being silently added back beside the upstream suite.

On Unix hosts, Nextest runs each test through the native
`soldr-nextest-wrapper` (`crates/soldr-nextest-wrapper`, the only
implementation since soldr#3454). `.config/nextest.toml` launches it via
`.github/scripts/nextest_wrapper.sh`, which refuses a developer host and then
resolves the binary by one rule: `SOLDR_NEXTEST_NATIVE_WRAPPER` when set,
otherwise `<profile dir>/soldr-nextest-wrapper` beside the test binary's
`deps/` directory. Cargo builds it there whenever the `soldr-nextest-wrapper`
package is selected (`--workspace` does), and `nextest archive` ships it at the
same relative path, so archived target runs find it after extraction. A
scoped run (`soldr cargo nextest run -p soldr-cli --test guards ...`) never
builds the wrapper's package, so on a miss the first test builds it once into
that same directory -- profile and triple read from the test binary's path,
with the run's own `$CARGO`, under a lock the other tests wait on. Without
`$CARGO` (an archive host) or when that build fails, the shim refuses (exit
98) rather than run a test unwrapped. When Nextest's per-test timeout
sends SIGTERM, the wrapper terminates the isolated child process group and
drains stdout and stderr to EOF before returning. On Linux it first dumps the
child thread stacks (or `/proc` thread state when a debugger is unavailable).
The configured 30-second Nextest grace period bounds that diagnostic shutdown.
Other Unix hosts still get termination and output draining but currently lack
a thread dumper. Windows Nextest timeouts kill their job object immediately,
so the graceful hook cannot run there; output captured before termination is
still retained by Nextest.

The wrapper also isolates each test from two costs that used to leak out of the
suite. On Linux every test gets a private `TMPDIR` that is removed when it exits
(soldr#3079; set `SOLDR_NEXTEST_KEEP_TMPDIR=1` to keep it). On every Unix host it
sets `SOLDR_TEST_FORBID_TOOLCHAIN_INSTALL=1` and `RUSTUP_AUTO_INSTALL=0`
(soldr#3195), so no test can download a Rust toolchain: two fixtures that faked
every tool except `rustup` made every CI runner install a nightly it lacked, 1.6 GB
and ~30 s per gate for one of them. A test that exercises an install path must
give soldr a fake rustup through `SOLDR_TEST_RUSTUP_BIN`; one that reaches the
real rustup now fails with a `test tripwire:` diagnostic naming the command.

A fixture that spawns a child and pipes its stdout or stderr must drain those
pipes while the child runs (soldr#3197). A pipe holds about 64 KB; past that the
child blocks in `write(2)` until someone reads, so `wait_timeout` followed by
`wait_with_output` deadlocks as soon as the output grows, and the failure shows up
as a timeout with the output already written. Spawn through
`common::tracked_child::spawn_tracked`, which drains both pipes from the start. It
also gives you `try_status`, `wait_for_exit` and `wait_for_stdout` for fixtures
that poll a child or time its exit. `Command::output()` is always safe.
`guards/piped_child_drain_lint.rs` fails the build on the undrained shape.

A fixture must not build into the target directory the suite runs from
(soldr#3203). Soldr resolves Cargo's target directory the way Cargo does, so a
fixture that runs `soldr cargo ...` from the crate directory, the test's default
working directory, lands in the repository's own `target/`. There the no-cache
preflight and cleanup hooks would modify live test binaries. The wrapper
therefore sets `SOLDR_TEST_FORBID_TARGET_CONTAINING` to the running test binary,
and such a build fails with a `test tripwire:` diagnostic. Run the fixture in a
temporary workspace or give it its own `CARGO_TARGET_DIR`:
`common::isolated_soldr_command_in(dir)` and
`common::isolated_soldr_command_with_target(dir)` do each, the latter after the
scrub that removes an inherited value.

## Naming a failing test (soldr#2934)

Since the category consolidation, a `crates/soldr-cli` integration test's full
ID is:

```
<category_binary>::<module>::<test_name>
```

for example `cargo_front_door::cli_cargo_wrappers::cargo_fmt_routes_rustfmt_through_zccache_formatter`.
The `<module>` segment is the file the test lives in — the same name it used to
have as a standalone test binary. Tests in `crates/soldr-daemon/tests/` and
`crates/soldr-cache/tests/` did not move and keep one binary per file; unit
tests inside a crate's `src/` are named by their full module path
(`daemon::session_serve::tests::<test_name>`).

Nextest's failure and timeout lines print the binary and the test name in two
pieces — `TIMEOUT [> Ns] <crate>::<binary> <module>::<test_name>` — so the
module prefix appears on the name side, not the binary side. Reproduce a single
test with `soldr cargo nextest run --test <category_binary> -E
'test(=<module>::<test_name>)'`.

Triage is otherwise unchanged: take the name from the failing line, ask whether
that test can even reach your change, **rerun the failed job before blaming the
PR** (these lanes time out on `main` too under runner contention), and diff a
red lane against `main` *by test name*, not by lane name. Only raise a budget
when the test is legitimately long.

Two consequences for `.config/nextest.toml`, which grants extended budgets and
test-group membership through filtersets:

- `binary(<old_file_name>)` no longer selects anything. The equivalent is
  `binary(<category>) & test(/^<module>::/)` — `test(/.../)` is nextest's regex
  matcher and `^` anchors the module prefix to the start of the test name.
- `test(=<test_name>)` no longer selects anything. It must be written
  module-qualified: `test(=<module>::<test_name>)`.

**Nextest does not error on a filter that matches nothing.** It is silently
ignored, the test drops back to the default 60s × 2 budget, and the regression
surfaces later as a bogus `TIMEOUT` with nothing pointing at the config. So
whenever a test is renamed or moves between modules, re-verify every filter that
names it with `cargo nextest list -E '<filter>'`; an empty selection means the
filter is dead.

## How native tests reach CI

Native behavioral tests do not run in the Ubuntu lint job. The existing target
lanes carry them through this path:

1. `.github/workflows/_ci-cross-build-linux.yml` creates a target-specific,
   complete `--workspace` nextest archive.
2. `.github/workflows/_ci-target-run.yml` inventories the complete archive,
   verifies every positive selector in `ci/target-run-ownership.json`, then
   replays only the owned host-sensitive tests on a native runner.
3. `.github/workflows/ci.yml` connects the producer and consumer. Windows uses
   the existing `windows-2025` x64 and `windows-11-arm` ARM64 runners.

The ownership file is an allowlist, not an exclusion filter. Its
`source_classifications` say whether each host-sensitive integration source is
validated once on canonical Linux or needs native target replay. Classification
never selects tests by itself. Separate `replay_selectors` positively name an
exact test or module-qualified test-ID prefix, and a platform-only contract
lists the exact target triples where it applies. The target-run helper fails if
an applicable selector matches zero tests in the complete archive, selectors
overlap, a replay classification lacks a selector, or the target union is
empty. Its inverse source guard fails when a new integration module uses real
process, filesystem, IPC, host, or platform facilities without an explicit
classification. This makes classification and selector decay red CI failures
instead of silently losing native coverage (soldr#2999).

When adding a host-sensitive test, add or update its source classification in
the same change and state the concrete native facility in `reason`. Add a
positive replay selector only when the behavior must execute on each applicable
native target. Portable parsing, planning, source-policy, and data-shape tests
are classified `native-linux-once`: the canonical Linux host suite already
executes them once.

Schema v2 was checked against the complete x86_64 Darwin inventory from Actions
run 33343932713: **113 of 2,863 discovered tests** are selected for native replay
(a 96.1% reduction in executed inventory before ignored tests). Every
target-run writes both the complete and selected inventories to its diagnostics
artifact and reports the live counts in the job
summary; those runtime inventories, not these baseline numbers, are the
coverage authority.

A target only gets step 2 when the replay lands on a runner that is **native
to that target**. `x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`
build on `ubuntu-24.04` and have no other runner to reach, so they are
build-only lanes (`kind: cross-build` in `ci/canonical-targets.json`) — a
replay there would re-run the suite on the image it was built on, which is the
degenerate split soldr#1978 item 3 removed. Their artifact-level invariants are
checked in the build lane instead (`verify_static_link.py`,
`verify_glibc_baseline.py`).

The former no-hosted-macOS mandate (soldr#3071) has a narrow exception:
opt-in `ci-full` and exact-SHA release validation execute Intel and Apple
Silicon target tests on hosted `macos-15-intel` and `macos-15` runners.
Ordinary PR/main validation remains Mac-free; the opt-in `ci-test` label also
runs the Apple Silicon archive replay. A separate advisory Intel replay uses a
[zackees/docker-mac-x64](https://github.com/zackees/docker-mac-x64) macOS
**Recovery** guest (KVM) hosted on an `ubuntu-24.04` runner — see
`ci/macos_recovery_run.py` and the
`target_execution: x86_64-recovery` contract in
`.github/workflows/_ci-target-run.yml` (soldr#3076; this replaced soldr#3071's
hand-baked dockur/macos guest, whose image was never published and whose ssh
secret was never set, so it failed at preflight on every run). Recovery boots
fresh per script with no toolchain of its own and no per-command exec, so
soldr#3078 packs the same positively-owned nextest partition every other
target-run lane replays into that one guest script instead: the guest formats
and mounts the action's blank disk for scratch space, provisions a managed
rustup toolchain via `soldr toolchain ensure`/`link`, and runs `nextest list`
+ `nextest run` against the packaged archive, same as the native path just
without per-step exec. The ownership filter is precomputed Linux-side with
`target_run_ownership.py --filter-only` (the guest has no inventory to
validate it against before it boots) and the inventory/coverage validation
that would normally gate the filter runs afterward, against the guest's own
`nextest list` output, in `ci/macos_recovery_run.py`'s `verify-collected`
mode. That replay is not on the pull-request critical path: soldr#3116 moved
it from `ci.yml` into `.github/workflows/macos-recovery-replay.yml`, which
runs nightly against `main`, on `workflow_dispatch`, and on pull requests
labelled `macos-replay` (the lane had produced no green result in 25 CI runs
and a wedged guest set the run's wall clock; soldr#3088 holds the criteria
for restoring it). `auto-release.yml` runs the same replay again at release
time (`e2e_macos_x64_build` / `e2e_macos_x64_replay`), pinned to the release
commit. That release-time replay is advisory: soldr#3088 removed it from
`publish`'s gate after it blocked v0.9.12 twice on harness bugs without
ever having been green.
`aarch64-apple-darwin` is still cross-built on Linux, but it is no longer
build-only in full validation: its archived tests execute on hosted Apple
Silicon. The complete opt-in matrix and ordinary minimal lane are documented
in [CI_MODES.md](CI_MODES.md).

Linux-runnable contract tests in `tests/test_cross_compile_workflows.py` protect
that route from silently dropping native tests. Add a new runner only when the
behavior cannot be represented by an archived integration test; do not add a
native lint duplicate merely because the main lint job uses Linux.

## Review checklist

When a change relies on host behavior:

- state the assumption in a native behavioral test, not only in prose or a
  shell-specific reproduction;
- keep portable decisions covered in the Linux suite;
- gate the complete integration-test binary for the host it requires;
- run the suite with `soldr cargo nextest run` and avoid unbounded
  environmental waits;
- preserve complete **owned** target-run coverage — every positive selector
  must match the full archive and the union of replay partitions must execute
  every selected test — within the archive's explicit byte/disk budget
  (soldr#2931: linked test products are ephemeral transport, never cached, and
  the bundle must stay compact and single-extraction); and
- validate host failures by test name — now `<category_binary>::<module>::<test_name>`
  — comparing known runner flakes against `main` before changing a nextest
  budget; and
- if the change renames a test or moves it between modules, re-verify every
  `.config/nextest.toml` filter that names it (a filter matching nothing is
  silently ignored, not an error — see "Naming a failing test" above).
