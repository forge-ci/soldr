# Remote build prototype

Install the same source-built Soldr on the client and runner. Use a runner
account dedicated to trusted builds. Cargo build scripts and tests run with
that account's permissions. The HTTP listeners bind to loopback. Use a Soldr
peer connection or an SSH tunnel between machines.

## Forge CI pairing

Run `soldr sign in` and approve the pairing code with GitHub at
`https://forge-ci.com/soldr/pair`. Once Forge CI has a worker configured,
`soldr remote run build` uses the saved account to request a worker grant.
The worker receives that grant over HTTPS and never sees the device credential.

After pairing, builds, Cargo check, Clippy, and `soldr test --no-run` can
select a worker from a Git workspace root. Cargo shorthands and their explicit
`soldr cargo` forms share the same routing. This sends tracked worktree files and nonignored new files to Forge CI. Automatic routing
requires a matching worker host and the default target directory. It supports
common package, feature, profile, and binary selection flags, plus explicit
Rust triples and target aliases. Custom target JSON paths remain local.
Each compile-only action requires worker support. Ordinary tests still run
locally. Soldr uses the local path when worker discovery fails or the
invocation is unsupported.
After the remote build starts, Soldr returns its result and installs the
verified successful artifacts in the local target directory.

Plain `soldr lint` delegates the Rust suite from a supported Git workspace root
after pairing and writes the worker's result to `target/attestation.json`.
When enabled, formatting fixes run locally before the working tree is sent.
If no compatible worker is available, lint runs locally. Repository extra
commands and the explicit `lint rust --attest` evidence workflow run locally.

Run the worker in its dedicated build environment with
`soldr remote serve --forge-key-file /path/to/key --root /path/to/state`.
Put an HTTPS endpoint in front of the loopback listener. Signed sessions
authorize build and artifact requests with the original grant, so the proxy can
open or reuse upstream connections. The worker key must match the service's
configured worker key. A grant can start builds for 15 minutes and access sessions for
four hours after that. Configure the worker origin only once its HTTPS
endpoint is reachable.

## SSH tunnel

Generate a shared token on the client with a restrictive file mode and
copy the file to the runner via SSH. Use the corresponding token path on
each machine.

```sh
(umask 077 && openssl rand -hex 32 > "$HOME/.soldr-remote-token")
scp "$HOME/.soldr-remote-token" build-runner:.soldr-remote-token
```

## Register Soldr with Forge CI

Run `soldr pair` on your computer and leave it running. Soldr opens
[forge-ci.com/soldr/pair](https://forge-ci.com/soldr/pair) with the code filled
in. If the browser does not open, copy the full URL printed in your terminal.
Select **Continue with GitHub** within 30 minutes. The page and terminal
confirm registration.

## Persistent peer connection

On the runner, create a persistent identity and enroll its public ID with the
Forge signaling operator. The operator configures the GitHub user IDs allowed
to use that runner.

```sh
soldr remote peer-id --key "$HOME/.soldr/remote-peer.key"
soldr remote --token-file "$HOME/.soldr-remote-token" serve \
  --root "$HOME/.soldr/remote-build" --peer-key "$HOME/.soldr/remote-peer.key"
```

Start the gateway on the client and leave it running across builds. The first
connection prompts you to open GitHub's device authorization page and enter a
code. Forge checks your GitHub account and grants access for 24 hours, bound to
the two endpoint IDs. The key files stay on their original machines.

```sh
soldr remote connect --key "$HOME/.soldr/remote-peer.key" --runner "$RUNNER_PEER_ID"
```

Both peers reach `https://signal.forge-ci.com` outbound. Iroh establishes an
encrypted, authenticated connection and uses a direct UDP path when available.
The gateway logs when the selected path changes between `direct` and `relay`.
Add `--relay-only` to require the relay path. The gateway keeps the connection
alive between builds and reconnects after a drop. A build interrupted by a
complete connection loss must be run again.

Run `soldr remote --token-file "$HOME/.soldr-remote-token" run lint` from the
client checkout. The gateway forwards the existing HTTP/2 connection,
including logs and artifact requests, through the peer connection. The runner
checks OAuth authorization against the authenticated peer ID, then checks the
build token on every HTTP request.

## SSH connection

On the runner, start Soldr inside the build environment.

```sh
soldr remote --token-file "$HOME/.soldr-remote-token" serve --root "$HOME/.soldr/remote-build"
```

From the client, forward the runner's loopback port.

```sh
ssh -N -L 127.0.0.1:7744:127.0.0.1:7744 build-runner
```

From a clean checkout of this repository, build the standalone example.

```sh
soldr remote --token-file "$HOME/.soldr-remote-token" run \
  --fetch-out target/remote-demo \
  --fetch 'target/debug/soldr-remote-demo' --fetch attestation.json \
  build -- --locked --manifest-path examples/remote-build-demo/Cargo.toml
```

Use the session ID printed by `remote run`. The same command accepts
`test`, `dylint`, or `lint` in place of `build` and forwards arguments to that
Soldr command from the repository root. To export the runner's Soldr cache,
pass `--cache-zstd-level 3` before the action and include `cache.tar.zst`
among the fetch globs. The runner serializes builds and cache exports
against its dedicated compiler cache. The fetch destination must be a new
directory with an existing parent. Give the fetch exclusive use of that
destination path until it completes. Artifact paths retain their relative layout.
An explicit Cargo `--target-dir` is relative to the session's fetchable
output directory. For example, `--target-dir target/special` makes
`target/special/**` available for fetch.

Fetch uses one HTTP/2 connection and up to eight concurrent file streams.
The runner can advertise a lower limit with `remote serve --artifact-streams N`.
Each stream sends binary Zstandard at level 9. The client verifies the
uncompressed size and SHA-256 of every file before publishing the
download directory.

Downloads start after the runner reports the build result. For SSH tunnel
sessions, artifacts stay on the HTTP connection that started the build.
Signed Forge CI sessions can fetch artifacts across connections with their
original grant. The runner stages archives under `artifact-tmp/` and starts a
four-hour countdown at publication. Disconnecting an SSH tunnel releases its
archive as soon as in-flight work lets go of it. The runner also checks
for expired directories left by a crash at startup and once a minute.
Use `--fetch-out` and `--fetch` on the original run to save the files locally.

By default the client requires a commit and a clean Git status, including
untracked files, and reads sources from the committed Git tree. Pass
`--working-tree` to include the current bytes of tracked files and nonignored
new files, including local edits and deletions. Worktree builds require a
base commit and a worker that acknowledges this mode. They report a base
commit rather than claiming the files match that commit. They do not accept a
commit manifest. Ignored, untracked build outputs stay local. The runner
asks for SHA-256 blobs missing from its disk store, then materializes each
session in a fresh directory. The HTTP bodies use Zstandard-compressed
Protocol Buffers. A single request or response can carry up to 256 MiB of
decoded data. The prototype supports UTF-8 paths and regular files,
including executable files. Unix runners also support relative symlinks
whose targets contain no parent traversal.

`run` writes `target/attestation.json` by default. Pass `--attestation`
to choose a different client path. The JSON records the requested commit, source digests, runner,
command, and exit code. It is an unsigned record from the trusted runner.
The CI commit-trailer verification under `ci-attestations.yml` uses a
different format.

### Session manifests and warm slots

Pass `--manifest /path/to/session.json` before the action. Keep this file
outside the worktree or in an ignored directory so the clean-tree check
still applies. With no manifest or a JSON `null`, the runner creates an
anonymous session. Every session records `manifest.json`, which is fetchable
alongside its attestation. A populated manifest has this shape:

```json
{
  "github": {"url": "https://github.com/forge-ci/soldr"},
  "user_id": 1234,
  "parent-commit-hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "parent-branch": "main",
  "last-commit-hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
}
```

Use the exact Git commit and first parent. For a root commit, use `null`
for the parent. The client checks both hashes against Git. The runner
checks the manifest against the request and records the supplied repository
and user for accounting. The shared bearer token grants access to this
trusted runner. The user ID routes builds to a warm slot and does not
authenticate an individual user.

Fetch `stdout-stderr.jsonl` from a completed session for the captured output
and exit status. Each line is recorded as `{"t":0.25,"type":"o","v":"message\n"}`.
`t` is elapsed seconds at capture, `o` means stdout, and `e` means stderr.
The final record contains only `return`, such
as `{"return":0}` for a successful run. The client still prints the raw
streams to the terminal.

A new slot lives at `<root>/github/repo/<owner>/<repo>/<DATE>_<PARENT_BRANCH>_<commit>/`.
`DATE` is the UTC allocation timestamp, and `commit` is the full hash of the
commit used to allocate the slot. `parent-branch` is optional and defaults
to `unknown`. Names such as `release/stable` are escaped into a single path
component, `release%2Fstable`. A warm slot keeps its original path so later
commits can reuse the incremental target tree.

Put the runner root on Btrfs so the blob store, slot worktrees, and artifact
snapshots can share file extents through reflinks. Manifested requests
require cross-directory reflinking within that root. Copies are writable
and isolated. The SQLite ledger `slots.sqlite3` records slot ownership,
session start and finish times, and exit status. Slots are allocated on demand
up to one per 32 GiB of runner volume capacity, rounded down with a minimum
of one and a maximum of 1,024. Set `remote serve --slots N` to override the
calculated count, or `--slots 0` to record manifests without using warm slots.
New slots need five percent of the volume free, with a
5 GiB minimum. An explicit cap uses the 5 GiB floor. At the cap, the least recently used idle slot
gets a new workspace generation when needed by another user. An interrupted run's generation stays
isolated from future users. Retired generations remain on disk for operator
cleanup.

Each run creates fresh source files from the committed blob store at the
slot's stable path. The stable target tree carries incremental state for
that user and repository. Successful runs reflink new or changed files into
the session's fetch directory. An identical successful run can also reuse
files that still match its recorded output inventory. Failed commands still
publish their logs and metadata. A client disconnect leaves the build running
until its child exits or times out, so a slot cannot be reused while a
detached build is writing to it. Build execution remains serialized.

For a manifested session, `lint` defaults to `soldr lint rust --check`.
After a successful full Rust lint, the runner attempts a separate
`soldr cook` and publishes `cook.tar.zst` under the session's temporary output.
The attestation reports `cook_ready` and `cook_archive_sha256`. Request the
archive with `--fetch cook.tar.zst` on the lint run.
The runner materializes a committed source snapshot without Git
history. Use the remote session attestation for this workflow.

When launching the server inside a Bosn job configured with
`bosn ci run --github-proxy`, set `GITHUB_TOKEN` in the server's workflow step
environment to `${{ secrets.GITHUB_TOKEN }}`. Bosn supplies the stand-in token
as that secret and installs its proxy settings and CA bundle in the job
environment. The server passes those settings to the runner-side command.
Keep the server inside that job's network and lifetime. The SSH relay must
reach the job's loopback address so builds can connect to the server.
GitHub API calls using the stand-in token remain subject to Bosn's per-run
allowlist.

### Session protocol

One logical session has a control stream and separate transfer streams.
The control stream announces what to transfer and records the result.
Workspace upload, Zstandard download, and rsync each use their own stream.
A client can request up to eight concurrent downloads, subject to the
runner's advertised limit.

1. **Authenticate.** Use TLS 1.3 with mutual authentication inside the SSH
   tunnel. Generate a fresh self-signed identity for each peer. Exchange
   certificate fingerprints over authenticated SSH and bind the exchange
   to a single connection. Verify the peer fingerprint and its TLS
   proof of possession before negotiating HTTP/2.
2. **Set up.** Send the repository URL, commit lineage, action, and patch
   manifest on the control stream. The manifest identifies base and result
   content by digest. The runner replies with missing digests. Send missing
   content on a Zstandard upload stream, then materialize the workspace
   after verifying the digest of each object. A complete workspace upload
   is the fallback when no usable base is available.
3. **Run.** Start only after setup completes. The runner emits JSON Lines
   with elapsed seconds (`t`), stream code (`type`), and text (`v`).
   `o` denotes stdout, and `e` denotes stderr.
4. **Publish.** After output is published, send a timed return record and
   the artifact index pointer. The index describes immutable files by
   relative path, size, SHA-256, and `mime_type`. A failed build can still publish logs
   and metadata.
5. **Download.** On a Zstandard side stream, the client requests one path
   and receives binary Zstandard at level 9. The client sends
   `Accept-Encoding: zstd`. The file response uses its original
   `Content-Type` and `Content-Encoding: zstd`. On an rsync side stream, the
   peers trade native rsync protocol bytes, using an older local copy as
   the receiver's basis. The runner exposes only the selected files from
   the finished session. Each transfer can fail and be retried independently.

The runner supports the content-addressed setup and upload path today.
Runs stream live output and send missing workspace blobs in batches
compressed at Zstandard level 9. The runner spools logs to disk. Blank
NDJSON lines keep a quiet stream active every 15 seconds. The client
waits 30 seconds for response headers and 60 seconds between body reads.
The finished
session postamble points to `GET /v1/sessions/{id}/artifacts.json`.
Download each file with `GET /v1/sessions/{id}/artifact?path=<relative-path>`.
Mutual TLS, Git patch application, and the rsync side channel are extensions.
