# Verdict ledger

`soldr cargo clippy`, `soldr cargo fmt --check`, `soldr dylint`,
`soldr cargo test --doc`, `soldr cargo deny check bans licenses sources`,
`soldr cargo machete` and any check a workspace declares skip themselves when
a passing run with identical inputs is on record. Tracked in soldr#3894.

A cargo build cache cannot do this: on a fresh CI runner there is no
`target/` to be fresh against, and even a warm cargo still resolves the
toolchain, starts the daemon and walks the unit graph. The ledger answers the
question "did this exact check already pass?" before any of that starts.

## Core and adapters

The ledger is one core with per-tool adapters. The core
(`crates/soldr-cli/src/verdict/`) owns everything that is the same for every
checker: the key, the record, the store, the front-door hook and the CLI.
An adapter is a data row:

| field | meaning |
|---|---|
| `subcommand` | the cargo subcommand (`clippy`, `fmt`, `test`, ...) |
| `requires` | arguments that must all be present (`fmt`: `--check`) |
| `requires_any` | at least one must be present (`deny`: `bans`/`licenses`/`sources`) |
| `forbids` | arguments that make it something other than a pure check (`--fix`, `advisories`) |
| `inputs` | extra repo-relative globs (`**/clippy.toml`, `**/deny.toml`) |
| `env` | extra environment variables (`CLIPPY_CONF_DIR`); `PREFIX_*` matches a prefix |
| `identity` | version probes: `soldr`, or a command such as `cargo deny --version` |
| `replay` | print the recorded output on a hit |
| `pure` | `false` = recognised but never skipped (`audit` reads the network) |

`soldr verdict profiles` prints the effective table. A workspace adds or
overrides rows without a soldr release:

```toml
[workspace.metadata.soldr.verdict.tools.hack]
subcommand = "hack"
requires = ["check"]
identity = ["cargo hack --version"]

[workspace.metadata.soldr.verdict]
exclude = ["docs/**"]   # files known not to affect any check
# enabled = false       # turn the ledger off for this workspace
```

## The key

`soldr-verdict-v1-<tool>-<scope>-<digest>`, a BLAKE3 digest truncated to 128
bits over:

- every tracked file under a package root (the directory of a `Cargo.toml`
  with `[package]`), plus `**/*.rs`, `Cargo.toml`, `Cargo.lock`,
  `.cargo/config*` and `rust-toolchain*` anywhere, plus the adapter's
  `inputs` — by git blob id, so a clean tree reads no file contents;
- uncommitted and untracked files in that set, by content;
- the toolchain: an exact pin (`1.89.0`, `nightly-2026-05-28`) is its own
  identity; a floating channel is resolved with `rustc -vV`;
- the adapter's identity probes, the host triple and `$CARGO_HOME/config*`;
- `RUSTFLAGS`, `RUSTDOCFLAGS`, `CARGO_BUILD_*`, `CARGO_TARGET_*`,
  `CARGO_PROFILE_*`, `CARGO_UNSTABLE_*`, `RUSTUP_TOOLCHAIN`, and the
  adapter's `env`;
- the argv with presentation flags removed (`--color`, `-j`, `-q`, `-v`,
  `--target-dir`, `--timings`), and the working directory relative to the
  repo root;
- the adapter row itself, so changing an adapter invalidates its verdicts.

### Soundness

A false hit skips a check that would have failed, so the derivation fails
open — it runs the check — whenever it cannot account for an input:

- not a git checkout, or git fails;
- a `path` dependency, patch or replace that leaves the repo;
- an unreadable uncommitted file or an untracked directory (nested repo);
- a floating toolchain whose `rustc -vV` cannot be read;
- an identity probe that fails.

Package roots are hashed whole because build scripts, `include_str!` and
`#![doc = include_str!(...)]` read files that are not `.rs`. A workspace
whose root is itself a package therefore hashes the whole repo; a virtual
workspace hashes only its members. `exclude` narrows this, and a wrong
exclusion is a false hit, so use it only for trees nothing compiles.

Known limit: a build script's `rerun-if-env-changed` variables are not
knowable before the build. List any that change lint results in the
adapter's `env`.

## The record

```json
{"v":1,"key":"soldr-verdict-v1-clippy-ws-…","tool":"clippy","argv":["clippy","--workspace"],
 "cwd":".","toolchain":"pin:1.98.1","soldr":"0.9.40","exit":0,"created":1792800000,
 "output":"warning: …\n","output_captured":true}
```

Only passes are recorded. The record repeats its key, and lookup rejects a
record for another key, another schema, or a non-zero exit. The diagnostic
stream is kept (cargo progress lines removed, at most 16 KiB) so a replayed
run prints the same warnings; a run whose stderr went straight to a terminal
records `output_captured: false` and the replay says so.

## Store and remote tiers

```text
<soldr root>/verdicts/      ($SOLDR_VERDICT_DIR overrides)
  records/<key>.json        verdicts this machine may replay
  spool/<key>.json          verdicts recorded here, awaiting an uploader
  seen.jsonl                every gated invocation, hit or miss
```

The store never touches the network. A remote tier (setup-soldr, see
soldr#3894) restores `records/` before checks run — it predicts the keys
with `soldr verdict rekey <seen.jsonl>` from a previous job's log — and
publishes `spool/` afterwards under its own trust policy (trusted refs
write, pull requests only read).

## Controls

- `SOLDR_VERDICTS=off` — always run; `read` — replay but never record.
- `SOLDR_VERDICT_DEBUG=1` — print why each invocation was or was not gated.
- `soldr verdict explain -- clippy --workspace` — the key, every component,
  and hit or miss.
- `soldr verdict ls`, `soldr verdict gc [--max N]` (default 512 records).
