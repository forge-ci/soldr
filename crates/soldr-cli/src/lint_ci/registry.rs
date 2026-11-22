//! Policy-rule registry for `soldr lint ci` (soldr#2038).
//!
//! Adding a new CI policy is a bounded change: implement [`CiRule`] in its own
//! module and append it to [`rules`]. Each rule receives the pre-scanned CI
//! surfaces and returns structured [`Finding`]s; suppression and rendering are
//! handled centrally by the engine.

use super::cross_compile_surface::CrossCompileSurface;
use super::model::Finding;
use super::scan::ScannedFile;

/// A single, independent CI policy check.
pub trait CiRule {
    /// Stable rule identifier (kebab-case), e.g. `cross-compile-surface`.
    fn id(&self) -> &'static str;

    /// Inspect every scanned surface and return raw findings (before
    /// suppression is applied).
    fn check(&self, files: &[ScannedFile]) -> Vec<Finding>;
}

/// The registered CI policy rules, in evaluation order.
pub fn rules(root: Option<&std::path::Path>) -> Vec<Box<dyn CiRule>> {
    vec![
        Box::new(CrossCompileSurface),
        Box::new(super::canonical_invocation::CanonicalInvocation::new(root)),
    ]
}
