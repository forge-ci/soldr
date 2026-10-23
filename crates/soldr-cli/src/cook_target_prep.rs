//! Blessed target prep for `soldr cook --target`.
//!
//! `soldr build --target X` applies the target's blessed prep (catalogue
//! linker, `CC_<t>`, sysroot, rustflags) before cargo runs. `soldr cook`
//! routed `cargo chef cook --target X` straight to the front door, whose prep
//! gate only recognises build-like subcommands, so the cook compiled against
//! the host environment. Every cooked unit then fingerprinted differently from
//! the build that followed and was rebuilt: the whole cook was wasted work,
//! and consumers turned it off (zackees/clud#2037). Applying the same prep
//! here makes the cook produce exactly the units the build reuses.

use crate::blessed_build::BlessedPrep;
use crate::cook::CookArgs;
use crate::core::{SoldrError, SoldrPaths};

/// The triple a cook must prepare, or `None` for a host cook or a custom
/// target-spec path, which Cargo resolves itself (as `soldr build` does).
pub(crate) fn cook_prep_triple(args: &CookArgs) -> Option<String> {
    let mut target = vec![args.target.clone()?];
    crate::target_alias::normalize_target_aliases_in_args(&mut target);
    let triple = target.pop()?;
    crate::core::is_canonical(&triple).then_some(triple)
}

/// Apply the blessed prep for this cook's target to the process, so every
/// cargo the cook spawns sees the environment `soldr build` would.
pub(crate) async fn apply_cook_target_prep(
    args: &CookArgs,
) -> Result<Option<BlessedPrep>, SoldrError> {
    let Some(triple) = cook_prep_triple(args) else {
        return Ok(None);
    };
    crate::target_lifecycle::provision_target_toolchain(&triple)?;
    let paths = SoldrPaths::new()?;
    let feature_args = crate::target_lifecycle::cargo_feature_flags(&args.passthrough);
    let prep =
        crate::target_lifecycle::prepare_for_invocation(&paths, &triple, &feature_args).await?;
    crate::target_lifecycle::apply_to_process(&prep);
    if !prep.cargo_args.is_empty() {
        // cargo-chef's `cook` takes no `--config`, so `links` syslib
        // overrides reach only the exact-build phase.
        eprintln!(
            "soldr cook: note: {triple} uses Cargo --config syslib overrides; \
             cargo-chef cannot take them, so crates with `links` may rebuild"
        );
    }
    Ok(Some(prep))
}

#[cfg(test)]
#[path = "cook_target_prep_tests.rs"]
mod tests;
