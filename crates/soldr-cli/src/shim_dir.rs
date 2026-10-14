//! Transient child-process shims (issue #493).
//!
//! When a user runs `soldr <external-tool> ...` (e.g. `soldr maturin
//! build`), the external tool runs as itself but any `cargo` / `rustc`
//! / etc it spawns internally would normally bypass soldr because PATH
//! still resolves to the unwrapped system binaries.
//!
//! This module creates a transient directory of multicall shims for the
//! Rust toolchain binaries soldr already wraps (`cargo`, `rustc`,
//! `rustdoc`, `rustfmt`, `clippy-driver`). Each shim is a hardlink/copy
//! of the running soldr binary under the matching argv[0] name, so nested
//! toolchain calls route back through soldr (and therefore zccache, the
//! managed toolchain home, etc) without shell mediation or PATH setup.
//!
//! A recursion guard env var (`SOLDR_CHILD_SHIMS_ACTIVE`) is set in the
//! child environment so a nested `soldr <external-tool>` invocation
//! sees the sentinel and does NOT re-install another shim layer.

use crate::core::{SoldrError, SoldrPaths};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Sentinel that signals "you were invoked under a soldr shim dir;
/// do NOT install another shim layer for your own children." Read by
/// `should_install_shims` and set by `apply_to_command`.
pub(crate) const SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR: &str = "SOLDR_CHILD_SHIMS_ACTIVE";

/// Opt-out toggle. When set to a truthy value, `should_install_shims`
/// returns false and the external tool runs without a shim layer.
pub(crate) const SOLDR_DISABLE_CHILD_SHIMS_ENV_VAR: &str = "SOLDR_DISABLE_CHILD_SHIMS";

/// Names installed into the shim dir. These are the child toolchain
/// processes Soldr can safely proxy back through its front doors when
/// a long-lived external process (for example rust-analyzer) spawns
/// hardcoded `cargo` / `rustc` style commands.
const SHIMMED_TOOLS: &[&str] = &["cargo", "rustc", "rustdoc", "rustfmt", "clippy-driver"];
const DYLINT_SHIMMED_TOOLS: &[&str] = &[
    "cargo",
    "rustc",
    "rustdoc",
    "rustfmt",
    "clippy-driver",
    "rustup",
];

/// Drop-on-exit guard that removes the shim directory best-effort.
/// Holding the guard alive across the child's run is the caller's
/// responsibility.
///
/// `persistent` shim dirs (see [`build_dylint_shim_dir`]) are shared,
/// soldr-owned state keyed by the running soldr binary's identity —
/// dropping the guard must NOT delete them, since a concurrent
/// invocation may still be reading the same directory.
pub(crate) struct ShimDirGuard {
    pub(crate) path: PathBuf,
    persistent: bool,
}

impl Drop for ShimDirGuard {
    fn drop(&mut self) {
        if self.persistent {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Decide whether to install shims for the next child process. Returns
/// `false` when the recursion guard is tripped or when the user has
/// opted out via `SOLDR_DISABLE_CHILD_SHIMS`.
pub(crate) fn should_install_shims() -> bool {
    if std::env::var_os(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR).is_some() {
        return false;
    }
    if let Some(raw) = std::env::var_os(SOLDR_DISABLE_CHILD_SHIMS_ENV_VAR) {
        if crate::core::flag_value(&raw.to_string_lossy()) {
            return false;
        }
    }
    true
}

/// Build a fresh shim dir under the system tempdir and populate it
/// with one multicall executable per `SHIMMED_TOOLS` entry.
pub(crate) fn build_shim_dir() -> Result<ShimDirGuard, SoldrError> {
    build_shim_dir_for(SHIMMED_TOOLS)
}

/// Dylint sanitizes standard Rust environment variables before invoking its
/// nested tools. Its scoped shim set also includes rustup so Soldr can restore
/// the selected nightly from the retained SOLDR_DYLINT_* identity.
///
/// Unlike [`build_shim_dir`], this reuses a stable soldr-owned
/// directory (`<soldr_root>/dylint/shims/v1/<key>/`) across top-level
/// `soldr cargo dylint` runs instead of materializing a fresh temp dir
/// each time — cargo-dylint's nested cargo/rustc re-entries pay for
/// this directory on every invocation, so a warm reuse skips the
/// per-file hardlink/copy work entirely. Falls back to the ephemeral
/// tempdir path if the persistent location cannot be resolved (e.g.
/// no home directory available).
pub(crate) fn build_dylint_shim_dir() -> Result<ShimDirGuard, SoldrError> {
    match persistent_dylint_shim_dir() {
        Ok(guard) => Ok(guard),
        Err(_) => build_shim_dir_for(DYLINT_SHIMMED_TOOLS),
    }
}

fn persistent_dylint_shim_dir() -> Result<ShimDirGuard, SoldrError> {
    let paths = SoldrPaths::new()?;
    let soldr_bin = crate::shim_materialize::soldr_binary_source()?;
    let base = paths.root.join("dylint").join("shims").join("v1");
    persistent_shim_dir_in(&base, &soldr_bin, DYLINT_SHIMMED_TOOLS)
}

/// Pure, injectable-base-dir implementation of the persistent shim dir
/// logic, split out so unit tests can point `base` at a tempdir instead
/// of the real `~/.soldr/dylint/shims/v1`.
fn persistent_shim_dir_in(
    base: &Path,
    soldr_bin: &Path,
    tools: &[&str],
) -> Result<ShimDirGuard, SoldrError> {
    let key = dylint_shim_dir_key(soldr_bin)?;
    let dir_path = base.join(&key);

    if shim_dir_is_complete(&dir_path, tools) {
        mark_shim_dir_used(&dir_path);
        return Ok(ShimDirGuard {
            path: dir_path,
            persistent: true,
        });
    }

    std::fs::create_dir_all(&dir_path).map_err(SoldrError::Io)?;
    for tool in tools {
        write_shim(&dir_path, tool, soldr_bin)?;
    }
    mark_shim_dir_used(&dir_path);
    // Best-effort: drop sibling key dirs from previous soldr binary
    // identities, but only idle ones. Two soldr binaries legitimately share
    // one root (an installed soldr and a source build running the local
    // gate); pruning a sibling on sight deleted the other's in-use
    // `soldr-daemon` shim, so its broker route failed to spawn on every
    // alternate run. Failures are ignored: a held-open exe on Windows
    // self-heals on the next prune.
    prune_stale_sibling_shim_dirs(base, &key, SHIM_DIR_IDLE_BEFORE_PRUNE);
    Ok(ShimDirGuard {
        path: dir_path,
        persistent: true,
    })
}

/// True when `dir` already contains every expected shim executable, so
/// a warm run can skip rebuilding it entirely.
fn shim_dir_is_complete(dir: &Path, tools: &[&str]) -> bool {
    tools.iter().all(|tool| shim_tool_path(dir, tool).is_file())
}

/// Cheap, stable identity for the currently-running soldr binary: hash
/// its resolved path, byte length, and modification time (nanoseconds
/// since the epoch). Deliberately does NOT hash the binary's contents
/// — that would mean re-reading the whole executable on every dylint
/// invocation, defeating the point of caching.
fn dylint_shim_dir_key(soldr_bin: &Path) -> Result<String, SoldrError> {
    let metadata = std::fs::metadata(soldr_bin).map_err(SoldrError::Io)?;
    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();

    // `DefaultHasher::new()` uses fixed (non-randomized) keys, so the
    // digest is stable across separate soldr processes — unlike
    // `RandomState`-seeded hashers, which would mint a new key dir on
    // every invocation and defeat the cache entirely.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    soldr_bin.to_string_lossy().hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    mtime_nanos.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

/// A sibling key dir unused for this long belongs to a binary that is gone.
const SHIM_DIR_IDLE_BEFORE_PRUNE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Marker whose mtime records the last run that used a persistent shim dir.
const SHIM_DIR_LAST_USED: &str = ".last-used";

fn mark_shim_dir_used(dir: &Path) {
    // Rewriting the marker refreshes its mtime without a file-time setter.
    let _ = std::fs::write(dir.join(SHIM_DIR_LAST_USED), b"used\n");
}

fn shim_dir_last_used(dir: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(dir.join(SHIM_DIR_LAST_USED))
        .or_else(|_| std::fs::metadata(dir))
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn prune_stale_sibling_shim_dirs(base: &Path, keep_key: &str, max_idle: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy() == keep_key {
            continue;
        }
        let idle = shim_dir_last_used(&entry.path())
            .and_then(|used| now.duration_since(used).ok())
            .unwrap_or_default();
        if idle >= max_idle {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn build_shim_dir_for(tools: &[&str]) -> Result<ShimDirGuard, SoldrError> {
    let soldr_bin = crate::shim_materialize::soldr_binary_source()?;
    let dir = tempfile::Builder::new()
        .prefix("soldr-shims-")
        .tempdir()
        .map_err(SoldrError::Io)?;
    let dir_path = dir.path().to_path_buf();
    // Defuse the tempdir auto-cleanup; ShimDirGuard owns removal so
    // the lifetime matches the child process duration regardless of
    // panic / early return paths.
    let _ = dir.keep();
    for tool in tools {
        write_shim(&dir_path, tool, &soldr_bin)?;
    }
    Ok(ShimDirGuard {
        path: dir_path,
        persistent: false,
    })
}

pub(crate) fn shim_tool_path(dir: &Path, tool: &str) -> PathBuf {
    dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX))
}

fn write_shim(dir: &Path, tool: &str, soldr_bin: &Path) -> Result<(), SoldrError> {
    let path = shim_tool_path(dir, tool);
    // soldr#1856: a maturin/delocate-repaired macOS wheel binary loads its
    // bundled dylibs through `@loader_path/../<pkg>.dylibs/<lib>`. A hardlink
    // or copy into the shim dir strands that relative reference — `@loader_path`
    // then resolves against the shim dir — and dyld kills the child before
    // main(). A hardlink is no safer than a copy here: it has no "original
    // path" for the loader to resolve against either.
    //
    // Same failure the daemon path already avoids (soldr#1300), so use the same
    // predicate: leave the real binary where it is and trampoline to it.
    if soldr_core::self_relocate::exe_depends_on_bundled_wheel_libs(soldr_bin) {
        return write_trampoline_shim(&path, soldr_bin);
    }
    crate::shim_materialize::materialize_executable(soldr_bin, &path).map(|_| ())
}

/// The exact text of a trampoline shim.
///
/// Split out so the persistent installer can compare against it for
/// idempotency instead of byte-comparing a script to a Mach-O, which would
/// rewrite the shim on every invocation and defeat the memo fast path
/// (soldr#1831).
///
/// Single-quoted with `'\''` escaping: a wheel path can sit under a venv with
/// spaces, and the pre-0.8.10 double-quoted form would still expand `$` and
/// backticks inside the path.
///
/// # Why the identity travels in the environment (soldr#1934)
///
/// The obvious body — `exec <soldr> <tool> "$@"` — shipped in 0.8.26 and broke
/// every wheel install. A hardlinked shim carries its identity in **argv[0]**
/// and leaves `"$@"` alone; putting the tool name in argv[1] instead shifts
/// every remaining argument right by one. `RUSTC_WRAPPER` dispatch is
/// positional on argv[1] (`wrapper.rs`: `tool_arg = raw_args[1]`), so cargo's
/// `<shim> <real-rustc> <args…>` became `<soldr> rustc <real-rustc> <args…>`
/// and the compiler path was handed to rustc as a source file:
/// `error: multiple input filenames provided`.
///
/// So the tool name goes out of band and `"$@"` stays untouched.
/// [`crate::multicall::apply_shim_argv0_override`] puts it back into argv[0],
/// which makes every downstream index — and the `PATH` scrub, which also reads
/// argv[0] — identical to the hardlink shape by construction rather than by
/// remembering to special-case each caller.
///
/// `$0` rather than the literal tool name: it is the shim's own path, exactly
/// what argv[0] would have held. `exec -a` would say the same thing directly
/// but is not POSIX — `/bin/sh` is `dash` on most Linux distributions.
///
/// The body no longer varies by tool — `$0` supplies that — so every
/// trampoline in a shim dir is byte-identical.
pub(crate) fn trampoline_shim_body(soldr_bin: &Path) -> String {
    let quoted = format!("'{}'", soldr_bin.to_string_lossy().replace('\'', r"'\''"));
    format!(
        "#!/bin/sh\n{}=\"$0\"\nexport {}\nexec {quoted} \"$@\"\n",
        crate::multicall::SHIM_ARGV0_ENV,
        crate::multicall::SHIM_ARGV0_ENV,
    )
}

/// Write a `#!/bin/sh` trampoline that re-enters soldr in place.
///
/// Used only for wheel-repaired binaries; everything else keeps the faster
/// hardlink path so the startup-latency work (soldr#1831/#1834) is untouched.
/// Cost here is one `sh` fork per nested tool call, on wheel installs only.
pub(crate) fn write_trampoline_shim(path: &Path, soldr_bin: &Path) -> Result<(), SoldrError> {
    std::fs::write(path, trampoline_shim_body(soldr_bin)).map_err(SoldrError::Io)?;
    // Only the exec bit is genuinely platform-specific; the platform
    // crate owns it (no-op where the filesystem has no exec bits).
    crate::platform::fs::permissions::make_executable(path).map_err(SoldrError::Io)?;
    Ok(())
}

/// Apply the shim dir to `command`'s environment so the child sees it
/// at the front of PATH and inherits the recursion sentinel.
pub(crate) fn apply_to_command(command: &mut std::process::Command, shim_dir: &Path) {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    // Build "shim_dir<sep>existing" without copying the OsString — we
    // need OS-specific separators (';' on Windows, ':' elsewhere). The
    // canonical way is `env::join_paths`, but that requires a Vec of
    // paths; doing it by hand here is simpler and equivalent.
    let mut new_path = std::ffi::OsString::new();
    new_path.push(shim_dir.as_os_str());
    if !existing.is_empty() {
        new_path.push(crate::platform::host::facts::path_list_separator());
        new_path.push(&existing);
    }
    command.env("PATH", new_path);
    command.env(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// Build the maturin/delocate "repaired wheel" layout the predicate
    /// detects: `<root>/<pkg>.scripts/soldr` beside `<root>/<pkg>.dylibs/`.
    /// Pure path logic, so this works on every platform.
    fn repaired_wheel_layout(root: &Path) -> PathBuf {
        let scripts = root.join("soldr.scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::create_dir_all(root.join("soldr.dylibs")).unwrap();
        let exe = scripts.join("soldr");
        std::fs::write(&exe, b"MACH-O-PLACEHOLDER").unwrap();
        exe
    }

    #[test]
    fn trampoline_execs_the_real_binary_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = repaired_wheel_layout(tmp.path());
        let body = trampoline_shim_body(&exe);

        assert!(body.starts_with("#!/bin/sh\n"), "{body}");
        assert!(body.contains("exec "), "{body}");
        // soldr#1934: `"$@"` must be forwarded verbatim, with nothing inserted
        // ahead of it. A tool name here shifts every argument right by one and
        // breaks the positional RUSTC_WRAPPER contract.
        assert!(body.trim_end().ends_with(r#" "$@""#), "{body}");
        assert!(
            !body.contains(r#" cargo "$@""#),
            "the tool name must not be pushed into argv[1]: {body}"
        );
        // The identity instead rides in the environment as the shim's own $0.
        assert!(
            body.contains(&format!(
                "{env}=\"$0\"\nexport {env}\n",
                env = crate::multicall::SHIM_ARGV0_ENV
            )),
            "{body}"
        );
        // The real binary must stay where it is — that is the whole fix.
        assert!(body.contains(&exe.to_string_lossy().to_string()), "{body}");
    }

    /// A venv path can contain a single quote; the pre-0.8.10 double-quoted
    /// form would also have expanded `$` and backticks.
    #[test]
    fn trampoline_quoting_survives_hostile_paths() {
        let body = trampoline_shim_body(Path::new("/o'brien/$HOME/`x`/soldr"));
        assert!(body.contains(r"'/o'\''brien/$HOME/`x`/soldr'"), "{body}");
        // Nothing outside the single-quoted span may be interpolated.
        assert!(!body.contains("\"$HOME\""), "{body}");
    }

    #[test]
    fn repaired_wheel_layout_takes_the_trampoline_path() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = repaired_wheel_layout(tmp.path());
        assert!(
            soldr_core::self_relocate::exe_depends_on_bundled_wheel_libs(&exe),
            "the repaired-wheel layout must be detected"
        );

        let dir = tmp.path().join("shims");
        std::fs::create_dir_all(&dir).unwrap();
        write_shim(&dir, "cargo", &exe).unwrap();

        let written = std::fs::read_to_string(shim_tool_path(&dir, "cargo")).unwrap();
        assert_eq!(written, trampoline_shim_body(&exe));
        if let Some(mode) = crate::platform::fs::permissions::mode(&shim_tool_path(&dir, "cargo")) {
            assert_eq!(mode & 0o777, 0o755, "trampoline must be executable");
        }
    }

    /// The fast hardlink/copy path must survive for ordinary installs, or the
    /// startup-latency work (#1831/#1834) regresses for everyone.
    #[test]
    fn ordinary_layout_still_materializes_the_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("soldr-plain");
        std::fs::write(&exe, b"PLAIN-SOLDR-BYTES").unwrap();
        assert!(!soldr_core::self_relocate::exe_depends_on_bundled_wheel_libs(&exe));

        let dir = tmp.path().join("shims");
        std::fs::create_dir_all(&dir).unwrap();
        write_shim(&dir, "cargo", &exe).unwrap();

        assert_eq!(
            std::fs::read(shim_tool_path(&dir, "cargo")).unwrap(),
            b"PLAIN-SOLDR-BYTES",
            "non-wheel installs must keep the materialized binary"
        );
    }

    fn system_cmd_exe() -> PathBuf {
        let system_root = std::env::var_os("SystemRoot")
            .unwrap_or_else(|| std::ffi::OsString::from("C:\\Windows"));
        PathBuf::from(system_root).join("System32").join("cmd.exe")
    }

    #[test]
    fn shim_dir_contains_every_shimmed_tool() {
        let guard = build_shim_dir().expect("build_shim_dir");
        for tool in SHIMMED_TOOLS {
            let expected = shim_tool_path(&guard.path, tool);
            assert!(expected.is_file(), "missing shim at {}", expected.display());
        }
    }

    #[test]
    fn shim_tool_path_uses_native_executable_suffix() {
        let dir = PathBuf::from("/tmp/shims");
        let path = shim_tool_path(&dir, "cargo");
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            assert!(
                path.to_string_lossy().ends_with("cargo.exe"),
                "windows shims must be native executable files: {}",
                path.display()
            );
        } else {
            assert!(
                path.to_string_lossy().ends_with("cargo"),
                "unix shims keep extensionless tool names: {}",
                path.display()
            );
        }
    }

    #[test]
    fn windows_shims_are_visible_to_rust_command_lookup() {
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let cmd = system_cmd_exe();
        assert!(cmd.is_file(), "missing {}", cmd.display());

        for tool in SHIMMED_TOOLS {
            write_shim(temp.path(), tool, &cmd).expect("write shim");
            let output = std::process::Command::new(tool)
                .args(["/D", "/C", "exit 0"])
                .env("PATH", temp.path())
                .output()
                .unwrap_or_else(|err| {
                    panic!("Rust Command::new({tool:?}) did not find executable shim: {err}")
                });
            assert!(
                output.status.success(),
                "shim {tool} resolved but failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn apply_to_command_sets_recursion_sentinel_and_prepends_path() {
        let guard = build_shim_dir().expect("build_shim_dir");
        let mut cmd = std::process::Command::new("does-not-matter");
        apply_to_command(&mut cmd, &guard.path);
        let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
        let sentinel_set = envs
            .get(OsStr::new(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR))
            .copied()
            .flatten();
        assert_eq!(sentinel_set, Some(OsStr::new("1")));
        let path_value = envs.get(OsStr::new("PATH")).copied().flatten();
        let path_str = path_value
            .expect("PATH must be set")
            .to_string_lossy()
            .to_string();
        assert!(
            path_str.starts_with(&guard.path.to_string_lossy().to_string()),
            "PATH must lead with the shim dir: {path_str}"
        );
    }

    #[test]
    fn should_install_shims_respects_recursion_sentinel() {
        // Test seam: this test must not pollute the parent env. We set
        // the var, observe the guard, then unset.
        std::env::set_var(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
        let active = should_install_shims();
        std::env::remove_var(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR);
        assert!(!active);
    }

    #[test]
    fn should_install_shims_respects_opt_out() {
        std::env::set_var(SOLDR_DISABLE_CHILD_SHIMS_ENV_VAR, "1");
        let active = should_install_shims();
        std::env::remove_var(SOLDR_DISABLE_CHILD_SHIMS_ENV_VAR);
        assert!(!active);
    }

    // -----------------------------------------------------------------
    // Persistent dylint shim dir (nested cargo-dylint re-entry overhead
    // reduction). Uses `persistent_shim_dir_in` with a tempdir `base` so
    // these tests never touch the real `~/.soldr/dylint/shims/v1`.
    // -----------------------------------------------------------------

    fn fake_soldr_bin(dir: &Path, content: &[u8]) -> PathBuf {
        let path = dir.join("fake-soldr-bin");
        std::fs::write(&path, content).expect("write fake soldr binary");
        path
    }

    #[test]
    fn persistent_shim_dir_reuses_existing_complete_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("base");
        let bin = fake_soldr_bin(temp.path(), b"stub");
        let tools: &[&str] = &["cargo", "rustc"];

        let first = persistent_shim_dir_in(&base, &bin, tools).expect("first build");
        for tool in tools {
            assert!(
                shim_tool_path(&first.path, tool).is_file(),
                "missing shim for {tool}"
            );
        }

        let second = persistent_shim_dir_in(&base, &bin, tools).expect("second call reuses dir");
        assert_eq!(
            first.path, second.path,
            "same soldr-binary identity must resolve to the same shim dir"
        );
    }

    #[test]
    fn persistent_shim_dir_rebuilds_when_a_shim_file_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("base");
        let bin = fake_soldr_bin(temp.path(), b"stub");
        let tools: &[&str] = &["cargo", "rustc"];

        let first = persistent_shim_dir_in(&base, &bin, tools).expect("first build");
        let cargo_shim = shim_tool_path(&first.path, "cargo");
        std::fs::remove_file(&cargo_shim).expect("remove one shim to simulate a partial dir");
        assert!(!cargo_shim.is_file());

        let rebuilt =
            persistent_shim_dir_in(&base, &bin, tools).expect("rebuild after missing shim file");
        assert_eq!(
            rebuilt.path, first.path,
            "the key is unchanged, so the dir must be repaired in place, not relocated"
        );
        assert!(
            cargo_shim.is_file(),
            "the missing shim must be rewritten on the next call"
        );
    }

    #[test]
    fn persistent_shim_dir_guard_survives_drop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("base");
        let bin = fake_soldr_bin(temp.path(), b"stub");
        let tools: &[&str] = &["cargo"];

        let dir_path = {
            let guard = persistent_shim_dir_in(&base, &bin, tools).expect("build");
            assert!(guard.persistent, "dylint shim dirs must be persistent");
            guard.path.clone()
        }; // guard dropped here

        assert!(
            dir_path.is_dir(),
            "persistent shim dirs must NOT be deleted when the guard drops"
        );
    }

    #[test]
    fn persistent_shim_dir_prunes_stale_sibling_keys() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("base");
        let bin_path = temp.path().join("fake-soldr-bin");
        std::fs::write(&bin_path, b"stub").expect("write fake binary");
        let tools: &[&str] = &["cargo"];

        let first = persistent_shim_dir_in(&base, &bin_path, tools).expect("first build");

        // Change the binary's content (and therefore length) so the key
        // changes on the next call, forcing a fresh build.
        std::fs::write(&bin_path, b"stub-with-different-length").expect("rewrite fake binary");

        let second = persistent_shim_dir_in(&base, &bin_path, tools).expect("second build");
        assert_ne!(
            first.path, second.path,
            "a changed binary identity must mint a new key dir"
        );
        assert!(first.path.is_dir(), "a just-used sibling must survive");
        let key = second
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        prune_stale_sibling_shim_dirs(&base, &key, std::time::Duration::ZERO);
        assert!(
            !first.path.is_dir(),
            "an idle sibling key dir is stale and must be pruned"
        );
        assert!(second.path.is_dir());
    }

    #[test]
    fn persistent_shim_dir_keeps_a_sibling_another_binary_is_using() {
        // Two soldr binaries share one root (installed soldr + a source build
        // in the local gate). Neither may delete the other's live shim dir.
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("base");
        let installed = temp.path().join("installed-soldr");
        let source_built = temp.path().join("source-soldr-build");
        std::fs::write(&installed, b"installed").expect("write installed");
        std::fs::write(&source_built, b"source-built-binary").expect("write source");
        let tools: &[&str] = &["cargo"];

        let a = persistent_shim_dir_in(&base, &installed, tools).expect("installed build");
        let b = persistent_shim_dir_in(&base, &source_built, tools).expect("source build");
        assert_ne!(a.path, b.path);
        assert!(
            shim_tool_path(&a.path, "cargo").is_file(),
            "a recently used sibling shim dir must survive another binary's build"
        );
    }
}
