//! Fresh bounded host probes. Output is captured in private temporary files so
//! even a pipe-filling child cannot deadlock or grow the agent's resident memory.
use crate::{
    config::Config,
    proto::{Check, Dist, Load},
    Result,
};
use soldr_platform::platform::{
    fs::volume,
    process::{command, terminate},
};
use std::{
    io::{Read, Seek, SeekFrom},
    process::{Child, Command, Stdio},
    time::Duration,
};

pub const PROBE_LIMIT: Duration = Duration::from_millis(500);
const OUTPUT_LIMIT: u64 = 16 * 1024;
pub struct Probe {
    pub ok: bool,
    pub stdout: String,
    pub detail: String,
}
struct ProcessGroup(Child);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Also kill descendants that kept running after their parent exited.
        let _ = terminate::terminate_tree(&mut self.0);
        let _ = soldr_platform::platform::wait::wait_for(&mut self.0, Duration::from_millis(100));
    }
}
pub async fn probe(program: &str, args: &[String]) -> Result<Probe> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    command::configure_process_group(&mut cmd);
    let child = cmd.spawn()?;
    let mut group = ProcessGroup(child);
    let wait = async {
        loop {
            if let Some(status) = group.0.try_wait()? {
                return Ok::<_, std::io::Error>(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let result = tokio::time::timeout(PROBE_LIMIT, wait).await;
    // Drop kills the entire group before reading output, including on timeout.
    tokio::task::spawn_blocking(move || drop(group)).await?;
    let (out, output_truncated) = read_output(&mut stdout)?;
    let (err, stderr_truncated) = read_output(&mut stderr)?;
    let (ok, result) = match result {
        Ok(Ok(status)) => (status.success(), format!("exit {status}")),
        Ok(Err(error)) => (false, format!("wait failed: {error}")),
        Err(_) => (
            false,
            "timed out after 500 ms; process group terminated".into(),
        ),
    };
    Ok(Probe {
        ok: ok && !output_truncated,
        stdout: out,
        detail: format!(
            "{result}; stdout truncated: {output_truncated}; stderr: {}{}",
            truncate(&err),
            if stderr_truncated { " [truncated]" } else { "" }
        ),
    })
}
fn read_output(file: &mut std::fs::File) -> Result<(String, bool)> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > OUTPUT_LIMIT as usize;
    bytes.truncate(OUTPUT_LIMIT as usize);
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}
fn truncate(text: &str) -> String {
    let mut short: String = text.chars().take(256).collect();
    if text.chars().count() > 256 {
        short.push_str(" [truncated]");
    }
    short
}
pub async fn capacity(config: &Config) -> (Load, Option<Dist>, Vec<Check>) {
    let cpus =
        std::thread::available_parallelism().map_or(1, |v| v.get().min(u32::MAX as usize) as u32);
    let mut load = Load {
        cpus,
        ..Load::default()
    };
    let mut checks = Vec::new();
    if let Some(path) = &config.host_load_file {
        match read_json::<Load>(path).filter(|l| l.cpus > 0) {
            Some(value) => load = value,
            None => {
                load.gh_cpus_reserved = cpus;
                checks.push(Check {
                    name: "host_load".into(),
                    ok: false,
                    detail: "configured load file missing or invalid; all CPUs reserved".into(),
                });
            }
        }
    }
    let mut dist = None;
    if let Some(cmd) = &config.dist_status_cmd {
        match probe(&cmd[0], &cmd[1..]).await {
            Ok(p) => {
                dist = serde_json::from_str::<Dist>(&p.stdout)
                    .ok()
                    .filter(|d| p.ok && valid_dist(d));
                checks.push(Check {
                    name: "dist".into(),
                    ok: dist.is_some(),
                    detail: format!("{}; valid dist status: {}", p.detail, dist.is_some()),
                });
            }
            Err(e) => checks.push(Check {
                name: "dist".into(),
                ok: false,
                detail: e.to_string(),
            }),
        }
    }
    if !checks.iter().all(|c| c.ok) {
        if let Some(d) = &mut dist {
            d.slots_free = 0;
        }
    }
    (load, dist, checks)
}
fn valid_dist(d: &Dist) -> bool {
    !d.node_id.is_empty()
        && d.node_id.len() <= 256
        && d.slots_free <= d.slots_total
        && d.relay_url
            .as_ref()
            .is_none_or(|s| s.len() <= 2048 && s.starts_with("https://"))
}
fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let file = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    file.take(OUTPUT_LIMIT + 1).read_to_end(&mut data).ok()?;
    if data.len() > OUTPUT_LIMIT as usize {
        return None;
    }
    serde_json::from_slice(&data).ok()
}
pub async fn collect(config: &Config) -> Vec<Check> {
    let services = config.units.iter().map(|unit| async {
        let result = probe("systemctl", &["is-active".into(), unit.clone()]).await;
        match result {
            Ok(p) => Check {
                name: format!("systemd:{unit}"),
                ok: p.ok && p.stdout.trim() == "active",
                detail: format!("{}; stdout: {}", p.detail, truncate(&p.stdout)),
            },
            Err(e) => Check {
                name: format!("systemd:{unit}"),
                ok: false,
                detail: e.to_string(),
            },
        }
    });
    let (mut checks, (_, _, capacity_checks)) =
        tokio::join!(futures_util::future::join_all(services), capacity(config));
    checks.extend(capacity_checks);
    for path in [std::path::Path::new("/"), config.soldr_root.as_path()] {
        let space = volume::free_bytes(path)
            .and_then(|free| fs2::total_space(path).map(|total| (free, total)));
        let (ok, detail) = match space {
            Ok((free, total)) => (
                total > 0 && u128::from(free) * 100 > u128::from(total) * 5,
                format!("{free} bytes free of {total}; requires more than 5%"),
            ),
            Err(e) => (false, format!("disk probe failed: {e}")),
        };
        checks.push(Check {
            name: format!("disk:{}", path.display()),
            ok,
            detail,
        });
    }
    bound_checks(&mut checks);
    checks
}

/// At most 36 checks, each at most 400 encoded bytes: even adversarial
/// escaped output stays below signal's 16 KiB frame limit, including envelope.
fn bound_checks(checks: &mut [Check]) {
    for check in checks {
        if let Some((offset, _)) = check.name.char_indices().nth(128) { check.name.truncate(offset); }
        let mut shortened = false;
        if let Some((offset, _)) = check.detail.char_indices().nth(256) {
            check.detail.truncate(offset);
            shortened = true;
        }
        while serde_json::to_string(&check.name).is_ok_and(|s| s.len() > 128) {
            check.name.pop();
        }
        while serde_json::to_vec(&*check).is_ok_and(|s| s.len() > 380) {
            if check.detail.pop().is_none() {
                break;
            }
            shortened = true;
        }
        if shortened {
            check.detail.push_str(" [truncated]");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn largest_health_response_fits_even_with_json_escape_amplification() {
        let mut checks = vec![
            Check {
                name: "\u{1}".repeat(512),
                ok: false,
                detail: "\u{1}".repeat(16384)
            };
            36
        ];
        bound_checks(&mut checks);
        assert_eq!(checks.len(), 36);
        assert!(checks
            .iter()
            .all(|c| !c.ok && c.detail.ends_with(" [truncated]")));
        let pong = crate::proto::FromHost::Pong {
            id: u64::MAX,
            checks,
        };
        assert!(serde_json::to_vec(&pong).unwrap().len() < crate::proto::MAX_MESSAGE);
    }
}
