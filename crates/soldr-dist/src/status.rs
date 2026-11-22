//! Local protobuf snapshot: report the serving image, not the querying image.
use crate::{cas::Cas, proto::Status, trust::Trust, JobMetrics, Result};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};

/// Hash once at node startup, before a deployment can replace its pathname.
pub fn build_id() -> Result<String> {
    let mut file = std::fs::File::open(std::env::current_exe()?)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn load(root: &Path) -> Result<Status> {
    let path = root.join("status.pb");
    let mut status = if path.exists() {
        Status::decode(std::fs::read(path)?.as_slice())?
    } else {
        Status {
            node_id: crate::trust::node_key(root)?.public().to_string(),
            role: "client".into(),
            ..Status::default()
        }
    };
    if status.pid == 0 || !soldr_platform::process::inspect::is_alive(status.pid as u32) {
        status.role = "client".into();
        status.running_joblets = 0;
        status.pid = 0;
        status.build_id.clear();
    }
    status.trusted_clients = Trust::load(&root.join("trusted_clients.toml"))?.peers;
    status.trusted_workers = Trust::load(&root.join("trusted_workers.toml"))?.peers;
    status.cas_bytes = Cas::new(root, u64::MAX)?.size()?;
    let last_job = root.join("last-job.pb");
    if last_job.exists() {
        status.last_job = Some(JobMetrics::decode(std::fs::read(last_job)?.as_slice())?);
    }
    Ok(status)
}

pub fn json(status: &Status) -> serde_json::Value {
    serde_json::json!({
        "node_id": status.node_id, "role": status.role,
        "trusted_clients": status.trusted_clients, "trusted_workers": status.trusted_workers,
        "cas_bytes": status.cas_bytes, "running_joblets": status.running_joblets,
        "pid": status.pid, "build_id": status.build_id, "signal_url": status.signal_url,
        "last_job": status.last_job,
    })
}

pub fn record_job(root: &Path, metrics: &JobMetrics) -> Result<()> {
    use fs2::FileExt;
    use std::io::Write;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root.join("metrics.lock"))?;
    // Called on the blocking pool; concurrent joblets serialize here briefly
    // instead of failing with EWOULDBLOCK and dropping their metrics.
    lock.lock_exclusive()?;
    let path = root.join("last-job.pb");
    if path.exists() {
        let previous = JobMetrics::decode(std::fs::read(&path)?.as_slice())?;
        if previous.completed_unix_millis > metrics.completed_unix_millis {
            return Ok(());
        }
    }
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&metrics.encode_to_vec())?;
    file.persist(path)?;
    Ok(())
}
