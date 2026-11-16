//! End-to-end coverage of the toolchain origin list: the redirect manifest,
//! its on-disk cache, and per-request origin failover for the catalogue.
//!
//! Every server here is a loopback listener; nothing reaches the real
//! network. A redirect manifest may only name `https://` origins, so the
//! failover tests drive `fetch_catalogue_from_origins` with loopback origins
//! directly, and the redirect tests assert on the resolved origin list.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::common::catalogue_env::CatalogueEnvGuard;
use soldr_cli::fetch::manifest_lookup::{
    effective_origins, fetch_catalogue_from_origins, get_or_fetch, resolve_redirect_origins,
    OriginPlan, OriginsSource, BUILTIN_TOOLCHAIN_ORIGINS, MANIFEST_DISABLE_ENV_VAR,
    REDIRECT_CACHE_FILE_NAME, TOOLCHAIN_ORIGIN_ENV_VAR, TOOLCHAIN_REDIRECT_ENV_VAR,
    TOOLCHAIN_REDIRECT_URL_ENV_VAR,
};

/// Backstop so a server thread cannot outlive a wedged test forever; `Drop`
/// stops it as soon as its test is done.
const SERVER_BACKSTOP: Duration = Duration::from_secs(60);

type Routes = Arc<Mutex<HashMap<String, (u16, String)>>>;

/// A loopback HTTP server answering each path from a mutable route table
/// (unknown paths are 404) and recording every path it was asked for.
struct RoutedServer {
    base: String,
    routes: Routes,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl RoutedServer {
    fn spawn(routes: &[(&str, u16, &str)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let base = format!("http://{}", listener.local_addr().expect("local_addr"));
        let table: Routes = Arc::new(Mutex::new(
            routes
                .iter()
                .map(|(path, status, body)| (path.to_string(), (*status, body.to_string())))
                .collect(),
        ));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (served, seen, halt) = (Arc::clone(&table), Arc::clone(&requests), Arc::clone(&stop));
        let handle = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("nonblocking fixture listener");
            let deadline = Instant::now() + SERVER_BACKSTOP;
            while !halt.load(Ordering::SeqCst) && Instant::now() < deadline {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                // On macOS a socket accepted from a nonblocking listener
                // inherits O_NONBLOCK; this fixture serves synchronously.
                stream
                    .set_nonblocking(false)
                    .expect("blocking fixture stream");
                let mut request = [0_u8; 4096];
                let Ok(read) = stream.read(&mut request) else {
                    continue;
                };
                let text = String::from_utf8_lossy(&request[..read]);
                let path = text
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/")
                    .to_string();
                seen.lock().unwrap().push(path.clone());
                let (status, body) = served
                    .lock()
                    .unwrap()
                    .get(&path)
                    .cloned()
                    .unwrap_or((404, "not found".into()));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            base,
            routes: table,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn set(&self, path: &str, status: u16, body: &str) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_string(), (status, body.to_string()));
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for RoutedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn v1_catalogue(owner: &str) -> String {
    serde_json::json!({
        "entries": [{
            "owner": owner,
            "repo": "redirect-repo",
            "tag": "v1.0.0",
            "asset": "redirect-asset.zip",
            "url": "https://example.invalid/redirect-asset.zip",
            "sha256": "a".repeat(64),
        }]
    })
    .to_string()
}

fn redirect_manifest(origins: &[&str]) -> String {
    serde_json::json!({
        "schema_version": 1,
        "toolchain": { "origins": origins },
    })
    .to_string()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn catalogue_fails_over_from_a_404_origin() {
    let down = RoutedServer::spawn(&[]);
    let mirror = RoutedServer::spawn(&[("/catalogue.v1.json", 200, &v1_catalogue("mirror"))]);
    let origins = vec![down.base.clone(), mirror.base.clone()];

    let (index, served) = runtime()
        .block_on(fetch_catalogue_from_origins(&origins))
        .expect("the second origin serves the catalogue");

    assert_eq!(index.entries.len(), 1);
    assert_eq!(index.entries[0].owner, "mirror");
    assert_eq!(
        served.as_deref(),
        Some(mirror.url("/catalogue.v1.json").as_str())
    );
    assert_eq!(
        down.requests(),
        ["/catalogue.v2.json", "/catalogue.v1.json"],
        "a 404 origin is asked for v2, then its own v1, before failing over"
    );
}

#[test]
fn catalogue_fails_over_on_a_server_error() {
    let broken = RoutedServer::spawn(&[("/catalogue.v2.json", 503, "busy")]);
    let mirror = RoutedServer::spawn(&[("/catalogue.v1.json", 200, &v1_catalogue("mirror"))]);
    let origins = vec![broken.base.clone(), mirror.base.clone()];

    let (index, _) = runtime()
        .block_on(fetch_catalogue_from_origins(&origins))
        .expect("a 5xx origin hands over to the next");
    assert_eq!(index.entries[0].owner, "mirror");
    assert_eq!(broken.requests(), ["/catalogue.v2.json"]);
}

#[test]
fn catalogue_fails_over_when_an_origin_refuses_connections() {
    let refused = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}", listener.local_addr().expect("addr"))
    };
    let mirror = RoutedServer::spawn(&[("/catalogue.v1.json", 200, &v1_catalogue("mirror"))]);
    let (index, _) = runtime()
        .block_on(fetch_catalogue_from_origins(&[
            refused,
            mirror.base.clone(),
        ]))
        .expect("a network error hands over to the next origin");
    assert_eq!(index.entries[0].owner, "mirror");
}

#[test]
fn content_errors_do_not_fail_over() {
    for (status, body) in [(200, "this is not json"), (403, "forbidden")] {
        let bad = RoutedServer::spawn(&[("/catalogue.v1.json", status, body)]);
        let mirror = RoutedServer::spawn(&[("/catalogue.v1.json", 200, &v1_catalogue("mirror"))]);
        let result = runtime().block_on(fetch_catalogue_from_origins(&[
            bad.base.clone(),
            mirror.base.clone(),
        ]));
        assert!(
            result.is_err(),
            "HTTP {status} with a bad answer must be a hard error, not a failover"
        );
        assert!(
            mirror.requests().is_empty(),
            "HTTP {status}: the next origin must not be asked, got {:?}",
            mirror.requests()
        );
    }
}

#[test]
fn redirect_manifest_is_cached_and_reused_when_unreachable() {
    let server = RoutedServer::spawn(&[(
        "/soldr-redirect.v1.json",
        200,
        &redirect_manifest(&["https://zackees.github.io/soldr-toolchain/"]),
    )]);
    let manifest_url = server.url("/soldr-redirect.v1.json");
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = dir.path().join(REDIRECT_CACHE_FILE_NAME);
    let expected = vec![
        "https://zackees.github.io/soldr-toolchain".to_string(),
        "https://forge-ci.github.io/soldr-toolchain".to_string(),
    ];
    let rt = runtime();

    let fresh = rt.block_on(resolve_redirect_origins(&manifest_url, Some(&cache)));
    assert_eq!(fresh.source, OriginsSource::Manifest);
    assert_eq!(fresh.origins, expected);
    assert!(cache.is_file(), "a good manifest is written to the cache");

    server.set("/soldr-redirect.v1.json", 500, "down");
    let cached = rt.block_on(resolve_redirect_origins(&manifest_url, Some(&cache)));
    assert_eq!(cached.source, OriginsSource::Cache);
    assert_eq!(cached.origins, expected);

    // An invalid manifest is a fetch failure: it neither replaces the cache
    // nor gets used.
    server.set(
        "/soldr-redirect.v1.json",
        200,
        r#"{"schema_version":2,"toolchain":{"origins":["https://evil.example"]}}"#,
    );
    let still_cached = rt.block_on(resolve_redirect_origins(&manifest_url, Some(&cache)));
    assert_eq!(still_cached.source, OriginsSource::Cache);
    assert_eq!(still_cached.origins, expected);

    std::fs::remove_file(&cache).expect("remove cache");
    let builtin = rt.block_on(resolve_redirect_origins(&manifest_url, Some(&cache)));
    assert_eq!(builtin.source, OriginsSource::Builtin);
    assert_eq!(builtin.origins, effective_origins(&[]));
    assert_eq!(builtin.origins, BUILTIN_TOOLCHAIN_ORIGINS.map(String::from));
}

#[test]
fn explicit_overrides_and_the_off_switch_never_fetch_the_manifest() {
    let env = CatalogueEnvGuard::acquire();
    let server = RoutedServer::spawn(&[(
        "/soldr-redirect.v1.json",
        200,
        &redirect_manifest(&["https://zackees.github.io/soldr-toolchain"]),
    )]);
    env.set(
        TOOLCHAIN_REDIRECT_URL_ENV_VAR,
        &server.url("/soldr-redirect.v1.json"),
    );
    let rt = runtime();

    env.set(
        TOOLCHAIN_ORIGIN_ENV_VAR,
        "https://explicit.example/soldr-toolchain/",
    );
    let explicit = rt.block_on(OriginPlan::from_env().resolve());
    assert_eq!(explicit.source, OriginsSource::Explicit);
    assert_eq!(
        explicit.origins,
        ["https://explicit.example/soldr-toolchain".to_string()]
    );
    env.unset(TOOLCHAIN_ORIGIN_ENV_VAR);

    env.set(TOOLCHAIN_REDIRECT_ENV_VAR, "0");
    let builtin = rt.block_on(OriginPlan::from_env().resolve());
    assert_eq!(builtin.source, OriginsSource::Builtin);
    env.unset(TOOLCHAIN_REDIRECT_ENV_VAR);

    // The offline switch disables the catalogue and, with it, the redirect.
    env.set(MANIFEST_DISABLE_ENV_VAR, "1");
    let disabled = rt.block_on(get_or_fetch());
    assert!(disabled.entries.is_empty());
    env.unset(MANIFEST_DISABLE_ENV_VAR);

    assert!(
        server.requests().is_empty(),
        "no override path may request the redirect manifest, got {:?}",
        server.requests()
    );

    let redirected = rt.block_on(OriginPlan::from_env().resolve());
    assert_eq!(redirected.source, OriginsSource::Manifest);
    assert_eq!(
        redirected.origins,
        effective_origins(&["https://zackees.github.io/soldr-toolchain".to_string()])
    );
    assert_eq!(server.requests(), ["/soldr-redirect.v1.json"]);
}

#[test]
fn explicit_origin_drives_the_process_catalogue() {
    let env = CatalogueEnvGuard::acquire();
    let origin = RoutedServer::spawn(&[("/catalogue.v1.json", 200, &v1_catalogue("explicit"))]);
    env.set(TOOLCHAIN_ORIGIN_ENV_VAR, &origin.base);

    let index = runtime().block_on(get_or_fetch());
    assert_eq!(index.entries.len(), 1);
    assert_eq!(index.entries[0].owner, "explicit");
    assert_eq!(
        origin.requests(),
        ["/catalogue.v2.json", "/catalogue.v1.json"],
        "SOLDR_TOOLCHAIN_ORIGIN keeps v2-then-v1 at that one origin"
    );
}
