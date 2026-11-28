use super::*;

fn mark_readonly(path: &Path) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn link_count(path: &Path) -> u64 {
    let file = File::open(path).unwrap();
    let metadata = file.metadata().unwrap();
    hard_link_count(&file, &metadata).unwrap()
}

fn opened_root(path: &Path) -> OpenDirectory {
    open_target_root(path).unwrap().unwrap()
}

/// Create a file or directory symlink, or return false when the
/// platform/session cannot make one (Windows needs Developer Mode or
/// elevation — the symlink tests self-skip rather than failing).
fn try_link(target: &Path, link: &Path, is_dir: bool) -> bool {
    crate::platform::fs::links::create(&target.to_string_lossy(), link, is_dir).is_ok()
}

#[test]
fn readonly_shared_file_is_detached_without_unprotecting_blob() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let target = root.path().join("target");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(target.join("debug/deps")).unwrap();
    let blob = cache.join("blob");
    let output = target.join("debug/deps/libdemo.rmeta");
    std::fs::write(&blob, b"cached bytes").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    mark_readonly(&blob);

    let report = detach_target_tree(&target).unwrap();

    assert_eq!(report.detached_shared, 1);
    assert_eq!(report.made_writable, 0);
    assert_eq!(link_count(&blob), 1);
    assert_eq!(link_count(&output), 1);
    assert!(std::fs::metadata(&blob).unwrap().permissions().readonly());
    assert!(!std::fs::metadata(&output).unwrap().permissions().readonly());
    std::fs::write(&output, b"new compiler bytes").unwrap();
    assert_eq!(std::fs::read(&blob).unwrap(), b"cached bytes");
}

#[test]
fn writable_shared_file_is_detached_to_prevent_cache_poisoning() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let target = root.path().join("target");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    let blob = cache.join("blob");
    let output = target.join("artifact");
    std::fs::write(&blob, b"cached bytes").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();

    let report = detach_target_tree(&target).unwrap();

    assert_eq!(report.detached_shared, 1);
    std::fs::write(&output, b"new compiler bytes").unwrap();
    assert_eq!(std::fs::read(&blob).unwrap(), b"cached bytes");
}

#[test]
fn final_rename_failure_preserves_the_private_copy() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let target = root.path().join("target");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    let blob = cache.join("blob");
    let output = target.join("artifact");
    std::fs::write(&blob, b"cached bytes").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    mark_readonly(&blob);

    let directory = opened_root(&target);
    let error = prepare_file_with_final_rename(
        &directory,
        OsStr::new("artifact"),
        |_directory, _temp_name, _final_name| {
            Err(std::io::Error::other("injected final rename failure"))
        },
    )
    .unwrap_err();

    assert!(!output.exists(), "the shared target alias was removed");
    assert_eq!(link_count(&blob), 1);
    assert!(std::fs::metadata(&blob).unwrap().permissions().readonly());
    let preserved: Vec<PathBuf> = std::fs::read_dir(&target)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with(DETACH_TEMP_PREFIX))
        })
        .collect();
    assert_eq!(preserved.len(), 1);
    assert_eq!(std::fs::read(&preserved[0]).unwrap(), b"cached bytes");
    let message = error.to_string();
    let reported = message
        .split_once("private copy was preserved at ")
        .map(|(_, path)| PathBuf::from(path))
        .unwrap_or_else(|| panic!("missing preserved-copy path in error: {message}"));
    assert_eq!(
        std::fs::canonicalize(&reported).unwrap(),
        std::fs::canonicalize(&preserved[0]).unwrap(),
        "error reported the wrong preserved-copy path: {message}"
    );
}

#[test]
fn private_readonly_file_becomes_writable_without_copy() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let output = target.join("artifact");
    std::fs::write(&output, b"private bytes").unwrap();
    mark_readonly(&output);

    let report = detach_target_tree(&target).unwrap();

    assert_eq!(report.detached_shared, 0);
    assert_eq!(report.made_writable, 1);
    assert!(!std::fs::metadata(&output).unwrap().permissions().readonly());
    assert_eq!(std::fs::read(&output).unwrap(), b"private bytes");
}

#[test]
fn symlinks_are_not_followed() {
    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside");
    let target = root.path().join("target");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    let outside_file = outside.join("artifact");
    std::fs::write(&outside_file, b"outside").unwrap();
    mark_readonly(&outside_file);

    if !try_link(&outside_file, &target.join("link"), false) {
        return;
    }

    let report = detach_target_tree(&target).unwrap();
    assert_eq!(report.scanned_files, 0);
    assert!(std::fs::metadata(&outside_file)
        .unwrap()
        .permissions()
        .readonly());
}

#[test]
fn active_build_lock_refuses_the_preflight() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let lock_path = target.join(".cargo-lock");
    std::fs::write(&lock_path, b"").unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    kernal_api::fs_lock_exclusive(&lock).unwrap();

    let error = detach_target_tree(&target).unwrap_err();
    assert!(error.to_string().contains("build lock"));
}

#[test]
fn persistent_but_unlocked_build_lock_is_allowed() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join(".cargo-lock"), b"").unwrap();

    let report = detach_target_tree(&target).unwrap();
    assert_eq!(report.detached_shared, 0);
}

#[test]
fn acquired_build_locks_remain_held_until_guard_drop() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let lock_path = target.join(".cargo-lock");
    std::fs::write(&lock_path, b"").unwrap();

    let root = opened_root(&target);
    let guards = acquire_build_locks(&root).unwrap();
    assert_eq!(guards.len(), 1);
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    assert!(kernal_api::platform::fs::try_lock_exclusive(&contender).is_err());

    drop(guards);
    let _guard = kernal_api::platform::fs::try_lock_exclusive(&contender).unwrap();
}

#[test]
fn nested_cross_target_build_lock_refuses_the_preflight() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    let profile = target.join("aarch64-pc-windows-msvc/debug");
    std::fs::create_dir_all(&profile).unwrap();
    let lock_path = profile.join(".cargo-lock");
    std::fs::write(&lock_path, b"").unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    kernal_api::fs_lock_exclusive(&lock).unwrap();

    let error = detach_target_tree(&target).unwrap_err();
    assert!(error.to_string().contains("aarch64-pc-windows-msvc"));
}

#[test]
fn detach_temporaries_are_not_scanned_or_mutated() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let stale_temp = target.join(format!("{DETACH_TEMP_PREFIX}stale"));
    std::fs::write(&stale_temp, b"stale").unwrap();
    mark_readonly(&stale_temp);

    let report = detach_target_tree(&target).unwrap();

    assert_eq!(report.scanned_files, 0);
    assert!(std::fs::metadata(&stale_temp)
        .unwrap()
        .permissions()
        .readonly());
}

#[test]
fn vanished_snapshot_entry_is_tolerated() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let vanished = target.join("vanished");
    std::fs::write(&vanished, b"gone").unwrap();
    std::fs::remove_file(&vanished).unwrap();

    let root = opened_root(&target);
    assert_eq!(
        prepare_file(&root, OsStr::new("vanished")).unwrap(),
        PreparedFile::Unchanged
    );
}

#[test]
fn direct_no_follow_open_rejects_a_file_symlink() {
    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside");
    let link = root.path().join("link");
    std::fs::write(&outside, b"outside").unwrap();
    mark_readonly(&outside);

    if !try_link(&outside, &link, false) {
        return;
    }

    let root = opened_root(root.path());
    assert_eq!(
        prepare_file(&root, OsStr::new("link")).unwrap(),
        PreparedFile::Unchanged
    );
    assert!(std::fs::metadata(&outside)
        .unwrap()
        .permissions()
        .readonly());
}

#[test]
fn symlinked_target_root_is_resolved_and_prepared() {
    let root = tempfile::tempdir().unwrap();
    let physical = root.path().join("physical-target");
    let target_link = root.path().join("target-link");
    std::fs::create_dir_all(&physical).unwrap();
    let artifact = physical.join("artifact");
    std::fs::write(&artifact, b"private bytes").unwrap();
    mark_readonly(&artifact);

    if !try_link(&physical, &target_link, true) {
        return;
    }

    let report = detach_target_tree(&target_link).unwrap();

    assert_eq!(report.made_writable, 1);
    assert!(!std::fs::metadata(&artifact)
        .unwrap()
        .permissions()
        .readonly());
}

#[test]
fn opened_directory_capability_survives_ancestor_swap() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    let child = target.join("child");
    let displaced = target.join("displaced-child");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let child_artifact = child.join("artifact");
    let outside_artifact = outside.join("artifact");
    std::fs::write(&child_artifact, b"inside").unwrap();
    std::fs::write(&outside_artifact, b"outside").unwrap();
    mark_readonly(&child_artifact);
    mark_readonly(&outside_artifact);

    let mut attempted = false;
    let report = detach_target_tree_with_hook(&target, |opened| {
        if attempted || opened.file_name() != Some(OsStr::new("child")) {
            return;
        }
        attempted = true;
        match std::fs::rename(&child, &displaced) {
            Ok(()) => {
                // A failed symlink (e.g. no Developer Mode) restores the
                // displaced child so the detach proceeds on the real tree.
                if !try_link(&outside, &child, true) {
                    std::fs::rename(&displaced, &child).unwrap();
                }
            }
            Err(_) => {
                // Windows capability directory handles intentionally omit
                // FILE_SHARE_DELETE, so the attempted swap is rejected.
            }
        }
    })
    .unwrap();

    assert!(attempted);
    assert_eq!(report.made_writable, 1);
    assert!(std::fs::metadata(&outside_artifact)
        .unwrap()
        .permissions()
        .readonly());
    let prepared_inside = if displaced.exists() {
        displaced.join("artifact")
    } else {
        child.join("artifact")
    };
    assert!(!std::fs::metadata(prepared_inside)
        .unwrap()
        .permissions()
        .readonly());
}

#[test]
fn test_cargo_override_uses_default_target_without_metadata() {
    let root = tempfile::tempdir().unwrap();
    let missing_tool = root.path().join("fake-cargo-does-not-exist");
    let child_command = std::process::Command::new(&missing_tool);
    let selected_target = root.path().join("selected-target");
    let fake_cargo = OsStr::new("explicit-fake-cargo");
    let args = ["build".to_string()];

    let resolved = resolve_target_directory_with_env(
        &missing_tool,
        &args,
        &child_command,
        None,
        Some(fake_cargo),
        |_| Some(selected_target.clone()),
    )
    .unwrap();
    assert_eq!(resolved, selected_target);

    let explicit_target = root.path().join("explicit-target");
    let explicit_args = [
        "build".to_string(),
        format!("--target-dir={}", explicit_target.display()),
    ];
    let resolved = resolve_target_directory_with_env(
        &missing_tool,
        &explicit_args,
        &child_command,
        None,
        Some(fake_cargo),
        |_| panic!("explicit target must win before the test seam"),
    )
    .unwrap();
    assert_eq!(resolved, explicit_target);

    let env_target = root.path().join("env-target");
    let resolved = resolve_target_directory_with_env(
        &missing_tool,
        &args,
        &child_command,
        Some(env_target.as_os_str()),
        Some(fake_cargo),
        |_| panic!("environment target must win before the test seam"),
    )
    .unwrap();
    assert_eq!(resolved, env_target);

    let metadata_result =
        resolve_target_directory_with_env(&missing_tool, &args, &child_command, None, None, |_| {
            panic!("production resolution must not use the test seam")
        });
    assert!(metadata_result.is_err());
}
