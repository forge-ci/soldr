//! Native host-validation DAG for `soldr ci-test` (soldr#2867).

mod canonical_profile;
mod dylint_library_marker;
mod dylint_target_guard;
mod execute;
mod execute_report;
mod executor_contract;
mod model;
mod nextest_resident_lease;
mod parse;
pub(crate) mod plan;
mod policy_prefetch;
mod test_admission;
mod test_pressure;
mod test_targets;

use crate::core::SoldrError;
use model::OutputFormat;

pub(crate) async fn run(
    args: &[String],
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    let invocation = parse::parse(args)?;
    let plan = plan::freeze(&invocation, cache_enabled).await?;
    // soldr#2936: advisory, on stderr, before anything compiles — and before
    // the explain branch, so `--explain-plan` surfaces it too (the plan JSON
    // goes to stdout, so the two never interleave). This never touches the
    // exit code: soldr's own workspace is expected to trip it until
    // soldr#2931's consolidation lands, and warning about ourselves honestly
    // is the point.
    test_targets::warn_if_excessive(plan.test_target_count, plan.test_target_warn_threshold);
    if invocation.explain {
        // soldr#3460: an explained plan must be one the executor accepts.
        // The planner and the executor contract changed apart once and only
        // a real CI run noticed; validating here puts every `--explain-plan`
        // guard test on the executor's side of that contract too.
        execute::validate_executor_contract(&plan)?;
        render(&plan, invocation.format)?;
        return Ok(0);
    }
    execute::run(&plan, cache_enabled, trust_inherited_soldr_env).await
}

fn render(plan: &model::CiTestPlan, format: OutputFormat) -> Result<(), SoldrError> {
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(plan).map_err(|error| {
                SoldrError::Other(format!(
                    "soldr ci-test: could not encode plan JSON: {error}"
                ))
            })?
        ),
        OutputFormat::Human => {
            println!("soldr ci-test plan v{}", plan.schema_version);
            println!("  workspace: {}", plan.workspace_root);
            println!("  host: {}", plan.host_triple);
            println!("  target: {}", plan.target_triple);
            if let Some(archive) = &plan.archive_file {
                println!("  compile-only archive: {archive}");
            }
            println!(
                "  integration-test link targets: {} (warn above {})",
                plan.test_target_count, plan.test_target_warn_threshold
            );
            println!("  nextest admission: {}", plan.test_admission.summary());
            if let Some(warning) = &plan.test_admission.warning {
                println!("  nextest admission warning: {warning}");
            }
            println!("  domains:");
            for domain in &plan.domains {
                println!(
                    "    {} ({}) {} -> {} [{}]",
                    domain.id,
                    domain.family,
                    domain.toolchain,
                    domain.target_directory,
                    domain.profile
                );
            }
            println!("  stages:");
            for stage in &plan.stages {
                println!("    {}: {}", stage.name, stage.command.join(" "));
            }
            for step in &plan.subsumed_steps {
                println!("  {}: {}", step.name, step.subsumed_by);
            }
        }
    }
    Ok(())
}

pub(crate) use test_targets::workspace_member_names;
