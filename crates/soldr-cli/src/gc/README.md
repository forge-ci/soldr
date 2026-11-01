# `soldr-cli::gc`

Garbage collection for the soldr cache: the `soldr gc` command surface and the
automatic sweeper that runs alongside builds.

| File | Responsibility |
|---|---|
| `mod.rs` | Module wiring and the `gc` command entry points |
| `auto.rs` | The automatic sweeper — throttling, deferral while a build is active, the maintenance lease, and the tiered passes |
| `purge.rs` | Explicit `soldr gc purge` (`--all`, `--older-than`, `--larger-than`) |
| `rustup_toolchain.rs` | `gc purge --kind rustup_toolchain` (soldr#3507) — enumerates the caller + managed rustup homes, protects the active/default/pin toolchains, and delegates deletion to `rustup toolchain uninstall` |. `--unused-days N` keeps toolchains used within N days (stamps from `toolchain_store.rs`, soldr#4027)
| `disk.rs` | Free-space probing that decides which pressure tier applies |
| `cargo_native.rs` | `cargo`-native GC (`registry/src`, git checkouts) and `gc sweep` |
| `target_walker.rs`, `walks.rs` | Directory traversal used to size and prune `target/` trees |
| `discovery.rs` | Bridges the `target_walker` into registry-shaped rows so auto-GC sees targets the registry never recorded |
| `tests.rs` | Shared unit tests for the module |

## Auto-GC ordering

`run_auto_gc_background` defers to an active build before doing anything —
competing with `cargo` for IO and the `.package-cache` mutex is worse than
sweeping late. It then takes a maintenance lease so two sweepers never run
together.

Tier-0 work is unconditional and bounded: it runs even when the volume is above
the pressure trigger, so nothing can grow without limit between firings.
Anything gated behind a tier is best-effort by design.

## Scratch reclamation

`sweep_stale_scratch` reclaims aged entries from the scratch root
(`core::temp`). Scratch deliberately lives *beside* the cache rather than inside
it — same filesystem, so `temp` → `cache` renames stay atomic, but outside
`<cache>/**` so cache maintenance cannot reach into it. That isolation is why
this sweep has to exist: no other pass will ever see those files.

It is deliberately **not** gated on the state DB existing. A machine whose DB
was never created is exactly where scratch accumulates unnoticed.
