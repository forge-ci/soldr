# Daemon timeout and stall runbook

Cacheable compiler work has one mandatory route:

`Soldr front door → singleton broker → root/version daemon route → embedded zccache`

The front door registers the exact daemon image and cache root. Only the
broker places and starts the daemon. A wrapper re-entry never starts a broker
or daemon, and an infrastructure failure never silently switches to another
compiler path.

## First checks

Use these read-only commands before changing process state:

- `soldr doctor` shows effective timeouts and configuration.
- `soldr status` shows cache and daemon state.
- `soldr daemon status` checks the selected root's daemon.
- `soldr logs paths` prints the broker, daemon, build, and compile logs.

A long compile prints a progressive heartbeat rather than remaining silent.
The heartbeat names the operation, elapsed time, deadline, and the active
timeout override.

**Looks like a hang → check for a re-entrancy diagnostic first** (soldr#2566,
default-on since soldr#2739). Months of "hang" incidents were actually
unsanctioned `soldr -> tool -> soldr` re-entry multiplying startup work per
build unit. Enforcement now needs no opt-in anywhere: such an entry exits 1
immediately and prints a bounded stderr diagnostic naming both processes
(`inherited IN_SOLDR_PID=..., this pid=...`), the argv head, and the routing
variables — grep the failing step's stderr for `unsanctioned Soldr
re-entrancy`.

A marker whose writing process has already exited is ignored, so a
Soldr-spawned build script that outlives its parent and re-enters Soldr is not
reported as re-entrancy.

`SOLDR_REENTRANCY_GUARD=off` disables the check. It is emergency-only — for
unblocking a false positive while you report it, not a supported
configuration, and it should never be committed to a workflow. Any other
value is a hard error rather than a silent fallback, so a typo cannot quietly
disable the check. With the guard off the same shape presents as silence;
suspect it whenever a "hang" reproduces only under nesting.

## Timeout surface

| Bound | Default | Override | Notes |
|---|---:|---|---|
| Broker front-door readiness | 5 s | — | Active control + SESSION socket probes; log text is not readiness |
| Broker daemon-route acquisition | 120 s | `SOLDR_ROUTE_ACQUIRE_CEILING_MS` | The outer route ceiling enforced by `BrokerDeadlines`; the launcher uses a 45 s readiness window inside it and watches early exit |
| Explicit `soldr daemon start` caller safeguard | 180 s | — | An enclosing lifecycle-command wait for image staging, spawn, and readiness; negotiated route acquisition still stops at `BrokerDeadlines::route_ceiling` (120 s by default, configurable with `SOLDR_ROUTE_ACQUIRE_CEILING_MS`) |
| Status / shutdown reply | 2 s | — | Health handshakes should be immediate |
| Cache flush reply | 5 min | — | Large index/LTO flushes may be slow |
| Compile reply | 30 min | `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` | Shorten for diagnostic fail-fast behavior |
| Graceful shutdown wait | 5 min | — | Allows in-flight work and persistent state to drain |
| Daemon shutdown watchdog | 240 s | `SOLDR_SHUTDOWN_WATCHDOG_SECS` | Process exits regardless once graceful teardown (idle timeout, owner death, `soldr daemon stop`, SIGINT) runs this long; SIGTERM takes a fast-exit path and bypasses it. Sized under the 5 min graceful shutdown wait. Zero cannot disable it (soldr#3648) |
| Displacement drain | 120 s | `SOLDR_DISPLACEMENT_DRAIN_TIMEOUT_SECS` | Wall-clock budget for an acknowledged stale-daemon displacement drain before the kill fallback engages |
| Cache shutdown | 5 min | `SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS` | End-of-command embedded-cache drain |

Malformed, empty, or zero timeout overrides use the documented default; they
do not remove the backstop. `soldr doctor` reports effective provenance.

## Failure modes

### Broker is unreachable

Signal: the wrapper reports that the singleton broker SESSION socket is not
available and tells you to use a Soldr build front door.

Confirm that the invocation began at `soldr cargo ...`, `soldr build`, or
another compile-capable Soldr surface. A raw compiler-wrapper invocation is
not authorized to create infrastructure.

Inspect the broker spawn log reported by `soldr logs paths`. The front door
uses active socket probes and a bounded wait; stale log lines cannot satisfy
readiness.

### Broker image or version mismatch (soldr#2549)

Signal: an ordinary invocation prints, once per broker generation
(soldr#3985; recorded in `<broker dir>/broker-skew-warned`)

```
soldr: broker is <version>-<digest8> (this is <version>-<digest8>); work continues; retire with `soldr broker remove`
```

Under `SOLDR_PORCELAIN=1` nothing is printed; the JSON line carries a
`broker_skew` object (`running_broker`, `this_soldr`, `remedy`) instead.

This is a diagnostic, not a failure. The broker is a stable, long-lived
singleton for its user-home endpoint: Soldr never stops, kills, replaces, or
stages over a live broker because the running image's package version or
digest differs. Work continues through the running broker, and the running
image still gets a closely aligned daemon — the route's service name is keyed
on the daemon image hash, so the stable broker launches or adopts a matching
daemon generation behind itself while the prior daemon drains and expires
under daemon lifecycle policy (`displace_stale_daemon`).

Recovery is explicit and operator-driven:

```
soldr broker remove
```

That stops the PID-verified broker, unlinks its admission endpoint, and
deletes the staged broker image, so the next invocation installs a matching
one. Daemon routes are retained and re-adopted from their verified claims.
Use `soldr broker stop` instead when you only want to cycle the broker
process and keep its staged image.

### Brokers for other HOMEs pile up

Signal: `soldr doctor` lists processes under `soldr processes for other
HOMEs:`, or an interactive invocation prints `soldr: N leaked soldr-broker
... process(es) are running for other HOMEs` (soldr#3193). Test suites that
run soldr under a throwaway `HOME` per test leave one broker each; a broker
whose `HOME` directory survives is never stood down by the image watch.

```
soldr broker purge --dry-run   # list them
soldr broker purge             # terminate, force-kill after 5 s
```

The broker for the current `HOME` is never touched. Brokers built from this
version onward also stand themselves down after `SOLDR_BROKER_IDLE_EXIT_SECS`
(30 min) with no connection and no route, and callers that never need a
daemon can set `SOLDR_BROKER_AUTOSPAWN=0`.

### Daemon route does not become ready

Signal: the broker cannot provide the registered service route within the
bounded acquisition window.

Inspect the broker and daemon spawn logs. They include the selected root,
route service name, registered image, and early child-exit reason. Do not
start a daemon manually: the broker is the only placement/spawn owner.

### Compile initialization is slow

The daemon binds its broker-facing endpoint and answers BackendHandle probes
before embedded zccache and database initialization complete. The first real
SESSION compile awaits compile-service readiness just in time. This keeps
startup probes responsive without allowing a compile to race initialization.

If the compile-reply heartbeat appears, use a smaller positive
`SOLDR_COMPILE_REPLY_TIMEOUT_SECS` for the next diagnostic run so it fails
quickly with evidence rather than waiting for the production backstop.

### Daemon is wedged

Run `soldr daemon stop`, wait for its bounded graceful shutdown, then run
`soldr daemon start`. Explicit start re-registers the current daemon image
and asks the singleton broker to create the route.

Do not delete PID files, sockets, or `state.sqlite3`; those are ownership and
forensic records, not recovery switches.

### Distinct cache roots interfere

This is a correctness bug. One user-session broker must route distinct
canonical cache roots/version/image identities to distinct daemon services.
Capture `soldr logs paths`, both roots, route names, PIDs, and socket paths.
Do not work around it by starting one broker per root.

## Recovery checklist

1. Inspect `soldr doctor`, `soldr daemon status`, and `soldr logs paths`.
2. Shorten the compile-reply timeout only when a bounded diagnostic failure is
   preferable to the normal long-build allowance.
3. Restart the selected broker-owned route with `soldr daemon stop` followed
   by `soldr daemon start`.
4. Only when the front door reports a broker image/version mismatch and you
   want that broker gone, run `soldr broker remove`. Nothing in Soldr takes
   this step for you.
5. Re-run through a compile-capable Soldr front door.

All broker, daemon, transport, version-skew, retirement, initialization, and
protocol failures are hard failures for cacheable compiler work. There is no
rollout gate and no direct-daemon or direct-compiler fallback.

## `broker SESSION compile failed: Connection reset by peer` with every process alive (soldr#3102)

Signature: a wrapper fails with `Connection reset by peer (os error 104)`,
`daemon-spawn.log` ends with `rejected broker connection handoff: Broken pipe
(os error 32)` and shows no panic or `SIGTERM received`, and the daemon still
answers `soldr cache shutdown` at the end of the job. Nothing died: the broker
gave the daemon 5 s (`DEFAULT_HANDOFF_ACK_DEADLINE`) to acknowledge a passed
connection, the acknowledgement was late, and the broker relinquished the
connection (`~/.soldr/broker/broker-spawn.log`: `handoff fallback: abandoned
at AwaitAck stage`). The daemon then hit `EPIPE` writing the ACK and closed its
copy, which is what the wrapper saw as a reset.

Since soldr#3102 the daemon acknowledges handoffs on a dedicated control-plane
runtime, so compile-runtime workers parked in the embedded cache's synchronous
hit path (store-lock waits, artifact materialization) no longer delay the ACK.
If the signature reappears, the broker log above says which side was late.

