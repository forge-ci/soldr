//! `soldr cook` — cargo-chef-style content-addressable dep prebuild.
//!
//! This is a thin shim that:
//! 1. Validates that `Cargo.toml` (and ideally `Cargo.lock`) live in the
//!    cwd, so cargo-chef has something to read.
//! 2. Resolves the pinned `cargo-chef` binary via the standard fetch
//!    pipeline (registry entry in `crate::fetch::known_tools`).
//! 3. Routes `cargo chef prepare` and `cargo chef cook` through the
//!    existing cargo front door so the underlying compile picks up
//!    zccache (RUSTC_WRAPPER), `ZCCACHE_PATH_REMAP=auto`, the soldr
//!    linker selection, the soldr-managed CARGO_HOME / RUSTUP_HOME, and
//!    every other piece of `soldr cargo` plumbing.
//!
//! See issue zackees/soldr#359 for design context and the companion
//! `zackees/setup-soldr#110` for the GitHub Action that consumes the
//! resulting `target/` tarball.

use crate::cache_lib::cook_archive::{self, cook_cache_dir, sha_abbrev, PackedCookArchive};
use crate::cache_lib::strip_target::{strip_target, StripTargetOptions};
use crate::cargo_front_door;
use crate::core::git::{
    cargo_lock_is_gitignored, cargo_lock_is_tracked, current_branch_name, find_git_worktree_root,
    origin_url,
};
use crate::core::{
    probe_toolchain_binary, read_rust_toolchain_manifest, SoldrError, SoldrPaths, TargetTriple,
};
use crate::daemon::client::{self, CookLookupOutcome};
use std::path::{Path, PathBuf};

// soldr#3043 integration: the project-source snapshot/restore helpers moved to
// their own module to keep this file under the 1,000-line production ceiling.
// Re-exported so `crate::cook::…` paths (dylint_cook.rs, cook_tests.rs) are
// unchanged.
pub(crate) use crate::cook_source_snapshot::{
    restore_project_source, snapshot_project_source, ProjectSourceSnapshot,
};
use std::time::Instant;

/// Parsed `soldr cook` invocation surface. Mirrors the relevant subset of
/// `cargo build` knobs plus the cargo-chef-specific recipe controls.
#[derive(Debug, Clone, Default)]
pub(crate) struct CookArgs {
    /// Compile dependencies in release mode (`cargo chef cook --release`).
    pub release: bool,
    /// Compile against a non-default target triple. Forwarded to cargo-chef
    /// as `--target <triple>`.
    pub target: Option<String>,
    /// Build the whole workspace. Forwarded to cargo-chef as `--workspace`.
    pub workspace: bool,
    /// Explicit cargo profile (`--profile <name>`). Forwarded to cargo-chef.
    pub profile: Option<String>,
    /// Restrict the cook step to a specific package (`--package`/`-p`).
    pub packages: Vec<String>,
    /// Override the recipe path. Defaults to a per-invocation temp dir so
    /// the recipe is invisible to the project.
    pub recipe_path: Option<PathBuf>,
    /// When set, retain the recipe at `<cwd>/recipe.json` (or at the value
    /// of `--recipe-path` when both are supplied) so the user can inspect
    /// it. Without this the recipe is deleted on exit.
    pub keep_recipe: bool,
    /// Only run `cargo chef prepare`. Useful for Docker users who want the
    /// recipe layer ahead of the heavy `cook` step.
    pub prepare_only: bool,
    /// Only run `cargo chef cook` against an existing recipe. Requires
    /// `--recipe-path`.
    pub cook_only: bool,
    /// Catch-all for tokens past `--`. Forwarded verbatim to
    /// `cargo chef cook` so users can pass `--features foo` etc.
    pub passthrough: Vec<String>,
    /// Skip the post-cook target/ trim (issue #459). Default `false`:
    /// cook always trims cargo-recreatable noise so the downstream
    /// tarball ships dramatically fewer bytes.
    pub no_trim: bool,
    /// Skip Phase 4, the cross-repo cook-index pack (soldr#3499). On an
    /// ephemeral CI runner the index starts empty and dies with the VM, so
    /// the pack (100-240 s for a ~120 MiB target) can never be reused. Also
    /// set by `SOLDR_COOK_INDEX=0`.
    pub no_index: bool,
}

/// `SOLDR_COOK_INDEX` — an off value (`0`/`false`/`off`/`no`) skips the
/// cook-index pack, like `--no-index`.
pub(crate) const COOK_INDEX_ENV_VAR: &str = "SOLDR_COOK_INDEX";

/// Whether Phase 4 runs: on unless `--no-index` or an off `SOLDR_COOK_INDEX`.
pub(crate) fn cook_index_wanted(no_index_flag: bool, env_value: Option<&str>) -> bool {
    !no_index_flag && !env_value.is_some_and(crate::core::is_off_value)
}

#[derive(Debug, Clone)]
pub(crate) struct CookContext {
    /// Resolved workspace root (cwd or the nearest ancestor with a
    /// `Cargo.toml`). Surfaced for tests and for future flags that need
    /// to anchor paths relative to the manifest.
    #[allow(dead_code)]
    pub manifest_dir: PathBuf,
    pub recipe_path: PathBuf,
    /// True when the recipe lives inside a `tempfile::TempDir` that we own
    /// and should clean up on exit (unless `--keep-recipe` is set).
    pub recipe_owned_tempdir: bool,
}

/// Parse a Vec<String> argv (everything after `cook`) into a `CookArgs`.
///
/// Recognised flags: `--release`, `--target <triple>`, `--workspace`,
/// `--profile <name>`, `-p`/`--package <name>` (repeatable),
/// `--recipe-path <path>`, `--keep-recipe`, `--prepare-only`,
/// `--cook-only`, `--no-trim`, `--no-index`. Anything after a literal `--` is collected into
/// `passthrough`. Unknown flags before `--` are an error (so typos fail
/// fast).
pub(crate) fn parse_cook_args(args: &[String]) -> Result<CookArgs, SoldrError> {
    let mut out = CookArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--release" => out.release = true,
            "--workspace" | "--all" => out.workspace = true,
            "--keep-recipe" => out.keep_recipe = true,
            "--prepare-only" => out.prepare_only = true,
            "--cook-only" => out.cook_only = true,
            "--no-trim" => out.no_trim = true,
            "--no-index" => out.no_index = true,
            "--target" => {
                let value = iter
                    .next()
                    .ok_or_else(|| SoldrError::Other("--target requires a value".into()))?;
                out.target = Some(value.clone());
            }
            v if v.starts_with("--target=") => {
                out.target = Some(v.trim_start_matches("--target=").to_string());
            }
            "--profile" => {
                let value = iter
                    .next()
                    .ok_or_else(|| SoldrError::Other("--profile requires a value".into()))?;
                out.profile = Some(value.clone());
            }
            v if v.starts_with("--profile=") => {
                out.profile = Some(v.trim_start_matches("--profile=").to_string());
            }
            "--recipe-path" => {
                let value = iter
                    .next()
                    .ok_or_else(|| SoldrError::Other("--recipe-path requires a value".into()))?;
                out.recipe_path = Some(PathBuf::from(value));
            }
            v if v.starts_with("--recipe-path=") => {
                out.recipe_path = Some(PathBuf::from(v.trim_start_matches("--recipe-path=")));
            }
            "-p" | "--package" => {
                let value = iter
                    .next()
                    .ok_or_else(|| SoldrError::Other("--package requires a value".into()))?;
                out.packages.push(value.clone());
            }
            v if v.starts_with("--package=") => {
                out.packages
                    .push(v.trim_start_matches("--package=").to_string());
            }
            "--" => {
                out.passthrough.extend(iter.by_ref().cloned());
                break;
            }
            other if other.starts_with('-') => {
                return Err(SoldrError::Other(format!(
                    "soldr cook: unknown flag `{other}` — pass project-specific options after `--`"
                )));
            }
            other => {
                // Positional pre-`--` tokens go to the passthrough bag too;
                // cargo-chef cook ignores most of them but we want to be
                // generous (e.g. `--features foo` written without `--`).
                out.passthrough.push(other.to_string());
            }
        }
    }

    if out.prepare_only && out.cook_only {
        return Err(SoldrError::Other(
            "soldr cook: --prepare-only and --cook-only are mutually exclusive".into(),
        ));
    }
    if out.cook_only && out.recipe_path.is_none() {
        return Err(SoldrError::Other(
            "soldr cook: --cook-only requires --recipe-path so cargo-chef can read the recipe"
                .into(),
        ));
    }

    Ok(out)
}

/// Locate the workspace manifest by walking up from the cwd. Emits an
/// actionable error when `Cargo.toml` is missing entirely.
pub(crate) fn resolve_manifest_dir(start: &Path) -> Result<PathBuf, SoldrError> {
    let mut current = start.to_path_buf();
    loop {
        if current.join("Cargo.toml").is_file() {
            return Ok(current);
        }
        if !current.pop() {
            return Err(SoldrError::Other(format!(
                "soldr cook: no Cargo.toml found at or above {} — run from a Rust project root",
                start.display()
            )));
        }
    }
}

/// Build the [`CookContext`] used by [`run_cook`]. Pure enough to unit-test:
/// takes the resolved cwd, parsed args, and (for tests) a hook that creates
/// the tempdir backing an ephemeral recipe.
pub(crate) fn build_cook_context(
    cwd: &Path,
    args: &CookArgs,
) -> Result<(CookContext, Option<tempfile::TempDir>), SoldrError> {
    let manifest_dir = resolve_manifest_dir(cwd)?;

    let lockfile = manifest_dir.join("Cargo.lock");
    if !args.cook_only && !lockfile.is_file() {
        eprintln!(
            "soldr cook: warning: {} is missing — cargo-chef will derive the recipe from Cargo.toml alone, which weakens content-addressability",
            lockfile.display()
        );
    }

    let (recipe_path, tempdir, owned_tempdir) = match (args.recipe_path.as_ref(), args.keep_recipe)
    {
        (Some(path), _) => {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                manifest_dir.join(path)
            };
            (absolute, None, false)
        }
        (None, true) => {
            // --keep-recipe without --recipe-path: drop the recipe next to
            // the project's Cargo.toml so it's easy to spot.
            (manifest_dir.join("recipe.json"), None, false)
        }
        (None, false) => {
            let tmp = tempfile::tempdir().map_err(|e| {
                SoldrError::Other(format!("soldr cook: failed to create temp dir: {e}"))
            })?;
            let recipe = tmp.path().join("recipe.json");
            (recipe, Some(tmp), true)
        }
    };

    Ok((
        CookContext {
            manifest_dir,
            recipe_path,
            recipe_owned_tempdir: owned_tempdir,
        },
        tempdir,
    ))
}

/// Exit code for a cook that was skipped because the workspace is not
/// cookable (soldr#2788).
///
/// Non-zero on purpose, and the reason is the consumer rather than taste:
/// `setup-soldr/cook` sets `cookRan = runRes.exitCode === 0` and saves a cache
/// layer when it is true. Returning 0 for a skip would make it save a layer
/// holding nothing, poisoning the key for every later run -- strictly worse
/// than the silent no-op this replaces. Non-zero keeps today's behaviour
/// (`ran=false`, no layer saved); all that changes is that it now costs
/// milliseconds and explains itself.
const COOK_SKIPPED_UNCOOKABLE_WORKSPACE: i32 = 3;

/// Top-level dispatch. Invoked from `Commands::Cook` in `main.rs`.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) async fn run_cook(args: &[String], cache_enabled: bool) -> Result<i32, SoldrError> {
    let parsed = parse_cook_args(args)?;
    let cwd = std::env::current_dir()
        .map_err(|e| SoldrError::Other(format!("soldr cook: failed to read cwd: {e}")))?;
    let (ctx, _tempdir_guard) = build_cook_context(&cwd, &parsed)?;
    // soldr#3518: undo a previous cook killed mid-skeleton before `prepare`
    // reads (and hashes) the manifests.
    crate::cook_source_journal::recover_stale_cook_journals(&ctx.manifest_dir)?;

    // soldr#3043: publish this cook's target scope to the cargo front door
    // before ANY phase runs. Phase 1 below is `cargo chef prepare`, whose argv
    // cannot carry `--target` (cargo-chef's `prepare` takes no such flag), yet
    // it is "build-like" enough that the front door runs its cook-index
    // pre-flight hydrate. With only the argv to read, that hydrate would
    // extract a `--target X` archive into the bare `target/` root — a full
    // duplicate extraction into a directory Cargo never reads for a `--target`
    // build, and the restored warm-cook marker would land outside
    // `resolve_cook_target_dir`, so the #621 short-circuit checked below could
    // never fire on a hydrated tree. Left unset for a `--target`-less cook, so
    // that path keeps its existing bare-`target/` behaviour exactly.
    if let Some(triple) = parsed.target.as_deref() {
        std::env::set_var(
            crate::cargo_front_door::cook_hydrate::SOLDR_COOK_HYDRATE_TARGET_ENV,
            triple,
        );
    }

    // Same environment `soldr build --target` applies, before any phase runs,
    // so cooked units match the build's fingerprints.
    let target_prep = crate::cook_target_prep::apply_cook_target_prep(&parsed).await?;

    let watchdog_config = crate::cook_watchdog::WatchdogConfig::from_env();
    let watchdog_paths = SoldrPaths::new()?;

    // Phase 1: prepare. Cheap, deterministic, reads only the manifest tree.
    if !parsed.cook_only {
        let prepare_args = build_chef_prepare_args(&ctx);
        let prepare_target_dir = resolve_cook_target_dir(&ctx.manifest_dir, &parsed);
        let code = crate::cook_watchdog::run_with_watchdog(
            "prepare",
            prepare_target_dir,
            &watchdog_paths,
            watchdog_config,
            cargo_front_door::run_cargo_front_door(&prepare_args, cache_enabled, false),
        )
        .await?;
        if code != 0 {
            return Ok(code);
        }
    }

    let sanitized = sanitize_cargo_chef_recipe(&ctx.recipe_path)?;
    if sanitized.plugin_keys_removed > 0 {
        eprintln!(
            "soldr cook: removed {} generated cargo-chef plugin manifest keys from recipe",
            sanitized.plugin_keys_removed
        );
    }
    if sanitized.path_dependencies_rewritten + sanitized.patches_removed > 0 {
        eprintln!(
            "soldr cook: redirected {} excluded path dependencies and {} patches to their published sources for dependency cooking",
            sanitized.path_dependencies_rewritten,
            sanitized.patches_removed
        );
    }
    if parsed.prepare_only {
        eprintln!(
            "soldr cook: wrote recipe to {} (--prepare-only)",
            ctx.recipe_path.display()
        );
        return Ok(0);
    }

    // soldr#2788: refuse a cook that still cannot succeed, instead of spending
    // ~190s discovering it. A crate depending on a path outside the
    // skeleton (a sibling repo vendored as a submodule with its own
    // `[workspace]`, so it lands in `exclude` rather than `members`) is
    // compiled against an `--extern` nothing produced. Cook then degrades
    // with "continuing without cooked deps" -- the build passes, no layer
    // is saved, and every later run repeats the full cold build. Silent.
    //
    // Skipping with a named reason keeps the loss visible. Actually
    // cooking these workspaces is soldr#2791.
    if let Ok(raw) = std::fs::read_to_string(&ctx.recipe_path) {
        if let Ok(recipe) = serde_json::from_str::<serde_json::Value>(&raw) {
            let blocked = unmaterializable_path_deps(&recipe);
            if !blocked.is_empty() {
                let parts: Vec<String> = blocked
                    .iter()
                    .map(|(dep, owner)| format!("{dep} (required by {owner})"))
                    .collect();
                let detail = parts.join(", ");
                let plural = if blocked.len() == 1 { "y" } else { "ies" };
                let count = blocked.len();
                eprintln!(
                        "soldr cook: skipped - workspace depends on {count} path dependenc{plural} the cargo-chef recipe cannot materialize: {detail}. \
Dependencies were NOT prebuilt and no cache layer was saved; the build \
proceeds uncached. See https://github.com/zackees/soldr/issues/2791"
                    );
                // soldr#2802: the same code the layout preflight returns.
                //
                // This used to be `Ok(0)`, which contradicted the line
                // directly above it. `setup-soldr/cook` derives the save
                // decision from the exit code --
                //
                //   cookRan   = runRes.exitCode === 0;
                //   saveLayer = cookRan ? (baseReady ? "delta" : "base") : "none";
                //
                // -- so exit 0 saves a layer holding nothing cooked and
                // poisons that key for every later run, while the message
                // claims no layer was saved. Non-zero yields `saveLayer =
                // "none"`, which is what the message describes.
                //
                // `fail-on-error` defaults to false in that action, so this
                // does not fail the step; a consumer who opts into it gets a
                // hard failure on a deliberate skip, which is the same trade
                // the layout preflight already makes.
                crate::exit_guard::mark_spoke();
                return Ok(COOK_SKIPPED_UNCOOKABLE_WORKSPACE);
            }
        }
    }

    // Snapshot the project's source-defining files (manifests + Rust sources)
    // before the cook compile. `cargo chef cook` reconstructs the cargo-chef
    // skeleton IN PLACE — it stubs every crate root (`fn main() {}` / empty
    // lib) and normalizes every crate version to `0.0.1` — and does NOT
    // restore the real project. Left unrestored, any build run after
    // `soldr cook` in the same job compiles the stub source under the wrong
    // (`0.0.1`) version, which silently breaks correctness AND churns the
    // compile-cache key for first-party crates (different `-C metadata` than a
    // warm run where cook's cache hits and the tree is pristine) — see
    // zackees/soldr#566, zackees/zccache#448.
    // soldr#3518: the snapshot is journaled to disk (fsync) before chef
    // mutates anything, so a killed cook is undone by the next invocation.
    let source_guard = crate::cook_source_journal::CookSourceGuard::begin(&ctx.manifest_dir)?;

    // #621: skip-cook-when-warm. After Phase 1 (prepare) the recipe is
    // fully resolved. If a previous successful cook left a marker file
    // under target/ recording the same recipe + rustc version, the
    // restored target/ from cook-cache covers the entire dep set and
    // Phase 2 would do no net work — cargo-chef would still spend ~5
    // minutes walking the dep graph emitting "Compiling X" lines per
    // crate, with zccache hitting at ~96% (the orchestration cost is
    // the bottleneck, not codegen). When the marker matches, short-
    // circuit Phase 2 entirely.
    let cook_target_dir = resolve_cook_target_dir(&ctx.manifest_dir, &parsed);
    let cook_marker_path = cook_target_dir.join(COOK_MARKER_FILE_NAME);
    let expected_marker = compute_cook_marker(&ctx, &parsed);
    let warm_skip = matches!(
        (&expected_marker, read_cook_marker(&cook_marker_path)),
        (Some(expected), Some(existing)) if existing == *expected
    );
    if warm_skip {
        eprintln!(
            "soldr cook: warm-cook detected (recipe + rustc match the prior cook marker at {}) — skipping Phase 2 (cargo chef cook). Estimated savings: ~5 min on Coverage-shape workloads. See soldr#621.",
            cook_marker_path.display()
        );
        // Restore source before returning — same invariant as the
        // normal post-cook path (project tree must be pristine for
        // downstream cargo build).
        source_guard.restore()?;
        return Ok(0);
    }

    // soldr#3117: from here on this process must be a requester of the
    // daemon route. Otherwise the compile's wrapper re-entries are the only
    // requesters, the broker reaps the daemon two minutes after the last one
    // exits, and the pack below routinely outlives that -- the closing
    // CookRecord then finds no daemon and the artifact is never indexed.
    // Holding it before Phase 2 also gives the front door's hydrate
    // pre-flight a daemon to ask.
    if let Err(err) = crate::cook_route_hold::hold_daemon_route_for_cook() {
        eprintln!(
            "{} {err}; the cook proceeds, but hydrate and CookRecord may find no daemon.",
            yellow_warning_prefix()
        );
    }

    // Phase 2: cook. Heavy — compiles every transitive dep against a stub
    // project. Output lands in `target/`.
    let compile_started = Instant::now();
    let cook_args = build_chef_cook_args(&ctx, &parsed);
    let cook_result = crate::cook_watchdog::run_with_watchdog(
        "cook",
        cook_target_dir.clone(),
        &watchdog_paths,
        watchdog_config,
        cargo_front_door::run_cargo_front_door(&cook_args, cache_enabled, false),
    )
    .await;

    // Restore the project to its pre-cook state regardless of how cook exited,
    // so the tree is pristine for every subsequent build step (#566).
    // Restoration is a correctness boundary: never run the exact build or
    // persist/index a successful cook against cargo-chef's synthetic source.
    source_guard.restore()?;

    let code = cook_result?;
    if code != 0 {
        return Ok(code);
    }

    // The portable recipe cooks published versions of excluded sibling
    // workspaces. A vendored checkout can still have local patches or unit
    // variants that do not exist in that package, so populate those exact
    // units after restoring the user's real manifests and sources.
    if sanitized.needs_exact_build() {
        eprintln!(
            "soldr cook: supplementing portable dependency cook with the exact vendored dependency graph"
        );
        let mut exact_args = build_exact_cook_args(&parsed);
        if let Some(prep) = &target_prep {
            exact_args =
                crate::cli_dispatch::insert_cargo_config_args(exact_args, &prep.cargo_args);
        }
        let exact_code = crate::cook_watchdog::run_with_watchdog(
            "exact",
            cook_target_dir.clone(),
            &watchdog_paths,
            watchdog_config,
            cargo_front_door::run_cargo_front_door(&exact_args, cache_enabled, false),
        )
        .await?;
        if exact_code != 0 {
            return Ok(exact_code);
        }
    }

    let compile_duration_ms = elapsed_ms(compile_started);

    // #621: persist the warm-cook marker so the next run with the same
    // recipe + rustc can short-circuit Phase 2. Best-effort — if the
    // write fails, the only impact is the next run pays the full
    // cargo-chef orchestration cost. expected_marker is Some when
    // recipe + rustc could be hashed; None on rare resolution
    // failures (e.g. recipe.json missing — shouldn't happen post-cook).
    if let Some(marker) = expected_marker {
        if let Err(e) = write_cook_marker(&cook_marker_path, &marker) {
            eprintln!(
                "soldr cook: warning: failed to write warm-cook marker {} (next run won't short-circuit Phase 2): {e}",
                cook_marker_path.display()
            );
        }
    }

    // Phase 3: post-cook target/ trim (issue #459). Cargo-chef cook
    // leaves cargo-recreatable noise (incremental state, the synthetic
    // stub binary, large stderr blobs, and debug sidecars) under target/.
    // Build-script executables stay because Cargo needs them to preserve
    // dependency freshness after rematerialization. Trimming here
    // shrinks the tarball setup-soldr et al ship across CI runners.
    if !parsed.no_trim {
        run_cook_target_trim(&ctx.manifest_dir);
    }

    // Phase 4: index the cooked target/ into the cross-repo shared
    // `~/.soldr/cache/cook/` (issue #577, meta #579). Best-effort;
    // any failure here surfaces as a warning but never fails the
    // user's cook command — the local `target/` is still valid.
    let index_env = std::env::var(COOK_INDEX_ENV_VAR).ok();
    if !cook_index_wanted(parsed.no_index, index_env.as_deref()) {
        eprintln!(
            "soldr cook: cook-index pack skipped (--no-index / {COOK_INDEX_ENV_VAR}=0, soldr#3499)"
        );
    } else if let Err(err) = index_cooked_artifact(&ctx, &parsed, compile_duration_ms) {
        eprintln!("{} {err}", yellow_warning_prefix());
    }

    if parsed.keep_recipe {
        eprintln!(
            "soldr cook: recipe retained at {}",
            ctx.recipe_path.display()
        );
    } else if ctx.recipe_owned_tempdir {
        eprintln!("soldr cook: deps built; recipe was ephemeral");
    } else {
        eprintln!(
            "soldr cook: deps built; recipe retained at {}",
            ctx.recipe_path.display()
        );
    }
    Ok(0)
}

/// ANSI-yellow `warning:` prefix under the canonical color rule.
///
/// soldr#3437: this used to gate on the terminal alone, so `NO_COLOR` was
/// ignored here while every other surface honored it. It now asks
/// `crate::color_choice` like the rest.
fn yellow_warning_prefix() -> String {
    crate::color_choice::paint_stderr("soldr cook: warning:", crate::color_choice::YELLOW)
}

/// ANSI-green `indexed` prefix for the success line. Same ruling as
/// [`yellow_warning_prefix`].
fn green_indexed_prefix() -> String {
    crate::color_choice::paint_stderr("soldr cook: indexed", crate::color_choice::GREEN)
}

/// Resolve the `target/<triple?>/<profile>/` directory cargo-chef
/// just populated. cargo's mapping: `--release` → `release`, no
/// flag → `debug` (the "dev" profile), `--profile=<name>` → `<name>`.
/// With `--target X` the artifacts land under `target/X/<profile>/`.
fn resolve_cook_target_dir(manifest_dir: &Path, args: &CookArgs) -> PathBuf {
    let invocation_dir = std::env::current_dir().unwrap_or_else(|_| manifest_dir.to_path_buf());
    resolve_cook_target_dir_with(
        &crate::core::cargo_target_dir::CargoTargetDirInputs::from_process(&invocation_dir, &[]),
        manifest_dir,
        args,
    )
}

/// soldr#3203: the target root comes from the shared resolver, so a cook run
/// from inside a workspace member lands in the workspace root's `target/` and
/// honours `.cargo/config.toml`. `manifest_dir/target` is only the fallback for
/// a resolver that finds no manifest.
fn resolve_cook_target_dir_with(
    inputs: &crate::core::cargo_target_dir::CargoTargetDirInputs,
    manifest_dir: &Path,
    args: &CookArgs,
) -> PathBuf {
    let profile = resolve_profile_dir_name(args);
    let mut root = crate::core::cargo_target_dir::resolve_cargo_target_dir(inputs)
        .unwrap_or_else(|| manifest_dir.join("target"));
    if let Some(triple) = args.target.as_deref() {
        root = root.join(triple);
    }
    root.join(profile)
}

fn resolve_profile_dir_name(args: &CookArgs) -> &str {
    if let Some(p) = args.profile.as_deref() {
        // `dev` is special-cased by cargo → `debug` dir on disk.
        return if p == "dev" { "debug" } else { p };
    }
    if args.release {
        "release"
    } else {
        "debug"
    }
}

fn resolve_profile_name(args: &CookArgs) -> &str {
    if let Some(p) = args.profile.as_deref() {
        return p;
    }
    if args.release {
        "release"
    } else {
        "dev"
    }
}

/// Run `rustc -V` and return the trimmed first line, e.g.
/// `rustc 1.94.1 (abcdef0 2026-05-30)`. Returns `None` when rustc is
/// unreachable — the caller treats this as "no rustc identity" and
/// skips indexing (cross-repo sharing without rustc keying would be
/// unsafe).
/// #621 warm-cook marker. Captures the inputs that determine whether
/// the next Phase-2 cook would do net work. When this matches a
/// previously-persisted marker under `target/`, the cook orchestration
/// would walk the dep graph just to confirm zccache hits at ~96% —
/// ~5 minutes of wasted wall clock on Coverage-shape workloads.
#[derive(Debug, PartialEq, Eq)]
struct CookMarker {
    /// Schema version. Bump on incompatible format changes so an old
    /// marker doesn't accidentally satisfy a new check.
    version: u32,
    /// SHA-256 hex of the post-`sanitize_cargo_chef_recipe` recipe.json.
    recipe_sha256: String,
    /// SHA-256 of the exact Cargo selection (workspace/packages/features and
    /// other passthrough flags). A narrower cook must never warm-skip a later
    /// broader invocation that shares the same cargo-chef recipe.
    selection_sha256: String,
    /// `rustc -V` first line.
    rustc_version: String,
    /// `soldr --version` (CARGO_PKG_VERSION). A different soldr could
    /// theoretically have changed the recipe sanitizer behavior.
    soldr_version: String,
}

const COOK_MARKER_VERSION: u32 = 3;

/// Read + parse the warm-cook marker at `path`. Any error (missing
/// file, malformed JSON, missing field, version mismatch) returns
/// `None` so the caller falls through to the normal cook path.
/// The soldr#621 warm-cook marker `soldr cook` leaves in the cooked
/// `target/<triple>/` directory. Its presence also tells the front door's
/// hydrate pre-flight that this tree was cooked in place, so there is nothing
/// an archive restore could add (soldr#3117).
pub(crate) const COOK_MARKER_FILE_NAME: &str = ".soldr-cook-marker.json";

fn read_cook_marker(path: &Path) -> Option<CookMarker> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let version = value.get("version")?.as_u64()? as u32;
    if version != COOK_MARKER_VERSION {
        return None;
    }
    Some(CookMarker {
        version,
        recipe_sha256: value.get("recipe_sha256")?.as_str()?.to_string(),
        selection_sha256: value.get("selection_sha256")?.as_str()?.to_string(),
        rustc_version: value.get("rustc_version")?.as_str()?.to_string(),
        soldr_version: value.get("soldr_version")?.as_str()?.to_string(),
    })
}

/// Write the marker as JSON. Creates the parent dir if needed.
fn write_cook_marker(path: &Path, marker: &CookMarker) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({
        "version": marker.version,
        "recipe_sha256": &marker.recipe_sha256,
        "selection_sha256": &marker.selection_sha256,
        "rustc_version": &marker.rustc_version,
        "soldr_version": &marker.soldr_version,
    });
    std::fs::write(path, body.to_string())
}

/// Compute the expected warm-cook marker for the current invocation.
/// Returns `None` when any required input can't be resolved (recipe
/// file missing — shouldn't happen post-Phase-1, but if it does we
/// simply skip the optimization).
fn compute_cook_marker(ctx: &CookContext, parsed: &CookArgs) -> Option<CookMarker> {
    use sha2::{Digest, Sha256};
    let recipe_bytes = std::fs::read(&ctx.recipe_path).ok()?;
    let mut h = Sha256::new();
    h.update(&recipe_bytes);
    let recipe_sha256 = hex_lower(&h.finalize());
    let selection_sha256 = cook_selection_sha256(parsed);
    let rustc_version = match rustc_version_string(&ctx.manifest_dir) {
        Ok(version) => version,
        Err(error) => {
            eprintln!("soldr cook: warning: warm-cook marker has no rustc version: {error}");
            String::new()
        }
    };
    Some(CookMarker {
        version: COOK_MARKER_VERSION,
        recipe_sha256,
        selection_sha256,
        rustc_version,
        soldr_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

fn cook_selection_sha256(args: &CookArgs) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for arg in build_exact_cook_args(args) {
        h.update(arg.as_bytes());
        h.update([0]);
    }
    hex_lower(&h.finalize())
}

/// Lower-case hex helper (`crate::cache_lib::cook_archive::sha_abbrev` truncates).
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// `rustc -V` for the toolchain `manifest_dir` resolves to. Failures carry
/// the spawn error / exit status / stderr excerpt (soldr#3381).
fn rustc_version_string(manifest_dir: &Path) -> Result<String, SoldrError> {
    let rustc = probe_toolchain_binary("rustc", Some(manifest_dir))
        .unwrap_or_else(|| PathBuf::from("rustc"));
    crate::core::tool_output::rustc_version_line(&rustc)
}

fn resolve_channel(manifest_dir: &Path) -> Option<String> {
    match read_rust_toolchain_manifest(manifest_dir) {
        Ok(m) => m.channel,
        Err(_) => None,
    }
}

fn resolve_target_triple(manifest_dir: &Path, args: &CookArgs) -> Option<String> {
    if let Some(triple) = args.target.as_deref() {
        return Some(triple.to_string());
    }
    TargetTriple::detect_in_dir(manifest_dir)
        .ok()
        .map(|t| t.to_string())
}

/// Index the cooked `target/<profile>/` tree into the cross-repo
/// shared `~/.soldr/cache/cook/` (issue #577) and register the
/// artifact with the daemon via `CookRecord`. Best-effort: a failure
/// here surfaces as a warning but never fails the cook command.
fn index_cooked_artifact(
    ctx: &CookContext,
    args: &CookArgs,
    compile_duration_ms: u64,
) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    index_cooked_artifact_with_packer(
        ctx,
        args,
        &paths,
        compile_duration_ms,
        cook_archive::pack_cook_archive,
    )
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn index_cooked_artifact_with_packer<F>(
    ctx: &CookContext,
    args: &CookArgs,
    paths: &SoldrPaths,
    compile_duration_ms: u64,
    packer: F,
) -> Result<(), SoldrError>
where
    F: FnOnce(
        &Path,
        &Path,
    ) -> Result<PackedCookArchive, crate::cache_lib::target_registry::RegistryError>,
{
    // 1. Sharing-eligibility checks.
    let lockfile = ctx.manifest_dir.join("Cargo.lock");
    if !lockfile.is_file() {
        // Already warned earlier in build_cook_context.
        return Ok(());
    }
    let has_git = find_git_worktree_root(&ctx.manifest_dir).is_some();
    let lock_tracked = cargo_lock_is_tracked(&ctx.manifest_dir);
    let lock_gitignored = cargo_lock_is_gitignored(&ctx.manifest_dir);

    if !lock_tracked {
        // Includes both "no .git/" and "git but file untracked".
        if has_git {
            eprintln!(
                "{} cross-repo cook sharing disabled — Cargo.lock is not tracked by git. \
                 Commit it to enable.",
                yellow_warning_prefix()
            );
        } else {
            eprintln!(
                "{} no .git/ — cook artifact will be local-only, not indexed for sharing.",
                yellow_warning_prefix()
            );
        }
        return Ok(());
    }
    if lock_gitignored {
        eprintln!(
            "{} Cargo.lock is gitignored — sharing may leak deps that diverge from upstream. \
             Skipping cook-index registration.",
            yellow_warning_prefix()
        );
        return Ok(());
    }

    // 2. Resolve key components.
    let target_dir = resolve_cook_target_dir(&ctx.manifest_dir, args);
    if !target_dir.is_dir() {
        // cargo-chef cook didn't produce a target/<profile>/ tree —
        // most likely because the build failed earlier (we already
        // returned in that case). Defensive fallback: silently skip.
        return Ok(());
    }

    let triple = resolve_target_triple(&ctx.manifest_dir, args).ok_or_else(|| {
        SoldrError::Other("soldr cook: could not resolve target triple for cook-index key".into())
    })?;
    let channel = resolve_channel(&ctx.manifest_dir).unwrap_or_default();
    let rustc_version = rustc_version_string(&ctx.manifest_dir).map_err(|error| {
        SoldrError::Other(format!(
            "soldr cook: could not resolve rustc version for cook-index key: {error}"
        ))
    })?;
    // PR 3 (#578): the cook-index recipe hash is the workspace
    // content-fingerprint computed by
    // `cook_archive::compute_recipe_hash_proxy` so PR 3's cargo-front-
    // door pre-flight can compute the same key cheaply (no
    // cargo-chef invocation in the hot path).
    let recipe_hash =
        cook_archive::compute_recipe_hash_proxy(&ctx.manifest_dir).ok_or_else(|| {
            SoldrError::Other(
                "soldr cook: could not compute recipe hash proxy (Cargo.lock unreadable?)".into(),
            )
        })?;
    let origin = origin_url(&ctx.manifest_dir);
    let branch_name = current_branch_name(&ctx.manifest_dir);
    let profile = resolve_profile_name(args).to_string();

    // 3. Resolve paths under ~/.soldr/.
    let cook_dir = cook_cache_dir(paths);
    std::fs::create_dir_all(&cook_dir).map_err(|e| {
        SoldrError::Other(format!(
            "soldr cook: failed to create {}: {e}",
            cook_dir.display()
        ))
    })?;

    // 4. Pre-flight CookLookup so we can render a drift diagnostic
    //    when a previous recipe hash exists for this (origin, triple,
    //    profile, channel, rustc). If this cannot reach the daemon,
    //    CookRecord cannot be relied on either, so skip the expensive
    //    archive pack entirely.
    let sock = client::default_sock_path(paths);
    let lookup = client::cook_lookup(
        &sock,
        recipe_hash,
        triple.clone(),
        profile.clone(),
        channel.clone(),
        rustc_version.clone(),
        origin.clone(),
    );
    // soldr#3117: an exact hit whose archive is still on disk needs no
    // re-pack. The pack is the slow half of a warm cook, and the indexed
    // artifact already describes this dependency graph byte for byte.
    if let Some(existing) = lookup
        .as_ref()
        .ok()
        .and_then(|outcome| already_indexed_archive(&cook_dir, outcome))
    {
        eprintln!(
            "soldr cook: already indexed  path={}  (exact recipe match; skipping re-pack)",
            existing.display()
        );
        return Ok(());
    }
    let prior_drift = match lookup {
        Ok(CookLookupOutcome::Hit {
            sha256,
            matched_recipe_hash,
            exact_recipe_match,
            ..
        }) => {
            // An exact hit whose archive is gone (evicted, or a foreign
            // path in the index): re-pack + re-record so the index names
            // bytes that exist. A drifted hit re-packs as before.
            if exact_recipe_match {
                Some(DriftSignal::AlreadyIndexed(sha256))
            } else {
                matched_recipe_hash.map(DriftSignal::Drifted)
            }
        }
        Ok(CookLookupOutcome::Miss {
            previous_origin_recipe_hashes,
        }) => previous_origin_recipe_hashes
            .first()
            .copied()
            .map(DriftSignal::Drifted),
        Err(e) => {
            eprintln!(
                "{} cook index unavailable; skipping shared cook archive pack ({e:?}).",
                yellow_warning_prefix()
            );
            return Ok(());
        }
    };

    // 5. Pack the trimmed target/<profile>/ tree.
    let save_started = Instant::now();
    let packed = packer(&target_dir, &cook_dir)
        .map_err(|e| SoldrError::Other(format!("soldr cook: failed to pack cook archive: {e}")))?;
    let save_elapsed_ms = elapsed_ms(save_started);

    // 6. Register with the daemon. If the daemon is unreachable we
    //    still keep the on-disk artifact for the next PR-3 hydrate
    //    attempt, but warn so the user knows sharing is one-sided.
    let cook_cmd_summary = build_cook_cmd_summary(args);
    let register = client::cook_record_with_branch_timing(
        &sock,
        recipe_hash,
        triple.clone(),
        profile.clone(),
        channel.clone(),
        rustc_version.clone(),
        packed.sha256,
        packed.size_bytes,
        origin.clone(),
        branch_name,
        cook_cmd_summary,
        compile_duration_ms,
        save_elapsed_ms,
    );
    if let Err(e) = register {
        eprintln!(
            "{} CookRecord to daemon failed: {e:?}. Artifact written at {} but not indexed.",
            yellow_warning_prefix(),
            packed.path.display()
        );
        return Ok(());
    }

    // 7. Recipe-drift diagnostic (printed before the success line so
    //    the indexed line is the last word).
    if let Some(DriftSignal::Drifted(prev)) = prior_drift {
        eprintln!("soldr cook: recipe hash drift since {}", sha_abbrev(&prev));
    }

    // 8. Green success line.
    emit_indexed_line(
        &packed,
        origin.as_deref(),
        compile_duration_ms,
        save_elapsed_ms,
    );
    Ok(())
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

include!("cook_indexing.rs");
#[cfg(test)]
#[path = "cook_tests.rs"]
mod tests;
