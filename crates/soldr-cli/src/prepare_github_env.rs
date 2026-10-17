//! GitHub Actions environment-file export for `soldr prepare`.

use std::path::Path;

use crate::core::SoldrError;

pub(crate) fn append_env(path: Option<&Path>, key: &str, value: &str) -> Result<(), SoldrError> {
    if let Some(path) = path {
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| SoldrError::Other(format!("open {}: {error}", path.display())))?;
        writeln!(file, "{key}={value}")
            .map_err(|error| SoldrError::Other(format!("write {}: {error}", path.display())))?;
    }
    Ok(())
}

/// The complete prepared environment as ordered `(key, value)` pairs —
/// the ONE computation both `soldr env` and `soldr prepare --github-env`
/// project (soldr#2304). Before this, `soldr env` had a second, divergent
/// env (a hardcoded linker guess that contradicted the blessed prep) and
/// the GitHub path was the only complete one.
///
/// Side effect, deliberately kept: resolved env vars and the merged PATH
/// are `set_var` into this process as they are computed, because
/// `encoded_rustflags_for_prep` merges from the process environment and
/// callers (prepare, build) rely on the applied state. Both surfaces are
/// short-lived CLI verbs, so the mutation is process-local by design.
pub(crate) fn exported_env_pairs(
    prep: &crate::blessed_build::BlessedPrep,
    target_triple: &str,
) -> Result<Vec<(String, String)>, SoldrError> {
    let mut pairs = Vec::new();
    for (key, value) in crate::target_lifecycle::resolved_env(prep) {
        std::env::set_var(&key, &value);
        pairs.push((key, value));
    }
    // Cross-target aliases must stay scoped: Cargo also builds host-only build
    // scripts, and a global CC would make those use the target compiler. Native
    // preparation can still expose the conventional aliases to external tools.
    if target_triple.eq_ignore_ascii_case(crate::pyo3_detect::host_triple()) {
        for (source, alias) in [
            ("CMAKE_C_COMPILER", "CC"),
            ("CMAKE_CXX_COMPILER", "CXX"),
            ("CMAKE_AR", "AR"),
            ("CMAKE_RANLIB", "RANLIB"),
        ] {
            if let Some((_, value)) = prep.env.iter().find(|(key, _)| key == source) {
                pairs.push((alias.to_string(), value.clone()));
            }
        }
    }
    if let Some(encoded) = crate::target_lifecycle::encoded_rustflags_for_prep(prep) {
        pairs.push(("CARGO_ENCODED_RUSTFLAGS".to_string(), encoded));
    }
    let mut path_dirs = prep.path_prefix();
    if !path_dirs.is_empty() {
        if let Some(current) = std::env::var_os("PATH") {
            path_dirs.extend(std::env::split_paths(&current));
        }
        let path_value = std::env::join_paths(path_dirs)
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| {
                SoldrError::Other(format!("failed to build prepared PATH: {error}"))
            })?;
        std::env::set_var("PATH", &path_value);
        pairs.push(("PATH".to_string(), path_value));
    }
    Ok(pairs)
}

/// Write the exported pairs to the GitHub env file (when given) and
/// return them. Returning the same list the file received is the
/// soldr#2304 guardrail: `soldr env` and this writer cannot drift
/// because there is exactly one computation.
pub(crate) fn apply_blessed_prep_env(
    github_env_path: Option<&Path>,
    prep: &crate::blessed_build::BlessedPrep,
    target_triple: &str,
) -> Result<Vec<(String, String)>, SoldrError> {
    let pairs = exported_env_pairs(prep, target_triple)?;
    for (key, value) in &pairs {
        append_env(github_env_path, key, value)?;
    }
    if !prep.cargo_args.is_empty() {
        eprintln!(
            "soldr prepare: note: target uses Cargo --config syslib overrides; \
             `soldr build` applies those automatically"
        );
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;

    /// soldr#2304 guardrail: the GitHub env file receives exactly the
    /// pairs `exported_env_pairs` computes — `soldr env` projects the
    /// same function, so the two surfaces cannot drift.
    #[test]
    fn github_env_file_matches_exported_pairs_exactly() {
        let _lock = ENV_LOCK.lock();
        let saved_path = std::env::var_os("PATH");

        let prep = crate::blessed_build::BlessedPrep {
            env: vec![
                (
                    "CMAKE_C_COMPILER".to_string(),
                    "/toolchain/bin/cc".to_string(),
                ),
                (
                    "CC_x86_64_unknown_linux_gnu".to_string(),
                    "/toolchain/bin/cc".to_string(),
                ),
            ],
            path_dirs: vec![std::path::PathBuf::from("/toolchain/bin")],
            ..Default::default()
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("github.env");
        let pairs =
            apply_blessed_prep_env(Some(&file), &prep, "made-up-cross-triple").expect("apply");
        let written = std::fs::read_to_string(&file).expect("read env file");
        let expected: String = pairs
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect();
        assert_eq!(written, expected);

        // Cross triple: no host-only aliases; PATH merged last.
        assert!(!pairs.iter().any(|(key, _)| key == "CC"));
        let (last_key, last_value) = pairs.last().expect("has PATH");
        assert_eq!(last_key, "PATH");
        assert!(last_value.contains("toolchain"));

        // Host triple: the conventional aliases appear for external tools.
        match &saved_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        let host_pairs =
            exported_env_pairs(&prep, crate::pyo3_detect::host_triple()).expect("pairs");
        assert!(host_pairs
            .iter()
            .any(|(key, value)| key == "CC" && value == "/toolchain/bin/cc"));

        match saved_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        std::env::remove_var("CMAKE_C_COMPILER");
        std::env::remove_var("CC_x86_64_unknown_linux_gnu");
    }
}
