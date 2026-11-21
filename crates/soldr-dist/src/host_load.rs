//! Dist admission shares CPU capacity with the GitHub runner dispatcher.
use serde::Deserialize;
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostLoad {
    pub gh_jobs_running: u64,
    pub gh_cpus_reserved: usize,
    pub cap: usize,
    /// Unix epoch seconds, including fractional seconds if supplied by time.time().
    pub updated_at: f64,
}
impl HostLoad {
    pub fn limit(&self, configured: usize, total_cpus: usize, now: f64) -> usize {
        if !self.updated_at.is_finite()
            || self.updated_at > now + 5.0
            || now - self.updated_at > 30.0
        {
            return configured.min(1);
        }
        configured.min(total_cpus.saturating_sub(self.gh_cpus_reserved))
    }
}
pub fn limit(path: Option<&Path>, configured: usize, total_cpus: usize) -> usize {
    let Some(path) = path else {
        return configured.min(total_cpus);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let load = std::fs::read(path)
        .map_err(crate::Error::from)
        .and_then(|bytes| serde_json::from_slice::<HostLoad>(&bytes).map_err(crate::Error::from));
    match load {
        Ok(load) => load.limit(configured, total_cpus, now),
        Err(e) => {
            eprintln!(
                "soldr dist: host load unavailable at {}: {e}; allowing at most one joblet",
                path.display()
            );
            configured.min(1)
        }
    }
}
pub fn free_slots(limit: usize, running: usize) -> usize {
    limit.saturating_sub(running)
}
