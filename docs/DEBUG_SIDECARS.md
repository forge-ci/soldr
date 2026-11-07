# Debug-symbol sidecars in release archives

Policy for the debug-symbol files (`.pdb`, `.dSYM/`, `.debug`) built for the
released `soldr` binary. Established by issue #786; extended by soldr#3038
to cover Linux and macOS. The staging implementation lives in
`release-auto.yml` and `.github/scripts/{stage_release_binaries,
release_manifest,package_release_archive}.py`; the sidecar-format classifier
lives in `crates/soldr-cli/src/release_sidecar.rs`.

## Contract

- **Windows MSVC ships its `.pdb` inline**, inside the main
  `soldr-vX.Y.Z-<triple>.tar.zst` archive, recorded in `manifest.json` under
  `soldr.debug_info` as `{ "name", "sha256", "format": "pdb" }`. This is
  unchanged since #786/#782 and the release lane hard-fails if the PDB is
  missing.
- **Linux `.debug` and macOS `.dSYM/` ship as a SEPARATE, opt-in release asset**
  — `soldr-vX.Y.Z-<triple>-symbols.tar.zst` — never inside the main archive,
  and never referenced by `manifest.json`. **This is the load-bearing part of
  the contract, not a style choice**: `manifest.json`'s `soldr.debug_info` is
  read by every `setup-soldr` consumer via the vendored
  `.github/actions/setup-soldr/zccache_contract.py::validate_release_manifest`,
  which hard-rejects any `debug_info` entry whose `format` is not `"pdb"`
  (`unsupported soldr debug_info format for …`). soldr#786's original
  Linux/macOS `collect_debug_info` staging code assumed a `dwp`/`dsym` entry
  there was fine — it was dormant (the release profile emitted no
  split-debug info at all) until soldr#3038 turned `split-debuginfo` on, and
  turning it on through that path would have broken `setup-soldr` for every
  Linux/macOS consumer on the very next release. See "Why not inline" below.
- **An empty `debug_info: []` is a valid state**, not an error: on Linux and
  macOS it is now the *only* state — the field never carries a `debug`/`dsym`
  entry, by design. Consumers that only need the runtime binaries can ignore
  the field entirely.
- **The symbols asset is unmanifested and best-effort.** It carries no
  `manifest.json` of its own; its own sha256 is covered the same way every
  other release asset is, by the top-level
  `soldr-<version>-SHA256SUMS.txt`. A target whose toolchain produced no
  split-debug sidecar (Windows, always) simply has no `-symbols.tar.zst`
  asset for that target — `package_release_archive.py --allow-empty` skips
  packaging it rather than failing the release.

## Per-platform policy

| Platform | Sidecar | Where it ships | Mechanism |
|---|---|---|---|
| Windows MSVC (x64 + ARM64) | `soldr.pdb` | Inline, in the main archive | MSVC always produces a PDB next to the binary; the release lane hard-fails if it is missing (`expected a soldr PDB sidecar`). Cached builds retain it too: the vendored cache models `<image>.pdb` as a declared output of the link step (soldr#2148), so hits replay the sidecar beside the image. |
| Linux (gnu + musl, x64 + ARM64) | `soldr.debug` | Separate `-symbols.tar.zst` asset | Post-link `objcopy`/`strip` carve-out, NOT `split-debuginfo`. See "Why a post-link carve-out, not `split-debuginfo`" below. |
| macOS (x64 + ARM64) | `soldr.dSYM/` | Separate `-symbols.tar.zst` asset | `[profile.release]`'s `split-debuginfo` is overridden to `"packed"` for darwin targets only (`native_release_build.py::release_build_environment`), which runs `dsymutil` producing the `soldr.dSYM/` bundle, followed by a `strip -x` pass on the staged binary. |

### Why a post-link carve-out, not `split-debuginfo`, on Linux

soldr#3038's first pass at this used rustc's own `split-debuginfo = "packed"`
on every platform, matching what Windows and macOS already did for free. It
measured worse than not splitting at all, for a reason specific to how
`"packed"` works: it only splits the DWARF for the crates **rustc itself
compiles in this build**. It has no visibility into the precompiled standard
library or the object code every dependency built through the `cc` crate
contributes (`ring`, `zstd-sys`, `lzma-sys`, `libsqlite3-sys`, and
`mimalloc-pprof` itself) — all of that embeds its own DWARF into the final
link regardless of the split-debuginfo setting, because it was never
compiled with rustc's split flag in the first place. The result was
duplication, not splitting: `objdump`/`readelf -S` on the "packed" binary
still showed ~49 MiB of `.debug_line` / `.debug_ranges` / `.debug_loc` /
`.debug_info` sections, while the `.dwp` beside it held another ~48.5 MiB —
soldr's own shipped binary went from 21.3 MiB (stripped) to 73.8 MiB
("packed"), and the PyPI wheel that bundles it went from 9.8 MiB to 23.8 MiB,
for a `.dwp` that did not even capture that std/C-dependency debug info in
the first place.

The fix is to stop asking rustc to split anything, and instead run the
carve-out **after the link**, on the complete binary, which by definition
contains debug info from every source in one place:

```bash
objcopy --only-keep-debug soldr soldr.debug   # copy every debug section out
strip   --strip-debug     soldr               # remove them from the shipped binary
objcopy --add-gnu-debuglink=soldr.debug soldr # embed a debuglink back to the sidecar
```

This captures 100% of the DWARF (std, C dependencies, soldr's own crates) in
one non-duplicated pass, and the shipped binary carries BOTH the resulting
`.gnu_debuglink` section and the GNU build-id note this release lane already
injects (`-C link-arg=-Wl,--build-id`), so a debugger or symbolizer can
resolve `soldr.debug` by either route. `[profile.release]` accordingly sets
`split-debuginfo = "off"` (not `"packed"`) on the default/Linux path;
`debug = "line-tables-only"` stays, because it is what makes soldr's own
crates carry any debug info at all for the carve-out to find, and `strip`
stays `"none"` because the carve-out needs the fully linked, undisturbed
binary as its input. The three-step sequence itself lives in
`stage_release_binaries.py::stage_debug_symbols`, run after the binaries are
already staged in `package_dir` (so it strips the exact bytes that ship, not
a separate copy). "The runner" below means GitHub's `ubuntu-24.04` image.

**Which binutils runs is per-target, not per-runner (soldr#3085).** The first
pass at this hard-coded bare `objcopy`/`strip`, which resolve to the *host's*
GNU binutils. That is only correct when the lane builds for the host it runs
on, and most of the release matrix does not: `aarch64-unknown-linux-gnu` has
always been an x86_64-hosted cross lane, and the `*-apple-darwin` lanes moved
to Linux hosts in soldr#3073. Ubuntu's stock binutils is single-target, so it
rejects a foreign-arch ELF (`objcopy: Unable to recognise the format of the
input file`), and GNU binutils cannot read Mach-O at any architecture
(`strip: file format not recognized`). Both failures took down release run
33820395040 (v0.9.12).

`stage_debug_symbols` now asks `select_binutils()` for tools that can read
the *target*: host GNU `objcopy`/`strip` when the target is native to the
host (unchanged for the x64-gnu, x64-musl and native arm64-musl lanes), and
otherwise `llvm-objcopy`/`llvm-strip`, which are target-agnostic by
construction and GNU-option compatible — `--only-keep-debug`,
`--strip-debug`, `--add-gnu-debuglink` and Mach-O `-x` all behave identically.
They are found on `PATH` (soldr's managed LLVM is already first on PATH in
the darwin lanes), then in soldr's managed layouts, then in rustup's
`llvm-tools` component, which `release-auto.yml` installs on every non-Windows
lane so the aarch64-gnu lane has a deterministic source. A secondary fallback
derives the managed cross toolchain's target-prefixed GNU binutils (e.g.
`aarch64-conda-linux-gnu-objcopy`) from the `AR_<triple>` the bundle exports.
If none of that resolves, staging fails loudly naming every location it
searched — a release never silently ships an uncarved binary. `--objcopy`,
`--strip-tool` and `--darwin-strip` remain as explicit overrides.

The measurements below were taken with GNU objcopy/strip 2.46 on a native
x86_64 host, which is still exactly what the `x86_64-unknown-linux-gnu` lane
runs.

`soldr-daemon` is staged as a hardlink to `soldr` (`copy_or_link`), and the
strip above mutates `soldr`'s inode in place — so on the common path,
stripping `soldr` strips `soldr-daemon` too, for free, and one `soldr.debug`
covers both. `stage_debug_symbols` does not assume this: it hash-compares
the two staged binaries afterward and re-derives `soldr-daemon` from the
now-stripped `soldr` if the cross-device `copy_or_link` fallback ever left
them independent.

**macOS is different on purpose, not by oversight.** Mach-O does not embed
full DWARF in the linked executable the way ELF does in the first place —
`dsymutil` gathers debug info from the intermediate `.o` files it can still
find via `N_OSO` stab entries, not from what ended up in the linked binary —
so `split-debuginfo = "packed"` on macOS was never subject to the
ELF-specific duplication problem above; that model IS the platform-native
answer there, and soldr#3038 kept it, adding only a `strip -x` pass after
`dsymutil` runs to drop the local symbol table entries the shipped binary no
longer needs. Since Cargo profiles cannot vary by target triple, "packed" is
injected only for `-apple-darwin` targets via
`CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO` in
`native_release_build.py::release_build_environment` — Linux and Windows
never see it. This macOS half is documented from source-level reasoning
about Mach-O's debug-info model, not independently verified end-to-end in
this environment (no macOS host was available); the release lane's own
darwin lanes are the real gate.

### Why not inline (the setup-soldr consumer contract)

The asymmetry used to be framed as deliberate — "Windows PDBs are a free
by-product, enabling split-debug on Unix costs build time and size for
symbol data nobody consumes." That framing was never actually tested: Windows
was simply the platform that got built and verified first (#780, #782), and
nobody came back to flip `split-debuginfo` on Linux/macOS afterward. A crash
or heap profile from a released Linux or macOS daemon was unsymbolizable by
anyone until soldr#3038 fixed generation — that is a real cost, not a
hypothetical one.

Turning generation on is not the same problem as *shipping* it, though.
`manifest.json`'s `soldr.debug_info` is a load-bearing field for every
`setup-soldr` consumer, and the actual validator
(`.github/actions/setup-soldr/zccache_contract.py::validate_release_manifest`,
run by `ensure_soldr.py` on every install) is pdb-only:

```python
for entry in debug_info:
    name = str(entry["name"])
    if entry.get("format") != "pdb":
        raise RuntimeError(
            f"unsupported soldr debug_info format for {name}: {entry.get('format')}"
        )
```

`release_manifest.py::collect_debug_info` had carried Linux `.dwp` / macOS
`.dSYM` collection since #786, staged for a future where `split-debuginfo`
got turned on — untested against this consumer, because the code path was
dormant. soldr#3038 is that future, and it is why the sidecar now ships as
its own asset instead, and why Linux no longer uses `split-debuginfo` at
all (see "Why a post-link carve-out" above): `stage_release_binaries.py::
stage_debug_symbols` writes `.debug`/`.dSYM` into a directory that is never
the one `release_manifest.py` or the main archive read from, so
`soldr.debug_info` stays pdb-only (or empty) on every platform, exactly the
shape `zccache_contract.py` already accepted before soldr#3038. Both halves
of this — the writer never collecting a non-pdb sidecar, and the stager
never placing one where the writer could find it — are covered by
regression tests (`tests/test_release_manifest.py::TestDebugSidecars`,
`tests/test_stage_release_binaries.py`).

The size trade for carrying symbols at all is real and is measured below; it
now falls entirely on people who fetch the `-symbols.tar.zst` asset, not on
every ordinary `setup-soldr` install or every PyPI wheel download.

### Measured size cost (x86_64-unknown-linux-gnu, local build, same steps the release lane uses)

Baseline is the pre-#786 release profile (`strip = "symbols"`, no `debug`
key, no `split-debuginfo`), matching the shipped `v0.9.11` archive. "This
fix" is the objcopy carve-out described above: `debug = "line-tables-only"`,
`split-debuginfo = "off"`, `strip = "none"` at the Cargo level, then
`objcopy --only-keep-debug` / `strip --strip-debug` / `objcopy
--add-gnu-debuglink` in `stage_debug_symbols` after linking.

| Artifact | Baseline (v0.9.11) | This fix | Δ |
|---|---|---|---|
| `soldr` binary on disk, before the carve-out | 21.3 MiB (`strip = "symbols"` was the whole story) | 125.3 MiB (`split-debuginfo = "off"`: nothing split, std + C-dep + soldr's own DWARF all embedded together -- this is the objcopy carve-out's *input*, never shipped as-is) | n/a (not a shipped artifact) |
| `soldr` binary on disk, after the carve-out (archive copy) | 21.3 MiB (stripped) | 23.9 MiB (`objcopy --strip-debug` + `.gnu_debuglink` + build-id note; larger than baseline because `--strip-debug` keeps `.symtab`/`.strtab`, which the old Cargo-level `strip = "symbols"` also removed -- an available further lever, not applied here per the exact 3-step recipe) | +2.6 MiB |
| `soldr.debug` | -- (not emitted) | 104.3 MiB (std + every `cc`-built C dependency + soldr's own crates, in one file -- the `.dwp` never captured the first two) | new |
| Main archive `soldr-vX.Y.Z-<triple>.tar.zst` (soldr + soldr-daemon only, zstd level 19) | ~17.2 MiB *(full archive incl. crgx/cargo-chef/manifest)* | 15.5 MiB *(soldr + soldr-daemon only; add a few hundred KB–low MiB for crgx/cargo-chef/manifest to compare like-for-like -- still in baseline's range)* | roughly even |
| New `soldr-vX.Y.Z-<triple>-symbols.tar.zst` (soldr.debug, zstd level 19) | -- (doesn't exist) | 16.8 MiB | new, opt-in |
| PyPI wheel (`soldr-*-linux_x86_64.whl`, tokio-console feature on, matching what every published wheel already carries), **without** `maturin build --strip` | 9.8 MiB | 36.7 MiB compressed (bundles the same unstripped binary the archive step carves symbols out of -- this is the bug the first pass at this shipped, and why `--strip` is not optional) | +26.9 MiB |
| PyPI wheel, **with** `maturin build --strip` (what `build_release_wheel.py` / `native_release_build.py::musl_wheel_maturin_command` actually run) | 9.8 MiB | **10.3 MiB compressed** (binary inside: 22.8 MiB uncompressed) | +0.5 MiB |


Numbers measured locally with `soldr archive --stage-dir … --output …` (the
same native command `package_release_archive.py` drives), GNU
`objcopy`/`strip` 2.46 (`binutils`, matching what an unmodified
`ubuntu-24.04` GitHub Actions runner has preinstalled, and what the
host-native `x86_64-unknown-linux-gnu` lane still selects — the cross lanes
now select `llvm-objcopy`/`llvm-strip` instead, per soldr#3085 above), and a
`maturin build --release` matching `build_release_wheel.py`'s invocation —
not downloaded from a published release, so treat as representative rather
than exact byte-for-byte CI output.

**The wheel contains only the `soldr` binary, never `soldr.debug`** —
verified by listing the built wheel's zip contents directly; no
`.debug`/`.dwp`/`.dSYM` entry appears, because `stage_debug_symbols` never
stages one into the tree `maturin` packages.

**Correctness, not just size**: a symbolizer must actually resolve a
function using the separated `.debug` file against the stripped binary, not
merely produce files of the expected sizes. Verified locally with `gdb`
(17.2, which follows `.gnu_debuglink` automatically when the `.debug` file
sits beside the binary) against the real `stage_release_binaries.py`-staged
pair — the stripped `soldr` alone reports nothing (`No line number
information available for address 0x653ed0 <main>`); with `soldr.debug`
placed next to it:

```
$ gdb -q -batch -ex "info line main" soldr
Line 23 of "crates/soldr-cli/src/main.rs" starts at address 0x6538e0 <main>
and ends at 0x6538ef <main+15>.
```

`crates/soldr-cli/src/main.rs:23` is exactly `fn main() -> std::process::ExitCode {`
in the source tree at the commit this was built from — the resolution is
correct, not merely present. `llvm-symbolizer --obj=soldr <address>` and
`eu-unstrip` are the other two tools that follow a `.gnu_debuglink`/build-id
and would work equivalently; `eu-unstrip` (from `elfutils`) was not
installed in this environment, so `gdb` is what was actually used here.

musl targets: both linux libc flavors are plain ELF, so the same
`objcopy`/`strip` carve-out applies unmodified to `x86_64-unknown-linux-musl`
and `aarch64-unknown-linux-musl` — no libc-specific branch was needed (unlike
the abandoned `split-debuginfo` attempt, where a first pass had specifically
checked `rustc --target x86_64-unknown-linux-musl --print=split-debuginfo`
to confirm `"packed"` was even accepted for musl; that check is now moot
since Linux does not ask rustc to split anything at all). A host `objcopy`
built with full BFD multi-arch support (the GNU binutils default on most
Linux distributions, including the release runner) reads/writes ELF for
`aarch64` from an `x86_64` host without needing a target-specific `objcopy`;
this was not independently verified against a real `aarch64-unknown-linux-musl`
binary in this environment (no musl cross toolchain installed here) — the
release lane's own musl matrix lanes are the real gate.

## Verifying an archive

```bash
tar --zstd -tf soldr-vX.Y.Z-x86_64-pc-windows-msvc.tar.zst
# → soldr.exe, soldr-daemon.exe, cargo-chef.exe, crgx.exe,
#   manifest.json, soldr.pdb

tar --zstd -xOf … manifest.json | jq .soldr.debug_info
# → [{ "name": "soldr.pdb", "sha256": "…", "format": "pdb" }]

tar --zstd -tf soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.zst
# → soldr, soldr-daemon, cargo-chef, crgx, manifest.json
#   (no soldr.debug -- it ships separately, see below)

tar --zstd -xOf … manifest.json | jq .soldr.debug_info
# → [] on every non-Windows target, always

tar --zstd -tf soldr-vX.Y.Z-x86_64-unknown-linux-gnu-symbols.tar.zst
# → soldr.debug

readelf -S soldr | grep -c debug
# → 0 -- every .debug_* section was carved out, not just split

readelf -S soldr | grep gnu_debuglink
# → .gnu_debuglink present, pointing at soldr.debug's basename + CRC32

readelf -n soldr | grep -i build
# → GNU  ...  NT_GNU_BUILD_ID ...  (untouched by --strip-debug)

# Resolve a symbol using ONLY the stripped binary + the separate .debug file
# -- the correctness proof, not just a size measurement:
eu-unstrip -e soldr -d soldr.debug -n            # or:
gdb -batch -ex 'info symbol <address>' soldr      # gdb follows .gnu_debuglink
llvm-symbolizer --obj=soldr <address>             # resolves via the debug link
```

The symbols asset may not exist for every target (Windows never produces
one; see the Contract section) — check for the asset before assuming it is
there. Its own integrity is covered by the release's top-level
`soldr-<version>-SHA256SUMS.txt`, the same way every other release asset is,
rather than by a sidecar-specific manifest field.

## Bundled third-party binaries

`zccache` is embedded in the `soldr` binary itself (soldr#1368), so its
symbols are part of `soldr.pdb` / `soldr.debug` / `soldr.dSYM`. `crgx` and
`cargo-chef` are prebuilt upstream fetches — soldr does not (and cannot)
generate symbol files for them; they are out of scope for `debug_info` and
for the symbols asset.

## Local debugging without release sidecars

zccache is linked into the locally built `soldr-daemon` from the exact
published crate version in the lockfile. There is no external zccache daemon
or `SOLDR_ZCCACHE_LOCAL_DIR` symbol-copy path in the embedded architecture.

On Windows, cached builds retain the `.pdb`: `rustc_expected_output_paths` in
the vendored `zccache-daemon-core/src/daemon/server/rustc.rs` declares
`<image>.pdb` beside a linked MSVC image (soldr#2148), so the sidecar is
staged, stored, and replayed with the executable. A declared `.pdb` that a
debuginfo-off build never produces is filtered out at collection time rather
than failing the compile. There is no cache-bypass step in the debugging
workflow.
