//! Worker lifecycle. In-flight compilation is independent of connection lifetime.
use crate::{
    cas::Cas,
    compiler::{self, Compiler},
    proto::*,
    sandbox,
    transport::{self, VERSION},
    trust::Trust,
    Result,
};
use iroh::{endpoint::Connection, Endpoint};
use prost::Message;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{watch, Semaphore},
    task::JoinSet,
};

pub struct Worker {
    pub endpoint: Endpoint,
    pub root: PathBuf,
    pub compilers: Vec<Compiler>,
    pub slots: usize,
    pub cas_budget: u64,
    pub host_load_file: Option<PathBuf>,
    pub signal_url: String,
    pub total_cpus: usize,
    pub uploaded_blobs: Arc<AtomicU64>,
}
struct State {
    root: PathBuf,
    compilers: Vec<Compiler>,
    slots: Arc<Semaphore>,
    cas: Cas,
    uploaded: Arc<AtomicU64>,
    node_id: String,
    build_id: String,
    signal_url: String,
    total: usize,
    total_cpus: usize,
    host_load_file: Option<PathBuf>,
    admission: soldr_platform::sync::Mutex<()>,
}
impl Worker {
    pub async fn serve(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        if self.slots == 0 {
            return Err("Dist worker slots must be positive".into());
        }
        use fs2::FileExt;
        let root_lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("worker.lock"))?;
        root_lock
            .try_lock_exclusive()
            .map_err(|e| format!("Dist worker root already in use: {e}"))?;
        let state = Arc::new(State {
            cas: Cas::new(&self.root, self.cas_budget)?,
            root: self.root,
            compilers: self.compilers,
            slots: Arc::new(Semaphore::new(self.slots)),
            uploaded: self.uploaded_blobs,
            node_id: self.endpoint.id().to_string(),
            build_id: crate::status::build_id()?,
            signal_url: self.signal_url,
            total: self.slots,
            total_cpus: self.total_cpus,
            host_load_file: self.host_load_file,
            admission: soldr_platform::sync::Mutex::new(()),
        });
        let mut jobs = JoinSet::new();
        write_status(&state)?;
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else { break; };
                    let state = state.clone();
                    jobs.spawn(async move {
                        let conn = incoming.await?;
                        let trust = Trust::load(&state.root.join("trusted_clients.toml"))?;
                        if !trust.contains(conn.remote_id()) {
                            eprintln!("Dist rejected untrusted client {}", conn.remote_id());
                            conn.close(1u32.into(), b"untrusted client");
                            return Ok(());
                        }
                        let result = handle(&state, &conn).await;
                        if let Err(ref e) = result { eprintln!("Dist job from {}: {e}", conn.remote_id()); }
                        if result.is_ok() { let _ = tokio::time::timeout(Duration::from_secs(8), conn.closed()).await; }
                        match &result {
                            Ok(()) => conn.close(0u32.into(), b"job complete"),
                            Err(error) => {
                                let reason = error.to_string();
                                conn.close(1u32.into(), &reason.as_bytes()[..reason.len().min(200)]);
                            }
                        }
                        result
                    });
                }
                result = jobs.join_next(), if !jobs.is_empty() => {
                    report_join(result);
                    state.cas.evict()?;
                }
            }
            write_status(&state)?;
        }
        // Stop accepting while allowing accepted jobs to finish; timeout kills their children.
        let drain = async {
            while let Some(result) = jobs.join_next().await {
                report_join(Some(result));
            }
        };
        if tokio::time::timeout(Duration::from_secs(30), drain)
            .await
            .is_err()
        {
            eprintln!("Dist shutdown deadline reached; cancelling in-flight joblets");
            jobs.abort_all();
            while let Some(result) = jobs.join_next().await {
                report_join(Some(result));
            }
        }
        self.endpoint.close().await;
        state.cas.evict()?;
        let path = state.root.join("status.pb");
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}
fn report_join(result: Option<std::result::Result<Result<()>, tokio::task::JoinError>>) {
    if let Some(result) = result {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("Dist connection error: {e}"),
            Err(e) => eprintln!("Dist task error: {e}"),
        }
    }
}
fn write_status(state: &State) -> Result<()> {
    use std::io::Write;
    let status = Status {
        node_id: state.node_id.clone(),
        role: if state.host_load_file.is_some() {
            "shared"
        } else {
            "dist"
        }
        .into(),
        trusted_clients: Trust::load(&state.root.join("trusted_clients.toml"))?.peers,
        trusted_workers: Trust::load(&state.root.join("trusted_workers.toml"))?.peers,
        cas_bytes: state.cas.size()?,
        running_joblets: (state.total - state.slots.available_permits()) as u64,
        pid: std::process::id() as u64,
        build_id: state.build_id.clone(),
        signal_url: state.signal_url.clone(),
    };
    let mut temp = tempfile::NamedTempFile::new_in(&state.root)?;
    temp.write_all(&status.encode_to_vec())?;
    temp.persist(state.root.join("status.pb"))?;
    Ok(())
}
async fn decline(send: &mut iroh::endpoint::SendStream, reason: &str) -> Result<()> {
    transport::send(
        send,
        &Reply {
            declined: Some(Declined {
                reason: reason.into(),
            }),
            ..Reply::default()
        },
    )
    .await
}
async fn handle(state: &State, conn: &Connection) -> Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    let hello: Hello = transport::recv(&mut recv).await?;
    transport::send(
        &mut send,
        &Hello {
            version: VERSION,
            toolchains: state
                .compilers
                .iter()
                .map(|c| format!("{}:{}", c.kind, c.identity))
                .collect(),
        },
    )
    .await?;
    if hello.version != VERSION {
        return Err(format!(
            "Dist protocol version mismatch: peer {}, local {VERSION}",
            hello.version
        )
        .into());
    }
    let offer: Offer = transport::recv(&mut recv).await?;
    let job = offer.job.ok_or("missing job manifest")?;
    let Some(compiler) = state
        .compilers
        .iter()
        .find(|c| c.kind == job.compiler_kind && c.identity == job.compiler_identity)
    else {
        decline(&mut send, "toolchain mismatch").await?;
        send.finish()?;
        return Ok(());
    };
    if let Some(reason) = compiler.environment_decline(&job) {
        decline(&mut send, &reason).await?;
        send.finish()?;
        return Ok(());
    }
    let Some(_permit) = admit(state) else {
        decline(&mut send, "worker busy").await?;
        send.finish()?;
        return Ok(());
    };
    let mut lease = state.cas.lease(job.inputs.iter().map(|i| i.blake3.clone()));
    write_status(state)?;
    receive_inputs(state, conn, &job, &offer.speculative, &mut send).await?;
    state.cas.evict()?;
    let sandbox = tempfile::tempdir_in(&state.root)?;
    sandbox::materialise(sandbox.path(), &job, &state.cas)?;
    transport::send(
        &mut send,
        &Reply {
            accepted: Some(Accepted {}),
            ..Reply::default()
        },
    )
    .await?;
    let mut done = compiler.run(&job, sandbox.path()).await?;
    if done.exit_code == 0 {
        done.outputs = compiler::outputs(&job, sandbox.path(), &mut lease)?;
    }
    // Disconnected clients keep the compile alive only through producing its result.
    transport::send(
        &mut send,
        &Reply {
            done: Some(done.clone()),
            ..Reply::default()
        },
    )
    .await?;
    let fetch: Fetch = transport::recv(&mut recv).await?;
    let mut transfers = JoinSet::new();
    for digest in fetch.digests {
        if !done.outputs.iter().any(|o| o.blake3 == digest) {
            return Err("undeclared output requested".into());
        }
        let file = state.cas.open(&digest)?;
        let conn = conn.clone();
        transfers.spawn(async move { transport::upload(&conn, digest, file).await });
    }
    while let Some(result) = transfers.join_next().await {
        result??;
    }
    // An acknowledgement makes output delivery durable before closing QUIC.
    let _: Accepted = transport::recv(&mut recv).await?;
    send.finish()?;
    Ok(())
}
fn expected_inputs(job: &DistJob) -> Result<Vec<(String, u64)>> {
    sandbox::mirrored(std::path::Path::new("/validation"), &job.cwd)?;
    let mut paths = HashSet::new();
    for path in job
        .inputs
        .iter()
        .map(|i| &i.abs_path)
        .chain(job.outputs.iter())
    {
        sandbox::mirrored(std::path::Path::new("/validation"), path)?;
        let destination = sandbox::mirrored(std::path::Path::new("/validation"), path)?;
        if !paths.insert(destination) {
            return Err("duplicate or overlapping job path".into());
        }
    }
    let mut expected = std::collections::BTreeMap::new();
    for input in &job.inputs {
        if input.size > transport::BLOB_LIMIT {
            return Err("input exceeds blob limit".into());
        }
        if expected
            .insert(input.blake3.clone(), input.size)
            .is_some_and(|s| s != input.size)
        {
            return Err("conflicting input sizes".into());
        }
    }
    Ok(expected.into_iter().collect())
}

async fn receive_inputs(
    state: &State,
    conn: &Connection,
    job: &DistJob,
    speculative: &[String],
    send: &mut iroh::endpoint::SendStream,
) -> Result<()> {
    let expected = expected_inputs(job)?;
    let mut incoming = speculative.iter().cloned().collect::<HashSet<_>>();
    if incoming.len() != speculative.len()
        || incoming
            .iter()
            .any(|d| !expected.iter().any(|(e, _)| d == e))
    {
        return Err("invalid speculative manifest".into());
    }
    let mut need = vec![];
    for (digest, _) in &expected {
        if !incoming.contains(digest) && !state.cas.contains(digest)? {
            need.push(digest.clone());
        }
    }
    transport::send(
        send,
        &Reply {
            need: Some(Need {
                digests: need.clone(),
            }),
            ..Reply::default()
        },
    )
    .await?;
    incoming.extend(need);
    while !incoming.is_empty() {
        let digest = transport::receive_blob(conn, &state.cas, &expected).await?;
        if !incoming.remove(&digest) {
            return Err("duplicate input blob".into());
        }
        state.uploaded.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

fn admit(state: &State) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let limit = crate::host_load::limit(
        state.host_load_file.as_deref(),
        state.total,
        state.total_cpus,
    );
    let _guard = state.admission.lock();
    let running = state.total - state.slots.available_permits();
    if crate::host_load::free_slots(limit, running) == 0 {
        return None;
    }
    state.slots.clone().try_acquire_owned().ok()
}
