//! Detect when a standalone (Chocolatey/scoop/manual) `cargo` shadows
//! rustup's proxy on PATH — issue #1059.
//!
//! ## What this catches
//!
//! When a Windows host has both a Chocolatey-installed standalone Rust
//! and a rustup-managed toolchain, `~/.cargo/bin/` may be missing a
//! `cargo.exe` proxy (rustup ships one but the package can be removed,
//! corrupted, or simply absent on a Chocolatey-first setup). In that
//! state, `PATH` resolves bare `cargo` to
//! `C:\ProgramData\chocolatey\bin\cargo.exe` — a standalone 1.94.1
//! cargo that IGNORES per-crate `rust-toolchain.toml` overrides.
//!
//! soldr's own bash/PowerShell hook refuses bare `cargo` from the user
//! and routes through `soldr cargo ...`. But it cannot catch
//! subprocess invocations from already-built tools (`cargo-dylint`,
//! `cargo-binstall`, `cargo-make`, etc.) that hard-code `"cargo"` and
//! find the Chocolatey shim themselves. The user-visible symptom is
//! a `cargo-dylint` subprocess failing with `E0554` /
//! `can't find crate for rustc_driver` because the nightly switch
//! from `dylints/X/rust-toolchain.toml` never happened.
//!
//! This module:
//!
//!   * Resolves the first `cargo` on PATH.
//!   * Decides whether that path is a Chocolatey/scoop/standalone shim
//!     that doesn't honor `rust-toolchain.toml`.
//!   * Emits a structured [`CargoOnPathFinding`] consumed by
//!     `toolchain_doctor` (probe row) and `toolchain_ensure`
//!     (one-line warning).
//!
//! ## Why not "fix" PATH automatically
//!
//! The fix is genuinely user-side: either uninstall the standalone
//! Rust (`choco uninstall rust`) or prepend `~/.cargo/bin` to PATH so
//! the rustup proxy wins. soldr can't reorder system PATH safely. The
//! best soldr can do is *flag the problem clearly* + offer a
//! subprocess-safe escape hatch (`soldr exec <cmd>`).

use std::path::{Path, PathBuf};

/// A `cargo` binary found on PATH plus a classification of whether it
/// will honor `rust-toolchain.toml` overrides for subprocesses that
/// hard-code `"cargo"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoOnPathFinding {
    /// Absolute path that `which cargo` (or PowerShell `Get-Command
    /// cargo`) would resolve to today.
    pub resolved: PathBuf,
    /// `true` when the resolved path is a rustup proxy (or behaves
    /// equivalently — i.e. respects per-crate channel overrides).
    /// `false` when the resolved path is a standalone shim that
    /// ignores them.
    pub honors_rust_toolchain_toml: bool,
    /// Origin classification — drives the warning message and the
    /// machine-facing probe payload.
    pub classification: CargoClassification,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CargoClassification {
    /// Path matches a known Chocolatey install layout (e.g.
    /// `C:\ProgramData\chocolatey\bin\cargo.exe` or
    /// `C:\ProgramData\chocolatey\lib\rust\...\cargo.exe`).
    Chocolatey,
    /// Path matches a Scoop shim layout
    /// (`~/scoop/shims/cargo.exe` or `C:\scoop\shims\cargo.exe`).
    Scoop,
    /// Path matches a rustup-managed install (`~/.cargo/bin/cargo[.exe]`
    /// or `CARGO_HOME/bin/cargo[.exe]`). These ARE rustup proxies and
    /// honor per-crate channel overrides.
    Rustup,
    /// Path lives directly under a rustup toolchain dir
    /// (`~/.rustup/toolchains/<channel>/bin/cargo[.exe]`). Honors the
    /// channel that toolchain represents, but NOT per-crate overrides
    /// — flagged with `honors_rust_toolchain_toml = false`.
    RustupToolchainBin,
    /// Path is one of the soldr-installed shims
    /// (`<shim-dir>/cargo[.exe]`) — they re-exec into soldr so they
    /// inherit soldr's own channel-resolution logic.
    SoldrShim,
    /// Path matches a system package manager that ships a standalone
    /// cargo (apt, dnf, etc.). On Linux/macOS this is usually fine
    /// because the same package supplies the rustc/cargo pair
    /// consistently, but it does not honor `rust-toolchain.toml`.
    SystemPackage,
    /// Path doesn't match any known layout. Conservatively assumed to
    /// NOT honor `rust-toolchain.toml` overrides — that's the
    /// failure mode users hit.
    Unknown,
}

impl CargoClassification {
    /// Human-readable label for the warning / probe details.
    pub fn label(&self) -> &'static str {
        match self {
            CargoClassification::Chocolatey => "chocolatey",
            CargoClassification::Scoop => "scoop",
            CargoClassification::Rustup => "rustup",
            CargoClassification::RustupToolchainBin => "rustup-toolchain-bin",
            CargoClassification::SoldrShim => "soldr-multicall-shim",
            CargoClassification::SystemPackage => "system-package",
            CargoClassification::Unknown => "unknown",
        }
    }
}

/// Probe PATH for the first `cargo` binary and classify it. Returns
/// `None` when no `cargo` is anywhere on PATH.
pub fn detect_cargo_on_path() -> Option<CargoOnPathFinding> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            &["", ".exe"]
        } else {
            &[""]
        };
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in exts {
            let candidate: PathBuf = if ext.is_empty() {
                dir.join("cargo")
            } else {
                dir.join(format!("cargo{ext}"))
            };
            if candidate.is_file() {
                return Some(classify_cargo_path(&candidate));
            }
        }
    }
    None
}

/// Pure-function classifier. Linux-testable by handing in a synthetic
/// path. Looks for substrings characteristic of each install layout —
/// the prefixes are stable enough that substring matching beats trying
/// to chase canonical paths on every platform.
pub fn classify_cargo_path(path: &Path) -> CargoOnPathFinding {
    let raw = path.to_string_lossy();
    // Normalize separators before substring checks so the same patterns
    // catch both `C:\ProgramData\chocolatey\...` and the bash-shell
    // form `/c/ProgramData/chocolatey/...`.
    let lower = raw.to_ascii_lowercase().replace('\\', "/");

    // Order matters: rustup paths typically also contain `.cargo/bin`
    // so we have to check rustup before the more general "standalone"
    // heuristics.
    let classification = if lower.contains("/.cargo/bin/") || lower.contains("/cargo/bin/") {
        // Rustup proxy lives under CARGO_HOME/bin (default `~/.cargo/bin`).
        CargoClassification::Rustup
    } else if lower.contains("/.rustup/toolchains/") || lower.contains("/rustup/toolchains/") {
        CargoClassification::RustupToolchainBin
    } else if lower.contains("/chocolatey/") {
        CargoClassification::Chocolatey
    } else if lower.contains("/scoop/") {
        CargoClassification::Scoop
    } else if (lower.contains("/.soldr/v") && lower.contains("/shims/"))
        || lower.contains("/soldr/bin/")
        || lower.contains("\\soldr\\bin\\")
    {
        CargoClassification::SoldrShim
    } else if lower.starts_with("/usr/") || lower.starts_with("/opt/") {
        CargoClassification::SystemPackage
    } else {
        CargoClassification::Unknown
    };

    let honors = matches!(
        classification,
        CargoClassification::Rustup | CargoClassification::SoldrShim
    );

    CargoOnPathFinding {
        resolved: path.to_path_buf(),
        honors_rust_toolchain_toml: honors,
        classification,
    }
}

/// Render the user-actionable warning for a shadowing finding. Returns
/// `None` when the finding does NOT warrant a warning (rustup or a
/// soldr-managed shim).
pub fn warning_for(finding: &CargoOnPathFinding) -> Option<String> {
    if finding.honors_rust_toolchain_toml {
        return None;
    }
    let label = finding.classification.label();
    let path = finding.resolved.display();
    Some(format!(
        "soldr: warning: `cargo` on PATH resolves to a {label} install at {path}\n\
         soldr: this binary does NOT honor per-crate `rust-toolchain.toml` overrides.\n\
         soldr: subprocesses launched by cargo extensions (cargo-dylint, cargo-binstall, ...)\n\
         soldr: will use the WRONG channel and may fail with E0554 / E0463.\n\
         soldr: see https://github.com/zackees/soldr/issues/1059 for the fix.\n\
         soldr: workarounds:\n\
         soldr:   (a) uninstall the standalone Rust (e.g. `choco uninstall rust`), OR\n\
         soldr:   (b) prepend `~/.cargo/bin` to PATH so the rustup proxy wins, OR\n\
         soldr:   (c) wrap subprocess-y commands in `soldr exec <command...>`."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_chocolatey_windows_path() {
        let p = PathBuf::from(r"C:\ProgramData\chocolatey\bin\cargo.exe");
        let f = classify_cargo_path(&p);
        assert_eq!(f.classification, CargoClassification::Chocolatey);
        assert!(!f.honors_rust_toolchain_toml);
        assert!(warning_for(&f).is_some());
    }

    #[test]
    fn classify_chocolatey_msys_form() {
        // The MSYS / git-for-windows bash form of the same path.
        let p = PathBuf::from("/c/ProgramData/chocolatey/bin/cargo");
        let f = classify_cargo_path(&p);
        assert_eq!(f.classification, CargoClassification::Chocolatey);
        assert!(!f.honors_rust_toolchain_toml);
    }

    #[test]
    fn classify_scoop_path() {
        let p = PathBuf::from(r"C:\Users\me\scoop\shims\cargo.exe");
        let f = classify_cargo_path(&p);
        assert_eq!(f.classification, CargoClassification::Scoop);
        assert!(!f.honors_rust_toolchain_toml);
    }

    #[test]
    fn classify_rustup_cargo_home() {
        let p = PathBuf::from(r"C:\Users\me\.cargo\bin\cargo.exe");
        let f = classify_cargo_path(&p);
        assert_eq!(f.classification, CargoClassification::Rustup);
        assert!(f.honors_rust_toolchain_toml);
        assert!(warning_for(&f).is_none());
    }

    #[test]
    fn classify_rustup_cargo_home_linux() {
        let p = PathBuf::from("/home/me/.cargo/bin/cargo");
        let f = classify_cargo_path(&p);
        assert_eq!(f.classification, CargoClassification::Rustup);
        assert!(f.honors_rust_toolchain_toml);
    }

    #[test]
    fn classify_soldr_versioned_multicall_shim() {
        let p = PathBuf::from("/home/me/.soldr/v0.8.0/shims/cargo");
        let f = classify_cargo_path(&p);
        assert_eq!(f.classification, CargoClassification::SoldrShim);
        assert!(f.honors_rust_toolchain_toml);
        assert!(warning_for(&f).is_none());
    }

    #[test]
    fn classify_rustup_toolchain_bin_is_warned() {
        // Direct toolchain bin path — pinned to ONE channel, ignores
        // per-crate overrides. Warn.
        let p =
            PathBuf::from("/home/me/.rustup/toolchains/1.94.1-x86_64-pc-windows-msvc/bin/cargo");
        let f = classify_cargo_path(&p);
        assert_eq!(f.classification, CargoClassification::RustupToolchainBin);
        assert!(!f.honors_rust_toolchain_toml);
    }

    #[test]
    fn classify_system_package_linux() {
        let p = PathBuf::from("/usr/bin/cargo");
        let f = classify_cargo_path(&p);
        assert_eq!(f.classification, CargoClassification::SystemPackage);
        assert!(!f.honors_rust_toolchain_toml);
    }

    #[test]
    fn classify_unknown_falls_through() {
        let p = PathBuf::from("/tmp/random/cargo");
        let f = classify_cargo_path(&p);
        assert_eq!(f.classification, CargoClassification::Unknown);
        assert!(!f.honors_rust_toolchain_toml);
        assert!(warning_for(&f).is_some());
    }

    #[test]
    fn detect_cargo_on_path_respects_synth_env() {
        // Build an isolated PATH and a fake cargo binary; verify the
        // detector finds it. Restore PATH at the end so we don't
        // pollute downstream tests.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bin = tmp.path().join("scoop").join("shims");
        std::fs::create_dir_all(&bin).unwrap();
        let exe_name = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            "cargo.exe"
        } else {
            "cargo"
        };
        let exe = bin.join(exe_name);
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        crate::platform::fs::permissions::make_executable(&exe).unwrap();
        // soldr#1994: PATH is mutated here and in soldr-cli. Two barriers
        // over one variable are no barrier, and this test previously had
        // none at all -- so take the one process-wide lock, which lives in
        // this crate precisely so upstream and downstream can share it.
        let _env = crate::test_util::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior = std::env::var_os("PATH");
        std::env::set_var("PATH", &bin);
        let found = detect_cargo_on_path();
        match prior {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        let found = found.expect("detector should find the fake cargo");
        assert_eq!(found.classification, CargoClassification::Scoop);
    }
}
