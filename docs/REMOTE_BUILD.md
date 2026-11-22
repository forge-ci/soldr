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
mkdir -p target
soldr remote --token-file "$HOME/.soldr-remote-token" run --attestation target/attestation.json build -- \
  --locked --manifest-path examples/remote-build-demo/Cargo.toml
soldr remote --token-file "$HOME/.soldr-remote-token" fetch --out target/remote-demo SESSION_ID \
  'target/debug/soldr-remote-demo' 'attestation.json'
```

Use the session ID printed by `remote run`. The same command accepts
`test` or `dylint` in place of `build` and forwards arguments to that
Soldr command from the repository root. To export the runner's Soldr cache,
pass `--cache-zstd-level 3` before the action and include `cache.tar.zst`
among the fetch globs. The runner serializes builds and cache exports
against its dedicated compiler cache. The fetch destination must be a new
directory with an existing parent. Artifact paths retain their relative layout.

The client requires a commit and a clean Git status, including untracked
files. Ignored build outputs stay local. The client reads sources from the
committed Git tree. The runner
asks for SHA-256 blobs missing from its disk store, then materializes each
session in a fresh directory. The HTTP bodies use Zstandard-compressed
Protocol Buffers. A single request or response can carry up to 256 MiB of
decoded data. The prototype supports UTF-8 paths and regular files,
including executable files. Unix runners also support relative symlinks
whose targets contain no parent traversal.

`attestation.json` records the requested commit, source digests, runner,
command, and exit code. It is an unsigned record from the trusted runner.
The CI commit-trailer verification under `ci-attestations.yml` uses a
different format.

When launching the server inside a Bosn job configured with
`bosn ci run --github-proxy`, the runner-side command inherits Bosn's
stand-in `GITHUB_TOKEN`, proxy settings, and CA bundle. Keep the server
inside that job's network and lifetime so GitHub requests stay within
Bosn's per-run allowlist.
