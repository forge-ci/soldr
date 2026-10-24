use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const RELOCATED_EXE_ENV_VAR: &str = "SOLDR_RELOCATED_EXE";

/// One-hop marker for a first-party soldr-spawns-soldr edge (soldr#2739).
///
/// Distinct from [`RELOCATED_EXE_ENV_VAR`] on purpose. That one is
/// deliberately *persistent* -- `relocation_guard_active` reads it to
/// avoid relocating a second time -- so it is inherited by every
/// descendant. Sanctioning the guard on it would exempt the entire
/// process subtree beneath a relocated soldr, permanently.
///
/// This marker is consumed by the re-entrancy guard immediately after
/// it judges the entry, so it authorizes exactly the one hop it
/// describes and never reaches a grandchild.
///
/// Shared by every bounded first-party self-spawn: relocation, the
/// `-Zthreads` fallback retry, and the cargo-timeout no-cache retry. Each
/// site keeps its own recursion sentinel (`SOLDR_RELOCATED_EXE`,
/// `SOLDR_INTERNAL_ZTHREADS_FALLBACK_ATTEMPTED`, the timeout-retry
/// disable flag); this one says only *how* the child was entered.
pub const SELF_SPAWN_EDGE_ENV_VAR: &str = "SOLDR_INTERNAL_SELF_SPAWN_EDGE";
pub const ORIGINAL_EXE_ENV_VAR: &str = "SOLDR_ORIGINAL_EXE";
pub(crate) const FORCE_RELOCATION_ENV_VAR: &str = "SOLDR_TEST_SELF_RELOCATE_FORCE";

const RUNTIME_DIR: &str = "runtime";
const SELF_DIR: &str = "soldr-self";
/// Sibling of `SELF_DIR` used by the soldr-daemon trampoline. Same
/// copy / hash / GC machinery, different sub-tree so a stale daemon
/// runtime can be reaped without touching the soldr-self copies.
const DAEMON_DIR: &str = "soldr-daemon";
const LOCK_FILENAME: &str = ".lock";
const GC_MARKER_FILENAME: &str = ".last-gc";
const LAST_USED_FILENAME: &str = "last-used";
const GC_INTERVAL_SECONDS: u64 = 24 * 60 * 60;
/// Non-current version-residue (relocated soldr / soldr-daemon binary
/// copies keyed by `v{VERSION}-{hash}`) is collected after 48h of
/// non-use (soldr#1495 Workstream C). These are cheap to re-materialize
/// (a single file copy on next use), so a tight window keeps
/// `~/.soldr/runtime/` from accreting stale toolchain versions without
/// costing anything on the rare re-use of an older binary. The current
/// version's dir is always exempt (see `purge_stale_runtime_copies`).
const STALE_RUNTIME_SECONDS: u64 = 48 * 60 * 60;

#[derive(Debug, Default, PartialEq, Eq)]
struct RuntimeGcSummary {
    scanned_dirs: usize,
    removed_dirs: usize,
    skipped_current_dirs: usize,
    skipped_fresh_dirs: usize,
    /// Dirs that had no `last-used` ledger stamp and were self-healed
    /// with a fresh stamp instead of being aged out (soldr#1495): the
    /// GC is strictly ledger-based, never mtime-based, so an unstamped
    /// dir is treated as freshly-used this round rather than trusted to
    /// a filesystem mtime (which lies after archive restores).
    stamped_dirs: usize,
    failed_dirs: usize,
}

pub fn maybe_reexec_from_runtime(raw_args: &[String]) -> Result<Option<i32>, SoldrError> {
    if !relocation_requested() || relocation_guard_active() {
        return Ok(None);
    }

    let current_exe = std::env::current_exe()?;
    let paths = SoldrPaths::new()?;
    let runtime_root = runtime_root(&paths);
    fs::create_dir_all(&runtime_root)?;

    if path_is_under(&current_exe, &runtime_root) {
        return Ok(None);
    }

    let relocated_exe = ensure_relocated_exe_in(&runtime_root, &current_exe)?;
    run_periodic_runtime_gc(&paths, Some(&relocated_exe));

    let mut command = Command::new(&relocated_exe);
    command
        .args(raw_args.iter().skip(1))
        .env(RELOCATED_EXE_ENV_VAR, "1")
        .env(SELF_SPAWN_EDGE_ENV_VAR, "1")
        .env(ORIGINAL_EXE_ENV_VAR, &current_exe);
    suppress_windows_console_window(&mut command);
    let status =
        crate::platform::wait::status_for(&mut command, crate::platform::wait::WHOLE_COMMAND)?;
    Ok(Some(status.code().unwrap_or(1)))
}

pub(crate) fn run_periodic_runtime_gc(paths: &SoldrPaths, current_exe: Option<&Path>) {
    run_periodic_gc_in(&runtime_root(paths), current_exe);
}

/// Copy `daemon_src` into `~/.soldr/runtime/soldr-daemon/<hash>/` and
/// return the relocated path. Reuses the soldr-self relocation
/// machinery so the daemon running in long-lived processes is
/// decoupled from the source path (worktree, cargo target/, package
/// installer) — uninstall/upgrade/rm of the source no longer needs to
/// wait for the daemon to exit. Returns `daemon_src` unchanged if the
/// source already lives under the daemon-runtime root.
pub fn ensure_daemon_relocated(
    paths: &SoldrPaths,
    daemon_src: &Path,
) -> Result<PathBuf, SoldrError> {
    ensure_daemon_relocated_with_progress(paths, daemon_src, |_, _, _| {})
}

/// [`ensure_daemon_relocated`] with observable byte progress for callers that
/// must keep a bounded route-acquisition connection alive while hashing or
/// copying a large daemon image.
pub fn ensure_daemon_relocated_with_progress(
    paths: &SoldrPaths,
    daemon_src: &Path,
    mut progress: impl FnMut(&'static str, u64, u64),
) -> Result<PathBuf, SoldrError> {
    relocate_daemon(paths, daemon_src, &mut progress, hash_file_with_progress)
}

/// Digest an executable for placement identity, reporting byte progress
/// under the given stage. The default, [`hash_file_with_progress`], reads the
/// whole image. soldr#3831: the broker has already verified the same image
/// with a memoized digest, and that second whole-file read of a ~120 MB debug
/// image cost 5-7 s per cold route on CI runners, so it passes its own.
pub type ImageDigest =
    fn(&Path, &'static str, &mut dyn FnMut(&'static str, u64, u64)) -> Result<String, SoldrError>;

fn relocate_daemon(
    paths: &SoldrPaths,
    daemon_src: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
    digest: ImageDigest,
) -> Result<PathBuf, SoldrError> {
    // soldr#1300: a maturin-repaired wheel binary loads bundled shared
    // libraries through a path RELATIVE TO THE BINARY'S OWN LOCATION
    // (`@loader_path/../<pkg>.dylibs/...` on macOS). Copying just the
    // executable into the runtime dir strands that reference and dyld
    // kills the daemon at exec — before main(), before any logging —
    // so the wrapper saw NotRunning for the full retry budget on every
    // compile. Run the daemon in place instead; on Unix an upgrade /
    // uninstall can unlink the running binary without waiting for the
    // daemon to exit, so the relocation's motivation doesn't apply.
    if exe_depends_on_bundled_wheel_libs(daemon_src) {
        return Ok(daemon_src.to_path_buf());
    }
    let runtime_root = daemon_runtime_root(paths);
    fs::create_dir_all(&runtime_root)?;
    if path_is_under(daemon_src, &runtime_root) {
        return Ok(daemon_src.to_path_buf());
    }
    ensure_relocated_exe_in_with_progress(&runtime_root, daemon_src, progress, digest)
}

/// Place a daemon at a route-specific executable path, retaining the repaired
/// wheel directory layout when the executable loads bundled libraries through
/// `@loader_path/..`.
///
/// Unlike [`ensure_daemon_relocated_with_progress`], this function never
/// returns a position-dependent wheel executable in place. Broker routes use
/// the returned executable path as their endpoint identity, so every route
/// must own a distinct, real path even when multiple roots share one wheel.
pub fn ensure_daemon_relocated_for_route_with_progress(
    paths: &SoldrPaths,
    daemon_src: &Path,
    progress: impl FnMut(&'static str, u64, u64),
) -> Result<PathBuf, SoldrError> {
    ensure_daemon_relocated_for_route_with_digest(
        paths,
        daemon_src,
        hash_file_with_progress,
        progress,
    )
}

/// [`ensure_daemon_relocated_for_route_with_progress`] keyed by `digest`
/// instead of a full SHA-256 read of the image.
pub fn ensure_daemon_relocated_for_route_with_digest(
    paths: &SoldrPaths,
    daemon_src: &Path,
    digest: ImageDigest,
    mut progress: impl FnMut(&'static str, u64, u64),
) -> Result<PathBuf, SoldrError> {
    if !exe_depends_on_bundled_wheel_libs(daemon_src) {
        return relocate_daemon(paths, daemon_src, &mut progress, digest);
    }

    let source_scripts = daemon_src.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine repaired-wheel scripts directory for {}",
            daemon_src.display()
        ))
    })?;
    let scripts_name = source_scripts.file_name().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine repaired-wheel scripts name for {}",
            daemon_src.display()
        ))
    })?;
    let package_root = source_scripts.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine repaired-wheel package root for {}",
            daemon_src.display()
        ))
    })?;
    let package = scripts_name
        .to_str()
        .and_then(|name| name.strip_suffix(".scripts"))
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "invalid repaired-wheel scripts directory {}",
                source_scripts.display()
            ))
        })?;

    let runtime_root = daemon_runtime_root(paths);
    fs::create_dir_all(&runtime_root)?;
    let _lock = lock_runtime_root(&runtime_root)?;
    let identity = exe_identity_with_progress(daemon_src, &mut progress, digest)?;
    let dest_root = runtime_root.join(&identity.dir_name);
    let dest_scripts = dest_root.join(scripts_name);
    let file_name = daemon_src.file_name().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine daemon filename from {}",
            daemon_src.display()
        ))
    })?;
    let dest = dest_scripts.join(file_name);
    let complete = dest_root.join(".wheel-bundle-complete");
    if complete.is_file()
        && exe_hash_matches_with_progress(&dest, &identity.hash_hex, &mut progress, digest)
    {
        touch_last_used(&dest_root)?;
        return Ok(dest);
    }

    fs::create_dir_all(&dest_scripts)?;
    copy_file_with_progress(daemon_src, &dest, &mut progress)?;
    fs::set_permissions(&dest, fs::metadata(daemon_src)?.permissions())?;
    for kind in ["dylibs", "libs"] {
        let source_bundle = package_root.join(format!("{package}.{kind}"));
        if source_bundle.is_dir() {
            copy_directory_tree(
                &source_bundle,
                &dest_root.join(format!("{package}.{kind}")),
                &mut progress,
            )?;
        }
    }
    File::create(&complete)?;
    touch_last_used(&dest_root)?;
    Ok(dest)
}

/// Detect the maturin "repaired wheel" layout (soldr#1300).
///
/// From maturin 1.13.2 the macOS `bin`-bindings wheels are repaired
/// delocate-style: the real executables live under
/// `<platlib>/<pkg>.scripts/`, bundled external dylibs under the
/// sibling `<platlib>/<pkg>.dylibs/`, and every executable's
/// `LC_LOAD_DYLIB` entry for a bundled lib is rewritten to
/// `@loader_path/../<pkg>.dylibs/<lib>` (soldr 0.7.98's macOS wheel
/// ships `liblzma` this way). Such a binary only runs from its
/// original directory — copied anywhere else, dyld aborts it at exec
/// with "Library not loaded".
///
/// The check is pure path logic (parent dir named `<pkg>.scripts`
/// with a sibling `<pkg>.dylibs` / `<pkg>.libs` directory) so it can
/// be exercised on every platform. `<pkg>.libs` is auditwheel's
/// equivalent bundle dir on Linux, covered pre-emptively — today's
/// Linux wheels ship unrepaired binaries.
/// `pub` rather than `pub(crate)` because soldr-cli's shim writers need the
/// same guard (soldr#1856): a hardlink/copy of a repaired wheel binary into a
/// shim dir strands the same `@loader_path` reference the daemon path already
/// avoids.
pub fn exe_depends_on_bundled_wheel_libs(exe: &Path) -> bool {
    let Some(parent) = exe.parent() else {
        return false;
    };
    let Some(dir_name) = parent.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let Some(pkg) = dir_name.strip_suffix(".scripts") else {
        return false;
    };
    if pkg.is_empty() {
        return false;
    }
    let Some(grandparent) = parent.parent() else {
        return false;
    };
    ["dylibs", "libs"]
        .iter()
        .any(|kind| grandparent.join(format!("{pkg}.{kind}")).is_dir())
}

/// The shared libraries an ELF executable needs at load time (`DT_NEEDED`), or
/// `None` when `exe` is unreadable or not a dynamic ELF -- including every
/// non-ELF platform binary. Lets a guard assert a build did not pick up a
/// system library it should link statically (soldr#3403).
pub fn exe_needed_libraries(exe: &Path) -> Option<Vec<String>> {
    crate::elf_origin::elf_needed_libraries(&fs::read(exe).ok()?)
}

/// True when `exe` is position-dependent: it names a shared library
/// relative to its own location, so copying or hardlinking it elsewhere
/// strands that reference and dyld aborts it at exec.
///
/// This asks the binary directly instead of inferring from directory
/// layout. [`exe_depends_on_bundled_wheel_libs`] is a *proxy* — it
/// recognises the one producer (maturin/delocate) whose output happens to
/// look that way. The proxy is both too narrow (any other repair tool, or
/// a hand-rolled `install_name_tool` run, is invisible to it) and too
/// fragile (it is opt-in per writer, so every new shim writer is a fresh
/// chance to forget the guard — which is exactly how soldr#1908 happened
/// after soldr#1856 was fixed).
///
/// Scans only the load-command region, never the whole file: `@loader_path`
/// can legitimately appear in a data section as an ordinary string, and
/// matching that would misclassify unrelated binaries.
///
/// Also answers `true` for an ELF whose rpath carries `$ORIGIN` (soldr#3403).
///
/// Returns `false` for anything it cannot parse — a non-Mach-O, a
/// truncated file, an unreadable path. The caller's fallback is the
/// hardlink fast path, so a false negative preserves today's behaviour
/// while a false positive would needlessly slow every shim.
pub fn exe_has_loader_path_reference(exe: &Path) -> bool {
    let Ok(bytes) = fs::read(exe) else {
        return false;
    };
    // soldr#3403: an ELF names its bundled libraries relative to itself with
    // `$ORIGIN` in DT_RPATH/DT_RUNPATH, and breaks the same way on a copy.
    crate::elf_origin::elf_has_origin_rpath(&bytes)
        || crate::macho_loader_path::macho_load_commands_mention_loader_path(&bytes)
}

/// Periodic GC sweep for the daemon-runtime sub-tree. Same cadence and
/// stale threshold as the soldr-self GC so a long-lived workspace
/// can't grow unbounded copies.
pub fn run_periodic_daemon_runtime_gc(paths: &SoldrPaths, current_exe: Option<&Path>) {
    run_periodic_gc_in(&daemon_runtime_root(paths), current_exe);
}

/// Sweep the daemon runtime copies of every broker route (soldr#3164).
///
/// The broker stages a verified daemon image into
/// `<routes_root>/<route>/runtime/soldr-daemon/<version>/` for each route.
/// `run_periodic_daemon_runtime_gc` is the ledger-based sweep for exactly that
/// sub-tree, but it lost its only caller when the broker became the sole spawn
/// front door (#2441), so a route whose daemon never launched again kept its
/// ~105 MB copy forever: 116 stale copies, 10.8 GiB, on one workstation.
///
/// Each route keeps its own `.last-gc` marker, so calling this often is cheap:
/// a route is scanned at most once per `GC_INTERVAL_SECONDS`. A copy is
/// removed only when its `last-used` ledger is older than
/// `STALE_RUNTIME_SECONDS`; a running daemon keeps that ledger fresh through
/// [`refresh_running_image_ledger`].
/// Returns how many stale copies were removed.
pub fn sweep_route_runtime_copies(routes_root: &Path) -> usize {
    let Ok(now) = current_unix_seconds() else {
        return 0;
    };
    sweep_route_runtime_copies_at(routes_root, now, GC_INTERVAL_SECONDS, STALE_RUNTIME_SECONDS)
        .removed_dirs
}

fn sweep_route_runtime_copies_at(
    routes_root: &Path,
    now: u64,
    interval_seconds: u64,
    stale_seconds: u64,
) -> RuntimeGcSummary {
    let mut total = RuntimeGcSummary::default();
    let Ok(routes) = fs::read_dir(routes_root) else {
        return total;
    };
    for route in routes.flatten() {
        let runtime_root = route.path().join(RUNTIME_DIR).join(DAEMON_DIR);
        // Only sweep a runtime tree that already exists: the GC creates its
        // root, and a sweep must never grow a route it is reclaiming.
        if !runtime_root.is_dir() {
            continue;
        }
        let Ok(Some(summary)) =
            maybe_run_periodic_gc_at(&runtime_root, None, now, interval_seconds, stale_seconds)
        else {
            continue;
        };
        total.scanned_dirs += summary.scanned_dirs;
        total.removed_dirs += summary.removed_dirs;
        total.skipped_current_dirs += summary.skipped_current_dirs;
        total.skipped_fresh_dirs += summary.skipped_fresh_dirs;
        total.stamped_dirs += summary.stamped_dirs;
        total.failed_dirs += summary.failed_dirs;
    }
    total
}

/// Newest `last-used` ledger stamp, in Unix seconds, among the daemon images
/// staged in one broker route, or `None` when no image carries one.
///
/// The broker's daemon-disk sweep (soldr#3251) ages whole routes by this, and
/// reads it through here so the route layout and ledger format stay defined in
/// one place.
pub fn route_image_last_used(route: &Path) -> Option<u64> {
    fs::read_dir(route.join(RUNTIME_DIR).join(DAEMON_DIR))
        .ok()?
        .flatten()
        .filter_map(|entry| runtime_copy_last_used(&entry.path()))
        .max()
}

/// Re-stamp the `last-used` ledger beside a running daemon's own image.
///
/// The ledger is written when an image is placed. A daemon that stays up for
/// longer than `STALE_RUNTIME_SECONDS` would otherwise look abandoned to
/// [`sweep_route_runtime_copies`], which would delete the image it is running
/// from and trip its missing-image stand-down (soldr#1987). Only an existing
/// ledger is refreshed, so this never creates one beside an arbitrary binary.
pub fn refresh_running_image_ledger(executable: &Path) {
    let Some(dir) = executable.parent() else {
        return;
    };
    if dir.join(LAST_USED_FILENAME).is_file() {
        let _ = touch_last_used(dir);
    }
}

fn run_periodic_gc_in(runtime_root: &Path, current_exe: Option<&Path>) {
    let current_dir = current_exe.and_then(Path::parent);
    let Ok(now) = current_unix_seconds() else {
        return;
    };
    let _ = maybe_run_periodic_gc_at(
        runtime_root,
        current_dir,
        now,
        GC_INTERVAL_SECONDS,
        STALE_RUNTIME_SECONDS,
    );
}

fn relocation_requested() -> bool {
    // Windows relocates by default: the running executable is locked while
    // the daemon writes to it, so self-update must copy first. Unix can
    // replace the in-place binary and relocates only on explicit request.
    crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
        || crate::core::flag(FORCE_RELOCATION_ENV_VAR)
}

fn relocation_guard_active() -> bool {
    std::env::var_os(RELOCATED_EXE_ENV_VAR).is_some()
}

fn ensure_relocated_exe_in(runtime_root: &Path, current_exe: &Path) -> Result<PathBuf, SoldrError> {
    ensure_relocated_exe_in_with_progress(
        runtime_root,
        current_exe,
        &mut |_, _, _| {},
        hash_file_with_progress,
    )
}

fn ensure_relocated_exe_in_with_progress(
    runtime_root: &Path,
    current_exe: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
    digest: ImageDigest,
) -> Result<PathBuf, SoldrError> {
    fs::create_dir_all(runtime_root)?;
    let _lock = lock_runtime_root(runtime_root)?;

    let identity = exe_identity_with_progress(current_exe, progress, digest)?;
    let dest_dir = runtime_root.join(&identity.dir_name);
    fs::create_dir_all(&dest_dir)?;

    let file_name = current_exe.file_name().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine current executable filename from {}",
            current_exe.display()
        ))
    })?;
    let dest = dest_dir.join(file_name);

    if exe_hash_matches_with_progress(&dest, &identity.hash_hex, progress, digest) {
        touch_last_used(&dest_dir)?;
        return Ok(dest);
    }

    let temp = dest_dir.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_file(&temp);
    link_or_copy_with_progress(current_exe, &temp, progress)?;
    let permissions = fs::metadata(current_exe)?.permissions();
    fs::set_permissions(&temp, permissions)?;

    if dest.exists() && !exe_hash_matches_with_progress(&dest, &identity.hash_hex, progress, digest)
    {
        let _ = fs::remove_file(&dest);
    }

    match fs::rename(&temp, &dest) {
        Ok(()) => {}
        Err(_err)
            if exe_hash_matches_with_progress(&dest, &identity.hash_hex, progress, digest) =>
        {
            let _ = fs::remove_file(&temp);
        }
        Err(err) => {
            let _ = fs::remove_file(&temp);
            return Err(SoldrError::Io(err));
        }
    }

    touch_last_used(&dest_dir)?;
    Ok(dest)
}

fn runtime_root(paths: &SoldrPaths) -> PathBuf {
    paths.root.join(RUNTIME_DIR).join(SELF_DIR)
}

/// Set on the re-executed child so the hop can never happen twice.
///
/// This is the safety property, not an optimisation. If
/// [`daemon_should_reexec`]'s "am I already relocated?" answer were ever wrong
/// in the direction of "no", an unguarded hop would re-exec forever and no
/// daemon would ever start -- every build on the machine broken, and the
/// failure looks like a daemon that will not come up rather than like a bad
/// predicate. With a one-shot marker the worst case degrades to today's
/// behaviour: running from wherever we were launched.
pub const DAEMON_REEXEC_MARKER_ENV_VAR: &str = "SOLDR_INTERNAL_DAEMON_REEXECED";

/// Where a daemon launched from `current_exe` should re-exec itself from, if
/// anywhere.
///
/// soldr#1987: a daemon spawned from a directory that is later deleted -- a
/// `uv` or `pip` build materialises soldr into a temp dir and removes it --
/// keeps the root-ownership lock forever, and nothing outside the process can
/// clear it. `resolve_daemon_spawn_image` already relocates on the *spawner*
/// side, but that is advisory: it falls back to the original path on any error
/// (soldr#1998 made that audible), and a daemon started by any other route
/// never passes through it at all. Deciding here, inside the process that will
/// take the lock, is the only unconditional place.
///
/// Returns `None` when no hop is needed or possible:
/// * the marker is set -- we already hopped once;
/// * the image is already under the runtime root;
/// * the source is a maturin-repaired wheel, which must run in place
///   (soldr#1300) because relocating strands its bundled dylibs;
/// * relocation failed -- a daemon pinning the wrong directory still beats no
///   daemon, which is the same trade the spawner makes.
pub fn daemon_should_reexec(paths: &SoldrPaths, current_exe: &Path) -> Option<PathBuf> {
    if std::env::var_os(DAEMON_REEXEC_MARKER_ENV_VAR).is_some() {
        return None;
    }
    if exe_depends_on_bundled_wheel_libs(current_exe) {
        return None;
    }
    let runtime_root = daemon_runtime_root(paths);
    if path_is_under(current_exe, &runtime_root) {
        return None;
    }
    let relocated = ensure_daemon_relocated(paths, current_exe).ok()?;
    // Equal paths mean `ensure_daemon_relocated` declined to move it; hopping
    // to where we already are would be a no-op exec, and with a marker that
    // failed to stick it would be a loop.
    (relocated != current_exe).then_some(relocated)
}

pub fn daemon_runtime_root(paths: &SoldrPaths) -> PathBuf {
    paths.root.join(RUNTIME_DIR).join(DAEMON_DIR)
}

fn lock_runtime_root(runtime_root: &Path) -> Result<File, SoldrError> {
    fs::create_dir_all(runtime_root)?;
    let lock_path = runtime_root.join(LOCK_FILENAME);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    crate::platform::wait::lock_exclusive_announced(
        &file,
        crate::platform::wait::LONG_LOCK,
        "the self-relocation lock",
    )?;
    Ok(file)
}

struct ExeIdentity {
    dir_name: String,
    hash_hex: String,
}

fn exe_identity(path: &Path) -> Result<ExeIdentity, SoldrError> {
    exe_identity_with_progress(path, &mut |_, _, _| {}, hash_file_with_progress)
}

fn exe_identity_with_progress(
    path: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
    digest: ImageDigest,
) -> Result<ExeIdentity, SoldrError> {
    let hash_hex = digest(path, "source-hash", progress)?;
    Ok(ExeIdentity {
        dir_name: relocation_dir_name(env!("CARGO_PKG_VERSION"), &hash_hex),
        hash_hex,
    })
}

/// soldr#1597 Phase 3: official (`auto-release.yml`-published) builds
/// relocate to a hash-free `v{VERSION}/` directory. Dev/manual builds
/// keep the hash-keyed `v{VERSION}-{hash}` name unconditionally — the
/// hash is the only thing that distinguishes two dev rebuilds sharing a
/// version but differing in content, and without it a rebuild could try
/// to overwrite a locked, running daemon binary on Windows. Broker v2
/// discovery is pointer-file-based (PID file + `.servicedef.v2`), never
/// directory-scanning, so this naming choice is invisible to it either
/// way — no broker changes are needed.
fn relocation_dir_name(version: &str, hash_hex: &str) -> String {
    if crate::build_provenance::is_official_build() {
        format!("v{version}")
    } else {
        format!("v{version}-{hash_hex}")
    }
}

fn exe_hash_matches(path: &Path, expected_hash: &str) -> bool {
    exe_hash_matches_with_progress(
        path,
        expected_hash,
        &mut |_, _, _| {},
        hash_file_with_progress,
    )
}

fn exe_hash_matches_with_progress(
    path: &Path,
    expected_hash: &str,
    progress: &mut dyn FnMut(&'static str, u64, u64),
    digest: ImageDigest,
) -> bool {
    path.is_file()
        && digest(path, "placed-hash", progress).is_ok_and(|actual| actual == expected_hash)
}

fn hash_file(path: &Path) -> Result<String, SoldrError> {
    hash_file_with_progress(path, "hash", &mut |_, _, _| {})
}

fn hash_file_with_progress(
    path: &Path,
    stage: &'static str,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<String, SoldrError> {
    let mut file = File::open(path)?;
    let total = file.metadata()?.len();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut completed = 0_u64;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        completed = completed.saturating_add(read as u64);
        progress(stage, completed, total);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// Stage `source` at `target`, preferring a hardlink over a byte copy.
///
/// soldr#3138: this path materializes the daemon runtime image, and it is
/// ~105 MB for a debug build. Every route that has not seen this exact image
/// before paid a full read + write of that, which is invisible on a developer
/// box (the source is in page cache, ~110 ms measured) and expensive on a CI
/// runner, where it is a cold read of 105 MB followed by a 105 MB write. It is
/// also I/O-bound rather than CPU-bound, so concurrent cold starts contend
/// rather than overlap -- consistent with the +24% total test-seconds measured
/// when the gate's test groups went two-wide (soldr#3161/#3162).
///
/// A hardlink is safe here specifically because this destination is
/// content-addressed and immutable:
///
/// * `dest_dir` is keyed on `identity.dir_name`, which carries the image hash,
///   and the caller re-verifies with `exe_hash_matches_with_progress`. A link
///   can only ever be published under the hash of the bytes it points at.
/// * The staged image is executed, never written in place. The publish is
///   write-temp-then-rename, so the inode is never mutated after linking.
/// * `set_permissions` on the link would also apply to the source, but the
///   value being set is read *from* the source immediately above, so it is a
///   no-op rather than a mutation.
/// * A later rebuild of the source writes a new file and renames over it,
///   which leaves this inode intact -- the correct outcome, since a running
///   daemon must keep the image it started from.
///
/// Falls back to the byte copy whenever the link cannot be made: a different
/// filesystem (`EXDEV`, the common case when the build tree and the runtime
/// root live on separate volumes -- soldr#1831 notes that layout explicitly),
/// a filesystem without hardlinks, or a permissions refusal. The fallback is
/// the previous behaviour exactly, so the worst case is unchanged.
///
/// This mirrors what `soldr-cli`'s `shim_materialize` already does for shims,
/// which is why fixture shims cost nothing while this path cost a full copy.
fn link_or_copy_with_progress(
    source: &Path,
    target: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<(), SoldrError> {
    if fs::hard_link(source, target).is_ok() {
        // Report the whole image as completed so a caller rendering progress
        // sees a finished unit of work rather than a silent gap.
        let total = fs::metadata(target).map(|meta| meta.len()).unwrap_or(0);
        progress("link", total, total);
        return Ok(());
    }
    copy_file_with_progress(source, target, progress)
}

fn copy_file_with_progress(
    source: &Path,
    target: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<(), SoldrError> {
    use std::io::Write as _;

    let mut input = File::open(source)?;
    let total = input.metadata()?.len();
    let mut output = File::create(target)?;
    let mut buf = vec![0u8; 1024 * 1024];
    let mut completed = 0_u64;
    loop {
        let read = input.read(&mut buf)?;
        if read == 0 {
            break;
        }
        output.write_all(&buf[..read])?;
        completed = completed.saturating_add(read as u64);
        progress("copy", completed, total);
    }
    output.flush()?;
    Ok(())
}

fn copy_directory_tree(
    source: &Path,
    target: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<(), SoldrError> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory_tree(&source_path, &target_path, progress)?;
        } else {
            copy_file_with_progress(&source_path, &target_path, progress)?;
            fs::set_permissions(&target_path, fs::metadata(&source_path)?.permissions())?;
        }
    }
    fs::set_permissions(target, fs::metadata(source)?.permissions())?;
    Ok(())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn touch_last_used(dir: &Path) -> Result<(), SoldrError> {
    fs::write(
        dir.join(LAST_USED_FILENAME),
        current_unix_seconds()?.to_string(),
    )?;
    Ok(())
}

fn maybe_run_periodic_gc_at(
    runtime_root: &Path,
    current_dir: Option<&Path>,
    now: u64,
    interval_seconds: u64,
    stale_seconds: u64,
) -> Result<Option<RuntimeGcSummary>, SoldrError> {
    fs::create_dir_all(runtime_root)?;
    if !runtime_gc_due(runtime_root, now, interval_seconds) {
        return Ok(None);
    }

    let _lock = lock_runtime_root(runtime_root)?;
    if !runtime_gc_due(runtime_root, now, interval_seconds) {
        return Ok(None);
    }

    let summary = purge_stale_runtime_copies(runtime_root, current_dir, now, stale_seconds)?;
    fs::write(runtime_root.join(GC_MARKER_FILENAME), now.to_string())?;
    Ok(Some(summary))
}

fn runtime_gc_due(runtime_root: &Path, now: u64, interval_seconds: u64) -> bool {
    let marker = runtime_root.join(GC_MARKER_FILENAME);
    let Ok(raw) = fs::read_to_string(marker) else {
        return true;
    };
    let Ok(last_run) = raw.trim().parse::<u64>() else {
        return true;
    };
    now.saturating_sub(last_run) >= interval_seconds
}

fn purge_stale_runtime_copies(
    runtime_root: &Path,
    current_dir: Option<&Path>,
    now: u64,
    stale_seconds: u64,
) -> Result<RuntimeGcSummary, SoldrError> {
    let mut summary = RuntimeGcSummary::default();
    let cutoff = now.saturating_sub(stale_seconds);

    for entry in fs::read_dir(runtime_root)? {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }

        let path = entry.path();
        summary.scanned_dirs += 1;
        if current_dir.is_some_and(|current| same_path(current, &path)) {
            summary.skipped_current_dirs += 1;
            continue;
        }

        let Some(last_used) = runtime_copy_last_used(&path) else {
            // No ledger stamp. The GC is strictly ledger-based — never
            // trust the directory mtime, which lies after an archive
            // restore (an ancient saved mtime would age out a
            // freshly-rehydrated dir mid-build; soldr#1495 GHA safety).
            // Self-heal by stamping `now` and treating the dir as fresh
            // this round; it becomes eligible 48h from now if it stays
            // unused. Guarantees stampless dirs neither leak forever nor
            // get mtime-collected.
            let _ = fs::write(path.join(LAST_USED_FILENAME), now.to_string());
            summary.stamped_dirs += 1;
            continue;
        };
        if last_used > cutoff {
            summary.skipped_fresh_dirs += 1;
            continue;
        }

        match fs::remove_dir_all(&path) {
            Ok(()) => summary.removed_dirs += 1,
            Err(_) => summary.failed_dirs += 1,
        }
    }

    Ok(summary)
}

/// Read the `last-used` ledger stamp for a runtime copy. Returns `None`
/// when the stamp is absent or unparseable — deliberately with **no
/// mtime fallback**: filesystem mtimes survive archive save/restore and
/// would let a stale saved timestamp age out a freshly-materialized dir
/// (soldr#1495 Workstream C). Callers treat `None` as "unknown → keep
/// and self-heal", never as "old".
fn runtime_copy_last_used(path: &Path) -> Option<u64> {
    fs::read_to_string(path.join(LAST_USED_FILENAME))
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
}

fn current_unix_seconds() -> Result<u64, SoldrError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|e| SoldrError::Other(format!("system clock is before unix epoch: {e}")))
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => path.starts_with(root),
    }
}

#[cfg(test)]
#[path = "self_relocate_tests.rs"]
mod tests;
