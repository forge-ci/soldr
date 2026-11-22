//! Worker lifecycle. In-flight compilation is independent of connection lifetime.
use crate::{
    blob_router::{BlobInbox, BlobRouter},
    cas::Cas,
    compiler::{self, Compiler},
    proto::*,
    sandbox,
    transport::{self, VERSION},
    trust::Trust,
    Result,
};
use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    Endpoint, EndpointAddr,
};
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
mod outbound;

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
    pub clients: Vec<EndpointAddr>,
    pub outbound_only: bool,
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
    /// The build session holding this worker: one client at a time, released
    /// after `SESSION_IDLE` without a joblet from it.
    session: soldr_platform::sync::Mutex<Option<(String, std::time::Instant)>>,
    /// Wakes the maintenance task (eviction + status). Never done inline on a joblet.
    maintenance: tokio::sync::Notify,
}

/// Asks the maintenance task to evict and republish status. Coalesces: many
/// requests while a pass runs produce one more pass, not one per joblet.
fn request_maintenance(state: &State) {
    state.maintenance.notify_one();
}

/// Runs store eviction and status publication off the async runtime. Both walk
/// the whole content store, so they run on the blocking pool, one pass at a
/// time, when a joblet finishes or every 15 s.
async fn maintain(state: Arc<State>, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = state.maintenance.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(15)) => {},
            _ = shutdown.changed() => break,
        }
        let pass = state.clone();
        let result = tokio::task::spawn_blocking(move || {
            pass.cas.evict()?;
            write_status(&pass)
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("\x1b[31msoldr dist: worker maintenance failed: {error}\x1b[0m")
            }
            Err(error) => {
                eprintln!("\x1b[31msoldr dist: worker maintenance task failed: {error}\x1b[0m")
            }
        }
    }
}
impl Worker {
    pub async fn serve(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        if self.slots == 0 {
            return Err("Dist worker slots must be positive".into());
        }
        if self.outbound_only && self.clients.is_empty() {
            return Err("outbound-only dist serving requires at least one client ticket".into());
        }
        let trusted = Trust::load(&self.root.join("trusted_clients.toml"))?;
        for client in &self.clients {
            transport::validate_peer(client)?;
            if !trusted.contains(client.id) {
                return Err(
                    format!("configured outbound client {} is not trusted", client.id).into(),
                );
            }
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
        let root = std::fs::canonicalize(&self.root)?;
        let state = Arc::new(State {
            cas: Cas::new(&root, self.cas_budget)?,
            root,
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
            session: soldr_platform::sync::Mutex::new(None),
            maintenance: tokio::sync::Notify::new(),
        });
        let mut jobs = JoinSet::new();
        let maintenance = tokio::spawn(maintain(state.clone(), shutdown.clone()));
        // Ends with the endpoint; logs each relay disconnect and reconnect (soldr#4212).
        let _relay_log = crate::link::watch_relay(&self.endpoint, "worker");
        for client in self.clients {
            // One warm session per client multiplexes every joblet; slots bound
            // compiler admission, not sockets.
            jobs.spawn(outbound::dial(
                state.clone(),
                self.endpoint.clone(),
                client,
                shutdown.clone(),
            ));
        }
        write_status(&state)?;
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else { break; };
                    if self.outbound_only { incoming.refuse(); continue; }
                    jobs.spawn(outbound::incoming(state.clone(), incoming, shutdown.clone()));
                }
                result = jobs.join_next(), if !jobs.is_empty() => {
                    report_join(result);
                    request_maintenance(&state);
                }
            }
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
        maintenance.abort();
        let last = state.clone();
        tokio::task::spawn_blocking(move || last.cas.evict()).await??;
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
            // A client that drops mid-joblet re-leases it (soldr#4212): log that
            // once, as information, with the real cause ("timed out", "closed
            // by peer: ...") instead of a bare "connection lost" error.
            Ok(Err(e)) if crate::link::is_connection_loss(e.as_ref()) => eprintln!(
                "Dist client disconnected during a joblet ({}); the client re-leases it",
                crate::link::describe(e.as_ref())
            ),
            Ok(Err(e)) => eprintln!("Dist joblet failed: {}", crate::link::describe(e.as_ref())),
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
        last_job: None,
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
/// Serves every joblet a client multiplexes onto one session until the session
/// ends or the worker shuts down; in-flight joblets then finish (the caller's
/// drain deadline bounds them).
async fn serve_connection(
    state: Arc<State>,
    conn: Connection,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let router = BlobRouter::spawn(conn.clone());
    let mut jobs = JoinSet::new();
    loop {
        tokio::select! {
            streams = conn.accept_bi() => {
                let Ok(streams) = streams else { break };
                // Trust is rechecked per joblet, so revocation needs no restart.
                if !Trust::load(&state.root.join("trusted_clients.toml"))?.contains(conn.remote_id()) {
                    conn.close(1u32.into(), b"client trust revoked");
                    return Err("client trust revoked".into());
                }
                let (state, conn, router) = (state.clone(), conn.clone(), router.clone());
                jobs.spawn(async move {
                    let result = handle_job(&state, &conn, &router, streams).await;
                    request_maintenance(&state);
                    result
                });
            }
            _ = shutdown.changed() => break,
            result = jobs.join_next(), if !jobs.is_empty() => report_join(result),
        }
    }
    while let Some(result) = jobs.join_next().await {
        report_join(Some(result));
    }
    conn.close(0u32.into(), b"worker session complete");
    Ok(())
}
async fn handle_job(
    state: &State,
    conn: &Connection,
    router: &BlobRouter,
    (mut send, mut recv): (SendStream, RecvStream),
) -> Result<()> {
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
    // Register before any decline: it also finishes the token, so speculative
    // uploads for a declined joblet are stopped rather than parked.
    let mut inbox = router.register(offer.job_token)?;
    let job = offer.job.ok_or("missing job manifest")?;
    let Some(compiler) = state
        .compilers
        .iter()
        .find(|c| c.kind == job.compiler_kind && c.matches_identity(&job.compiler_identity))
    else {
        let reason = toolchain_mismatch(state, &job);
        eprintln!("Dist joblet declined: {reason}");
        decline(&mut send, &reason).await?;
        send.finish()?;
        return Ok(());
    };
    if let Some(reason) = compiler.environment_decline(&job) {
        decline(&mut send, &reason).await?;
        send.finish()?;
        return Ok(());
    }
    if let Some(holder) = claim_session(state, &conn.remote_id().to_string()) {
        let reason = format!("worker is in a build session with client {holder}");
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
    request_maintenance(state);
    let phase = std::time::Instant::now();
    receive_inputs(state, &mut inbox, &job, &offer.speculative, &mut send).await?;
    let receive_millis = millis(phase.elapsed());
    let phase = std::time::Instant::now();
    let sandbox = tempfile::tempdir_in(&state.root)?;
    {
        // Hard-links or copies every input: blocking filesystem work.
        let (path, job, cas) = (sandbox.path().to_path_buf(), job.clone(), state.cas.clone());
        tokio::task::spawn_blocking(move || sandbox::materialise(&path, &job, &cas)).await??;
    }
    let materialise_millis = millis(phase.elapsed());
    transport::send(
        &mut send,
        &Reply {
            accepted: Some(Accepted {}),
            ..Reply::default()
        },
    )
    .await?;
    let phase = std::time::Instant::now();
    let Some(mut done) = execute_compiler(compiler, &job, sandbox.path(), &mut send).await? else {
        return Ok(());
    };
    let compile_millis = millis(phase.elapsed());
    let phase = std::time::Instant::now();
    let (done, lease) = {
        // Rewrites depfiles, then hashes and stores every output: blocking work.
        let (path, job) = (sandbox.path().to_path_buf(), job.clone());
        tokio::task::spawn_blocking(move || -> Result<_> {
            crate::rust_output::restore(&job, &path, &mut done)?;
            if done.exit_code == 0 {
                done.outputs = compiler::outputs(&job, &path, &mut lease)?;
            }
            Ok((done, lease))
        })
        .await??
    };
    let mut done = done;
    done.receive_millis = receive_millis;
    done.materialise_millis = materialise_millis;
    done.compile_millis = compile_millis;
    done.outputs_millis = millis(phase.elapsed());
    // The lease pins the outputs in the store until delivery completes.
    let _lease = lease;
    deliver(state, conn, offer.job_token, &done, (send, recv)).await
}

fn millis(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// The decline reason naming the requested and every offered identity.
fn toolchain_mismatch(state: &State, job: &DistJob) -> String {
    let offered = state
        .compilers
        .iter()
        .filter(|c| c.kind == job.compiler_kind)
        .map(|c| c.identity.lines().next().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    format!(
        "toolchain mismatch: {} requested {}; worker offers {offered:?}",
        job.compiler_kind, job.compiler_identity
    )
}

/// Sends the result, then streams every output the client asks for.
async fn deliver(
    state: &State,
    conn: &Connection,
    token: u64,
    done: &crate::proto::Done,
    (mut send, mut recv): (SendStream, RecvStream),
) -> Result<()> {
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
        transfers.spawn(async move { transport::upload(&conn, token, digest, file).await });
    }
    while let Some(result) = transfers.join_next().await {
        result??;
    }
    // An acknowledgement makes output delivery durable before closing QUIC.
    let _: Accepted = transport::recv(&mut recv).await?;
    send.finish()?;
    Ok(())
}
async fn execute_compiler(
    compiler: &Compiler,
    job: &DistJob,
    sandbox: &std::path::Path,
    send: &mut SendStream,
) -> Result<Option<crate::proto::Done>> {
    match compiler.run(job, sandbox).await {
        Ok(done) => Ok(Some(done)),
        Err(error)
            if error
                .downcast_ref::<crate::dep_info::InvalidProof>()
                .is_some() =>
        {
            decline(send, &format!("Rust input backstop: {error}")).await?;
            send.finish()?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
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
    inbox: &mut BlobInbox,
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
    let mut cas_hits = 0;
    for (digest, _) in &expected {
        if state.cas.contains(digest)? {
            cas_hits += 1;
        } else if !incoming.contains(digest) {
            need.push(digest.clone());
        }
    }
    transport::send(
        send,
        &Reply {
            need: Some(Need {
                digests: need.clone(),
                cas_hits,
                cas_hits_known: true,
            }),
            ..Reply::default()
        },
    )
    .await?;
    incoming.extend(need);
    while !incoming.is_empty() {
        let (digest, _) = inbox.receive(&state.cas, &expected).await?;
        if !incoming.remove(&digest) {
            return Err("duplicate input blob".into());
        }
        state.uploaded.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// A build session is idle-released: this long without a joblet from its client.
const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

/// Claims or refreshes the worker's build session for `client`. Returns the
/// holder when another client's session is still live.
fn claim_session(state: &State, client: &str) -> Option<String> {
    claim(&mut state.session.lock(), client, std::time::Instant::now())
}

fn claim(
    session: &mut Option<(String, std::time::Instant)>,
    client: &str,
    now: std::time::Instant,
) -> Option<String> {
    match session {
        Some((holder, seen))
            if holder != client && now.saturating_duration_since(*seen) < SESSION_IDLE =>
        {
            Some(holder.clone())
        }
        _ => {
            if !matches!(session, Some((holder, _)) if holder == client) {
                eprintln!("Dist worker build session started for client {client}");
            }
            *session = Some((client.to_owned(), now));
            None
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::{claim, SESSION_IDLE};
    #[test]
    fn one_client_holds_the_worker_until_idle() {
        let t0 = std::time::Instant::now();
        let mut session = None;
        assert_eq!(claim(&mut session, "a", t0), None);
        assert_eq!(claim(&mut session, "b", t0), Some("a".into()));
        assert_eq!(claim(&mut session, "a", t0 + SESSION_IDLE), None, "refresh");
        let later = t0 + SESSION_IDLE * 2 + std::time::Duration::from_secs(1);
        assert_eq!(
            claim(&mut session, "b", later),
            None,
            "idle session released"
        );
        assert_eq!(claim(&mut session, "a", later), Some("b".into()));
    }
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
