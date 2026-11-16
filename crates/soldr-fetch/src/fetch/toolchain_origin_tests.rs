use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

const SPEC_MANIFEST: &str = r#"{
  "schema_version": 1,
  "toolchain": {
    "origins": [
      "https://forge-ci.github.io/soldr-toolchain",
      "https://zackees.github.io/soldr-toolchain"
    ]
  }
}"#;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// A loopback URL nothing listens on: the connection is refused at once,
/// without touching the real network.
fn refused_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    format!("http://{addr}/soldr-redirect.v1.json")
}

fn manifest_with(origins: &[&str]) -> String {
    serde_json::json!({
        "schema_version": 1,
        "toolchain": { "origins": origins },
    })
    .to_string()
}

#[test]
fn builtin_origins_prefer_the_mirror_and_keep_the_legacy_site() {
    assert_eq!(
        BUILTIN_TOOLCHAIN_ORIGINS,
        [
            "https://forge-ci.github.io/soldr-toolchain",
            "https://zackees.github.io/soldr-toolchain",
        ]
    );
    assert_eq!(DEFAULT_TOOLCHAIN_ORIGIN, BUILTIN_TOOLCHAIN_ORIGINS[0]);
    assert_eq!(
        DEFAULT_TOOLCHAIN_REDIRECT_URL,
        "https://forge-ci.github.io/soldr-redirect.v1.json"
    );
    assert_eq!(
        TOOLCHAIN_REDIRECT_URL_ENV_VAR,
        "SOLDR_TOOLCHAIN_REDIRECT_URL"
    );
    assert_eq!(TOOLCHAIN_REDIRECT_ENV_VAR, "SOLDR_TOOLCHAIN_REDIRECT");
    assert!(REDIRECT_FETCH_TIMEOUT <= Duration::from_secs(5));
}

#[test]
fn parses_the_published_manifest() {
    assert_eq!(
        parse_redirect_manifest(SPEC_MANIFEST).expect("spec manifest is valid"),
        BUILTIN_TOOLCHAIN_ORIGINS.map(String::from).to_vec()
    );
}

#[test]
fn strips_trailing_slashes_and_ignores_unknown_fields() {
    let body = serde_json::json!({
        "schema_version": 1,
        "note": "future field",
        "toolchain": {
            "origins": ["https://mirror.example/soldr-toolchain/"],
            "comment": "ignored",
        },
    })
    .to_string();
    assert_eq!(
        parse_redirect_manifest(&body).unwrap(),
        vec!["https://mirror.example/soldr-toolchain".to_string()]
    );
}

#[test]
fn rejects_invalid_manifests() {
    let cases: Vec<(&str, String)> = vec![
        ("not json", "not json".into()),
        ("array root", "[]".into()),
        (
            "schema 2",
            r#"{"schema_version":2,"toolchain":{"origins":["https://a.example"]}}"#.into(),
        ),
        (
            "schema as string",
            r#"{"schema_version":"1","toolchain":{"origins":["https://a.example"]}}"#.into(),
        ),
        (
            "missing schema",
            r#"{"toolchain":{"origins":["https://a.example"]}}"#.into(),
        ),
        ("missing toolchain", r#"{"schema_version":1}"#.into()),
        (
            "origins not a list",
            r#"{"schema_version":1,"toolchain":{"origins":"https://a.example"}}"#.into(),
        ),
        ("empty origins", manifest_with(&[])),
        (
            "non-string origin",
            r#"{"schema_version":1,"toolchain":{"origins":[1]}}"#.into(),
        ),
        ("http origin", manifest_with(&["http://a.example/x"])),
        (
            "one bad origin poisons the list",
            manifest_with(&["https://a.example", "ftp://b.example"]),
        ),
        ("credentials", manifest_with(&["https://user:pw@a.example"])),
        ("query", manifest_with(&["https://a.example/x?token=1"])),
        ("fragment", manifest_with(&["https://a.example/x#frag"])),
        ("relative", manifest_with(&["/soldr-toolchain"])),
        (
            "too many origins",
            manifest_with(&["https://a.example"; MAX_REDIRECT_ORIGINS + 1]),
        ),
    ];
    for (name, body) in cases {
        assert!(
            parse_redirect_manifest(&body).is_err(),
            "{name}: {body} must be rejected"
        );
    }
    let huge = format!(
        r#"{{"schema_version":1,"toolchain":{{"origins":["https://a.example"]}},"pad":"{}"}}"#,
        "x".repeat(MAX_REDIRECT_BODY_BYTES)
    );
    assert!(parse_redirect_manifest(&huge).is_err());
}

#[test]
fn effective_origins_append_builtins_and_dedupe_in_order() {
    assert_eq!(
        effective_origins(&[]),
        BUILTIN_TOOLCHAIN_ORIGINS.map(String::from).to_vec()
    );
    assert_eq!(
        effective_origins(&[
            "https://zackees.github.io/soldr-toolchain".into(),
            "https://new.example/soldr-toolchain".into(),
            "https://zackees.github.io/soldr-toolchain/".into(),
        ]),
        vec![
            "https://zackees.github.io/soldr-toolchain".to_string(),
            "https://new.example/soldr-toolchain".to_string(),
            "https://forge-ci.github.io/soldr-toolchain".to_string(),
        ]
    );
}

#[test]
fn plan_precedence_explicit_then_switch_then_redirect() {
    let cache = Some(PathBuf::from("/cache/toolchain-redirect.v1.json"));
    assert_eq!(
        OriginPlan::from_values(
            Some("https://mirror.example/x/".into()),
            Some("1"),
            Some("https://r.example/m.json".into()),
            cache.clone(),
        ),
        OriginPlan::Explicit("https://mirror.example/x".into()),
        "SOLDR_TOOLCHAIN_ORIGIN wins over every redirect setting"
    );
    for off in ["0", "false", "no", "off", " OFF "] {
        assert_eq!(
            OriginPlan::from_values(None, Some(off), None, cache.clone()),
            OriginPlan::Builtin,
            "SOLDR_TOOLCHAIN_REDIRECT={off:?} disables the manifest fetch"
        );
    }
    for on in [None, Some(""), Some("1"), Some("typo")] {
        assert_eq!(
            OriginPlan::from_values(None, on, None, cache.clone()),
            OriginPlan::Redirect {
                manifest_url: DEFAULT_TOOLCHAIN_REDIRECT_URL.into(),
                cache_file: cache.clone(),
            },
            "SOLDR_TOOLCHAIN_REDIRECT={on:?} keeps the redirect"
        );
    }
    assert_eq!(
        OriginPlan::from_values(None, None, Some("https://r.example/m.json".into()), None),
        OriginPlan::Redirect {
            manifest_url: "https://r.example/m.json".into(),
            cache_file: None,
        }
    );
    assert_eq!(
        OriginPlan::from_values(None, None, None, None).without_redirect(),
        OriginPlan::Builtin
    );
    assert_eq!(
        OriginPlan::Explicit("https://a.example".into()).without_redirect(),
        OriginPlan::Explicit("https://a.example".into())
    );
}

#[test]
fn explicit_and_builtin_plans_resolve_without_any_request() {
    let rt = runtime();
    let explicit = rt.block_on(OriginPlan::Explicit("https://a.example".into()).resolve());
    assert_eq!(explicit.origins, vec!["https://a.example".to_string()]);
    assert_eq!(explicit.source, OriginsSource::Explicit);
    let builtin = rt.block_on(OriginPlan::Builtin.resolve());
    assert_eq!(builtin.origins, effective_origins(&[]));
    assert_eq!(builtin.source, OriginsSource::Builtin);
}

#[test]
fn cache_round_trips_and_is_keyed_by_manifest_url() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("nested").join(REDIRECT_CACHE_FILE_NAME);
    let body = manifest_with(&["https://new.example/soldr-toolchain"]);
    write_redirect_cache(&file, "https://r.example/m.json", &body).unwrap();
    assert_eq!(
        read_redirect_cache(&file, "https://r.example/m.json"),
        Some(vec!["https://new.example/soldr-toolchain".to_string()])
    );
    assert_eq!(
        read_redirect_cache(&file, "https://other.example/m.json"),
        None,
        "another manifest URL's cache must not be used"
    );
    std::fs::write(&file, "{not json").unwrap();
    assert_eq!(read_redirect_cache(&file, "https://r.example/m.json"), None);
    std::fs::write(
        &file,
        serde_json::json!({
            "manifest_url": "https://r.example/m.json",
            "manifest": manifest_with(&["http://downgraded.example"]),
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        read_redirect_cache(&file, "https://r.example/m.json"),
        None,
        "a cached manifest is re-validated on read"
    );
}

#[test]
fn unreachable_manifest_falls_back_to_cache_then_builtins() {
    let rt = runtime();
    let url = refused_url();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(REDIRECT_CACHE_FILE_NAME);

    let no_cache = rt.block_on(resolve_redirect_origins(&url, Some(&file)));
    assert_eq!(no_cache.source, OriginsSource::Builtin);
    assert_eq!(no_cache.origins, effective_origins(&[]));

    write_redirect_cache(
        &file,
        &url,
        &manifest_with(&["https://zackees.github.io/soldr-toolchain"]),
    )
    .unwrap();
    let cached = rt.block_on(resolve_redirect_origins(&url, Some(&file)));
    assert_eq!(cached.source, OriginsSource::Cache);
    assert_eq!(
        cached.origins,
        vec![
            "https://zackees.github.io/soldr-toolchain".to_string(),
            "https://forge-ci.github.io/soldr-toolchain".to_string(),
        ]
    );

    let without_cache_file = rt.block_on(resolve_redirect_origins(&url, None));
    assert_eq!(without_cache_file.source, OriginsSource::Builtin);
}

#[test]
fn status_failover_is_404_and_5xx_only() {
    for status in [404, 500, 502, 503, 504, 599] {
        assert!(status_fails_over(status), "{status} must fail over");
    }
    for status in [200, 301, 304, 400, 401, 403, 410, 429, 600] {
        assert!(!status_fails_over(status), "{status} must not fail over");
    }
    assert!(matches!(
        OriginError::from_status(404, SoldrError::Network("x".into())),
        OriginError::Unavailable(_)
    ));
    assert!(matches!(
        OriginError::from_status(403, SoldrError::Network("x".into())),
        OriginError::Fatal(_)
    ));
}

fn origins(names: &[&str]) -> Vec<String> {
    names
        .iter()
        .map(|name| format!("https://{name}.example"))
        .collect()
}

#[test]
fn failover_moves_on_when_an_origin_is_unavailable() {
    let calls = AtomicUsize::new(0);
    let result = runtime().block_on(fetch_from_origins(
        &origins(&["a", "b", "c"]),
        "test file",
        |origin, is_last| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if origin.starts_with("https://a.") {
                    assert!(!is_last);
                    Err(OriginError::Unavailable(SoldrError::Network(
                        "HTTP 404".into(),
                    )))
                } else {
                    Ok(origin)
                }
            }
        },
    ));
    assert_eq!(result.unwrap(), "https://b.example");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "c is never asked");
}

#[test]
fn failover_stops_on_integrity_errors() {
    let calls = AtomicUsize::new(0);
    let result: Result<(), _> = runtime().block_on(fetch_from_origins(
        &origins(&["a", "b"]),
        "test file",
        |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(OriginError::Fatal(SoldrError::Other(
                    "sha256 mismatch".into(),
                )))
            }
        },
    ));
    assert!(result.unwrap_err().to_string().contains("sha256 mismatch"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "an integrity error must not be retried on another origin"
    );
}

#[test]
fn failover_returns_the_last_error_when_every_origin_is_down() {
    let seen_last = AtomicUsize::new(0);
    let result: Result<(), _> = runtime().block_on(fetch_from_origins(
        &origins(&["a", "b"]),
        "test file",
        |origin, is_last| {
            if is_last {
                seen_last.fetch_add(1, Ordering::SeqCst);
            }
            async move {
                Err(OriginError::Unavailable(SoldrError::Network(format!(
                    "down: {origin}"
                ))))
            }
        },
    ));
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("down: https://b.example"));
    assert_eq!(seen_last.load(Ordering::SeqCst), 1);

    let empty: Result<(), _> =
        runtime().block_on(fetch_from_origins(&[], "test file", |_, _| async {
            Ok(())
        }));
    assert!(
        empty.is_err(),
        "an empty origin list is an error, not a hang"
    );
}
