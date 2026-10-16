//! `soldr toolchain ensure` — auto-bootstrap rustup if missing, run the
//! `prepare` pipeline, then smoke-verify the resolved toolchain by
//! spawning `cargo --version` and `rustc --version`. With `--json`,
//! emits a stable machine-facing payload (`schema_version: 1`) that
//! `setup-soldr#133` consumes to delegate its TS toolchain logic.
//!
//! Phase 2 of #407. The JSON schema is frozen at version 1 — bumping it
//! requires a coordinated version bump in the consumer.

use serde::Serialize;
use std::io::Write;
use std::time::Instant;

use crate::cargo_front_door::provision::{BuildToolsSummary, CrossToolsSummary};
use crate::core::{
    command_output_with_timeout, suppress_windows_console_window, RustToolchainManifest,
    SoldrError, SoldrPaths,
};
use crate::{
    resolve_toolchain_binary,
    toolchain::{run_prepare_inner, LinkerSummary, PrepareSummary},
};

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Debug)]
pub(crate) struct ToolchainEnsureOutput {
    pub schema_version: u32,
    pub channel: Option<String>,
    pub rustup_bootstrapped: bool,
    pub components_added: Vec<String>,
    pub targets_added: Vec<String>,
    pub plugins_installed: Vec<String>,
    /// Additive (soldr#3276 T4): reports the fetched `reld` linker when the
    /// project selects it (`SOLDR_LINKER=reld` or `linker = "reld"` in
    /// `config.toml`), `null` otherwise. Does not bump `schema_version`.
    pub linker: Option<LinkerSummary>,
    /// Additive (soldr#3802): what build-tool provisioning did -- the linker
    /// driver, `reld` and managed cmake/ninja a native `soldr cargo build`
    /// would fetch -- or why it did nothing (`provisioned: false` + `note`).
    /// `null` when provisioning did not run (no pin and no opt-in). Does not
    /// bump `schema_version`.
    pub build_tools: Option<BuildToolsSummary>,
    pub smoke_verify: SmokeVerify,
    pub elapsed_ms: u128,
    /// Additive (soldr#3693): set when an early step (bootstrap, manifest
    /// read, prepare, smoke verify) errored; `null` otherwise. The payload is
    /// still emitted, with `smoke_verify.ok = false` and a non-zero exit.
    /// Does not bump `schema_version`.
    pub error: Option<String>,
}

impl ToolchainEnsureOutput {
    fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            channel: None,
            rustup_bootstrapped: false,
            components_added: Vec::new(),
            targets_added: Vec::new(),
            plugins_installed: Vec::new(),
            linker: None,
            build_tools: None,
            smoke_verify: SmokeVerify::default(),
            elapsed_ms: 0,
            error: None,
        }
    }
}

#[derive(Serialize, Debug, Default)]
pub(crate) struct SmokeVerify {
    pub cargo_version: Option<String>,
    pub rustc_version: Option<String>,
    /// Additive (soldr#3376): host or declared-target triples whose
    /// `libcore`/`libstd` files are absent from the resolved toolchain even
    /// though rustup's metadata may still claim them. Always present, possibly
    /// empty; a non-empty list makes `ok` false. Does not bump `schema_version`.
    pub missing_std_targets: Vec<String>,
    pub ok: bool,
}

/// Implementation of `soldr toolchain ensure`. Returns the soldr exit
/// code: 0 on success, non-zero when smoke verify fails or the
/// underlying `prepare` pipeline reported a non-zero rustup / cargo
/// exit. In `--json` mode the JSON payload is emitted unconditionally
/// (even on smoke-verify failure) so the consumer can introspect.
pub(crate) async fn run_toolchain_ensure(
    json: bool,
    target: Option<String>,
) -> Result<i32, SoldrError> {
    let started = Instant::now();

    // soldr#2892: in `--json` this process's stdout IS the payload, and the
    // installer children below inherit it. They did, and rustup's stdout
    // landed in front of the JSON -- `json.load` rejects the result with
    // `Extra data: line 2 column 7`, which is how the target-run lane found
    // this.
    //
    // Not the fully-quiet marker `env --json` uses: that also silences the
    // stall heartbeat, on the grounds that its callers merge stdout and
    // stderr. This verb's caller reads stderr as a log and stdout as a file,
    // and rustup's progress is exactly what stops a human killing a
    // multi-minute first-time install. So the child's stdout moves to stderr
    // rather than being discarded.
    if json {
        std::env::set_var(crate::core::quiet::PAYLOAD_STDOUT_ENV_VAR, "1");
    }

    // 1. Bootstrap rustup if missing. Auto-bootstrap respects
    //    SOLDR_NO_BOOTSTRAP=1 by silently leaving the host unchanged —
    //    `prepare` will then surface the usual "rustup not found"
    //    diagnostic via the spawn-failure path.
    let bootstrap = bootstrap_rustup_if_missing().await;

    let steps = EnsureSteps {
        load_manifest: &|| {
            let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
            // soldr#3633: nearest pin in cwd or ancestors, like cargo/rustup.
            crate::core::read_rust_toolchain_manifest_from_ancestors(&workspace_root)
        },
        prepare: &run_prepare_inner,
        smoke: &run_smoke_verify,
        unpinned_build_tools: &unpinned_build_tools,
        cross_tools: &|| {
            target
                .as_deref()
                .map(crate::toolchain_prepare::provision_cross_target)
                .transpose()
        },
    };
    run_ensure_pipeline(json, started, bootstrap, &steps, &mut std::io::stdout())
}

type PrepareFn<'a> =
    dyn Fn(&str, &RustToolchainManifest) -> Result<(i32, PrepareSummary), SoldrError> + 'a;

/// Fallible steps of `ensure`, injectable so the error-to-payload contract
/// (soldr#3693) is testable without rustup or a real manifest.
pub(crate) struct EnsureSteps<'a> {
    pub load_manifest: &'a dyn Fn() -> Result<RustToolchainManifest, SoldrError>,
    pub prepare: &'a PrepareFn<'a>,
    pub smoke: &'a dyn Fn(&[String]) -> Result<SmokeVerify, SoldrError>,
    /// Build-tool provisioning when no channel is declared; `None` when the
    /// caller did not opt into an unpinned build.
    pub unpinned_build_tools: &'a dyn Fn() -> Result<Option<BuildToolsSummary>, SoldrError>,
    /// soldr#3806: provisions what `soldr cc --target` needs; `None` without
    /// `--target`.
    pub cross_tools: &'a dyn Fn() -> Result<Option<CrossToolsSummary>, SoldrError>,
}

/// Runs the ensure pipeline after bootstrap. In `--json` mode an early error
/// is never propagated bare (soldr#3693): it becomes a schema-v1 payload with
/// `error` set and `smoke_verify.ok = false`, and the exit code is 1.
pub(crate) fn run_ensure_pipeline(
    json: bool,
    started: Instant,
    bootstrap: Result<bool, SoldrError>,
    steps: &EnsureSteps<'_>,
    out: &mut dyn Write,
) -> Result<i32, SoldrError> {
    let mut partial = ToolchainEnsureOutput::empty();
    match ensure_steps(json, started, bootstrap, steps, &mut partial, out) {
        Ok(code) => Ok(code),
        Err(err) if json => {
            eprintln!("soldr toolchain ensure: {err}");
            partial.error = Some(err.to_string());
            partial.smoke_verify.ok = false;
            partial.elapsed_ms = started.elapsed().as_millis();
            emit_json(out, &partial)?;
            Ok(1)
        }
        Err(err) => Err(err),
    }
}

fn ensure_steps(
    json: bool,
    started: Instant,
    bootstrap: Result<bool, SoldrError>,
    steps: &EnsureSteps<'_>,
    partial: &mut ToolchainEnsureOutput,
    out: &mut dyn Write,
) -> Result<i32, SoldrError> {
    partial.rustup_bootstrapped = bootstrap?;

    // 2. Read the manifest. Missing manifest is not an error — emit the
    //    schema-v1 empty payload so consumers can still parse it.
    let manifest = (steps.load_manifest)()?;
    partial.channel = manifest.channel.clone();

    // 3. Run the prepare pipeline if a channel is declared. Otherwise
    //    skip it entirely (matching `prepare`'s no-channel behavior).
    let (prepare_code, prepare_summary) = if let Some(channel) = manifest.channel.as_deref() {
        (steps.prepare)(channel, &manifest)?
    } else {
        (0, PrepareSummary::default())
    };
    partial.components_added = prepare_summary.components_added;
    partial.targets_added = prepare_summary.targets_added;
    partial.plugins_installed = prepare_summary.plugins_installed;
    partial.linker = prepare_summary.linker;
    partial.build_tools = prepare_summary.build_tools;
    if manifest.channel.is_none() {
        // soldr#3802: no rustup work without a pin, but build tools are still
        // provisionable under an unpinned opt-in.
        partial.build_tools = (steps.unpinned_build_tools)()?;
    }
    if let Some(cross) = (steps.cross_tools)()? {
        eprintln!("{}", cross.render());
        let tools = partial
            .build_tools
            .get_or_insert_with(|| BuildToolsSummary {
                note: Some("native host build tools not provisioned".to_string()),
                ..BuildToolsSummary::default()
            });
        tools.provisioned = true;
        tools.cross = Some(cross);
    }

    // 4. If prepare reported a non-zero exit, surface it without running
    //    smoke verify (the toolchain is already broken).
    if prepare_code != 0 {
        if json {
            partial.elapsed_ms = started.elapsed().as_millis();
            emit_json(out, partial)?;
        } else {
            eprintln!("soldr toolchain ensure: prepare exited with status {prepare_code}");
        }
        return Ok(prepare_code);
    }

    // 5. Smoke verify only when a channel exists. Without a manifest
    //    there's no toolchain to validate against.
    partial.smoke_verify = if manifest.channel.is_some() {
        (steps.smoke)(manifest.targets.as_deref().unwrap_or(&[]))?
    } else {
        SmokeVerify {
            cargo_version: None,
            rustc_version: None,
            missing_std_targets: Vec::new(),
            // No channel means nothing to verify; treat as ok so the
            // exit code stays 0 (matches prepare's no-channel path).
            ok: true,
        }
    };
    partial.elapsed_ms = started.elapsed().as_millis();

    if json {
        emit_json(out, partial)?;
    } else {
        emit_human(partial);
    }

    // soldr#1059 — flag a shadowing standalone `cargo` on PATH. JSON
    // callers (setup-soldr#133) already pick this up through the
    // doctor probe; the warning here is for the human surface only,
    // emitted to stderr so JSON consumers ignore it.
    if !json {
        if let Some(finding) = crate::cargo_path_check::detect_cargo_on_path() {
            if let Some(msg) = crate::cargo_path_check::warning_for(&finding) {
                eprintln!("{msg}");
            }
        }
    }

    // Non-zero exit so shell-pipeline consumers (setup-soldr#133)
    // can detect the failure without parsing JSON.
    Ok(if partial.smoke_verify.ok { 0 } else { 1 })
}

async fn bootstrap_rustup_if_missing() -> Result<bool, SoldrError> {
    // Honor the test-only `SOLDR_TEST_RUSTUP_BIN` override: when set, we
    // are inside an integration test that has already pre-provisioned a
    // fake rustup. Skip the bootstrap branch entirely.
    if std::env::var_os(crate::TEST_RUSTUP_BIN_ENV_VAR).is_some_and(|v| !v.is_empty()) {
        return Ok(false);
    }

    let paths = SoldrPaths::new()?;
    match crate::fetch::auto_bootstrap_if_missing(&paths).await? {
        crate::fetch::AutoBootstrapOutcome::AlreadyInstalled(_) => Ok(false),
        crate::fetch::AutoBootstrapOutcome::Installed(_) => Ok(true),
        // Opt-out leaves the host unchanged. The subsequent prepare run
        // will surface the standard "rustup not found" message.
        crate::fetch::AutoBootstrapOutcome::OptedOut => Ok(false),
    }
}

fn unpinned_build_tools() -> Result<Option<BuildToolsSummary>, SoldrError> {
    if !crate::toolchain_prepare::unpinned_provisioning_allowed() {
        return Ok(None);
    }
    let summary = crate::toolchain_prepare::provision_build_tools()?;
    crate::toolchain_prepare::report_build_tools(&summary);
    Ok(Some(summary))
}

fn run_smoke_verify(declared_targets: &[String]) -> Result<SmokeVerify, SoldrError> {
    let cargo_version = probe_version("cargo");
    let rustc_version = probe_version("rustc");
    let missing_std_targets = missing_std_targets(declared_targets);
    let ok = cargo_version.is_some() && rustc_version.is_some() && missing_std_targets.is_empty();
    Ok(SmokeVerify {
        cargo_version,
        rustc_version,
        missing_std_targets,
        ok,
    })
}

/// Triples whose standard-library files are missing from the toolchain the
/// resolved `rustc` belongs to (soldr#3376). Reads files only, so a toolchain
/// whose `components` manifest still claims a deleted `rust-std` fails here
/// instead of at `E0463` in a later build. An unresolvable `rustc` reports
/// nothing: the version probe already fails `ok` in that case.
fn missing_std_targets(declared_targets: &[String]) -> Vec<String> {
    let Ok(rustc) = resolve_toolchain_binary("rustc") else {
        return Vec::new();
    };
    let Some(toolchain_dir) = rustc.parent().and_then(std::path::Path::parent) else {
        return Vec::new();
    };
    // Only a real toolchain layout is judged: a `rustc` that is not inside a
    // `lib/rustlib` tree (a wrapper, a test double) has no std files to check.
    if !toolchain_dir.join("lib").join("rustlib").is_dir() {
        return Vec::new();
    }
    let host = crate::toolchain_std_libs::host_of_toolchain_dir(
        toolchain_dir,
        crate::pyo3_detect::host_triple(),
    );
    crate::toolchain_std_libs::missing_std_triples(toolchain_dir, host.as_deref(), declared_targets)
}

/// Spawn `<tool> --version` and capture stdout. Returns `None` when
/// either the binary couldn't be resolved, the spawn failed, the exit
/// status was non-zero, or stdout was empty. The `None` case is what
/// drives `smoke_verify.ok = false`.
fn probe_version(tool: &str) -> Option<String> {
    let binary = resolve_toolchain_binary(tool).ok()?;
    let mut command = std::process::Command::new(&binary);
    command.arg("--version");
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &binary);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, &format!("{tool} --version")).ok()?;
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

fn emit_json(out: &mut dyn Write, output: &ToolchainEnsureOutput) -> Result<(), SoldrError> {
    let payload = serde_json::to_string_pretty(output)
        .map_err(|e| SoldrError::Other(format!("ensure: failed to serialize JSON: {e}")))?;
    writeln!(out, "{payload}").map_err(SoldrError::from)?;
    out.flush().map_err(SoldrError::from)
}

fn emit_human(output: &ToolchainEnsureOutput) {
    match output.channel.as_deref() {
        Some(channel) => println!("soldr toolchain ensure: channel {channel}"),
        None => println!(
            "soldr toolchain ensure: no rust-toolchain.toml channel found; \
             nothing to install or verify."
        ),
    }
    if output.rustup_bootstrapped {
        println!("soldr toolchain ensure: bootstrapped rustup into soldr-managed bin dir");
    }
    if !output.components_added.is_empty() {
        println!(
            "soldr toolchain ensure: components installed: {}",
            output.components_added.join(", ")
        );
    }
    if !output.targets_added.is_empty() {
        println!(
            "soldr toolchain ensure: targets installed: {}",
            output.targets_added.join(", ")
        );
    }
    if !output.plugins_installed.is_empty() {
        println!(
            "soldr toolchain ensure: plugins installed: {}",
            output.plugins_installed.join(", ")
        );
    }
    if let Some(cargo) = output.smoke_verify.cargo_version.as_deref() {
        println!("soldr toolchain ensure: smoke verify cargo: {cargo}");
    }
    if let Some(rustc) = output.smoke_verify.rustc_version.as_deref() {
        println!("soldr toolchain ensure: smoke verify rustc: {rustc}");
    }
    if output.smoke_verify.ok {
        println!("soldr toolchain ensure: smoke verify ok");
    } else if output.channel.is_some() {
        println!("soldr toolchain ensure: smoke verify FAILED");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn empty_smoke_verify_defaults_to_not_ok() {
        let s = SmokeVerify::default();
        assert!(!s.ok);
        assert!(s.cargo_version.is_none());
        assert!(s.rustc_version.is_none());
    }

    #[test]
    fn json_serialises_schema_version_first_and_arrays_present() {
        let output = ToolchainEnsureOutput {
            schema_version: SCHEMA_VERSION,
            channel: Some("1.94.1".to_string()),
            rustup_bootstrapped: false,
            components_added: vec!["clippy".to_string()],
            targets_added: vec![],
            plugins_installed: vec!["cargo-nextest@0.9".to_string()],
            linker: None,
            build_tools: None,
            smoke_verify: SmokeVerify {
                cargo_version: Some("cargo 1.94.1".to_string()),
                rustc_version: Some("rustc 1.94.1".to_string()),
                missing_std_targets: Vec::new(),
                ok: true,
            },
            elapsed_ms: 42,
            error: None,
        };
        let json = serde_json::to_string(&output).expect("serialise");
        let parsed: Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], Value::from(1));
        assert_eq!(parsed["channel"], Value::from("1.94.1"));
        assert!(parsed["targets_added"].is_array());
        assert_eq!(parsed["smoke_verify"]["ok"], Value::from(true));
        assert!(parsed["linker"].is_null());
    }

    /// soldr#3276 T4: the additive `linker` key must serialize as an object
    /// when the project selected `reld`, and as JSON `null` (not an omitted
    /// key) when it did not. Does not bump `schema_version`.
    #[test]
    fn linker_key_serializes_as_object_when_selected_else_null() {
        let with_linker = ToolchainEnsureOutput {
            schema_version: SCHEMA_VERSION,
            channel: Some("1.94.1".to_string()),
            rustup_bootstrapped: false,
            components_added: vec![],
            targets_added: vec![],
            plugins_installed: vec![],
            linker: Some(LinkerSummary {
                choice: "reld".to_string(),
                path: "/home/user/.soldr/bin/reld-0.1.0/reld".to_string(),
                version: "0.1.0".to_string(),
            }),
            build_tools: None,
            smoke_verify: SmokeVerify {
                cargo_version: Some("cargo 1.94.1".to_string()),
                rustc_version: Some("rustc 1.94.1".to_string()),
                missing_std_targets: Vec::new(),
                ok: true,
            },
            elapsed_ms: 7,
            error: None,
        };
        let json = serde_json::to_string(&with_linker).expect("serialise");
        let parsed: Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["linker"]["choice"], Value::from("reld"));
        assert_eq!(parsed["linker"]["version"], Value::from("0.1.0"));
        assert_eq!(parsed["schema_version"], Value::from(1));

        let without_linker = ToolchainEnsureOutput {
            linker: None,
            build_tools: None,
            ..without_linker_defaults()
        };
        let json = serde_json::to_string(&without_linker).expect("serialise");
        let parsed: Value = serde_json::from_str(&json).expect("parse");
        assert!(parsed["linker"].is_null());
        assert!(json.contains("\"linker\":null"));
    }

    /// soldr#3693: an early `prepare` error (e.g. a plugin spawn failure)
    /// must still yield a schema-v1 payload on stdout with an `error` field,
    /// `smoke_verify.ok = false`, and a non-zero exit.
    #[test]
    fn json_payload_emitted_when_prepare_errors() {
        let manifest = crate::core::RustToolchainManifest {
            channel: Some("1.94.1".to_string()),
            ..Default::default()
        };
        let steps = EnsureSteps {
            load_manifest: &|| Ok(manifest.clone()),
            prepare: &|_, _| {
                Err(SoldrError::Other(
                    "failed to spawn cargo install cargo-nextest".to_string(),
                ))
            },
            smoke: &|_| panic!("smoke verify must not run after prepare error"),
            unpinned_build_tools: &|| Ok(None),
            cross_tools: &|| Ok(None),
        };
        let mut out = Vec::new();
        let code = run_ensure_pipeline(true, Instant::now(), Ok(false), &steps, &mut out)
            .expect("json mode converts errors into a payload");
        assert_ne!(code, 0);
        let parsed: Value = serde_json::from_slice(&out).expect("stdout is JSON");
        assert_eq!(parsed["schema_version"], Value::from(1));
        assert_eq!(parsed["channel"], Value::from("1.94.1"));
        assert_eq!(parsed["smoke_verify"]["ok"], Value::from(false));
        assert!(parsed["error"]
            .as_str()
            .expect("error string")
            .contains("cargo-nextest"));
    }

    /// soldr#3693: a bootstrap failure is reported the same way.
    #[test]
    fn json_payload_emitted_when_bootstrap_errors() {
        let steps = EnsureSteps {
            load_manifest: &|| panic!("manifest must not be read after bootstrap error"),
            prepare: &|_, _| panic!("prepare must not run"),
            smoke: &|_| panic!("smoke must not run"),
            unpinned_build_tools: &|| Ok(None),
            cross_tools: &|| Ok(None),
        };
        let mut out = Vec::new();
        let code = run_ensure_pipeline(
            true,
            Instant::now(),
            Err(SoldrError::Other("rustup-init download failed".to_string())),
            &steps,
            &mut out,
        )
        .expect("payload");
        assert_ne!(code, 0);
        let parsed: Value = serde_json::from_slice(&out).expect("stdout is JSON");
        assert_eq!(parsed["schema_version"], Value::from(1));
        assert!(parsed["channel"].is_null());
        assert!(parsed["error"].as_str().unwrap().contains("rustup-init"));
    }

    /// soldr#3802: the additive `build_tools` key is `null` when provisioning
    /// did not run, and an object (with an explicit "nothing provisioned"
    /// shape) when it did. `schema_version` stays 1 and every prior key stays.
    #[test]
    fn build_tools_key_is_additive_null_or_object() {
        let absent = serde_json::to_string(&without_linker_defaults()).expect("serialise");
        let parsed: Value = serde_json::from_str(&absent).expect("parse");
        assert!(parsed["build_tools"].is_null());
        assert!(absent.contains("\"build_tools\":null"));
        assert_eq!(parsed["schema_version"], Value::from(1));

        let nothing = ToolchainEnsureOutput {
            build_tools: Some(BuildToolsSummary {
                provisioned: false,
                linker: None,
                cmake: false,
                cross: None,
                note: Some("linker: no soldr linker selected".to_string()),
            }),
            ..without_linker_defaults()
        };
        let parsed: Value =
            serde_json::from_str(&serde_json::to_string(&nothing).expect("serialise"))
                .expect("parse");
        assert_eq!(parsed["build_tools"]["provisioned"], Value::from(false));
        assert_eq!(parsed["build_tools"]["cmake"], Value::from(false));
        assert!(parsed["build_tools"]["linker"].is_null());
        assert!(parsed["build_tools"]["note"]
            .as_str()
            .is_some_and(|note| note.contains("no soldr linker")));
        assert!(parsed["linker"].is_null(), "the reld key is untouched");
    }

    /// Without a channel the ensure pipeline still provisions build tools
    /// when the caller opted into an unpinned build (soldr#3802).
    #[test]
    fn ensure_without_channel_reports_unpinned_build_tools() {
        let steps = EnsureSteps {
            load_manifest: &|| Ok(RustToolchainManifest::default()),
            prepare: &|_, _| panic!("no channel, no prepare"),
            smoke: &|_| panic!("no channel, no smoke"),
            unpinned_build_tools: &|| {
                Ok(Some(BuildToolsSummary {
                    provisioned: true,
                    cmake: true,
                    cross: None,
                    ..BuildToolsSummary::default()
                }))
            },
            cross_tools: &|| Ok(None),
        };
        let mut out = Vec::new();
        let code = run_ensure_pipeline(true, Instant::now(), Ok(false), &steps, &mut out)
            .expect("pipeline");
        assert_eq!(code, 0);
        let parsed: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(parsed["build_tools"]["cmake"], Value::from(true));
    }

    /// soldr#3806: `--target` reports the cc toolchain under the additive
    /// `build_tools.cross` key; the schema version does not move.
    #[test]
    fn ensure_with_target_reports_cross_tools_additively() {
        let steps = EnsureSteps {
            load_manifest: &|| Ok(RustToolchainManifest::default()),
            prepare: &|_, _| panic!("no channel, no prepare"),
            smoke: &|_| panic!("no channel, no smoke"),
            unpinned_build_tools: &|| Ok(None),
            cross_tools: &|| {
                Ok(Some(CrossToolsSummary {
                    target: "aarch64-apple-darwin".to_string(),
                    bin_dir: "/llvm/bin".to_string(),
                    sysroot: Some("/sdk".to_string()),
                    cc: "/drv/cc".to_string(),
                    cxx: "/drv/c++".to_string(),
                    ar: "/llvm/bin/llvm-ar".to_string(),
                    linker: "/drv/cc".to_string(),
                }))
            },
        };
        let mut out = Vec::new();
        let code = run_ensure_pipeline(true, Instant::now(), Ok(false), &steps, &mut out)
            .expect("pipeline");
        assert_eq!(code, 0);
        let parsed: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(parsed["schema_version"], Value::from(1));
        let cross = &parsed["build_tools"]["cross"];
        assert_eq!(cross["target"], Value::from("aarch64-apple-darwin"));
        assert_eq!(cross["sysroot"], Value::from("/sdk"));
        assert_eq!(cross["cc"], Value::from("/drv/cc"));
        assert_eq!(parsed["build_tools"]["provisioned"], Value::from(true));
    }

    /// Without `--target` the payload has no `cross` key at all.
    #[test]
    fn ensure_without_target_has_no_cross_key() {
        let summary = BuildToolsSummary::default();
        let json = serde_json::to_value(&summary).expect("serialise");
        assert!(json.get("cross").is_none(), "{json}");
    }

    fn without_linker_defaults() -> ToolchainEnsureOutput {
        ToolchainEnsureOutput {
            schema_version: SCHEMA_VERSION,
            channel: None,
            rustup_bootstrapped: false,
            components_added: vec![],
            targets_added: vec![],
            plugins_installed: vec![],
            linker: None,
            build_tools: None,
            smoke_verify: SmokeVerify::default(),
            elapsed_ms: 0,
            error: None,
        }
    }
}
