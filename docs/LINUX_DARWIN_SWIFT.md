# Linux-hosted Darwin Swift builds

`soldr toolchain darwin` provisions the verified Linux x64 GNU Swift/LLVM/Apple
SDK closure for `aarch64-apple-darwin` (Refs #4165). It requires a glibc Linux
x64 host. It never consults host Xcode or `SDKROOT`.

```sh
soldr toolchain darwin --target aarch64-apple-darwin --sdk-version 26.5 --json > darwin.json
cmake -S . -B build-darwin -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$(jq -r .cmake_toolchain_file darwin.json)"
cmake --build build-darwin
```

The pinned closure is LLVM **21.1.5**, Darwin compiler-rt **21.1.5**, Swift
**6.3.2**, and SDK **26.5**. Unsupported SDK versions, hosts, targets, missing
catalogue units and SDKSettings version mismatches fail explicitly. Add
`--universal-sdk` to select the 26.5 universal2 SDK; output remains arm64.
The ordinary SDK resolver also accepts `SOLDR_APPLE_SDK_VERSION=26.5`.
Swift provisioning is explicit and does not enlarge ordinary Rust SDK installs.

The command returns schema version 1 JSON containing:

- `swiftc`, `clang`, `clangxx`, `linker` (`ld64.lld`), `dsymutil` and
  `llvm_dsymutil` paths;
- `sdk`, `clang_resource_dir`, `compiler_rt`, `swift_resource_dir`, and
  `module_cache` paths;
- `swift_compile_args`, `swift_link_args`, `c_compile_args` and `c_link_args` argument arrays
  for native/bridge builds; pass each item as one argument;
- `cmake_toolchain_file`, `identity`, versions, target, and each unit's asset
  identity, SHA256 and compressed size.

The generated CMake file selects Darwin/arm64, SDK 26.5, a macOS 11.0 deployment
floor, the managed C/C++/Swift compilers, linker and debug tools. It supplies
Swift target resources, SDK shim includes, Linux host plugins and importer
resources. Projects remain responsible for their framework/library dependencies
and any higher deployment floor. Sanitizers remain available through normal
`-fsanitize=address,undefined` compile/link flags. Stage the matching runtime
dylibs from `compiler_rt` for execution on macOS.

Native example, using the JSON argument arrays through Python:

```sh
uv run --no-project python - <<'PY'
import json
import subprocess
with open("darwin.json", encoding="utf-8") as stream:
    t = json.load(stream)
subprocess.run([t["swiftc"], *t["swift_compile_args"], "-swift-version", "6",
                "-parse-as-library", "-g", "-emit-object", "Probe.swift",
                "-o", "Probe.o"], check=True)
subprocess.run([t["swiftc"], *t["swift_link_args"], "-emit-library", "-g",
                "Probe.o", "-framework", "AppKit", "-framework", "Metal",
                "-o", "libProbe.dylib"], check=True)
subprocess.run([t["dsymutil"], "libProbe.dylib", "-o", "Probe.dSYM"], check=True)
subprocess.run([t["clang"], *t["c_compile_args"], *t["c_link_args"], "-fsanitize=address,undefined",
                "probe.c", "-Wl,-adhoc_codesign", "-o", "probe"], check=True)
PY
```

For the shared setup-soldr root `R` (Soldr's `SOLDR_CACHE_DIR`, otherwise
`~/.soldr`), the exact relative layout is:

```text
R/sdk/darwin/aarch64-apple-darwin/llvm-21.1.5-swift-6.3.2-sdk-26.5/<identity>/
  llvm/package/bin/{clang,clang++,ld64.lld,dsymutil,llvm-dsymutil}
  llvm/package/lib/clang/21/include/sanitizer/
  llvm/package/lib/clang/21/lib/darwin/
  swift/package/bin/swiftc
  apple/package/sdk/
  apple/package/swift-resource-dir/
  swift-resources/{macosx,shims,apinotes,swiftToCxx,host,clang}
  module-cache/
  darwin.cmake
  toolchain.json
  .complete
```

`identity` hashes the extraction format, host, target, ordered unit identities,
versions, content hashes and sizes. All input archives use the existing verified
catalogue/publication-state and multipart transport (client capability 2), with
SHA256 and size checks. A cross-process lock serializes installers of one
identity. Downloads use the content-addressed archive cache; extraction,
compiler-rt overlay, Swift resource composition, validation and completion
metadata happen in a temporary sibling directory before one rename publishes
the complete closure. Existing closures are never overwritten. Six runner slots
can therefore prepare/reuse one root without writing over active tools.

The target Swift resource tree exposes only SDK `macosx`, `shims`, `apinotes`,
`swiftToCxx` and Linux Swift `host`/`clang` resources, using relative directory
links. Linux CoreFoundation/dispatch/os headers cannot shadow the Darwin SDK.
The original package trees, license notices and provenance remain intact.
The module cache path includes the complete content identity.

Producer contracts and the real AppKit/Metal and sanitizer link recipe are in
[soldr-toolchain's Linux Darwin guide](https://github.com/zackees/soldr-toolchain/blob/main/docs/LINUX_DARWIN_CROSS.md).
Cross-produced Mach-O files can be inspected and their ad-hoc signatures checked
on Linux; target execution and full OBS CTest coverage belong downstream.
