//! Warm, authenticated worker-initiated connections. One connection per joblet.
use crate::{transport, trust::Trust, Result};
use iroh::{endpoint::Connection, Endpoint, EndpointAddr, EndpointId};
use soldr_platform::sync::Mutex;
use std::{
    collections::{HashMap, VecDeque},
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

struct State {
    pending: Mutex<HashMap<EndpointId, VecDeque<Connection>>>,
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
            pending: Mutex::new(HashMap::new()),
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

    pub fn incoming_connections(&self) -> u64 {
        self.state.incoming.load(Ordering::Relaxed)
    }
    pub fn outbound_connections(&self) -> u64 {
        self.state.outbound.load(Ordering::Relaxed)
    }

    pub async fn connect(&self, worker: &EndpointAddr, reverse_only: bool) -> Result<Connection> {
        tokio::time::timeout(Duration::from_secs(8), async {
            if let Some(conn) = take(&self.state, worker.id) {
                return Ok(conn);
            }
            if reverse_only {
                return wait(&self.state, worker.id).await;
            }
            self.state.outbound.fetch_add(1, Ordering::Relaxed);
            tokio::select! {
                result = self.endpoint.connect(worker.clone(), transport::ALPN) => match result {
                    Ok(conn) => Ok(conn),
                    Err(_) => wait(&self.state, worker.id).await,
                },
                conn = wait(&self.state, worker.id) => conn,
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

fn take(state: &State, id: EndpointId) -> Option<Connection> {
    let mut map = state.pending.lock();
    let queue = map.get_mut(&id)?;
    while let Some(conn) = queue.pop_front() {
        if conn.close_reason().is_none() {
            return Some(conn);
        }
    }
    None
}

async fn wait(state: &State, id: EndpointId) -> Result<Connection> {
    loop {
        let changed = state.changed.notified();
        if let Some(conn) = take(state, id) {
            return Ok(conn);
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
    let mut map = state.pending.lock();
    let queue = map.entry(id).or_default();
    queue.retain(|conn| conn.close_reason().is_none());
    if queue.len() >= 64 {
        conn.close(1u32.into(), b"reverse connection pool full");
        return Ok(());
    }
    queue.push_back(conn);
    state.incoming.fetch_add(1, Ordering::Relaxed);
    state.changed.notify_waiters();
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
