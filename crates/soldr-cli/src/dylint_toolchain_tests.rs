//! Unit coverage split from `dylint_toolchain.rs` for the soldr#2493 1,000-line
//! production-source ceiling.
//!
//! Also covers the driver half that soldr#2945 moved out to `dylint_driver.rs`
//! for the same ceiling: those tests share this module's `EnvVarGuard` /
//! `sample_plan` scaffolding, so duplicating it into a second test file would
//! have bought nothing but a second copy to keep in sync.

use super::*;

use crate::dylint_driver::{
    driver_source_build_warning, dylint_driver_version, unavailable_driver_error,
};

const COMMIT: &str = "31a9463c6e2794a59ce57a8f37abc6966afc2a58";

#[test]
fn parses_prebuilt_dylint_driver_version() {
    assert_eq!(
        dylint_driver_version("dylint-driver 6.0.3\n"),
        Some("6.0.3")
    );
    assert_eq!(dylint_driver_version(""), None);
}

fn sample_map(selected: &str) -> Vec<u8> {
    format!(
        r#"{{
              "schema_version": 1,
              "nightlies": {{
                "nightly-2026-01-18": {{
                  "rust_version": "1.94",
                  "rustc_release": "1.94.0-nightly",
                  "rustc_commit_hash": "{COMMIT}"
                }},
                "nightly-2026-01-17": {{
                  "rust_version": "1.94",
                  "rustc_release": "1.94.0-nightly",
                  "rustc_commit_hash": "1111111111111111111111111111111111111111"
                }}
              }},
              "versions": {{
                "1.94": {{
                  "nightlies": ["nightly-2026-01-18", "nightly-2026-01-17"],
                  "selected": "{selected}"
                }}
              }}
            }}"#
    )
    .into_bytes()
}

#[test]
fn selects_first_newest_nightly_and_full_identity() {
    let plan =
        select_from_map(&sample_map("nightly-2026-01-18"), "1.94").expect("select map entry");
    assert_eq!(plan.channel, "nightly-2026-01-18");
    assert_eq!(
        plan.cache_identity(),
        format!("nightly-2026-01-18|1.94.0-nightly|{COMMIT}")
    );
}

#[test]
fn rejects_selected_nightly_that_is_not_first() {
    let error = select_from_map(&sample_map("nightly-2026-01-17"), "1.94")
        .expect_err("must reject a non-first selection");
    assert!(error.to_string().contains("newest-first contract"));
}

#[test]
fn explicit_nightly_uses_mapped_identity_without_installing() {
    let plan = select_explicit_from_map(&sample_map("nightly-2026-01-18"), "nightly-2026-01-18")
        .expect("explicit map entry");
    assert_eq!(plan.channel, "nightly-2026-01-18");
    assert_eq!(plan.compiler_commit, COMMIT);
}

#[test]
fn extracts_major_minor_versions() {
    assert_eq!(major_minor("1.94.1").as_deref(), Some("1.94"));
    assert_eq!(major_minor("1.94.0-nightly").as_deref(), Some("1.94"));
    assert_eq!(major_minor("stable"), None);
}

#[test]
fn recognizes_only_explicit_dated_nightlies() {
    assert!(is_dated_nightly("nightly-2026-04-16"));
    assert!(is_dated_nightly(
        "nightly-2026-04-16-x86_64-unknown-linux-gnu"
    ));
    assert!(!is_dated_nightly("nightly"));
    assert!(!is_dated_nightly("nightly-latest"));
    assert!(!is_dated_nightly("nightly-2026-04-16junk"));
    assert!(!is_dated_nightly("nightly-2026-04-16-"));
    assert!(!is_dated_nightly("nightly-2026-04"));
    assert!(!is_dated_nightly("1.97.0"));
}

#[test]
fn qualifies_nightly_names_with_the_compiler_host() {
    assert!(!is_fully_qualified_nightly("nightly-2026-01-18"));
    assert!(is_fully_qualified_nightly(
        "nightly-2026-01-18-x86_64-unknown-linux-gnu"
    ));
    let host = parse_compiler_host(
        "rustc 1.94.0-nightly\nrelease: 1.94.0-nightly\nhost: x86_64-unknown-linux-gnu\n",
    )
    .expect("parse host");
    assert_eq!(host, "x86_64-unknown-linux-gnu");
}

// -----------------------------------------------------------------
// Warm-run prepared-plan marker (issue: dylint warm-run fast path).
// -----------------------------------------------------------------

use std::ffi::{OsStr, OsString};
use std::sync::Mutex;

/// Guards mutation of process-global env vars so these tests never
/// race other tests in this binary that read the same keys. Mirrors
/// the pattern in `toolchain.rs`'s test module.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    /// The precedence tests need a *known-absent* variable, not merely an
    /// unset-by-default one: a developer with `RUSTUP_TOOLCHAIN` exported
    /// would otherwise silently exercise a different tier.
    fn unset(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn sample_plan() -> DylintToolchainPlan {
    DylintToolchainPlan::identity(
        "nightly-2026-01-18".to_string(),
        "1.94.0-nightly".to_string(),
        COMMIT.to_string(),
    )
}

/// Creates a complete filesystem-shaped toolchain without spawning the
/// manager. The readiness predicate deliberately needs more than a directory:
/// the authoritative channel manifest and compiler must both be present.
fn stub_installed_toolchain(rustup_home: &Path, channel: &str) {
    let dir = rustup_home
        .join("toolchains")
        .join(format!("{channel}-stub-triple"));
    write_ready_toolchain(&dir);
}

fn write_ready_toolchain(dir: &Path) {
    std::fs::create_dir_all(dir.join("bin")).expect("create stub bin dir");
    std::fs::create_dir_all(dir.join("lib/rustlib")).expect("create stub manifest dir");
    std::fs::write(
        dir.join("bin")
            .join(crate::platform::executable::name::native("rustc")),
        b"stub compiler",
    )
    .expect("write stub compiler");
    std::fs::write(
        dir.join(TOOLCHAIN_CHANNEL_MANIFEST),
        b"manifest-version = '2'\n",
    )
    .expect("write stub manifest");
    // soldr#3376: a directory named for this host must carry its std files.
    let lib = dir
        .join("lib/rustlib")
        .join(crate::pyo3_detect::host_triple())
        .join("lib");
    std::fs::create_dir_all(&lib).expect("create stub std dir");
    std::fs::write(lib.join("libcore-0123456789abcdef.rlib"), b"core").expect("stub libcore");
    std::fs::write(lib.join("libstd-0123456789abcdef.rlib"), b"std").expect("stub libstd");
}

fn stub_partial_toolchain(rustup_home: &Path, channel: &str) -> PathBuf {
    let dir = rustup_home
        .join("toolchains")
        .join(format!("{channel}-stub-triple"));
    std::fs::create_dir_all(&dir).expect("create partial toolchain dir");
    dir
}

#[test]
fn prepared_marker_roundtrip_hits_when_fresh_and_installed() {
    let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
    let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
    let plan = sample_plan();
    stub_installed_toolchain(rustup_home.path(), &plan.channel);

    write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

    let loaded = load_prepared_marker_from(
        soldr_root.path(),
        rustup_home.path(),
        "1.94",
        Duration::from_secs(60 * 60),
        SystemTime::now(),
    );
    assert_eq!(loaded, Some(plan));
}

#[test]
fn prepared_marker_accepts_fully_qualified_toolchain_directory() {
    let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
    let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
    let mut plan = sample_plan();
    plan.channel.push_str("-x86_64-unknown-linux-gnu");
    let directory = rustup_home.path().join("toolchains").join(&plan.channel);
    write_ready_toolchain(&directory);

    write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

    let loaded = load_prepared_marker_from(
        soldr_root.path(),
        rustup_home.path(),
        "1.94",
        Duration::from_secs(60 * 60),
        SystemTime::now(),
    );
    assert_eq!(loaded, Some(plan));
}

#[test]
fn prepared_marker_rejected_when_ttl_expired() {
    let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
    let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
    let plan = sample_plan();
    stub_installed_toolchain(rustup_home.path(), &plan.channel);

    write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

    // "now" far in the future relative to the just-written marker's
    // mtime, well past a 1-second TTL.
    let far_future = SystemTime::now() + Duration::from_secs(3600);
    let loaded = load_prepared_marker_from(
        soldr_root.path(),
        rustup_home.path(),
        "1.94",
        Duration::from_secs(1),
        far_future,
    );
    assert_eq!(loaded, None);
}

#[test]
fn prepared_marker_ttl_zero_never_trusts_marker() {
    let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
    let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
    let plan = sample_plan();
    stub_installed_toolchain(rustup_home.path(), &plan.channel);

    write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

    let loaded = load_prepared_marker_from(
        soldr_root.path(),
        rustup_home.path(),
        "1.94",
        Duration::ZERO,
        SystemTime::now(),
    );
    assert_eq!(loaded, None);
}

#[test]
fn prepared_marker_rejected_when_malformed() {
    let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
    let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
    stub_installed_toolchain(rustup_home.path(), "nightly-2026-01-18");

    let path = prepared_marker_path(soldr_root.path(), "1.94");
    std::fs::create_dir_all(path.parent().unwrap()).expect("create marker dir");
    std::fs::write(&path, "not-a-valid-identity-line\n").expect("write malformed marker");

    let loaded = load_prepared_marker_from(
        soldr_root.path(),
        rustup_home.path(),
        "1.94",
        Duration::from_secs(60 * 60),
        SystemTime::now(),
    );
    assert_eq!(loaded, None);
}

#[test]
fn prepared_marker_rejected_when_toolchain_dir_missing() {
    let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
    // No stubbed toolchain directory under this rustup_home.
    let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
    let plan = sample_plan();

    write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

    let loaded = load_prepared_marker_from(
        soldr_root.path(),
        rustup_home.path(),
        "1.94",
        Duration::from_secs(60 * 60),
        SystemTime::now(),
    );
    assert_eq!(loaded, None);
}

#[test]
fn dylint_readiness_matrix_requires_manifest_and_compiler() {
    let rustup_home = tempfile::tempdir().expect("manager home tempdir");
    let channel = "nightly-2026-01-18";
    assert_eq!(
        dylint_toolchain_readiness_at(rustup_home.path(), channel),
        DylintToolchainReadiness::Missing
    );

    let partial = stub_partial_toolchain(rustup_home.path(), channel);
    let expect_partial =
        |missing: &[&str]| match dylint_toolchain_readiness_at(rustup_home.path(), channel) {
            DylintToolchainReadiness::Partial {
                missing: actual, ..
            } => {
                assert_eq!(actual, missing)
            }
            other => panic!("expected partial readiness, got {other:?}"),
        };
    expect_partial(&[TOOLCHAIN_CHANNEL_MANIFEST, "bin/rustc"]);

    std::fs::create_dir_all(partial.join("bin")).expect("compiler parent");
    let compiler = partial
        .join("bin")
        .join(crate::platform::executable::name::native("rustc"));
    std::fs::write(&compiler, b"stub compiler").expect("compiler");
    expect_partial(&[TOOLCHAIN_CHANNEL_MANIFEST]);

    std::fs::create_dir_all(partial.join("lib/rustlib")).expect("manifest parent");
    std::fs::write(
        partial.join(TOOLCHAIN_CHANNEL_MANIFEST),
        b"manifest-version = '2'\n",
    )
    .expect("manifest");
    assert!(matches!(
        dylint_toolchain_readiness_at(rustup_home.path(), channel),
        DylintToolchainReadiness::Ready { .. }
    ));

    std::fs::remove_file(&compiler).expect("remove compiler");
    expect_partial(&["bin/rustc"]);
}

#[test]
fn prepared_marker_rejects_a_partial_toolchain() {
    let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
    let manager_home = tempfile::tempdir().expect("manager home tempdir");
    let plan = sample_plan();
    stub_partial_toolchain(manager_home.path(), &plan.channel);
    write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

    assert_eq!(
        load_prepared_marker_from(
            soldr_root.path(),
            manager_home.path(),
            "1.94",
            Duration::from_secs(60 * 60),
            SystemTime::now(),
        ),
        None,
        "a warm marker must not bypass the same partial-toolchain check as the cold path"
    );
}

#[test]
fn manager_exit_zero_but_partial_toolchain_fails_without_deleting_it() {
    let manager_home = tempfile::tempdir().expect("manager home tempdir");
    let channel = "nightly-2026-01-18";
    let partial = manager_home
        .path()
        .join("toolchains")
        .join(format!("{channel}-stub-triple"));
    let error = ensure_dylint_toolchain_ready_at(manager_home.path(), channel, || {
        std::fs::create_dir_all(&partial).expect("fake manager leaves partial dir");
        Ok(0)
    })
    .expect_err("an exit-zero manager must not bless a partial tree")
    .to_string();

    assert!(
        partial.is_dir(),
        "the Dylint path must not delete shared state"
    );
    assert!(error.contains(&format!("{channel}-stub-triple")), "{error}");
    assert!(error.contains(TOOLCHAIN_CHANNEL_MANIFEST), "{error}");
    let manager = ["rust", "up"].concat();
    assert!(
        error.contains(&format!(
            "soldr {manager} toolchain uninstall {channel}-stub-triple"
        )),
        "{error}"
    );
    assert!(error.contains(&partial.display().to_string()), "{error}");
}

#[test]
fn clean_manager_install_is_accepted_after_readiness_recheck() {
    let manager_home = tempfile::tempdir().expect("manager home tempdir");
    let channel = "nightly-2026-01-18";
    ensure_dylint_toolchain_ready_at(manager_home.path(), channel, || {
        stub_installed_toolchain(manager_home.path(), channel);
        Ok(0)
    })
    .expect("a clean install must pass the same readiness predicate");
}

#[test]
fn parse_marker_identity_roundtrips_cache_identity_format() {
    let plan = sample_plan();
    let parsed = parse_marker_identity(&plan.cache_identity()).expect("parse identity line");
    assert_eq!(parsed, plan);
}

#[test]
fn parse_marker_identity_rejects_malformed_lines() {
    assert!(parse_marker_identity("garbage").is_none());
    assert!(parse_marker_identity("nightly-2026-01-18|1.94.0-nightly|short").is_none());
    assert!(parse_marker_identity("not-nightly|1.94.0-nightly|").is_none());
}

#[test]
fn sanitize_marker_key_strips_path_hostile_characters() {
    assert_eq!(sanitize_marker_key("1.94"), "1.94");
    assert_eq!(
        sanitize_marker_key("nightly-2026-01-18"),
        "nightly-2026-01-18"
    );
    assert_eq!(sanitize_marker_key("a/b\\c:d"), "a_b_c_d");
}

#[test]
fn truthy_env_bypasses_marker_lookup() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    {
        let _env = EnvVarGuard::set(REVERIFY_ENV_VAR, "1");
        assert!(crate::core::flag(REVERIFY_ENV_VAR));
    }
    {
        let _env = EnvVarGuard::set(REVERIFY_ENV_VAR, "true");
        assert!(crate::core::flag(REVERIFY_ENV_VAR));
    }
    {
        let _env = EnvVarGuard::set(REVERIFY_ENV_VAR, "0");
        assert!(!crate::core::flag(REVERIFY_ENV_VAR));
    }
    assert!(!crate::core::flag(REVERIFY_ENV_VAR));
}

#[test]
fn prepare_ttl_parses_env_override_and_falls_back_to_default() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    {
        let _env = EnvVarGuard::set(PREPARE_TTL_ENV_VAR, "60");
        assert_eq!(prepare_ttl(), Duration::from_secs(60));
    }
    {
        let _env = EnvVarGuard::set(PREPARE_TTL_ENV_VAR, "0");
        assert_eq!(prepare_ttl(), Duration::ZERO);
    }
    {
        let _env = EnvVarGuard::set(PREPARE_TTL_ENV_VAR, "not-a-number");
        assert_eq!(prepare_ttl(), DEFAULT_PREPARE_TTL);
    }
    assert_eq!(prepare_ttl(), DEFAULT_PREPARE_TTL);
}

// -----------------------------------------------------------------
// Regression guard: DylintToolchainPlan::apply_to_command must only
// stamp the dylint-scoped identity env vars (plus the best-effort
// DYLINT_DRIVER_PATH) and must NEVER switch the analyzed
// workspace's cargo build profile. A sibling repo shipped exactly
// this bug once — soldr injected a profile override inside a
// dylint run and silently changed what got built/analyzed.
// -----------------------------------------------------------------
#[test]
fn apply_to_command_never_touches_build_profile_or_injects_args() {
    let plan = sample_plan();
    let mut command = std::process::Command::new("does-not-matter");
    plan.apply_to_command(&mut command);

    let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = command.get_envs().collect();

    let expected_keys = [
        "RUSTUP_TOOLCHAIN",
        TOOLCHAIN_ENV_VAR,
        COMPILER_RELEASE_ENV_VAR,
        COMPILER_COMMIT_ENV_VAR,
        CACHE_IDENTITY_ENV_VAR,
        PREPARED_IDENTITY_ENV_VAR,
    ];
    for key in expected_keys {
        assert!(
            envs.contains_key(OsStr::new(key)),
            "apply_to_command must set {key}"
        );
    }

    for key in envs.keys() {
        let key_str = key.to_string_lossy();
        assert!(
            !key_str.starts_with("CARGO_PROFILE_RELEASE_")
                && !key_str.starts_with("CARGO_BUILD_")
                && key_str != "PROFILE",
            "dylint scope stamping must never switch the analyzed workspace's \
                 build profile, but set: {key_str}"
        );
        // DYLINT_DRIVER_PATH and RUSTUP_HOME are the soldr-owned
        // additions beyond the identity env vars (both best-effort; absent
        // if SoldrPaths::new() / the Dylint home can't resolve here).
        // RUSTUP_HOME pins the home the nightly was provisioned in
        // (soldr#3567); `dylint_rustup_home_tests` asserts its value.
        assert!(
            expected_keys.contains(&key_str.as_ref())
                || key_str == "DYLINT_DRIVER_PATH"
                || key_str == "RUSTUP_HOME",
            "unexpected env var set by DylintToolchainPlan::apply_to_command: {key_str}"
        );
    }

    // A profile switch could also arrive as an injected CLI arg
    // (`--release` / `--profile <name>`); apply_to_command must
    // never add args to the command at all.
    assert_eq!(
        command.get_args().count(),
        0,
        "apply_to_command must not inject any CLI args (e.g. --release/--profile)"
    );
}

// -----------------------------------------------------------------
// soldr#2945 — channel precedence and the driver-gate diagnostic.
// -----------------------------------------------------------------

/// A workspace shaped like this repo: a stable root pin, and lint libraries
/// declared through a glob that pin a nightly.
fn library_workspace(library_channel: &str) -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("workspace tempdir");
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers=[]\n[workspace.metadata.dylint]\nlibraries=[{path='dylints/*'}]\n",
    )
    .expect("write workspace manifest");
    std::fs::write(
        temp.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel='1.95.0'\n",
    )
    .expect("write root manifest");
    let lint = temp.path().join("dylints").join("ban_something");
    std::fs::create_dir_all(&lint).expect("create lint dir");
    std::fs::write(
        lint.join("rust-toolchain.toml"),
        format!("[toolchain]\nchannel='{library_channel}'\n"),
    )
    .expect("write lint manifest");
    temp
}

fn inherited_library_workspace(root_channel: Option<&str>) -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("workspace tempdir");
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers=[]\n[workspace.metadata.dylint]\nlibraries=[{path='dylints/inherited'}]\n",
    )
    .expect("write workspace manifest");
    std::fs::create_dir_all(temp.path().join("dylints/inherited")).expect("create lint dir");
    if let Some(channel) = root_channel {
        std::fs::write(
            temp.path().join("rust-toolchain.toml"),
            format!("[toolchain]\nchannel='{channel}'\n"),
        )
        .expect("write root manifest");
    }
    temp
}

/// The defect: this workspace's lint libraries pin `nightly-2026-05-28`, but
/// the resolver read the *root* `1.95.0` and derived a nightly nobody has ever
/// published a driver for. Libraries now sit above the root manifest, and both
/// the explicit argument and the environment still sit above them.
#[test]
fn channel_precedence_is_explicit_then_environment_then_libraries_then_root() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let _retained = EnvVarGuard::unset(TOOLCHAIN_ENV_VAR);
    let _configured = EnvVarGuard::unset(CONFIGURED_TOOLCHAIN_ENV_VAR);
    let _rustup = EnvVarGuard::unset("RUSTUP_TOOLCHAIN");
    let workspace = library_workspace("nightly-2026-05-28");

    let requested =
        requested_toolchain_channel(None, workspace.path()).expect("resolve from libraries");
    assert_eq!(requested.channel.as_deref(), Some("nightly-2026-05-28"));
    assert_eq!(requested.provenance, ChannelProvenance::LintLibraries);

    let _env = EnvVarGuard::set(TOOLCHAIN_ENV_VAR, "nightly-2026-01-18");
    let requested =
        requested_toolchain_channel(None, workspace.path()).expect("resolve from environment");
    assert_eq!(requested.channel.as_deref(), Some("nightly-2026-01-18"));
    assert_eq!(requested.provenance, ChannelProvenance::Environment);

    let requested = requested_toolchain_channel(Some("nightly-2026-02-02"), workspace.path())
        .expect("resolve from the explicit argument");
    assert_eq!(requested.channel.as_deref(), Some("nightly-2026-02-02"));
    assert_eq!(requested.provenance, ChannelProvenance::Explicit);
}

#[test]
fn all_inherit_accepts_a_dated_nightly_root_with_root_provenance() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let _retained = EnvVarGuard::unset(TOOLCHAIN_ENV_VAR);
    let _configured = EnvVarGuard::unset(CONFIGURED_TOOLCHAIN_ENV_VAR);
    let _rustup = EnvVarGuard::unset("RUSTUP_TOOLCHAIN");
    let workspace = inherited_library_workspace(Some("nightly-2026-05-28"));

    let requested = requested_toolchain_channel(None, workspace.path())
        .expect("dated nightly inherited from the root must resolve");
    assert_eq!(requested.channel.as_deref(), Some("nightly-2026-05-28"));
    assert_eq!(requested.provenance, ChannelProvenance::RootManifest);
}

#[test]
fn all_inherit_rejects_root_channels_without_a_publishable_driver() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let _retained = EnvVarGuard::unset(TOOLCHAIN_ENV_VAR);
    let _configured = EnvVarGuard::unset(CONFIGURED_TOOLCHAIN_ENV_VAR);
    let _rustup = EnvVarGuard::unset("RUSTUP_TOOLCHAIN");

    for channel in ["1.95.0", "nightly"] {
        let workspace = inherited_library_workspace(Some(channel));
        let error = match requested_toolchain_channel(None, workspace.path()) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("an inherited non-dated-nightly cannot have a driver"),
        };
        assert!(error.contains("dylints/inherited"), "{error}");
        assert!(error.contains(channel), "{error}");
        assert!(
            error.contains("published only for dated nightly"),
            "{error}"
        );
        assert!(error.contains("impossible driver"), "{error}");
    }
}

#[test]
fn all_inherit_rejects_a_missing_root_channel_before_resolution() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let _retained = EnvVarGuard::unset(TOOLCHAIN_ENV_VAR);
    let _configured = EnvVarGuard::unset(CONFIGURED_TOOLCHAIN_ENV_VAR);
    let _rustup = EnvVarGuard::unset("RUSTUP_TOOLCHAIN");
    let workspace = inherited_library_workspace(None);

    let error = match requested_toolchain_channel(None, workspace.path()) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("an inherited missing root channel cannot have a driver"),
    };
    assert!(error.contains("dylints/inherited"), "{error}");
    assert!(error.contains("has no [toolchain].channel"), "{error}");
    assert!(
        error.contains("published only for dated nightly"),
        "{error}"
    );
}

#[test]
fn explicit_and_environment_channels_precede_invalid_inherited_roots() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let _retained = EnvVarGuard::unset(TOOLCHAIN_ENV_VAR);
    let _configured = EnvVarGuard::unset(CONFIGURED_TOOLCHAIN_ENV_VAR);
    let _rustup = EnvVarGuard::unset("RUSTUP_TOOLCHAIN");
    let workspace = inherited_library_workspace(Some("1.95.0"));

    let _env = EnvVarGuard::set(TOOLCHAIN_ENV_VAR, "nightly-2026-01-18");
    let requested = requested_toolchain_channel(None, workspace.path())
        .expect("environment must precede library inheritance validation");
    assert_eq!(requested.channel.as_deref(), Some("nightly-2026-01-18"));
    assert_eq!(requested.provenance, ChannelProvenance::Environment);

    let requested = requested_toolchain_channel(Some("nightly-2026-02-02"), workspace.path())
        .expect("explicit argument must precede library inheritance validation");
    assert_eq!(requested.channel.as_deref(), Some("nightly-2026-02-02"));
    assert_eq!(requested.provenance, ChannelProvenance::Explicit);
}

/// Tiers 4 and 5 are not dead code — they are the whole answer for a workspace
/// with no lint libraries to read.
#[test]
fn a_workspace_without_lint_libraries_still_falls_back_to_root_then_map() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let _retained = EnvVarGuard::unset(TOOLCHAIN_ENV_VAR);
    let _configured = EnvVarGuard::unset(CONFIGURED_TOOLCHAIN_ENV_VAR);
    let _rustup = EnvVarGuard::unset("RUSTUP_TOOLCHAIN");

    let temp = tempfile::tempdir().expect("workspace tempdir");
    std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n")
        .expect("write workspace manifest");
    assert_eq!(
        crate::dylint_libraries::toolchain_state(temp.path()).expect("read pins"),
        crate::dylint_libraries::LibraryToolchainState::NoLibraries,
        "a workspace with no lint libraries must not claim authority"
    );

    std::fs::write(
        temp.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel='1.95.0'\n",
    )
    .expect("write root manifest");
    let requested =
        requested_toolchain_channel(None, temp.path()).expect("resolve from the root manifest");
    assert_eq!(requested.channel.as_deref(), Some("1.95.0"));
    assert_eq!(requested.provenance, ChannelProvenance::RootManifest);

    std::fs::remove_file(temp.path().join("rust-toolchain.toml")).expect("remove root manifest");
    let requested =
        requested_toolchain_channel(None, temp.path()).expect("resolve with nothing pinned");
    assert_eq!(requested.channel, None);
    assert_eq!(requested.provenance, ChannelProvenance::VersionMap);
}

/// The old text said "Dylint v6.0.3 is not built for this machine" and told
/// the reader to pick a Dylint version with prebuilts for their host. Dylint
/// 6.0.3 ships a driver for every supported triple; the nightly is what was
/// wrong. The replacement has to say so, and say who chose the nightly.
#[test]
fn driver_diagnostic_names_the_provenance_and_the_missing_asset() {
    let plan = sample_plan().with_provenance(ChannelProvenance::LintLibraries);
    let driver_dir = Path::new("/soldr/dylint/drivers/nightly-2026-01-18-host");
    let reason = "no driver binary at that path";
    let message = unavailable_driver_error(&plan, driver_dir, reason).to_string();

    assert!(
        message.contains("no usable Dylint driver for nightly-2026-01-18"),
        "{message}"
    );
    assert!(
        message.contains("workspace.metadata.dylint.libraries"),
        "the diagnostic must name the tier that chose the channel: {message}"
    );
    assert!(
        message.contains("dylint-driver 6.2.0-nightly-2026-01-18"),
        "{message}"
    );
    assert!(message.contains("/soldr/dylint/drivers/"), "{message}");
    assert!(message.contains("DYLINT_DRIVER_PATH"), "{message}");
    assert!(
        message.contains(crate::wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR),
        "{message}"
    );
    assert!(
        !message.contains("is not built for this machine"),
        "the driver gate must stop blaming the host: {message}"
    );
}

/// The opt-in is off by default (binary-or-exit, soldr#2432/#2484) and, when
/// on, must say loudly what it just allowed.
#[test]
fn the_driver_build_opt_in_is_off_by_default_and_warns_when_set() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    {
        let _env = EnvVarGuard::unset(crate::wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR);
        assert!(!crate::wrapper::allow_dylint_driver_build());
    }
    {
        let _env = EnvVarGuard::set(crate::wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR, "0");
        assert!(!crate::wrapper::allow_dylint_driver_build());
    }
    let _env = EnvVarGuard::set(crate::wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR, "1");
    assert!(crate::wrapper::allow_dylint_driver_build());

    let plan = sample_plan().with_provenance(ChannelProvenance::LintLibraries);
    let warning = driver_source_build_warning(
        &plan,
        "6.2.0",
        &SoldrError::Other("catalogue has no asset row".into()),
    );
    assert!(warning.contains("WARNING"), "{warning}");
    assert!(warning.contains("nightly-2026-01-18"), "{warning}");
    assert!(
        warning.contains("dylint-driver 6.2.0-nightly-2026-01-18"),
        "{warning}"
    );
    assert!(
        warning.contains("rustc-dev"),
        "the warning must state the real cost of a driver source build: {warning}"
    );
    assert!(
        warning.contains(crate::wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR),
        "{warning}"
    );
}

/// soldr#3051: the readiness probe must read the home the install wrote to.
/// With no caller `RUSTUP_HOME` and a managed home on disk, `rustup toolchain
/// install` runs under the managed home; the probe used to read `~/.rustup`
/// instead and report a successful install as never having happened.
#[test]
fn dylint_readiness_reads_the_home_the_install_writes_to() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("temp soldr root");
    let managed = root.path().join("rustup");
    std::fs::create_dir_all(&managed).expect("managed rustup home");
    let _root = EnvVarGuard::set(crate::core::SOLDR_CACHE_DIR_ENV_VAR, root.path());
    let _home = EnvVarGuard::unset(crate::core::RUSTUP_HOME_ENV_VAR);

    let mut install = std::process::Command::new("rustup");
    crate::apply_implicit_toolchain_homes(&mut install);
    let install_home = install
        .get_envs()
        .find_map(|(name, value)| (name == crate::core::RUSTUP_HOME_ENV_VAR).then_some(value))
        .flatten()
        .map(PathBuf::from);

    assert_eq!(install_home.as_deref(), Some(managed.as_path()));
    assert_eq!(
        crate::dylint_rustup_home::dylint_rustup_home().expect("Dylint home"),
        managed
    );
}
