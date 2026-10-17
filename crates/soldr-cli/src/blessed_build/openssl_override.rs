//! Target-scoped `openssl-sys` environment for the managed static OpenSSL
//! syslib (soldr#3246).
//!
//! `openssl-src` (the `vendored` feature) locates `nmake.exe` through the
//! Windows registry, so a `vendored` crate cannot build `*-pc-windows-msvc`
//! from a non-Windows host. soldr instead points `openssl-sys` at the
//! catalogue's static OpenSSL, using only hooks its build script already
//! reads. Verified against openssl-sys 0.9.117:
//!
//! * `build/main.rs:42-46` — `env(name)` reads `<PREFIX>_<name>` before
//!   `<name>`, where `PREFIX = TARGET.to_uppercase().replace('-', "_")`.
//!   Every key below uses that prefix, so the values reach only the target
//!   build and never a host build script, and no unscoped `OPENSSL_*` is set.
//! * `build/main.rs:48-58` — with `vendored`, `OPENSSL_NO_VENDOR` set to
//!   anything but `"0"` skips `openssl-src` and takes `find_normal`.
//! * `build/find_normal.rs:7-32` — `OPENSSL_DIR` supplies `lib/` (and
//!   `lib64/`) plus `include/`. pkg-config is only consulted when
//!   `OPENSSL_DIR` is unset, and `find_normal.rs:210-220` skips it
//!   entirely for `windows-msvc`, so `OPENSSL_DIR` is the lever there.
//! * `build/main.rs:504-510` — `OPENSSL_STATIC` other than `"0"` forces
//!   static linking; `main.rs:230-235` then links `libssl`/`libcrypto` on
//!   `windows-msvc`, the names the bundle ships.
//!
//! `PKG_CONFIG_PATH_<triple>` is also prepended off MSVC, like the other
//! syslibs, for C dependencies that find OpenSSL through pkg-config.
//!
//! Injecting `OPENSSL_NO_VENDOR` replaces a crate's own OpenSSL choice, so
//! it is gated the same way as the mimalloc and lzma overrides: only when
//! `openssl-sys` is the sole provider of `links = "openssl"` in the graph
//! (see [`super::links_provider`]).

use std::path::{Path, PathBuf};

use super::links_provider::{self, LinksProvider};
use super::BlessedPrep;
use crate::core::SoldrPaths;

/// The crate the managed OpenSSL environment is written for.
const OPENSSL_SYS_CRATE: &str = "openssl-sys";
const OPENSSL_LINKS: &str = "openssl";

/// `openssl-sys` inputs that mean the caller already chose an OpenSSL.
const CALLER_OWNED_SUFFIXES: [&str; 4] = [
    "OPENSSL_DIR",
    "OPENSSL_LIB_DIR",
    "OPENSSL_INCLUDE_DIR",
    "OPENSSL_NO_VENDOR",
];

pub(super) async fn inject(
    paths: &SoldrPaths,
    target_triple: &str,
    prep: &mut BlessedPrep,
    feature_args: &[String],
) {
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let provider =
        links_provider::resolve(&workspace_root, OPENSSL_LINKS, target_triple, feature_args);
    inject_for_provider(paths, target_triple, &provider, prep).await;
}

async fn inject_for_provider(
    paths: &SoldrPaths,
    target_triple: &str,
    provider: &LinksProvider,
    prep: &mut BlessedPrep,
) {
    if !provider_is_openssl_sys(target_triple, provider) {
        return;
    }
    if let Some(key) = caller_owned_openssl_key(target_triple) {
        if !crate::core::quiet::diagnostics_suppressed() {
            eprintln!(
                "soldr build: not exporting the managed OpenSSL for {target_triple}: \
                 `{key}` is already set"
            );
        }
        return;
    }
    match crate::fetch::openssl_sysroot::ensure_openssl_sysroot(paths, target_triple).await {
        Ok(sysroot) => add_openssl_env(prep, target_triple, &sysroot),
        Err(error) => super::log_sys_unavailable("openssl", target_triple, &error),
    }
}

fn provider_is_openssl_sys(target_triple: &str, provider: &LinksProvider) -> bool {
    let reason = match provider {
        LinksProvider::Package(name) if name == OPENSSL_SYS_CRATE => return true,
        // Nothing links OpenSSL: skip silently and fetch nothing.
        LinksProvider::Absent => return false,
        LinksProvider::Package(name) => {
            format!("`links = \"openssl\"` is provided by `{name}`, not `{OPENSSL_SYS_CRATE}`")
        }
        LinksProvider::Unknown(reason) => {
            format!("could not determine which crate provides `links = \"openssl\"` ({reason})")
        }
    };
    if !crate::core::quiet::diagnostics_suppressed() {
        eprintln!("soldr build: not exporting the managed OpenSSL for {target_triple}: {reason}");
    }
    false
}

/// The first `[<PREFIX>_]OPENSSL_{DIR,LIB_DIR,INCLUDE_DIR,NO_VENDOR}` the
/// caller set. An explicit choice wins over the managed syslib; a
/// target-scoped key soldr would write would otherwise shadow an unscoped
/// one the caller set.
fn caller_owned_openssl_key(target_triple: &str) -> Option<String> {
    let prefix = openssl_env_prefix(target_triple);
    CALLER_OWNED_SUFFIXES
        .iter()
        .flat_map(|suffix| [format!("{prefix}_{suffix}"), (*suffix).to_string()])
        .find(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
}

/// The prefix `openssl-sys` puts in front of every env input for
/// `target_triple`: `TARGET.to_uppercase().replace('-', "_")`
/// (openssl-sys 0.9.117 `build/main.rs:43`).
pub(crate) fn openssl_env_prefix(target_triple: &str) -> String {
    target_triple.to_uppercase().replace('-', "_")
}

fn add_openssl_env(prep: &mut BlessedPrep, target_triple: &str, sysroot: &Path) {
    let prefix = openssl_env_prefix(target_triple);
    prep.env.push((
        format!("{prefix}_OPENSSL_DIR"),
        sysroot.to_string_lossy().into_owned(),
    ));
    prep.env
        .push((format!("{prefix}_OPENSSL_NO_VENDOR"), "1".to_string()));
    prep.env
        .push((format!("{prefix}_OPENSSL_STATIC"), "1".to_string()));
    // openssl-sys never runs pkg-config for windows-msvc, and the MSVC
    // bundle's `.lib` archives are not pkg-config consumers either.
    if !target_triple.ends_with("-windows-msvc") {
        super::prepend_pkg_config_path_for_target(prep, target_triple, sysroot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::openssl_sysroot::{
        catalogue_slug_for, MANAGED_OPENSSL_VERSION, OPENSSL_TARGETS,
    };
    use crate::{CwdGuard, EnvVarGuard, TEST_PROCESS_ENV_LOCK};

    const MSVC: &str = "x86_64-pc-windows-msvc";
    const OPENSSL_SYS_GRAPH: &str = r#"{"name":"openssl","links":null},
        {"name":"openssl-sys","links":"openssl"}"#;

    fn metadata(packages: &str) -> Vec<u8> {
        format!("{{\"packages\":[{packages}]}}").into_bytes()
    }

    /// Seed a completed bundle so `ensure_openssl_sysroot` needs no network.
    fn seed_bundle(paths: &SoldrPaths, slug: &str) -> PathBuf {
        let install_root = paths
            .bin
            .join("syslib")
            .join("openssl")
            .join(MANAGED_OPENSSL_VERSION)
            .join(slug);
        let package = install_root.join("package");
        std::fs::create_dir_all(package.join("lib").join("pkgconfig")).expect("seed lib");
        std::fs::create_dir_all(package.join("include").join("openssl")).expect("seed include");
        std::fs::write(install_root.join(".complete"), "test bundle").expect("seed stamp");
        package
    }

    #[must_use]
    fn clear_openssl_inputs(prefixed: [&'static str; 4]) -> Vec<EnvVarGuard> {
        prefixed
            .into_iter()
            .chain(CALLER_OWNED_SUFFIXES)
            .map(EnvVarGuard::remove)
            .collect()
    }

    const MSVC_INPUTS: [&str; 4] = [
        "X86_64_PC_WINDOWS_MSVC_OPENSSL_DIR",
        "X86_64_PC_WINDOWS_MSVC_OPENSSL_LIB_DIR",
        "X86_64_PC_WINDOWS_MSVC_OPENSSL_INCLUDE_DIR",
        "X86_64_PC_WINDOWS_MSVC_OPENSSL_NO_VENDOR",
    ];

    #[test]
    fn env_names_match_openssl_sys_for_every_managed_shape() {
        // Literal expectations rather than a re-derivation of the prefix.
        let expected = [
            ("x86_64-pc-windows-msvc", "X86_64_PC_WINDOWS_MSVC", false),
            ("aarch64-pc-windows-msvc", "AARCH64_PC_WINDOWS_MSVC", false),
            ("x86_64-pc-windows-gnu", "X86_64_PC_WINDOWS_GNU", true),
            ("x86_64-apple-darwin", "X86_64_APPLE_DARWIN", true),
            ("aarch64-apple-darwin", "AARCH64_APPLE_DARWIN", true),
            ("x86_64-unknown-linux-gnu", "X86_64_UNKNOWN_LINUX_GNU", true),
            (
                "aarch64-unknown-linux-gnu",
                "AARCH64_UNKNOWN_LINUX_GNU",
                true,
            ),
            (
                "x86_64-unknown-linux-musl",
                "X86_64_UNKNOWN_LINUX_MUSL",
                true,
            ),
            (
                "aarch64-unknown-linux-musl",
                "AARCH64_UNKNOWN_LINUX_MUSL",
                true,
            ),
        ];
        assert_eq!(expected.len(), OPENSSL_TARGETS.len());
        let sysroot = Path::new("/soldr/syslib/openssl/package");
        for (triple, prefix, pkg_config) in expected {
            assert!(catalogue_slug_for(triple).is_some(), "{triple} is managed");
            assert_eq!(openssl_env_prefix(triple), prefix);

            let mut prep = BlessedPrep::default();
            add_openssl_env(&mut prep, triple, sysroot);
            let mut want = vec![
                (
                    format!("{prefix}_OPENSSL_DIR"),
                    sysroot.to_string_lossy().into_owned(),
                ),
                (format!("{prefix}_OPENSSL_NO_VENDOR"), "1".to_string()),
                (format!("{prefix}_OPENSSL_STATIC"), "1".to_string()),
            ];
            if pkg_config {
                want.push((
                    format!("PKG_CONFIG_PATH_{triple}"),
                    sysroot
                        .join("lib")
                        .join("pkgconfig")
                        .to_string_lossy()
                        .into_owned(),
                ));
            }
            assert_eq!(prep.env, want, "{triple}");
            assert!(
                !prep.env.iter().any(|(key, _)| key.starts_with("OPENSSL_")),
                "{triple}: unscoped OPENSSL_* would leak into host build scripts"
            );
        }
    }

    #[test]
    fn injects_only_when_openssl_sys_provides_links_openssl() {
        let _lock = TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
        let _inputs = clear_openssl_inputs(MSVC_INPUTS);
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let package = seed_bundle(&paths, "windows-x64");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");

        let cases = [
            ("openssl-sys provides it", OPENSSL_SYS_GRAPH, true),
            ("nothing links openssl", r#"{"name":"serde"}"#, false),
            (
                "a fork claims links = openssl",
                r#"{"name":"openssl-sys-fork","links":"openssl"}"#,
                false,
            ),
            (
                "boringssl uses its own links name",
                r#"{"name":"boring-sys","links":"boringssl"}"#,
                false,
            ),
            (
                "two claimants are ambiguous",
                r#"{"name":"openssl-sys","links":"openssl"},
                   {"name":"other-ssl-sys","links":"openssl"}"#,
                false,
            ),
        ];
        for (label, packages, expected) in cases {
            let provider =
                links_provider::provider_from_metadata_json(&metadata(packages), OPENSSL_LINKS);
            let mut prep = BlessedPrep::default();
            runtime.block_on(inject_for_provider(&paths, MSVC, &provider, &mut prep));
            if expected {
                assert_eq!(
                    prep.env,
                    vec![
                        (
                            "X86_64_PC_WINDOWS_MSVC_OPENSSL_DIR".to_string(),
                            package.to_string_lossy().into_owned()
                        ),
                        (
                            "X86_64_PC_WINDOWS_MSVC_OPENSSL_NO_VENDOR".to_string(),
                            "1".to_string()
                        ),
                        (
                            "X86_64_PC_WINDOWS_MSVC_OPENSSL_STATIC".to_string(),
                            "1".to_string()
                        ),
                    ],
                    "{label}"
                );
            } else {
                assert!(prep.env.is_empty(), "{label}: {:?}", prep.env);
            }
        }

        let unresolved = links_provider::provider_from_metadata_json(b"not json", OPENSSL_LINKS);
        let mut prep = BlessedPrep::default();
        runtime.block_on(inject_for_provider(&paths, MSVC, &unresolved, &mut prep));
        assert!(prep.env.is_empty(), "an unresolvable graph must not inject");
    }

    #[test]
    fn unavailable_sysroot_logs_and_continues() {
        let _lock = TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
        let _inputs = clear_openssl_inputs([
            "AARCH64_PC_WINDOWS_MSVC_OPENSSL_DIR",
            "AARCH64_PC_WINDOWS_MSVC_OPENSSL_LIB_DIR",
            "AARCH64_PC_WINDOWS_MSVC_OPENSSL_INCLUDE_DIR",
            "AARCH64_PC_WINDOWS_MSVC_OPENSSL_NO_VENDOR",
        ]);
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let provider = LinksProvider::Package(OPENSSL_SYS_CRATE.to_string());
        let runtime = tokio::runtime::Runtime::new().expect("runtime");

        // Not seeded, and the network is refused: the fetch fails.
        let mut prep = BlessedPrep::default();
        runtime.block_on(inject_for_provider(
            &paths,
            "aarch64-pc-windows-msvc",
            &provider,
            &mut prep,
        ));
        assert!(prep.env.is_empty(), "{:?}", prep.env);

        // Not a managed shape at all.
        runtime.block_on(inject_for_provider(
            &paths,
            "wasm32-unknown-unknown",
            &provider,
            &mut prep,
        ));
        assert!(prep.env.is_empty(), "{:?}", prep.env);
    }

    #[test]
    fn a_caller_chosen_openssl_wins() {
        let _lock = TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
        let _inputs = clear_openssl_inputs(MSVC_INPUTS);
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        seed_bundle(&paths, "windows-x64");
        let provider = LinksProvider::Package(OPENSSL_SYS_CRATE.to_string());
        let runtime = tokio::runtime::Runtime::new().expect("runtime");

        for key in ["OPENSSL_DIR", "X86_64_PC_WINDOWS_MSVC_OPENSSL_NO_VENDOR"] {
            let _caller = EnvVarGuard::set(key, "/opt/caller-openssl");
            let mut prep = BlessedPrep::default();
            runtime.block_on(inject_for_provider(&paths, MSVC, &provider, &mut prep));
            assert!(prep.env.is_empty(), "{key} must suppress: {:?}", prep.env);
        }
    }

    #[test]
    fn prepare_exports_managed_openssl_unless_legacy_vendored_sys_opt_out() {
        let _lock = TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let triple = "x86_64-unknown-linux-gnu";
        let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
        let _cmake = EnvVarGuard::set(super::super::USE_SYSTEM_CMAKE_ENV_VAR, "1");
        let _inputs = clear_openssl_inputs([
            "X86_64_UNKNOWN_LINUX_GNU_OPENSSL_DIR",
            "X86_64_UNKNOWN_LINUX_GNU_OPENSSL_LIB_DIR",
            "X86_64_UNKNOWN_LINUX_GNU_OPENSSL_INCLUDE_DIR",
            "X86_64_UNKNOWN_LINUX_GNU_OPENSSL_NO_VENDOR",
        ]);
        let tmp = tempfile::tempdir().expect("tmpdir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let _cwd = CwdGuard::enter(&workspace);
        // Fake `cargo metadata` for the graph `prepare` resolves from cwd.
        links_provider::prime_metadata_for_test(
            &std::env::current_dir().expect("cwd"),
            triple,
            &metadata(OPENSSL_SYS_GRAPH),
        );
        let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
        let package = seed_bundle(&paths, "linux-x64-gnu");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");

        let prep = {
            let _opt_out = EnvVarGuard::remove(super::super::USE_LEGACY_VENDORED_SYS_ENV_VAR);
            runtime
                .block_on(super::super::prepare(&paths, triple, &[]))
                .expect("prepare")
        };
        let value = |key: &str| {
            prep.env
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            value("X86_64_UNKNOWN_LINUX_GNU_OPENSSL_DIR").as_deref(),
            Some(&*package.to_string_lossy())
        );
        assert_eq!(
            value("X86_64_UNKNOWN_LINUX_GNU_OPENSSL_NO_VENDOR").as_deref(),
            Some("1")
        );
        assert_eq!(
            value("X86_64_UNKNOWN_LINUX_GNU_OPENSSL_STATIC").as_deref(),
            Some("1")
        );
        let pkg_config = value("PKG_CONFIG_PATH_x86_64-unknown-linux-gnu").expect("pkg-config");
        assert!(
            pkg_config.contains(&*package.join("lib").join("pkgconfig").to_string_lossy()),
            "{pkg_config}"
        );

        let opted_out = {
            let _opt_out = EnvVarGuard::set(super::super::USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
            runtime
                .block_on(super::super::prepare(&paths, triple, &[]))
                .expect("prepare")
        };
        assert!(
            !opted_out.env.iter().any(|(key, _)| key.contains("OPENSSL")),
            "SOLDR_USE_LEGACY_VENDORED_SYS must disable the managed OpenSSL: {:?}",
            opted_out.env
        );
    }
}
