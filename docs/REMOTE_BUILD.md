# Remote build prototype

Install the same source-built Soldr on the client and runner. Use a runner
account dedicated to trusted builds. Cargo build scripts and tests run with
that account's permissions. Both endpoints bind to loopback and the SSH
tunnel authenticates and encrypts the connection between machines.

Generate a shared token on the client with a restrictive file mode and
copy the file to the runner via SSH. Use the corresponding token path on
each machine.

```sh
(umask 077 && openssl rand -hex 32 > "$HOME/.soldr-remote-token")
scp "$HOME/.soldr-remote-token" build-runner:.soldr-remote-token
```

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
soldr remote --token-file "$HOME/.soldr-remote-token" run build -- \
  --locked --manifest-path examples/remote-build-demo/Cargo.toml
soldr remote --token-file "$HOME/.soldr-remote-token" fetch --out target/remote-demo SESSION_ID \
  'target/debug/soldr-remote-demo' 'attestation.json'
```

Use the session ID printed by `remote run`. The same command accepts
`test`, `dylint`, or `lint` in place of `build` and forwards arguments to that
Soldr command from the repository root. To export the runner's Soldr cache,
pass `--cache-zstd-level 3` before the action and include `cache.tar.zst`
among the fetch globs. The runner serializes builds and cache exports
against its dedicated compiler cache. The fetch destination must be a new
directory with an existing parent. Artifact paths retain their relative layout.
An explicit Cargo `--target-dir` is relative to the session's fetchable
output directory. For example, `--target-dir target/special` makes
`target/special/**` available for fetch.

Fetch uses one HTTP/2 connection and up to eight concurrent file streams.
The runner can advertise a lower limit with `remote serve --artifact-streams N`.
Each stream sends binary Zstandard at level 15. The client verifies the
uncompressed size and SHA-256 before publishing a file.

To reuse the build connection for downloads, request artifacts on the run
command. Downloads start after the runner reports the build result.

```sh
soldr remote --token-file "$HOME/.soldr-remote-token" run \
  --fetch-out target/remote-demo \
  --fetch 'target/debug/soldr-remote-demo' --fetch attestation.json \
  build -- --locked --manifest-path examples/remote-build-demo/Cargo.toml
```

The client requires a commit and a clean Git status, including untracked
files. Ignored build outputs stay local. The client reads sources from the
committed Git tree. The runner
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
that user and repository, and finished session artifacts are reflinked into
their own fetch directory. A client disconnect leaves the build running
until its child exits or times out, so a slot cannot be reused while a
detached build is writing to it. Build execution remains serialized.

For a manifested session, `lint` defaults to `soldr lint rust --check`.
After a successful full Rust lint, the runner attempts a separate
`soldr cook` and stores a content-addressed archive under `cook/` in its
state root. The attestation reports `cook_ready` and `cook_archive_sha256`.
Fetch the archive as `cook.tar.zst` from the completed lint session.
The same dependency recipe reuses its archive on subsequent requests in
that slot. The runner materializes a committed source snapshot without Git
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
