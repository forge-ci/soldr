//! Bootstrap `rustup` from `https://static.rust-lang.org/rustup/dist/` when it
//! is missing on the host. Rust port of the logic that previously lived only in
//! `.github/actions/setup-soldr/ensure_rust_toolchain.py` (issue #406).
//!
//! Goal: turn the soldr CLI binary into a true one-stop entry point. On any
//! environment without a system-managed toolchain manager (e.g. nektos/act with
//! the `catthehacker/ubuntu:act-24.04` image, fresh container shells, alternative
//! CI providers), calling a soldr subcommand that needs `rustup` now triggers a
//! transparent install instead of failing with `command not found`.
//!
//! Two surfaces:
//!
//! - **Explicit:** [`bootstrap_rustup`] is what `soldr bootstrap` calls.
//! - **Implicit:** [`auto_bootstrap_if_missing_blocking`] runs from
//!   `crates/soldr-cli/src/binaries.rs` right before any rustup invocation when
//!   the binary is not on PATH and the user has not opted out via
//!   `SOLDR_NO_BOOTSTRAP=1`.
//!
//! Output of either path: a working `rustup` binary inside the soldr-managed
//! `bin/` directory. No global PATH mutation. The caller is responsible for
//! exporting the soldr-managed `CARGO_HOME` / `RUSTUP_HOME` to child processes
//! (`apply_implicit_toolchain_homes` already does this).
//!
//! The rustup-init binary is downloaded with the same trust policy as every
//! other soldr-fetched binary: pin via `SOLDR_CHECKSUMS_FILE`,
//! `SOLDR_TRUST_MODE=strict` refuses unpinned fetches.

use super::stream_download::{asset_http_client, get_request};
use super::stream_download::{
    send_asset_request, stream_response_to_temp_file, DownloadedAsset, ASSET_HEADER_TIMEOUT,
    ASSET_IDLE_TIMEOUT,
};
use super::trust::{verify_download, PinnedChecksumStore, TrustMode, VerifyOutcome};
use crate::core::{run_installer_command, InstallerWatchdogConfig, SoldrError, SoldrPaths};
use std::path::{Path, PathBuf};

/// Opt-out env var. When set to a truthy value (`1`, `true`, `yes`, `on`,
/// case-insensitive), the auto-bootstrap path becomes a no-op and the caller
/// falls back to the legacy "rustup not found" diagnostic.
pub const NO_BOOTSTRAP_ENV_VAR: &str = "SOLDR_NO_BOOTSTRAP";

/// Override env var for the rustup-init host triple. Useful for tests that
/// exercise per-platform URL construction without depending on the runner's
/// host.
pub const RUSTUP_INIT_TRIPLE_ENV_VAR: &str = "SOLDR_RUSTUP_INIT_TRIPLE_OVERRIDE";

/// Override env var for the rustup-init download URL. Lets internal CI image
/// tests point at a fixture server.
pub const RUSTUP_INIT_URL_ENV_VAR: &str = "SOLDR_RUSTUP_INIT_URL_OVERRIDE";

const RUSTUP_INIT_TOOL_NAME: &str = "rustup-init";
const RUSTUP_INIT_PSEUDO_VERSION: &str = "latest";
pub const RUSTUP_INIT_TIMEOUT_ENV_VAR: &str = "SOLDR_RUSTUP_INIT_TIMEOUT_SECS";

/// Result of a bootstrap attempt.
#[derive(Debug, Clone)]
pub struct BootstrapReport {
    /// Resolved path to the installed `rustup` binary (inside the soldr-managed
    /// bin directory).
    pub rustup_path: PathBuf,
    /// True if `rustup` was already present (nothing was downloaded).
    pub already_installed: bool,
    /// URL the rustup-init binary was fetched from, when applicable.
    pub source_url: Option<String>,
}

/// Outcome of an auto-bootstrap attempt from a sync caller.
#[derive(Debug)]
pub enum AutoBootstrapOutcome {
    /// `rustup` was already discoverable (PATH or soldr-managed bin).
    AlreadyInstalled(PathBuf),
    /// Bootstrap ran and installed rustup.
    Installed(BootstrapReport),
    /// User opted out via [`NO_BOOTSTRAP_ENV_VAR`]. Caller should fall back to
    /// the legacy missing-rustup diagnostic.
    OptedOut,
}

/// Look up `rustup` on PATH or inside the soldr-managed bin dir.
///
/// This is the cheap discovery that runs on every CLI invocation. Returns
/// `None` only if the host has no rustup at all.
///
/// Order:
///   1. `which("rustup")` — covers system rustup + any rustup that's
///      already on PATH.
///   2. `paths.bin/rustup{,.exe}` — where the bootstrap *copies* rustup
///      to so it can be discovered on every invocation.
///   3. `paths.root/cargo/bin/rustup{,.exe}` — where `rustup-init`'s
///      profile=minimal install actually puts the binary. The bootstrap
///      copies-up to `paths.bin/` after install, but a pre-existing
///      soldr-managed install (e.g. via `soldr toolchain ensure` from a
///      previous session) may only have the cargo/bin/ copy. Without
///      this fallback, the cheap discover misses it and forces an
///      unnecessary re-bootstrap on every cold-cache CLI start.
pub fn discover_rustup(paths: &SoldrPaths) -> Option<PathBuf> {
    if let Some(p) = which_on_path("rustup") {
        return Some(p);
    }
    let managed = managed_rustup_path(paths);
    if managed.is_file() {
        return Some(managed);
    }
    let cargo_bin = managed_cargo_home(paths)
        .join("bin")
        .join(rustup_filename());
    if cargo_bin.is_file() {
        return Some(cargo_bin);
    }
    None
}

/// Sync wrapper around the async bootstrap. Spawns a short-lived Tokio current-
/// thread runtime on a dedicated OS thread so the call works whether the caller
/// is inside an existing runtime (`#[tokio::main]`) or not.
pub fn auto_bootstrap_if_missing_blocking(
    paths: &SoldrPaths,
) -> Result<AutoBootstrapOutcome, SoldrError> {
    if let Some(p) = discover_rustup(paths) {
        return Ok(AutoBootstrapOutcome::AlreadyInstalled(p));
    }
    if no_bootstrap_opt_out() {
        return Ok(AutoBootstrapOutcome::OptedOut);
    }
    crate::core::forbid_toolchain_install_tripwire("rustup bootstrap")?;
    eprintln!(
        concat!(
            "soldr: rustup not found; bootstrapping the Rust toolchain into {} ",
            "(set {}=1 to disable auto-install). This downloads and installs ",
            "rustup + a toolchain and can take several minutes on a cold ",
            "machine - do not kill this process; interrupting mid-bootstrap ",
            "corrupts the toolchain state and forces a full re-bootstrap."
        ),
        paths.bin.display(),
        NO_BOOTSTRAP_ENV_VAR
    );
    let report = with_bootstrap_lock(paths, || bootstrap_rustup_blocking(paths))?;
    eprintln!("soldr: rustup bootstrap complete");
    Ok(AutoBootstrapOutcome::Installed(report))
}

/// Async variant of [`auto_bootstrap_if_missing_blocking`]. Call this when you
/// already own a tokio runtime context — avoids spawning a worker thread for
/// the download.
pub async fn auto_bootstrap_if_missing(
    paths: &SoldrPaths,
) -> Result<AutoBootstrapOutcome, SoldrError> {
    if let Some(p) = discover_rustup(paths) {
        return Ok(AutoBootstrapOutcome::AlreadyInstalled(p));
    }
    if no_bootstrap_opt_out() {
        return Ok(AutoBootstrapOutcome::OptedOut);
    }
    crate::core::forbid_toolchain_install_tripwire("rustup bootstrap")?;
    eprintln!(
        concat!(
            "soldr: rustup not found; bootstrapping the Rust toolchain into {} ",
            "(set {}=1 to disable auto-install). This downloads and installs ",
            "rustup + a toolchain and can take several minutes on a cold ",
            "machine - do not kill this process; interrupting mid-bootstrap ",
            "corrupts the toolchain state and forces a full re-bootstrap."
        ),
        paths.bin.display(),
        NO_BOOTSTRAP_ENV_VAR
    );
    // The lock is blocking, so hop off the async worker for it. The bootstrap
    // it guards is a download plus a toolchain install -- already far too long
    // to hold a runtime thread.
    let report = {
        let paths_clone = SoldrPaths::with_root(paths.root.clone());
        tokio::task::spawn_blocking(move || {
            with_bootstrap_lock(&paths_clone, || bootstrap_rustup_blocking(&paths_clone))
        })
        .await
        .map_err(|e| SoldrError::Other(format!("rustup bootstrap task failed: {e}")))??
    };
    eprintln!("soldr: rustup bootstrap complete");
    Ok(AutoBootstrapOutcome::Installed(report))
}

/// Download `rustup-init` for the current host and run it against the
/// soldr-managed `CARGO_HOME` / `RUSTUP_HOME`. Idempotent: a second call when
/// the resulting `rustup` binary already exists short-circuits without going
/// to the network.
pub async fn bootstrap_rustup(paths: &SoldrPaths) -> Result<BootstrapReport, SoldrError> {
    paths.ensure_dirs()?;

    let rustup_path = managed_rustup_path(paths);
    if rustup_path.is_file() {
        return Ok(BootstrapReport {
            rustup_path,
            already_installed: true,
            source_url: None,
        });
    }

    let url = rustup_init_download_url()?;
    let installer = download_rustup_init(&paths.cache, &url).await?;

    let cargo_home = managed_cargo_home(paths);
    let rustup_home = managed_rustup_home(paths);
    std::fs::create_dir_all(&cargo_home)?;
    std::fs::create_dir_all(&rustup_home)?;

    let mut command = std::process::Command::new(&installer);
    command.args([
        "-y",
        "--no-modify-path",
        "--default-toolchain",
        "none",
        "--profile",
        "minimal",
    ]);
    command.env("CARGO_HOME", &cargo_home);
    command.env("RUSTUP_HOME", &rustup_home);
    let status = run_rustup_init(&mut command, &installer)?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "bootstrap: rustup-init exited with status {status}"
        )));
    }

    // rustup-init installs `rustup` (and shims) into `$CARGO_HOME/bin`. Copy
    // the rustup binary into the soldr-managed `bin/` so it lives in a
    // predictable, soldr-owned location. The shims and toolchain state stay
    // under `cargo_home` / `rustup_home`.
    let installed = cargo_home.join("bin").join(rustup_filename());
    if !installed.is_file() {
        return Err(SoldrError::Other(format!(
            "bootstrap: expected rustup at {} after rustup-init ran but the file is missing",
            installed.display()
        )));
    }
    std::fs::copy(&installed, &rustup_path).map_err(|err| {
        SoldrError::Other(format!(
            "bootstrap: failed to copy {} -> {}: {err}",
            installed.display(),
            rustup_path.display()
        ))
    })?;

    crate::platform::fs::permissions::make_executable(&rustup_path)?;

    Ok(BootstrapReport {
        rustup_path,
        already_installed: false,
        source_url: Some(url),
    })
}

/// Sync entry point used by sync callers (e.g. `resolve_toolchain_binary`).
/// Spawns a current-thread runtime on a dedicated worker OS thread so it
/// composes safely whether or not the caller is already inside a tokio runtime.
pub fn bootstrap_rustup_blocking(paths: &SoldrPaths) -> Result<BootstrapReport, SoldrError> {
    let paths_clone = SoldrPaths::with_root(paths.root.clone());
    run_blocking(async move { bootstrap_rustup(&paths_clone).await })
}

/// How long a losing bootstrapper waits for the winner before installing
/// anyway. Generous because the thing being waited on is a real download plus
/// a toolchain install; abandoning early would just recreate the collision
/// this lock exists to prevent.
const BOOTSTRAP_LOCK_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

/// Serialize concurrent bootstraps of the same soldr home (soldr#2728).
///
/// `discover_rustup` returning `None` is not a promise that no one else is
/// installing. `soldr lint deps` fans out three child soldr processes against
/// one home, and on a home with no rustup yet all three miss the discovery
/// check at the same instant and install into the same `bin/` concurrently.
/// On Windows the losers do not benignly overwrite — they get
/// `ERROR_SHARING_VIOLATION` (os error 32) copying over a `rustup.exe` that
/// another process holds open — and the "fall back to rustup on PATH" recovery
/// cannot help, because the point of a soldr-managed home is that PATH's
/// rustup is not the one wanted. The failure surfaced three layers away as
/// `rustup which rustc: program not found`.
///
/// Runs `install` under an exclusive file lock, and re-checks discovery after
/// acquiring it: the winner does the work, and everyone who queued behind it
/// finds rustup already present and returns it instead of installing a second
/// copy. That double-check is the point — a lock alone would merely serialize
/// three identical downloads.
fn with_bootstrap_lock<F>(paths: &SoldrPaths, install: F) -> Result<BootstrapReport, SoldrError>
where
    F: FnOnce() -> Result<BootstrapReport, SoldrError>,
{
    with_bootstrap_lock_using(paths, discover_rustup, install)
}

/// [`with_bootstrap_lock`] with the discovery probe injected.
///
/// Production passes [`discover_rustup`], which consults `PATH` before the
/// managed home. That is right for the real call path — it is only reached
/// once discovery has already returned `None` — but it makes the post-lock
/// re-check untestable directly: on any machine with rustup on `PATH` the
/// re-check short-circuits and the install never runs, so the test would
/// assert about the developer's machine rather than about this function.
/// Injecting the probe tests the logic instead of the environment.
fn with_bootstrap_lock_using<D, F>(
    paths: &SoldrPaths,
    discover: D,
    install: F,
) -> Result<BootstrapReport, SoldrError>
where
    D: Fn(&SoldrPaths) -> Option<PathBuf>,
    F: FnOnce() -> Result<BootstrapReport, SoldrError>,
{
    use fs2::FileExt as _;

    let _ = std::fs::create_dir_all(&paths.bin);
    let lock_path = paths.bin.join(".rustup-bootstrap.lock");
    let lock = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(file) => file,
        // A home we cannot create a lock file in is a home we cannot
        // serialize on. Installing unlocked is what happened before this
        // existed, so degrade to it rather than failing the build outright.
        Err(_) => return install(),
    };

    let started = std::time::Instant::now();
    let deadline = started + BOOTSTRAP_LOCK_BUDGET;
    let mut waited = false;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(_) => {
                if !waited {
                    waited = true;
                    // Never wait silently: an unexplained multi-minute pause
                    // is indistinguishable from a hang.
                    eprintln!(
                        "soldr: another process is bootstrapping rustup into {}; waiting",
                        paths.bin.display()
                    );
                }
                if std::time::Instant::now() >= deadline {
                    eprintln!(
                        "soldr: rustup bootstrap lock still held after {}s; installing directly",
                        started.elapsed().as_secs()
                    );
                    return install();
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }

    // The winner may already have finished while we queued. Re-check before
    // spending a second download on a rustup that is now present.
    if let Some(path) = discover(paths) {
        if waited {
            eprintln!(
                "soldr: rustup bootstrapped by another process after {}ms",
                started.elapsed().as_millis()
            );
        }
        let _ = fs2::FileExt::unlock(&lock);
        return Ok(BootstrapReport {
            rustup_path: path,
            already_installed: true,
            source_url: None,
        });
    }

    let result = install();
    let _ = fs2::FileExt::unlock(&lock);
    result
}

/// Path the bootstrap routine copies `rustup` to (under the soldr-managed `bin/`).
pub fn managed_rustup_path(paths: &SoldrPaths) -> PathBuf {
    paths.bin.join(rustup_filename())
}

/// Where `rustup-init` lays out the toolchain it manages. Callers can pass
/// these to child rustup invocations via `CARGO_HOME` / `RUSTUP_HOME` so the
/// soldr-managed state doesn't bleed into the user's `~/.cargo` / `~/.rustup`.
pub fn managed_cargo_home(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("cargo")
}

pub fn managed_rustup_home(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("rustup")
}

fn rustup_filename() -> &'static str {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        "rustup.exe"
    } else {
        "rustup"
    }
}

fn rustup_init_filename() -> &'static str {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        "rustup-init.exe"
    } else {
        "rustup-init"
    }
}

/// Compute the URL for rustup-init based on the current host. Respects the
/// `SOLDR_RUSTUP_INIT_URL_OVERRIDE` and `SOLDR_RUSTUP_INIT_TRIPLE_OVERRIDE`
/// env vars used by tests.
pub fn rustup_init_download_url() -> Result<String, SoldrError> {
    if let Ok(override_url) = std::env::var(RUSTUP_INIT_URL_ENV_VAR) {
        let trimmed = override_url.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let triple = rustup_init_host_triple()?;
    let suffix = if triple.contains("-windows-") {
        ".exe"
    } else {
        ""
    };
    Ok(format!(
        "https://static.rust-lang.org/rustup/dist/{triple}/rustup-init{suffix}"
    ))
}

/// Host triple in rustup-init's URL namespace. Mirrors
/// `rustup_init_target_triple()` in the Python script
/// (`ensure_rust_toolchain.py`) — rustup-init uses `*-unknown-linux-gnu` and
/// `*-pc-windows-msvc`, not the abstract MSVC default soldr's `TargetTriple`
/// resolves at build time.
pub fn rustup_init_host_triple() -> Result<String, SoldrError> {
    if let Ok(override_triple) = std::env::var(RUSTUP_INIT_TRIPLE_ENV_VAR) {
        let trimmed = override_triple.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    // The libc of this build: compile-time on Linux (mirrors the
    // target_env the binary was built for), always None elsewhere.
    let is_musl =
        crate::platform::host::facts::libc() == crate::platform::host::facts::HostLibc::Musl;
    Ok(match (os, arch) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc".to_string(),
        ("windows", "aarch64") => "aarch64-pc-windows-msvc".to_string(),
        ("windows", "x86") => "i686-pc-windows-msvc".to_string(),
        ("macos", "x86_64") => "x86_64-apple-darwin".to_string(),
        ("macos", "aarch64") => "aarch64-apple-darwin".to_string(),
        ("linux", "x86_64") if is_musl => "x86_64-unknown-linux-musl".to_string(),
        ("linux", "aarch64") if is_musl => "aarch64-unknown-linux-musl".to_string(),
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu".to_string(),
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu".to_string(),
        ("linux", "x86") => "i686-unknown-linux-gnu".to_string(),
        _ => {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "bootstrap: unsupported host for rustup-init: os={os}, arch={arch}"
            )));
        }
    })
}

/// One download attempt for `rustup-init`. Every error is
/// [`SoldrError::Network`], which is what [`super::retry::is_transient`]
/// matches.
async fn download_rustup_init_asset(url: &str) -> Result<DownloadedAsset, SoldrError> {
    let client = asset_http_client("rustup-init bootstrap")?;
    let resp = send_asset_request(get_request(&client, url), url, ASSET_HEADER_TIMEOUT)
        .await
        .map_err(|error| SoldrError::Network(format!("bootstrap: GET {url}: {error}")))?;
    stream_response_to_temp_file(resp, url, ASSET_IDLE_TIMEOUT).await
}

async fn download_rustup_init(cache_dir: &Path, url: &str) -> Result<PathBuf, SoldrError> {
    std::fs::create_dir_all(cache_dir)?;
    let destination = cache_dir.join(rustup_init_filename());

    // soldr#2132: retry the download. This one runs during bootstrap, before
    // any build starts, so a blip here fails the toolchain rather than a
    // compile -- the same shape as the cmake failure that stopped the v0.8.30
    // release, one step earlier. Checksum verification stays below, outside
    // the retry.
    let downloaded =
        super::retry::with_asset_backoff("rustup-init", || download_rustup_init_asset(url)).await?;

    let sha256 = downloaded.sha256();

    let store = PinnedChecksumStore::from_env()?;
    let mode = TrustMode::from_env();
    let outcome = verify_download(
        RUSTUP_INIT_TOOL_NAME,
        RUSTUP_INIT_PSEUDO_VERSION,
        rustup_init_filename(),
        sha256,
        &store,
        mode,
    )?;
    match outcome {
        VerifyOutcome::Verified { sha256 } => {
            eprintln!("soldr: trust: verified rustup-init sha256={sha256}");
        }
        VerifyOutcome::Unverified { sha256 } => {
            eprintln!("soldr: trust: unverified rustup-init sha256={sha256}");
        }
    }

    let tmp = destination.with_extension("tmp");
    std::fs::copy(downloaded.path(), &tmp)?;

    // Add execute bits (no-op on Windows, where Unix mode bits are
    // meaningless) before the tmp file becomes the destination.
    crate::platform::fs::permissions::make_executable(&tmp)?;

    if destination.exists() {
        let _ = std::fs::remove_file(&destination);
    }
    std::fs::rename(&tmp, &destination)?;
    Ok(destination)
}

fn which_on_path(tool: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let exe_names: Vec<String> =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            // PATHEXT controls which extensions PATH lookups try.
            let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            std::iter::once(tool.to_string())
                .chain(
                    pathext
                        .split(';')
                        .map(str::trim)
                        .filter(|e| !e.is_empty())
                        .map(|ext| format!("{tool}{ext}")),
                )
                .collect()
        } else {
            vec![tool.to_string()]
        };
    for dir in std::env::split_paths(&paths) {
        for name in &exe_names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn no_bootstrap_opt_out() -> bool {
    match std::env::var(NO_BOOTSTRAP_ENV_VAR) {
        Ok(value) => crate::core::flag_value(&value),
        Err(_) => false,
    }
}

fn run_rustup_init(
    command: &mut std::process::Command,
    installer: &Path,
) -> Result<std::process::ExitStatus, SoldrError> {
    run_installer_command(
        command,
        &format!("bootstrap: rustup-init ({})", installer.display()),
        "bootstrap",
        InstallerWatchdogConfig::from_env(RUSTUP_INIT_TIMEOUT_ENV_VAR),
    )
}

/// Run `fut` to completion on a fresh tokio current-thread runtime spawned on
/// a dedicated OS thread. Works whether the caller is already inside a tokio
/// runtime (it is, when reached from `#[tokio::main]`) or not.
fn run_blocking<F, T>(fut: F) -> Result<T, SoldrError>
where
    F: std::future::Future<Output = Result<T, SoldrError>> + Send + 'static,
    T: Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name("soldr-bootstrap".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| SoldrError::Other(format!("bootstrap: tokio init failed: {e}")))?;
            crate::platform::wait::block_on_for(&runtime, fut, crate::platform::wait::LONG_CHILD)
                .unwrap_or_else(|error| Err(SoldrError::Other(format!("bootstrap: {error}"))))
        })
        .map_err(|e| SoldrError::Other(format!("bootstrap: spawn thread failed: {e}")))?;
    crate::platform::wait::join_for(handle, crate::platform::wait::LONG_CHILD)
        .map_err(|_| SoldrError::Other("bootstrap: worker thread panicked or overran".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn host_triple_uses_unknown_linux_gnu_on_linux_x86_64() {
        let _env_lock = test_env_lock();
        let _guard = EnvVarGuard::remove(RUSTUP_INIT_TRIPLE_ENV_VAR);
        if std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64" {
            let expected = if crate::platform::host::facts::libc()
                == crate::platform::host::facts::HostLibc::Musl
            {
                "x86_64-unknown-linux-musl"
            } else {
                "x86_64-unknown-linux-gnu"
            };
            assert_eq!(rustup_init_host_triple().unwrap(), expected);
        }
    }

    #[test]
    fn host_triple_uses_pc_windows_msvc_on_windows_x86_64() {
        let _env_lock = test_env_lock();
        let _guard = EnvVarGuard::remove(RUSTUP_INIT_TRIPLE_ENV_VAR);
        if std::env::consts::OS == "windows" && std::env::consts::ARCH == "x86_64" {
            assert_eq!(rustup_init_host_triple().unwrap(), "x86_64-pc-windows-msvc");
        }
    }

    #[test]
    fn host_triple_honors_override_env_var() {
        let _env_lock = test_env_lock();
        let _guard = EnvVarGuard::set(RUSTUP_INIT_TRIPLE_ENV_VAR, "aarch64-apple-darwin");
        assert_eq!(rustup_init_host_triple().unwrap(), "aarch64-apple-darwin");
    }

    #[test]
    fn download_url_appends_exe_for_windows_triple_only() {
        let _env_lock = test_env_lock();
        let _url = EnvVarGuard::remove(RUSTUP_INIT_URL_ENV_VAR);
        let _triple = EnvVarGuard::set(RUSTUP_INIT_TRIPLE_ENV_VAR, "x86_64-pc-windows-msvc");
        assert_eq!(
            rustup_init_download_url().unwrap(),
            "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"
        );

        let _triple2 = EnvVarGuard::set(RUSTUP_INIT_TRIPLE_ENV_VAR, "x86_64-unknown-linux-gnu");
        assert_eq!(
            rustup_init_download_url().unwrap(),
            "https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init"
        );
    }

    #[test]
    fn download_url_override_short_circuits_triple_resolution() {
        let _env_lock = test_env_lock();
        let _guard = EnvVarGuard::set(
            RUSTUP_INIT_URL_ENV_VAR,
            "http://127.0.0.1:9/rustup-init-test",
        );
        assert_eq!(
            rustup_init_download_url().unwrap(),
            "http://127.0.0.1:9/rustup-init-test"
        );
    }

    /// soldr#2740: SOLDR_NO_BOOTSTRAP is soldr-owned, so it takes the
    /// allowlist rule. `maybe` reading as "on" would leave a user with no
    /// toolchain, which is why unknown-is-off is the right default here.
    /// The spelling matrix itself is owned by `soldr_core::core::env_flag`.
    #[test]
    fn no_bootstrap_uses_the_owned_flag_rule() {
        for v in ["1", "true", "TRUE", "Yes", "on", " 1 "] {
            assert!(crate::core::flag_value(v), "expected {v:?} to be truthy");
        }
        for v in ["0", "false", "no", "off", "", "maybe"] {
            assert!(!crate::core::flag_value(v), "expected {v:?} to be falsy");
        }
    }

    #[test]
    fn rustup_init_timeout_is_an_explicit_safety_ceiling() {
        let _env_lock = test_env_lock();

        {
            let _guard = EnvVarGuard::set(RUSTUP_INIT_TIMEOUT_ENV_VAR, "17");
            assert_eq!(
                InstallerWatchdogConfig::from_env(RUSTUP_INIT_TIMEOUT_ENV_VAR).safety_timeout,
                Duration::from_secs(17)
            );
        }

        for value in ["0", "-1", "not-a-number"] {
            let _guard = EnvVarGuard::set(RUSTUP_INIT_TIMEOUT_ENV_VAR, value);
            assert_eq!(
                InstallerWatchdogConfig::from_env(RUSTUP_INIT_TIMEOUT_ENV_VAR).safety_timeout,
                Duration::from_secs(crate::core::DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS)
            );
        }

        let _guard = EnvVarGuard::remove(RUSTUP_INIT_TIMEOUT_ENV_VAR);
        assert_eq!(
            InstallerWatchdogConfig::from_env(RUSTUP_INIT_TIMEOUT_ENV_VAR).safety_timeout,
            Duration::from_secs(crate::core::DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS)
        );
    }

    #[test]
    fn auto_bootstrap_reports_opted_out_when_env_var_set() {
        // Take the env lock BEFORE the PATH probe so the two
        // `which_on_path` invocations (one here, one inside
        // `discover_rustup`) see the same PATH. Without the lock, a
        // concurrent test mutating PATH between those two calls could
        // let us slip past the skip-check, then find rustup inside
        // auto_bootstrap_if_missing_blocking, then panic with
        // "expected OptedOut, got AlreadyInstalled(...)" — observed
        // when this module's tests share a process with the rest of
        // the suite after the four-crate → one-crate collapse.
        let _env_lock = test_env_lock();
        // The test only exercises the OptedOut path. Skip when rustup
        // is already discoverable on PATH — the early `if let Some` in
        // auto_bootstrap_if_missing_blocking would then return
        // AlreadyInstalled, which is correct but not what we're
        // testing.
        if which_on_path("rustup").is_some() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let _guard = EnvVarGuard::set(NO_BOOTSTRAP_ENV_VAR, "1");
        match auto_bootstrap_if_missing_blocking(&paths).unwrap() {
            AutoBootstrapOutcome::OptedOut => {}
            other => panic!("expected OptedOut, got {other:?}"),
        }
    }

    /// Module-wide lock that serialises every test in this file that
    /// touches the process-wide env. Every such test must call
    /// `test_env_lock()` at the top and bind its return value for the
    /// scope of the test. This was harmless when the rustup_init
    /// module lived in its own crate (`cargo test` gave it a dedicated
    /// process), but became necessary after the four-crate → one-crate
    /// collapse — the env mutations now share a process with every
    /// other test in `soldr-cli`.
    fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// RAII helper so tests can mutate process env without leaking the
    /// mutation across cases. Does NOT take the env lock — the
    /// enclosing test must already hold one via `test_env_lock()` for
    /// the full duration of all guards it constructs.
    struct EnvVarGuard {
        key: String,
        prior: Option<String>,
    }
    impl EnvVarGuard {
        fn set(key: &str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key: key.to_string(),
                prior,
            }
        }
        fn remove(key: &str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::remove_var(key);
            Self {
                key: key.to_string(),
                prior,
            }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

#[cfg(test)]
mod bootstrap_lock_tests {
    use super::*;

    fn temp_paths(label: &str) -> (tempfile::TempDir, SoldrPaths) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let root = dir.path().join(label);
        std::fs::create_dir_all(&root).expect("root");
        let paths = SoldrPaths::with_root(root);
        (dir, paths)
    }

    /// Stand-in for "no rustup anywhere". The real `discover_rustup` consults
    /// `PATH` first, so using it here would make these tests assert about
    /// whatever machine they run on -- which is exactly how the first version
    /// of them passed on Windows and failed on every Unix target-run lane.
    fn absent(_paths: &SoldrPaths) -> Option<PathBuf> {
        None
    }

    /// The lock must not change the uncontended path: one caller installs.
    #[test]
    fn an_uncontended_bootstrap_runs_the_install() {
        let (_dir, paths) = temp_paths("uncontended");
        let ran = std::sync::atomic::AtomicUsize::new(0);

        let report = with_bootstrap_lock_using(&paths, absent, || {
            ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(BootstrapReport {
                rustup_path: paths.bin.join("rustup"),
                already_installed: false,
                source_url: None,
            })
        })
        .expect("uncontended bootstrap");

        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!report.already_installed);
    }

    /// soldr#2728's whole point: the caller that loses the race must find the
    /// winner's rustup and return it, not install a second copy over the top.
    /// A lock that merely serialized three identical downloads would pass a
    /// "did it collide" test while still doing the work three times.
    #[test]
    fn a_caller_that_finds_rustup_already_present_does_not_install_again() {
        let (_dir, paths) = temp_paths("already-there");
        std::fs::create_dir_all(&paths.bin).expect("bin");
        // Stands in for the winner's completed install.
        let installed = managed_rustup_path(&paths);
        std::fs::write(&installed, b"#!/bin/sh\n").expect("seed rustup");
        let found = installed.clone();

        let ran = std::sync::atomic::AtomicUsize::new(0);
        let report = with_bootstrap_lock_using(
            &paths,
            move |_paths| Some(found.clone()),
            || {
                ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                panic!("must not install when rustup is already present");
            },
        )
        .expect("second caller");

        assert_eq!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the install closure must not run"
        );
        assert!(report.already_installed);
        assert_eq!(report.rustup_path, installed);
    }

    /// A home the lock file cannot be created in must still bootstrap --
    /// unlocked is what happened before this existed, and is strictly better
    /// than refusing to build.
    #[test]
    fn an_unlockable_home_degrades_to_an_unlocked_install() {
        let (_dir, paths) = temp_paths("unlockable");
        // Occupy `bin` with a file so `create_dir_all` and the lock open both
        // fail, without permissions games that differ per platform.
        std::fs::write(&paths.bin, b"not a directory").expect("block bin");

        let report = with_bootstrap_lock_using(&paths, absent, || {
            Ok(BootstrapReport {
                rustup_path: paths.bin.join("rustup"),
                already_installed: false,
                source_url: None,
            })
        })
        .expect("degraded bootstrap still runs");

        assert!(!report.already_installed);
    }
}
