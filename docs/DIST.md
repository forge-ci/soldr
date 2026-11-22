# soldr dist — distributed compilation for soldr

> Living design document. Status: **in development** (design v0.1, 2026-11-21).
> Updated as implementation forces changes; see the Decision log at the end.

Name: **dist** (soldr's IncrediBuild-style mode). A host that lends compute is a
**dist node**; a build that offloads compiles is a **dist client**.
Host roles (soldrix / gh-runner host config `forge.role`):
- `runner`  — GitHub Actions runners only (today's behaviour)
- `dist`    — soldr dist worker only
- `shared`  — both; the host arbitrates locally between GH jobs and dist joblets

## Goals (priority order)
1. Zero-effort setup. A client with soldr installed finds workers automatically
   (LAN discovery + signal.forge-ci.com). No VPN, no root, no system daemon on the client.
2. Correctness: a remote compile is byte-for-byte what the local compile would produce
   (same toolchain identity, same inputs). Results go into the LOCAL zccache store exactly
   like a local miss; hits never leave the machine.
3. Transport minimisation: content-addressed everything, workers keep a large persistent CAS,
   clients send only digests the worker lacks; outputs return as digests; pipelining hides latency.
4. Survive worker power loss: leases with heartbeats; a lost worker's joblets are rescheduled
   (another worker or local) with a visible warning, never a silent hang (soldr#3815 rule).

## Connectivity decision: iroh, not Tailscale/Headscale
- iroh (n0-computer, Rust, Apache/MIT): QUIC endpoints addressed by Ed25519 public key,
  NAT hole punching, relay fallback (self-hostable `iroh-relay`), local-network discovery.
  It embeds in soldr-daemon as a library: no TUN device, no root, works in CI containers.
- Tailscale/Headscale would require tailscaled + root/TUN on every client (incl. ephemeral
  CI containers) and a second identity system. Rejected as overkill; revisit only if iroh
  hole punching fails in practice on the home network.
- signal.forge-ci.com = rendezvous/registry (magic-wormhole role): workers register their
  iroh NodeId + relay URL + capacity; clients query it. We self-host an iroh-relay on the
  droplet (relay.forge-ci.com) so no third-party relay is in the path.
- Pairing (trust): workers execute arbitrary code (proc macros, build-script outputs), so a
  worker serves only trusted clients. `soldr dist pair` on the worker prints a short
  wormhole-style code; `soldr dist pair <code>` on the client exchanges and pins public keys
  (rendezvous via signal). Org-level trust via GitHub OIDC (CI) comes later.

## Executable and host-identity policy

Soldr ships one executable, `soldr`. Phone-home is `soldr agent` and compilation
is `soldr dist`; `soldr-dist` remains a library (`autobins = false`), with no binary target.
The host Ed25519 identity is `/var/lib/soldr/host.key`. A node selects that same
identity with `soldr dist serve --key /var/lib/soldr/host.key`; omitting `--key`
preserves `<root>/node.key` (normally `~/.soldr/dist/node.key`).

## Code layout
- zccache: new public **executor hook** on the embedded API at the miss path
  (`handle_compile/pipeline/compile_exec.rs` spawn site). The hook receives a fully resolved
  `CompileJob` (compiler identity, argv, env, cwd, input file set with blake3 digests, declared
  outputs) and returns `{exit_code, stdout, stderr, output files}` or `Declined` (run locally).
  Hashing, depgraph and store_outcome stay local. Remote jobs bypass the local RSS permit.
- soldr: new crate `crates/soldr-dist` (publish = false):
  - `proto/dist.proto` + hand-written prost types (soldr serialization rule; add to
    check_proto_drift PAIRS).
  - `cas` — persistent blake3 CAS (worker: big, LRU by bytes; client: shares zccache's store
    where possible).
  - `transport` — iroh endpoint, ALPN `soldr-dist/0`, one bidirectional stream per joblet,
    separate uni streams for blob upload so large blobs never block control traffic.
  - `worker` — `soldr dist serve`: accepts joblets, materialises a sandbox dir from CAS
    (hardlinks), runs the compiler under the matching toolchain, returns output digests.
  - `client` — implements the zccache executor hook; scheduler picks worker by
    (free slots, CAS affinity, RTT); falls back to local on Declined/timeout/worker loss.
  - `discovery` — LAN (iroh local discovery / mDNS) + signal registry.
- Wire flow per joblet: `Offer{job manifest: digests}` -> worker `Need{missing digests}` ->
  client streams blobs -> worker `Accepted` -> runs -> `Done{exit, stdout, stderr, outputs: digests}`
  -> client fetches outputs it lacks. Speculative: client may push blobs before `Need` for
  small inputs (< 64 KiB total) to save one RTT.
- Toolchain identity: rustc `-vV` commit-hash + host triple; cc by binary blake3 + version.
  Worker materialises matching toolchain via soldr's own toolchain machinery; if unavailable
  -> Declined (client runs locally). Sysroot/toolchain files are never shipped.

## Rustc specifics
- Inputs: crate source files (from zccache depgraph dep-info; first-ever compile of a unit
  runs locally to learn dep-info), `--extern` rlibs/rmetas, proc-macro .so (same host triple
  only), `-L` search dirs referenced, OUT_DIR contents, `include_bytes!`/env deps.
- Paths: remote sandbox mirrors client absolute paths under a sandbox root and uses
  `--remap-path-prefix` consistent with zccache's ZCCACHE_PATH_REMAP=auto so outputs match.
- Linking (bin/test/cdylib final link) stays local in phase 1.

## C/C++ specifics
- Ship source + resolved header closure from zccache's depgraph (scanner + depfile);
  system headers are part of toolchain identity, not shipped.

## Host registry and phone-home (shared with soldrix / GH pool)
Machines can be plugged in anywhere (wired or Wi-Fi, behind NAT) and become usable
immediately. Everything is outbound; nothing listens on the internet.
- **Identity**: each host has an Ed25519 key (`/var/lib/soldr/host.key`), enrolled once
  (public key + name + role in signal's host policy). The same key derives the host's iroh
  NodeId, so the dist node and admin tunnel share the identity.
- **`soldr agent`** (host side, inside the single soldr binary; argv[0] alias `soldr-agent`; every soldrix host has soldr; systemd, starts after network-online): holds one outbound
  WebSocket `wss://signal.forge-ci.com/v1/hosts/connect`; authenticates by signing a
  server challenge; sends `hello` (name, role, ips, boot id, uptime, versions, load, dist
  NodeId/slots) then heartbeats every 5 s; answers server `ping` with fresh local checks;
  reconnects with jittered backoff. Also serves `/healthz` on the LAN only.
- **signal registry** (`/v1/hosts/*`, ledger module): host leases (TTL 15 s; expiry =
  offline, with `offline_since`), `GET /v1/hosts` (admin), `GET /v1/hosts/<name>/healthz`
  (live round trip through the socket, 2 s timeout -> 200 / 503 / 504 offline),
  `GET /v1/dist/nodes` (online dist/shared hosts for dist clients). New `boot_id` on
  reconnect = the host rebooted -> its in-flight dist joblets are declared lost so clients
  reschedule immediately instead of waiting for QUIC timeouts.
- **Admin access without iDRAC/switch**: ssh over iroh (agent forwards an iroh ALPN to
  localhost:22 for enrolled admin keys; `soldr agent ssh-proxy <host>` as ssh ProxyCommand).
- **Wi-Fi caveats**: iDRAC has its own NIC and no Wi-Fi, so out-of-band power/console is
  lost unless its port stays cabled; set BIOS "AC power recovery = On" so a power cycle
  always brings the host back. USB Wi-Fi adapter with in-kernel driver; credentials are a
  root-only secret in the image.
- **Preferred Wi-Fi topology (keeps iDRAC)**: host NIC + iDRAC dedicated port -> small
  switch -> Ethernet port of a mesh satellite / client bridge. Requirements: the bridge must
  pass multiple wired MACs (mesh/4-address/WDS; cheap MAC-NAT repeaters can break DHCP for
  the second device), DHCP reservations for host + iDRAC, BIOS AC-power-recovery On.
  iDRAC stays LAN-only; remote iDRAC access later via soldr agent forwarding (Redfish/web)
  to enrolled admins, available only while some phone-home host on that segment is up.

## Signal registry (signal.forge-ci.com, /v1/dist/*)
- `POST /v1/dist/register` (worker bearer token): node_id, relay_url, role, slots_total,
  slots_free, cpu/mem, soldr build id. Returns lease_ttl (15 s). Heartbeat = re-POST every 5 s.
- `GET /v1/dist/workers` (client auth): live workers (lease not expired).
- `POST /v1/dist/allocate` (later): coordinator assigns joblet batches across workers.
- Worker lease expiry (power loss) -> worker disappears from /workers; clients with in-flight
  joblets on it see QUIC idle timeout (5 s) and reschedule.
- Admin view: forge-ci.com reads `GET /v1/dist/status` (global state).

## Shared-mode internal balancing (host side)
- Host agent exposes `dist.slots_free = cap - gh_jobs_cpu_share - dist_running`; the gh
  pool dispatcher and dist node share one host semaphore file/socket so a dist joblet
  arriving while GH jobs saturate the box is queued/declined locally (client reschedules).

## Fast deploy to the test E640 (bring-up only)
- `gh-runner scripts/dist-deploy.py <host>`: build soldr release binary locally (soldr cargo build
  --release -p soldr-cli), scp to `/var/lib/soldr-dist/bin/soldr.<sha>`, atomically swap
  symlink, `systemctl restart soldr-dist`. No PyPI, no version bump. Target: < 60 s.

## Test ladder
1. Unit: proto round trip, CAS, scheduler.
2. Loopback: client+worker in one process, compile a hello-world rustc unit remotely.
3. bosn/docker: two containers (client, worker), cargo build of a small crate graph, then
   mixed rust + cc (cc crate build script), then corner cases (proc macros, build scripts,
   include_bytes, worker killed mid-job, worker restart, toolchain mismatch).
4. E640 `dell-jw22zc3` (64 threads, 125 GiB) in shared mode: the GH pool dispatcher cap is
   lowered (12 -> ~6) during announced test windows; the host stays in the CI pool.

## Fleet constraints
- Dells open **no inbound/listening ports**. The worker dials out only: signal registry
  over HTTPS + iroh relay; direct paths arise from UDP hole punching (stateful-firewall
  return traffic). iroh bind port is ephemeral.
- Dells are shared CI capacity for four orgs; dist never takes a host out of the pool.

## Implementation status
| Slice | Repo | State |
|---|---|---|
| CompileExecutor hook on the miss path | forge-ci/zccache | in progress |
| `soldr-dist` crate: proto, CAS, iroh, worker/client, `soldr dist` CLI | forge-ci/soldr | in progress |
| `soldr-ui` quick-launch panel (kernal-api gfx) | forge-ci/soldr `ui/soldr-ui` | in progress |
| `/v1/dist/*` registry + iroh-relay | forge-ci/signal.forge-ci.com | planned |
| Admin panel | forge-ci/forge-ci.com | planned |
| Host role module (runner/dist/shared) + fast deploy | forge-ci/gh-runner, soldr `ci/` | planned |
| bosn/docker pilots | soldr | planned |

## One-joblet bring-up implementation

`crates/soldr-dist` deliberately has no dependency on zccache's unreleased
executor hook. `DistJob`/`DistOutcome` are the adapter seam. `workers(&[tickets])`
parses an explicit worker list; discovery and scheduling remain future work.

- iroh 1.5 uses the name `EndpointId` for the Ed25519 NodeId. Tickets are JSON
  `EndpointAddr` configuration strings (quote them when passing `--worker`). The
  job wire is bounded, length-prefixed protobuf over ALPN `soldr-dist/0`; Hello
  version 1 is mandatory. Each connection carries one joblet/control bi-stream,
  independent uni-stream input/output blobs, and an output-delivery acknowledgement.
- `--root` names the Dist directory (default `~/.soldr/dist`). `node.key` is an
  atomically created private 32-byte secret. Trust files are human-edited TOML:
  `peers = ["<node-id>"]`. `soldr dist trust add ID` grants both roles;
  `--role client`/`--role worker` grants only the selected direction. Trust is
  checked on every new authenticated connection, before accepting its job stream.
- CAS is `<root>/cas/<aa>/<blake3>`. Streaming receive verifies digest and size
  before atomic publication. Filesystem access times are the persistent LRU.
  The byte budget evicts unpinned least-recently-used entries after transfers and
  completed jobs. Per-job digest leases protect active inputs and outputs across
  concurrent jobs, including errors/disconnects. Active working sets may
  temporarily exceed the budget; dropping their leases makes them evictable.
  Admission is a worker-local slot semaphore, never a Cargo job cap.
- Sandboxes mirror absolute POSIX paths under `<job>/root/`. Compiler args,
  declared paths are rewritten; semantic environment values stay verbatim.
  Rust worker-runtime environment dependencies and path-dependent environment such as
  `OUT_DIR`/`CPATH` explicitly decline until namespace isolation is available.
  rustc remaps the mirrored
  cwd to `.` to match the `ZCCACHE_PATH_REMAP=auto` workspace convention. The
  caller's local compile must use that same remap; explicit caller workspace
  remaps are preserved (including when cwd is below the workspace root). Linking stays local. Directory
  materialisation is **not a security sandbox**; trusted compilers and proc macros
  execute with the worker account's privileges. Windows path prefixes currently
  fail explicitly instead of being mapped incorrectly.
- Debug `exec` requires explicit `-o`, rustc `--emit=obj`/`--emit=metadata` or cc
  `-c`, scans dep-info locally, and includes explicit `--extern` and `-L` files.
  Response files and debug stdin/stdout paths are refused; the library job API
  supports stdin bytes directly. It is test scaffolding, not a replacement for the
  complete zccache depgraph (especially generated inputs and environment deps).
- Worker compiler commands are locally configured, never supplied as remote
  executable paths. Rust identity is commit-hash + host from Soldr's resolved
  rustc; C identity hashes the resolved compiler binary. A mismatch declines.
- `status.pb` is an atomic protobuf snapshot with PID, role, trust, CAS bytes and
  running count; status refreshes trust/CAS and recognises a dead worker PID.
  SIGTERM/Ctrl-C stops acceptance and grants accepted joblets 30 seconds to drain.
  QUIC has five-second idle/path timeouts and one-second keepalives; connection
  setup has an eight-second deadline. Worker loss visibly declines for local retry.
- Small-input speculation is suppressed after a successful offer to the same
  client instance. `Need` remains authoritative after worker restart/eviction.
  A repeated job with retained CAS inputs transfers zero input blobs.

Bring-up examples:

```sh
soldr dist serve --relay-url https://relay.forge-ci.com --slots 4 --cas-budget 10737418240
soldr dist trust add <peer-node-id> --role client
soldr dist exec --worker '<printed-ticket>' -- rustc --emit=obj hello.rs -o hello.o
soldr dist status --json
```

### Direct LAN bundle bring-up

`--relay none` disables relay transports and ignores an ambient relay URL. With
no relay URL configured, serving also defaults to direct LAN mode. The printed
JSON ticket carries interface IP addresses and ephemeral UDP ports; copy the
whole ticket to the client. The workstation must reach one of those addresses
(the hosts must permit that UDP traffic). This mode needs no DNS, relay, signal
registry, port forwarding, or `online()` wait. Keep explicit HTTPS relay mode
for hosts that require outbound-only connectivity across NAT.

```sh
# Dell, as soldr-dist; CLIENT_ID comes from workstation status --json.
ROOT=/var/lib/soldr-dist/state
soldr dist trust add "$CLIENT_ID" --role client --root "$ROOT"
RUSTUP_TOOLCHAIN=1.98.1 soldr dist serve --root "$ROOT" --relay none \
  --slots 8 --cas-budget 10737418240 --host-load-file /run/forge/host-load.json

# Workstation, after copying the printed WORKER_ID and DIST_TICKET.
CLIENT_ROOT="$PWD/target/dist-client"
soldr dist trust add "$WORKER_ID" --role worker --root "$CLIENT_ROOT"
RUSTUP_TOOLCHAIN=1.98.1 soldr dist exec --root "$CLIENT_ROOT" --relay none \
  --worker "$DIST_TICKET" -- rustc --crate-name dist_smoke --crate-type lib \
  --emit=obj "$PWD/target/dist-smoke/smoke.rs" -o "$PWD/target/dist-smoke/smoke.o"
```

Use the source creation and byte-comparison steps below for a complete smoke
check. A bare node ID has no LAN routing information: direct mode requires a
full ticket. `SOLDR_DIST_PEERS` may contain that ticket instead of `--worker`.

### Static E640 bring-up (no registry)

The existing `serve` command prints the node ID and a JSON dial ticket. `worker`
is an alias for `serve`. Run trust commands as the account that owns `--root`;
pre-create the service state as that account before root polls its status.
Both machines need the same Soldr-resolved rustc commit and host triple.
Set `RELAY_URL` to an operational HTTPS iroh relay; the signal URL is not a relay.

```sh
# Workstation: initialise identity, then give node_id to the node operator.
CLIENT_ROOT="$PWD/target/dist-client"
soldr dist status --root "$CLIENT_ROOT" --json

# Dell: run as the soldr-dist service account, after provisioning its state dir.
ROOT=/var/lib/soldr-dist/state
soldr dist status --root "$ROOT" --json
soldr dist trust add "$CLIENT_ID" --role client --root "$ROOT"
RUSTUP_TOOLCHAIN=1.98.1 soldr dist serve --root "$ROOT" --slots 4 \
  --cas-budget 10737418240 --relay-url "$RELAY_URL" \
  --host-load-file /run/forge/host-load.json

# Workstation: copy the printed node ID and ticket into these variables.
soldr dist trust add "$WORKER_ID" --role worker --root "$CLIENT_ROOT"
SMOKE="$PWD/target/dist-smoke"
mkdir -p "$SMOKE"
printf 'pub fn answer() -> u32 { 42 }\n' > "$SMOKE/smoke.rs"
RUSTUP_TOOLCHAIN=1.98.1 soldr --no-cache rustc --crate-name dist_smoke \
  --crate-type lib --emit=obj --remap-path-prefix="$PWD=." \
  "$SMOKE/smoke.rs" -o "$SMOKE/smoke.o"
cp "$SMOKE/smoke.o" "$SMOKE/local.o"
safe-rm "$SMOKE/smoke.o"
RUSTUP_TOOLCHAIN=1.98.1 soldr dist exec --root "$CLIENT_ROOT" \
  --worker "$DIST_TICKET" -- rustc --crate-name dist_smoke --crate-type lib \
  --emit=obj --remap-path-prefix="$PWD=." "$SMOKE/smoke.rs" -o "$SMOKE/smoke.o"
cmp "$SMOKE/local.o" "$SMOKE/smoke.o"
```

For repeated commands, `SOLDR_DIST_PEERS="$WORKER_ID"` plus
`SOLDR_DIST_RELAY_URL="$RELAY_URL"` replaces `--worker`. This accepts exactly one
static peer, or a full printed ticket (which already includes relay routing).
There is no peer scheduler or registry call. A decline fails the debug command;
it never falls back to a local object compile. The local dep-info probe still
runs as input-discovery scaffolding. Normal `soldr cargo` offload requires the
released zccache hook and a later adapter; this crate builds on zccache 1.15.14.

Direct-only loopback tests disable relay/address lookup and bind localhost;
no rendezvous, DNS discovery or external network is needed by those endpoints.

## Outbound relay and shared-host admission

With `--relay-url` or `SOLDR_DIST_RELAY_URL` (HTTPS), `dist serve`
connects outbound to that configured relay and waits at most ten seconds for
relay readiness before printing its ticket. No public lookup service, default
third-party relay, TCP listener, fixed UDP port or router port mapping is enabled.
Ephemeral UDP sockets carry outbound traffic and hole-punch return packets; the
host firewall continues to reject unsolicited inbound traffic in relay mode. Clients use the
configured relay or the relay embedded in the worker ticket. Direct-only tests
bind localhost with relays disabled. `--relay none`, or no configured relay,
instead serves direct LAN tickets and requires reachable interface UDP ports.

`--host-load-file PATH` consumes a dispatcher-owned JSON snapshot:
`{"gh_jobs_running":2,"gh_cpus_reserved":16,"cap":6,"updated_at":1795290000.0}`.
`updated_at` is Unix epoch seconds. Fresh data grants
`min(--slots, total_cpus - gh_cpus_reserved) - running` free joblets (saturating
at zero). The process's available CPU count supplies `total_cpus`; `cap` and
`gh_jobs_running` are dispatcher telemetry, not CPU quantities. Data older than
30 seconds, missing/malformed files, and implausible future timestamps allow at
most one concurrent joblet. No new joblet is accepted when the resulting free
count is zero. Admission is checked atomically, and reservation changes affect
new work without cancelling accepted compiles. Serving with a host-load file
reports role `shared`; a standalone node reports `dist`.

Standalone `serve` and `exec` processes hold `endpoint.lock` while using their
persistent identity. A second endpoint using the same key would replace the
first endpoint's relay registration, so concurrent CLI roles must use separate
`--root` directories and pin those identities separately. Embedding callers can
reuse an existing iroh Endpoint and give the client a dedicated CAS/trust root.

## Decision log
- 2026-11-21 Name **dist**; host roles `runner` / `dist` / `shared`.
- 2026-11-21 Transport **iroh** over Tailscale/Headscale: no client-side root/TUN, embeds in
  soldr, self-hostable relay; signal.forge-ci.com is the rendezvous (magic-wormhole role).
- 2026-11-21 Remote execution hooks into zccache **after** the cache lookup, so hits never
  leave the client and remote results populate the local store.
- 2026-11-21 Test host `dell-jw22zc3` in shared mode (the Dell pool owner: no host may
  leave CI service; capacity is reserved by lowering the dispatcher cap).
  A worker host is a *dist node*; a remote job is a *joblet*.
- 2026-11-21 Name: **soldr dist** (a nod to distcc), chosen by the owner. Host roles `runner` / `dist` / `shared`. A worker host is a
  *dist node*; a remote job is a *joblet*.

- 2026-11-21 One shared **host registry** on signal (owner approved): phone-home agent
  over outbound WebSocket, live `/healthz` proxy, lease expiry marks power loss; dist node
  discovery is a view over it (`/v1/dist/nodes`), not a separate registry.
- 2026-11-21 **One shipped executable** (owner): every host-side tool is an argv[0] alias of
  `soldr`. The phone-home agent is `soldr agent` (not a separate forge-agent binary): every
  soldrix host has soldr. One host Ed25519 key serves phone-home and the dist node's iroh
  identity. signal's `proto/` crate is canonical; soldr carries a documented copy checked
  against shared JSON fixtures.

- 2026-11-21 Bring-up uses `DistJob`/`DistOutcome` independently of the unreleased
  zccache executor; explicit tickets only keep discovery/scheduling out of this slice.
- 2026-11-21 Wire version 1 uses bounded protobuf control frames, separate blob
  uni-streams and delivery acknowledgements; tickets remain human-portable JSON config.
- 2026-11-21 CAS leases pin active digests so byte-budget LRU can run during traffic;
  only active working sets may temporarily exceed the configured budget.
- 2026-11-21 Mirrored POSIX paths plus workspace remaps match local rustc paths;
  directory materialisation is not process confinement. Unsupported path prefixes decline.
- 2026-11-21 Environment strings stay verbatim; Rust runtime-environment dependencies and
  path-dependent variables such as OUT_DIR decline until namespace isolation exists.
- 2026-11-21 Debug exec uses dep-info plus explicit extern/search files, requires
  object/metadata output and explicit -o, and refuses response files; full depgraphs
  remain the executor hook's responsibility.
- 2026-11-21 Local status is an atomic protobuf snapshot with PID; graceful shutdown
  drains for 30 seconds, and QUIC keepalive/idle timers bound transport loss.

- 2026-11-21 Nodes require an explicit outbound HTTPS relay and ephemeral UDP
  binds; minimal iroh configuration avoids public DNS/relay defaults and router
  port mapping, while direct paths remain authenticated hole-punch return traffic.
- 2026-11-21 Shared-host admission reads the dispatcher snapshot per joblet;
  timestamps are Unix seconds, stale/unreadable data limits concurrency to one,
  and atomic admission prevents racing offers from exceeding the reservation.

- 2026-11-21 Standalone endpoint identity ownership uses a file lease so concurrent
  CLI processes cannot silently replace one another's outbound relay session;
  embedded callers should reuse an Endpoint, and CLI roles can use separate roots.

- 2026-11-21 Compiler probes/scans use Soldr's existing one-hop self-spawn edge,
  consumed at child entry, so strict re-entry remains enabled. Key file mode is
  explicitly 0600 (the generic private-permissions helper is for executables).

- 2026-11-21 Remote Rustc joblets use `soldr --no-cache rustc` with the resolved
  toolchain pinned. This is intentional direct execution after the client's cache
  miss: it avoids recursive executor/cache routing and keeps the compiler owned
  by the joblet for shutdown. Node slots/host-load admission apply to these
  children; no Cargo-wide job cap is set. The absolute-path wrapper surface needs
  a registered front-door route and is not a standalone executor API.

- 2026-11-21 Compiler children preserve the worker's cache, temporary and runtime
  roots along with toolchain homes; client semantic variables cannot replace
  that process context. This retains the repository's nextest root isolation.

- 2026-11-21 The gh-runner service interface accepts `serve --signal-url`
  (`SOLDR_DIST_SIGNAL_URL`, default `https://signal.forge-ci.com`). The node
  stores it in its runtime state and protobuf status snapshot; registration is
  deferred. `status --json --root /var/lib/soldr-dist/state` reads that snapshot
  across user/HOME boundaries and reports its `build_id`: lowercase SHA-256 of
  the serving executable, computed once at startup rather than hashing the
  querying binary or a subsequently deployed replacement. This is separate from
  linker build IDs. Stale/dead or absent snapshots are marked as client status with an empty
  `build_id`, so a stopped node cannot falsely confirm deployment.
- 2026-11-21 Hardened services keep CAS, sandbox directories, snapshots, locks,
  and `node.key` beneath `--root`; serve creates `<root>/tmp` before compiler
  probes for the service's `TMPDIR=<root>/tmp`. No capabilities or writes to
  system directories are required. Fractional Unix `updated_at` values from the
  dispatcher retain the strict 30-second freshness boundary.

- 2026-11-21 The CLI's `dist` Cargo feature is default-on but optional because
  the initial iroh graph adds more than 120 crates. `soldr cargo build -p
  soldr-cli --no-default-features` excludes soldr-dist and its networking stack
  from the CLI build graph. The frozen `dist` verb remains captured and reports
  that the feature is disabled. Iroh defaults remain off; only `tls-ring` is
  enabled (no metrics, portmapper, or discovery feature backends).

- 2026-11-21 Mono-binary policy: soldr-dist remains library-only behind the
  soldr dist command; host phone-home is another command in the same executable.
- 2026-11-21 `serve --key PATH` selects the shared host Ed25519 seed, defaulting
  to `<root>/node.key`. `trust::load_or_create_key` is the isolated adapter for
  future `soldr_agent::host_identity::load_or_create`; no unreleased agent
  dependency is added. The current file format is the existing 32-byte raw seed.
  Existing explicit keys are read without writes or chmod, so the host key can
  live outside the hardened node's writable root. Provision it and grant the
  node service read access before startup; missing external keys fail if their
  parent is read-only, without falling back to a different identity. Newly
  created keys remain atomic and private (0600).

- 2026-11-21 CLI endpoints hold a read-only-open advisory lock on the selected
  key in addition to the root lease. Two roots selecting the same key file cannot silently replace that
  identity's relay registration. Ordinary key reads/signing by phone-home stay
  available; no shared-key permissions or contents are changed by the lease.

- 2026-11-21 Static bring-up accepts one `SOLDR_DIST_PEERS` node ID with an explicit
  HTTPS relay, or a printed ticket; `worker` aliases `serve`. This enables the
  first remote joblet without adding registry discovery or unreleased hook APIs.

- 2026-11-21 Owner-priority LAN bring-up makes relays optional: `--relay none`
  disables relay transports and startup waits, retaining explicit interface
  addresses in tickets. This requires LAN UDP reachability; outbound-only NAT
  hosts still use explicit HTTPS relay mode.
