# Wasm-driven viewport screenshot (in progress)

This private example is the application requested by #20. Its business logic
is in `guest/src/main.rs`, compiled as one Rust 1.95.0
`wasm32-wasip1-threads` module. The guest receives opaque URL and exact-output
grants, then performs open → matching load completion → five-second kernel
sleep → native viewport capture → opaque output write → close. It neither
receives a URL/path string nor reads PNG bytes into guest memory.

Guest host effects now use the public `kernal_api::guest` surface: `WebviewUrl`,
`OutputFile`, `sleep`, and the capture-only `Webview` methods. Generated ABI
types remain private inside the facade. Successful `OutputFile::write_blob`
consumes both host authorities, so the guest does not close them again. The
method borrows its handles so preflight failure does not discard the caller's
ability to seal, retry, or close a blob.
Before its first host grant, the artifact runs two actual Wasm guest threads.
They rendezvous through a standard `Mutex`/`Condvar`, exchange bounded
Crossbeam messages, and concurrently update a `DashMap`; both are joined before
the screenshot lifecycle starts. The map uses a deterministic standard hasher
and the channel reads are non-blocking after join: the closed profile therefore
adds neither ambient `random_get` nor `poll_oneoff` imports.

The source fixture uses an exact `=0.1.26` version plus a **migration-only local
path**; it must switch to an actually published guest-capable release before
release acceptance. The packaged facade has separately passed a Wasm check,
but this is not evidence that the guest-capable package has been published.

## Current acceptance stage

The library's `SketchWorkerConfig::with_webview_capture` now runs this real
guest in a separate contained worker on Linux. The focused
`wasm_tauri_screenshot` tests matching `inside_containment` prove successful
pixel capture, trap-after-capture output preservation, and cooperative
cancellation during an unfinished page load when built with
`wasm-sketch-worker,tauri-webview-test-support`. They use the same normal/trap
artifact environment variables documented below. Windows/macOS native worker
execution and exhaustive renderer-destruction evidence
remain pending. Linux also passes the forced-deadline proof described below.
The cancellation proof requires `Stopped(Cancelled)`, not forced termination,
and checks transported worker cleanup plus parent counters and unchanged output.

`native_capture_quota_failure` runs the same guest against the fixed-seed noise
fixture in `fixtures/over-budget.html`. The real native PNG encoder exceeds its
unchanged 1 MiB bound. The test requires capture-rejected (exit 97), the bounded
`capture-encoded-byte-limit` trace marker, no output-write submission, preserved
files, and zero worker/native-resource counters without force. This Linux proof
does not substitute for the remaining native Windows/macOS failure matrix.

For the containment-only blocking variant, build with
`build_guest.py --block-after-capture` and
set `KERNAL_API_SCREENSHOT_BLOCK_ARTIFACT_WASM` to the admitted artifact under
`kernal-api-wasm-tauri-guest-block`. This guest waits on an unnotified condition
variable after receiving the native snapshot. Never run that variant in the
`--diagnostic-in-process` mode: the `block_inside_containment` test exercises the worker's
20-second deadline and forced-reap path. Post-capture fault variants are
source-only acceptance fixtures and are disabled in the normal build.

Commit `52d4a3b` preserves the initial real Rust build failure: the generated
`WebviewUrl`, `run`, and `OperationFuture::wait` interfaces were absent. The
generated contract, native dispatch, and CLI now run the actual sequence.
On Linux x86-64, the checked-in offline CLI proof produced an 800×600,
2,809-byte PNG in about 7.5 seconds, preserved an unrelated file, and left no
temporary output. The portable Rust decoder verifies red and blue regions
at six interior sample points. An initial native run exposed a
redundant guest close after output commit; commit consumes the snapshot and
output grants, so the guest now proceeds directly to closing the view.

This is still an in-progress #20/#21 implementation, not completion of the
full acceptance matrix. Broader failure-path trace coverage,
queued/cancelled native-creation destruction evidence, the complete
failure matrix, renderer-destruction evidence, and native Windows/macOS execution
remain outstanding. The CLI now defaults to the contained worker. Only builds
with `tauri-webview-test-support` accept the explicit `--diagnostic-in-process`
mode used by the detailed timing/failure proofs; that mode is not containment
for arbitrary untrusted Wasm. Worker launch failure never falls back to it.

Build the actual guest (any host, Python 3.11+) with the pinned target installed:

```sh
soldr rustup target add wasm32-wasip1-threads
CARGO_TARGET_DIR="$PWD/target/screenshot-proof" python3 examples/wasm-tauri-screenshot/build_guest.py
CARGO_TARGET_DIR="$PWD/target/screenshot-proof" python3 examples/wasm-tauri-screenshot/build_guest.py --trap-after-capture
```

The build produces one module with generated ABI metadata; metadata embedding
alone does not establish admission or successful execution. The build script
never runs a native replacement for the guest sequence. The focused artifact
test must be selected explicitly:

```sh
KERNAL_API_SCREENSHOT_ARTIFACT_WASM="$PWD/target/screenshot-proof/kernal-api-wasm-tauri-guest/wasm32-wasip1-threads/release/kernal-api-wasm-tauri-guest.admitted.wasm" \
  soldr cargo test --locked --features wasm-sketch-host --test wasm_tauri_screenshot -- --ignored
```

On Windows (PowerShell) the same entry point takes absolute storage:

```powershell
soldr rustup target add wasm32-wasip1-threads
$env:CARGO_TARGET_DIR = Join-Path (Get-Location).Path 'target/screenshot-proof'
python examples/wasm-tauri-screenshot/build_guest.py
$env:KERNAL_API_SCREENSHOT_ARTIFACT_WASM = Join-Path $env:CARGO_TARGET_DIR 'kernal-api-wasm-tauri-guest/wasm32-wasip1-threads/release/kernal-api-wasm-tauri-guest.admitted.wasm'
soldr cargo test --locked --features wasm-sketch-host --test wasm_tauri_screenshot -- --ignored
```

The script requires the pinned Wasm target to be installed, preserves the
caller's build storage, and fails on build/metadata errors. It runs
every tool in a child process, so the caller's working directory and `SOLDR_LINKER`
setting are never changed.
Building a guest this way does not prove native Windows webview execution.
The existing Windows threaded-artifact CI lane invokes this script and the
host-only admission test without enabling GUI dependencies. The script's
actual build and caller-state restoration have been checked with PowerShell
on Linux; native Windows validation remains separate.

That test checks closed-profile admission and rejection without webview grants,
not successful native capture. It also mutates the real artifact's import
namespace and requires pre-compilation rejection. The original admission
check required every compatibility import used by the threaded smoke guest;
this smaller program exposed that mismatch. Admission now accepts a subset
of the same exact name/signature allowlist, while requiring a complete
generated lifecycle (or the legacy kernel-yield boundary). No new import or
ambient authority was allowed by this change.

To run the real offline native CLI regression, supply the same artifact and
enable both capabilities on a host with a working native display. This Bash
example uses `jq` to resolve the test executable after Soldr exits:

```sh
set -o pipefail
test_executable="$(soldr cargo test --locked --features wasm-sketch-worker,tauri-webview-test-support --test wasm_tauri_screenshot --no-run --message-format=json |
  jq -er -s '[.[] | select(.reason == "compiler-artifact" and .target.name == "wasm_tauri_screenshot" and .profile.test == true and .executable != null)] | if length == 1 then .[0].executable else error("expected exactly one screenshot test executable") end')"
KERNAL_API_SCREENSHOT_ARTIFACT_WASM="$PWD/target/screenshot-proof/kernal-api-wasm-tauri-guest/wasm32-wasip1-threads/release/kernal-api-wasm-tauri-guest.admitted.wasm" \
KERNAL_API_SCREENSHOT_TRAP_ARTIFACT_WASM="$PWD/target/screenshot-proof/kernal-api-wasm-tauri-guest-trap/wasm32-wasip1-threads/release/kernal-api-wasm-tauri-guest.admitted.wasm" \
KERNAL_API_SCREENSHOT_BLOCK_ARTIFACT_WASM="$PWD/target/screenshot-proof/kernal-api-wasm-tauri-guest-block/wasm32-wasip1-threads/release/kernal-api-wasm-tauri-guest.admitted.wasm" \
  "$test_executable" --ignored --nocapture --test-threads=1
```

The native CLI tests launch the executable as a separate process, serve the checked-in
fixture over loopback, verify successful guest completion, decode the PNG
with portable Rust tooling, check bounded dimensions and tolerant red/blue
regions at six interior points, and check exact-output replacement and
temporary-file cleanup. A bounded host trace records generated submit/poll/yield
crossings, preserves the matching native load callback's monotonic timestamp,
and requires capture at least five seconds later. After joined cleanup and
dropping the admitted sketch, the proof requires zero native admission,
backings, hub resources/operations/transfer bytes, clock/output jobs, Wasm
roots/threads/stores/instances, epoch registrations, and memory reservations.
An additional non-ignored CLI test rejects malformed URLs, credentials, and
file/data/javascript/tauri schemes before module loading. It needs no display
and verifies unchanged output/sentinel files, with a valid-URL negative control
that reaches the separate missing-module error.
The recorder holds at most 512 events; any omitted event fails the proof.
The actual-guest redirect proof supplies an HTTP redirect to a prohibited
scheme, requires `screenshot-load-rejected`, preserves the original output and
unrelated file, and applies the same zero-resource teardown assertions.
The timeout proof holds the real HTTP request open for the production
30-second load deadline, requires `screenshot-load-timed-out`, forbids capture,
and applies the same output-preservation and teardown checks.
Both load-failure proofs now run through the default contained CLI and assert
transported worker traces plus zero parent worker/task/lease counters.
The write-failure proof relocates the fixture-owned output directory only
after the guest's HTTP request, so grants and native execution have already
started. The guest completes real capture before its output job encounters
the now-missing parent. The proof requires `screenshot-write-rejected`, no
recreated parent or sibling temporary file, unchanged preserved files, and
zero captured blobs/output jobs/native/Wasm resources. This is a missing-parent
I/O failure in explicit diagnostic mode, not yet a contained permissions or
disk-exhaustion proof.
The direct contained renamed-parent proof now performs the same relocation
with the real worker. It requires write rejection (guest exit 113), drained
worker and parent counters, preserved original files, and removal of staging
from the moved directory through the parent's retained directory handle.
The default contained publication-failure proof instead preserves the original
file and places a nonempty directory at its final path after grants are installed.
The guest completes real capture and staging; the parent reports
`worker-output-commit`, removes staging, preserves the fixture-owned files and
obstruction, and reports zero worker/root resources. This covers real atomic
replacement failure without introducing a fake capture or filesystem backend.
The trap proof compiles the same actual guest with the explicit
`proof-trap-after-capture` feature. It traps only after the native capture
result returns through the generated ABI, before output commit. The test
requires a genuine Wasm `trapped` result, no output-commit submission, unchanged
files, and zero resources after teardown. The scripts select separate `-trap`
storage with `--trap-after-capture` (Bash) or `-TrapAfterCapture` (PowerShell),
so this variant never overwrites the normal guest artifact. It is a source-level
fault injection, not a fake native capture or replacement guest workflow.
These observations do not establish the remaining failure matrix.
Run under Xvfb on headless
Linux, using the environment in `docs/tauri-external-content-isolation.md`.

The dedicated `wasm-tauri-screenshot-native` CI matrix builds the actual guest
and runs the native proofs on six explicitly matched Rust hosts. Linux uses
WebKitGTK 4.1 under Xvfb; macOS uses WKWebView; Windows checks/installs the
Evergreen WebView2 Runtime. The configured runner/target pairs are:

| Runner | Native Rust target |
| --- | --- |
| `ubuntu-24.04` | `x86_64-unknown-linux-gnu` |
| `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` |
| `macos-15-intel` | `x86_64-apple-darwin` |
| `macos-15` | `aarch64-apple-darwin` |
| `windows-2025` | `x86_64-pc-windows-msvc` |
| `windows-11-arm` | `aarch64-pc-windows-msvc` |

These labels follow the [GitHub runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
Windows setup follows [Microsoft's Runtime detection/deployment procedure](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution),
verifies the downloaded installer's signature, and records the installed version.
The matrix fails a Rust-host mismatch instead of counting emulation or a
cross-build as native proof. Native harness builds omit `--target` so native
GUI libraries use the host build setup rather than an explicit-target sysroot.
**The five additional native runs are not yet
verified; configured jobs are not acceptance evidence.** Set
`KERNAL_API_SCREENSHOT_PROOF_DIR` to retain a unique diagnostic directory
containing runner stdout/stderr, process outcome/timing JSON, and the exact
output directory even if an assertion fails. CI uploads these artifacts on
success or failure. `runner.stderr.log` includes structured
`kernal-webview-trace` records; `process.json` separately summarizes the child
process. With the test feature enabled, the default contained CLI receives one
64-KiB-capped trace batch through private protocol version 6; its success and
trap tests check the same detailed timing/resource assertions as diagnostic mode.
Rebuild the worker and CLI together when the private protocol changes.
Direct containment proofs also retain unique `contained-<scenario>-*` folders
under the same proof directory. Their `process.json` records OS/architecture,
terminal category, guest exit status, parent counters, and worker-trace
availability before assertions run. `worker.trace` exists only when the worker
delivered it; `output/` retains the final file and neighbor. A `validation.txt`
marker is written only after all assertions pass. In particular, forced
termination records a missing trace rather than fabricated zero worker counters.
Hard termination can prevent the bounded trace from being emitted,
so failure-path/containment diagnostics remain incomplete. Existing local Linux
x86-64 evidence does not establish native execution on the other five targets.

Focused formatting and lint checks:

```sh
soldr cargo fmt --all -- --check
soldr cargo clippy --locked --features wasm-sketch-worker,tauri-webview-test-support --lib --tests --bins -- -D warnings
soldr cargo test --locked --features wasm-sketch-worker,tauri-webview-test-support --test wasm_tauri_screenshot
```

The generated command driver handles only
generated kernel futures: Wasmtime suspends the guest stack at its async host
import, so no guest runtime is created. Foreign futures returning `Pending`
are rejected rather than busy-polled by an alternate scheduler.

Run the native executable from this source checkout:

```sh
soldr cargo build --locked --features wasm-sketch-worker,tauri-webview --bins
./target/debug/kernal-api-wasm-tauri --url http://127.0.0.1:8000/ --output screenshot.png
```

Without `--module`, the CLI builds the checked-in guest through Soldr and
embeds its generated metadata. `--module <artifact>` selects an already-built
diagnostic artifact; either path validates imports before instantiation.
The native-enabled `kernal-wasm-worker` must be beside the CLI executable, or
selected explicitly with `--worker <path>`. Build both with the same features;
the worker independently validates the module and grants. Adjust the executable
path if `CARGO_TARGET_DIR` is set (Windows also uses the `.exe` suffix).
Launch the built executable directly: wrapping the no-`--module` path in
`soldr cargo run` nests Soldr front doors and is rejected by its strict reentry
guard. The contained success proof likewise builds the test harness through
Soldr and runs it after Soldr exits; it omits `--module` to exercise the full
guest build/admission/contained-capture path. CI obtains the exact executable
from Cargo JSON rather than guessing its hashed filename.
URL and exact-output grants are installed before module start. The UI service
gets the root's existing hub and the same runtime, never copied tokens from a
different client hub. Native jobs are bounded and joined; cancelled native
creation and capture retain separate four-request admission until callbacks
release them. Failed root execution reports a semantic host error; full guest
status includes its failing step and semantic operation error. The shared
`status.rs` encodes eight steps and five causes in the already-admitted
`proc_exit` scalar; it introduces no import, path, payload, or logging grant.
For example, missing URL authority is exit 17, a rejected load is exit 65,
and a timed-out load is exit 69.
The CLI uses a finite 120-second whole-root epoch budget, leaving room for
the production 30-second operation deadlines and five-second wait. The
generic compiler default remains 30 seconds; using it unchanged here masked
the load timeout with a whole-root `deadline-exceeded` error.
Actual traps remain `trapped`, distinct from these reported failures. The
generated poll decoder preserves rejected status 7 rather than collapsing it
to a generic failure; status 3 is preserved as `TimedOut`. The rest of the
negative matrix and native Windows/macOS execution remain required.
