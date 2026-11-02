//! Uplift Dylint's toolchain-suffixed library copy (soldr#3989).
//!
//! `cargo-dylint` looks for `lib<name>@<toolchain>.<ext>` in the profile
//! directory (`<target>/release/`). `dylint-link` writes that copy itself
//! (soldr sets `DYLINT_LINK_ENABLE_COPY_LIBRARY`, see `dylint_toolchain.rs`),
//! deriving the profile directory from rustc's output path. Under the cached
//! rustc wrapper, the copy is materialized as a link output beside the plain
//! library in rustc's `--out-dir`, which with Cargo's build-dir layout is
//! `<profile>/build/<package>/<hash>/out`. Cargo uplifts only the plain
//! library, so the suffixed copy never reached `<profile>/` and cargo-dylint
//! failed with "Could not find ... despite successful build".
//!
//! After a successful `soldr-dylint` rustc invocation this module copies any
//! suffixed library from such an out-dir up to the profile directory, which is
//! what `dylint-link`'s own `strip_build` would have done.

use std::path::{Path, PathBuf};

/// Copies every `lib<crate>@*<dll suffix>` in rustc's build-dir-layout
/// `--out-dir` up to the profile directory. Best effort: the build already
/// succeeded, and cargo-dylint reports a missing library on its own.
pub(crate) fn uplift_suffixed_library(rustc_args: &[String]) {
    let Some(plan) = UpliftPlan::from_rustc_args(rustc_args) else {
        return;
    };
    for source in plan.suffixed_libraries() {
        let Some(name) = source.file_name() else {
            continue;
        };
        let dest = plan.profile_dir.join(name);
        if files_equal(&source, &dest) {
            continue;
        }
        if let Err(err) = std::fs::copy(&source, &dest) {
            eprintln!(
                "soldr-dylint: could not copy {} to {}: {err}",
                source.display(),
                dest.display()
            );
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UpliftPlan {
    pub(crate) out_dir: PathBuf,
    pub(crate) profile_dir: PathBuf,
    pub(crate) crate_name: String,
}

impl UpliftPlan {
    /// A plan only for a cdylib whose `--out-dir` is
    /// `<profile>/build/<package>/<16-hex hash>/out`.
    pub(crate) fn from_rustc_args(args: &[String]) -> Option<Self> {
        let crate_name = flag_value(args, "--crate-name")?;
        if !flag_values(args, "--crate-type").any(|value| value == "cdylib") {
            return None;
        }
        let out_dir = PathBuf::from(flag_value(args, "--out-dir")?);
        let profile_dir = profile_dir_of_build_out(&out_dir)?;
        Some(Self {
            out_dir,
            profile_dir,
            crate_name: crate_name.to_string(),
        })
    }

    fn suffixed_libraries(&self) -> Vec<PathBuf> {
        let prefix = format!("{}{}@", std::env::consts::DLL_PREFIX, self.crate_name);
        let Ok(entries) = std::fs::read_dir(&self.out_dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| is_suffixed_library_name(name, &prefix))
            })
            .collect()
    }
}

pub(crate) fn is_suffixed_library_name(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix) && name.ends_with(std::env::consts::DLL_SUFFIX)
}

/// `<profile>/build/<package>/<hash>/out` -> `<profile>`.
pub(crate) fn profile_dir_of_build_out(out_dir: &Path) -> Option<PathBuf> {
    if out_dir.file_name()? != "out" {
        return None;
    }
    let hash_dir = out_dir.parent()?;
    let hash = hash_dir.file_name()?.to_str()?;
    if hash.len() != 16 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let build_dir = hash_dir.parent()?.parent()?;
    if build_dir.file_name()? != "build" {
        return None;
    }
    build_dir.parent().map(Path::to_path_buf)
}

/// Equal content means no copy, so the uplifted file's mtime does not change
/// and Cargo does not rebuild the crates rustc recorded it as a dependency of.
fn files_equal(lhs: &Path, rhs: &Path) -> bool {
    match (std::fs::read(lhs), std::fs::read(rhs)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn flag_values<'a>(args: &'a [String], flag: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    let eq_prefix = format!("{flag}=");
    args.iter().enumerate().filter_map(move |(index, arg)| {
        if arg == flag {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix(eq_prefix.as_str())
        }
    })
}

fn flag_value<'a>(args: &'a [String], flag: &'a str) -> Option<&'a str> {
    flag_values(args, flag).next()
}

#[cfg(test)]
#[path = "dylint_library_uplift_tests.rs"]
mod tests;
