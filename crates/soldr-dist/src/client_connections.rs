//! A pool of long-lived, authenticated worker sessions.
//!
//! A client keeps one QUIC session per worker and multiplexes every joblet onto
//! it as a separate bidirectional stream, so a build pays the handshake (and,
//! off-LAN, the hole punch) once per worker rather than once per compile. A
//! session is either dialled by the client or initiated by the worker (reverse
//! mode for clients behind static tickets); both land in the same pool.
//! Affinity is per worker: every joblet for a worker reuses its session, and
//! with it the worker's warm content store.
use crate::{blob_router::BlobRouter, transport, trust::Trust, Result};
use iroh::{endpoint::Connection, Endpoint, EndpointAddr, EndpointId};
use soldr_platform::sync::Mutex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::Notify,
    task::{JoinHandle, JoinSet},
};

/// Worker-initiated sessions kept per worker; one is enough to multiplex.
const REVERSE_SESSIONS_PER_WORKER: usize = 4;

/// One authenticated worker connection shared by concurrent joblets.
pub struct Session {
    pub conn: Connection,
    pub router: BlobRouter,
    next_token: AtomicU64,
}

impl Session {
    fn new(conn: Connection) -> Arc<Self> {
        Arc::new(Self {
            router: BlobRouter::spawn(conn.clone()),
            conn,
            next_token: AtomicU64::new(1),
        })
    }

    /// A token unique within this session for one joblet's blob streams.
    pub fn next_token(&self) -> u64 {
        self.next_token.fetch_add(1, Ordering::Relaxed)
    }

    pub fn is_live(&self) -> bool {
        self.conn.close_reason().is_none()
    }
}

struct State {
    sessions: Mutex<HashMap<EndpointId, Vec<Arc<Session>>>>,
    // One dial at a time per worker: concurrent joblets share its result.
    dialing: Mutex<HashMap<EndpointId, Arc<tokio::sync::Mutex<()>>>>,
    changed: Notify,
    incoming: AtomicU64,
    outbound: AtomicU64,
}

pub struct ClientConnections {
    pub endpoint: Endpoint,
    state: Arc<State>,
    listener: JoinHandle<()>,
}

impl ClientConnections {
    pub fn new(endpoint: Endpoint, root: &Path) -> Result<Arc<Self>> {
        std::fs::create_dir_all(root)?;
        let state = Arc::new(State {
            sessions: Mutex::new(HashMap::new()),
            dialing: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            incoming: AtomicU64::new(0),
            outbound: AtomicU64::new(0),
        });
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "creating a dist client requires an active Tokio runtime")?;
        let listener = runtime.spawn(listen(endpoint.clone(), root.into(), state.clone()));
        Ok(Arc::new(Self {
            endpoint,
            state,
            listener,
        }))
    }

    /// Worker-initiated sessions accepted so far.
    pub fn incoming_connections(&self) -> u64 {
        self.state.incoming.load(Ordering::Relaxed)
    }
    /// Sessions this client dialled (a reused session does not count again).
    pub fn outbound_connections(&self) -> u64 {
        self.state.outbound.load(Ordering::Relaxed)
    }

    /// Returns the live session for `worker`, dialling once if there is none.
    pub async fn session(&self, worker: &EndpointAddr, reverse_only: bool) -> Result<Arc<Session>> {
        tokio::time::timeout(Duration::from_secs(8), async {
            if let Some(session) = live(&self.state, worker.id) {
                return Ok(session);
            }
            if reverse_only {
                return wait(&self.state, worker.id).await;
            }
            let gate = self
                .state
                .dialing
                .lock()
                .entry(worker.id)
                .or_default()
                .clone();
            let _dialing = gate.lock().await;
            if let Some(session) = live(&self.state, worker.id) {
                return Ok(session);
            }
            self.state.outbound.fetch_add(1, Ordering::Relaxed);
            tokio::select! {
                result = self.endpoint.connect(worker.clone(), transport::ALPN) => match result {
                    Ok(conn) => Ok(insert(&self.state, worker.id, conn)),
                    Err(_) => wait(&self.state, worker.id).await,
                },
                session = wait(&self.state, worker.id) => session,
            }
        })
        .await
        .map_err(|_| "dist worker connection deadline reached")?
    }
}

impl Drop for ClientConnections {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

fn live(state: &State, id: EndpointId) -> Option<Arc<Session>> {
    let mut map = state.sessions.lock();
    let sessions = map.get_mut(&id)?;
    sessions.retain(|s| s.is_live());
    sessions.first().cloned()
}

fn insert(state: &State, id: EndpointId, conn: Connection) -> Arc<Session> {
    let session = Session::new(conn);
    let mut map = state.sessions.lock();
    let sessions = map.entry(id).or_default();
    sessions.retain(|s| s.is_live());
    sessions.push(session.clone());
    drop(map);
    state.changed.notify_waiters();
    session
}

async fn wait(state: &State, id: EndpointId) -> Result<Arc<Session>> {
    loop {
        let changed = state.changed.notified();
        if let Some(session) = live(state, id) {
            return Ok(session);
        }
        changed.await;
    }
}

async fn listen(endpoint: Endpoint, root: PathBuf, state: Arc<State>) {
    let mut tasks = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_ticket = String::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break; };
                let (state, root) = (state.clone(), root.clone());
                tasks.spawn(async move {
                    if let Err(error) = accept(incoming, &root, &state).await {
                        eprintln!("Dist client rejected reverse connection: {error}");
                    }
                });
            },
            _ = tick.tick() => {
                if let Err(error) = advertise(&endpoint, &root, &mut last_ticket) {
                    eprintln!("Dist client ticket publication failed: {error}");
                }
            },
            _ = tasks.join_next(), if !tasks.is_empty() => {},
        }
    }
}

async fn accept(incoming: iroh::endpoint::Incoming, root: &Path, state: &State) -> Result<()> {
    let conn = tokio::time::timeout(Duration::from_secs(8), incoming).await??;
    let id = conn.remote_id();
    if !Trust::load(&root.join("trusted_workers.toml"))?.contains(id) {
        conn.close(1u32.into(), b"untrusted worker");
        return Err(format!("untrusted worker {id}").into());
    }
    let full = {
        let mut map = state.sessions.lock();
        let sessions = map.entry(id).or_default();
        sessions.retain(|s| s.is_live());
        sessions.len() >= REVERSE_SESSIONS_PER_WORKER
    };
    if full {
        conn.close(1u32.into(), b"reverse session pool full");
        return Ok(());
    }
    insert(state, id, conn);
    state.incoming.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn advertise(endpoint: &Endpoint, root: &Path, previous: &mut String) -> Result<()> {
    use std::io::Write;
    let ticket = transport::ticket(&endpoint.addr())?;
    if ticket != *previous {
        let mut file = tempfile::NamedTempFile::new_in(root)?;
        file.write_all(ticket.as_bytes())?;
        file.persist(root.join("client-ticket.json"))?;
        eprintln!("Dist client {} ticket: {}", endpoint.id(), ticket);
        *previous = ticket;
    }
    Ok(())
}
