use super::*;

fn fixture() -> ManifestIndex {
    ManifestIndex::from_v2_json(include_str!(
        "../../tests/fixtures/darwin-toolchain.v2.json"
    ))
    .unwrap()
}

#[test]
fn fixture_selects_exact_linux_arm64_closure_and_sdk_shape() {
    let index = fixture();
    let units = select_units(&index, HOST, TARGET, SDK_VERSION, false).unwrap();
    assert_eq!(units.len(), 4);
    assert_eq!(
        units[0].asset,
        "llvm-tools/21.1.5/linux-x64-gnu/bundle.tar.zst"
    );
    assert_eq!(
        units[1].asset,
        "compiler-rt/21.1.5/darwin-arm64/bundle.tar.zst"
    );
    assert_eq!(units[2].asset, "swift/6.3.2/linux-x64-gnu/bundle.tar.zst");
    assert!(units[3].asset.contains("26.5/darwin-aarch64/"));
    assert!(
        select_units(&index, HOST, TARGET, SDK_VERSION, true).unwrap()[3]
            .asset
            .contains("darwin-universal2/")
    );
    assert!(super::super::apple_sdk::SUPPORTED_APPLE_SDK_VERSIONS.contains(&SDK_VERSION));
}

fn llvm_archive() -> Vec<u8> {
    archive(&[
        ("manifest.json", b"LLVM provenance"),
        ("package/bin/clang", b"clang"),
        ("package/bin/clang++", b"clang++"),
        ("package/bin/llvm-ar", b"llvm-ar"),
        ("package/bin/llvm-ranlib", b"llvm-ranlib"),
        ("package/bin/ld64.lld", b"ld64.lld"),
        ("package/bin/dsymutil", b"dsymutil"),
        ("package/bin/llvm-dsymutil", b"dsymutil"),
    ])
}

fn runtime_archive() -> Vec<u8> {
    archive(&[
        ("manifest.json", b"runtime provenance"),
        (
            "package/lib/clang/21/lib/darwin/libclang_rt.osx.a",
            b"builtins",
        ),
        (
            "package/lib/clang/21/lib/darwin/libclang_rt.asan_osx_dynamic.dylib",
            b"asan",
        ),
        (
            "package/lib/clang/21/lib/darwin/libclang_rt.ubsan_osx_dynamic.dylib",
            b"ubsan",
        ),
        (
            "package/lib/clang/21/lib/darwin/libclang_rt.tsan_osx_dynamic.dylib",
            b"tsan",
        ),
        (
            "package/lib/clang/21/lib/darwin/libclang_rt.lsan_osx_dynamic.dylib",
            b"lsan",
        ),
        (
            "package/lib/clang/21/include/sanitizer/asan_interface.h",
            b"headers",
        ),
    ])
}

fn swift_archive() -> Vec<u8> {
    archive(&[
        ("package/bin/swiftc", b"swiftc"),
        ("package/lib/swift/host/plugin", b"Linux plugin"),
        ("package/lib/swift/clang/include/stddef.h", b"importer"),
        ("package/lib/swift/CoreFoundation/wrong.h", b"must not leak"),
    ])
}

fn sdk_archive(sdk_version: &str) -> Vec<u8> {
    let sdk_settings = format!(r#"{{"Version":"{sdk_version}"}}"#);
    archive(&[
        ("package/sdk/SDKSettings.json", sdk_settings.as_bytes()),
        ("package/sdk/usr/include/stdio.h", b"stdio"),
        ("package/sdk/usr/lib/swift/shim.h", b"sdk shim"),
        (
            "package/swift-resource-dir/macosx/Swift.swiftmodule/arm64.swiftinterface",
            b"target overlay",
        ),
        (
            "package/swift-resource-dir/shims/module.modulemap",
            b"Darwin shim",
        ),
        (
            "package/swift-resource-dir/apinotes/AppKit.apinotes",
            b"apinotes",
        ),
        ("package/swift-resource-dir/swiftToCxx/header.h", b"cxx"),
    ])
}

#[test]
fn rejects_wrong_host_target_version_and_missing_or_misbound_units() {
    let index = fixture();
    for (host, target, sdk) in [
        ("aarch64-unknown-linux-gnu", TARGET, SDK_VERSION),
        (HOST, "x86_64-apple-darwin", SDK_VERSION),
        (HOST, TARGET, "14.5"),
        (HOST, TARGET, "26.5/../../escape"),
    ] {
        assert!(select_units(&index, host, target, sdk, false).is_err());
    }
    for n in 0..4 {
        let mut index = fixture();
        index.entries.remove(n);
        assert!(select_units(&index, HOST, TARGET, SDK_VERSION, false).is_err());
    }
    let mut index = fixture();
    index.entries[1].source_path = Some("compiler-rt/21.1.6/darwin-arm64/bundle.tar.zst".into());
    assert!(select_units(&index, HOST, TARGET, SDK_VERSION, false).is_err());
}

fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    let encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
    let mut tar = tar::Builder::new(encoder);
    for (name, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        // Match published GNU tar bundles. append_data/set_path normalize `./`,
        // so preserve the raw name to exercise the overlay filter's boundary.
        let name = format!("./{name}");
        assert!(name.len() < 100);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        tar.append(&header, *bytes).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

fn seeded(root: &Path, sdk_version: &str) -> (SoldrPaths, ManifestIndex) {
    let paths = SoldrPaths::with_root(root.to_path_buf());
    let cache = paths.cache.join("catalogue-v2/assets");
    std::fs::create_dir_all(&cache).unwrap();
    let archives = [
        llvm_archive(),
        runtime_archive(),
        swift_archive(),
        sdk_archive(sdk_version),
    ];
    let mut index = fixture();
    for (entry, bytes) in index.entries.iter_mut().zip(archives) {
        entry.sha256 = super::super::trust::sha256_of(&bytes);
        entry.size_bytes = bytes.len() as u64;
        std::fs::write(cache.join(&entry.sha256), bytes).unwrap();
    }
    (paths, index)
}

// Resource composition uses Linux directory-link behavior. The decision and
// catalogue/transport tests below remain portable on every host.
fn provisioning_host() -> bool {
    let runnable = super::super::catalogue_linux_host::current_host_fitness(
        super::super::catalogue_linux_host::GNU_BUNDLE_HOST,
    )
    .is_runnable();
    if !runnable {
        eprintln!("skip: Darwin closure installation requires a glibc Linux x64 host");
    }
    runnable
}

#[tokio::test]
async fn fixture_installs_resources_atomically_and_reuses_complete_closure() {
    if !provisioning_host() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (paths, index) = seeded(&tmp.path().join("shared root"), SDK_VERSION);
    let tool = ensure_from_index(&paths, &index, HOST, TARGET, SDK_VERSION, false)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(tool.root.join("llvm/manifest.json")).unwrap(),
        b"LLVM provenance"
    );
    assert_eq!(
        std::fs::read(tool.root.join("runtime/manifest.json")).unwrap(),
        b"runtime provenance"
    );
    assert_eq!(
        crate::platform::fs::permissions::mode(&tool.root).unwrap() & 0o777,
        0o755
    );
    assert!(tool.root.starts_with(paths.root.join("sdk/darwin")));
    assert_eq!(
        std::fs::read(tool.compiler_rt.join("libclang_rt.asan_osx_dynamic.dylib")).unwrap(),
        b"asan"
    );
    assert!(tool
        .clang_resource_dir
        .join("include/sanitizer/asan_interface.h")
        .is_file());
    assert!(tool.swift_resource_dir.join("host/plugin").is_file());
    assert!(tool
        .swift_resource_dir
        .join("macosx/Swift.swiftmodule/arm64.swiftinterface")
        .is_file());
    assert!(tool
        .swift_resource_dir
        .join("clang/include/stddef.h")
        .is_file());
    assert!(tool
        .swift_resource_dir
        .join("shims/module.modulemap")
        .is_file());
    assert!(!tool.swift_resource_dir.join("CoreFoundation").exists());
    assert!(tool.dsymutil.is_file() && tool.llvm_dsymutil.is_file());
    assert!(tool
        .swift_compile_args
        .contains(&tool.swift_resource_dir.join("shims").display().to_string()));
    assert!(tool
        .swift_link_args
        .contains(&tool.clang.parent().unwrap().display().to_string()));
    assert!(!serde_json::to_string(&tool).unwrap().contains(".staging-"));
    let cmake = std::fs::read_to_string(&tool.cmake_toolchain_file).unwrap();
    assert!(cmake.contains("CMAKE_Swift_COMPILER"));
    assert!(cmake.contains("shared root"));
    assert!(cmake.contains("-tools-directory"));
    assert!(cmake.contains("LINK_LANGUAGE:C,CXX"));
    assert!(!tool
        .c_compile_args
        .iter()
        .any(|a| a.starts_with("-fuse-ld")));
    assert_eq!(tool.c_link_args, ["-fuse-ld=lld"]);
    let mtime = std::fs::metadata(tool.root.join(".complete"))
        .unwrap()
        .modified()
        .unwrap();
    let again = ensure_from_index(&paths, &index, HOST, TARGET, SDK_VERSION, false)
        .await
        .unwrap();
    assert_eq!(tool.root, again.root);
    assert_eq!(
        mtime,
        std::fs::metadata(tool.root.join(".complete"))
            .unwrap()
            .modified()
            .unwrap()
    );
}

#[tokio::test]
async fn mismatched_sdk_or_missing_debug_tool_never_publishes_and_preserves_previous() {
    if !provisioning_host() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (paths, mut index) = seeded(tmp.path(), "15.5");
    let err = ensure_from_index(&paths, &index, HOST, TARGET, SDK_VERSION, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("version mismatch"), "{err}");
    let expected = identity(&select_units(&index, HOST, TARGET, SDK_VERSION, false).unwrap());
    assert!(!paths
        .root
        .join("sdk/darwin")
        .join(TARGET)
        .join(format!(
            "llvm-{LLVM_VERSION}-swift-{SWIFT_VERSION}-sdk-{SDK_VERSION}"
        ))
        .join(expected)
        .exists());
    let (_, good) = seeded(tmp.path(), SDK_VERSION);
    let tool = ensure_from_index(&paths, &good, HOST, TARGET, SDK_VERSION, false)
        .await
        .unwrap();
    let bytes = archive(&[
        ("package/bin/clang", b"clang"),
        ("package/bin/clang++", b"clang++"),
        ("package/bin/llvm-ar", b"llvm-ar"),
        ("package/bin/llvm-ranlib", b"llvm-ranlib"),
        ("package/bin/ld64.lld", b"ld64.lld"),
        ("package/bin/llvm-dsymutil", b"llvm-dsymutil"),
    ]);
    index = good;
    index.entries[0].sha256 = super::super::trust::sha256_of(&bytes);
    index.entries[0].size_bytes = bytes.len() as u64;
    std::fs::write(
        paths
            .cache
            .join("catalogue-v2/assets")
            .join(&index.entries[0].sha256),
        bytes,
    )
    .unwrap();
    assert!(
        ensure_from_index(&paths, &index, HOST, TARGET, SDK_VERSION, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("bin/dsymutil")
    );
    assert!(tool.dsymutil.is_file());
}

#[tokio::test]
async fn wrong_logical_hash_fails_verification_before_promotion() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, mut index) = seeded(tmp.path(), SDK_VERSION);
    let original = index.entries[0].sha256.clone();
    let bytes = std::fs::read(paths.cache.join("catalogue-v2/assets").join(&original)).unwrap();
    let parts = paths.cache.join("catalogue-v2/parts");
    std::fs::create_dir_all(&parts).unwrap();
    std::fs::write(parts.join(&original), &bytes).unwrap();
    index.entries[0].transport = manifest_lookup::AssetTransport::Multipart {
        parts: vec![manifest_lookup::Part {
            number: 1,
            size_bytes: bytes.len() as u64,
            sha256: original,
            urls: vec!["https://example.invalid/unused".into()],
        }],
    };
    index.entries[0].sha256 = "f".repeat(64);
    let err = ensure_from_index(&paths, &index, HOST, TARGET, SDK_VERSION, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("hash verification"), "{err}");
}

#[test]
fn six_slots_converge_on_one_complete_install() {
    if !provisioning_host() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (paths, index) = seeded(tmp.path(), SDK_VERSION);
    let threads: Vec<_> = (0..6)
        .map(|_| {
            let paths = paths.clone();
            let index = index.clone();
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(ensure_from_index(
                        &paths,
                        &index,
                        HOST,
                        TARGET,
                        SDK_VERSION,
                        false,
                    ))
                    .unwrap()
                    .root
            })
        })
        .collect();
    let roots: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(roots.iter().all(|r| *r == roots[0]));
    assert!(roots[0].join(".complete").is_file());
    assert!(!std::fs::read_dir(roots[0].parent().unwrap())
        .unwrap()
        .any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging-")));
}

#[test]
fn identity_changes_with_content_size_and_sdk_shape() {
    let index = fixture();
    let mut units = select_units(&index, HOST, TARGET, SDK_VERSION, false).unwrap();
    let original = identity(&units);
    units[0].sha256 = "f".repeat(64);
    assert_ne!(identity(&units), original);
    units = select_units(&index, HOST, TARGET, SDK_VERSION, false).unwrap();
    units[0].size_bytes += 1;
    assert_ne!(identity(&units), original);
    assert_ne!(
        identity(&select_units(&index, HOST, TARGET, SDK_VERSION, true).unwrap()),
        original
    );
}
