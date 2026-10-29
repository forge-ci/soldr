//! Unit coverage split from `self_relocate.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use std::{
    ffi::{OsStr, OsString},
    sync::Mutex,
};
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var(self.key, value);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn seed_runtime_dir(root: &Path, name: &str, last_used: u64) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).expect("create runtime dir");
    fs::write(dir.join(LAST_USED_FILENAME), last_used.to_string()).expect("write last-used");
    dir
}

#[test]
fn relocation_guard_prevents_recursive_reexec_even_when_forced() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _force = EnvVarGuard::set(FORCE_RELOCATION_ENV_VAR, "1");
    let _marker = EnvVarGuard::set(RELOCATED_EXE_ENV_VAR, "1");

    let result = maybe_reexec_from_runtime(&["soldr".to_string(), "version".to_string()]).unwrap();

    assert_eq!(result, None);
}

#[test]
fn ensure_relocated_exe_copies_to_hash_keyed_runtime_dir() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("soldr-test.exe");
    fs::write(&source, b"binary-content").expect("write source");
    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

    let relocated = ensure_relocated_exe_in(&runtime_root(&paths), &source).expect("relocate exe");
    let expected_hash = hash_file(&source).expect("hash source");

    assert!(relocated.is_file());
    assert_eq!(
        fs::read(&relocated).expect("read relocated"),
        b"binary-content"
    );
    assert!(relocated
        .parent()
        .expect("relocated exe has parent")
        .file_name()
        .and_then(OsStr::to_str)
        .expect("dir is utf-8")
        .contains(&expected_hash));
    assert!(relocated
        .parent()
        .expect("relocated exe has parent")
        .join(LAST_USED_FILENAME)
        .is_file());

    let second =
        ensure_relocated_exe_in(&runtime_root(&paths), &source).expect("reuse relocated exe");
    assert_eq!(second, relocated);
}

#[test]
fn relocation_dir_name_is_hash_free_only_for_official_builds() {
    // soldr#1597 Phase 3: dev/manual builds keep the hash-keyed name
    // unconditionally (same-version-different-content safety); only
    // an official (auto-release.yml-stamped) build gets the
    // hash-free, version-rooted name.
    assert_eq!(relocation_dir_name("1.2.3", "deadbeef"), "v1.2.3-deadbeef");
}

#[test]
fn ensure_daemon_relocated_copies_into_daemon_subtree() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("soldr-daemon.exe");
    fs::write(&source, b"daemon-bin").expect("write daemon");
    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

    let relocated = ensure_daemon_relocated(&paths, &source).expect("relocate daemon");
    assert!(relocated.is_file());
    assert_eq!(fs::read(&relocated).expect("read relocated"), b"daemon-bin");
    // Sub-tree must be the daemon root, NOT soldr-self.
    let daemon_root = daemon_runtime_root(&paths);
    assert!(
        relocated.starts_with(&daemon_root),
        "relocated path {} not under daemon root {}",
        relocated.display(),
        daemon_root.display(),
    );
    assert!(!relocated.starts_with(runtime_root(&paths)));

    // Calling again with a source already under the daemon root
    // is a no-op (returns the same path).
    let reused = ensure_daemon_relocated(&paths, &relocated).expect("noop relocation");
    assert_eq!(reused, relocated);
}

#[test]
fn daemon_relocation_reports_real_byte_progress() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("soldr-daemon");
    let contents = vec![0x5a_u8; 2 * 1024 * 1024 + 17];
    fs::write(&source, &contents).expect("write daemon");
    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
    let mut events = Vec::new();

    let relocated =
        ensure_daemon_relocated_with_progress(&paths, &source, |stage, completed, total| {
            events.push((stage, completed, total))
        })
        .expect("relocate with progress");

    assert_eq!(fs::read(relocated).expect("read relocated"), contents);

    // `source-hash` always streams: hashing reads every byte regardless of how
    // the image is then published.
    let hash_events: Vec<_> = events
        .iter()
        .filter(|(reported, _, _)| *reported == "source-hash")
        .collect();
    assert!(
        hash_events.len() >= 2,
        "missing streaming source-hash events"
    );
    assert_eq!(
        hash_events.last().map(|(_, done, _)| *done),
        Some(contents.len() as u64)
    );
    assert!(hash_events.iter().all(|(_, done, total)| done <= total));

    // Publication reports either `copy` or `link` (soldr#3138). A hardlink
    // moves no bytes, so demanding streaming `copy` events would be demanding
    // that the slow path be taken. What must hold either way is that the stage
    // reports the full image as completed and never overshoots the total.
    let publish_events: Vec<_> = events
        .iter()
        .filter(|(reported, _, _)| *reported == "copy" || *reported == "link")
        .collect();
    assert!(
        !publish_events.is_empty(),
        "publication reported neither copy nor link progress"
    );
    let published_via_copy = publish_events.iter().any(|(stage, _, _)| *stage == "copy");
    if published_via_copy {
        assert!(
            publish_events.len() >= 2,
            "a byte copy must still stream its progress"
        );
    }
    assert_eq!(
        publish_events.last().map(|(_, done, _)| *done),
        Some(contents.len() as u64),
        "publication must report the whole image as completed"
    );
    assert!(publish_events.iter().all(|(_, done, total)| done <= total));
}

// soldr#1300 — the maturin-repaired macOS wheel layout: binaries
// under `<platlib>/soldr.scripts/` load bundled dylibs via
// `@loader_path/../soldr.dylibs/`. Relocating the daemon out of
// that directory strands the reference and dyld kills it at exec,
// so `ensure_daemon_relocated` must run it in place.
#[test]
fn daemon_in_repaired_wheel_layout_is_not_relocated() {
    let temp = TempDir::new().expect("tempdir");
    let platlib = temp.path().join("site-packages");
    let scripts = platlib.join("soldr.scripts");
    fs::create_dir_all(&scripts).expect("scripts dir");
    fs::create_dir_all(platlib.join("soldr.dylibs")).expect("dylibs dir");
    let daemon = scripts.join("soldr-daemon");
    fs::write(&daemon, b"daemon-bin").expect("write daemon");

    assert!(exe_depends_on_bundled_wheel_libs(&daemon));

    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
    let resolved = ensure_daemon_relocated(&paths, &daemon).expect("resolve daemon");
    assert_eq!(
        resolved, daemon,
        "repaired-wheel daemon must run in place, not from the runtime copy"
    );
    assert!(
        !daemon_runtime_root(&paths).exists()
            || fs::read_dir(daemon_runtime_root(&paths))
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
        "no runtime copy may be materialized for a repaired-wheel daemon"
    );
}

#[test]
fn route_placement_preserves_repaired_wheel_layout() {
    let temp = TempDir::new().expect("tempdir");
    let platlib = temp.path().join("site-packages");
    let scripts = platlib.join("soldr.scripts");
    let libs = platlib.join("soldr.dylibs");
    fs::create_dir_all(&scripts).expect("scripts dir");
    fs::create_dir_all(&libs).expect("dylibs dir");
    let daemon = scripts.join("soldr-daemon");
    fs::write(&daemon, b"daemon-bin").expect("write daemon");
    fs::write(libs.join("libexample.dylib"), b"library").expect("write library");

    let paths = SoldrPaths::with_root(temp.path().join("route-a"));
    let resolved = ensure_daemon_relocated_for_route_with_progress(&paths, &daemon, |_, _, _| {})
        .expect("place route daemon");

    assert_ne!(resolved, daemon);
    assert_eq!(
        resolved.parent().and_then(Path::file_name),
        Some(OsStr::new("soldr.scripts"))
    );
    let placed_root = resolved
        .parent()
        .and_then(Path::parent)
        .expect("placed bundle root");
    assert_eq!(
        fs::read(placed_root.join("soldr.dylibs/libexample.dylib")).expect("read placed library"),
        b"library"
    );
}

// The auditwheel spelling (`<pkg>.libs`) is covered pre-emptively.
#[test]
fn repaired_wheel_detection_accepts_auditwheel_libs_dir() {
    let temp = TempDir::new().expect("tempdir");
    let scripts = temp.path().join("soldr.scripts");
    fs::create_dir_all(&scripts).expect("scripts dir");
    fs::create_dir_all(temp.path().join("soldr.libs")).expect("libs dir");
    let daemon = scripts.join("soldr-daemon");
    fs::write(&daemon, b"daemon-bin").expect("write daemon");

    assert!(exe_depends_on_bundled_wheel_libs(&daemon));
}

/// Build a minimal 64-bit little-endian Mach-O whose load-command
/// region contains `payload`. Enough to exercise the parser on every
/// platform — the real question is header arithmetic, not linking.
fn synthetic_macho_le64(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0xfeed_facfu32.to_le_bytes()); // magic
    out.extend_from_slice(&0u32.to_le_bytes()); // cputype
    out.extend_from_slice(&0u32.to_le_bytes()); // cpusubtype
    out.extend_from_slice(&2u32.to_le_bytes()); // filetype
    out.extend_from_slice(&1u32.to_le_bytes()); // ncmds
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // sizeofcmds
    out.extend_from_slice(&0u32.to_le_bytes()); // flags
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    out.extend_from_slice(payload);
    out
}

// #1908: ask the binary, not the directory it happens to sit in.
#[test]
fn loader_path_reference_is_detected_in_load_commands() {
    let temp = TempDir::new().expect("tempdir");
    let exe = temp.path().join("repaired");
    fs::write(
        &exe,
        synthetic_macho_le64(b"\x0c\x00\x00\x00@loader_path/../soldr.dylibs/liblzma.dylib\x00"),
    )
    .expect("write");
    assert!(
        exe_has_loader_path_reference(&exe),
        "a load command naming @loader_path must be detected"
    );
}

// The scan must stay inside the load commands. `@loader_path` is a
// perfectly ordinary string for a binary to carry in its data --
// soldr's own source mentions it -- and matching that would condemn
// unrelated binaries to the slow trampoline path.
#[test]
fn loader_path_in_the_body_is_not_a_reference() {
    let temp = TempDir::new().expect("tempdir");
    let exe = temp.path().join("innocent");
    let mut bytes = synthetic_macho_le64(b"\x0c\x00\x00\x00/usr/lib/libSystem.dylib\x00");
    bytes.extend_from_slice(b"... @loader_path mentioned in a data section ...");
    fs::write(&exe, bytes).expect("write");
    assert!(
        !exe_has_loader_path_reference(&exe),
        "a data-section mention must not count"
    );
}

// Unparseable input must fall back to today's behaviour (hardlink),
// never panic: these run inside shim writers on every platform.
#[test]
fn non_macho_inputs_are_not_position_dependent() {
    let temp = TempDir::new().expect("tempdir");

    let script = temp.path().join("script");
    fs::write(&script, b"#!/bin/sh\nexec soldr cargo \"$@\"\n").expect("write");
    assert!(!exe_has_loader_path_reference(&script));

    let elf = temp.path().join("elf");
    fs::write(&elf, b"\x7fELF\x02\x01\x01\x00 @loader_path").expect("write");
    assert!(!exe_has_loader_path_reference(&elf));

    let empty = temp.path().join("empty");
    fs::write(&empty, b"").expect("write");
    assert!(!exe_has_loader_path_reference(&empty));

    // Truncated: header claims more load commands than exist.
    let truncated = temp.path().join("truncated");
    let mut bytes = synthetic_macho_le64(b"@loader_path/x");
    bytes.truncate(20);
    fs::write(&truncated, bytes).expect("write");
    assert!(!exe_has_loader_path_reference(&truncated));

    assert!(!exe_has_loader_path_reference(&temp.path().join("nope")));
}

// A universal binary hides its slices behind an offset table, so the
// parser has to follow them; a naive whole-file scan would pass this
// test for the wrong reason, hence the offset padding.
#[test]
fn fat_binary_slices_are_followed() {
    let temp = TempDir::new().expect("tempdir");
    let exe = temp.path().join("universal");

    let slice = synthetic_macho_le64(b"\x0c\x00\x00\x00@loader_path/../soldr.dylibs/x\x00");
    let slice_offset: u32 = 4096;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0xcafe_babeu32.to_be_bytes()); // FAT_MAGIC
    bytes.extend_from_slice(&1u32.to_be_bytes()); // nfat_arch
    bytes.extend_from_slice(&0u32.to_be_bytes()); // cputype
    bytes.extend_from_slice(&0u32.to_be_bytes()); // cpusubtype
    bytes.extend_from_slice(&slice_offset.to_be_bytes()); // offset
    bytes.extend_from_slice(&(slice.len() as u32).to_be_bytes()); // size
    bytes.extend_from_slice(&0u32.to_be_bytes()); // align
    bytes.resize(slice_offset as usize, 0);
    bytes.extend_from_slice(&slice);
    fs::write(&exe, bytes).expect("write");

    assert!(
        exe_has_loader_path_reference(&exe),
        "fat slices must be followed through the offset table"
    );
}

#[test]
fn plain_layouts_are_still_relocated() {
    let temp = TempDir::new().expect("tempdir");

    // `.scripts` dir WITHOUT a sibling bundle dir → not repaired
    // (nothing @loader_path-relative to strand): relocate normally.
    let scripts_only = temp.path().join("a").join("soldr.scripts");
    fs::create_dir_all(&scripts_only).expect("scripts dir");
    let daemon = scripts_only.join("soldr-daemon");
    fs::write(&daemon, b"daemon-bin").expect("write daemon");
    assert!(!exe_depends_on_bundled_wheel_libs(&daemon));

    // Bundle dir with a mismatched package prefix → unrelated.
    let other = temp.path().join("b").join("soldr.scripts");
    fs::create_dir_all(&other).expect("scripts dir");
    fs::create_dir_all(temp.path().join("b").join("otherpkg.dylibs")).expect("dylibs dir");
    let daemon_b = other.join("soldr-daemon");
    fs::write(&daemon_b, b"daemon-bin").expect("write daemon");
    assert!(!exe_depends_on_bundled_wheel_libs(&daemon_b));

    // Ordinary sibling layout (dev target/, venv bin/) → relocate.
    let plain = temp.path().join("bin").join("soldr-daemon");
    fs::create_dir_all(plain.parent().unwrap()).expect("bin dir");
    fs::write(&plain, b"daemon-bin").expect("write daemon");
    assert!(!exe_depends_on_bundled_wheel_libs(&plain));

    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
    let relocated = ensure_daemon_relocated(&paths, &plain).expect("relocate daemon");
    assert_ne!(relocated, plain, "plain layout must still relocate");
    assert!(relocated.starts_with(daemon_runtime_root(&paths)));
}

#[test]
fn runtime_gc_removes_stale_dirs_and_skips_current_and_fresh() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("runtime").join("soldr-self");
    fs::create_dir_all(&root).expect("create runtime root");
    let stale = seed_runtime_dir(&root, "stale", 10);
    let fresh = seed_runtime_dir(&root, "fresh", 90);
    let current = seed_runtime_dir(&root, "current", 10);

    let summary = purge_stale_runtime_copies(&root, Some(&current), 100, 50).expect("runtime gc");

    assert_eq!(summary.scanned_dirs, 3);
    assert_eq!(summary.removed_dirs, 1);
    assert_eq!(summary.skipped_current_dirs, 1);
    assert_eq!(summary.skipped_fresh_dirs, 1);
    assert!(!stale.exists());
    assert!(fresh.exists());
    assert!(current.exists());
}

#[test]
fn runtime_gc_treats_hash_free_dirs_identically_to_hash_keyed_dirs() {
    // soldr#1597 Phase 4: purge_stale_runtime_copies operates on any
    // directory under the runtime root by ledger stamp alone, with no
    // dependence on the naming scheme. A hash-free `v{VERSION}/` dir
    // (official builds, Phase 3) must GC exactly like a hash-keyed
    // `v{VERSION}-{hash}/` dir (dev builds) — no code change needed
    // here, this locks that in as a regression test.
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("runtime").join("soldr-daemon");
    fs::create_dir_all(&root).expect("create runtime root");
    let stale_hash_keyed = seed_runtime_dir(&root, "v0.8.5-deadbeef", 10);
    let stale_hash_free = seed_runtime_dir(&root, "v0.8.5", 10);
    let fresh_hash_free = seed_runtime_dir(&root, "v0.9.0", 90);

    let summary = purge_stale_runtime_copies(&root, None, 100, 50).expect("runtime gc");

    assert_eq!(summary.scanned_dirs, 3);
    assert_eq!(summary.removed_dirs, 2);
    assert_eq!(summary.skipped_fresh_dirs, 1);
    assert!(!stale_hash_keyed.exists());
    assert!(!stale_hash_free.exists());
    assert!(fresh_hash_free.exists());
}

#[test]
fn periodic_runtime_gc_respects_marker_interval() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("runtime").join("soldr-self");
    fs::create_dir_all(&root).expect("create runtime root");
    let stale = seed_runtime_dir(&root, "stale", 10);
    fs::write(root.join(GC_MARKER_FILENAME), "95").expect("write gc marker");

    let summary = maybe_run_periodic_gc_at(&root, None, 100, 10, 50).expect("periodic gc");

    assert!(summary.is_none());
    assert!(stale.exists());
}

#[test]
fn periodic_runtime_gc_deletes_stale_dirs_when_due() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("runtime").join("soldr-self");
    fs::create_dir_all(&root).expect("create runtime root");
    let stale = seed_runtime_dir(&root, "stale", 10);
    fs::write(root.join(GC_MARKER_FILENAME), "80").expect("write gc marker");

    let summary = maybe_run_periodic_gc_at(&root, None, 100, 10, 50)
        .expect("periodic gc")
        .expect("gc should run");

    assert_eq!(summary.removed_dirs, 1);
    assert!(!stale.exists());
    assert_eq!(
        fs::read_to_string(root.join(GC_MARKER_FILENAME)).expect("read marker"),
        "100"
    );
}

fn seed_route_copy(routes_root: &Path, route: &str, version: &str, last_used: u64) -> PathBuf {
    seed_runtime_dir(
        &routes_root.join(route).join(RUNTIME_DIR).join(DAEMON_DIR),
        version,
        last_used,
    )
}

// soldr#3164: every route's stale daemon copies are reclaimed, fresh ones kept.
#[test]
fn route_sweep_reclaims_stale_copies_across_every_route() {
    let temp = TempDir::new().expect("tempdir");
    let routes = temp.path().join("routes");
    let stale_a = seed_route_copy(&routes, "soldr-daemon-a", "v0.9.11", 10);
    let fresh_a = seed_route_copy(&routes, "soldr-daemon-a", "v0.9.14", 95);
    let stale_b = seed_route_copy(&routes, "soldr-daemon-b", "v0.9.12", 20);

    let summary = sweep_route_runtime_copies_at(&routes, 100, 10, 50);

    assert_eq!(summary.removed_dirs, 2);
    assert!(!stale_a.exists());
    assert!(!stale_b.exists());
    assert!(
        fresh_a.exists(),
        "a copy used within the stale window is kept"
    );
}

#[test]
fn route_sweep_honours_each_routes_gc_interval() {
    let temp = TempDir::new().expect("tempdir");
    let routes = temp.path().join("routes");
    let stale = seed_route_copy(&routes, "soldr-daemon-a", "v0.9.11", 10);
    let runtime = routes
        .join("soldr-daemon-a")
        .join(RUNTIME_DIR)
        .join(DAEMON_DIR);
    fs::write(runtime.join(GC_MARKER_FILENAME), "95").expect("recent gc marker");

    let summary = sweep_route_runtime_copies_at(&routes, 100, 10, 50);

    assert_eq!(summary.removed_dirs, 0);
    assert!(
        stale.exists(),
        "a route swept within the interval is not rescanned"
    );
}

#[test]
fn route_sweep_leaves_non_route_entries_untouched() {
    let temp = TempDir::new().expect("tempdir");
    let routes = temp.path().join("routes");
    fs::create_dir_all(routes.join("soldr-daemon-empty")).expect("route without runtime");
    fs::write(routes.join("stray-file"), b"x").expect("stray file");

    let summary = sweep_route_runtime_copies_at(&routes, 100, 10, 50);

    assert_eq!(summary.removed_dirs, 0);
    assert!(
        !routes.join("soldr-daemon-empty").join(RUNTIME_DIR).exists(),
        "a sweep must not create runtime trees"
    );
    assert!(routes.join("stray-file").is_file());
    // A missing routes root is simply nothing to do.
    let absent = sweep_route_runtime_copies_at(&temp.path().join("absent"), 100, 10, 50);
    assert_eq!(absent.removed_dirs, 0);
}

// soldr#3251: the broker's daemon-disk sweep ages a whole route by its newest
// image ledger, read through the same layout the runtime sweep uses.
#[test]
fn route_image_last_used_is_the_newest_ledger_in_the_route() {
    let temp = TempDir::new().expect("tempdir");
    let routes = temp.path().join("routes");
    seed_route_copy(&routes, "soldr-daemon-a", "v0.9.11", 10);
    seed_route_copy(&routes, "soldr-daemon-a", "v0.9.14", 95);
    fs::create_dir_all(routes.join("soldr-daemon-empty")).expect("route without images");

    assert_eq!(
        route_image_last_used(&routes.join("soldr-daemon-a")),
        Some(95)
    );
    assert_eq!(
        route_image_last_used(&routes.join("soldr-daemon-empty")),
        None
    );
    assert_eq!(route_image_last_used(&routes.join("absent")), None);
}

// soldr#3164: a long-running daemon keeps its own image out of the sweep.
#[test]
fn running_image_ledger_is_refreshed_only_where_one_exists() {
    let temp = TempDir::new().expect("tempdir");
    let placed = seed_runtime_dir(temp.path(), "v0.9.14", 10);
    let placed_exe = placed.join("soldr-daemon");
    fs::write(&placed_exe, b"image").expect("placed image");
    let bare = temp.path().join("target-debug");
    fs::create_dir_all(&bare).expect("bare dir");
    let bare_exe = bare.join("soldr-daemon");
    fs::write(&bare_exe, b"image").expect("bare image");

    refresh_running_image_ledger(&placed_exe);
    refresh_running_image_ledger(&bare_exe);

    let stamped: u64 = fs::read_to_string(placed.join(LAST_USED_FILENAME))
        .expect("ledger")
        .trim()
        .parse()
        .expect("unix seconds");
    assert!(stamped > 10, "the running image's ledger must be refreshed");
    assert!(
        !bare.join(LAST_USED_FILENAME).exists(),
        "no ledger may be created beside a binary that never had one"
    );
}

// soldr#1495 Workstream C: the version-residue window is 48h.
#[test]
fn stale_runtime_threshold_is_48_hours() {
    assert_eq!(STALE_RUNTIME_SECONDS, 48 * 60 * 60);
}

// soldr#1495 GHA safety: the GC is strictly ledger-based. A dir whose
// filesystem mtime is ancient but whose `last-used` stamp is fresh
// must be KEPT — mtimes lie after an archive restore, the ledger
// stamp is authoritative.
#[test]
fn gc_trusts_ledger_stamp_not_mtime() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("runtime").join("soldr-self");
    fs::create_dir_all(&root).expect("create runtime root");
    // Fresh ledger stamp (95), well within a 50s window ending at now=100.
    let restored = seed_runtime_dir(&root, "restored", 95);
    // The dir itself is old on disk, but the ledger says fresh.
    let old_stamp = seed_runtime_dir(&root, "genuinely-old", 10);

    let summary = purge_stale_runtime_copies(&root, None, 100, 50).expect("runtime gc");

    assert!(
        restored.exists(),
        "a dir with a fresh ledger stamp must be kept regardless of mtime"
    );
    assert!(
        !old_stamp.exists(),
        "a dir with a stale ledger stamp must be collected"
    );
    assert_eq!(summary.removed_dirs, 1);
    assert_eq!(summary.skipped_fresh_dirs, 1);
}

// soldr#1495 GHA safety: a dir with NO ledger stamp is never
// mtime-aged-out. It is self-healed with a fresh stamp and kept this
// round (becomes eligible 48h later if it stays unused), so a
// freshly-materialized-but-unstamped dir can never be swept mid-build.
#[test]
fn gc_self_heals_unstamped_dir_instead_of_mtime_aging() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("runtime").join("soldr-self");
    fs::create_dir_all(&root).expect("create runtime root");
    // Dir with content but no `last-used` file at all.
    let unstamped = root.join("no-stamp");
    fs::create_dir_all(&unstamped).expect("create dir");
    fs::write(unstamped.join("soldr.exe"), b"bin").expect("write bin");

    let summary = purge_stale_runtime_copies(&root, None, 1_000_000, 50).expect("runtime gc");

    assert!(unstamped.exists(), "unstamped dir must not be collected");
    assert_eq!(summary.stamped_dirs, 1);
    assert_eq!(summary.removed_dirs, 0);
    let stamp =
        fs::read_to_string(unstamped.join(LAST_USED_FILENAME)).expect("self-healed stamp written");
    assert_eq!(stamp, "1000000", "self-heal stamps `now`");
}

// soldr#1495 GHA safety, structural: the version-residue GC operates
// only on the `runtime/` sub-tree under `~/.soldr/`. The compile
// cache (`~/.soldr/cache/`, what CI rehydrates via `soldr load`)
// lives in a sibling tree and is never walked, so a rehydrated warm
// cache — however old its saved timestamps — is untouched.
#[test]
fn gc_never_touches_the_compile_cache_tree() {
    let temp = TempDir::new().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

    // A rehydrated compile cache with an ancient stamp/mtime.
    let cache_file = paths.cache.join("artifacts").join("blob.bin");
    fs::create_dir_all(cache_file.parent().unwrap()).expect("cache dir");
    fs::write(&cache_file, b"warm-artifact").expect("write cache");

    // A genuinely stale runtime copy that SHOULD be collected.
    let self_root = runtime_root(&paths);
    fs::create_dir_all(&self_root).expect("self root");
    let stale = seed_runtime_dir(&self_root, "v0.0.0-old", 10);

    // The self-runtime GC root and the cache root are disjoint sub-trees.
    assert!(
        !runtime_root(&paths).starts_with(&paths.cache)
            && !daemon_runtime_root(&paths).starts_with(&paths.cache),
        "runtime GC roots must live outside the compile cache tree"
    );

    purge_stale_runtime_copies(&self_root, None, 100, 50).expect("runtime gc");

    assert!(
        cache_file.exists(),
        "the compile cache must never be touched by the runtime GC"
    );
    assert!(!stale.exists(), "the stale runtime copy is still collected");
}

#[cfg(test)]
mod reexec_hop_tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // Shares the module's env barrier: these read/write the marker variable
    // that the tests above also disturb.
    static HOP_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fake_daemon(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let exe = dir.join(name);
        std::fs::write(&exe, b"daemon-bin").expect("write");
        exe
    }

    // THE safety property. If the "already relocated?" answer were ever wrong
    // in the direction of "no", an unguarded hop would re-exec forever and no
    // daemon would ever start. The marker makes that impossible regardless of
    // how the predicate behaves, so the worst case is the status quo -- running
    // from wherever we were launched.
    #[test]
    fn the_marker_stops_a_second_hop_whatever_the_predicate_says() {
        let _lock = HOP_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        // Deliberately outside the runtime root: without the marker this is
        // exactly the case that WOULD hop.
        let exe = fake_daemon(&temp.path().join("elsewhere"), "soldr-daemon.exe");
        assert!(
            daemon_should_reexec(&paths, &exe).is_some(),
            "precondition: this image should hop when unmarked"
        );

        std::env::set_var(DAEMON_REEXEC_MARKER_ENV_VAR, "1");
        let decision = daemon_should_reexec(&paths, &exe);
        std::env::remove_var(DAEMON_REEXEC_MARKER_ENV_VAR);

        assert!(
            decision.is_none(),
            "a marked process must never hop again -- that is the loop guard"
        );
    }

    // An image already under the runtime root is where we want it; hopping
    // would be a pointless exec of ourselves.
    #[test]
    fn an_already_relocated_daemon_does_not_hop() {
        let _lock = HOP_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(DAEMON_REEXEC_MARKER_ENV_VAR);
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let exe = fake_daemon(&daemon_runtime_root(&paths).join("abc"), "soldr-daemon.exe");
        assert!(daemon_should_reexec(&paths, &exe).is_none());
    }

    // soldr#1300: a maturin-repaired wheel resolves bundled dylibs relative to
    // its own location, so relocating strands them and the daemon dies before
    // main(). It must run in place even though it is outside the runtime root.
    #[test]
    fn a_repaired_wheel_layout_runs_in_place() {
        let _lock = HOP_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(DAEMON_REEXEC_MARKER_ENV_VAR);
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let platlib = temp.path().join("platlib");
        std::fs::create_dir_all(platlib.join("soldr.dylibs")).expect("dylibs");
        let exe = fake_daemon(&platlib.join("soldr.scripts"), "soldr-daemon");
        assert!(
            daemon_should_reexec(&paths, &exe).is_none(),
            "relocating a repaired wheel strands its bundled dylibs (soldr#1300)"
        );
    }

    // The #1987 case itself: a temp-dir image that is free to move.
    #[test]
    fn a_temp_dir_daemon_hops_into_the_runtime_root() {
        let _lock = HOP_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(DAEMON_REEXEC_MARKER_ENV_VAR);
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let exe = fake_daemon(
            &temp.path().join("uv-build-tmp").join("Scripts"),
            "soldr-daemon.exe",
        );

        let target = daemon_should_reexec(&paths, &exe).expect("should hop");
        assert!(
            path_is_under(&target, &daemon_runtime_root(&paths)),
            "must land under the runtime root, got {}",
            target.display()
        );
        assert_ne!(
            target, exe,
            "hopping to the same path would be a no-op exec"
        );
    }
}

/// soldr#3138: staging the daemon runtime image must not copy ~105 MB when a
/// hardlink will do. The destination is content-addressed and the image is
/// executed rather than written, so a link is safe -- see
/// `link_or_copy_with_progress`.
///
/// Hardlink support is probed at runtime rather than assumed from a `cfg`:
/// `soldr-core` is inside the platform-cfg boundary (`ban_platform_cfg_outside_
/// boundary`), and the property under test is "did staging take the link path",
/// which the reported progress stage states directly on every platform.
#[test]
fn staging_hardlinks_instead_of_copying_when_possible() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Probe: can this filesystem hardlink at all? If not, the link path is
    // legitimately unavailable and there is nothing to assert.
    let probe_src = dir.path().join("probe");
    let probe_dst = dir.path().join("probe-link");
    fs::write(&probe_src, b"probe").expect("write probe");
    if fs::hard_link(&probe_src, &probe_dst).is_err() {
        return;
    }

    let source = dir.path().join("soldr-daemon");
    fs::write(&source, b"pretend this is 105 MB of daemon").expect("write source");
    let target = dir.path().join("staged");

    let mut seen: Vec<&'static str> = Vec::new();
    super::link_or_copy_with_progress(&source, &target, &mut |label, _done, _total| {
        if !seen.contains(&label) {
            seen.push(label);
        }
    })
    .expect("stage the image");

    assert_eq!(
        fs::read(&source).expect("read source"),
        fs::read(&target).expect("read target"),
        "the staged image must have the source's bytes",
    );
    assert_eq!(
        seen,
        vec!["link"],
        "same-filesystem staging must hardlink, not copy: a byte copy of the \
         daemon image is ~105 MB of read plus ~105 MB of write per route",
    );
}

/// The fallback must be the previous behaviour exactly, so a filesystem that
/// cannot link (or a cross-device destination, `EXDEV`) is never worse off.
#[test]
fn staging_falls_back_to_a_byte_copy_when_the_link_cannot_be_made() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("soldr-daemon");
    std::fs::write(&source, b"payload").expect("write source");

    // An existing target makes `hard_link` fail with EEXIST, which is the same
    // shape the caller must survive as EXDEV on a cross-volume runtime root.
    let target = dir.path().join("staged");
    std::fs::write(&target, b"stale").expect("seed a stale target");

    let mut labels: Vec<&'static str> = Vec::new();
    super::link_or_copy_with_progress(&source, &target, &mut |label, _d, _t| {
        if !labels.contains(&label) {
            labels.push(label);
        }
    })
    .expect("fall back to copying");

    assert_eq!(
        std::fs::read(&target).expect("read target"),
        b"payload",
        "the fallback must still publish the source bytes",
    );
    assert_eq!(
        labels,
        vec!["copy"],
        "the fallback must report copy progress"
    );
}

// soldr#3831: a caller-supplied digest keys the placement and verifies the
// placed copy, so the image is never re-read with SHA-256.
#[test]
fn route_placement_uses_the_supplied_digest() {
    fn fixed_digest(
        _: &Path,
        _: &'static str,
        _: &mut dyn FnMut(&'static str, u64, u64),
    ) -> Result<String, SoldrError> {
        Ok("d".repeat(64))
    }
    let temp = TempDir::new().expect("tempdir");
    let daemon = temp.path().join("soldr-daemon");
    fs::write(&daemon, b"daemon-bin").expect("write daemon");
    let paths = SoldrPaths::with_root(temp.path().join("route-a"));

    let mut stages = Vec::new();
    let placed = ensure_daemon_relocated_for_route_with_digest(
        &paths,
        &daemon,
        fixed_digest,
        |stage, _, _| stages.push(stage),
    )
    .expect("place route daemon");
    assert!(!stages.contains(&"source-hash"), "{stages:?}");
    let dir_name = placed.parent().and_then(Path::file_name).unwrap();
    assert!(dir_name.to_string_lossy().ends_with(&"d".repeat(64)));

    let reused =
        ensure_daemon_relocated_for_route_with_digest(&paths, &daemon, fixed_digest, |_, _, _| {})
            .expect("reuse placed daemon");
    assert_eq!(reused, placed);
}
