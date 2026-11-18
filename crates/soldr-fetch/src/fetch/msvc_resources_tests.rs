use super::*;

fn fixture() -> ManifestIndex {
    ManifestIndex::from_v2_json(include_str!(
        "../../tests/fixtures/msvc-compiler-rt.v2.json"
    ))
    .unwrap()
}

#[test]
fn fixture_catalogue_requires_matching_windows_unit_and_source_identity() {
    let mut index = fixture();
    assert_eq!(
        select_unit(&index).unwrap().asset,
        "compiler-rt/21.1.5/windows-x64/bundle.tar.zst"
    );
    index.entries[0].source_path = Some("compiler-rt/21.1.6/windows-x64/bundle.tar.zst".into());
    assert!(select_unit(&index).is_err());
    assert!(select_unit(&ManifestIndex::empty()).is_err());
}

fn seed(root: &Path, complete: bool) -> (SoldrPaths, PathBuf, ManifestIndex) {
    let paths = SoldrPaths::with_root(root.join("root"));
    let resource = root.join("llvm/lib/clang/21");
    for (path, data) in [
        ("include/intrin.h", "original headers"),
        ("lib/linux/builtins.a", "Linux"),
        ("lib/darwin/builtins.a", "Darwin"),
    ] {
        let path = resource.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }
    let encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
    let mut tar = tar::Builder::new(encoder);
    let mut files = vec![
        ("manifest.json".to_string(), b"provenance".as_slice()),
        (
            "licenses/compiler-rt.txt".to_string(),
            b"license".as_slice(),
        ),
    ];
    if complete {
        files.push((format!("package/lib/clang/21/{BUILTINS}"), b"COFF builtins"));
    }
    for (name, data) in files {
        let mut header = tar::Header::new_gnu();
        let name = format!("./{name}");
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, data).unwrap();
    }
    let bytes = tar.into_inner().unwrap().finish().unwrap();
    let mut index = fixture();
    let entry = &mut index.entries[0];
    entry.sha256 = super::super::trust::sha256_of(&bytes);
    entry.size_bytes = bytes.len() as u64;
    let cache = paths.cache.join("catalogue-v2/assets");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join(&entry.sha256), bytes).unwrap();
    (paths, resource, index)
}

#[tokio::test]
async fn overlay_keeps_headers_and_other_targets_and_reuses_published_tree() {
    let temp = tempfile::tempdir().unwrap();
    let (paths, original, index) = seed(temp.path(), true);
    let resource = ensure_from_index(&paths, &original, &index).await.unwrap();
    assert!(resource.starts_with(paths.root.join("sdk")));
    assert_eq!(
        std::fs::read_to_string(resource.join("include/intrin.h")).unwrap(),
        "original headers"
    );
    for target in ["linux", "darwin"] {
        assert_eq!(
            std::fs::read(resource.join(format!("lib/{target}/builtins.a"))).unwrap(),
            std::fs::read(original.join(format!("lib/{target}/builtins.a"))).unwrap()
        );
    }
    assert!(resource.join(BUILTINS).is_file());
    assert!(!original.join(BUILTINS).exists());
    let modified = std::fs::metadata(resource.join(BUILTINS))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        ensure_from_index(&paths, &original, &index).await.unwrap(),
        resource
    );
    assert_eq!(
        std::fs::metadata(resource.join(BUILTINS))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
    assert!(resource
        .ancestors()
        .nth(4)
        .unwrap()
        .join("compiler-rt-input/licenses/compiler-rt.txt")
        .is_file());
}

#[tokio::test]
async fn missing_builtins_never_publishes() {
    let temp = tempfile::tempdir().unwrap();
    let (paths, original, index) = seed(temp.path(), false);
    assert!(ensure_from_index(&paths, &original, &index).await.is_err());
    let parent = paths.root.join("sdk/windows-msvc/clang-resources");
    assert!(std::fs::read_dir(parent)
        .unwrap()
        .all(|e| !e.unwrap().path().is_dir()));
}

#[test]
fn concurrent_slots_publish_one_complete_tree() {
    let temp = tempfile::tempdir().unwrap();
    let (paths, resource, index) = seed(temp.path(), true);
    let threads: Vec<_> = (0..6)
        .map(|_| {
            let (paths, resource, index) = (paths.clone(), resource.clone(), index.clone());
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(ensure_from_index(&paths, &resource, &index))
                    .unwrap()
            })
        })
        .collect();
    let roots: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(roots
        .iter()
        .all(|r| r == &roots[0] && r.join(BUILTINS).is_file()));
}
