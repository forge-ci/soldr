//! `soldr toolchain doctor [--json]` — env-detection probes that ship
//! the diagnostic intel `setup-soldr` used to compute in TypeScript.
//! Phase 4 of #407 (Wave 3.3 of zackees/soldr#514).
//!
//! Ports the env-detection halves of three setup-soldr modules:
//!   * `detect-musl-cc.ts` — locate `musl-gcc` / `musl-clang` so
//!     `*-unknown-linux-musl` cross-compiles don't silently link against
//!     host glibc.
//!   * `detect-shared-target-warning.ts` — flag a pre-populated cargo
//!     `target/` directory that will collide with rust-plan restore.
//!   * `diagnostics.ts` — env-detection helpers only (the GitHub
//!     Actions dump stays in setup-soldr).
//!
//! Each probe is a pure function over its inputs; the wrapping
//! [`run_toolchain_doctor`] orchestrates them and emits either a
//! human-readable summary or the stable `schema_version: 1` JSON
//! payload that setup-soldr#133 will consume.

use serde::Serialize;
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::core::{command_output_with_timeout, suppress_windows_console_window, SoldrError};

const SCHEMA_VERSION: u32 = 1;

/// Distinct from the top-level `soldr doctor` subcommand: this one is
/// namespaced under `toolchain` and performs pure env-detection probes
/// rather than the full system check.
pub(crate) fn run_toolchain_doctor(json: bool) -> Result<i32, SoldrError> {
    let started = Instant::now();
    let host = HostInfo::detect();
    let workspace = std::env::current_dir().map_err(SoldrError::from)?;

    let probes = vec![
        probe_musl_cc(&host),
        probe_shared_target_warning_for(
            &crate::core::cargo_target_dir::CargoTargetDirInputs::from_process(&workspace, &[]),
        ),
        probe_cargo_on_path_shadowing(),
        probe_rustlib_integrity(),
    ];

    let all_ok = probes.iter().all(|p| p.ok);
    let output = DoctorOutput {
        schema_version: SCHEMA_VERSION,
        host: host.clone(),
        probes,
        elapsed_ms: started.elapsed().as_millis(),
    };

    if json {
        emit_json(&output)?;
    } else {
        emit_human(&output);
    }

    Ok(if all_ok { 0 } else { 1 })
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostInfo {
    pub os: String,
    pub arch: String,
    pub libc: String,
}

impl HostInfo {
    pub(crate) fn detect() -> HostInfo {
        HostInfo {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            libc: detect_libc(),
        }
    }
}

fn detect_libc() -> String {
    // We can't introspect glibc-vs-musl at compile-time without a build
    // script; default to "gnu" for the standard linux-gnu host triple
    // and leave the musl-cc probe to determine whether musl tooling is
    // available.
    match crate::platform::host::facts::os() {
        crate::platform::host::facts::HostOs::Linux => "gnu".to_string(),
        // CLAUDE.md mandates MSVC on Windows; the GNU host is opt-in.
        crate::platform::host::facts::HostOs::Windows => "msvc".to_string(),
        crate::platform::host::facts::HostOs::MacOs => "darwin".to_string(),
    }
}

#[derive(Serialize, Debug)]
pub(crate) struct DoctorOutput {
    pub schema_version: u32,
    pub host: HostInfo,
    pub probes: Vec<ProbeResult>,
    pub elapsed_ms: u128,
}

#[derive(Serialize, Debug)]
pub(crate) struct ProbeResult {
    pub name: String,
    pub ok: bool,
    pub details: Value,
}

/// Probe name for `musl-gcc` / `musl-clang` detection (ports
/// `detect-musl-cc.ts`'s env-detection half).
pub(crate) const PROBE_MUSL_CC: &str = "musl-cc";
/// Probe name for the pre-populated-target warning (ports
/// `detect-shared-target-warning.ts`).
pub(crate) const PROBE_SHARED_TARGET_WARNING: &str = "shared-target-warning";
/// Probe name for the soldr#1059 Chocolatey-cargo-shadowing detector.
pub(crate) const PROBE_CARGO_ON_PATH_SHADOWING: &str = "cargo-on-path-shadowing";
/// Probe name for soldr#884 — detect a rustup toolchain whose component
/// metadata claims a target's rust-std is installed while the on-disk
/// `lib/rustlib/<target>/lib/` dir is missing or empty. Symptom is a
/// `can't find crate for core / std` error even though `rustup target
/// list --installed` reports the target as present.
pub(crate) const PROBE_RUSTLIB_INTEGRITY: &str = "rustlib-integrity";

/// Detect availability of a musl C compiler on PATH. On non-Linux
/// hosts the probe is skipped (ok=true, details=`{"skipped": "not-linux"}`)
/// since musl cross-compilers are only meaningful for
/// `*-unknown-linux-musl` targets.
pub(crate) fn probe_musl_cc(host: &HostInfo) -> ProbeResult {
    if host.os != "linux" {
        return ProbeResult {
            name: PROBE_MUSL_CC.to_string(),
            ok: true,
            details: json!({ "skipped": "not-linux" }),
        };
    }

    let candidates = ["musl-gcc", "musl-clang"];
    for cmd in candidates {
        if let Some(found) = find_on_path(cmd) {
            let version = read_version(&found).unwrap_or_default();
            return ProbeResult {
                name: PROBE_MUSL_CC.to_string(),
                ok: true,
                details: json!({
                    "musl_cc": found.display().to_string(),
                    "tool": cmd,
                    "version": version,
                }),
            };
        }
    }

    // No musl-cc found. Not all linux hosts need musl tooling, so we
    // return ok=true with a `found: false` detail rather than failing
    // the entire doctor run. setup-soldr's TS predecessor was similarly
    // permissive — it only emitted a warning at most.
    ProbeResult {
        name: PROBE_MUSL_CC.to_string(),
        ok: true,
        details: json!({
            "found": false,
            "searched": candidates,
        }),
    }
}

/// Probe whether the current workspace already contains a pre-populated
/// `target/` directory that would collide with `rust-plan restore`.
/// Mirrors the `.fingerprint/`-based detector in `rust_plan.rs` (added
/// in PR #508) and `detect-shared-target-warning.ts`.
pub(crate) fn probe_shared_target_warning(workspace: &Path) -> ProbeResult {
    probe_target_dir(workspace.join("target"))
}

/// Resolve the target dir the way Cargo would from `inputs` (soldr#3695):
/// `CARGO_TARGET_DIR`, cargo config, then the workspace root.
pub(crate) fn probe_shared_target_warning_for(
    inputs: &crate::core::cargo_target_dir::CargoTargetDirInputs,
) -> ProbeResult {
    let target_dir = crate::core::cargo_target_dir::resolve_cargo_target_dir(inputs)
        .unwrap_or_else(|| inputs.cwd.join("target"));
    probe_target_dir(target_dir)
}

fn probe_target_dir(target_dir: PathBuf) -> ProbeResult {
    if !target_dir.is_dir() {
        return ProbeResult {
            name: PROBE_SHARED_TARGET_WARNING.to_string(),
            ok: true,
            details: json!({
                "target_dir": target_dir.display().to_string(),
                "fingerprint_dirs_found": 0,
                "would_warn": false,
                "reason": "no-target-dir",
            }),
        };
    }

    let fingerprint_dirs_found = count_populated_fingerprint_dirs(&target_dir, 3);
    let would_warn = fingerprint_dirs_found > 0;
    // ok=true even when would_warn=true: the probe successfully made a
    // diagnosis. The caller decides whether the warning is a blocker.
    ProbeResult {
        name: PROBE_SHARED_TARGET_WARNING.to_string(),
        ok: true,
        details: json!({
            "target_dir": target_dir.display().to_string(),
            "fingerprint_dirs_found": fingerprint_dirs_found,
            "would_warn": would_warn,
        }),
    }
}

/// PATH lookup helper. Mirrors setup-soldr's `findOnPathSync`.
/// soldr#1059 — probe the first `cargo` on PATH and classify whether
/// it honors per-crate `rust-toolchain.toml` overrides. The probe
/// returns `ok = true` even when the resolved cargo is a shadowing
/// shim — the diagnosis itself succeeded; the *finding* (in
/// `details.honors_rust_toolchain_toml`) tells the caller whether to
/// act on it. Mirrors how `probe_shared_target_warning` already
/// reports `would_warn: true` with `ok: true`.
pub(crate) fn probe_cargo_on_path_shadowing() -> ProbeResult {
    let Some(finding) = crate::cargo_path_check::detect_cargo_on_path() else {
        return ProbeResult {
            name: PROBE_CARGO_ON_PATH_SHADOWING.to_string(),
            ok: true,
            details: json!({
                "cargo_on_path": null,
                "found": false,
            }),
        };
    };
    let warning = crate::cargo_path_check::warning_for(&finding);
    ProbeResult {
        name: PROBE_CARGO_ON_PATH_SHADOWING.to_string(),
        ok: true,
        details: json!({
            "cargo_on_path": finding.resolved.display().to_string(),
            "classification": finding.classification.label(),
            "honors_rust_toolchain_toml": finding.honors_rust_toolchain_toml,
            "would_warn": warning.is_some(),
        }),
    }
}

/// soldr#884 — cross-check `rustup target list --installed` against the
/// on-disk `${toolchain_root}/lib/rustlib/<target>/lib/` directory. When
/// rustup's component metadata claims a target's rust-std is installed
/// but the on-disk dir is missing or contains no `.rlib` files, cargo
/// builds targeting that triple fail with `error[E0463]: can't find
/// crate for core/std` even though `rustup target add <target>` reports
/// "info: component rust-std is up to date" and takes no action.
///
/// This probe surfaces the corruption plus the exact `rustup component
/// remove` + `rustup target add` commands that force-reinstall the
/// missing `rust-std`.
///
/// Behavior:
/// * `ok = false` when at least one installed target has a corrupt
///   rustlib dir. This is the only doctor probe that reports `ok=false`
///   as a real finding (the others report `ok=true` even when they
///   flag a would_warn condition) because a corrupt rustlib is a hard
///   build failure, not an advisory.
/// * `ok = true` with `details.skipped=<reason>` when the probe cannot
///   run (no rustc on PATH, no rustup, `rustup which rustc` failed,
///   etc.) — a probe that cannot execute is not a corruption finding.
pub(crate) fn probe_rustlib_integrity() -> ProbeResult {
    let rustc = match crate::binaries::resolve_toolchain_binary("rustc") {
        Ok(path) => path,
        Err(err) => {
            return ProbeResult {
                name: PROBE_RUSTLIB_INTEGRITY.to_string(),
                ok: true,
                details: json!({
                    "skipped": "no-rustc",
                    "reason": err.to_string(),
                }),
            };
        }
    };
    let toolchain_root = match query_rustc_sysroot(&rustc) {
        Ok(path) => path,
        Err(reason) => {
            return ProbeResult {
                name: PROBE_RUSTLIB_INTEGRITY.to_string(),
                ok: true,
                details: json!({
                    "skipped": "rustc-sysroot-failed",
                    "rustc": rustc.display().to_string(),
                    "reason": reason,
                }),
            };
        }
    };

    let installed = match rustup_installed_targets_active() {
        Ok(targets) => targets,
        Err(err) => {
            return ProbeResult {
                name: PROBE_RUSTLIB_INTEGRITY.to_string(),
                ok: true,
                details: json!({
                    "skipped": "rustup-list-failed",
                    "reason": err.to_string(),
                    "toolchain_root": toolchain_root.display().to_string(),
                }),
            };
        }
    };

    let rustlib_root = toolchain_root.join("lib").join("rustlib");
    let mut targets_report: Vec<Value> = Vec::with_capacity(installed.len());
    let mut corrupted: Vec<String> = Vec::new();
    for target in &installed {
        let lib_dir = rustlib_root.join(target).join("lib");
        let status = classify_rustlib_lib_dir(&lib_dir);
        let ok = status == RustlibLibDirStatus::Populated;
        if !ok {
            corrupted.push(target.clone());
        }
        targets_report.push(json!({
            "triple": target,
            "lib_dir": lib_dir.display().to_string(),
            "status": status.label(),
            "ok": ok,
        }));
    }

    let ok = corrupted.is_empty();
    let mut details = json!({
        "toolchain_root": toolchain_root.display().to_string(),
        "rustlib_root": rustlib_root.display().to_string(),
        "installed_targets": installed,
        "targets": targets_report,
        "corrupted_targets": corrupted.clone(),
    });
    if !corrupted.is_empty() {
        let commands: Vec<String> = corrupted
            .iter()
            .map(|t| {
                format!("rustup component remove rust-std --target {t} && rustup target add {t}")
            })
            .collect();
        if let Some(obj) = details.as_object_mut() {
            obj.insert("repair_commands".to_string(), json!(commands));
        }
    }

    ProbeResult {
        name: PROBE_RUSTLIB_INTEGRITY.to_string(),
        ok,
        details,
    }
}

/// Ask the resolved compiler for its sysroot instead of deriving it from the
/// executable path. `resolve_toolchain_binary("rustc")` can legitimately
/// return a soldr/rustup shim under `CARGO_HOME/bin`; the shim's parent is not
/// the active toolchain root and treating it as one makes every installed
/// target look corrupt.
fn query_rustc_sysroot(rustc: &Path) -> Result<PathBuf, String> {
    let mut command = std::process::Command::new(rustc);
    command.args(["--print", "sysroot"]);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, rustc);
    suppress_windows_console_window(&mut command);
    let output =
        command_output_with_timeout(&mut command, "rustc --print sysroot").map_err(|err| {
            format!(
                "failed to execute {} --print sysroot: {err}",
                rustc.display()
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{} --print sysroot exited with {}: {}",
            rustc.display(),
            output.status,
            stderr.trim()
        ));
    }
    parse_rustc_sysroot_stdout(&output.stdout)
}

fn parse_rustc_sysroot_stdout(stdout: &[u8]) -> Result<PathBuf, String> {
    let value = std::str::from_utf8(stdout)
        .map_err(|err| format!("rustc --print sysroot emitted non-UTF-8 output: {err}"))?
        .trim();
    if value.is_empty() {
        return Err("rustc --print sysroot emitted an empty path".to_string());
    }
    Ok(PathBuf::from(value))
}

/// Classification of `${toolchain_root}/lib/rustlib/<target>/lib/`.
///
/// * `Populated` — dir exists and contains at least one `.rlib`.
/// * `Missing` — dir does not exist (or is not a directory). This is
///   the exact symptom soldr#884 filed against.
/// * `EmptyOrNoRlibs` — dir exists but has no `.rlib` files. Same
///   symptom class (cargo build will fail with `can't find crate for
///   core/std`), separately labeled so operators can tell whether the
///   dir was rm-rf'd vs. partially populated.
#[derive(Debug, PartialEq, Eq)]
enum RustlibLibDirStatus {
    Populated,
    Missing,
    EmptyOrNoRlibs,
}

impl RustlibLibDirStatus {
    fn label(&self) -> &'static str {
        match self {
            RustlibLibDirStatus::Populated => "populated",
            RustlibLibDirStatus::Missing => "missing",
            RustlibLibDirStatus::EmptyOrNoRlibs => "empty-or-no-rlibs",
        }
    }
}

fn classify_rustlib_lib_dir(lib_dir: &Path) -> RustlibLibDirStatus {
    if !lib_dir.is_dir() {
        return RustlibLibDirStatus::Missing;
    }
    let has_rlib = std::fs::read_dir(lib_dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "rlib"))
        })
        .unwrap_or(false);
    if has_rlib {
        RustlibLibDirStatus::Populated
    } else {
        RustlibLibDirStatus::EmptyOrNoRlibs
    }
}

/// Ask rustup for installed targets on the *active* toolchain (no
/// explicit `--toolchain <channel>` flag). This lets the probe run
/// against whichever toolchain `rustup which rustc` just resolved
/// against — the two calls agree by construction because we let rustup
/// decide the toolchain in both cases.
fn rustup_installed_targets_active() -> Result<Vec<String>, SoldrError> {
    let mut command = std::process::Command::new(crate::binaries::rustup_binary());
    command.args(["target", "list", "--installed"]);
    crate::binaries::apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "rustup target list --installed")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup target list --installed` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

fn find_on_path(cmd: &str) -> Option<PathBuf> {
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
                dir.join(cmd)
            } else {
                dir.join(format!("{cmd}{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Run `<bin> --version` and capture the first line of stdout. Returns
/// `None` when the spawn fails, the exit code is non-zero, or stdout is
/// empty.
fn read_version<P: AsRef<OsStr>>(bin: P) -> Option<String> {
    let output = crate::platform::wait::output_for(
        std::process::Command::new(bin.as_ref()).arg("--version"),
        crate::platform::wait::PROBE,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .map(str::to_string)?;
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

fn emit_json(output: &DoctorOutput) -> Result<(), SoldrError> {
    let payload = serde_json::to_string_pretty(output)
        .map_err(|e| SoldrError::Other(format!("doctor: failed to serialize JSON: {e}")))?;
    println!("{payload}");
    Ok(())
}

fn emit_human(output: &DoctorOutput) {
    println!(
        "soldr toolchain doctor: host {os}/{arch}/{libc}",
        os = output.host.os,
        arch = output.host.arch,
        libc = output.host.libc,
    );
    for probe in &output.probes {
        let status = if probe.ok { "ok" } else { "FAIL" };
        println!("  [{status}] {} {}", probe.name, probe.details);
    }
    println!("soldr toolchain doctor: {} ms", output.elapsed_ms);
}

// soldr#2996: relocated from the deleted `rust_plan` module, whose target
// cache was removed. This probe is the only remaining consumer.
/// Count `.fingerprint/` directories under `root` (up to `max_depth` levels)
/// that contain at least one entry. The walk stops descending once a
/// `.fingerprint/` is encountered — cargo never nests another inside.
fn count_populated_fingerprint_dirs(root: &std::path::Path, max_depth: usize) -> usize {
    let mut count = 0usize;
    walk_for_fingerprint_dirs(root, max_depth, &mut count);
    count
}

fn walk_for_fingerprint_dirs(dir: &std::path::Path, remaining_depth: usize, count: &mut usize) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".fingerprint") {
            // Only count as "populated" if at least one entry exists.
            let populated = std::fs::read_dir(&path)
                .map(|mut iter| iter.next().is_some())
                .unwrap_or(false);
            if populated {
                *count += 1;
            }
            continue;
        }
        if remaining_depth > 0 {
            walk_for_fingerprint_dirs(&path, remaining_depth - 1, count);
        }
    }
}

#[cfg(test)]
#[path = "toolchain_doctor_target_tests.rs"]
mod target_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("soldr-doctor-test-{label}-{nanos}"));
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn host_info_serializes_with_three_string_fields() {
        let host = HostInfo {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            libc: "gnu".to_string(),
        };
        let json = serde_json::to_value(&host).expect("serialize");
        assert_eq!(json["os"], Value::from("linux"));
        assert_eq!(json["arch"], Value::from("x86_64"));
        assert_eq!(json["libc"], Value::from("gnu"));
    }

    #[test]
    fn host_info_detect_populates_all_fields() {
        let host = HostInfo::detect();
        assert!(!host.os.is_empty(), "os should not be empty: {host:?}");
        assert!(!host.arch.is_empty(), "arch should not be empty: {host:?}");
        assert!(!host.libc.is_empty(), "libc should not be empty: {host:?}");
    }

    #[test]
    fn probe_musl_cc_is_skipped_on_non_linux() {
        let host = HostInfo {
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            libc: "darwin".to_string(),
        };
        let probe = probe_musl_cc(&host);
        assert_eq!(probe.name, PROBE_MUSL_CC);
        assert!(probe.ok, "non-linux probe should report ok=true");
        assert_eq!(
            probe.details["skipped"],
            Value::from("not-linux"),
            "non-linux probe should report skipped=not-linux: {}",
            probe.details
        );
    }

    #[test]
    fn probe_musl_cc_on_linux_returns_search_metadata() {
        // We can't guarantee musl is installed on the test host, so just
        // assert that the linux path returns either a found-tool record
        // (with `musl_cc` populated) or a not-found record (`found: false`).
        let host = HostInfo {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            libc: "gnu".to_string(),
        };
        let probe = probe_musl_cc(&host);
        assert_eq!(probe.name, PROBE_MUSL_CC);
        assert!(probe.ok);
        assert!(
            probe.details.get("skipped").is_none(),
            "linux probe must NOT be skipped: {}",
            probe.details
        );
        let has_musl_cc = probe.details.get("musl_cc").is_some();
        let has_found_false = probe.details.get("found").and_then(Value::as_bool) == Some(false);
        assert!(
            has_musl_cc || has_found_false,
            "linux probe must report either a found musl-cc or found=false: {}",
            probe.details
        );
    }

    #[test]
    fn probe_shared_target_warning_reports_no_target_dir() {
        let dir = tempdir("no-target");
        let probe = probe_shared_target_warning(&dir);
        assert_eq!(probe.name, PROBE_SHARED_TARGET_WARNING);
        assert!(probe.ok);
        assert_eq!(probe.details["would_warn"], Value::from(false));
        assert_eq!(probe.details["fingerprint_dirs_found"], Value::from(0));
        assert_eq!(probe.details["reason"], Value::from("no-target-dir"));
    }

    #[test]
    fn probe_shared_target_warning_clean_target_dir() {
        let dir = tempdir("clean-target");
        // Empty target/ dir — exists but has no `.fingerprint/`.
        fs::create_dir_all(dir.join("target")).expect("mkdir target");
        let probe = probe_shared_target_warning(&dir);
        assert!(probe.ok);
        assert_eq!(probe.details["would_warn"], Value::from(false));
        assert_eq!(probe.details["fingerprint_dirs_found"], Value::from(0));
    }

    #[test]
    fn probe_shared_target_warning_detects_populated_fingerprint_dir() {
        let dir = tempdir("populated-target");
        // Mirror cargo's layout: target/<profile>/.fingerprint/<crate>/...
        let fingerprint = dir.join("target").join("debug").join(".fingerprint");
        fs::create_dir_all(fingerprint.join("some-crate-abc123")).expect("mkdir fingerprint");
        fs::write(
            fingerprint
                .join("some-crate-abc123")
                .join("invoked.timestamp"),
            "",
        )
        .expect("seed fingerprint entry");

        let probe = probe_shared_target_warning(&dir);
        assert!(probe.ok);
        assert_eq!(probe.details["would_warn"], Value::from(true));
        assert!(
            probe.details["fingerprint_dirs_found"]
                .as_u64()
                .unwrap_or(0)
                >= 1,
            "expected at least 1 fingerprint dir, got {}",
            probe.details["fingerprint_dirs_found"]
        );
    }

    #[test]
    fn classify_rustlib_lib_dir_reports_missing_for_absent_dir() {
        let dir = tempdir("rustlib-missing").join("does-not-exist");
        assert_eq!(classify_rustlib_lib_dir(&dir), RustlibLibDirStatus::Missing);
    }

    #[test]
    fn classify_rustlib_lib_dir_reports_empty_for_dir_without_rlibs() {
        let dir = tempdir("rustlib-empty");
        fs::create_dir_all(&dir).expect("mkdir lib");
        fs::write(dir.join("libstd-abc.rmeta"), b"placeholder").expect("write non-rlib");
        assert_eq!(
            classify_rustlib_lib_dir(&dir),
            RustlibLibDirStatus::EmptyOrNoRlibs,
            "only .rmeta present → status must be EmptyOrNoRlibs"
        );
    }

    #[test]
    fn classify_rustlib_lib_dir_reports_populated_when_rlib_present() {
        let dir = tempdir("rustlib-populated");
        fs::create_dir_all(&dir).expect("mkdir lib");
        fs::write(dir.join("libcore-abcdef.rlib"), b"placeholder").expect("write rlib");
        assert_eq!(
            classify_rustlib_lib_dir(&dir),
            RustlibLibDirStatus::Populated
        );
    }

    #[test]
    fn rustlib_lib_dir_status_label_is_stable() {
        assert_eq!(RustlibLibDirStatus::Populated.label(), "populated");
        assert_eq!(RustlibLibDirStatus::Missing.label(), "missing");
        assert_eq!(
            RustlibLibDirStatus::EmptyOrNoRlibs.label(),
            "empty-or-no-rlibs"
        );
    }

    #[test]
    fn parse_rustc_sysroot_stdout_accepts_trimmed_absolute_path() {
        let parsed = parse_rustc_sysroot_stdout(b"  C:\\toolchains\\1.94.1  \r\n")
            .expect("valid sysroot output");
        assert_eq!(parsed, PathBuf::from(r"C:\toolchains\1.94.1"));
    }

    #[test]
    fn parse_rustc_sysroot_stdout_rejects_empty_output() {
        let err = parse_rustc_sysroot_stdout(b" \r\n\t")
            .expect_err("empty sysroot output must be rejected");
        assert!(err.contains("empty path"), "unexpected error: {err}");
    }

    #[test]
    fn parse_rustc_sysroot_stdout_rejects_non_utf8_output() {
        let err = parse_rustc_sysroot_stdout(&[0xff, 0xfe])
            .expect_err("non-UTF-8 sysroot output must be rejected");
        assert!(err.contains("non-UTF-8"), "unexpected error: {err}");
    }

    #[test]
    fn doctor_output_serializes_schema_version_one() {
        let output = DoctorOutput {
            schema_version: SCHEMA_VERSION,
            host: HostInfo {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                libc: "gnu".to_string(),
            },
            probes: vec![
                ProbeResult {
                    name: PROBE_MUSL_CC.to_string(),
                    ok: true,
                    details: json!({"skipped": "not-linux"}),
                },
                ProbeResult {
                    name: PROBE_SHARED_TARGET_WARNING.to_string(),
                    ok: true,
                    details: json!({"would_warn": false, "fingerprint_dirs_found": 0}),
                },
            ],
            elapsed_ms: 7,
        };
        let json = serde_json::to_value(&output).expect("serialize");
        assert_eq!(json["schema_version"], Value::from(1));
        assert!(json["host"].is_object());
        let probes = json["probes"].as_array().expect("probes array");
        assert_eq!(probes.len(), 2);
        assert_eq!(probes[0]["name"], Value::from(PROBE_MUSL_CC));
        assert_eq!(probes[1]["name"], Value::from(PROBE_SHARED_TARGET_WARNING));
        assert!(json["elapsed_ms"].is_number());
    }
}
