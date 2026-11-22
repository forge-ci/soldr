# Canonical Cargo invocation (soldr#4195)

Status: design, 2026-11-21. Scope: feature/package-selection drift that makes
`soldr cargo` rebuild the dependency graph. Measurements in this document are
from soldr `main` at `616f6019`, Cargo 1.98.1, and zccache `=1.15.14`. zccache
line numbers refer to the local checkout at `bc3a6079`.

## 1. Problem

Two `soldr cargo` invocations in one repo that select different features,
packages or dev-target sets make Cargo resolve different dependency features.
Every dependency whose resolved features differ is a real miss. **Every crate
above it also misses**, because Cargo hashes each dependency's metadata into the
dependent's `-C metadata`, and zccache keys on that hash. The cascade, not the
handful of real feature changes, is what turns a few-minute validation into
hours on a loaded shared host.

The fix is not in the cache. The miss is a correct miss: the artifacts really
differ (section 3.2). The fix is to stop issuing divergent invocations, and to
make divergence visible when it happens anyway.

## 2. Measured evidence

### 2.1 soldr's own workspace: `--workspace` vs `-p soldr-core`

Method: `soldr cargo tree -e normal,build,dev --prefix none --no-dedupe -f '{p}|{f}'`
for each selection, union the features per package, intersect the package sets,
then take the reverse-dependency closure over `cargo metadata` resolve nodes.
This step resolves only and compiles nothing. Script: section 10, slice S1.

| measure | value |
|---|---|
| packages built under `--workspace` | 294 |
| packages built under `-p soldr-core` | 158 (all of them shared with `--workspace`) |
| shared packages whose **own** resolved features differ | **22** |
| shared packages whose `-C metadata` changes (own + inherited) | **67 of 158 (42%)** |
| packages that change **only** through inheritance (cascade) | **45** |

The 22 include `serde`, `serde_core` (`rc`), `syn` (`visit-mut`), `tokio`
(`full`, `io-std`, `parking_lot`, `test-util`, `tracing`), `regex` (14
`perf`/`unicode` features), `rustix`, `log` (`std`), `mio`, `clap`,
`clap_builder` (`env`), `cc` (`parallel`), `kernal-api` (11 features) and
`running-process` (`client-async`). Because `syn`, `serde`, `log` and `tokio`
sit near the bottom of the graph, one `-p` run in soldr itself rebuilds 42% of
the shared dependency units, two thirds of them through the cascade alone.

The cascade count is an upper bound. It walks the workspace-wide resolve graph,
which includes edges a narrower build may not activate.

### 2.2 datalake-core: measured at plan level

`cargo tree` on `twp/datalake-core` (`bf3b5ee2`) timed out after 300 s on all
three pairs at load average 245 — the same failure the issue reported. Measuring
Cargo's *build plan* instead needs no compile and takes about a second per shape:

```
cargo +nightly build <shape> --unit-graph -Z unstable-options
```

`--unit-graph` prints every unit Cargo would build with its resolved feature
list and its dependency edges. A unit is dirty when its own features differ from
the other shape's, or when any dependency is dirty (the metadata cascade of
3.2). 17-member workspace, 386–542 dependency units per shape:

| from | to | dependency units rebuilt | % of deps |
|---|---|---|---|
| `--workspace --all-features --all-targets` (lint) | `--workspace --lib --bins` (test) | 101 / 450 | **22 %** |
| `--workspace --all-features --all-targets` (lint) | `-p datalake-spool --lib` | 133 / 386 | **34 %** |
| `--workspace --lib --bins` (test) | `-p datalake-spool --lib` | 106 / 386 | **27 %** |
| `--workspace --all-features --all-targets` | `--workspace --all-features --lib --bins` | 76 / 458 | **17 %** |
| `--workspace --all-features --all-targets` | `-p datalake-spool --all-features --all-targets` + `-Zfeature-unification` | 31 / 388 | **8 %** |

Readings:

- `-p` subsets are the worst axis, as the issue says: 27–34 % of the dependency
  graph per switch.
- `--all-targets` is its own 17 % axis. It pulls dev-dependencies into the graph
  for every member, which changes the *lib* units' features (`test-util` on
  tokio, `v7` on uuid, `server-auto` on hyper-util). This repo's `./lint` and
  `./test` therefore disagree with no `-p` and no feature flags involved, so the
  canonical profile must pin target selection too (3.3).
- The last row is 2.4's mechanism: one flags shape plus workspace feature
  unification takes the 34 % case to 8 %, and the residue is mostly units that
  do not exist in the other graph at all rather than feature mismatch.

The issue's own `cargo tree` attempt failed the same way (400 s). Plan-level
measurement is the technique to use on a loaded host; the S1 probe should use
it rather than `cargo tree`.

### 2.3 Workspace feature unification removes `-p` drift at the source

3-crate probe workspace: `liba` (optional feature `extra`), `app` → `liba`,
`sibling` → `liba` with `features = ["extra"]`. `cargo +nightly build -v`,
reading the `-C metadata` Cargo emits:

```
# 1. build -p app
crate-name liba   metadata=395eae17f01b5271
crate-name app    metadata=a73a95e9a1aab770

# 2. build --workspace                    (sibling enables liba/extra)
crate-name liba   metadata=fd7a104697c7cdb6
crate-name app    metadata=f87411030d5eb1f2

# 3. build -p app, CARGO_RESOLVER_FEATURE_UNIFICATION=workspace, -Zfeature-unification
crate-name liba   metadata=fd7a104697c7cdb6      same as --workspace
crate-name app    metadata=f87411030d5eb1f2      same as --workspace
```

Run 1 vs run 2 is the cascade of 3.4 proved with real rustc invocations: `app`'s
own source and own features are identical, and only `liba`'s features changed,
yet `app`'s metadata moves. Run 3 is the mechanism: with
`resolver.feature-unification = "workspace"` (RFC [3692], tracking
[rust-lang/cargo#14774]) Cargo resolves features before filtering to the selected
packages, so `-p app` emits an argv identical to `--workspace` and the artifacts
are literally reused.

Precedence, verified on this host:

| toolchain | how the mode is requested | effect |
|---|---|---|
| nightly | `-Zfeature-unification` + `CARGO_RESOLVER_FEATURE_UNIFICATION=workspace` (env or `.cargo/config.toml`) | argv becomes identical to `--workspace`: `liba/extra` is unified in |
| nightly | `-Zfeature-unification` alone | **no effect**; the default mode is `selected` |
| stable | either | `warning: ignoring 'resolver.feature-unification' without '-Zfeature-unification'`, then the default `selected` behaviour |

Both halves are needed, and the env var alone is not enough on stable. This is
why 4.5 makes it a profile field that soldr applies only when the toolchain
accepts `-Z`, and why a stable repo needs a workspace-hack crate (4.5) for the
same effect.

### 2.4 Cargo 1.98.1 has no stable workspace feature unification

A two-member probe workspace with `.cargo/config.toml`
`[resolver] feature-unification = "workspace"` prints:

```
warning: ignoring `resolver.feature-unification` without `-Zfeature-unification`
```

and `-Zfeature-unification` is refused on stable. On soldr's pinned train the one
Cargo-native fix (RFC 3692) is therefore inert, which is why 4.5 splits the
mechanism by toolchain: `unify = "workspace"` is applied when the toolchain
accepts `-Z`, and a workspace-hack crate is the stable equivalent.

## 3. Key mechanics

### 3.1 soldr adds no manifest-level selection key (refutes the premise)

`crates/soldr-cli/src/cargo_front_door/inputs.rs:44` `cargo_feature_inputs()`
and `inputs.rs:67` `selected_cargo_args()` have **no callers**. They are only
re-exported at `cargo_front_door/mod.rs:112-116`. Their sibling
`stable_hash_json` is still used, at `inputs.rs:194` and in tests. The module
docstring (`inputs.rs:1-10`) still describes `rust_plan` feeding a build
manifest. That target-level cache was removed in soldr#2996 (CLAUDE.md
"Serialization": "The former rust-plan schema was removed with the target cache
in soldr#2996"). So:

| cache layer | sees feature/package selection? | how |
|---|---|---|
| soldr `rust_plan` / build manifest | **no**. The layer is gone; `cargo_feature_inputs` is dead code | — |
| soldr `ci-test` plan fingerprint | records it for reporting only (`ci_test/plan.rs:457-459`) | not a cache key |
| `soldr cook` | prep only (`cook_target_prep.rs:35`, `target_lifecycle::cargo_feature_flags`) | uses selection to pick SDK overrides, not as a key |
| **zccache per-unit key** | **yes, indirectly** | through Cargo's rustc argv for each unit |

The only key that selection poisons is zccache's per-unit key, and it is
poisoned by Cargo's argv rather than by anything soldr adds. Nothing in soldr
needs "un-poisoning". Delete the dead code (slice S0) so nobody spends a day
chasing it again.

### 3.2 What zccache keys on

`zccache-depgraph/src/context/mod.rs:503-575` (`from_parsed_args_with_driver`)
builds the Rust compile context from:

- `cfgs` (`:571`): every `--cfg feature="…"`, i.e. the unit's **own** features.
- `cargo_metadata` (`:574`): `-C metadata=<hash>`.
- `extra_filename` (`:575`): `-C extra-filename=-<hash>`, parsed at
  `zccache-compiler/src/parse_rustc.rs:604,615`.
- `extern_crates` (`:513-518`), reduced by `extern_path_key` (`:190-203`) to the
  extern's **file name**. That name embeds the upstream crate's metadata hash
  (`libserde-<hash>.rmeta`). The function's own doc says the file name "alone
  uniquely identifies the extern".

The request fast path (`zccache-daemon-core/src/daemon/server/keys.rs:565-605`,
`request_fingerprint`) hashes the full normalized argv. All three channels carry
the upstream metadata hash, so a dependent with identical source and identical
own features still gets a new key.

### 3.3 Which selection inputs change which units

Resolver 2 unifies features over the **selected** packages and the **selected**
target kinds. Dev-dependency features count only when a target that needs them
is built.

| input | changes a shared unit's argv? | mechanism |
|---|---|---|
| `--features`, `--all-features`, `--no-default-features` | **yes** | member `--cfg feature`; dependency features through unification |
| `-p`/`--package`, `--workspace`, `--exclude` | **yes** | the unification set changes (2.1: 22 direct, 45 cascade) |
| `--all-targets`, `--tests`, `--test X`, `--benches`, `--bench X`, `--examples`, `--example X` | **yes, when any dev-dependency enables a feature on a normal dependency** | dev-deps join unification only when a test, bench or example target is built |
| `--lib`, `--bins`, `--bin X` alone | no | selects units only; a lib or bin build never activates dev-deps |
| `--target <host>` vs no `--target` | **yes, every target-kind unit** | Cargo passes `--target` to rustc only when given, and the output dir moves to `target/<triple>/` |
| flags after `--` on `cargo clippy` | workspace members only | they reach `clippy-driver` through `CLIPPY_ARGS`. Cargo uses `RUSTC_WORKSPACE_WRAPPER` for members only; dependencies are not linted |
| `check`/`clippy` vs `build`/`test` | yes, always | `--emit=metadata` vs `--emit=link`; disjoint by design |

`--target` is a drift source the issue did not list, and it is live inside
soldr. `soldr ci-test` always passes `--target <host>` (`ci_test/plan.rs:114`).
The local-gate `rust` lane's clippy (`ci/local_gate.py:664-669`) and
`soldr lint rust` (`lint_cmd.rs:208-214`) do not. On Linux the front door never
injects one: `cargo_front_door/target.rs:47-56` injects only on Windows. The
gate's clippy and the remote ci-test clippy therefore share no units.

### 3.4 Is `-C metadata` the hard blocker for option B? Yes

- rustc derives `StableCrateId` from the crate name and the `-C metadata`
  values. Every exported symbol carries it: as the legacy-mangling `h<hash>`
  suffix input, and as the v0-mangling crate disambiguator. An rlib built under
  metadata M1 exports different symbol names than one built under M2. A
  dependent compiled against M1 cannot link against M2.
- A dependent's `.rmeta` records each upstream crate's `StableCrateId`/SVH.
  Loading a mismatched upstream fails (E0460, "found possibly newer version of
  crate"). Metadata-only units (`check`, `clippy`) are blocked too.
- Content-hashing the externs instead of using their file names does not help:
  the upstream bytes differ because the symbols differ.

Reuse across metadata drift is therefore impossible without rewriting symbol
tables, and soldr will not do that. What is achievable is **attribution**
(section 4.4).

## 4. Recommended design: A (enforced canonical profile) + B-min (miss attribution)

Option C is a documented opt-in, not a default (section 7).

### 4.1 Configuration schema

The profile lives in the existing soldr metadata table, which is read by
`crates/soldr-cli/src/cargo_metadata_soldr.rs`, the same place as
`[workspace.metadata.soldr.lint]` (soldr#3958). It needs no new file, Cargo
ignores it, and `soldr`, `soldr lint` and `ci-test` already parse this table.

```toml
[workspace.metadata.soldr.cargo]
# off | warn | refuse. Env override: SOLDR_CANONICAL=off|warn|refuse.
# Default when the table is present: "warn". Table absent: "off".
enforce = "warn"

# The one dependency resolution every check/clippy/test run should share.
features = []                # explicit list, or the string "all"
default-features = true
packages = "workspace"       # "workspace" or an explicit list of members
exclude = []                 # only with packages = "workspace"
dev-targets = true           # target breadth: include all dev target kinds
target = "explicit-host"     # "explicit-host" | "implicit"; see 3.3

# Clippy shape. These are the ONLY args allowed after `--` on `soldr cargo clippy`.
clippy-args = ["-D", "warnings"]

# Declared alternates: legitimate extra resolutions (e2e suites, feature-gated
# lanes). Each one is its own cache family. Declaring it suppresses the
# warning, and `soldr lint ci` reports how many families the repo pays for.
[workspace.metadata.soldr.cargo.alternates.e2e]
features = ["e2e"]
packages = ["datalake-core"]
```

Rules:

- `features = "all"` together with `default-features = false` is a parse error,
  matching `add_all_features` at `lint_cmd.rs:483-493`.
- An unknown key is a hard error naming the key. This follows the existing
  `LintConfigError` pattern (`cargo_metadata_soldr.rs:148-173`).
- `[package.metadata.soldr.cargo]` is accepted for single-crate repos, with the
  same precedence as `targets`/`lint` (`cargo_metadata_soldr.rs:19-20`).

### 4.2 Front-door behaviour (`soldr cargo …`)

The check covers the compiling verbs `check`, `clippy`, `test`, `build`,
`nextest run` and `nextest archive`, and the shorthand forms (`soldr test`, …).
It runs once in `cargo_front_door/run.rs` `run_cargo_front_door` (`run.rs:1`)
before the cargo child is spawned, and only when the table exists and
`enforce != off`.

The comparison is **syntactic** on a normalized selection:
`(feature set | "all", default-features, package set | workspace∖exclude, dev-targets?, target-explicit?)`.

- Feature lists are sorted and deduplicated. Package names are compared as a
  set.
- `dev-targets?` measures breadth, not dev-dependency activation. It is true
  for `--all-targets`, `--tests`, `--benches`, `--examples`, or a test verb
  with no target selector. A single `--test`, `--bench`, `--example`,
  `--bin`, `--lib` or `--bins` narrows selection and does not widen breadth.
- A `-p` list that equals the whole member set counts as `workspace`.

An invocation matches when its tuple equals the canonical tuple or any declared
alternate. Otherwise:

```
soldr: warning: off-profile cargo invocation (soldr#4195)
  this run resolves dependency features differently from the repo's canonical
  profile ([workspace.metadata.soldr.cargo] in Cargo.toml), so dependency
  crates built by the canonical lint/test runs will NOT be reused.
    differs in: packages (-p datalake-spool vs --workspace), features (none vs all)
  canonical:  soldr cargo test --workspace --all-features --target x86_64-unknown-linux-gnu
  to narrow what runs without changing what compiles, keep the canonical
  selection and filter tests instead:
              soldr cargo nextest run --workspace --all-features -E 'package(datalake-spool)'
  silence: SOLDR_CANONICAL=off, or declare this selection under
           [workspace.metadata.soldr.cargo.alternates.<name>]
```

With `enforce = "refuse"` the same text is printed in red with the header
`soldr: error: off-profile cargo invocation refused (soldr#4195)`, and soldr
exits 2 without spawning cargo, as the fail-loud policy (soldr#3815) requires.

The "narrow without changing what compiles" line is the main user-facing fix.
For tests, a nextest `-E 'package(X)'` / `-E 'test(...)'` filter with the
canonical selection compiles the canonical unit set, which is all cache hits on
a warm host, and runs only what the user asked for. For `cargo test` the
suggestion is `--workspace … -- <filter>`.

Why syntactic rather than resolving equality with `cargo tree`: the check has to
cost nothing on the hot path. 2.2 shows that resolution can take more than
300 s on a loaded host, which is exactly when the check matters. A syntactic
mismatch that happens to resolve identically is a false positive, and the
declared alternate silences it. See open question Q2 for a cached semantic
upgrade.

Bypasses, which are never checked: `--no-cache`, `RUSTC_WRAPPER=""`
(`cargo_args_use_reserved_no_cache`), `metadata`, `tree`, `fmt`, `doc`, and
every non-compiling verb.

### 4.3 Making soldr's own entrypoints canonical

`soldr lint rust`, `soldr ci-test` and the plan builders read the profile and
emit its selection:

- `lint_cmd.rs` `rust_step_args` (`:194-275`) takes features, packages and
  target-explicitness from the profile, and clippy uses `clippy-args`. Today it
  hard-codes `--workspace --all-targets` and `-D warnings`.
- `ci_test/plan.rs` `workspace_selection` (`:603-609`) and the scope args do the
  same. An explicit `--features`, `--all-features` or `-p` on
  `soldr ci-test` is then checked against the profile like any other
  invocation. Explicit caller selectors replace the corresponding generated
  selection dimension; they are never silently discarded. With no declared
  profile, all historical argv is preserved byte-for-byte.
- Doctests keep `--doc` and the canonical packages/features/target, but omit
  target-kind selectors: Cargo rejects `--doc --all-targets`. The normalizer
  therefore compares every other axis for `test --doc`, leaving target breadth
  to the separate rustdoc family. An implicit-host profile also uses Cargo's
  unqualified artifact directory in the plan domains.

The command lint runs and the command test runs then share one resolution by
construction, so a repo cannot drift between its own two entrypoints.

### 4.5 `unify`: remove `-p` drift at the resolution level

The profile gains one field, and it is the difference between "warn about `-p`"
and "make `-p` free":

```toml
[workspace.metadata.soldr.cargo]
unify = "auto"      # auto | workspace | off. Default "auto".
```

`auto` means: when the resolved toolchain is a nightly (or a stable that accepts
`-Zfeature-unification`), soldr's front door passes `-Zfeature-unification` to
every compiling verb and sets `CARGO_RESOLVER_FEATURE_UNIFICATION=workspace` on
the cargo child. Per 2.3 that makes `-p X` emit an argv identical to
`--workspace`, so dependency artifacts are reused instead of rebuilt. When the
toolchain refuses `-Z`, `auto` degrades to today's behaviour plus the 4.4
attribution line — never to an error, and never to a silently different
resolution.

Two consequences worth stating explicitly:

- **`packages` stops being a drift axis under `unify = "workspace"`.** A `-p`
  run is then the canonical dependency resolution with fewer units, so 4.2's
  `differs in: packages (...)` warning is suppressed and only feature, dev-target
  and target-explicitness differences are reported.
- **It must be uniform.** Switching unification mode changes every dependency
  unit's `-C metadata`, so a host that mixes unified and non-unified runs pays
  one full rebuild per switch. It therefore has to come from the profile, not
  from a flag an agent can add or forget — which is the same enforcement
  argument as the rest of this design.

`soldr` already knows how to get a pinned toolchain and how to pass extra flags
to it, and already ships a nightly for its Dylint libraries, so `auto` is a
toolchain-channel test plus two argv/env edits, not new plumbing. Keep Dylint's
nightly out of `auto`: the *build* toolchain decides, not the lint toolchain.

**Stable fallback (opt-in C).** For a repo whose pinned toolchain is stable and
which is willing to take the churn, a committed workspace-hack crate gives the
same resolution with no `-Z`: one empty crate depending on the union of every
workspace dependency's features, depended on by every member. Because the hack
sits in every member's graph, a dependency gets the union features under any
`-p` selection, so its argv matches the `--workspace` one. `cargo-hakari`
generates it (register it in `known_tools`, S8); `soldr lint` should fail when
the generated crate is stale relative to `Cargo.toml`. Do not write a
`soldr cargo unify` generator — hakari exists and is reviewed.



### 4.4 B-min: miss attribution

zccache already journals each compile with its argv-derived context. Add a
post-session attribution in soldr's session stats (`crates/soldr-cli/src/cache/session.rs`):

For each miss, look up the most recent journal entry with the same
`(crate_name, crate_types, emit, target, source-tree hash)` and compare it with
the current entry:

| class | condition | label in reports |
|---|---|---|
| `selection-drift` | own `cfgs` equal, `cargo_metadata` differs | metadata changed, source unchanged |
| `own-features` | own `cfgs` differ | unit features changed |
| `flags` | rustc/clippy flags differ | flags changed |
| `source` | source hash differs, or no prior entry | real miss |

`soldr status` and the end-of-build summary print one line:

```
soldr: 412 misses: 287 selection-drift (metadata changed, source unchanged), 31 own-features, 94 source
       selection drift is avoidable: see [workspace.metadata.soldr.cargo] (soldr#4195)
```

This meets the issue's acceptance criterion "The session stats show it". It
also measures the cost of drift with no `cargo tree` call.

## 5. Clippy-specific drift

- **Clippy vs test never share units** (3.3, emit kinds). This is expected and is
  not a defect. State it in the attribution report rather than "fix" it: a
  clippy miss after a test run is classified as `flags` (different emit), not
  `selection-drift`.
- **Flags after `--` (`-A clippy::disallowed-methods`) invalidate every member's
  clippy unit.** Dependencies are unaffected (3.3). The cheapest correct answer
  has two parts:
  1. `clippy-args` in the profile is the only allowed tail. The front door warns
     or refuses on any other tail, with the same error shape and
     `differs in: clippy args (-D warnings -A clippy::disallowed-methods vs -D warnings)`.
  2. Lint levels belong in `[workspace.lints.clippy]` / `[lints]`, not on the
     command line. Changing them is then a reviewed manifest edit, not a
     per-invocation habit. The warning text names this:
     `move lint levels to [workspace.lints.clippy] in Cargo.toml`.
- A one-off investigative run such as `-A clippy::all -D clippy::disallowed_methods`
  in zccache (`ci/lint.py:596`, `clippy.yml:71`) is declared as an alternate
  with `clippy-args` set, so it is a known, bounded family.

## 6. Enforcement: `soldr lint ci` rule `canonical-cargo-invocation`

This is a new `CiRule` in `crates/soldr-cli/src/lint_ci/canonical_invocation.rs`,
appended to `rules()` (`lint_ci/registry.rs:23-25`). It scans the surfaces that
`lint_ci/scan.rs` already discovers (workflows and actions), plus the repo
entrypoints `lint`, `test`, `push`, `ci/**/*.py`, `local-gate.toml`,
`bosn.toml`, `justfile` and `Makefile`.

The rule fires when the repo declares `[workspace.metadata.soldr.cargo]` and a
literal `soldr cargo <compiling-verb> …` (or `cargo …` inside a `soldr` front
door) has a normalized selection tuple that matches neither the profile nor an
alternate. The finding names `file:line`, the differing fields and the
canonical command. Per-line suppression uses the existing
`# soldr-lint-ci: allow canonical-cargo-invocation` mechanism
(`lint_ci/scan.rs:30` `allows`).

What fails:

- datalake-core `ci/lint.py:80` (`--all-features`) vs `ci/test.py:179`
  (default features): whichever one the profile does not name fails.
- zccache `ci/lint.py:579/592` (three clippy feature sets) without declared
  alternates.
- soldr `ci/local_gate.py:664` (no `--target`) once `target = "explicit-host"`
  is declared.

What does not fail: dynamically built argv (Python lists assembled at runtime).
The static rule reuses the filesystem-only workspace-member census (including
its single-component `*` wildcard limit); it does not run Cargo metadata.
The front-door check (4.2) is the runtime authority for computed commands and
full Cargo membership, in the same way
the bare-Cargo trap complements the `rg` scan in soldr#2878.

The clud command scanner (`block_bad_cmd`) can call
`soldr lint ci --check-command '<argv>'` to apply the same decision to agent
shell commands. That is a follow-up in talos-hq/clud, not in this repo.

## 7. Rollout per repo

Data comes from a read-only survey of the local checkouts, 2026-11-21. No repo
has a workspace-hack crate or `[workspace.metadata.hakari]`.

| repo | entrypoints (file:line) | drift | proposed canonical | alternates |
|---|---|---|---|---|
| **twp/datalake-core** | lint `ci/lint.py:80` clippy `--all-targets --all-features` (root pkg); unit `ci/test.py:179` test `--workspace --lib --bins --test …`; integ `ci/test.py:478` root pkg; e2e `ci/test.py:478` `--features e2e` | **3+ resolutions**: features (all vs default vs e2e) × packages (root vs workspace) | `--workspace --all-features`, dev-targets, explicit host. The `all-features` build already compiles `e2e`/`test-support`, so lint and test share it | none needed if `all` is canonical; otherwise `e2e` |
| **soldr** | gate rust lane `ci/local_gate.py:664` clippy `--workspace --all-targets` (no `--target`); `ci-test` `plan.rs:112` clippy `--workspace --all-targets --target <host>`; `test:33-34` `-p soldr-cli`; gate tests lane `ci/workspace_test.py:158` `-p <closure>` (soldr#3731); `ci/wine_lane.py:105` 3-crate `-p` | **target explicitness** (gate vs ci-test); **package subset** (`test`, the soldr#3731 tests lane, wine) = 22 direct + 45 cascade units (2.1) | `--workspace`, default features, dev-targets, explicit host | `wine` (msvc, 3 crates). The soldr#3731 tests lane should keep `--workspace` compilation and narrow **execution** with a nextest `-E 'package(...)'` filter |
| **forge-ci/zccache** | clippy `ci/lint.py:579` default + PRODUCT, `:592` HOSTED, `:596` `-p zccache-daemon-core --features test-support -- -A clippy::all -D clippy::disallowed_methods`; tests `ci-check.yml:220` `--workspace --lib --bins`; `ci/test.py:94` per-`-p` `test-support` | **4 clippy resolutions + test resolution**; clippy tail drift | HOSTED feature set for clippy and test | `product` (PRODUCT features), `disallowed` (daemon-core clippy tail) |
| **running-process** | `ci/lint.py:173` clippy `--workspace`; `ci-preflight-rust.yml:136` clippy `--exclude running-process-py --features client\|daemon`; tests `ci/test.py:917` `--workspace`, `:949/:970` `-p running-process --features test-seams\|daemon`; build `ci-preflight.yml:154` 5-feature set | **most drift**: ≥5 resolutions | `--workspace --exclude running-process-py --features client` | `daemon`, `test-seams`, `all` (coverage) |
| **obs-rust/obs-studio** | `ci/local_gate.py:710` clippy `--workspace`; `:712` five `-p` feature sets; cross `:805` `--exclude obs-c-oracle --lib --bins` | deliberate 5-way clippy matrix; cross exclude | `--workspace`, default | the five `FEATURE_SETS` as alternates (deliberate, now counted) |
| **fbuild** | `lint:77` / `ci/lint.py:68` per-file `-p crate`; `check-ubuntu.yml:141-152,275` `-p` ignored-test steps | package subsets only | `--workspace`, default | none; convert `-p` ignored-test steps to nextest filters |
| **clud** | tests and lint `--workspace`; `bosn.toml` `-p clud` ad hoc; `ci/xbuild.py:333` `-p clud --features installer-ci-fixture` (build) | ad hoc subsets only | `--workspace`, default | `installer-fixture` |
| **ai-tools-clone** | `ci/lint:16` clippy `--all-features`; `ci/test:13` test `--all` | features | `--workspace --all-features` | none |
| template-python-rust-cmd, fastled-cli | consistent `--workspace`, default | none | declare to lock it in | none |

Order: soldr (dogfood; it fixes its own gate-vs-CI miss), then datalake-core
(the reported incident), then zccache and running-process (largest matrices),
then the rest. Each rollout PR adds the table and fixes or suppresses every
`canonical-cargo-invocation` finding. `enforce` starts at `warn` and moves to
`refuse` after one week of clean attribution reports.

**Template default:** `soldr init`-style templates (template-python-rust-cmd)
ship the table with `packages = "workspace"`, default features and
`enforce = "warn"`.

## 8. Rejected alternatives

- **B-full: reuse rlibs across `-C metadata` drift.** Impossible. Symbol names
  and rmeta dependency hashes embed `StableCrateId` (3.4).
- **Rewriting `-p X` to `--workspace` silently.** It changes what is tested and
  what can fail. soldr must not change the semantics of a cargo command. The
  front door suggests the rewrite instead.
- **`resolver.feature-unification = "workspace"` (RFC 3692) as the *only* fix.**
  It is the right long-term fix for package-subset drift and 4.5 adopts it, but
  on its own it is nightly-only (2.4) and it does nothing for `--all-features`
  vs default, for `--all-targets` drift, for `--target` explicitness or for
  clippy tails. It is one axis of the profile, not a substitute for it.
- **Option C (cargo-hakari workspace-hack) as the default.** It fixes
  package-subset and dev-target drift for dependencies only, and does nothing
  for `--all-features` vs default on members' optional deps or for `--target`.
  It adds a generated crate that every member must depend on, which is churn on
  every dependency change in each of nine repos, and every `-p` build then
  compiles the full union. It does not interact with clippy `-A` drift (members
  only, 3.3). Verdict: opt-in. Register `cargo-hakari` in `known_tools` so
  `soldr cargo hakari generate` works pre-built; do **not** build a
  `soldr cargo unify` generator, because hakari exists. A repo that adopts it
  can declare `packages = ["x"]` subsets as alternates at no cost.
- **Resolving equality with `cargo tree` on every invocation.** It costs 300 s
  or more on a loaded host (2.2). See Q2.
- **A new `soldr.toml` or `ci.toml [rust.canonical]`.** `ci.toml` belongs to the
  ci.yml linter and is not read by `soldr cargo` at runtime. A new file splits
  soldr config across two homes, when `[workspace.metadata.soldr]` already
  exists and is parsed.

## 9. Open questions

- **Q1.** How does `CLIPPY_ARGS` reach zccache's key? Partly checked. No
  non-test zccache source names `CLIPPY_ARGS`. The explicit untracked-env set
  for non-`rustc` drivers is the `CARGO_*` family
  (`zccache-core/src/key_env.rs:217-247`, `may_read_env_untracked`).
  clippy-driver records `CLIPPY_ARGS` as a `# env-dep:` in dep-info (its
  `track_clippy_args`), so the key should pick it up through the env-dep path.
  Add a zccache regression test before relying on it: two clippy compiles that
  differ only in `CLIPPY_ARGS` must miss. If they hit, that is a correctness bug
  (stale lint results), separate from this design.
- **Q2.** Semantic equivalence cache: compute a resolution fingerprint
  (`cargo tree -e features` digest) once per `(Cargo.lock, manifests, selection)`
  in the background, then store the selections that resolve identically to the
  canonical profile in `state.sqlite3`, so a syntactically different but
  equivalent `-p` stops warning. Worth it only if the attribution data shows
  many false positives.
- **Q3.** Is the drift worse in datalake-core than in soldr (2.1)? Re-run the S1
  probe off-peak and record the numbers here.
- **Q4.** soldr#3731 (gate tests lane `-p` closure) was a speed win measured on
  an idle host. Re-measure it against `--workspace` compile plus nextest filter
  on a warm cache before flipping it.

## 10. Implementation slices (ordered)

| # | slice | files | proving test |
|---|---|---|---|
| S0 | Delete dead `cargo_feature_inputs` / `selected_cargo_args` and fix the stale `inputs.rs` docstring (refactor only) | `cargo_front_door/inputs.rs`, `cargo_front_door/mod.rs` | build and clippy green; `rg cargo_feature_inputs` is empty |
| S1 | Drift probe: `ci/feature_drift_probe.py <repo> <selA> <selB>`, using `cargo +nightly build --unit-graph -Z unstable-options` (2.2) so it costs ~1 s per shape and never compiles; falls back to `cargo tree` when no nightly is available | `ci/feature_drift_probe.py`, `tests/test_feature_drift_probe.py` | fixture unit-graph JSON → asserts 1 direct + N cascade, and that a `-Zfeature-unification` pair reports 0 |
| S2 | Parse `[workspace.metadata.soldr.cargo]` into a typed `CanonicalProfile` with alternates, `unify`, and errors | `cargo_metadata_soldr.rs` (or a new `cargo_metadata_canonical.rs` because of the 1,500-line ratchet) | `reads_canonical_profile`, `rejects_all_features_with_no_default`, `rejects_unknown_key`, `unify_defaults_to_auto` |
| S2b | `unify = auto`: detect toolchain channel, pass `-Zfeature-unification` + `CARGO_RESOLVER_FEATURE_UNIFICATION=workspace`; inert on stable | `cargo_front_door/run.rs`, `toolchain*.rs` as needed | fixture: nightly toolchain + `-p a` → the dependency's rustc argv equals the `--workspace` one (the 2.3 probe as an assertion); stable → flag omitted, no warning |
| S3 | Pure selection normalizer `Selection::from_args(verb, args, members)` + `diff(&profile) -> Vec<Drift>` | `cargo_front_door/canonical.rs` (new) | table test: `-p` == all members → match; `--lib` → no drift; `test` without selector ⇒ dev-targets; `--target host` vs none differs; clippy tail differs |
| S4 | Front-door warn/refuse with the exact text from 4.2; `SOLDR_CANONICAL` env override | `cargo_front_door/run.rs`, `canonical.rs` | integration test in `tests/cargo_front_door/`: fixture workspace with `enforce="refuse"` + `soldr cargo test -p a` exits 2, no cargo spawned (stub cargo records invocation); `warn` spawns and prints the header |
| S5 | `soldr lint rust` and `soldr ci-test` emit the profile selection; repository profile declarations and gate rollout remain S9 | `cargo_front_door/canonical.rs`, `lint_cmd.rs`, `ci_test/canonical_profile.rs`, `ci_test/plan.rs` | `lint_cmd_tests`: clippy argv equals profile; `ci_test` plan JSON snapshot; ci-test clippy argv == canonical profile argv |
| S6 | `soldr lint ci` rule `canonical-cargo-invocation` | `lint_ci/canonical_invocation.rs` (new), `lint_ci/registry.rs`, `lint_ci/scan.rs` (entrypoint surfaces) | fixture repo: off-profile line in `ci/lint.py` → one finding with file:line; alternate-matched line → none; suppression comment → none |
| S7 | Miss attribution (`selection-drift` / `own-features` / `flags` / `source`) in session stats and `soldr status` | `cache/session.rs`, status renderer | unit test over synthetic journal pairs per class; integration: build fixture `--workspace` then `-p a` → summary reports `selection-drift > 0`, `source == 0` |
| S8 | Register `cargo-hakari` in `known_tools` (opt-in C) | `crates/soldr-fetch/src/fetch/known_tools.rs` | `known_tools` registry test resolves `hakari` subcommand |
| S9 | Fleet rollout PRs (section 7 order) | each repo's `Cargo.toml` + entrypoints | `soldr lint ci` clean; attribution report shows `selection-drift == 0` across lint → test → filtered subset (the issue's acceptance) |

### S5/S6 handoff scope clarification

S5 makes the native lint/test entrypoints profile-aware; S6 adds the static
rule and entrypoint discovery. The S5 implementation handoff does not opt this
repository into the profile. Keep that declaration and the local gate's explicit
host argument for S9: bootstrap/build surfaces need a rollout audit first, and
current alternates cannot override target explicitness. Declaring the profile
before that audit would make the new lint fail existing CI bootstrap commands.
