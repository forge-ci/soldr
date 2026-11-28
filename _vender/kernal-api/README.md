# kernal-api

`kernal-api` is the shared systems facade for Soldr, zccache, and fbuild. Its
target architecture builds on `running-process`, the trusted low-level
native/process substrate, and adds stable application contracts for async
execution, hashing, diagnostics, profiling, symbolization, allocation,
networking, storage, and other common capabilities. The private
`running-process` phase-1 adapter has landed: this crate depends on the exact
published `running-process` 4.10.21 registry release unconditionally. Like
Tokio, it is a private backend: no `running-process` type appears in this
crate's public API, including the facade-owned placement contract behind the
explicit `independent-spawn` feature.

In the target architecture, applications use `kernal-api`; they do not use
`running-process` or Tokio directly. The permanent dependency direction and
staged migration are defined in [ARCHITECTURE.md](ARCHITECTURE.md).

The name is intentionally spelled **kernal-api**. The spelling is the stable
package and repository identity on crates.io, PyPI, and GitHub.

## Compatibility

- Rust 1.95.0, edition 2021
- Python 3.10 or newer for the PyPI companion package
- Linux, macOS, and Windows
- x86-64 and ARM64
- `kernal_api::async_engine`, backed by exactly Tokio 1.53.1 in this release
- `kernal_api::hash`, with kernel-owned BLAKE3 byte, reader, and file digest
  operations
- `kernal_api::ape`, which runs Cosmopolitan Actually Portable Executables on
  hosts with no APE `binfmt_misc` handler (stock NixOS, containers, macOS)
  through their own embedded loader; every spawn path applies it

The async facade also owns cooperative cancellation plus separate connection
and transfer-progress timeout policies. A connection is bounded by a fixed
deadline; a transfer is bounded by an idle budget that is reset only when the
caller records meaningful progress. Clients therefore do not need to expose
Tokio or choose an unsafe global download timeout.

Consumers must pin the same exact `kernal-api` release while the API is below
1.0. There is no compatibility fallback to the `0.0.0` namespace reservation:
the crates.io copy is yanked and the PyPI copy has the impossible
`Requires-Python: <0` marker.

See [COMPATIBILITY.md](COMPATIBILITY.md) for the client contract and feature
matrix. Client repositories install the [boundary Dylint](DYLINT.md) to reject
direct use of implementation crates owned by this package.

## Rust features

The base crate contains the async process/host facade. Its bounded process
adapter uses `running-process` 4.10.21; that dependency is mandatory, not
feature-gated. Backend types, `running-process` included, remain private.
With `independent-spawn`,
`SpawnMode::Inherited` remains the default; `SpawnMode::Independent` requires
verified native scheduler or already-external broker placement and never
silently falls back. Independent placement is not detachment, privilege
elevation, or an escape from container-wide limits.
Optional features keep consumers from linking tooling they do not use:

- `sqlite` for synchronous, bounded SQLite connection/transaction/query and
  backup mechanics; applications retain schema and SQL. See [SQLite facade](docs/sqlite.md).

- `fs`, `ipc`, `ipc-async`, `session-relay`, `pty`, `conpty-sidecar`
- `conpty-sidecar` fetches the verified Windows 10 ConPTY sidecar through
  WinHTTP; the sidecar code is Windows-only. A consumer that wants it only on
  Windows can enable it under `[target.'cfg(windows)'.dependencies]`, so the
  other hosts skip its `pty`, `tar`, `zstd` and SHA-256 dependencies.
  The download connects directly (no proxy, as before) under one overall
  deadline. Compared with the earlier `ureq` client, TLS roots come from the
  Windows certificate store instead of bundled roots, an HTTPS-to-HTTP
  redirect is refused, a body over the 64 MiB cap is an error rather than a
  truncation, and only HTTP 200 is accepted. Proxied or air-gapped hosts set
  `KERNAL_API_CONPTY_OFFLINE` and pre-stage `conpty.dll`.
- `fs` includes `platform::fs::read_private_regular_file_bounded` for small
  imported markers and credentials. Unix requires an effective-user-owned,
  mode-private regular file below an equally private parent. Windows requires
  a trusted protected owner-and-SYSTEM-DACL parent and, on the opened file,
  current-user ownership plus exactly the private owner-rights-and-SYSTEM
  full-control DACL (direct or inherited). It rejects only a reparse point in
  the final path component and detects replacement of that component where
  supported; it is not a filesystem sandbox. Callers must keep the parent and
  ancestor path trusted and free of replacement races. It reads at most the
  supplied limit plus one byte. The facade hard-caps this whole-value operation
  at 64 MiB; larger artifacts must use a streaming operation.
- `fs` also enables `hash::blake3_tree`: content-authoritative fingerprints of
  glob-selected directory trees, with bounded parallel streaming reads and a
  versioned path/content encoding. Include/exclude rules remain caller policy.
  Defaults admit at most one million selected files, 16 GiB per file, and eight
  readers (limited by host parallelism). Digests ignore absolute roots and
  timestamps; symlinks are skipped and non-UTF-8 regular-file paths fail explicitly.
- `fs` also owns cache-materialization mechanics: `platform::fs::replacement`
  (native atomic replace, generation rename, delete-fallback replace, and
  directory install, with a fixed 50/100/250/500 ms Windows sharing-violation
  retry), `platform::fs::path_file` identity, link classification and counts,
  readonly/execute/mode bits, Windows USN change markers, raw volume identity,
  allocated size, and native path spellings. Materialization policy stays with
  the application.
- `fs-watch` for filesystem-change watcher construction and event
  classification (created/modified/removed/renamed plus an explicit
  overflow-or-lost-watch rescan signal); debouncing, ignore-lists, and
  cache-invalidation policy stay with the application
- `http-client` / `http-client-rustls` for a bounded HTTP/1 client (GET,
  HEAD, POST, pull-based reads, a blocking adapter on the caller's runtime)
  with native TLS or rustls; see [docs/http-client.md](docs/http-client.md).
  **Behaviour change:** the client is now owned on Hyper rather than reqwest.
  It never uses a proxy (reqwest honoured `HTTP_PROXY`/`HTTPS_PROXY`/
  `ALL_PROXY`/`NO_PROXY` silently), and a URL or redirect whose host is
  non-ASCII or percent-encoded is rejected with `InvalidData` instead of being
  IDNA-converted; pass pre-encoded `xn--` hosts. The public API is unchanged
- `snapshot` for cooperative thread capture and deferred unwinding
- `crash` for the one native crash-handler and bounded crash spool
- `profile` for bounded CPU profiles and checked-in pprof encoding
- `allocator` for dormant mimalloc sampling and heap dumps
- `tokio-console` for off-CPU task profiles and runtime diagnostics (the
  backend name is diagnostic metadata, not the application API)
- `symbolize` for the worker wire/client API; `symbolize-worker` builds the
  isolated `kernal-symbolize` parser executable
- `symbolize-split` for post-link debug-symbol splitting: given a linked
  binary it produces a stripped binary plus a complete, matched symbol file,
  reports the mechanism obtained (`gnu-debuglink`, `dsym-bundle`, or the
  `.pdb` the MSVC linker already wrote), and can prove the pair resolves a
  known function through the isolated worker rather than trusting file sizes
- `window-icon` for host-console and child window/stock icons. This is GUI
  hosting: on Linux it decodes PNG and speaks the X11 client protocol. Unlike
  the other optional backends this gates the code and public surface rather
  than the dependency graph: `running-process` declares `png` and `x11rb`
  non-optionally on Linux, so a headless build still resolves them until the
  substrate gates its own copy. **Source break:** `platform::window_icon`,
  `set_window_icon_impl` and `window_icon_support_impl` were available on the
  default feature set before this release and now require `window-icon`
- `wasm-sketch-host` for opt-in core-Wasm sketch admission; the real threaded
  Rust artifact fixture remains source-only under `guests/threaded-smoke`
- `full` for diagnostic executables that need the entire non-daemon surface

## Build-script resources

`build-resources` is a feature of this package, outside `full`, for work that
only an application's own `build.rs` can do.
`kernal_api::build_resources::embed_windows_app_resources` embeds a Windows
executable's icon, version information and Common-Controls v6 manifest; on
every other target it does nothing. Cargo links a build script's resources
only into the binaries of the package that runs the script, so an application
adds `kernal-api` a second time, as a build-dependency with only this feature:

```toml
[dependencies]
kernal-api = { version = "=0.1.37", features = ["window-icon"] }

[build-dependencies]
kernal-api = { version = "=0.1.34", default-features = false, features = ["build-resources"] }
```

Features enabled in the `[dependencies]` entry never reach the build script:
Cargo's edition-2021 resolver resolves build-dependency features separately.
A `kernal-api/<feature>` entry in the application's own `[features]` table is
the exception -- Cargo applies it to every dependency named `kernal-api`, the
build-dependency included -- and the build-dependency cannot be renamed out of
its reach, because Cargo rejects depending on one package under two names.
An application that forwards a heavy capability either accepts compiling it
for the host build script or forwards through an application-owned library
crate (`viewer = ["dep:app-viewer"]`). Either way the build script compiles
this crate's mandatory dependencies for the host. It is a host-only
facility, so CI's per-target build matrix links every feature except it
(`ci/target_features.py`) and covers it through that fixture instead. See the
[`build_resources` module documentation](src/build_resources.rs) and
`tests/build-resources-consumer`.

The four daemon slices are deliberately outside `full`, because each one
carries a frozen wire that only an application already speaking it should
compile. They are documented on docs.rs but must be enabled by name:

- `independent-spawn` for the facade-owned scheduler/broker resource-placement
  contract; its options, launch payload, and live handle convert to the
  selected `running-process` release privately
- `daemon-identity` for direct-daemon identity, sidecar, probe,
  endpoint-mux, and verified-process control semantics over an existing
  endpoint; endpoint naming, payload protocols, and daemon lifecycle stay with
  the application
- `daemon-frame-v1` for the frozen v1 daemon-frame envelope codec alone,
  independent of identity, broker IPC, hashing, and runtime
- `daemon-registration` for the frozen v1 registration records and
  owner-private persistence, excluding endpoint/client policy and identity
- `daemon-registration-v2` for frozen v2 service-definition registration
  alone, so an application dual-writing during the v1-to-v2 rollout pulls no
  broker client, identity, IPC, or runtime policy
- `broker-client` for the broker client adapter: a blocking backend connect
  returning an owned `std::io` stream, plus owned route, refusal-code,
  refusal-kind, and error values for classifying why a broker declined; the
  broker implementation stays in the private substrate
- `broker-v2` for the broker v2 daemon lifecycle: a daemon names an
  owner-private backend endpoint, recovers a broker-assigned launch, and
  publishes its identity where the v2 broker resolves it; a client connects
  through a bound v2 broker or, with none bound, the published identity

The library never installs a global allocator or subscriber by surprise.
Applications opt in explicitly and can still compile all facilities into one
final executable without allocator, crash-handler, pprof-schema, or Tokio
Console version collisions.

Process-to-process and durable machine-readable contracts use protobuf with
fixed field numbers. JSON is reserved for human/tool export formats such as a
Firefox profile; it is not an IPC control protocol. See
[PROTOCOLS.md](PROTOCOLS.md) for the wire-format rules and the deliberately
signal-safe crash-journal exception.

## Compile-resource ownership

First-party clients compile through Soldr. Its default native-cache route
wraps both Rust compilation and `cc`/`c++` build-script work, so the
facade-owned native sources share zccache and Soldr's oversized-unit resource
gate. Large or known amalgamated C/C++ files and the published zccache Rust
amalgamation receive exclusive compile admission instead of competing with a
full set of ordinary compiler children.

That scheduler protection is separate from this crate's API boundary. The
boundary Dylint prevents clients from adding their own copies of the runtime,
allocator, profiler, symbolizer, and OS-HAL implementation dependencies,
including direct use of `running-process` after the relevant facade is ready.
It does not rename or combine third-party source files.

## License

BSD 3-Clause, matching the platform implementation from running-process.

### Desktop status area

The optional `system-tray` feature exposes `system_tray::{TrayOptions, TrayHandle,
TrayEvent}` without exporting toolkit or D-Bus types. Register from an existing
`async_engine` runtime; registration and status updates are bounded. Linux uses
the KDE/freedesktop StatusNotifierItem protocol. Windows and macOS currently
return `TrayError::Unsupported`, so callers retain their existing status window.
Keep a visible fallback whenever `is_online()` becomes false; a returning desktop
host reclaims the same item. Activation/menu events use a bounded queue, and
dropping the handle unregisters the item. The icon remains `Active` while idle so
the desktop can keep it visible; user tray visibility preferences still apply.
