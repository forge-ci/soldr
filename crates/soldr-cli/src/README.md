# soldr-cli source

The `soldr` binary. The CLI itself, plus the trampoline that lets `soldr cargo …`, `soldr cook`, `soldr cache …`, and the daemon helpers share one entrypoint.

See the crate-level [`Cargo.toml`](../Cargo.toml) for the dependency graph and `lib.rs` for the public re-exports.

## Major modules

- `main.rs` — CLI entrypoint; argv parsing and dispatch.
- `cargo_front_door.rs` — `soldr cargo …` subcommand setup, env injection (RUSTC_WRAPPER, CC/CXX, target dir, jobserver).
- `native_cc.rs` — default-on native C/C++ compiler cache wiring (issue #310). `inject_native_cache_env` plumbs `CC="zccache cc"` / `CXX="zccache c++"` into the cargo subprocess unless `SOLDR_NATIVE_CACHE=0`.
- `cache.rs` + `cache_lib/` — cache root layout, `zccache` integration, target prune / auto-GC.
- `daemon/` — `soldr_daemon` IPC server + client + db.
- `wrapper.rs`, `wrapper_target.rs` — `RUSTC_WRAPPER` hot-path.
- `verdict/` — verdict ledger (soldr#3894): per-tool adapters over one key/record/store core; the cargo front door replays a recorded pass. See `docs/VERDICT_LEDGER.md`.
- `trampoline.rs`, `trampoline_workspace.rs` — verb-specific trampolines that intercept e.g. `soldr clippy …`.
- `bin/` — additional binaries that ship in the same crate (`soldr_daemon`).
- `*_tests.rs` — `#[path]`-included `#[cfg(test)] mod tests` files for the adjacent source module.
