use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use ring::hmac;

use super::{insert, open, prune, query, unix_ms, Completion, ACK_HEADER, DAY_MS, RETENTION_MS};

#[derive(clap::Subcommand)]
pub(crate) enum AttestationCommand {
    /// Serve durable remote-build statistics.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8788")]
        listen: SocketAddr,
        #[arg(long)]
        database: PathBuf,
    },
}

struct Server {
    database: PathBuf,
    key: hmac::Key,
    write_tag: hmac::Tag,
    read_tag: hmac::Tag,
}

impl Server {
    fn new(database: PathBuf, read: &str, write: &str) -> io::Result<Self> {
        if read == write {
            return Err(io::Error::other(
                "attestation read and write tokens must differ",
            ));
        }
        let mut seed = [0; 32];
        getrandom::fill(&mut seed).map_err(io::Error::other)?;
        let key = hmac::Key::new(hmac::HMAC_SHA256, &seed);
        Ok(Self {
            database,
            write_tag: hmac::sign(&key, write.as_bytes()),
            read_tag: hmac::sign(&key, read.as_bytes()),
            key,
        })
    }

    fn authorized(&self, headers: &HeaderMap, tag: &hmac::Tag) -> bool {
        headers
            .get("authorization")
            .is_some_and(|header| hmac::verify(&self.key, header.as_bytes(), tag.as_ref()).is_ok())
    }
}

pub(crate) async fn run(command: AttestationCommand) -> io::Result<i32> {
    let AttestationCommand::Serve { listen, database } = command;
    let read = credential("SOLDR_ATTESTATION_READ_TOKEN")?;
    let write = credential("SOLDR_ATTESTATION_WRITE_TOKEN")?;
    let state = Arc::new(Server::new(database, &read, &write)?);
    open(&state.database).map_err(io::Error::other)?;
    retain_recent_records(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!(
        "soldr attestation listening at http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, router(state)).await?;
    Ok(0)
}

fn retain_recent_records(state: Arc<Server>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let state = Arc::clone(&state);
            let result =
                tokio::task::spawn_blocking(move || prune(&open(&state.database)?, unix_ms()))
                    .await;
            if !matches!(result, Ok(Ok(()))) {
                tracing::warn!("could not prune expired attestation statistics");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stats_http_separates_credentials_and_deduplicates_worker_events() {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("stats.sqlite");
        let read = "read";
        let write = "write";
        let state = Arc::new(Server::new(database, "Bearer read", "Bearer write").unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router(state)).await });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let origin = format!("http://{address}");
        let stats_url = format!("{origin}/v1/attestations/stats");
        let completion_url = format!("{origin}/v1/attestations/completions");
        let now = unix_ms();
        let event = serde_json::json!({
            "version": 1,
            "worker": "e640",
            "session": "1".repeat(32),
            "action": "build",
            "target": null,
            "exit_code": 0,
            "timing": {"version": 1, "started_unix_ms": now - 10, "lock_ms": 0, "preparation_ms": 2,
                       "command_ms": 8, "postprocessing_ms": 0, "worker_ms": 10, "publication_ms": 2},
        });
        let outbox = scratch.path().join("outbox");
        std::fs::create_dir(&outbox).unwrap();
        let worker = soldr_fetch::fetch::RemoteBuildHttpClient::new(address).unwrap();
        let pending = serde_json::from_value(event.clone()).unwrap();
        super::super::outbox::enqueue(&outbox, &pending).unwrap();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_client =
            soldr_fetch::fetch::RemoteBuildHttpClient::new(proxy.local_addr().unwrap()).unwrap();
        let proxy_task = tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/attestations/completions",
                post(|| async { StatusCode::BAD_REQUEST }),
            );
            axum::serve(proxy, app).await
        });
        assert!(super::super::outbox::deliver(&outbox, &proxy_client, write)
            .await
            .is_err());
        proxy_task.abort();
        assert!(super::super::outbox::deliver(&outbox, &worker, read)
            .await
            .is_err());
        assert!(outbox.join(format!("{}.json", pending.session)).is_file());
        assert_eq!(
            client
                .get(&stats_url)
                .bearer_auth(write)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        for (token, expected) in [
            (read, StatusCode::UNAUTHORIZED),
            (write, StatusCode::CREATED),
            (write, StatusCode::NO_CONTENT),
        ] {
            let response = client
                .post(&completion_url)
                .bearer_auth(token)
                .body(event.to_string())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        let response = client
            .get(stats_url)
            .bearer_auth(read)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let stats: serde_json::Value =
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(stats["completed"], 1);
        assert_eq!(stats["worker_ms"]["p95"], 10);
        assert_eq!(stats["publication_ms"]["p95"], 2);
        super::super::outbox::deliver(&outbox, &worker, write)
            .await
            .unwrap();
        assert!(!outbox.join(format!("{}.json", pending.session)).exists());
        task.abort();
    }
}

fn credential(name: &str) -> io::Result<String> {
    let token = std::env::var(name).unwrap_or_default();
    if token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(io::Error::other(format!(
            "{name} must be a token of at least 32 printable ASCII bytes"
        )));
    }
    Ok(format!("Bearer {token}"))
}

fn router(state: Arc<Server>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { StatusCode::NO_CONTENT }))
        .route("/v1/attestations/completions", post(accept))
        .route("/v1/attestations/stats", get(stats))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(state)
}

fn valid_identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_event(event: &Completion, now: u64) -> bool {
    let timing = &event.timing;
    event.version == 1
        && timing.version == 1
        && valid_identifier(&event.worker, 128)
        && event.session.len() == 32
        && event.session.bytes().all(|byte| byte.is_ascii_hexdigit())
        && valid_identifier(&event.action, 64)
        && event
            .target
            .as_deref()
            .is_none_or(|target| valid_identifier(target, 128))
        && (now.saturating_sub(RETENTION_MS)..=now.saturating_add(300_000))
            .contains(&timing.started_unix_ms)
        && timing.worker_ms <= DAY_MS
        && timing
            .publication_ms
            .is_some_and(|duration| duration <= DAY_MS)
        && [
            timing.lock_ms,
            timing.preparation_ms,
            timing.command_ms,
            timing.postprocessing_ms,
        ]
        .into_iter()
        .all(|duration| duration <= timing.worker_ms)
}

fn ingestion_ack(status: StatusCode) -> Response {
    (status, [(ACK_HEADER, "1")]).into_response()
}

async fn accept(State(state): State<Arc<Server>>, headers: HeaderMap, body: Bytes) -> Response {
    if !state.authorized(&headers, &state.write_tag) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let now = unix_ms();
    let Ok(event) = serde_json::from_slice::<Completion>(&body) else {
        return ingestion_ack(StatusCode::UNPROCESSABLE_ENTITY);
    };
    if !valid_event(&event, now) {
        return ingestion_ack(StatusCode::UNPROCESSABLE_ENTITY);
    }
    match tokio::task::spawn_blocking(move || insert(&open(&state.database)?, &event, now)).await {
        Ok(Ok(true)) => ingestion_ack(StatusCode::CREATED),
        Ok(Ok(false)) => ingestion_ack(StatusCode::NO_CONTENT),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[derive(serde::Deserialize)]
struct Window {
    start_unix_ms: Option<u64>,
    end_unix_ms: Option<u64>,
}

async fn stats(
    State(state): State<Arc<Server>>,
    headers: HeaderMap,
    Query(window): Query<Window>,
) -> Response {
    if !state.authorized(&headers, &state.read_tag) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let now = unix_ms();
    let end = window.end_unix_ms.unwrap_or(now);
    let start = window
        .start_unix_ms
        .unwrap_or_else(|| end.saturating_sub(DAY_MS));
    if start >= end
        || start < now.saturating_sub(RETENTION_MS)
        || end > now.saturating_add(300_000)
        || end - start > RETENTION_MS
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match tokio::task::spawn_blocking(move || {
        let database = open(&state.database).map_err(io::Error::other)?;
        let stats = query(&database, start, end).map_err(io::Error::other)?;
        serde_json::to_string(&stats).map_err(io::Error::other)
    })
    .await
    {
        Ok(Ok(body)) => (
            [
                ("content-type", "application/json"),
                ("cache-control", "no-store"),
            ],
            body,
        )
            .into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
