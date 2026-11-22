//! Trusted single-worker execution; transport loss is a visible local-fallback decision.
use crate::{
    cas::Cas,
    client_connections::ClientConnections,
    proto::*,
    transport::{self, VERSION},
    trust::Trust,
    DistJob, DistOutcome, JobMetrics, Result,
};
use iroh::{Endpoint, EndpointAddr};
use soldr_platform::sync::AsyncMutex as Mutex;
use std::{
    collections::HashSet,
    fs,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::task::JoinSet;

pub struct DistClient {
    pub endpoint: Endpoint,
    pub connections: Arc<ClientConnections>,
    reverse_only: bool,
    worker: EndpointAddr,
    root: std::path::PathBuf,
    cas: Cas,
    // Speculation is an optimisation, never a substitute for Need after worker restart.
    offered: Mutex<HashSet<String>>,
    memo: crate::cas::DigestMemo,
    pub uploaded_blobs: AtomicU64,
}
impl DistClient {
    pub fn new(endpoint: Endpoint, worker: EndpointAddr, root: &Path) -> Result<Self> {
        let connections = ClientConnections::new(endpoint, root)?;
        Self::with_connections(connections, worker, root)
    }
    pub fn with_connections(
        connections: Arc<ClientConnections>,
        worker: EndpointAddr,
        root: &Path,
    ) -> Result<Self> {
        if !Trust::load(&root.join("trusted_workers.toml"))?.contains(worker.id) {
            return Err(format!("untrusted Dist worker {}", worker.id).into());
        }
        Ok(Self {
            endpoint: connections.endpoint.clone(),
            connections,
            reverse_only: false,
            worker,
            root: root.into(),
            cas: Cas::new(root, u64::MAX)?,
            offered: Mutex::new(HashSet::new()),
            memo: crate::cas::DigestMemo::default(),
            uploaded_blobs: AtomicU64::new(0),
        })
    }
    pub fn with_reverse_only(mut self, value: bool) -> Self {
        self.reverse_only = value;
        self
    }
    /// Reuses metadata-keyed digests across job manifests from this client.
    pub fn input_files(&mut self, paths: &[std::path::PathBuf]) -> Result<Vec<InputFile>> {
        paths.iter().map(|path| self.memo.input(path)).collect()
    }
    pub async fn execute(&self, job: DistJob) -> DistOutcome {
        let started = std::time::Instant::now();
        let mut metrics = JobMetrics::for_job(self.worker.id.to_string(), &job);
        let mut outcome = match self.try_execute(&job, &mut metrics).await {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!(
                    "\x1b[31msoldr dist: WARNING worker {} lost: {error}; run locally\x1b[0m",
                    self.worker.id
                );
                DistOutcome::Declined {
                    metrics: JobMetrics::default(),
                    reason: "worker lost".into(),
                }
            }
        };
        metrics.elapsed_millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        metrics.finish(&outcome);
        outcome.set_metrics(metrics);
        if let Err(error) = crate::status::record_job(&self.root, outcome.metrics()) {
            eprintln!("soldr dist: WARNING cannot persist job metrics: {error}");
        }
        outcome
    }
    async fn try_execute(&self, job: &DistJob, metrics: &mut JobMetrics) -> Result<DistOutcome> {
        if !Trust::load(&self.root.join("trusted_workers.toml"))?.contains(self.worker.id) {
            return Ok(DistOutcome::Declined {
                metrics: JobMetrics::default(),
                reason: "untrusted worker".into(),
            });
        }
        let conn = self
            .connections
            .connect(&self.worker, self.reverse_only)
            .await?;
        let before = conn.stats();
        let result = self.exchange(&conn, job, metrics).await;
        metrics.capture(&conn, (before.udp_tx.bytes, before.udp_rx.bytes));
        conn.close(0u32.into(), b"client complete");
        result
    }
    async fn exchange(
        &self,
        conn: &iroh::endpoint::Connection,
        job: &DistJob,
        metrics: &mut JobMetrics,
    ) -> Result<DistOutcome> {
        let (mut send, mut recv) = conn.open_bi().await?;
        if let Some(reason) = hello(&mut send, &mut recv, &job.compiler_identity).await? {
            return Ok(DistOutcome::Declined {
                reason,
                metrics: JobMetrics::default(),
            });
        }
        let total: u64 = job.inputs.iter().map(|i| i.size).sum();
        let offered = self.offered.lock().await;
        let speculative: HashSet<String> = if total < 65536 {
            job.inputs
                .iter()
                .filter(|i| !offered.contains(&i.blake3))
                .map(|i| i.blake3.clone())
                .collect()
        } else {
            HashSet::new()
        };
        drop(offered);
        transport::send(
            &mut send,
            &Offer {
                job: Some(job.clone()),
                speculative: speculative.iter().cloned().collect(),
            },
        )
        .await?;
        metrics.speculative_blobs = speculative.len() as u64;
        let mut transfers = JoinSet::new();
        self.upload(conn, job, &speculative, &mut transfers)?;
        let reply: Reply = transport::recv(&mut recv).await?;
        if let Some(declined) = reply.declined {
            return Ok(DistOutcome::Declined {
                metrics: JobMetrics::default(),
                reason: declined.reason,
            });
        }
        let need = reply.need.ok_or("expected Need")?;
        let requested: HashSet<_> = need.digests.into_iter().collect();
        metrics.need_digests = requested.len() as u64;
        metrics.cas_hits_known = need.cas_hits_known || speculative.is_empty();
        metrics.cas_hits = if need.cas_hits_known {
            need.cas_hits
        } else if speculative.is_empty() {
            metrics.input_digests.saturating_sub(metrics.need_digests)
        } else {
            0
        };
        self.upload(conn, job, &requested, &mut transfers)?;
        while let Some(result) = transfers.join_next().await {
            result??;
            metrics.uploaded_blobs += 1;
        }
        let reply: Reply = transport::recv(&mut recv).await?;
        if reply.accepted.is_none() {
            return Err("expected Accepted".into());
        }
        self.offered
            .lock()
            .await
            .extend(job.inputs.iter().map(|i| i.blake3.clone()));
        // No compile wall-clock timeout: QUIC keepalive bounds worker loss without killing live work.
        let reply: Reply = transport::recv(&mut recv).await?;
        let done = reply.done.ok_or("expected Done")?;
        let outputs = self.fetch(conn, job, &done, &mut send).await?;
        transport::send(&mut send, &Accepted {}).await?;
        send.finish()?;
        // After publication starts, errors must not permit a local fallback over
        // partly replaced outputs. No await/cancellation point follows publication.
        if let Err(error) = publish(outputs) {
            let mut stderr = done.stderr;
            stderr.extend_from_slice(
                format!("\nsoldr dist: output publication failed: {error}\n").as_bytes(),
            );
            return Ok(DistOutcome::Executed {
                metrics: JobMetrics::default(),
                exit_code: 1,
                stdout: done.stdout,
                stderr,
            });
        }
        Ok(DistOutcome::Executed {
            metrics: JobMetrics::default(),
            exit_code: done.exit_code,
            stdout: done.stdout,
            stderr: done.stderr,
        })
    }
    fn upload(
        &self,
        conn: &iroh::endpoint::Connection,
        job: &DistJob,
        digests: &HashSet<String>,
        transfers: &mut JoinSet<Result<()>>,
    ) -> Result<()> {
        for digest in digests {
            let input = job
                .inputs
                .iter()
                .find(|i| &i.blake3 == digest)
                .ok_or("worker requested undeclared digest")?;
            let file = fs::File::open(&input.abs_path)?;
            let conn = conn.clone();
            let digest = digest.clone();
            self.uploaded_blobs.fetch_add(1, Ordering::Relaxed);
            transfers.spawn(async move { transport::upload(&conn, digest, file).await });
        }
        Ok(())
    }
    async fn fetch(
        &self,
        conn: &iroh::endpoint::Connection,
        job: &DistJob,
        done: &Done,
        send: &mut iroh::endpoint::SendStream,
    ) -> Result<Vec<(OutputFile, Vec<u8>)>> {
        if done.exit_code == 0
            && (done.outputs.len() != job.outputs.len()
                || job
                    .outputs
                    .iter()
                    .any(|p| !done.outputs.iter().any(|o| &o.abs_path == p)))
        {
            return Err("worker omitted declared output".into());
        }
        let mut missing = HashSet::new();
        for output in &done.outputs {
            if !job.outputs.contains(&output.abs_path) {
                return Err("worker returned undeclared output".into());
            }
            if !self.cas.contains(&output.blake3)? {
                missing.insert(output.blake3.clone());
            }
        }
        transport::send(
            send,
            &Fetch {
                digests: missing.iter().cloned().collect(),
            },
        )
        .await?;
        let expected: Vec<_> = done
            .outputs
            .iter()
            .map(|o| (o.blake3.clone(), o.size))
            .collect();
        while !missing.is_empty() {
            let digest = transport::receive_blob(conn, &self.cas, &expected).await?;
            if !missing.remove(&digest) {
                return Err("duplicate output blob".into());
            }
        }
        // Verify every output before replacing any local file.
        let verified: Vec<_> = done
            .outputs
            .iter()
            .map(|o| self.cas.get(&o.blake3))
            .collect::<Result<_>>()?;
        Ok(done.outputs.iter().cloned().zip(verified).collect())
    }
}

fn publish(outputs: Vec<(OutputFile, Vec<u8>)>) -> Result<()> {
    use std::io::Write;
    let mut pending = Vec::with_capacity(outputs.len());
    for (output, bytes) in outputs {
        let path = std::path::PathBuf::from(output.abs_path);
        let parent = path.parent().ok_or("output parent missing")?;
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&bytes)?;
        soldr_platform::fs::permissions::restore_mode(temp.path(), Some(output.mode & 0o777))?;
        pending.push((temp, path));
    }
    for (temp, path) in pending {
        temp.persist(path)?;
    }
    Ok(())
}
/// Explicit tickets only: discovery and scheduling belong to a later slice.
pub fn workers(tickets: &[String]) -> Result<Vec<EndpointAddr>> {
    tickets.iter().map(|t| transport::parse_ticket(t)).collect()
}

#[test]
fn preparing_all_outputs_before_publication_preserves_existing_files_on_error() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("old.o");
    let blocked = root.path().join("file");
    fs::write(&output, b"old").unwrap();
    fs::write(&blocked, b"not a directory").unwrap();
    let file = |path: &Path| OutputFile {
        abs_path: path.to_str().unwrap().into(),
        mode: 0o644,
        ..Default::default()
    };
    assert!(publish(vec![
        (file(&output), b"new".to_vec()),
        (file(&blocked.join("impossible.o")), b"other".to_vec()),
    ])
    .is_err());
    assert_eq!(fs::read(output).unwrap(), b"old");
}

async fn hello(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    identity: &str,
) -> Result<Option<String>> {
    transport::send(
        send,
        &Hello {
            version: VERSION,
            toolchains: vec![identity.to_owned()],
        },
    )
    .await?;
    let hello: Hello = transport::recv(recv).await?;
    Ok((hello.version != VERSION).then(|| {
        format!(
            "protocol version mismatch: peer {}, local {VERSION}",
            hello.version
        )
    }))
}
