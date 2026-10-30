fn suggest_cargo_subcommand_typo(sub: &str) -> Option<String> {
    if crate::cli_args::is_cargo_builtin_verb(sub) {
        return None;
    }
    let known = crate::fetch::known_cargo_subcommands();
    crate::fuzzy_match::suggest_close_match(sub, &known).map(|s| s.to_string())
}

/// Env var name for the PATH-first override (issue #816). Reads as truthy
/// when set to a recognised on-spelling (`1`/`true`/`yes`/`on`). An
/// unrecognised value does not force (soldr#2740).
pub(crate) const FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR: &str =
    "SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS";

fn force_managed_cargo_subcommands() -> bool {
    match std::env::var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR) {
        Ok(value) => {
            crate::core::flag_value(&value)
        }
        Err(_) => false,
    }
}

/// The `cargo-<sub>` binary on `PATH` that `soldr cargo <sub>` defers to
/// instead of fetching, if any (issue #816).
///
/// Shared with `ci_test`'s policy prefetch (soldr#3143) so the prefetch skips
/// exactly the tools this bootstrap would not fetch. A copied rule would drift,
/// and the symptom would be silent: downloads nobody reads.
pub(crate) fn path_deferred_subcommand_tool(sub: &str) -> Option<std::path::PathBuf> {
    if force_managed_cargo_subcommands() {
        return None;
    }
    find_on_path(&format!("cargo-{sub}"))
}

/// The version `soldr cargo <sub>` fetches for a managed tool: its registry
/// pin, else the upstream latest release.
///
/// Shared with `ci_test`'s policy prefetch (soldr#3143): a prefetch only helps
/// if it lands the same cache entry the stage later resolves.
pub(crate) fn managed_subcommand_version(
    spec: &crate::fetch::known_tools::ToolSpec,
) -> VersionSpec {
    spec.pinned_version
        .map(|v| VersionSpec::Exact(v.to_string()))
        .unwrap_or(VersionSpec::Latest)
}

/// Walk `$PATH` looking for an executable named `tool`. Mirrors the
/// hand-rolled lookup in `core::toolchain_resolve::path_bin_dir` —
/// duplicated rather than re-exported to keep the cargo-front-door
/// independent of `core::toolchain_resolve`'s internals. On Windows the
/// `PATHEXT` suffix sweep matches what the toolchain resolver does so
/// `cargo-zigbuild.exe` is found even when the caller typed `cargo-zigbuild`.
fn find_on_path(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return Some(candidate);
        }
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            if std::path::Path::new(tool).extension().is_some() {
                continue;
            }
            let pathext = std::env::var_os("PATHEXT")
                .and_then(|value| value.into_string().ok())
                .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
            for suffix in pathext.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                let suffixed = dir.join(format!("{tool}{suffix}"));
                if suffixed.is_file() {
                    return Some(suffixed);
                }
            }
        }
    }
    None
}

/// Result of subcommand tool resolution: PATH-prepended bin dirs +
/// env-var overrides for the child cargo invocation.
pub(crate) struct SubcommandToolBootstrap {
    pub bin_dirs: Vec<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    pub cargo_args: Vec<String>,
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) async fn ensure_known_subcommand_tool(
    args: &[String],
    paths: &SoldrPaths,
) -> Result<SubcommandToolBootstrap, SoldrError> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Ok(SubcommandToolBootstrap {
            bin_dirs: Vec::new(),
            env: Vec::new(),
            cargo_args: Vec::new(),
        });
    };
    let Some(spec) = crate::fetch::lookup_by_cargo_subcommand(sub) else {
        // Issue #412: when the typed subcommand isn't in
        // `known_tools` but LOOKS like a typo of one that IS, drop a
        // "did you mean?" hint on stderr. We still return empty so the
        // underlying cargo invocation continues as today — the
        // suggestion is advisory and cargo's own external-command
        // dispatch may still find the tool on PATH.
        if let Some(suggestion) = suggest_cargo_subcommand_typo(sub) {
            eprintln!("soldr: '{sub}' is not a cargo subcommand soldr ships a prebuilt for.");
            eprintln!("soldr: did you mean: cargo {suggestion}?");
        }
        // soldr#3571: plain cargo verbs (`clippy`, `build`, `check`, `test`,
        // …) never reach `append_subcommand_transitive_bin_dirs` — this
        // early return fires first. But a configured lint crate's own
        // `.cargo/config.toml` still hands rustc `-C linker=dylint-link`
        // for exactly these invocations, so the managed binary must reach
        // the child PATH from here too.
        let mut bin_dirs = Vec::new();
        ensure_dylint_link_bin_dir(sub, args, paths, &mut bin_dirs).await?;
        return Ok(SubcommandToolBootstrap {
            bin_dirs,
            env: Vec::new(),
            cargo_args: Vec::new(),
        });
    };

    // Issue #816: if `cargo-<sub>` is already on PATH, defer to it instead
    // of running the managed fetch. This matches the discipline
    // `ensure_rustup_available` uses for rustup and avoids two failure modes:
    //   1. The managed fetcher writing an unrunnable artifact (the original
    //      #816 / #810 cargo-zigbuild bug, now fixed by xz2 extraction —
    //      but PATH-first is a structural belt-and-suspenders).
    //   2. Bypassing a user who deliberately installed a specific version
    //      via `cargo install <name>` or their distro package
    //      manager. cargo's own external-subcommand dispatch will find the
    //      PATH binary; soldr returning Ok(empty) here leaves that path
    //      open without prepending its own bin dir.
    // Escape hatch: SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1 forces the
    // managed fetch even when PATH has the tool — useful for CI runs that
    // want byte-identical pinned binaries.
    let mut extra_bin_dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut extra_env: Vec<(String, String)> = Vec::new();
    let mut extra_cargo_args: Vec<String> = Vec::new();

    let deferred = path_deferred_subcommand_tool(sub).filter(|path| {
        sub != "dylint" || path_dylint_matches_pin(path, spec.pinned_version.unwrap_or("unknown"))
    });
    if let Some(path) = deferred {
        let exe_name = format!("cargo-{sub}");
            if sub == "dylint" {
                let version = spec.pinned_version.unwrap_or("unknown");
                validate_dylint_path_binary(&path, "cargo-dylint", version)?;
                if let Some(link) = find_on_path("dylint-link") {
                    validate_dylint_path_binary(&link, "dylint-link", version)?;
                }
            }
            // Informational; on GitHub Actions it repeats once per nested
            // invocation (a Dylint cook runs dozens) and the override it
            // advertises is a workflow-level decision, not a per-line one.
            if !foreign_env_flag("GITHUB_ACTIONS") {
                eprintln!(
                    "soldr: deferring to {exe_name} on PATH at {} (set SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1 to override)",
                    path.display()
                );
            }
            // Even when cargo-zigbuild is provided by the host, it
            // still shells out to `zig`. Run the transitive bootstrap
            // before returning so the deferred-on-PATH branch doesn't
            // silently regress.
            append_subcommand_transitive_bin_dirs(
                sub,
                args,
                paths,
                &mut extra_bin_dirs,
                &mut extra_env,
                &mut extra_cargo_args,
            )
            .await?;
            return Ok(SubcommandToolBootstrap {
                bin_dirs: extra_bin_dirs,
                env: extra_env,
                cargo_args: extra_cargo_args,
            });
    }

    let version = managed_subcommand_version(spec);

    // Progress chatter for a human at a terminal only (soldr#3099): under
    // `soldr ci-test` and the Dylint cook these lines repeated once per
    // nested invocation into the CI log. A real download still reports
    // itself below. Same rule as the External-tool path in
    // `soldr_main_dispatch`.
    let chatty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if chatty {
        eprintln!("soldr: fetching {}...", spec.crate_name);
    }
    let result = match crate::fetch::fetch_tool_for_host_with_paths(
        spec.crate_name,
        &version,
        paths,
    )
    .await
    {
        Ok(result) => result,
        Err(error) if sub == "dylint" => {
            return Err(dylint_unavailable_error(
                "cargo-dylint",
                spec.pinned_version.unwrap_or("unknown"),
                &error,
            ));
        }
        Err(error) => return Err(error),
    };

    if result.cached {
        if chatty {
            eprintln!(
                "soldr: using cached {} v{}",
                spec.crate_name, result.version
            );
        }
    } else {
        eprintln!("soldr: downloaded {} v{}", spec.crate_name, result.version);
    }

    let dir = result
        .binary_path
        .parent()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to resolve bin dir for fetched {}",
                spec.crate_name
            ))
        })?
        .to_path_buf();
    extra_bin_dirs.push(dir);
    append_subcommand_transitive_bin_dirs(
        sub,
        args,
        paths,
        &mut extra_bin_dirs,
        &mut extra_env,
        &mut extra_cargo_args,
    )
    .await?;
    Ok(SubcommandToolBootstrap {
        bin_dirs: extra_bin_dirs,
        env: extra_env,
        cargo_args: extra_cargo_args,
    })
}

/// soldr#3571: prepend the managed `dylint-link` bin dir to the child
/// cargo's PATH when this invocation will actually need it.
///
/// Two triggers share one fetch so they cannot drift:
///   * `sub == "dylint"` — the historical `cargo dylint` condition,
///     unchanged (its gate below is byte-equivalent to the old
///     `force_managed || find_on_path("dylint-link").is_none()`).
///   * any other subcommand — only when the project's own cargo config
///     drives rustc with a bare `dylint-link`
///     ([`project_config_needs_dylint_link`]). That is a lint crate's
///     standalone `soldr cargo clippy`: its `.cargo/config.toml` declares
///     `[target.'cfg(all())'] rustflags = ["-C", "linker=dylint-link"]`,
///     and without this dir on the child PATH rustc dies with
///     `linker dylint-link not found` before a single lint runs.
///
/// An ordinary project declares no `dylint-link` anywhere, so this never
/// downloads Dylint tools merely because Clippy ran. When the fetch does
/// fire it mirrors the `cargo dylint` path exactly: pinned prebuilt from
/// cache or release, smoke-tested, and skipped entirely when a `dylint-link`
/// is already resolvable on PATH (or
/// `SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1` demands the managed one).
async fn ensure_dylint_link_bin_dir(
    sub: &str,
    args: &[String],
    paths: &SoldrPaths,
    extra_bin_dirs: &mut Vec<std::path::PathBuf>,
) -> Result<(), SoldrError> {
    let needed = sub == "dylint" || project_config_needs_dylint_link(args);
    if !needed {
        return Ok(());
    }
    if !force_managed_cargo_subcommands() && find_on_path("dylint-link").is_some() {
        return Ok(());
    }
    extra_bin_dirs.push(dylint_link_bin_dir(paths).await?);
    Ok(())
}

/// soldr#3571: whether this invocation's own cargo config resolves to a
/// bare `dylint-link` linker for the target being built.
///
/// The config walk mirrors `linker::resolve_project_choice_from_cwd`
/// exactly — same project-root discovery from the current directory, same
/// `$CARGO_HOME` fallback, and the declared Dylint library roots layered
/// ahead of the project root only while the Dylint scope is active
/// (soldr#3483: outside the scope Cargo would not read a lint package's
/// `.cargo/config.toml` for this invocation either, so neither do we).
///
/// The target resolves as `--target` → `CARGO_BUILD_TARGET` → the
/// compile-time host triple. Unlike the front door's linker injection
/// (`target::resolve_active_target_triple`) this deliberately does not pay
/// a second `rustc --print target-triple` probe per invocation: it runs
/// before every cargo verb, and the detection that matters —
/// `[target.'cfg(all())']`, the shape every lint crate declares — needs no
/// triple at all. The only divergence from the injected-linker resolution
/// is an exact-triple section under a `build.target` override with no
/// `--target` on the command line; a lint crate pins nothing of the kind
/// (its cdylib links for the host, soldr#2350).
fn project_config_needs_dylint_link(args: &[String]) -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    let root = crate::linker::project_root(&cwd);
    let lint_roots = if std::env::var_os(crate::dylint_toolchain::TOOLCHAIN_ENV_VAR).is_some() {
        crate::dylint_libraries::library_directories(&root).unwrap_or_default()
    } else {
        Vec::new()
    };
    let target = target::known_cargo_build_target(args, None).or_else(|| {
        crate::core::TargetTriple::host()
            .ok()
            .map(|triple| triple.triple())
    });
    crate::linker::project_config_declares_dylint_link(
        target.as_deref(),
        &root,
        crate::core::resolve_cargo_home().as_deref(),
        &lint_roots,
    )
}

async fn dylint_link_bin_dir(paths: &SoldrPaths) -> Result<std::path::PathBuf, SoldrError> {
    let pinned_version = crate::fetch::known_tools::lookup_by_crate("dylint-link")
        .and_then(|spec| spec.pinned_version)
        .ok_or_else(|| SoldrError::Other("dylint-link must have a registry pin".into()))?;
    let version = VersionSpec::Exact(pinned_version.to_string());
    let chatty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if chatty {
        eprintln!("soldr: fetching dylint-link...");
    }
    match crate::fetch::fetch_tool_for_host_with_paths("dylint-link", &version, paths).await {
        Ok(result) => {
            if let Err(error) = validated_dylint_link_prebuilt(&result) {
                return Err(dylint_unavailable_error(
                    "dylint-link",
                    pinned_version,
                    &error,
                ));
            }
            if result.cached {
                if chatty {
                    eprintln!("soldr: using cached dylint-link v{}", result.version);
                }
            } else {
                eprintln!("soldr: downloaded dylint-link v{}", result.version);
            }
            result
                .binary_path
                .parent()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| {
                    SoldrError::Other(format!(
                        "failed to resolve bin dir for fetched dylint-link: {}",
                        result.binary_path.display()
                    ))
                })
        }
        Err(error) => Err(dylint_unavailable_error(
            "dylint-link",
            pinned_version,
            &error,
        )),
    }
}

fn validated_dylint_link_prebuilt(result: &crate::fetch::FetchResult) -> Result<(), SoldrError> {
    let target = crate::core::TargetTriple::host()?;
    crate::fetch::smoke_test_or_evict(&result.binary_path, "dylint-link", &target)
}

fn dylint_unavailable_error(component: &str, version: &str, error: &SoldrError) -> SoldrError {
    let host = crate::core::TargetTriple::host()
        .map(|target| target.triple().to_string())
        .unwrap_or_else(|_| "unknown-host".to_string());
    SoldrError::Other(format!(
        "Dylint v{version} is not built for this machine (host: {host}; missing or unusable \
         component: {component}). Soldr will not build Dylint from source. Cause: {error}. \
         Corrective action: install compatible Dylint v{version} binaries for {host} on PATH \
         (and dylint-driver under DYLINT_DRIVER_PATH), publish matching prebuilt release assets, \
         or select a Dylint version that provides {host} prebuilts."
    ))
}

/// A PATH `cargo-dylint` of another release than the pin builds lint
/// libraries its paired driver cannot load (6.0.3 against a 6.2.0 driver
/// reports "Could not find `lib<lint>@<toolchain>.so` despite successful
/// build"). Such a binary is skipped in favour of the managed pin.
fn path_dylint_matches_pin(binary: &Path, pinned: &str) -> bool {
    let mut command = std::process::Command::new(binary);
    command.args(["dylint", "--version"]);
    suppress_windows_console_window(&mut command);
    let reported = crate::core::tool_output::capture_small_tool(
        &mut command,
        "cargo-dylint dylint --version",
        Some(Duration::from_secs(2)),
    )
    .ok()
    .filter(|output| output.status.success())
    .and_then(|output| {
        crate::dylint_driver::dylint_driver_version(&String::from_utf8_lossy(&output.stdout))
            .map(str::to_owned)
    });
    if reported.as_deref() == Some(pinned) {
        return true;
    }
    eprintln!(
        "soldr: WARNING: ignoring cargo-dylint {} on PATH at {}; Soldr pins Dylint v{pinned} \
         and uses its managed copy. Remove the stale install with \
         `soldr cargo uninstall cargo-dylint dylint-link`.",
        reported.as_deref().unwrap_or("<unknown version>"),
        binary.display()
    );
    false
}

fn validate_dylint_path_binary(
    binary: &Path,
    component: &str,
    version: &str,
) -> Result<(), SoldrError> {
    let host = crate::core::TargetTriple::host()?;
    if component == "dylint-link" {
        return validate_dylint_link_path_binary(binary, version, &host);
    }
    let mut failures = Vec::new();
    for argument in ["--version", "--help"] {
        let mut command = std::process::Command::new(binary);
        command.arg(argument);
        suppress_windows_console_window(&mut command);
        // soldr#3382: bounded, output-capturing probe (same 2 s wall-clock
        // bound as before). Output is forwarded + logged by the shared
        // small-tool helper and the failure names the component's stderr.
        match crate::core::tool_output::capture_small_tool(
            &mut command,
            &format!("{component} {argument}"),
            Some(Duration::from_secs(2)),
        ) {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => failures.push(format!(
                "{argument} exited with {} (stderr: {})",
                output.status,
                crate::core::tool_output::stderr_excerpt(&output)
            )),
            Err(error) => failures.push(format!("{argument} probe failed: {error}")),
        }
    }
    Err(dylint_unavailable_error(
        component,
        version,
        &SoldrError::Other(format!(
            "PATH binary at {} failed bounded validation: {}",
            binary.display(),
            failures.join("; ")
        )),
    ))
}

/// Validate a PATH-supplied `dylint-link` (soldr#3274).
///
/// `dylint-link` is a transparent linker wrapper: it forwards its arguments to
/// the platform linker, so generic `--version` / `--help` probes are rejected
/// by a *correct* binary (MSVC `link.exe` exits non-zero and prints its banner
/// plus `usage: LINK`). #2468 named this defect; the managed smoke test in
/// `soldr-fetch` already handles it, and this validator now uses that one
/// predicate instead of a second, weaker rule. The output must be captured for
/// the banner to be readable at all — the previous implementation piped both
/// streams to `Stdio::null()`, so it could not have passed under any status.
///
/// The #2432 binary-or-exit-1 invariant is preserved: a pair that neither
/// exits 0 nor produces the MSVC banner + usage still fails with the
/// actionable `dylint_unavailable_error` diagnostic.
fn validate_dylint_link_path_binary(
    binary: &Path,
    version: &str,
    host: &crate::core::TargetTriple,
) -> Result<(), SoldrError> {
    let mut failures = Vec::new();
    for argument in ["--version", crate::fetch::smoke_help_argument("dylint-link")] {
        let mut command = std::process::Command::new(binary);
        command.arg(argument);
        if let Some(toolchain) = crate::fetch::smoke_rustup_toolchain("dylint-link", host) {
            // Identical to the string this branch set inline before
            // soldr#3274 (`nightly-<host triple>`), now sourced from the one
            // function that defines it.
            command.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        suppress_windows_console_window(&mut command);
        // Bounded, output-capturing probe. `command_output_with_timeout_duration`
        // is soldr's sanctioned wall-clock containment for small host probes:
        // it pipes and drains both streams on reader threads (no pipe-buffer
        // deadlock) and kills + reaps the child at the deadline.
        match crate::core::tool_output::capture_small_tool(
            &mut command,
            &format!("dylint-link {argument}"),
            Some(Duration::from_secs(2)),
        ) {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output)
                if crate::fetch::dylint_link_help_output_is_valid(
                    output.status.code(),
                    &output.stdout,
                    &output.stderr,
                ) =>
            {
                return Ok(());
            }
            Ok(output) => failures.push(format!(
                "{argument} exited with {} without a linker banner (output: {})",
                output.status,
                dylint_link_probe_excerpt(&output.stdout, &output.stderr)
            )),
            Err(error) => failures.push(format!("{argument} probe failed: {error}")),
        }
    }
    Err(dylint_unavailable_error(
        "dylint-link",
        version,
        &SoldrError::Other(format!(
            "PATH binary at {} failed bounded validation: {}",
            binary.display(),
            failures.join("; ")
        )),
    ))
}

/// A short, single-line excerpt of a probe's output for the diagnostic. The
/// full MSVC banner is multi-line and the message must stay actionable.
fn dylint_link_probe_excerpt(stdout: &[u8], stderr: &[u8]) -> String {
    let combined = format!(
        "{} {}",
        String::from_utf8_lossy(stdout).trim(),
        String::from_utf8_lossy(stderr).trim()
    );
    let flattened = combined.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.is_empty() {
        return "<no output>".to_string();
    }
    flattened.chars().take(200).collect()
}

// Pick the cargo-dylint binary to use given the outcome of the managed
// prebuilt fetch. Source compilation is intentionally never consulted.
//
// Split out of `ensure_known_subcommand_tool` so the binary-only policy is
// unit-testable without a network round-trip: the fetch outcome is an
// already-resolved `Result` and the forbidden source build is a closure.
//
// The failure this exists for is a *smoke-test* failure, not a download
// failure. `fetch_tool_for_host_with_paths` runs `--version` on the
// extracted binary (soldr#936, `smoke_test_or_evict`) and evicts it on a
// non-zero exit, so an incompatible upstream Dylint asset download
// fine on Debian 12 and then fails the probe with a loader error — which
// is exactly the `Err` arm below.
