//! Optional presentation values. Missing measurements never become fake zeroes.
use crate::model::{Source, State};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Build {
    pub project: String,
    pub duration: Option<u64>,
    pub hits: Option<u64>,
    pub misses: Option<u64>,
    pub rate: Option<f64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerKind {
    Helper,
    Client,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Connected,
    Connecting,
    Disconnected,
    Unknown,
}
#[derive(Clone, Debug)]
pub struct Peer {
    pub name: String,
    pub kind: PeerKind,
    pub used: Option<u64>,
    pub total: Option<u64>,
    pub latency: Option<u64>,
    pub connection: Connection,
}

pub fn version(state: &State) -> Option<&str> {
    state.value(Source::Status)["soldr_version"].as_str()
}
pub fn rate(value: &Value) -> Option<f64> {
    value["hit_rate"]
        .as_f64()
        .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
        .or_else(|| {
            let hits = value["hits"].as_u64()?;
            let misses = value["misses"].as_u64()?;
            (hits + misses > 0).then(|| hits as f64 / (hits + misses) as f64)
        })
}
pub fn session_rate(state: &State) -> Option<f64> {
    rate(&state.value(Source::Session)["last_session"])
}
pub fn bytes(n: u64) -> String {
    if n >= 1_073_741_824 {
        format!("{:.1} GiB", n as f64 / 1_073_741_824.0)
    } else if n >= 1_048_576 {
        format!("{:.0} MiB", n as f64 / 1_048_576.0)
    } else {
        format!("{n} bytes")
    }
}
pub fn builds(state: &State) -> Vec<Build> {
    state.value(Source::Builds)["launches"]
        .as_array()
        .into_iter()
        .flatten()
        .take(12)
        .map(|v| {
            let project = v["repo_root"]
                .as_str()
                .unwrap_or("Build")
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("Build")
                .to_string();
            Build {
                project,
                duration: v["duration_ms"].as_u64().filter(|n| *n > 0),
                hits: v["cache"]["hits"].as_u64(),
                misses: v["cache"]["misses"].as_u64(),
                rate: rate(&v["cache"]),
            }
        })
        .collect()
}
impl Build {
    pub fn duration_label(&self) -> String {
        self.duration
            .map_or_else(String::new, |n| format!("{:.1}s", n as f64 / 1000.0))
    }
    pub fn cached_label(&self) -> String {
        match (self.hits, self.misses) {
            (Some(h), Some(m)) if h + m > 0 => format!("{h} of {} cached", h + m),
            _ => String::new(),
        }
    }
    pub fn summary(&self) -> String {
        [
            self.project.clone(),
            self.duration_label(),
            self.cached_label(),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
    }
}
pub fn peers(state: &State) -> Vec<Peer> {
    let dist = state.value(Source::Dist);
    [("nodes", PeerKind::Helper), ("clients", PeerKind::Client)]
        .into_iter()
        .flat_map(|(key, kind)| {
            dist[key]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |v| Peer {
                    name: v["name"].as_str().unwrap_or("Device").into(),
                    kind,
                    used: v["slots_used"].as_u64(),
                    total: v["slots_total"].as_u64().filter(|n| *n > 0),
                    latency: v["rtt_ms"].as_u64().filter(|n| *n > 0),
                    connection: match v["connection_state"]
                        .as_str()
                        .or_else(|| v["state"].as_str())
                    {
                        Some("connected" | "ready") => Connection::Connected,
                        Some("connecting") => Connection::Connecting,
                        Some("disconnected") => Connection::Disconnected,
                        _ => {
                            if v["connected"] == true {
                                Connection::Connected
                            } else {
                                Connection::Unknown
                            }
                        }
                    },
                })
        })
        .collect()
}
pub fn cache(state: &State) -> Option<(u64, u64)> {
    let c = state.value(Source::Cache);
    let used = c["on_disk_bytes"].as_u64().filter(|n| *n > 0)?;
    let budget = c["budget_bytes"]
        .as_u64()
        .or_else(|| c["last_pass_budget_bytes"].as_u64())
        .filter(|n| *n > 0)?;
    Some((used, budget))
}
pub fn cache_label(state: &State) -> Option<String> {
    cache(state).map(|(used, budget)| {
        format!(
            "Cache  {}  {} of {} GiB",
            progress_glyphs(used, budget, 8),
            format_gib(used),
            format_gib(budget)
        )
    })
}
fn format_gib(n: u64) -> String {
    let n = n as f64 / 1_073_741_824.0;
    if n.fract() < 0.05 {
        format!("{n:.0}")
    } else {
        format!("{n:.1}")
    }
}
/// Compatibility boundary for the forthcoming facade progress bar.
pub fn progress_glyphs(used: u64, total: u64, width: usize) -> String {
    let count = if total == 0 {
        0
    } else {
        ((used.min(total) as f64 / total as f64) * width as f64).round() as usize
    };
    format!("[{}{}]", "#".repeat(count), "-".repeat(width - count))
}
pub fn sparkline(builds: &[Build]) -> Option<Vec<f64>> {
    let rates: Vec<_> = builds.iter().rev().filter_map(|b| b.rate).collect();
    (rates.len() >= 2).then_some(rates)
}
pub fn contribution(state: &State) -> Option<String> {
    let v = state.value(Source::Dist);
    let mut rows = Vec::new();
    for (key, label) in [("joblets_remote", "remote"), ("joblets_local", "local")] {
        if let Some(n) = v[key].as_u64().filter(|n| *n > 0) {
            rows.push(format!("{n} {label}"));
        }
    }
    for (key, label) in [("bytes_transferred", "sent"), ("bytes_saved", "saved")] {
        if let Some(n) = v[key].as_u64().filter(|n| *n > 0) {
            rows.push(format!("{label} {}", bytes(n)));
        }
    }
    (!rows.is_empty()).then(|| rows.join(" · "))
}
