# soldr agent

Host phone-home is a library linked into the single `soldr` executable.
`soldr-agent` is a materialized argv[0] alias for `soldr agent run`;
`soldr_cli::host_agent_cmd::materialize_alias()` creates it from Soldr itself.
No additional executable is compiled or shipped.

Create an enrollment key with `soldr agent keygen --key /var/lib/soldr/host.key`.
The parent directory must exist, belong to the service account and be private.
The command prints only the base64 public key and never replaces an existing key.
Private keys are raw 32-byte Ed25519 seeds, atomically published with mode 0600.
`host_identity::load_or_create(path) -> HostKey` is the shared identity API;
`HostKey::iroh_secret_bytes()` supplies the same seed to dist's iroh node.
Wiring dist serve to this API is a follow-up, not a second key generation step.

Run `soldr agent run --config /etc/soldr-agent.toml`, or start the `soldr-agent`
alias under systemd after network-online. SIGTERM/SIGINT stop the loop.
A nonempty systemd invocation environment selects `/var/lib/soldr/host.key`
and `/var/lib/soldr/agent`; interactive defaults use `~/.soldr/host.key` and
`~/.soldr/agent`. Set `key`/`state_dir` explicitly in other service managers.
`--key` overrides the configured key. No systemd unit is installed automatically.

```toml
name = "build-host"
role = "shared" # runner, dist, shared (must match enrollment)
signal_url = "wss://signal.forge-ci.com"
key = "/var/lib/soldr/host.key"
state_dir = "/var/lib/soldr/agent"
soldr_root = "/var/lib/soldr"
health_bind = "0.0.0.0:9099"
units = ["soldr-dist.service"]
host_load_file = "/run/forge/host-load.json" # optional; omit when not used
# Optional trusted executable and arguments; no shell expansion.
# It must print exactly the signal protocol's typed Dist JSON.
# dist_status_cmd = ["/usr/local/bin/soldr", "dist", "status", "--json"]
```

`signal_url` accepts wss/https; plaintext ws is restricted to literal loopback
addresses for tests. The path is `/v1/hosts/connect`. WebSocket messages are
capped at 16 KiB. Handshake has a 5-second deadline, sends have a 2-second
deadline. Heartbeats run every 5 seconds. Reconnects back off 1, 2, 4, 8, 16,
30 seconds with up to 20% downward jitter; each failed cycle emits a warning
naming the URL and error category. Protocol errors omit frame contents.

The LAN-only `/healthz` returns the signal Health JSON (200/503). Never forward
9099 publicly; restrict unspecified binds with the host's LAN firewall. Explicit
public bind addresses are rejected. Unit and dist probes run concurrently with
500 ms deadlines and terminate their process groups, including descendants.
Details include truncated stderr, as requested by Soldr's host diagnostics policy.
Disk checks require more than 5% free on `/` and `soldr_root`. Configured load-file
failure reserves all CPUs and advertises zero dist slots. Invalid/unavailable
dist status omits the node from discovery. Probe arguments/config are trusted
service-user input; no remote request can select a command.

`soldr agent status --json [--state-dir PATH]` reads an atomic protobuf snapshot
and prints state, updated_ms, signal_url and detail. An unexpected process death
can leave a stale connected snapshot: always inspect updated_ms. Connected means
hello was sent; signal has no explicit authentication acknowledgement. The JSON
phone-home types deliberately copy signal's canonical proto crate (provenance in
src/proto.rs and tests/fixtures/signal-proto/README.md).

Linux `/proc` supplies the boot ID, uptime and kernel version. Other hosts fail
with an explicit diagnostic until a platform facts adapter is added. Admin SSH
and iroh forwarding are outside this slice. The existing `soldr agent capabilities`
interface remains available unchanged.
