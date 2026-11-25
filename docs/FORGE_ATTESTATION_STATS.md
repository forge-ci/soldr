# Forge CI attestation statistics

Remote build attestations retain schema version 1 and add an optional `timing` object. `timing.version` is `1`. All durations are whole milliseconds measured on the worker's monotonic clock. `started_unix_ms` is the worker's UTC wall-clock timestamp when it began processing the run request.

| Field | Interval |
| --- | --- |
| `lock_ms` | Run request loading and acquisition of the worker session lock |
| `preparation_ms` | Source and output preparation, including any slot wait |
| `command_ms` | Command execution and its action-specific output collection |
| `postprocessing_ms` | Publication preparation, including optional cook and compiler cache export |
| `worker_ms` | Run processing up to attestation serialization |

The timing excludes client submission, uploads, and network transit. The stats event adds `publication_ms` after the worker serializes the attestation and publishes the result. Automatic remote Cargo commands write a separate `target/forge-ci-client-timings/<session>.json` receipt. Explicit remote invocations and remote lint write a sibling `<session>.client-timing` suffix beside the attestation. `attestation_ms` measures through the attestation fetch and includes client work such as discovering the worker and, for relayed tests, running the test executables. `artifact_download_ms` measures subsequent artifact transfer and local installation when applicable. `total_ms` is the entire measured client operation. Client and worker durations come from separate clocks. Existing attestations have neither measurement.

## Run the statistics service

Start a dedicated process on persistent storage. The database parent directory must exist and be writable by the process. Set two distinct random bearer tokens of at least 32 printable ASCII characters in the service environment. Put a TLS reverse proxy in front of the listener for network access.

```sh
soldr attestation serve --listen 127.0.0.1:8788 --database /var/lib/soldr-attestation/stats.sqlite
```

The service uses `SOLDR_ATTESTATION_READ_TOKEN` and `SOLDR_ATTESTATION_WRITE_TOKEN`. A load balancer can probe `GET /healthz`. Workers send `POST /v1/attestations/completions` with the write token. The dashboard uses the read token on `GET /v1/attestations/stats`, which defaults to the previous 24 hours. The optional `start_unix_ms` and `end_unix_ms` parameters form a half-open window by worker start time. The service reports counts and nearest-rank p50 and p95 milliseconds. An empty window has zero counts and null percentiles. The retention interval is 30 days.

For systemd, the following service keeps its database in a dedicated state directory. Install the Soldr binary at the configured path and put the two token assignments in the root-owned environment file. The example port can be changed to an available loopback port. Route only `/v1/attestations/*` from a shared HTTPS proxy; the health endpoint can remain private.

```ini
[Unit]
Description=Soldr attestation statistics
After=network.target

[Service]
DynamicUser=yes
StateDirectory=soldr-attestation
EnvironmentFile=/etc/soldr-attestation/environment
ExecStart=/opt/soldr-attestation/bin/soldr attestation serve --listen 127.0.0.1:8877 --database /var/lib/soldr-attestation/stats.sqlite
Restart=on-failure
NoNewPrivileges=yes

[Install]
WantedBy=multi-user.target
```

On every reporting worker set `SOLDR_ATTESTATION_STATS_ORIGIN` to the service's HTTPS origin, and set `SOLDR_ATTESTATION_WRITE_TOKEN` to the same write token used by the stats service. A literal loopback HTTP address is supported for local testing. The worker keeps up to 4,096 pending completion records under `attestation-stats-outbox` in its remote worker state directory. It retries sends every five seconds. The service deduplicates by worker and session. A full outbox logs the dropped sample and build completion still uses the worker's normal results and attestation. Stats omit source paths, arguments, and source contents.

The worker strips stats credentials from the environment of its build commands. If the worker accepts untrusted builds, isolate them from the worker service with the worker's OS or container boundary. A build that shares the worker's OS identity may be able to inspect its processes or private files.

Set `FORGE_ATTESTATION_STATS_ORIGIN` and `FORGE_ATTESTATION_STATS_READ_TOKEN` on Forge CI. The signed-in dashboard's **Attestations** tab reads that API through the dashboard backend. Verify the route returns an empty window before enabling workers, run a real paired build, then refresh the tab and compare the remote session's timing with the reported counts. Stop the stats service during a test build to confirm the attestation still returns. Restart the service and verify the worker delivers the pending sample once.
