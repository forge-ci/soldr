//! Linux-hosted Darwin Swift closure. Inputs and composed outputs are immutable,
//! content-addressed, locked across processes, and published by directory rename.
use kernal_api::hash::Sha256Hasher;
use std::path::{Path, PathBuf};

use super::manifest_lookup::{self, ManifestEntry, ManifestIndex};
use crate::core::{SoldrError, SoldrPaths};

pub const LLVM_VERSION: &str = "21.1.5";
pub const SWIFT_VERSION: &str = "6.3.2";
pub const SDK_VERSION: &str = "26.5";
pub const TARGET: &str = "aarch64-apple-darwin";
const HOST: &str = super::catalogue_linux_host::GNU_BUNDLE_HOST.host;
// Bump when the installed layout or generated CMake/JSON changes; roots are never rewritten.
const FORMAT: &str = "darwin-swift-v1";

fn select_units(
    index: &ManifestIndex,
    host: &str,
    target: &str,
    sdk: &str,
    universal: bool,
) -> Result<Vec<ManifestEntry>, SoldrError> {
    if host != HOST || target != TARGET || sdk != SDK_VERSION {
        return Err(SoldrError::UnsupportedPlatform(format!(
            "Darwin Swift requires host {HOST}, target {TARGET}, SDK {SDK_VERSION}; got {host}, {target}, {sdk}"
        )));
    }
    let shape = if universal {
        "darwin-universal2"
    } else {
        "darwin-aarch64"
    };
    [
        format!("llvm-tools/{LLVM_VERSION}/linux-x64-gnu/bundle.tar.zst"),
        format!("compiler-rt/{LLVM_VERSION}/darwin-arm64/bundle.tar.zst"),
        format!("swift/{SWIFT_VERSION}/linux-x64-gnu/bundle.tar.zst"),
        format!("apple-sdk/{sdk}/{shape}/sdk.tar.zst"),
    ].iter().map(|asset| {
        index.lookup("zackees", "soldr-toolchain", "assets", asset)
            .filter(|e| e.source_path.as_deref() == Some(asset.as_str()))
            .cloned().ok_or_else(|| SoldrError::Other(format!(
                "Darwin Swift requires verified catalogue unit {asset}; missing or mismatched identity (Refs #4165)"
            )))
    }).collect()
}

/// Machine-facing native/CMake paths. Argument vectors preserve paths with spaces.
#[derive(Debug, serde::Serialize)]
pub struct DarwinToolchain {
    pub schema_version: u32,
    pub target: String,
    pub llvm_version: String,
    pub swift_version: String,
    pub sdk_version: String,
    pub identity: String,
    pub root: PathBuf,
    pub clang: PathBuf,
    pub clangxx: PathBuf,
    pub linker: PathBuf,
    pub ar: PathBuf,
    pub ranlib: PathBuf,
    pub dsymutil: PathBuf,
    pub llvm_dsymutil: PathBuf,
    pub swiftc: PathBuf,
    pub sdk: PathBuf,
    pub clang_resource_dir: PathBuf,
    pub compiler_rt: PathBuf,
    pub swift_resource_dir: PathBuf,
    pub module_cache: PathBuf,
    pub cmake_toolchain_file: PathBuf,
    pub swift_compile_args: Vec<String>,
    pub swift_link_args: Vec<String>,
    pub c_compile_args: Vec<String>,
    pub c_link_args: Vec<String>,
    pub units: Vec<serde_json::Value>,
}

fn identity(units: &[ManifestEntry]) -> String {
    let mut h = Sha256Hasher::new();
    h.update(format!("{FORMAT}\n{HOST}\n{TARGET}\n").as_bytes());
    for unit in units {
        h.update(format!("{} {} {}\n", unit.asset, unit.sha256, unit.size_bytes).as_bytes());
    }
    h.finalize().to_hex()
}

fn swift_common_args(sdk: &Path, resources: &Path) -> Vec<String> {
    vec![
        "-target".into(),
        "arm64-apple-macosx11.0".into(),
        "-sdk".into(),
        sdk.display().to_string(),
        "-resource-dir".into(),
        resources.display().to_string(),
    ]
}

fn swift_compile_args(sdk: &Path, resources: &Path, module_cache: &Path) -> Vec<String> {
    let string = |p: &Path| p.to_string_lossy().into_owned();
    let mut args = swift_common_args(sdk, resources);
    args.extend([
        "-I".into(),
        string(&sdk.join("usr/lib/swift")),
        "-I".into(),
        string(&resources.join("shims")),
        "-Xcc".into(),
        "-resource-dir".into(),
        "-Xcc".into(),
        string(&resources.join("clang")),
        "-Xfrontend".into(),
        "-enable-cross-import-overlays".into(),
        "-module-cache-path".into(),
        string(module_cache),
        "-F".into(),
        string(&sdk.join("System/Library/Frameworks")),
    ]);
    args
}

fn swift_link_args(sdk: &Path, resources: &Path, tools: &Path) -> Vec<String> {
    let mut args = swift_common_args(sdk, resources);
    args.extend([
        "-tools-directory".into(),
        tools.display().to_string(),
        "-use-ld=lld".into(),
        "-L".into(),
        sdk.join("usr/lib/swift").display().to_string(),
        "-Xlinker".into(),
        "-adhoc_codesign".into(),
    ]);
    args
}

impl DarwinToolchain {
    fn at(root: PathBuf, identity: String, units: &[ManifestEntry]) -> Self {
        let llvm = root.join("llvm/package");
        let sdk = root.join("apple/package/sdk");
        let resources = root.join("swift-resources");
        let clang_resources = llvm.join("lib/clang/21");
        let module_cache = root.join("module-cache");
        let string = |p: &Path| p.to_string_lossy().into_owned();
        let swift_compile_args = swift_compile_args(&sdk, &resources, &module_cache);
        let swift_link_args = swift_link_args(&sdk, &resources, &llvm.join("bin"));
        let units = units.iter().map(|e| serde_json::json!({"asset": e.asset, "sha256": e.sha256, "size_bytes": e.size_bytes})).collect();
        Self {
            schema_version: 1,
            target: TARGET.into(),
            llvm_version: LLVM_VERSION.into(),
            swift_version: SWIFT_VERSION.into(),
            sdk_version: SDK_VERSION.into(),
            identity,
            clang: llvm.join("bin/clang"),
            clangxx: llvm.join("bin/clang++"),
            ar: llvm.join("bin/llvm-ar"),
            ranlib: llvm.join("bin/llvm-ranlib"),
            linker: llvm.join("bin/ld64.lld"),
            dsymutil: llvm.join("bin/dsymutil"),
            llvm_dsymutil: llvm.join("bin/llvm-dsymutil"),
            swiftc: root.join("swift/package/bin/swiftc"),
            compiler_rt: clang_resources.join("lib/darwin"),
            c_compile_args: vec![
                "--target=arm64-apple-macosx11.0".into(),
                "-isysroot".into(),
                string(&sdk),
                "-resource-dir".into(),
                string(&clang_resources),
            ],
            sdk,
            clang_resource_dir: clang_resources,
            swift_resource_dir: resources,
            cmake_toolchain_file: root.join("darwin.cmake"),
            module_cache,
            root,
            swift_compile_args,
            swift_link_args,
            c_link_args: vec!["-fuse-ld=lld".into()],
            units,
        }
    }
}

/// Provision only catalogue-verified inputs; never consult SDKROOT or host Xcode.
pub async fn ensure_darwin_toolchain(
    paths: &SoldrPaths,
    target: &str,
    sdk: &str,
    universal: bool,
) -> Result<DarwinToolchain, SoldrError> {
    let requirement = super::catalogue_linux_host::GNU_BUNDLE_HOST;
    if !super::catalogue_linux_host::current_host_fitness(requirement).is_runnable() {
        return Err(SoldrError::UnsupportedPlatform(
            "Darwin Swift requires Linux x64 with glibc".into(),
        ));
    }
    let host = HOST;
    // Reject unsupported requests before fetching the catalogue.
    if target != TARGET || sdk != SDK_VERSION {
        select_units(&ManifestIndex::empty(), host, target, sdk, universal)?;
    }
    let index = manifest_lookup::get_or_fetch().await;
    ensure_from_index(paths, &index, host, target, sdk, universal).await
}

async fn ensure_from_index(
    paths: &SoldrPaths,
    index: &ManifestIndex,
    host: &str,
    target: &str,
    sdk: &str,
    universal: bool,
) -> Result<DarwinToolchain, SoldrError> {
    let units = select_units(index, host, target, sdk, universal)?;
    let identity = identity(&units);
    let parent = paths.root.join("sdk/darwin").join(TARGET).join(format!(
        "llvm-{LLVM_VERSION}-swift-{SWIFT_VERSION}-sdk-{sdk}"
    ));
    let _lock = super::syslib_common::acquire_install_lock(&parent, &identity)?;
    // Consumers run from other working directories; always export absolute paths.
    let parent = parent.canonicalize()?;
    let root = parent.join(&identity);
    let result = DarwinToolchain::at(root.clone(), identity.clone(), &units);
    if root.exists() {
        if std::fs::read_to_string(root.join(".complete"))
            .ok()
            .as_deref()
            != Some(&identity)
        {
            return Err(SoldrError::Other(format!(
                "incomplete Darwin toolchain {}; move it aside and retry",
                root.display()
            )));
        }
        validate_layout(&root, sdk)?;
        return Ok(result);
    }
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&parent)?;
    for (unit, dest) in units.iter().zip(["llvm", "runtime", "swift", "apple"]) {
        let downloaded = manifest_lookup::materialize_catalogue_entry(paths, unit).await?;
        eprintln!(
            "soldr: trust: verified {} sha256={}",
            unit.asset,
            downloaded.sha256()
        );
        let dir = staging.path().join(dest);
        std::fs::create_dir_all(&dir)?;
        extract_unit(downloaded.path(), &dir, false)?;
        if dest == "runtime" {
            // Preserve each producer's manifest/licenses in its input tree;
            // overlay only Clang resources into the newly staged LLVM tree.
            extract_unit(downloaded.path(), &staging.path().join("llvm"), true)?;
        }
    }
    compose_resources(staging.path())?;
    validate_layout(staging.path(), sdk)?;
    std::fs::write(staging.path().join(".complete"), &identity)?;
    std::fs::write(staging.path().join("darwin.cmake"), cmake_file(&result))?;
    std::fs::write(
        staging.path().join("toolchain.json"),
        serde_json::to_vec_pretty(&result)
            .map_err(|e| SoldrError::Other(format!("Darwin toolchain JSON: {e}")))?,
    )?;
    // The locked content identity is never replaced: six runner slots can keep
    // using an older closure while a new catalogue identity is being prepared.
    // tempfile starts owner-only; published tools must be traversable by
    // readers of the shared setup-soldr root.
    crate::platform::fs::permissions::restore_mode(staging.path(), Some(0o755))?;
    std::fs::rename(staging.path(), &root)?;
    Ok(result)
}

fn extract_unit(archive: &Path, dest: &Path, resources_only: bool) -> Result<(), SoldrError> {
    let decoder = zstd::stream::read::Decoder::new(std::fs::File::open(archive)?)?;
    super::tar_extract::unpack_tar_filtered(&mut tar::Archive::new(decoder), dest, |path| {
        // GNU tar producer bundles preserve a leading `./` component.
        let path = path.strip_prefix(".").unwrap_or(path);
        Ok(resources_only && !path.starts_with("package/lib/clang/21"))
    })
}

fn compose_resources(root: &Path) -> Result<(), SoldrError> {
    let resources = root.join("swift-resources");
    std::fs::create_dir(&resources)?;
    for name in ["macosx", "shims", "apinotes", "swiftToCxx"] {
        crate::platform::fs::links::create(
            &format!("../apple/package/swift-resource-dir/{name}"),
            &resources.join(name),
            true,
        )?;
    }
    for name in ["host", "clang"] {
        crate::platform::fs::links::create(
            &format!("../swift/package/lib/swift/{name}"),
            &resources.join(name),
            true,
        )?;
    }
    Ok(())
}

fn validate_layout(root: &Path, sdk_version: &str) -> Result<(), SoldrError> {
    for file in [
        "llvm/package/bin/clang",
        "llvm/package/bin/clang++",
        "llvm/package/bin/llvm-ar",
        "llvm/package/bin/llvm-ranlib",
        "llvm/package/bin/ld64.lld",
        "llvm/package/bin/dsymutil",
        "llvm/package/bin/llvm-dsymutil",
        "swift/package/bin/swiftc",
        "llvm/package/lib/clang/21/include/sanitizer/asan_interface.h",
        "llvm/package/lib/clang/21/lib/darwin/libclang_rt.osx.a",
        "llvm/package/lib/clang/21/lib/darwin/libclang_rt.asan_osx_dynamic.dylib",
        "llvm/package/lib/clang/21/lib/darwin/libclang_rt.ubsan_osx_dynamic.dylib",
        "llvm/package/lib/clang/21/lib/darwin/libclang_rt.tsan_osx_dynamic.dylib",
        "llvm/package/lib/clang/21/lib/darwin/libclang_rt.lsan_osx_dynamic.dylib",
        "apple/package/sdk/usr/include/stdio.h",
    ] {
        if !root.join(file).is_file() {
            return Err(SoldrError::Other(format!(
                "Darwin toolchain missing {file}"
            )));
        }
    }
    for dir in ["macosx", "shims", "apinotes", "swiftToCxx", "host", "clang"] {
        if !root.join("swift-resources").join(dir).is_dir() {
            return Err(SoldrError::Other(format!(
                "Darwin Swift resource missing {dir}"
            )));
        }
    }
    let settings: serde_json::Value = serde_json::from_slice(&std::fs::read(
        root.join("apple/package/sdk/SDKSettings.json"),
    )?)
    .map_err(|e| SoldrError::Other(format!("Apple SDKSettings.json: {e}")))?;
    if settings["Version"].as_str() != Some(sdk_version) {
        return Err(SoldrError::Other(format!(
            "Apple SDK version mismatch: requested {sdk_version}, SDKSettings.json reports {}",
            settings["Version"]
        )));
    }
    Ok(())
}

fn cmake_file(tool: &DarwinToolchain) -> String {
    // Quote once for the compiler's command line and once for CMake's string
    // parser. JSON callers can instead pass the argument arrays directly.
    fn quote(value: &str) -> String {
        format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('$', "\\$")
        )
    }
    fn flags(args: &[String]) -> String {
        args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
    }
    let mut swift = tool
        .swift_compile_args
        .iter()
        .skip(2)
        .cloned()
        .collect::<Vec<_>>();
    swift.extend(tool.swift_link_args.iter().skip(6).cloned());
    let mut text = String::from(
        "# Generated by soldr toolchain darwin (Refs #4165).\ninclude_guard(GLOBAL)\n",
    );
    for (key, value) in [
        ("CMAKE_SYSTEM_NAME", "Darwin".into()),
        ("CMAKE_SYSTEM_PROCESSOR", "arm64".into()),
        ("CMAKE_C_COMPILER_TARGET", "arm64-apple-darwin".into()),
        ("CMAKE_CXX_COMPILER_TARGET", "arm64-apple-darwin".into()),
        ("CMAKE_OSX_ARCHITECTURES", "arm64".into()),
        ("CMAKE_OSX_DEPLOYMENT_TARGET", "11.0".into()),
        ("CMAKE_OSX_SYSROOT", tool.sdk.to_string_lossy().into_owned()),
        (
            "CMAKE_C_COMPILER",
            tool.clang.to_string_lossy().into_owned(),
        ),
        (
            "CMAKE_CXX_COMPILER",
            tool.clangxx.to_string_lossy().into_owned(),
        ),
        (
            "CMAKE_Swift_COMPILER",
            tool.swiftc.to_string_lossy().into_owned(),
        ),
        ("CMAKE_AR", tool.ar.to_string_lossy().into_owned()),
        ("CMAKE_RANLIB", tool.ranlib.to_string_lossy().into_owned()),
        ("CMAKE_LINKER", tool.linker.to_string_lossy().into_owned()),
        (
            "CMAKE_DSYMUTIL",
            tool.dsymutil.to_string_lossy().into_owned(),
        ),
        (
            "CMAKE_C_FLAGS_INIT",
            flags(
                &tool
                    .c_compile_args
                    .iter()
                    .skip(1)
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "CMAKE_CXX_FLAGS_INIT",
            flags(
                &tool
                    .c_compile_args
                    .iter()
                    .skip(1)
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
        ),
        ("CMAKE_Swift_FLAGS_INIT", flags(&swift)),
    ] {
        if key == "CMAKE_OSX_DEPLOYMENT_TARGET" {
            text.push_str(&format!(
                "set({key} {} CACHE STRING \"Minimum macOS version\")\n",
                quote(&value)
            ));
        } else {
            text.push_str(&format!("set({key} {})\n", quote(&value)));
        }
    }
    text.push_str(
        "set(CMAKE_Swift_COMPILER_TARGET \"arm64-apple-macosx${CMAKE_OSX_DEPLOYMENT_TARGET}\")\n",
    );
    text.push_str("add_link_options(\"$<$<LINK_LANGUAGE:C,CXX>:-fuse-ld=lld>\")\n");
    text
}

#[cfg(test)]
#[path = "darwin_toolchain_tests.rs"]
mod tests;
