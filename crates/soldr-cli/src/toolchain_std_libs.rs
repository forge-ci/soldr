//! Target standard-library files as toolchain readiness evidence (soldr#3376).
//!
//! rustup's `lib/rustlib/components` manifest is a *claim* that
//! `rust-std-<target>` is installed. When the files under
//! `lib/rustlib/<target>/lib` are gone (a poisoned cache, an extraction killed
//! midway, a manual deletion) the claim stays, rustup reports "up to date", and
//! the build fails with `E0463: can't find crate for core/std`. These checks
//! read the files themselves -- `stat` calls only, no process -- so they are
//! cheap enough for the memo-hit path, and they drive both the memo identity and
//! the repair.

use crate::core::SoldrError;
use std::path::{Path, PathBuf};

/// `lib/rustlib/<triple>/lib/<stem>-<hash>.rlib`, if any.
fn rlib(toolchain_dir: &Path, triple: &str, stem: &str) -> Option<PathBuf> {
    let lib_dir = toolchain_dir
        .join("lib")
        .join("rustlib")
        .join(triple)
        .join("lib");
    let prefix = format!("{stem}-");
    std::fs::read_dir(lib_dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&prefix) && name.ends_with(".rlib")
        })
        .map(|entry| entry.path())
}

/// Every triple that must have `libcore`; the host also needs `libstd`.
fn required(host: Option<&str>, targets: &[String]) -> Vec<(String, bool)> {
    let mut triples: Vec<(String, bool)> = Vec::new();
    if let Some(host) = host {
        triples.push((host.to_string(), true));
    }
    for target in targets {
        if !triples.iter().any(|(triple, _)| triple == target) {
            triples.push((target.clone(), false));
        }
    }
    triples
}

/// The triples whose required library files are absent from `toolchain_dir`.
pub(crate) fn missing_std_triples(
    toolchain_dir: &Path,
    host: Option<&str>,
    targets: &[String],
) -> Vec<String> {
    required(host, targets)
        .into_iter()
        .filter(|(triple, needs_std)| {
            rlib(toolchain_dir, triple, "libcore").is_none()
                || (*needs_std && rlib(toolchain_dir, triple, "libstd").is_none())
        })
        .map(|(triple, _)| triple)
        .collect()
}

/// `(triple, libcore length)` for every required triple, or `None` when any
/// required library is missing -- so a deleted library is a memo miss rather
/// than a false hit.
pub(crate) fn std_lib_fingerprint(
    toolchain_dir: &Path,
    host: Option<&str>,
    targets: &[String],
) -> Option<Vec<(String, u64)>> {
    if !missing_std_triples(toolchain_dir, host, targets).is_empty() {
        return None;
    }
    required(host, targets)
        .into_iter()
        .map(|(triple, _)| {
            let core = rlib(toolchain_dir, &triple, "libcore")?;
            let len = std::fs::metadata(core).ok()?.len();
            Some((triple, len))
        })
        .collect()
}

/// The host triple when `toolchain_dir` is named for it (`<channel>-<host>`).
/// A toolchain installed for another host is not judged against this one.
pub(crate) fn host_of_toolchain_dir(toolchain_dir: &Path, host: &str) -> Option<String> {
    let name = toolchain_dir.file_name()?.to_str()?;
    name.ends_with(host).then(|| host.to_string())
}

/// Restore missing standard-library files.
///
/// A Soldr-managed home is repaired by `repair` (remove then re-add the
/// `rust-std` component); a caller-selected `RUSTUP_HOME` is never mutated and
/// fails closed with the directory and the recovery commands (soldr#2977).
pub(crate) fn repair_missing_std_libs(
    channel: &str,
    toolchain_dir: &Path,
    host: Option<&str>,
    targets: &[String],
    caller_selected_home: bool,
    repair: &mut dyn FnMut(&str) -> Result<(), SoldrError>,
) -> Result<(), SoldrError> {
    let missing = missing_std_triples(toolchain_dir, host, targets);
    if missing.is_empty() {
        return Ok(());
    }
    if caller_selected_home {
        return Err(SoldrError::Other(missing_std_guidance(
            channel,
            toolchain_dir,
            &missing,
        )));
    }
    for triple in &missing {
        if !crate::core::quiet::diagnostics_suppressed() {
            eprintln!(
                "soldr: toolchain {channel} lists rust-std-{triple} but its library files are \
                 missing; reinstalling that component (soldr#3376)"
            );
        }
        repair(triple)?;
    }
    let still_missing = missing_std_triples(toolchain_dir, host, targets);
    if still_missing.is_empty() {
        Ok(())
    } else {
        Err(SoldrError::Other(format!(
            "toolchain {channel} is still missing standard-library files for {} after \
             reinstalling rust-std under {}",
            still_missing.join(", "),
            toolchain_dir.display()
        )))
    }
}

fn missing_std_guidance(channel: &str, toolchain_dir: &Path, missing: &[String]) -> String {
    let manager = ["rust", "up"].concat();
    let commands = missing
        .iter()
        .map(|triple| {
            format!(
                "`soldr {manager} component remove rust-std-{triple} --toolchain {channel}` \
                 then `soldr {manager} target add {triple} --toolchain {channel}`"
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Rust toolchain {channel} at {} lists rust-std for {} but the library files are \
         missing (builds would fail with E0463). This is caller-selected RUSTUP_HOME state, \
         so Soldr will not modify it automatically. Run {commands}.",
        toolchain_dir.display(),
        missing.join(", ")
    )
}

/// The installed toolchain directory for `channel` under `rustup_home`:
/// `<channel>-<host>` first, then a bare `<channel>` (custom/linked names).
pub(crate) fn installed_toolchain_dir(
    rustup_home: &Path,
    channel: &str,
    host: &str,
) -> Option<PathBuf> {
    let toolchains = rustup_home.join("toolchains");
    [format!("{channel}-{host}"), channel.to_string()]
        .into_iter()
        .map(|name| toolchains.join(name))
        .find(|dir| dir.is_dir())
}

/// soldr#3793: after an ad hoc `rustup target add <triple>` (a `--target`
/// that no manifest declared), rustup may answer "up to date" over a
/// `rust-std` whose files are gone. Check the files, and repair or fail
/// closed exactly as the declared-target path does.
pub(crate) fn verify_added_target_std(
    rustup_home: &Path,
    channel: &str,
    triple: &str,
    caller_selected_home: bool,
    repair: &mut dyn FnMut(&str) -> Result<(), SoldrError>,
) -> Result<(), SoldrError> {
    let host = crate::pyo3_detect::host_triple();
    let Some(toolchain_dir) = installed_toolchain_dir(rustup_home, channel, host) else {
        // No directory to judge; cargo reports the real state.
        return Ok(());
    };
    repair_missing_std_libs(
        channel,
        &toolchain_dir,
        None,
        &[triple.to_string()],
        caller_selected_home,
        repair,
    )
}

/// Reinstall `rust-std-<triple>` for `channel` in the Soldr-managed home.
pub(crate) fn reinstall_rust_std(channel: &str, triple: &str) -> Result<(), SoldrError> {
    crate::core::forbid_toolchain_install_tripwire(&format!(
        "rustup target add {triple} --toolchain {channel}"
    ))?;
    let mut remove = std::process::Command::new(crate::rustup_binary());
    remove.args([
        "component",
        "remove",
        "--toolchain",
        channel,
        &format!("rust-std-{triple}"),
    ]);
    crate::apply_implicit_toolchain_homes(&mut remove);
    // A failed removal is not fatal: the component may already be gone.
    let _ = crate::toolchain::run_toolchain_command(
        &mut remove,
        &format!("rustup component remove rust-std-{triple} --toolchain {channel}"),
    )?;
    let code = crate::toolchain::rustup_target_add(channel, triple)?;
    if code == 0 {
        Ok(())
    } else {
        Err(SoldrError::Other(format!(
            "target add {triple} exited with code {code}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toolchain_with(libs: &[(&str, &[&str])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (triple, files) in libs {
            let lib = dir.path().join("lib/rustlib").join(triple).join("lib");
            std::fs::create_dir_all(&lib).unwrap();
            for file in *files {
                std::fs::write(lib.join(file), b"rlib").unwrap();
            }
        }
        dir
    }

    const HOST: &str = "x86_64-unknown-linux-gnu";
    const WASM: &str = "wasm32-unknown-unknown";

    #[test]
    fn host_needs_core_and_std_and_targets_need_core() {
        let dir = toolchain_with(&[
            (HOST, &["libcore-1.rlib", "libstd-1.rlib"]),
            (WASM, &["libcore-2.rlib"]),
        ]);
        let targets = vec![WASM.to_string()];
        assert!(missing_std_triples(dir.path(), Some(HOST), &targets).is_empty());
        assert!(std_lib_fingerprint(dir.path(), Some(HOST), &targets).is_some());
    }

    #[test]
    fn a_deleted_library_is_missing_and_defeats_the_fingerprint() {
        let dir = toolchain_with(&[(HOST, &["libcore-1.rlib"]), (WASM, &[])]);
        let targets = vec![WASM.to_string()];
        assert_eq!(
            missing_std_triples(dir.path(), Some(HOST), &targets),
            vec![HOST.to_string(), WASM.to_string()]
        );
        assert!(std_lib_fingerprint(dir.path(), Some(HOST), &targets).is_none());
    }

    #[test]
    fn a_toolchain_for_another_host_is_not_judged_against_this_one() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("1.98.1-aarch64-apple-darwin");
        assert_eq!(host_of_toolchain_dir(&named, HOST), None);
        let matching = dir.path().join(format!("1.98.1-{HOST}"));
        assert_eq!(
            host_of_toolchain_dir(&matching, HOST).as_deref(),
            Some(HOST)
        );
    }

    #[test]
    fn a_managed_home_is_repaired_and_a_caller_home_fails_closed() {
        let dir = toolchain_with(&[(HOST, &["libcore-1.rlib", "libstd-1.rlib"]), (WASM, &[])]);
        let targets = vec![WASM.to_string()];
        let lib = dir.path().join("lib/rustlib").join(WASM).join("lib");

        let mut called = Vec::new();
        let caller =
            repair_missing_std_libs("1.98.1", dir.path(), Some(HOST), &targets, true, &mut |t| {
                called.push(t.to_string());
                Ok(())
            })
            .unwrap_err()
            .to_string();
        assert!(
            called.is_empty(),
            "a caller-selected home must not be mutated"
        );
        assert!(
            caller.contains(WASM) && caller.contains("target add"),
            "{caller}"
        );

        repair_missing_std_libs(
            "1.98.1",
            dir.path(),
            Some(HOST),
            &targets,
            false,
            &mut |t| {
                called.push(t.to_string());
                std::fs::write(lib.join("libcore-9.rlib"), b"rlib").unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(called, vec![WASM.to_string()]);
    }

    #[test]
    fn an_ad_hoc_target_with_missing_files_is_repaired_or_fails_closed() {
        // soldr#3793: no declared targets; `target add` said "up to date".
        let home = tempfile::tempdir().unwrap();
        let host = crate::pyo3_detect::host_triple();
        let toolchain = home.path().join(format!("toolchains/1.98.1-{host}"));
        let lib = toolchain.join("lib/rustlib").join(WASM).join("lib");
        std::fs::create_dir_all(&lib).unwrap();

        let caller = verify_added_target_std(home.path(), "1.98.1", WASM, true, &mut |_| {
            panic!("a caller-selected home must not be mutated")
        })
        .unwrap_err()
        .to_string();
        assert!(
            caller.contains(WASM) && caller.contains("E0463"),
            "{caller}"
        );

        let mut called = Vec::new();
        verify_added_target_std(home.path(), "1.98.1", WASM, false, &mut |t| {
            called.push(t.to_string());
            std::fs::write(lib.join("libcore-9.rlib"), b"rlib").unwrap();
            Ok(())
        })
        .unwrap();
        assert_eq!(called, vec![WASM.to_string()]);

        // Files present: no repair.
        verify_added_target_std(home.path(), "1.98.1", WASM, false, &mut |_| {
            panic!("an intact target must not be reinstalled")
        })
        .unwrap();
    }

    #[test]
    fn an_unknown_toolchain_dir_is_not_judged() {
        let home = tempfile::tempdir().unwrap();
        verify_added_target_std(home.path(), "1.98.1", WASM, false, &mut |_| {
            panic!("nothing to judge")
        })
        .unwrap();
    }

    #[test]
    fn a_repair_that_restores_nothing_is_an_error() {
        let dir = toolchain_with(&[(WASM, &[])]);
        let targets = vec![WASM.to_string()];
        let error =
            repair_missing_std_libs("1.98.1", dir.path(), None, &targets, false, &mut |_| Ok(()))
                .unwrap_err()
                .to_string();
        assert!(error.contains("still missing"), "{error}");
    }
}
