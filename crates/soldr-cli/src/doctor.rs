//! `soldr doctor` — drift detector for `rust-toolchain.toml`. Extracted
//! from `main.rs` as part of issue #339.

use crate::cache::print_json;
use crate::core::{
    command_output_with_timeout, suppress_windows_console_window, SoldrError, SoldrPaths,
};
use crate::defender_probe::{self, DefenderProbeState, DefenderVerdict, SCANNED_THRESHOLD_MS};
#[cfg(test)]
use crate::doctor_cook::fmt_bytes;
use crate::doctor_cook::{collect_cook_stats, print_cook_section, DoctorCookStats};
use crate::{apply_implicit_toolchain_homes, rustup_binary, JSON_SCHEMA_VERSION};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
struct DoctorComponent {
    name: String,
    installed: bool,
}

#[derive(Serialize)]
struct DoctorTarget {
    triple: String,
    installed: bool,
}

#[derive(Serialize)]
struct DoctorToolchain {
    channel: String,
    installed: bool,
}

#[derive(Serialize)]
struct DoctorOutput {
    schema_version: u32,
    command: &'static str,
    /// Absolute path to the inspected `rust-toolchain.toml`. `None`
    /// when no manifest exists in the current working directory.
    manifest_path: Option<String>,
    /// `None` when the manifest is missing or omits `channel`.
    toolchain: Option<DoctorToolchain>,
    components: Vec<DoctorComponent>,
    targets: Vec<DoctorTarget>,
    /// Whether any declared component or target is missing from the
    /// installed rustup state. Always `false` when no manifest exists.
    drift: bool,
    missing_components: Vec<String>,
    missing_targets: Vec<String>,
    /// Effective value and provenance of every timeout with an env override (soldr#1838 Phase 3).
    timeouts: Vec<crate::timeout_registry::DoctorTimeout>,
    /// Stable-broker route acquisition deadlines and their environment provenance (soldr#2476).
    broker_deadlines: Vec<crate::broker_deadlines::DoctorBrokerDeadline>,
    /// Authoritative broker installation identity and physical bind endpoint.
    broker_endpoint: crate::broker_identity::DoctorBrokerEndpoint,
    /// soldr-broker / soldr-daemon processes on this host serving a HOME
    /// other than the invoking one (soldr#3193). `None` when HOME is unset.
    leaked_processes: Option<crate::broker_inventory::DoctorInventory>,
    /// Rollup of compile-daemon fallback events -- builds that ran
    /// uncached via direct rustc (soldr#1838 Phase 4). Empty means the
    /// cache was never bypassed.
    fallbacks: crate::compile_fallback_rollup::FallbackRollup,
    /// Embedded zccache backend + version (soldr#1368).
    zccache: DoctorZccache,
    /// Debug-info sidecar state for the running soldr binary. On
    /// Windows this points at the matching PDB directory for dump
    /// symbolication.
    soldr_debug_info: DoctorSoldrDebugInfo,
    /// Defender real-time-scan probe state for the cache directory
    /// (issue #357). `None` on non-Windows platforms.
    defender_probe: Option<DoctorDefenderProbe>,
    /// Cook-artifact cache state. `None` when neither the daemon
    /// `CookStats` nor the on-disk scan succeeds (e.g. malformed
    /// paths). Otherwise populated with whatever subset was
    /// available. (#590)
    cook: Option<DoctorCookStats>,
    /// Degenerate-cache detectors (soldr#2436 phase 4, D10).
    cache_health: crate::cache_health::CacheHealth,
    /// Whether this worktree builds into another checkout's target dir
    /// (soldr#3092). `None` outside a git checkout.
    worktree_target: Option<crate::worktree_target::WorktreeTarget>,
}

#[derive(Serialize)]
struct DoctorDefenderProbe {
    /// `scanned`, `excluded`, or `not_applicable`.
    verdict: &'static str,
    /// Path the probe targeted (the resolved soldr cache dir).
    probed_path: String,
    /// Median write time across the probe's repeat samples, in ms.
    median_write_ms: u64,
    /// Unix timestamp the probe was run.
    probed_at_unix: u64,
    /// True when the result was just produced (and persisted) this
    /// invocation; false when the cached state was served.
    refreshed_this_run: bool,
}

#[derive(Serialize, Clone)]
struct DoctorZccache {
    /// Always "embedded" — rustc compile caching runs through the
    /// soldr-daemon's in-process zccache service; there is no
    /// externally-downloaded managed binary (soldr#1368).
    backend: &'static str,
    /// Compiled-in `zccache::core::VERSION`.
    version: &'static str,
    /// Stale per-launch `zccache-daemon*` binary copies under
    /// `<zccache-root>/*/runtime-binaries/`, left behind by old installs
    /// or direct standalone zccache CLI use (soldr#1467). Always empty
    /// on a healthy soldr box — the cache runs embedded in soldr-daemon.
    stale_runtime_binaries: Vec<String>,
    /// Running processes whose image name starts with `zccache-daemon`
    /// (soldr#1467). Best-effort advisory scan; empty on failure.
    standalone_daemon_processes: Vec<String>,
    /// Most recent root-local daemon maintenance outcome.
    maintenance: Option<crate::daemon::maintenance::MaintenanceStatus>,
}

#[derive(Serialize, Clone)]
struct DoctorSoldrDebugInfo {
    binary_path: String,
    debug_info_found: usize,
    debug_info_expected: usize,
    symbol_path: String,
}

/// Implementation of `soldr doctor`. Read-only — never invokes
/// `rustup component add` / `target add` / `toolchain install`.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn run_doctor(
    json: bool,
    refresh_defender_probe: bool,
    remove_shadowing_shim: bool,
) -> Result<i32, SoldrError> {
    // soldr#1979: the repair is its own action, not a mode of the report.
    // Doing both would mean a user cannot see what is wrong without also
    // changing it -- the implicitness #1983 objected to.
    if remove_shadowing_shim {
        crate::shim_hygiene::print_shim_removal_section();
        return Ok(0);
    }
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    // Walk ancestors like rustup/cargo (soldr#3633).
    let found_manifest = crate::core::find_rust_toolchain_manifest(&workspace_root);
    let manifest_present = found_manifest.is_some();
    let manifest_path =
        found_manifest.unwrap_or_else(|| workspace_root.join("rust-toolchain.toml"));
    let manifest = crate::core::read_rust_toolchain_manifest_from_ancestors(&workspace_root)?;
    crate::startup_trace::phase(crate::startup_trace::phase::DOCTOR_MANIFEST);
    // soldr#2571: `doctor` emits its first byte only at the closing
    // `print_json`, so every collector below runs inside the silent window
    // that made the 15 s wedge undiagnosable. Two of them are the prime
    // suspects — the Defender probe shells out to PowerShell on Windows, and
    // the cook stats dial the daemon (the very endpoint the flaky test is
    // named for) — so each reports its own completion under the trace.
    let bundle = collect_zccache_bundle()?;
    crate::startup_trace::phase(crate::startup_trace::phase::DOCTOR_ZCCACHE_BUNDLE);
    let soldr_debug_info = collect_soldr_debug_info();
    crate::startup_trace::phase(crate::startup_trace::phase::DOCTOR_DEBUG_INFO);
    let defender = collect_defender_probe(refresh_defender_probe);
    crate::startup_trace::phase(crate::startup_trace::phase::DOCTOR_DEFENDER_PROBE);
    let cook = collect_cook_stats();
    crate::startup_trace::phase(crate::startup_trace::phase::DOCTOR_COOK_STATS);
    let fallbacks = collect_fallback_rollup();
    crate::startup_trace::phase(crate::startup_trace::phase::DOCTOR_FALLBACK_ROLLUP);
    let cache_health = crate::cache_health::assess(&SoldrPaths::new()?);
    let worktree_target = crate::worktree_target::probe(&workspace_root);
    crate::startup_trace::phase(crate::startup_trace::phase::DOCTOR_CACHE_HEALTH);
    let leaked_processes = crate::broker_inventory::scan_for_doctor();
    crate::startup_trace::phase(crate::startup_trace::phase::DOCTOR_BROKER_INVENTORY);

    let Some(channel) = manifest.channel.as_deref() else {
        if json {
            let output = DoctorOutput {
                schema_version: JSON_SCHEMA_VERSION,
                command: "doctor",
                manifest_path: manifest_present.then(|| manifest_path.display().to_string()),
                toolchain: None,
                components: Vec::new(),
                targets: Vec::new(),
                drift: false,
                missing_components: Vec::new(),
                missing_targets: Vec::new(),
                timeouts: crate::timeout_registry::doctor_rows(),
                broker_deadlines: crate::broker_deadlines::doctor_deadlines(),
                broker_endpoint: crate::broker_identity::doctor_endpoint(),
                leaked_processes: leaked_processes.clone(),
                fallbacks: fallbacks.clone(),
                zccache: bundle.clone(),
                soldr_debug_info: soldr_debug_info.clone(),
                defender_probe: defender_for_json(defender.as_ref()),
                cook: cook.clone(),
                cache_health: cache_health.clone(),
                worktree_target: worktree_target.clone(),
            };
            print_json(&output)?;
        } else if manifest_present {
            println!(
                "manifest: {} (present but no [toolchain] channel declared)",
                manifest_path.display()
            );
            print_zccache_sections(&bundle);
            crate::cache_health::print_human(&cache_health);
            crate::worktree_target::print_human(worktree_target.as_ref());
            crate::broker_deadlines::print_doctor_deadlines();
            crate::broker_identity::print_doctor_endpoint();
            crate::broker_inventory::print_doctor_human(leaked_processes.as_ref());
            print_soldr_debug_info_human(&soldr_debug_info);
            print_defender_probe_human(defender.as_ref());
            if let Some(c) = cook.as_ref() {
                print_cook_section(c);
            }
            crate::shim_hygiene::print_shim_hygiene_section();
            println!("result: no manifest fields to compare; nothing to do");
        } else {
            println!(
                "no rust-toolchain.toml found in {}",
                workspace_root.display()
            );
            print_zccache_sections(&bundle);
            crate::cache_health::print_human(&cache_health);
            crate::worktree_target::print_human(worktree_target.as_ref());
            crate::broker_deadlines::print_doctor_deadlines();
            crate::broker_identity::print_doctor_endpoint();
            crate::broker_inventory::print_doctor_human(leaked_processes.as_ref());
            print_soldr_debug_info_human(&soldr_debug_info);
            print_defender_probe_human(defender.as_ref());
            if let Some(c) = cook.as_ref() {
                print_cook_section(c);
            }
            crate::shim_hygiene::print_shim_hygiene_section();
            println!("result: no manifest found; nothing to compare");
        }
        return Ok(0);
    };

    let toolchain_installed = rustup_toolchain_is_installed(channel)?;

    let declared_components: Vec<String> = manifest.components.clone().unwrap_or_default();
    let declared_targets: Vec<String> = manifest.targets.clone().unwrap_or_default();

    let installed_components = if toolchain_installed && !declared_components.is_empty() {
        rustup_installed_components(channel)?
    } else {
        Vec::new()
    };
    let installed_targets = if toolchain_installed && !declared_targets.is_empty() {
        rustup_installed_targets(channel)?
    } else {
        Vec::new()
    };

    let component_rows: Vec<DoctorComponent> = declared_components
        .iter()
        .map(|declared| DoctorComponent {
            name: declared.clone(),
            installed: component_is_installed(declared, &installed_components),
        })
        .collect();
    let target_rows: Vec<DoctorTarget> = declared_targets
        .iter()
        .map(|declared| DoctorTarget {
            triple: declared.clone(),
            installed: target_is_installed(declared, &installed_targets),
        })
        .collect();

    let missing_components: Vec<String> = component_rows
        .iter()
        .filter(|row| !row.installed)
        .map(|row| row.name.clone())
        .collect();
    let missing_targets: Vec<String> = target_rows
        .iter()
        .filter(|row| !row.installed)
        .map(|row| row.triple.clone())
        .collect();

    let drift =
        !toolchain_installed || !missing_components.is_empty() || !missing_targets.is_empty();

    if json {
        let output = DoctorOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "doctor",
            manifest_path: Some(manifest_path.display().to_string()),
            toolchain: Some(DoctorToolchain {
                channel: channel.to_string(),
                installed: toolchain_installed,
            }),
            components: component_rows,
            targets: target_rows,
            drift,
            missing_components,
            missing_targets,
            timeouts: crate::timeout_registry::doctor_rows(),
            broker_deadlines: crate::broker_deadlines::doctor_deadlines(),
            broker_endpoint: crate::broker_identity::doctor_endpoint(),
            leaked_processes: leaked_processes.clone(),
            fallbacks: fallbacks.clone(),
            zccache: bundle.clone(),
            soldr_debug_info: soldr_debug_info.clone(),
            defender_probe: defender_for_json(defender.as_ref()),
            cook: cook.clone(),
            cache_health: cache_health.clone(),
            worktree_target: worktree_target.clone(),
        };
        print_json(&output)?;
    } else {
        print_doctor_human(
            &manifest_path,
            channel,
            toolchain_installed,
            &component_rows,
            &target_rows,
            &missing_components,
            &missing_targets,
            drift,
            &bundle,
            &soldr_debug_info,
            defender.as_ref(),
            cook.as_ref(),
            &cache_health,
        );
        crate::worktree_target::print_human(worktree_target.as_ref());
        crate::timeout_registry::print_doctor_section();
        crate::broker_deadlines::print_doctor_deadlines();
        crate::broker_identity::print_doctor_endpoint();
        crate::broker_inventory::print_doctor_human(leaked_processes.as_ref());
        crate::compile_fallback_rollup::print_section(&fallbacks);
    }

    Ok(if drift { 1 } else { 0 })
}

/// Read the compile-daemon fallback journal for the running soldr root
/// (soldr#1838 Phase 4). Best-effort: if the soldr paths cannot be
/// resolved, report an empty rollup rather than failing `doctor`.
fn collect_fallback_rollup() -> crate::compile_fallback_rollup::FallbackRollup {
    match SoldrPaths::new() {
        Ok(paths) => crate::compile_fallback_rollup::collect(
            &paths,
            crate::compile_fallback_rollup::DOCTOR_RECENT_LIMIT,
        ),
        Err(_) => crate::compile_fallback_rollup::FallbackRollup::default(),
    }
}

/// In-memory record of the defender probe result the doctor command
/// will surface. Wraps the persisted state plus a flag that tracks
/// whether this invocation produced a fresh probe (true) or reused
/// the cached one (false).
struct DefenderProbeOutcome {
    state: DefenderProbeState,
    refreshed_this_run: bool,
}

/// Count debug-info sidecars next to the soldr binary (soldr#1368
/// replacement for the deleted count_debug_info_sidecars helper). On
/// Windows soldr ships a `.pdb`; elsewhere there is no separate sidecar.
fn count_soldr_debug_sidecars(binary: &Path) -> (usize, usize) {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        let pdb = binary.with_extension("pdb");
        (usize::from(pdb.is_file()), 1)
    } else {
        (0, 0)
    }
}

fn collect_soldr_debug_info() -> DoctorSoldrDebugInfo {
    let binary_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("soldr"));
    let symbol_path = binary_path
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let (debug_info_found, debug_info_expected) = count_soldr_debug_sidecars(&binary_path);

    DoctorSoldrDebugInfo {
        binary_path: binary_path.display().to_string(),
        debug_info_found,
        debug_info_expected,
        symbol_path: symbol_path.display().to_string(),
    }
}

/// Read the cached probe state (or run a fresh probe if forced /
/// stale / missing) and return the outcome. Errors are swallowed:
/// doctor is a diagnostic command and the probe is best-effort. On
/// non-Windows the probe always classifies as `NotApplicable`.
fn collect_defender_probe(refresh: bool) -> Option<DefenderProbeOutcome> {
    let paths = SoldrPaths::new().ok()?;
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target_dir = paths.cache.clone();
    let cached = defender_probe::read_probe_state(&paths);
    let soldr_version = env!("CARGO_PKG_VERSION");

    let reason = defender_probe::reprobe_reason(
        cached.as_ref(),
        &target_dir,
        soldr_version,
        now_unix,
        refresh,
    );

    match reason {
        None => cached.map(|state| DefenderProbeOutcome {
            state,
            refreshed_this_run: false,
        }),
        Some(_) => {
            // Skip the probe entirely outside Windows: there's no
            // Defender to measure, and writing a sample state file
            // on every Linux/macOS doctor invocation would just be
            // noise. Returning None makes the human/JSON printer
            // omit the section.
            if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
                return None;
            }
            let fresh = defender_probe::run_probe(&target_dir, soldr_version).ok()?;
            let _ = defender_probe::write_probe_state(&paths, &fresh);
            Some(DefenderProbeOutcome {
                state: fresh,
                refreshed_this_run: true,
            })
        }
    }
}

fn defender_for_json(outcome: Option<&DefenderProbeOutcome>) -> Option<DoctorDefenderProbe> {
    let outcome = outcome?;
    Some(DoctorDefenderProbe {
        verdict: outcome.state.verdict.as_str(),
        probed_path: outcome.state.probed_path.display().to_string(),
        median_write_ms: outcome.state.median_write_ms,
        probed_at_unix: outcome.state.probed_at_unix,
        refreshed_this_run: outcome.refreshed_this_run,
    })
}

fn print_defender_probe_human(outcome: Option<&DefenderProbeOutcome>) {
    let Some(outcome) = outcome else {
        return;
    };
    println!();
    println!("defender probe (cache directory):");
    println!("  path:          {}", outcome.state.probed_path.display());
    let age_label = if outcome.refreshed_this_run {
        "just now".to_string()
    } else {
        format_age(outcome.state.probed_at_unix)
    };
    match outcome.state.verdict {
        DefenderVerdict::NotApplicable => {
            println!("  verdict:       not applicable (non-Windows platform)");
        }
        DefenderVerdict::Excluded => {
            println!(
                "  verdict:       {} ms median write — path appears excluded from real-time scanning",
                outcome.state.median_write_ms,
            );
            println!("  probed:        {age_label}");
        }
        DefenderVerdict::Scanned => {
            println!(
                "  verdict:       {} ms median write — likely being scanned by Defender",
                outcome.state.median_write_ms,
            );
            println!("  probed:        {age_label}");
            println!(
                "  recommendation: run bench/add_defender_exclusions.ps1 as admin, or move \
                 SOLDR_CACHE_DIR onto a trusted Dev Drive (Windows 11 22H2+)"
            );
            println!("  threshold:     median > {SCANNED_THRESHOLD_MS} ms classifies as scanned");
        }
    }
}

fn format_age(probed_at_unix: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let delta = now.saturating_sub(probed_at_unix);
    if delta < 60 {
        return "just now".to_string();
    }
    if delta < 3600 {
        let mins = delta / 60;
        return format!("{mins} min ago");
    }
    if delta < 24 * 3600 {
        let hours = delta / 3600;
        return format!("{hours} h ago");
    }
    let days = delta / (24 * 3600);
    format!("{days} d ago")
}

/// Collect embedded-zccache info for doctor output (soldr#1368 +
/// soldr#1467). There is no externally-resolved zccache binary any more —
/// compile caching runs through the soldr-daemon embedded service — so
/// this reports the compiled-in backend + version plus an advisory scan
/// for standalone-zccache leftovers that should not exist on a soldr box.
fn collect_zccache_bundle() -> Result<DoctorZccache, SoldrError> {
    let paths = SoldrPaths::new()?;
    Ok(DoctorZccache {
        backend: "embedded",
        version: zccache::core::VERSION,
        stale_runtime_binaries: scan_stale_runtime_binaries(),
        standalone_daemon_processes: scan_standalone_daemon_processes(),
        maintenance: crate::daemon::maintenance::read_status(&paths),
    })
}

/// Test seam (soldr#1467): overrides the directory scanned for stale
/// `*/runtime-binaries/zccache-daemon*` copies. Production default is
/// the parent of `zccache::core::config::default_cache_dir()` (the
/// top-level `~/.zccache/` above the per-version subdir).
pub(crate) const SOLDR_TEST_ZCCACHE_SCAN_ROOT_ENV: &str = "SOLDR_TEST_ZCCACHE_SCAN_ROOT";

/// Test seam (soldr#1467): when set, points at a file with one
/// `pid name` pair per line that is used INSTEAD of the real process
/// scan (`tasklist` / `ps`).
pub(crate) const SOLDR_TEST_PROCESS_LIST_FILE_ENV: &str = "SOLDR_TEST_PROCESS_LIST_FILE";

/// Image-name prefix that identifies a standalone zccache daemon, both
/// as a process name and as a per-launch runtime-binaries copy.
const STANDALONE_DAEMON_PREFIX: &str = "zccache-daemon";

/// The directory whose `*/runtime-binaries/` children are scanned for
/// stale daemon copies. `None` when neither the test seam nor a home
/// directory resolves.
fn zccache_scan_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(SOLDR_TEST_ZCCACHE_SCAN_ROOT_ENV) {
        return Some(PathBuf::from(root));
    }
    // `default_cache_dir()` is the *versioned* dir (`~/.zccache/v1.12.x`);
    // stale copies from other zccache versions live in sibling versioned
    // dirs, so scan the parent.
    zccache::core::config::default_cache_dir()
        .as_path()
        .parent()
        .map(Path::to_path_buf)
}

/// Scan `<root>/*/runtime-binaries/` for `zccache-daemon*` files
/// (soldr#1467). Best-effort: any I/O failure yields an empty result.
fn scan_stale_runtime_binaries() -> Vec<String> {
    zccache_scan_root()
        .map(|root| scan_stale_runtime_binaries_in(&root))
        .unwrap_or_default()
}

/// Pure scan body, seam-testable with any directory.
fn scan_stale_runtime_binaries_in(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let runtime_dir = entry.path().join("runtime-binaries");
        let Ok(copies) = std::fs::read_dir(&runtime_dir) else {
            continue;
        };
        for copy in copies.flatten() {
            let name = copy.file_name();
            if name.to_string_lossy().starts_with(STANDALONE_DAEMON_PREFIX) {
                found.push(copy.path().display().to_string());
            }
        }
    }
    found.sort();
    found
}

/// Best-effort scan for running `zccache-daemon*` processes
/// (soldr#1467). Honors [`SOLDR_TEST_PROCESS_LIST_FILE_ENV`]; real-scan
/// failures are silent (empty vec) — the probe is advisory.
fn scan_standalone_daemon_processes() -> Vec<String> {
    if let Some(file) = std::env::var_os(SOLDR_TEST_PROCESS_LIST_FILE_ENV) {
        return std::fs::read_to_string(file)
            .map(|text| parse_pid_name_lines(&text))
            .unwrap_or_default();
    }
    real_process_scan().unwrap_or_default()
}

fn real_process_scan() -> Option<Vec<String>> {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        let mut command = std::process::Command::new("tasklist");
        command.args(["/FO", "CSV", "/NH"]);
        suppress_windows_console_window(&mut command);
        let output = command_output_with_timeout(&mut command, "tasklist /FO CSV /NH").ok()?;
        if !output.status.success() {
            return None;
        }
        Some(parse_tasklist_csv(&String::from_utf8_lossy(&output.stdout)))
    } else {
        let mut command = std::process::Command::new("ps");
        command.args(["-eo", "pid=,comm="]);
        suppress_windows_console_window(&mut command);
        let output = command_output_with_timeout(&mut command, "ps -eo pid=,comm=").ok()?;
        if !output.status.success() {
            return None;
        }
        Some(parse_pid_name_lines(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

/// Format one matching process row; `None` when the image name is not
/// a standalone zccache daemon.
fn daemon_process_entry(pid: &str, name: &str) -> Option<String> {
    let (pid, name) = (pid.trim(), name.trim());
    if pid.is_empty() || !name.starts_with(STANDALONE_DAEMON_PREFIX) {
        return None;
    }
    Some(format!("{name} (pid {pid})"))
}

/// Parse `pid name` pairs (one per line): the seam-file format, which is
/// also exactly what `ps -eo pid=,comm=` emits on Unix.
fn parse_pid_name_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let (pid, name) = line.trim().split_once(char::is_whitespace)?;
            daemon_process_entry(pid, name)
        })
        .collect()
}

/// Parse `tasklist /FO CSV /NH` output (Windows): quoted CSV rows with
/// the image name first and the PID second.
fn parse_tasklist_csv(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.trim().trim_matches('"').split("\",\"");
            let name = fields.next()?;
            let pid = fields.next()?;
            daemon_process_entry(pid, name)
        })
        .collect()
}

fn print_zccache_sections(bundle: &DoctorZccache) {
    println!();
    println!("zccache: {} (version {})", bundle.backend, bundle.version);
    print_standalone_zccache_human(bundle);
    crate::cache::print_maintenance_status(bundle.maintenance.as_ref());
}

/// Warn about standalone-zccache leftovers (soldr#1467); one quiet line
/// when the box is clean.
fn print_standalone_zccache_human(bundle: &DoctorZccache) {
    let daemons = bundle.standalone_daemon_processes.len();
    let stale = bundle.stale_runtime_binaries.len();
    if daemons == 0 && stale == 0 {
        println!("standalone zccache: none detected");
        return;
    }
    let root_hint = zccache_scan_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.zccache".to_string());
    println!(
        "standalone zccache: {daemons} running daemon process(es) / {stale} stale \
         runtime-binaries copies — soldr uses the embedded cache only (soldr#1467); \
         stop the daemons and delete {root_hint}{sep}*{sep}runtime-binaries to clean up.",
        sep = std::path::MAIN_SEPARATOR,
    );
    for proc in &bundle.standalone_daemon_processes {
        println!("  daemon process: {proc}");
    }
    for copy in &bundle.stale_runtime_binaries {
        println!("  stale copy:     {copy}");
    }
}

fn print_soldr_debug_info_human(summary: &DoctorSoldrDebugInfo) {
    println!();
    println!("soldr debug info:");
    println!("  binary:        {}", summary.binary_path);
    let pdb_hint = if summary.debug_info_found == 0 {
        "no PDB present; build soldr with `[profile.release] debug = \"line-tables-only\"` or set CARGO_PROFILE_*_DEBUG=line-tables-only"
    } else {
        "complete"
    };
    println!(
        "  pdbs found:    {}/{} ({})",
        summary.debug_info_found, summary.debug_info_expected, pdb_hint
    );
    println!("  symbol path:   {}", summary.symbol_path);
}

fn component_is_installed(declared: &str, installed: &[String]) -> bool {
    let prefix = format!("{declared}-");
    installed
        .iter()
        .any(|entry| entry == declared || entry.starts_with(&prefix))
}

fn target_is_installed(declared: &str, installed: &[String]) -> bool {
    installed.iter().any(|entry| entry == declared)
}

#[allow(clippy::too_many_arguments)]
fn print_doctor_human(
    manifest_path: &std::path::Path,
    channel: &str,
    toolchain_installed: bool,
    components: &[DoctorComponent],
    targets: &[DoctorTarget],
    missing_components: &[String],
    missing_targets: &[String],
    drift: bool,
    bundle: &DoctorZccache,
    soldr_debug_info: &DoctorSoldrDebugInfo,
    defender: Option<&DefenderProbeOutcome>,
    cook: Option<&DoctorCookStats>,
    cache_health: &crate::cache_health::CacheHealth,
) {
    println!("manifest: {}", manifest_path.display());
    println!("toolchain: {channel}");
    println!(
        "  status: {}",
        if toolchain_installed {
            "installed"
        } else {
            "MISSING"
        }
    );

    if !components.is_empty() {
        println!();
        println!("components (declared {}):", components.len());
        let width = components
            .iter()
            .map(|row| row.name.len())
            .max()
            .unwrap_or(0);
        for row in components {
            println!(
                "  {:<width$}   {}",
                row.name,
                if row.installed {
                    "installed"
                } else {
                    "MISSING"
                },
                width = width
            );
        }
    }

    if !targets.is_empty() {
        println!();
        println!("targets (declared {}):", targets.len());
        let width = targets
            .iter()
            .map(|row| row.triple.len())
            .max()
            .unwrap_or(0);
        for row in targets {
            println!(
                "  {:<width$}   {}",
                row.triple,
                if row.installed {
                    "installed"
                } else {
                    "MISSING"
                },
                width = width
            );
        }
    }

    print_zccache_sections(bundle);
    crate::cache_health::print_human(cache_health);
    print_soldr_debug_info_human(soldr_debug_info);
    print_defender_probe_human(defender);
    if let Some(c) = cook {
        print_cook_section(c);
    }
    crate::shim_hygiene::print_shim_hygiene_section();

    println!();
    if drift {
        let missing_component_count = missing_components.len();
        let missing_target_count = missing_targets.len();
        let mut parts: Vec<String> = Vec::new();
        if !toolchain_installed {
            parts.push("toolchain not installed".to_string());
        }
        if missing_component_count > 0 {
            parts.push(format!(
                "{missing_component_count} missing component{}",
                if missing_component_count == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if missing_target_count > 0 {
            parts.push(format!(
                "{missing_target_count} missing target{}",
                if missing_target_count == 1 { "" } else { "s" }
            ));
        }
        println!("result: drift detected ({})", parts.join(", "));
        println!(
            "hint: run `soldr toolchain prepare` to bring installed state in sync with manifest"
        );
    } else {
        println!("result: no drift");
    }
}

fn rustup_toolchain_is_installed(channel: &str) -> Result<bool, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["toolchain", "list"]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "rustup toolchain list")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup toolchain list` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == channel
            || trimmed.starts_with(&format!("{channel} "))
            || trimmed.starts_with(&format!("{channel}-"))
    }))
}

fn rustup_installed_components(channel: &str) -> Result<Vec<String>, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["component", "list", "--installed", "--toolchain", channel]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "rustup component list --installed")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup component list --installed --toolchain {channel}` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(parse_rustup_list_output(&output.stdout))
}

fn rustup_installed_targets(channel: &str) -> Result<Vec<String>, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["target", "list", "--installed", "--toolchain", channel]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "rustup target list --installed")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup target list --installed --toolchain {channel}` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(parse_rustup_list_output(&output.stdout))
}

fn parse_rustup_list_output(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
