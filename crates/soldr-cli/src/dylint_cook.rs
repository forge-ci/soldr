//! Exact-nightly dependency preparation for Dylint.
//!
//! The pass is check-shaped for the default `--tree analysis` and
//! build-shaped for `--tree tests` (soldr#3042); see
//! [`crate::dylint_cook_tree::CookTree::operation`].

use crate::cargo_front_door::{self, DYLINT_DEPENDENCY_COOK_FLAG};
use crate::core::{read_rust_toolchain_manifest, SoldrError};
// soldr#2945: one definition of "reduce a channel to the driver identity",
// shared with the glob-aware library reader that needs the same rule.
use crate::dylint_cook_tree::CookTree;
use crate::dylint_libraries::canonical_channel;
use crate::dylint_toolchain::DylintToolchainPlan;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;
const MARKER_NAME: &str = ".soldr-dylint-cook-v1.json";

/// The `CACHEDIR.TAG` cargo writes into a target directory it creates.
///
/// The signature line is fixed by the cache-directory-tagging spec; the rest is
/// cargo's own wording, kept verbatim so a reader who greps for it finds the
/// same file cargo would have written.
const CACHEDIR_TAG_CONTENTS: &str = "\
Signature: 8a477f597d28d172789f06886806bc55
# This file is a cache directory tag created by cargo.
# For information about cache directory tags see https://bford.info/cachedir/
";

/// Give the target directory the tag cargo would have written itself.
///
/// soldr#2820: `soldr dylint cook` creates its nested target directory
/// (`target/dylint/target/<nightly>`) with `create_dir_all` *before* handing it
/// to cargo as `--target-dir`. Cargo writes `CACHEDIR.TAG` when it creates a
/// target directory, so pre-creating it means the tag is never written -- and
/// cargo's own cleanup guard then refuses the directory:
///
/// ```text
/// error: cannot clean `.../target/dylint/target/nightly-2026-05-28`:
///   missing or invalid `CACHEDIR.TAG` file
///   = note: cleaning has been aborted to prevent accidental deletion of
///     unrelated files
/// ```
///
/// which fails the whole `dylint cook` at its dummy-artifact cleanup step.
///
/// Writing the tag is not defeating that guard. The guard exists so cargo never
/// deletes a directory it does not recognise as a cache; this one *is* a cargo
/// target directory, created by soldr for cargo's exclusive use, and tagging it
/// restores the state cargo would have reached on its own.
///
/// Best-effort: an existing tag is left alone, and a failure to write is not
/// worth failing the cook over -- the clean will surface it with cargo's own
/// message if it matters.
fn ensure_cachedir_tag(target_dir: &Path) {
    let tag = target_dir.join("CACHEDIR.TAG");
    if tag.exists() {
        return;
    }
    let _ = std::fs::write(&tag, CACHEDIR_TAG_CONTENTS);
}
const LOCK_NAME: &str = ".soldr-dylint-cook.lock";
const WRAPPER_IDENTITY: &str = "soldr-dylint-dependency-cook-v1";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct DylintCookArgs {
    #[serde(skip)]
    json: bool,
    #[serde(skip)]
    plan_only: bool,
    release: bool,
    profile: Option<String>,
    target: Option<String>,
    workspace: bool,
    packages: Vec<String>,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    all_targets: bool,
    tests: bool,
    benches: bool,
    examples: bool,
    locked: bool,
    frozen: bool,
    offline: bool,
    cargo_config: Vec<String>,
    toolchain: Option<String>,
    // Both `#[serde(skip)]`: `tree` enters the digest explicitly (see
    // `build_output`'s `digest.update(args.tree.name().as_bytes())`) and
    // `target_root` is already covered by the marker's `target_directory`
    // field comparison, so double-hashing either through the generic
    // `BuildShape`/`args` JSON encoding would be redundant.
    #[serde(skip)]
    tree: CookTree,
    #[serde(skip)]
    target_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
struct CompilerPlan {
    channel: String,
    release: String,
    commit_hash: String,
    verified: bool,
}

#[derive(Debug, Clone, Serialize)]
struct BuildShape {
    operation: &'static str,
    /// Which of the two Dylint target trees this cook prepared
    /// (`"analysis"` or `"tests"`, soldr#3042 step 3).
    tree: &'static str,
    profile: String,
    target: Option<String>,
    workspace: bool,
    packages: Vec<String>,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    all_targets: bool,
    tests: bool,
    benches: bool,
    examples: bool,
    locked: bool,
    frozen: bool,
    offline: bool,
    cargo_config: Vec<String>,
    wrapper_identity: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct DylintCookOutput {
    schema_version: u32,
    command: &'static str,
    compiler: CompilerPlan,
    target_directory: String,
    build_shape: BuildShape,
    cache_key: String,
    outcome: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DylintCookMarker {
    schema_version: u32,
    cache_key: String,
    compiler_commit: String,
    target_directory: String,
}

fn parse_args(args: &[String]) -> Result<DylintCookArgs, SoldrError> {
    let mut parsed = DylintCookArgs::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let next = |flag: &str, index: &mut usize| -> Result<String, SoldrError> {
            *index += 1;
            args.get(*index).cloned().ok_or_else(|| {
                SoldrError::Other(format!("soldr dylint cook: {flag} requires a value"))
            })
        };
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--plan-only" => parsed.plan_only = true,
            "--release" => parsed.release = true,
            "--workspace" | "--all" => parsed.workspace = true,
            "--all-features" => parsed.all_features = true,
            "--no-default-features" => parsed.no_default_features = true,
            "--all-targets" => parsed.all_targets = true,
            "--tests" => parsed.tests = true,
            "--benches" => parsed.benches = true,
            "--examples" => parsed.examples = true,
            "--locked" => parsed.locked = true,
            "--frozen" => parsed.frozen = true,
            "--offline" => parsed.offline = true,
            "--target" => parsed.target = Some(next("--target", &mut index)?),
            "--tree" => parsed.tree = CookTree::parse(&next("--tree", &mut index)?)?,
            "--target-root" => {
                parsed.target_root = Some(PathBuf::from(next("--target-root", &mut index)?))
            }
            "--profile" => parsed.profile = Some(next("--profile", &mut index)?),
            "-p" | "--package" => parsed.packages.push(next("--package", &mut index)?),
            "--features" => extend_features(&mut parsed.features, &next("--features", &mut index)?),
            "--config" => parsed.cargo_config.push(next("--config", &mut index)?),
            "--toolchain" => parsed.toolchain = Some(next("--toolchain", &mut index)?),
            value if value.starts_with("--target=") => parsed.target = Some(value[9..].to_string()),
            value if value.starts_with("--tree=") => parsed.tree = CookTree::parse(&value[7..])?,
            value if value.starts_with("--target-root=") => {
                parsed.target_root = Some(PathBuf::from(&value[14..]))
            }
            value if value.starts_with("--profile=") => {
                parsed.profile = Some(value[10..].to_string())
            }
            value if value.starts_with("--package=") => {
                parsed.packages.push(value[10..].to_string())
            }
            value if value.starts_with("--features=") => {
                extend_features(&mut parsed.features, &value[11..])
            }
            value if value.starts_with("--config=") => {
                parsed.cargo_config.push(value[9..].to_string())
            }
            value if value.starts_with("--toolchain=") => {
                parsed.toolchain = Some(value[12..].to_string())
            }
            value if value.starts_with("+nightly-") => {
                if parsed.toolchain.replace(value[1..].to_string()).is_some() {
                    return Err(SoldrError::Other(
                        "soldr dylint cook: multiple toolchain selectors".into(),
                    ));
                }
            }
            other => {
                return Err(SoldrError::Other(format!(
                    "soldr dylint cook: unsupported option `{other}`"
                )))
            }
        }
        index += 1;
    }
    if parsed.release && parsed.profile.is_some() {
        return Err(SoldrError::Other(
            "soldr dylint cook: --release conflicts with --profile".into(),
        ));
    }
    if parsed.all_features && parsed.no_default_features {
        return Err(SoldrError::Other(
            "soldr dylint cook: --all-features conflicts with --no-default-features".into(),
        ));
    }
    if parsed.tree == CookTree::Tests && parsed.target.is_some() {
        // `ci_test/plan.rs` passes no `--target` to the UI-test stage, so a
        // `--target` cook would land artifacts under `<key>/<triple>/debug`,
        // where cargo will not look.
        return Err(SoldrError::Other(
            "soldr dylint cook: --target conflicts with --tree tests; the Dylint UI-test stages compile for the host only".into(),
        ));
    }
    parsed.features.sort();
    parsed.features.dedup();
    parsed.packages.sort();
    parsed.packages.dedup();
    Ok(parsed)
}

fn extend_features(features: &mut Vec<String>, value: &str) {
    features.extend(
        value
            .split([',', ' '])
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );
}

pub(crate) async fn run(args: &[String], cache_enabled: bool) -> Result<i32, SoldrError> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print_help();
        return Ok(0);
    }
    let parsed = parse_args(args)?;
    let cwd = std::env::current_dir()?;
    let root = crate::cook::resolve_manifest_dir(&cwd)?;
    if !root.join(concat!("Car", "go.lock")).is_file() {
        return Err(SoldrError::Other(
            "soldr dylint cook: a lockfile is required for a restorable dependency layer".into(),
        ));
    }
    // Acquire before reading any manifest/toolchain/hash inputs: another cook
    // may be inside cargo-chef's in-place skeleton reconstruction even before
    // this invocation reaches its own snapshot.
    let source_lock = lock_workspace_source(&root)?;
    // soldr#3518: never read, hash, or snapshot a tree a killed cook left as
    // a cargo-chef skeleton.
    crate::cook_source_journal::recover_stale_cook_journals(&root)?;
    let configured = configured_library_toolchain(&root)?;
    let requested = reconcile_toolchain(parsed.toolchain.as_deref(), configured.as_deref())?;

    if parsed.plan_only {
        let plan = crate::dylint_toolchain::resolve_plan(requested, &root).await?;
        let verified = crate::dylint_toolchain::verify_if_installed(&plan)?;
        let output = build_output(&root, &parsed, &plan, verified)?;
        emit_output(&output, parsed.json)?;
        return Ok(0);
    }

    let plan = crate::dylint_toolchain::prepare(requested, &root).await?;
    crate::dylint_toolchain::verify_observed_identity(&plan)?;
    if parsed.tree == CookTree::Tests {
        // FACT 3 (soldr#3042 step 3): mirror the environment
        // `ci_test/execute.rs:848-861` gives every `dylint-*` stage —
        // the `ensure_known_subcommand_tool(["dylint"])` bootstrap env +
        // bin dirs (this is where `dylint-link` comes from), and
        // `SOLDR_LINKER=default` / `SOLDR_NO_GC_TARGET=1`. Without this,
        // the tests-tree cook produces artifacts the UI-test stage's own
        // fingerprint (RUSTFLAGS included) rejects as stale.
        let paths = crate::core::SoldrPaths::new()?;
        paths.ensure_dirs()?;
        let bootstrap =
            crate::cargo_front_door::ensure_known_subcommand_tool(&["dylint".to_string()], &paths)
                .await?;
        crate::dylint_cook_tree::apply_dylint_ui_test_environment(&bootstrap)?;
    }
    let mut output = build_output(&root, &parsed, &plan, true)?;
    let target_dir = PathBuf::from(&output.target_directory);
    std::fs::create_dir_all(&target_dir)?;
    ensure_cachedir_tag(&target_dir);
    let lock = lock_target(&target_dir)?;
    let expected = marker_for_output(&output);
    let marker_path = target_dir.join(MARKER_NAME);
    if read_marker(&marker_path).as_ref() == Some(&expected)
        && target_has_dependency_payload(&target_dir)
    {
        output.outcome = "skip";
        emit_output(&output, parsed.json)?;
        FileExt::unlock(&lock)?;
        FileExt::unlock(&source_lock)?;
        return Ok(0);
    }

    let recipe_dir = tempfile::tempdir()?;
    let recipe_path = recipe_dir.path().join("recipe.json");
    // soldr#3518: journaled to disk before chef's in-place reconstruction.
    let source_guard = crate::cook_source_journal::CookSourceGuard::begin(&root)?;
    let result =
        run_check_shaped_cook(&recipe_path, &target_dir, &parsed, &plan, cache_enabled).await;
    source_guard.restore().map_err(|error| {
        SoldrError::Other(format!(
            "soldr dylint cook: failed to restore workspace sources: {error}"
        ))
    })?;
    let code = result?;
    if code != 0 {
        return Ok(code);
    }
    remove_workspace_dummy_artifacts(&recipe_path, &target_dir, &parsed, &plan).await?;
    write_marker(&marker_path, &expected)?;
    output.outcome = "miss";
    emit_output(&output, parsed.json)?;
    FileExt::unlock(&lock)?;
    FileExt::unlock(&source_lock)?;
    Ok(0)
}

fn print_help() {
    println!(
        "Prepare Dylint dependencies with an exact nightly pass \
(check-shaped for --tree analysis, build-shaped for --tree tests).\n\n\
Usage: soldr dylint cook [OPTIONS]\n\n\
Options:\n  --plan-only --json\n  --toolchain <NIGHTLY>\n  --target <TRIPLE>\n  \
--tree <analysis|tests>\n  --target-root <DIR>\n  \
--release | --profile <NAME>\n  --workspace | --package <NAME>\n  \
--features <LIST> | --all-features | --no-default-features\n  \
--all-targets --tests --benches --examples\n  --config <KEY=VALUE>\n  \
--locked --frozen --offline"
    );
}

fn reconcile_toolchain<'a>(
    explicit: Option<&'a str>,
    configured: Option<&'a str>,
) -> Result<Option<&'a str>, SoldrError> {
    if let (Some(explicit), Some(configured)) = (explicit, configured) {
        if canonical_channel(explicit) != canonical_channel(configured) {
            return Err(SoldrError::Other(format!(
                "soldr dylint cook: explicit `{explicit}` conflicts with custom-lint requirement `{configured}`"
            )));
        }
    }
    Ok(explicit.or(configured))
}

async fn run_check_shaped_cook(
    recipe_path: &Path,
    target_dir: &Path,
    args: &DylintCookArgs,
    plan: &DylintToolchainPlan,
    cache_enabled: bool,
) -> Result<i32, SoldrError> {
    let prepare = vec![
        format!("+{}", plan.channel),
        "chef".into(),
        "prepare".into(),
        "--recipe-path".into(),
        recipe_path.display().to_string(),
    ];
    let code = cargo_front_door::run_cargo_front_door(&prepare, cache_enabled, false).await?;
    if code != 0 {
        return Ok(code);
    }
    let reconstruct = vec![
        format!("+{}", plan.channel),
        "chef".into(),
        "cook".into(),
        "--recipe-path".into(),
        recipe_path.display().to_string(),
        "--no-build".into(),
    ];
    let code = cargo_front_door::run_cargo_front_door(&reconstruct, cache_enabled, false).await?;
    if code != 0 {
        return Ok(code);
    }
    cargo_front_door::run_cargo_front_door(
        &build_check_args(args, plan, target_dir),
        cache_enabled,
        false,
    )
    .await
}

/// Build the cargo invocation for the cook's final build/check step.
///
/// The operation is tree-dependent, not always `check`: FACT 2 (soldr#3042
/// step 3) is that `cargo check` emits `.rmeta` under a different
/// fingerprint mode than the `cargo test` the UI-test stage runs
/// (`ci_test/plan.rs:160-177`), so a check-shaped cook of the tests tree
/// would be recompiled wholesale the first time the UI-test stage touched
/// it. `args.tree.operation()` resolves to `"check"` for the historical
/// analysis tree and `"build"` for the tests tree.
fn build_check_args(
    args: &DylintCookArgs,
    plan: &DylintToolchainPlan,
    target_dir: &Path,
) -> Vec<String> {
    let mut result = vec![
        format!("+{}", plan.channel),
        DYLINT_DEPENDENCY_COOK_FLAG.into(),
    ];
    for config in &args.cargo_config {
        result.extend(["--config".into(), config.clone()]);
    }
    result.push(args.tree.operation().into());
    result.extend(["--target-dir".into(), target_dir.display().to_string()]);
    if args.release {
        result.push("--release".into());
    }
    if let Some(profile) = &args.profile {
        result.extend(["--profile".into(), profile.clone()]);
    }
    if let Some(target) = &args.target {
        result.extend(["--target".into(), target.clone()]);
    }
    if args.workspace {
        result.push("--workspace".into());
    }
    for package in &args.packages {
        result.extend(["--package".into(), package.clone()]);
    }
    if !args.features.is_empty() {
        result.extend(["--features".into(), args.features.join(",")]);
    }
    for (enabled, flag) in [
        (args.all_features, "--all-features"),
        (args.no_default_features, "--no-default-features"),
        (args.all_targets, "--all-targets"),
        (args.tests, "--tests"),
        (args.benches, "--benches"),
        (args.examples, "--examples"),
        (args.locked, "--locked"),
        (args.frozen, "--frozen"),
        (args.offline, "--offline"),
    ] {
        if enabled {
            result.push(flag.into());
        }
    }
    result
}

async fn remove_workspace_dummy_artifacts(
    recipe_path: &Path,
    target_dir: &Path,
    args: &DylintCookArgs,
    plan: &DylintToolchainPlan,
) -> Result<(), SoldrError> {
    let packages = workspace_package_names(recipe_path)?;
    if packages.is_empty() {
        return Ok(());
    }
    let mut clean = vec![
        format!("+{}", plan.channel),
        DYLINT_DEPENDENCY_COOK_FLAG.into(),
    ];
    for config in &args.cargo_config {
        clean.extend(["--config".into(), config.clone()]);
    }
    clean.extend([
        "clean".into(),
        "--target-dir".into(),
        target_dir.display().to_string(),
    ]);
    for package in packages {
        clean.extend(["--package".into(), package]);
    }
    let code = cargo_front_door::run_cargo_front_door(&clean, false, false).await?;
    if code != 0 {
        return Err(SoldrError::Other(format!(
            "soldr dylint cook: dummy artifact cleanup failed with exit {code}"
        )));
    }
    Ok(())
}

fn workspace_package_names(recipe_path: &Path) -> Result<Vec<String>, SoldrError> {
    let mut names = Vec::new();
    let recipe: serde_json::Value = serde_json::from_slice(&std::fs::read(recipe_path)?)
        .map_err(|error| SoldrError::Other(format!("recipe parsing failed: {error}")))?;
    let manifests = recipe
        .get("skeleton")
        .and_then(|value| value.get("manifests"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| SoldrError::Other("recipe contains no manifests".into()))?;
    for manifest in manifests {
        let Some(contents) = manifest.get("contents").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Ok(value) = toml::from_str::<toml::Value>(contents) else {
            continue;
        };
        if let Some(name) = value
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn build_output(
    root: &Path,
    args: &DylintCookArgs,
    plan: &DylintToolchainPlan,
    verified: bool,
) -> Result<DylintCookOutput, SoldrError> {
    let host = crate::core::TargetTriple::host()?.triple().to_string();
    let target_directory = cook_target_root(root, args)?
        .join("dylint")
        .join(args.tree.directory())
        .join(args.tree.channel_segment(&plan.channel, &host));
    let mut digest = Sha256::new();
    digest.update(b"soldr-dylint-cook-v1\0");
    digest.update(args.tree.name().as_bytes());
    digest.update([0]);
    digest.update(plan.channel.as_bytes());
    digest.update([0]);
    digest.update(plan.compiler_release.as_bytes());
    digest.update([0]);
    digest.update(plan.compiler_commit.as_bytes());
    digest.update([0]);
    digest.update(semantic_input_hash(root, args)?.as_bytes());
    let cache_key = hex::encode(digest.finalize());
    let marker = DylintCookMarker {
        schema_version: SCHEMA_VERSION,
        cache_key: cache_key.clone(),
        compiler_commit: plan.compiler_commit.clone(),
        target_directory: target_directory.display().to_string(),
    };
    let outcome = if verified
        && read_marker(&target_directory.join(MARKER_NAME)).as_ref() == Some(&marker)
        && target_has_dependency_payload(&target_directory)
    {
        "hit"
    } else {
        "miss"
    };
    Ok(DylintCookOutput {
        schema_version: SCHEMA_VERSION,
        command: "dylint-cook",
        compiler: CompilerPlan {
            channel: plan.channel.clone(),
            release: plan.compiler_release.clone(),
            commit_hash: plan.compiler_commit.clone(),
            verified,
        },
        target_directory: target_directory.display().to_string(),
        build_shape: BuildShape {
            operation: args.tree.operation(),
            tree: args.tree.name(),
            profile: args
                .profile
                .clone()
                .unwrap_or_else(|| if args.release { "release" } else { "dev" }.into()),
            target: args.target.clone(),
            workspace: args.workspace,
            packages: args.packages.clone(),
            features: args.features.clone(),
            all_features: args.all_features,
            no_default_features: args.no_default_features,
            all_targets: args.all_targets,
            tests: args.tests,
            benches: args.benches,
            examples: args.examples,
            locked: args.locked,
            frozen: args.frozen,
            offline: args.offline,
            cargo_config: args.cargo_config.clone(),
            wrapper_identity: WRAPPER_IDENTITY,
        },
        cache_key,
        outcome,
    })
}

fn marker_for_output(output: &DylintCookOutput) -> DylintCookMarker {
    DylintCookMarker {
        schema_version: SCHEMA_VERSION,
        cache_key: output.cache_key.clone(),
        compiler_commit: output.compiler.commit_hash.clone(),
        target_directory: output.target_directory.clone(),
    }
}

fn emit_output(output: &DylintCookOutput, json: bool) -> Result<(), SoldrError> {
    if json {
        println!(
            "{}",
            serde_json::to_string(output).map_err(|error| {
                SoldrError::Other(format!("failed to encode Dylint cook output: {error}"))
            })?
        );
    } else {
        eprintln!(
            "soldr dylint cook: {} {} -> {} [{}]",
            output.compiler.channel,
            output.compiler.commit_hash,
            output.target_directory,
            output.outcome
        );
    }
    Ok(())
}

/// soldr#3203: the shared resolver, run as Cargo would be from `root`.
fn cargo_target_root(root: &Path) -> Result<PathBuf, SoldrError> {
    let inputs = crate::core::cargo_target_dir::CargoTargetDirInputs::from_process(root, &[]);
    Ok(
        crate::core::cargo_target_dir::resolve_cargo_target_dir(&inputs)
            .unwrap_or_else(|| root.join("target")),
    )
}

/// The target root the cook should fill, honouring `--target-root` when
/// given.
///
/// soldr#3042: the tests-tree cook runs with cwd inside `dylints/<lint>`
/// (its own workspace, its own `Cargo.lock`), but the tree it must fill
/// lives under the REPO's `target/`. Without this flag the path would
/// resolve under `cargo_target_root` relative to the lint's own root —
/// i.e. `dylints/<lint>/target/...` — which
/// `.github/scripts/verify_dylint_target_dirs.py` fails the build over.
fn cook_target_root(root: &Path, args: &DylintCookArgs) -> Result<PathBuf, SoldrError> {
    match &args.target_root {
        Some(path) if path.is_absolute() => Ok(path.clone()),
        Some(path) => Ok(std::env::current_dir()?.join(path)),
        None => cargo_target_root(root),
    }
}

fn semantic_input_hash(root: &Path, args: &DylintCookArgs) -> Result<String, SoldrError> {
    let mut entries = BTreeMap::<String, Vec<u8>>::new();
    visit_semantic_files(root, &mut |path, bytes| {
        if let Ok(relative) = path.strip_prefix(root) {
            entries.insert(normalize_path(relative), bytes.to_vec());
        }
    })?;
    let mut environment = BTreeMap::new();
    for (key, value) in std::env::vars() {
        if matches!(
            key.as_str(),
            "RUSTFLAGS" | "CARGO_ENCODED_RUSTFLAGS" | "CARGO_BUILD_TARGET" | "SOLDR_RUSTC_WRAPPER"
        ) || key.starts_with("CARGO_PROFILE_")
            || key.starts_with("CARGO_TARGET_")
        {
            environment.insert(key, value);
        }
    }
    let mut digest = Sha256::new();
    for (path, bytes) in entries {
        hash_field(&mut digest, path.as_bytes());
        hash_field(&mut digest, &bytes);
    }
    hash_field(
        &mut digest,
        &serde_json::to_vec(args)
            .map_err(|error| SoldrError::Other(format!("shape encoding failed: {error}")))?,
    );
    hash_field(
        &mut digest,
        &serde_json::to_vec(&environment)
            .map_err(|error| SoldrError::Other(format!("environment encoding failed: {error}")))?,
    );
    hash_field(&mut digest, WRAPPER_IDENTITY.as_bytes());
    Ok(hex::encode(digest.finalize()))
}

fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

fn visit_semantic_files(
    root: &Path,
    callback: &mut dyn FnMut(&Path, &[u8]),
) -> Result<(), SoldrError> {
    fn visit(directory: &Path, callback: &mut dyn FnMut(&Path, &[u8])) -> Result<(), SoldrError> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".claude" | "target" | "node_modules")
                ) {
                    continue;
                }
                visit(&path, callback)?;
            } else if kind.is_file() {
                let name = path.file_name().and_then(|value| value.to_str());
                if matches!(name, Some("config.toml" | "config"))
                    || name == Some(concat!("Car", "go.toml"))
                    || name == Some(concat!("Car", "go.lock"))
                {
                    let bytes = std::fs::read(&path)?;
                    callback(&path, &bytes);
                }
            }
        }
        Ok(())
    }
    visit(root, callback)
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// soldr#2945: this used to join each declared `libraries[].path` **literally**
/// and read `<root>/<path>/rust-toolchain.toml`. This workspace declares
/// `{ path = "dylints/*" }`, so it read a manifest that does not exist, took
/// `read_rust_toolchain_manifest`'s missing-file default, found no
/// requirements, and silently fell through to the root *stable* channel — the
/// conflict branch below it could therefore never be reached. The glob-aware
/// read now lives in one place, preserves the all-inherit state for validation,
/// and is shared with `dylint_toolchain` and `ci_test::plan`.
fn configured_library_toolchain(root: &Path) -> Result<Option<String>, SoldrError> {
    match crate::dylint_libraries::toolchain_state(root)? {
        crate::dylint_libraries::LibraryToolchainState::NoLibraries => {
            Ok(read_rust_toolchain_manifest(root)?.channel)
        }
        crate::dylint_libraries::LibraryToolchainState::InheritRoot { libraries } => Ok(Some(
            crate::dylint_libraries::inherited_root_channel(root, &libraries)?,
        )),
        crate::dylint_libraries::LibraryToolchainState::Pinned { channel, .. } => Ok(Some(channel)),
    }
}

fn lock_target(target: &Path) -> Result<File, SoldrError> {
    let path = target.join(LOCK_NAME);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    lock_exclusive_announced(&file, &path, "target tree").map_err(|error| {
        SoldrError::Other(format!("failed to lock {}: {error}", path.display()))
    })?;
    Ok(file)
}

/// Take a cross-process cook lock, saying so when another cook holds it.
/// These locks are held for a whole cook, so a wait can be long; it must
/// never be silent (soldr#3815: degenerate states fail loudly).
fn lock_exclusive_announced(file: &File, path: &Path, what: &str) -> std::io::Result<()> {
    if file.try_lock_exclusive().is_ok() {
        return Ok(());
    }
    eprintln!(
        "soldr dylint cook: waiting for the {what} lock {} -- another cook holds it",
        path.display()
    );
    let started = std::time::Instant::now();
    file.lock_exclusive()?;
    eprintln!(
        "soldr dylint cook: acquired the {what} lock after {:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn lock_workspace_source(root: &Path) -> Result<File, SoldrError> {
    let path = workspace_source_lock_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    lock_exclusive_announced(&file, &path, "workspace source").map_err(|error| {
        SoldrError::Other(format!(
            "failed to lock workspace sources via {}: {error}",
            path.display()
        ))
    })?;
    Ok(file)
}

fn workspace_source_lock_path(root: &Path) -> PathBuf {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return dot_git.join("soldr-dylint-cook.lock");
    }
    if dot_git.is_file() {
        if let Ok(contents) = std::fs::read_to_string(&dot_git) {
            if let Some(git_dir) = contents.trim().strip_prefix("gitdir:") {
                let git_dir = PathBuf::from(git_dir.trim());
                let git_dir = if git_dir.is_absolute() {
                    git_dir
                } else {
                    root.join(git_dir)
                };
                return git_dir.join("soldr-dylint-cook.lock");
            }
        }
    }
    // Archive/non-git workspaces still need cross-nightly serialization.
    // Keep a stable advisory lock at the workspace boundary; it contains no
    // state and may be safely ignored by source archives.
    root.join(".soldr-dylint-cook.lock")
}

fn read_marker(path: &Path) -> Option<DylintCookMarker> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_marker(path: &Path, marker: &DylintCookMarker) -> Result<(), SoldrError> {
    let bytes = serde_json::to_vec(marker)
        .map_err(|error| SoldrError::Other(format!("marker encoding failed: {error}")))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    replace_marker_file(&temporary, path, |from, to| std::fs::rename(from, to))?;
    Ok(())
}

fn replace_marker_file(
    temporary: &Path,
    path: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Err(error) = rename(temporary, path) {
        // Windows rename does not replace an existing destination. The
        // workspace + target locks make this remove-then-rename fallback
        // safe; interruption can only omit the marker and force a recook.
        if path.exists() {
            std::fs::remove_file(path)?;
            rename(temporary, path)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

fn target_has_dependency_payload(target: &Path) -> bool {
    fn contains_file(directory: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && contains_file(&path) {
                return true;
            }
            if path.is_file()
                && !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(MARKER_NAME | LOCK_NAME)
                )
            {
                return true;
            }
        }
        false
    }
    contains_file(target)
}

#[cfg(test)]
#[path = "dylint_cook_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "dylint_cook_cachedir_tests.rs"]
mod cachedir_tag_tests;
