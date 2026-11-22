//! Keep standalone lint-library builds in one compiler/home and path domain.
use running_process::broker::protocol_v2::SessionEnvVar;
use std::path::Path;
use std::process::Command;

use crate::core::SoldrPaths;

pub(crate) const BUILDING_LIBRARIES: &str = "DYLINT_BUILDING_LIBRARIES";
pub(crate) const SOURCE_ROOT: &str = "SOLDR_DYLINT_LIBRARY_SOURCE_ROOT";

/// Homes follow the selected compiler, including when the caller checkout has
/// a `.cargo/config.toml` directory that is not a registry/toolchain home.
pub(crate) fn apply_homes(
    command: &mut Command,
    cargo: &Path,
    paths: &SoldrPaths,
    caller_home: Option<&std::ffi::OsStr>,
) {
    if caller_home.is_none()
        && crate::binaries::home_origin_for_binary(cargo, paths)
            == crate::binaries::HomeOrigin::Managed
    {
        command.env(
            crate::core::CARGO_HOME_ENV_VAR,
            crate::fetch::managed_cargo_home(paths),
        );
    }
}

/// `cargo-dylint` supplies a checkout-local symlink as `--target-dir`. Resolve
/// it before building, so rustc output/search/extern paths use the real store.
pub(crate) fn canonical_target_args(args: &[String]) -> Vec<String> {
    let mut args = args.to_vec();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--" {
            break;
        }
        if args[index] == "--target-dir" {
            if let Some(value) = args.get_mut(index + 1) {
                canonicalize_target(value);
            }
            index += 2;
        } else {
            if let Some(value) = args[index].strip_prefix("--target-dir=") {
                let mut value = value.to_owned();
                canonicalize_target(&mut value);
                args[index] = format!("--target-dir={value}");
            }
            index += 1;
        }
    }
    args
}

fn canonicalize_target(value: &mut String) {
    // A cold store has no toolchain child yet. Create the directory the build
    // would create anyway, then resolve its checkout-local parent symlink.
    let _ = std::fs::create_dir_all(&*value);
    if let Ok(path) = Path::new(value).canonicalize() {
        *value = path.to_string_lossy().into_owned();
    }
}

/// The library target is outside the source checkout. Use it as the cache key
/// root, with a matching rustc remap injected by zccache's existing auto mode.
pub(crate) fn apply_target_remap(command: &mut Command, args: &[String]) {
    if !crate::toolchain_dir_name::effective_command_env(
        command,
        crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR,
    )
    .is_some_and(|value| value.eq_ignore_ascii_case("auto"))
    {
        return;
    }
    let cwd = command
        .get_current_dir()
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let target = crate::core::cargo_target_dir::resolve_cargo_target_dir(
        &crate::core::cargo_target_dir::CargoTargetDirInputs::from_process(&cwd, args),
    );
    if let Some(target) = target {
        if let Some(source) = crate::toolchain_dir_name::effective_command_env(command, SOURCE_ROOT)
            .or_else(|| {
                crate::toolchain_dir_name::effective_command_env(
                    command,
                    crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR,
                )
            })
        {
            command.env(SOURCE_ROOT, source);
        }
        command.env(crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR, target);
    }
}

/// Registry dependencies share the external target domain. Lint-local units
/// keep the source domain so identical lints can hit across checkouts sharing
/// that target. Change only the request, never this process's environment.
pub(crate) fn apply_compile_root(env: &mut [SessionEnvVar]) {
    let get = |key: &str| {
        env.iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
    };
    if !get(BUILDING_LIBRARIES).is_some_and(crate::core::flag_value)
        || !get(crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR)
            .is_some_and(|value| value.eq_ignore_ascii_case("auto"))
    {
        return;
    }
    let Some(source) = get(SOURCE_ROOT).map(str::to_owned) else {
        return;
    };
    if !get("CARGO_MANIFEST_DIR").is_some_and(|manifest| Path::new(manifest).starts_with(&source)) {
        return;
    }
    if let Some(root) = env
        .iter_mut()
        .find(|entry| entry.key == crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR)
    {
        root.value = source;
    }
}

#[cfg(test)]
#[path = "dylint_library_cache_tests.rs"]
mod tests;
