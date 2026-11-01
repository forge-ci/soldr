//! `soldr cache stats` / `soldr cache gc`: the embedded zccache store's size
//! against its cap (soldr#4028).
//!
//! Eviction itself is zccache's: its disk maintenance removes the
//! least-recently-used artifacts until the store fits the budget soldr hands
//! it at service start. soldr's part is choosing a bounded default budget,
//! surfacing it, and letting a user run the pass on demand.

use super::print_json;
use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::maintenance::MaintenanceStatus;
use crate::JSON_SCHEMA_VERSION;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct CacheStatsOutput {
    schema_version: u32,
    command: &'static str,
    store_dir: String,
    on_disk_bytes: u64,
    files: u64,
    /// Fixed byte cap, or `None` when a percentage / zccache's dynamic
    /// default applies.
    budget_bytes: Option<u64>,
    budget_percent: Option<u8>,
    budget_source: String,
    /// The budget and usage the last daemon maintenance pass measured.
    last_pass_budget_bytes: Option<u64>,
    last_pass_usage_bytes: Option<u64>,
    over_budget: bool,
}

fn collect_stats(paths: &SoldrPaths) -> Result<CacheStatsOutput, SoldrError> {
    let store_dir = crate::zccache::managed_zccache_cache_dir(paths)?;
    let (on_disk_bytes, files) = crate::cache_lib::dir_footprint::measure(&store_dir);
    let policy = crate::zccache_embedded::resolve_disk_policy(paths)
        .map_err(|error| SoldrError::Other(error.to_string()))?;
    let last = crate::daemon::maintenance::read_status(paths)
        .and_then(|status| status.zccache)
        .map(|report| (report.budget_bytes, report.usage_after_bytes));
    Ok(build_stats(
        store_dir.display().to_string(),
        on_disk_bytes,
        files,
        policy,
        last,
    ))
}

fn build_stats(
    store_dir: String,
    on_disk_bytes: u64,
    files: u64,
    policy: crate::zccache_embedded::EmbeddedDiskPolicy,
    last: Option<(u64, u64)>,
) -> CacheStatsOutput {
    // The configured byte cap is authoritative; without one, fall back to
    // the budget zccache itself resolved on the last pass.
    let effective_budget = policy.max_cache_bytes.or(last.map(|(budget, _)| budget));
    CacheStatsOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache stats",
        store_dir,
        on_disk_bytes,
        files,
        budget_bytes: policy.max_cache_bytes,
        budget_percent: policy.max_cache_percent,
        budget_source: policy.source,
        last_pass_budget_bytes: last.map(|(budget, _)| budget),
        last_pass_usage_bytes: last.map(|(_, usage)| usage),
        over_budget: effective_budget.is_some_and(|budget| on_disk_bytes > budget),
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn print_stats(stats: &CacheStatsOutput) {
    println!("zccache store:  {}", stats.store_dir);
    println!(
        "on disk:        {} ({} files)",
        gib(stats.on_disk_bytes),
        stats.files
    );
    let cap = match (stats.budget_bytes, stats.budget_percent) {
        (Some(bytes), _) => gib(bytes),
        (None, Some(percent)) => format!("{percent}% of volume"),
        (None, None) => "zccache dynamic default".to_string(),
    };
    println!("size cap:       {cap} (source: {})", stats.budget_source);
    if let (Some(budget), Some(usage)) = (stats.last_pass_budget_bytes, stats.last_pass_usage_bytes)
    {
        println!(
            "last pass:      {} of {} artifacts",
            gib(usage),
            gib(budget)
        );
    }
    if stats.over_budget {
        println!("status:         over cap; run `soldr cache gc` to evict LRU artifacts");
    }
    println!(
        "configure with: soldr config set zccache.max_size_gb <N>  (0 = zccache dynamic default)"
    );
}

pub(crate) fn run_cache_stats_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let stats = collect_stats(&paths)?;
    if json {
        print_json(&stats)
    } else {
        print_stats(&stats);
        Ok(())
    }
}

#[derive(Serialize)]
struct CacheGcOutput {
    schema_version: u32,
    command: &'static str,
    /// `"ran"` or `"daemon_live"`.
    outcome: &'static str,
    daemon_pid: Option<u32>,
    maintenance: Option<MaintenanceStatus>,
    stats: CacheStatsOutput,
}

pub(crate) async fn run_cache_gc_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    if let Some(pid) = crate::daemon::lifecycle::claimed_daemon_occupies_route(&paths) {
        let stats = collect_stats(&paths)?;
        if json {
            return print_json(&CacheGcOutput {
                schema_version: JSON_SCHEMA_VERSION,
                command: "cache gc",
                outcome: "daemon_live",
                daemon_pid: Some(pid),
                maintenance: None,
                stats,
            });
        }
        println!(
            "soldr-daemon pid {pid} owns this cache and enforces the size cap every 5 minutes."
        );
        println!(
            "The cap is read when the daemon starts; run `soldr cache shutdown` then `soldr cache gc` to apply a changed cap now."
        );
        print_stats(&stats);
        return Ok(());
    }
    std::fs::create_dir_all(&paths.root)?;
    let status = crate::daemon::maintenance::run_manual_root(paths.root.clone())
        .await
        .map_err(SoldrError::Other)?;
    let stats = collect_stats(&paths)?;
    let completed = status.successful_at_ms.is_some();
    let deferred = status.deferred_reason.clone();
    if json {
        print_json(&CacheGcOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "cache gc",
            outcome: "ran",
            daemon_pid: None,
            maintenance: Some(status),
            stats,
        })?;
    } else {
        super::print_maintenance_status(Some(&status));
        println!();
        print_stats(&stats);
    }
    if !completed {
        return Err(SoldrError::Other(format!(
            "cache gc did not complete: {}",
            deferred.as_deref().unwrap_or("component failure")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zccache_embedded::EmbeddedDiskPolicy;

    fn policy(bytes: Option<u64>) -> EmbeddedDiskPolicy {
        EmbeddedDiskPolicy {
            source: "config_max_size_gb".to_string(),
            max_cache_bytes: bytes,
            max_cache_percent: None,
        }
    }

    #[test]
    fn over_budget_uses_configured_cap_first() {
        let stats = build_stats("s".into(), 30, 3, policy(Some(20)), Some((100, 10)));
        assert!(stats.over_budget);
        assert_eq!(stats.budget_bytes, Some(20));
        let stats = build_stats("s".into(), 10, 1, policy(Some(20)), None);
        assert!(!stats.over_budget);
    }

    #[test]
    fn dynamic_budget_falls_back_to_last_pass() {
        let stats = build_stats("s".into(), 50, 1, policy(None), Some((40, 39)));
        assert!(stats.over_budget);
        let stats = build_stats("s".into(), 50, 1, policy(None), None);
        assert!(!stats.over_budget);
    }
}
