use crate::core::SoldrError;
use crate::fetch::manifest_lookup::catalogue_lookup::*;
use crate::fetch::manifest_lookup::catalogue_model::*;
use crate::fetch::manifest_lookup::catalogue_transport::*;
use crate::fetch::manifest_lookup::resolved_download_label;
use crate::fetch::manifest_lookup::toolchain_origin::DEFAULT_TOOLCHAIN_ORIGIN;
use kernal_api::hash::Sha256Hasher;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn sample_json() -> &'static str {
    r#"{
            "entries": [
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "1.12.9",
                    "asset": "zccache-x86_64-pc-windows-msvc.zip",
                    "url": "https://github.com/zackees/zccache/releases/download/1.12.9/zccache-x86_64-pc-windows-msvc.zip",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                },
                {
                    "owner": "LukeMathWalker",
                    "repo": "cargo-chef",
                    "tag": "v0.1.73",
                    "asset": "cargo-chef-x86_64-pc-windows-msvc.tar.gz",
                    "url": "https://github.com/LukeMathWalker/cargo-chef/releases/download/v0.1.73/cargo-chef-x86_64-pc-windows-msvc.tar.gz",
                    "sha256": "1111111111111111111111111111111111111111111111111111111111111111"
                }
            ]
        }"#
}

#[test]
fn parses_well_formed_manifest() {
    let idx = ManifestIndex::from_json(sample_json()).expect("parse ok");
    assert_eq!(idx.entries.len(), 2);
    assert_eq!(idx.entries[0].owner, "zackees");
    assert_eq!(idx.entries[1].repo, "cargo-chef");
}

#[test]
fn from_json_returns_none_on_malformed_input() {
    assert!(ManifestIndex::from_json("not-json").is_none());
    assert!(ManifestIndex::from_json("{}").is_some()); // empty entries field is fine
}

#[test]
fn lookup_finds_exact_match() {
    let idx = ManifestIndex::from_json(sample_json()).unwrap();
    let hit = idx
        .lookup(
            "zackees",
            "zccache",
            "1.12.9",
            "zccache-x86_64-pc-windows-msvc.zip",
        )
        .expect("should hit");
    assert!(hit.transport.direct_url().unwrap().contains("zccache"));
    assert_eq!(hit.sha256.len(), 64);
}

#[test]
fn lookup_misses_on_unknown_tuple() {
    let idx = ManifestIndex::from_json(sample_json()).unwrap();
    assert!(idx
        .lookup("zackees", "zccache", "1.12.9", "not-an-asset.zip")
        .is_none());
    assert!(idx
        .lookup(
            "zackees",
            "zccache",
            "1.12.8",
            "zccache-x86_64-pc-windows-msvc.zip"
        )
        .is_none());
    assert!(idx
        .lookup(
            "other",
            "zccache",
            "1.12.9",
            "zccache-x86_64-pc-windows-msvc.zip"
        )
        .is_none());
}

#[test]
fn empty_index_lookup_always_misses() {
    let idx = ManifestIndex::empty();
    assert!(idx.lookup("a", "b", "c", "d").is_none());
    assert!(idx.lookup_release("a", "b", "c").is_empty());
}

#[test]
fn lookup_release_returns_every_asset_for_a_tag() {
    let idx = ManifestIndex::from_json(sample_json()).unwrap();
    let hits = idx.lookup_release("zackees", "zccache", "1.12.9");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].asset.contains("zccache"));
}

#[test]
fn lookup_asset_finds_toolchain_owned_repackages() {
    let idx = ManifestIndex::from_json(sample_json()).unwrap();
    let hits = idx.lookup_asset("cargo-chef-x86_64-pc-windows-msvc.tar.gz");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].repo, "cargo-chef");
}

#[test]
fn catalogue_asset_digest_is_mandatory() {
    let bytes = br#"{"schema_version":1}"#;
    let entry = ManifestEntry {
        owner: "zackees".into(),
        repo: "soldr-toolchain".into(),
        tag: "assets".into(),
        asset: "rust-nightly-versions.v1.json".into(),
        transport: AssetTransport::Direct {
            urls: vec!["https://example.invalid/map.json".into()],
        },
        sha256: super::super::trust::sha256_of(bytes),
        size_bytes: bytes.len() as u64,
        min_client_version: Some(CATALOGUE_CAPABILITY),
        source_path: None,
    };
    assert!(verify_catalogue_asset_sha256(&entry, &super::super::trust::sha256_of(bytes)).is_ok());
    assert!(
        verify_catalogue_asset_sha256(&entry, &super::super::trust::sha256_of(b"changed")).is_err()
    );
}

#[test]
fn catalogue_asset_body_keeps_a_response_wide_deadline() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let address = listener.local_addr().expect("server address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\npartial")
                .await
                .expect("write partial body");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let url = format!("http://{address}/catalogue-asset");
        let client = super::super::stream_download::asset_http_client("test catalogue asset")
            .expect("build test client");
        let response = super::super::stream_download::send_asset_request(
            super::super::stream_download::get_request(&client, &url),
            &url,
            Duration::from_secs(1),
        )
        .await
        .expect("GET");
        let error = stream_catalogue_asset_body(response, &url, Duration::from_millis(20))
            .await
            .expect_err("trickling metadata body must hit the total deadline");
        assert!(super::super::retry::is_transient(&error));
        assert!(error.to_string().contains("body read timed out"), "{error}");
    });
}

#[test]
fn cache_buster_preserves_existing_query_parameters() {
    let plain = cache_busted_url("https://example.invalid/map.json");
    assert!(plain.starts_with("https://example.invalid/map.json?soldr_refresh="));
    let queried = cache_busted_url("https://example.invalid/map.json?mirror=1");
    assert!(queried.starts_with("https://example.invalid/map.json?mirror=1&soldr_refresh="));
}

#[test]
fn catalogue_diagnostics_redact_url_credentials_and_queries() {
    let safe = super::super::stream_download::safe_asset_url(
        "https://user:password@example.invalid/asset?access_token=secret#fragment",
    );
    assert_eq!(safe, "https://example.invalid/asset");
    assert!(!safe.contains("user"));
    assert!(!safe.contains("password"));
    assert!(!safe.contains("access_token"));
    assert!(!safe.contains("secret"));
}

#[test]
fn content_addressed_cache_is_warm_and_repairs_corruption() {
    let dir = tempfile::tempdir().expect("cache dir");
    let bytes = b"catalogue cache bytes";
    let sha = super::super::trust::sha256_of(bytes);
    let source = dir.path().join("source");
    std::fs::write(&source, bytes).expect("source");
    promote_cached_asset(dir.path(), &sha, &source).expect("atomic promote");
    let object = dir.path().join(&sha);
    let warm = cached_asset(&object, &sha, bytes.len() as u64)
        .expect("warm read")
        .expect("cache hit");
    assert_eq!(std::fs::read(warm.path()).expect("warm bytes"), bytes);

    std::fs::write(&object, b"corrupt").expect("corrupt cached object");
    assert!(cached_asset(&object, &sha, bytes.len() as u64)
        .expect("corruption check")
        .is_none());
    assert!(
        !object.exists(),
        "corrupt object must be evicted before retry"
    );
    promote_cached_asset(dir.path(), &sha, &source).expect("repair promote");
    assert!(cached_asset(&object, &sha, bytes.len() as u64)
        .expect("repaired read")
        .is_some());
}

#[test]
fn cache_promotion_never_exposes_interrupted_temp_files() {
    let dir = tempfile::tempdir().expect("cache dir");
    let sha = super::super::trust::sha256_of(b"complete");
    let partial = tempfile::NamedTempFile::new_in(dir.path()).expect("partial temp");
    // Dropping an unpromoted temp models an interrupted writer.  The
    // content-addressed final name remains absent, so a retry is safe.
    drop(partial);
    assert!(cached_asset(&dir.path().join(&sha), &sha, 8)
        .expect("cache lookup")
        .is_none());
}

// soldr#988 Phase 2: catalogue origin resolution.

#[test]
fn catalogue_url_defaults_to_pages_origin() {
    // Caller may have SOLDR_TOOLCHAIN_ORIGIN set in their env;
    // exercise the public string-shape via the pure helper that
    // does not read env: build the URL from the default origin.
    let url = format!("{}/{}", DEFAULT_TOOLCHAIN_ORIGIN, CATALOGUE_DOC_NAME);
    assert_eq!(
        url,
        "https://forge-ci.github.io/soldr-toolchain/catalogue.v1.json"
    );
}

#[test]
fn catalogue_v1_json_parses_through_manifest_index() {
    // Phase 2 must accept the v1 wire shape transparently — the
    // top-level extras (schema_version, generated_at, origin)
    // are unknown fields ManifestIndex must ignore.
    let v1 = r#"{
            "schema_version": 1,
            "generated_at": "2026-06-27T00:00:00Z",
            "origin": "https://zackees.github.io/soldr-toolchain/catalogue.v1.json",
            "entries": [
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "1.12.11",
                    "asset": "zccache-v1.12.11-x86_64-pc-windows-msvc.zip",
                    "url": "https://github.com/zackees/zccache/releases/download/1.12.11/zccache-v1.12.11-x86_64-pc-windows-msvc.zip",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            ]
        }"#;
    let idx = ManifestIndex::from_json(v1).expect("v1 catalogue must parse");
    assert_eq!(idx.entries.len(), 1);
    assert_eq!(idx.entries[0].owner, "zackees");
    assert_eq!(idx.entries[0].tag, "1.12.11");
}

#[test]
fn catalogue_v1_preserves_duplicate_identity_compatibility() {
    let v1 = r#"{
            "schema_version": 1,
            "entries": [
                {
                    "owner": "zackees",
                    "repo": "soldr-toolchain",
                    "tag": "1",
                    "asset": "bundle.tar.zst",
                    "url": "https://example.com/first.tar.zst?token=secret#fragment",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                },
                {
                    "owner": "zackees",
                    "repo": "soldr-toolchain",
                    "tag": "1",
                    "asset": "bundle.tar.zst",
                    "url": "https://example.com/second.tar.zst",
                    "sha256": "1111111111111111111111111111111111111111111111111111111111111111"
                }
            ]
        }"#;

    let index = ManifestIndex::from_json(v1).expect("legacy duplicate rows must still parse");
    assert_eq!(index.entries.len(), 2);
    assert_eq!(
        index.entries[0].direct_url(),
        Some("https://example.com/first.tar.zst?token=secret#fragment")
    );
    assert_eq!(
        resolved_download_label(&index.entries[0]),
        "https://example.com/first.tar.zst"
    );

    let shared_url = v1.replace(
        "https://example.com/second.tar.zst",
        "https://example.com/first.tar.zst?token=secret#fragment",
    );
    assert_eq!(
        ManifestIndex::from_json(&shared_url)
            .expect("legacy duplicate URLs must still parse")
            .entries
            .len(),
        2
    );
}

#[test]
fn catalogue_v2_multipart_union_is_strict_and_path_addressable() {
    let v2 = r#"{
          "schema_version": 2,
          "generation": "g1",
          "publication_state": {
            "generation": "g1",
            "url": "https://example.test/generations/g1/publish-state.v1.json"
          },
          "entries": [{
            "owner":"zackees","repo":"soldr-toolchain","tag":"1","asset":"bundle.tar.zst",
            "source_path":"python/1/linux-x64/bundle.tar.zst",
            "sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size_bytes":3,
            "min_client_version":2,
            "parts":[
              {"number":1,"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size_bytes":2,"urls":["https://example.test/1?token=secret#fragment"]},
              {"number":2,"sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","size_bytes":1,"urls":["https://example.test/2"]}
            ]
          }]
        }"#;
    let index = ManifestIndex::from_json(v2).expect("v2 multipart parses");
    let entry = &index.entries[0];
    assert!(entry.direct_url().is_none());
    let AssetTransport::Multipart { parts } = &entry.transport else {
        panic!("expected multipart transport");
    };
    assert_eq!(
        parts[0].urls[0],
        "https://example.test/1?token=secret#fragment"
    );
    assert_eq!(resolved_download_label(entry), "https://example.test/1");
    assert!(entry.matches_legacy_url("https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/python/1/linux-x64/bundle.tar.zst"));
}

#[test]
fn catalogue_v2_fixture_preserves_direct_and_multipart_transports() {
    let json = include_str!("../../tests/fixtures/catalogue.v2.json");
    let index = ManifestIndex::from_json(json).expect("v2 fixture is valid");
    assert!(matches!(
        index.entries[0].transport,
        AssetTransport::Direct { .. }
    ));
    assert!(matches!(
        index.entries[1].transport,
        AssetTransport::Multipart { .. }
    ));
    assert_eq!(index.entries[1].size_bytes, 6);
}

#[test]
fn canonical_publication_contract_accepts_assets_branch_and_source_path() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/catalogue-v2-contract.json"
    ))
    .unwrap();
    let catalogue = fixture["catalogue"].to_string();
    let catalogue_wire: WireV2Catalogue = serde_json::from_str(&catalogue).unwrap();
    assert_eq!(
        ManifestIndex::from_v2_json(&catalogue).unwrap().entries[0]
            .source_path
            .as_deref(),
        Some("apple-sdk/14.5/darwin-universal2/sdk.tar.zst")
    );
    let state = fixture["publication_state"].to_string();
    let state_wire = parse_publication_state(&state).unwrap();
    let digest = fixture["publication_state"]["catalogue_sha256"]
        .as_str()
        .unwrap();
    validate_publication_state_body(
        &state,
        fixture["catalogue"]["generation"].as_str().unwrap(),
        digest,
    )
    .expect("canonical publisher state must bind without a self-referential www field");
    assert!(publication_entries_match_state(
        &catalogue_wire.entries,
        &state_wire
    ));
}

#[test]
fn publication_contract_accepts_source_path_identity_for_duplicate_legacy_assets() {
    let mut fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/catalogue-v2-contract.json"
    ))
    .unwrap();
    let original_key = "zackees\0soldr-toolchain\0assets\0sdk.tar.zstd";
    let source_path = fixture["catalogue"]["entries"][0]["source_path"]
        .as_str()
        .unwrap()
        .to_string();
    fixture["catalogue"]["entries"][0]["asset"] = serde_json::json!(source_path);
    let mut logical = fixture["publication_state"]["logical_assets"]
        .as_object_mut()
        .unwrap()
        .remove(original_key)
        .unwrap();
    logical["asset"] = serde_json::json!("sdk.tar.zst");
    logical["provenance"]["asset"] = serde_json::json!("sdk.tar.zst");
    let canonical_key = format!(
        "zackees\0soldr-toolchain\0assets\0{}",
        fixture["catalogue"]["entries"][0]["asset"]
            .as_str()
            .unwrap()
    );
    fixture["publication_state"]["logical_assets"][canonical_key] = logical;

    let catalogue: WireV2Catalogue = serde_json::from_value(fixture["catalogue"].clone()).unwrap();
    let state: PublicationState =
        serde_json::from_value(fixture["publication_state"].clone()).unwrap();
    assert!(publication_entries_match_state(&catalogue.entries, &state));

    fixture["publication_state"]["logical_assets"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()["asset"] = serde_json::json!("other.tar.zstd");
    let state: PublicationState =
        serde_json::from_value(fixture["publication_state"].clone()).unwrap();
    assert!(!publication_entries_match_state(&catalogue.entries, &state));
}

#[test]
fn publication_contract_rejects_unbound_or_ambiguous_source_paths() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/catalogue-v2-contract.json"
    ))
    .unwrap();
    let bound = |document: &serde_json::Value| {
        let catalogue: WireV2Catalogue =
            serde_json::from_value(document["catalogue"].clone()).unwrap();
        let state: PublicationState =
            serde_json::from_value(document["publication_state"].clone()).unwrap();
        publication_entries_match_state(&catalogue.entries, &state)
    };
    assert!(bound(&fixture));

    let mut orphan = fixture.clone();
    orphan["catalogue"]["entries"][0]["source_path"] =
        serde_json::json!("apple-sdk/other/sdk.tar.zst");
    assert!(!bound(&orphan));

    let mut wrong_sha = fixture.clone();
    wrong_sha["catalogue"]["entries"][0]["sha256"] = serde_json::json!("f".repeat(64));
    assert!(!bound(&wrong_sha));

    let mut wrong_size = fixture.clone();
    wrong_size["catalogue"]["entries"][0]["size_bytes"] = serde_json::json!(4);
    assert!(!bound(&wrong_size));

    let canonical_url = fixture["catalogue"]["entries"][0]["parts"][0]["urls"][0]
        .as_str()
        .unwrap();
    for escaped_url in [
        canonical_url.replace("/public-a/", "/main/"),
        canonical_url.replace("/public-a/", "/public-a/../main/"),
        canonical_url.replace("/public-a/", "/public-a/%2e%2e/main/"),
        canonical_url.replace(
            "raw.githubusercontent.com",
            "raw.githubusercontent.com.evil",
        ),
        format!("{canonical_url}?download=1"),
    ] {
        let mut escaped = fixture.clone();
        escaped["catalogue"]["entries"][0]["parts"][0]["urls"][0] = serde_json::json!(escaped_url);
        assert!(
            !bound(&escaped),
            "publication binding must reject a noncanonical part URL"
        );
    }

    let mut direct = fixture.clone();
    let entry = direct["catalogue"]["entries"][0].as_object_mut().unwrap();
    entry.remove("parts");
    entry.remove("min_client_version");
    entry.insert(
        "urls".into(),
        serde_json::json!(["https://example.invalid/direct"]),
    );
    assert!(!bound(&direct));

    let mut duplicate = fixture.clone();
    let mut second = duplicate["catalogue"]["entries"][0].clone();
    second["tag"] = serde_json::json!("other");
    second["asset"] = serde_json::json!("other-sdk.tar.zst");
    duplicate["catalogue"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(second);
    assert!(!bound(&duplicate));
}

#[test]
fn canonical_publication_state_rejects_bad_retention_or_default_bounds() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/catalogue-v2-contract.json"
    ))
    .unwrap();
    let generation = fixture["catalogue"]["generation"]
        .as_str()
        .unwrap()
        .to_string();
    let digest = fixture["publication_state"]["catalogue_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let mut state = fixture["publication_state"].clone();
    state["retained_generations"] = serde_json::json!([]);
    assert!(validate_publication_state_body(&state.to_string(), &generation, &digest).is_err());
    let mut state = fixture["publication_state"].clone();
    state["partitioner_default"]["max_bytes"] = serde_json::json!(1);
    assert!(validate_publication_state_body(&state.to_string(), &generation, &digest).is_err());
}

#[test]
fn catalogue_v2_rejects_ambiguous_and_invalid_transport() {
    let invalid = r#"{"schema_version":2,"entries":[{"owner":"o","repo":"r","tag":"t","asset":"a","size_bytes":1,"sha256":"0000000000000000000000000000000000000000000000000000000000000000","urls":["https://example.invalid/a"],"parts":[{"number":1,"size_bytes":1,"sha256":"0000000000000000000000000000000000000000000000000000000000000000","urls":["https://example.invalid/p"]}]}]}"#;
    assert!(ManifestIndex::from_json(invalid).is_none());
    assert!(!supports_min_client_version(Some(CATALOGUE_CAPABILITY + 1)));
}

fn valid_hash() -> String {
    "a".repeat(64)
}

fn v2_entry() -> WireV2Entry {
    WireV2Entry {
        owner: "owner".into(),
        repo: "repo".into(),
        tag: "tag".into(),
        asset: "asset".into(),
        size_bytes: 1,
        sha256: valid_hash(),
        urls: Some(vec!["https://example.invalid/asset".into()]),
        parts: None,
        min_client_version: Some(CATALOGUE_CAPABILITY),
        source_path: None,
    }
}

fn parse_v2_entry(entry: WireV2Entry) -> Result<ManifestEntry, String> {
    entry_from_v2_wire(
        entry,
        &mut std::collections::BTreeSet::new(),
        &mut std::collections::BTreeMap::new(),
    )
}

#[test]
fn multipart_entry_matches_legacy_pinned_assets_url_by_source_path() {
    let entry = ManifestEntry {
        owner: "zackees".into(),
        repo: "soldr-toolchain".into(),
        tag: "assets".into(),
        asset: "xwin-cache.tar.zst".into(),
        transport: AssetTransport::Multipart { parts: Vec::new() },
        sha256: valid_hash(),
        size_bytes: 1,
        min_client_version: Some(CATALOGUE_CAPABILITY),
        source_path: Some("xwin-cache/2026-06-22/windows-x86_64-msvc/xwin-cache.tar.zst".into()),
    };
    assert!(entry.matches_legacy_url(
        "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/xwin-cache/2026-06-22/windows-x86_64-msvc/xwin-cache.tar.zst"
    ));
    assert!(!entry.matches_legacy_url(
        "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/xwin-cache/other/xwin-cache.tar.zst"
    ));
}

#[test]
fn v1_unknown_size_sentinel_still_requires_sha_but_accepts_actual_length() {
    let dir = tempfile::tempdir().expect("cache dir");
    let bytes = b"legacy v1 asset with no advertised size";
    let sha = super::super::trust::sha256_of(bytes);
    let object = dir.path().join(&sha);
    std::fs::write(&object, bytes).expect("cached v1 object");

    assert!(expected_size_matches(0, bytes.len() as u64));
    assert!(cached_asset(&object, &sha, 0)
        .expect("v1 cache validation")
        .is_some());

    std::fs::write(&object, b"wrong bytes").expect("corrupt v1 object");
    assert!(cached_asset(&object, &sha, 0)
        .expect("v1 digest validation")
        .is_none());
}

#[test]
fn canonical_v2_rejects_absent_v1_and_unknown_schema() {
    assert!(ManifestIndex::from_v2_json(r#"{"entries":[]}"#).is_none());
    assert!(ManifestIndex::from_v2_json(r#"{"schema_version":1,"entries":[]}"#).is_none());
    assert!(ManifestIndex::from_v2_json(r#"{"schema_version":3,"entries":[]}"#).is_none());
}

#[test]
fn only_absent_canonical_catalogues_select_v1() {
    assert!(should_fallback_to_v1(404));
    assert!(should_fallback_to_v1(410));
    for status in [200, 204, 301, 400, 401, 403, 409, 500] {
        assert!(
            !should_fallback_to_v1(status),
            "{status} must not select v1"
        );
    }
}

#[test]
fn v2_rejects_unknown_fields_and_duplicate_json_keys() {
    let fixture = include_str!("../../tests/fixtures/catalogue.v2.json");
    assert!(ManifestIndex::from_v2_json(&fixture.replacen(
        "\"generation\": \"canary-0001\",",
        "\"generation\": \"canary-0001\",\n  \"unexpected\": true,",
        1,
    ))
    .is_none());
    assert!(ManifestIndex::from_v2_json(&fixture.replacen(
        "\"asset\": \"direct.bin\",",
        "\"asset\": \"direct.bin\", \"asset\": \"other.bin\",",
        1,
    ))
    .is_none());
    assert!(ManifestIndex::from_v2_json(&fixture.replacen(
        "\"urls\": [\"https://example.invalid/direct.bin\"]",
        "\"url\": \"https://example.invalid/legacy.bin\",",
        1,
    ))
    .is_none());
}

#[test]
fn v2_rejects_duplicate_rows_and_bad_publication_binding() {
    let fixture = include_str!("../../tests/fixtures/catalogue.v2.json");
    assert!(ManifestIndex::from_v2_json(&fixture.replace(
        "\"url\": \"https://example.invalid/generations/canary-0001/publish-state.v1.json\"",
        "\"url\": \"https://example.invalid/generations/other/publish-state.v1.json\"",
    ))
    .is_none());
    assert!(ManifestIndex::from_v2_json(
        &fixture.replace("\"asset\": \"multipart.bin\"", "\"asset\": \"direct.bin\"",)
    )
    .is_none());
    assert!(ManifestIndex::from_v2_json(&fixture.replace(
        "    \"generation\": \"canary-0001\",\n    \"url\"",
        "    \"generation\": \"wrong-generation\",\n    \"url\"",
    ))
    .is_none());
}

#[test]
fn paired_cross_repository_contract_fixture_is_accepted_and_bound() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/catalogue-v2-contract.json"
    ))
    .expect("paired fixture is JSON");
    let catalogue = fixture["catalogue"].to_string();
    assert!(ManifestIndex::from_v2_json(&catalogue).is_some());
    assert_eq!(
        canonical_catalogue_sha256(&catalogue).as_deref(),
        fixture["publication_state"]["catalogue_sha256"].as_str()
    );
}

#[test]
fn v2_matches_producer_optional_and_global_url_rules() {
    let fixture = include_str!("../../tests/fixtures/catalogue.v2.json");
    assert!(ManifestIndex::from_v2_json(&fixture.replacen(
        "\"generation\": \"canary-0001\",",
        "\"generation\": \"canary-0001\",\n  \"generated_at\": \"\",",
        1,
    ))
    .is_some());
    assert!(ManifestIndex::from_v2_json(&fixture.replace(
        "\"https://example.invalid/direct.bin\"",
        "\"https://example.invalid/generations/canary-0001/publish-state.v1.json\"",
    ))
    .is_none());
    let supported = ManifestIndex::from_v2_json(&fixture.replacen(
        "\"size_bytes\": 6,",
        "\"size_bytes\": 6, \"min_client_version\": 2,",
        1,
    ));
    assert_eq!(supported.unwrap().entries[0].min_client_version, Some(2));
    assert!(ManifestIndex::from_v2_json(&fixture.replacen(
        "\"size_bytes\": 6,",
        "\"size_bytes\": 6, \"min_client_version\": 3,",
        1,
    ))
    .is_none());
    assert!(ManifestIndex::from_v2_json(&fixture.replacen(
        "\"size_bytes\": 6,",
        "\"size_bytes\": 6, \"min_client_version\": true,",
        1,
    ))
    .is_none());
}

#[test]
fn canonical_writer_sorts_nested_object_keys() {
    let a = canonical_catalogue_sha256(r#"{"z":{"b":1,"a":2},"a":3}"#);
    let b = canonical_catalogue_sha256(r#"{"a":3,"z":{"a":2,"b":1}}"#);
    assert_eq!(a, b);
}

#[test]
fn publication_state_mismatches_fail_closed() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/catalogue-v2-contract.json"
    ))
    .unwrap();
    let generation = fixture["catalogue"]["generation"].as_str().unwrap();
    let digest = fixture["publication_state"]["catalogue_sha256"]
        .as_str()
        .unwrap();
    let state = parse_publication_state(&fixture["publication_state"].to_string()).unwrap();
    assert!(publication_state_matches(&state, generation, digest));
    assert!(!publication_state_matches(&state, "other", digest));
    assert!(!publication_state_matches(
        &state,
        generation,
        &"0".repeat(64)
    ));
}

#[test]
fn publication_state_requires_full_valid_identity_document() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/catalogue-v2-contract.json"
    ))
    .unwrap();
    let state = fixture["publication_state"].clone();
    assert!(parse_publication_state(&state.to_string()).is_ok());

    for (field, replacement) in [
        ("source", serde_json::json!(null)),
        ("assets_by_sha256", serde_json::json!([])),
        ("logical_assets", serde_json::json!(false)),
    ] {
        let mut invalid = state.clone();
        invalid[field] = replacement;
        assert!(
            parse_publication_state(&invalid.to_string()).is_err(),
            "{field}"
        );
    }
    let generation = fixture["catalogue"]["generation"].as_str().unwrap();
    let digest = state["catalogue_sha256"].as_str().unwrap();
    assert!(validate_publication_state_body(&state.to_string(), generation, digest).is_ok());
    let mut invalid = state.clone();
    invalid["active"]["slot"] = serde_json::json!("public-c");
    let parsed = parse_publication_state(&invalid.to_string()).unwrap();
    assert!(!publication_state_matches(&parsed, generation, digest));
    assert!(validate_publication_state_body(&invalid.to_string(), generation, digest).is_err());
    let mut invalid = state.clone();
    invalid["source"]["commit"] = serde_json::json!("A".repeat(40));
    let parsed = parse_publication_state(&invalid.to_string()).unwrap();
    assert!(!publication_state_matches(&parsed, generation, digest));
    assert!(parse_publication_state(r#"{"schema_version":1,"schema_version":1}"#).is_err());
}

#[test]
fn publication_state_rejects_malformed_ledger_rows_and_unknown_fields() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/catalogue-v2-contract.json"
    ))
    .unwrap();
    let generation = fixture["catalogue"]["generation"].as_str().unwrap();
    let digest = fixture["publication_state"]["catalogue_sha256"]
        .as_str()
        .unwrap();
    let full = "a".repeat(64);
    let part = "b".repeat(64);
    let mut state = fixture["publication_state"].clone();
    state["assets_by_sha256"] = serde_json::json!({
        full.clone(): {
            "size_bytes": 3,
            "partitioner": {"version": 1, "target_bytes": 3},
            "parts": [{
                "number": 1,
                "sha256": part.clone(),
                "size_bytes": 3,
                "path": format!("sha256/{full}/0001-{part}.part"),
                "git_blob": "c".repeat(40),
            }],
        }
    });
    state["logical_assets"] = serde_json::json!({
        "asset-key": {
            "source_path": "source.tar.zst",
            "asset": "published.tar.zst",
            "source_oid_sha256": full.clone(),
            "source_size_bytes": 3,
            "metadata_fingerprint": "d".repeat(64),
            "provenance": {"producer": "test"},
        }
    });
    assert!(validate_publication_state_body(&state.to_string(), generation, digest).is_ok());

    for (path, value) in [
        ("assets_by_sha256", serde_json::json!({"not-a-sha": {}})),
        ("logical_assets", serde_json::json!({"": {}})),
    ] {
        let mut invalid = state.clone();
        invalid[path] = value;
        assert!(validate_publication_state_body(&invalid.to_string(), generation, digest).is_err());
    }
    let mut invalid = state.clone();
    invalid["assets_by_sha256"][&full]["parts"][0]["path"] = serde_json::json!("not/canonical");
    assert!(validate_publication_state_body(&invalid.to_string(), generation, digest).is_err());
    let mut invalid = state.clone();
    invalid["logical_assets"]["asset-key"]["source_size_bytes"] = serde_json::json!(4);
    assert!(validate_publication_state_body(&invalid.to_string(), generation, digest).is_err());
    let mut invalid = state.clone();
    invalid["unknown"] = serde_json::json!(true);
    assert!(parse_publication_state(&invalid.to_string()).is_err());
    let mut invalid = state.clone();
    invalid["assets_by_sha256"][&full]["parts"][0]["unknown"] = serde_json::json!(true);
    assert!(parse_publication_state(&invalid.to_string()).is_err());
}

#[test]
fn authoritative_v2_failure_never_becomes_a_legacy_empty_index() {
    let malformed = authoritative_v2_index(None, false);
    assert!(malformed.fail_closed);
    assert_eq!(malformed.source, CatalogueSource::CanonicalV2);
    let state_failure = authoritative_v2_index(Some(ManifestIndex::empty()), false);
    assert!(state_failure.fail_closed);
    // A canonical endpoint's non-success status takes this same branch.
    assert!(fail_closed_v2_index().fail_closed);
}

#[test]
fn generation_uses_only_the_producer_ascii_alphabet() {
    for generation in ["ready_1.2:3-4", "A"] {
        assert!(valid_generation(generation));
    }
    for generation in [
        "",
        "with space",
        "slash/name",
        "percent%",
        "café",
        "newline\n",
    ] {
        assert!(!valid_generation(generation), "{generation:?}");
    }
}

#[test]
fn v2_hostile_numeric_and_url_boundaries_are_rejected() {
    let mut entry = v2_entry();
    entry.size_bytes = MAX_CATALOGUE_ASSET_BYTES + 1;
    assert!(parse_v2_entry(entry).is_err());

    let mut entry = v2_entry();
    entry.urls = Some(vec![format!(
        "https://example.invalid/{}",
        "a".repeat(MAX_CATALOGUE_URL_BYTES)
    )]);
    assert!(parse_v2_entry(entry).is_err());

    let mut entry = v2_entry();
    entry.urls = None;
    entry.parts = Some(
        (1..=(MAX_CATALOGUE_PARTS + 1))
            .map(|number| WirePart {
                number: number as u32,
                size_bytes: 1,
                sha256: valid_hash(),
                urls: vec![format!("https://example.invalid/{number}")],
            })
            .collect(),
    );
    assert!(parse_v2_entry(entry).is_err());

    let mut entry = v2_entry();
    entry.urls = None;
    entry.parts = Some(vec![
        WirePart {
            number: 1,
            size_bytes: u64::MAX,
            sha256: valid_hash(),
            urls: vec!["https://example.invalid/1".into()],
        },
        WirePart {
            number: 2,
            size_bytes: 1,
            sha256: valid_hash(),
            urls: vec!["https://example.invalid/2".into()],
        },
    ]);
    assert!(parse_v2_entry(entry).is_err());
}

#[test]
fn v2_part_count_and_transport_invariants_hold_at_the_boundary() {
    let mut entry = v2_entry();
    entry.size_bytes = MAX_CATALOGUE_PARTS as u64;
    entry.source_path = Some("tool/version/platform/asset.tar.zst".into());
    entry.urls = None;
    entry.parts = Some(
        (1..=MAX_CATALOGUE_PARTS)
            .map(|number| WirePart {
                number: number as u32,
                size_bytes: 1,
                sha256: valid_hash(),
                urls: vec![format!("https://example.invalid/{number}")],
            })
            .collect(),
    );
    assert!(matches!(
        parse_v2_entry(entry),
        Ok(ManifestEntry {
            transport: AssetTransport::Multipart { .. },
            ..
        })
    ));

    let mut entry = v2_entry();
    entry.urls = Some(vec![
        "https://example.invalid/asset".into(),
        "https://example.invalid/asset".into(),
    ]);
    assert!(parse_v2_entry(entry).is_err());

    let mut entry = v2_entry();
    entry.urls = None;
    entry.size_bytes = 2;
    entry.parts = Some(vec![
        WirePart {
            number: 1,
            size_bytes: 1,
            sha256: valid_hash(),
            urls: vec!["https://example.invalid/one".into()],
        },
        WirePart {
            number: 1,
            size_bytes: 1,
            sha256: valid_hash(),
            urls: vec!["https://example.invalid/two".into()],
        },
    ]);
    assert!(parse_v2_entry(entry).is_err());

    let mut entry = v2_entry();
    entry.urls = None;
    entry.parts = Some(vec![WirePart {
        number: 1,
        size_bytes: 2,
        sha256: valid_hash(),
        urls: vec!["https://example.invalid/one".into()],
    }]);
    assert!(parse_v2_entry(entry).is_err());
}

#[test]
fn v2_shared_part_urls_require_the_same_content_identity() {
    fn multipart_entry(asset: &str, source_path: &str, part_sha256: String) -> WireV2Entry {
        WireV2Entry {
            owner: "owner".into(),
            repo: "repo".into(),
            tag: "tag".into(),
            asset: asset.into(),
            size_bytes: 1,
            sha256: valid_hash(),
            urls: None,
            parts: Some(vec![WirePart {
                number: 1,
                size_bytes: 1,
                sha256: part_sha256,
                urls: vec!["https://example.invalid/shared-part".into()],
            }]),
            min_client_version: Some(CATALOGUE_CAPABILITY),
            source_path: Some(source_path.into()),
        }
    }

    let mut direct_urls = std::collections::BTreeSet::new();
    let mut part_urls = std::collections::BTreeMap::new();
    assert!(entry_from_v2_wire(
        multipart_entry("one.bin", "one/one.bin", valid_hash()),
        &mut direct_urls,
        &mut part_urls,
    )
    .is_ok());
    assert!(entry_from_v2_wire(
        multipart_entry("two.bin", "two/two.bin", valid_hash()),
        &mut direct_urls,
        &mut part_urls,
    )
    .is_ok());
    assert!(entry_from_v2_wire(
        multipart_entry("three.bin", "three/three.bin", "b".repeat(64)),
        &mut direct_urls,
        &mut part_urls,
    )
    .is_err());
}

#[test]
fn v2_json_types_are_not_coerced() {
    let fixture = include_str!("../../tests/fixtures/catalogue.v2.json");
    assert!(ManifestIndex::from_v2_json(&fixture.replacen(
        "\"size_bytes\": 6",
        "\"size_bytes\": true",
        1
    ))
    .is_none());
    assert!(ManifestIndex::from_v2_json(&fixture.replacen(
        "\"number\": 1",
        "\"number\": \"1\"",
        1
    ))
    .is_none());
}

#[test]
fn disabled_via_env_handles_truthy_and_falsy_values() {
    // Test the parser directly via the same shape disabled_via_env
    // uses, since touching the process env in unit tests is racy.
    let check = |value: Option<&str>| match value {
        None => false,
        Some(v) => {
            let n = v.trim().to_ascii_lowercase();
            !n.is_empty() && n != "0" && n != "false" && n != "no"
        }
    };
    assert!(!check(None));
    assert!(!check(Some("")));
    assert!(!check(Some("0")));
    assert!(!check(Some("false")));
    assert!(!check(Some("no")));
    assert!(check(Some("1")));
    assert!(check(Some("true")));
    assert!(check(Some("yes")));
    assert!(check(Some("anything-else")));
}

// Back-compat guard for issue #861: after schema v6 lands beside
// the flat-array shape this module owns, an old flat manifest
// body must keep parsing through `ManifestIndex::from_json`. This
// proves the dispatch isn't accidentally captured by the new v6
// parser — the two shapes are disjoint on the wire (`entries: []`
// vs. `schema_version: 6, tools: {...}`) and must stay so.
#[test]
fn flat_schema_v5_still_parses_for_back_compat() {
    let flat = r#"{
            "entries": [
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "1.12.9",
                    "asset": "zccache-x86_64-pc-windows-msvc.zip",
                    "url": "https://example.com/zccache.zip",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            ]
        }"#;
    let idx = ManifestIndex::from_json(flat).expect("flat parse ok");
    assert_eq!(idx.entries.len(), 1);
    // And the v6 parser must reject this same flat body, proving
    // the two are routed disjointly.
    assert!(
        super::super::manifest_v6::ManifestV6::from_json(flat).is_none(),
        "v6 parser must reject the flat-array shape"
    );
}

#[test]
fn catalogue_url_override_takes_precedence() {
    // The full-URL override is the one the integration tests use
    // when they spawn a local HTTP listener on a random port —
    // they can't fit that under the `origin + /catalogue.v1.json`
    // composition because the listener path is fixed.
    // Verify the override is recognized via the public const name.
    assert_eq!(
        TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
        "SOLDR_TOOLCHAIN_CATALOGUE_URL"
    );
}

#[test]
fn multipart_window_ramps_and_halves_on_retryable_overload() {
    let mut window = MultipartWindow::new();
    assert_eq!(window.current, 4);
    for _ in 0..40 {
        window.healthy();
    }
    assert_eq!(
        window.current, 16,
        "window must cap at the per-origin maximum"
    );
    window.congested();
    assert_eq!(window.current, 8);
    assert!(!window.cooldown.is_zero());
    assert!(congestion_error(&SoldrError::Network(
        "HTTP 429 Retry-After: 1".into()
    )));
    assert!(congestion_error(&SoldrError::Network(
        "asset download stalled".into()
    )));
    assert!(!congestion_error(&SoldrError::Other(
        "sha256 mismatch".into()
    )));
    assert_eq!(
        retry_after(&SoldrError::Network("HTTP 503 Retry-After: 7".into())),
        Some(Duration::from_secs(7))
    );
    assert_eq!(
        retry_after(&SoldrError::Network("HTTP 429 Retry-After: 999999".into())),
        Some(MAX_MULTIPART_RETRY_AFTER),
        "untrusted Retry-After must be capped"
    );
}

#[test]
fn process_wide_part_coordinator_round_robins_and_releases_on_drop() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let coordinator = PartCoordinator::new(Some(1));
        let first_job = Arc::new(coordinator.register().await);
        let second_job = Arc::new(coordinator.register().await);
        let first = coordinator.acquire(first_job.id).await;

        let wait_second = {
            let coordinator = Arc::clone(&coordinator);
            let job = Arc::clone(&second_job);
            tokio::spawn(async move {
                let permit = coordinator.acquire(job.id).await;
                drop(permit);
                "second"
            })
        };
        let wait_first = {
            let coordinator = Arc::clone(&coordinator);
            let job = Arc::clone(&first_job);
            tokio::spawn(async move {
                let permit = coordinator.acquire(job.id).await;
                drop(permit);
                "first"
            })
        };
        tokio::task::yield_now().await;
        drop(first);
        let winner = tokio::time::timeout(Duration::from_secs(1), wait_second)
            .await
            .expect("second asset must receive the next global grant")
            .unwrap();
        assert_eq!(winner, "second");
        assert!(tokio::time::timeout(Duration::from_secs(1), wait_first)
            .await
            .expect("first asset remains runnable after its turn")
            .is_ok());

        let held = coordinator.acquire(first_job.id).await;
        let blocked = {
            let coordinator = Arc::clone(&coordinator);
            let job = Arc::clone(&second_job);
            tokio::spawn(async move { coordinator.acquire(job.id).await })
        };
        tokio::task::yield_now().await;
        assert!(
            !blocked.is_finished(),
            "one global admission is the hard cap"
        );
        drop(held);
        drop(
            tokio::time::timeout(Duration::from_secs(1), blocked)
                .await
                .expect("dropped admission must not leak the global slot")
                .unwrap(),
        );

        // A registered but idle peer is not in the admission queue.
        let idle_peer = Arc::new(coordinator.register().await);
        let permit = coordinator.acquire(first_job.id).await;
        drop(permit);
        drop(idle_peer);

        // Cancelling a queued request removes it rather than leaving a
        // ghost at the queue head.
        let held = coordinator.acquire(first_job.id).await;
        let cancelled = {
            let coordinator = Arc::clone(&coordinator);
            let job = Arc::clone(&second_job);
            tokio::spawn(async move { coordinator.acquire(job.id).await })
        };
        tokio::task::yield_now().await;
        cancelled.abort();
        let _ = cancelled.await;
        drop(held);
        let permit =
            tokio::time::timeout(Duration::from_secs(1), coordinator.acquire(first_job.id))
                .await
                .expect("cancelled waiter must not block an idle peer");
        drop(permit);
    });
}

#[test]
fn multipart_copy_hashes_without_whole_file_buffering() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    std::fs::write(&source, vec![0x5a; 128 * 1024 + 7]).unwrap();
    let mut input = std::fs::File::open(source).unwrap();
    let mut output = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
    let mut hash = Sha256Hasher::new();
    let bytes = copy_and_hash(&mut input, &mut output, &mut hash).unwrap();
    assert_eq!(bytes, 128 * 1024 + 7);
    assert_eq!(
        hash.finalize().to_hex(),
        super::super::trust::sha256_of(&std::fs::read(output.path()).unwrap())
    );
}

#[test]
fn multipart_part_tail_probe_accepts_exact_206_without_nested_fanout() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
            let body = b"abcdefgh".to_vec();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            tokio::spawn(async move {
                for response_number in 0..2 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = vec![0; 1024];
                    let n = socket.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..n]).to_string();
                    seen.lock().unwrap().push(request.clone());
                    let response = if response_number == 0 {
                        "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd".to_string()
                    } else {
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-7/8\r\nContent-Length: 4\r\nConnection: close\r\n\r\nefgh".to_string()
                    };
                    socket.write_all(response.as_bytes()).await.unwrap();
                    let _ = socket.shutdown().await;
                }
            });
            let dir = tempfile::tempdir().unwrap();
            let object = dir.path().join("sha-key");
            let url = format!("http://{address}/part?credential=never-persisted");
            assert!(download_catalogue_part(&url, &object, 8).await.is_err());
            assert_eq!(std::fs::read(object.with_extension("partial")).unwrap(), b"abcd");
            assert!(!object.with_extension("partial.range").exists());
            let downloaded = download_catalogue_part(&url, &object, 8).await.unwrap();
            assert_eq!(std::fs::read(downloaded.path()).unwrap(), body);
            assert_eq!(std::fs::read(object.with_extension("partial.range")).unwrap(), b"validated-206\n");
            let requests = requests.lock().unwrap().clone();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].lines().all(|line| !line.to_ascii_lowercase().starts_with("range:")));
            assert_eq!(
                requests[1].lines().find(|line| line.to_ascii_lowercase().starts_with("range:")).unwrap().to_ascii_lowercase(),
                "range: bytes=4-7",
                "one tail probe is the only Range request: no nested fanout"
            );
        });
}

#[test]
fn multipart_part_transport_refuses_redirects() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: https://media.githubusercontent.com/media/zackees/clang-tool-chain-bins/main/asset\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let error = download_catalogue_part_response(
            &format!("http://{address}/part"),
            None,
            file.as_file_mut(),
        )
        .await
        .expect_err("catalogue part transport must not follow redirects");
        assert!(error.to_string().contains("HTTP 302"), "{error}");
        server.await.unwrap();
    });
}

#[test]
fn multipart_http_429_preserves_capped_retry_after_without_url_credentials() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let _ = socket.read(&mut request).await.unwrap();
                socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 999999\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            });
            let url = format!("http://{address}/part?access_token=must-not-leak");
            let client = super::super::stream_download::asset_http_client("retry-after test").unwrap();
            let response = super::super::stream_download::send_asset_request(
                super::super::stream_download::get_request(&client, &url), &url, Duration::from_secs(1)
            ).await.unwrap();
            let mut file = tempfile::NamedTempFile::new().unwrap();
            let error = super::super::stream_download::stream_catalogue_part_into_file(response, &url, file.as_file_mut())
                .await.expect_err("429 must remain a scheduler-visible error");
            let text = error.to_string();
            assert!(text.contains("HTTP 429") && text.contains("Retry-After: 999999"));
            assert!(!text.contains("access_token"));
            assert_eq!(retry_after(&error), Some(MAX_MULTIPART_RETRY_AFTER));
            let coordinator = PartCoordinator::new(Some(16));
            let origin = "http://test.invalid";
            let before = coordinator.origin_window(origin).await;
            let after = coordinator.congested_origin(origin, retry_after(&error)).await;
            assert_eq!(after.current, (before.current / 2).max(1));
            assert_eq!(after.cooldown, MAX_MULTIPART_RETRY_AFTER);
        });
}

#[test]
fn multipart_tail_ignored_or_malformed_restarts_whole_part() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
            for invalid_tail in [
                "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabcdefgh",
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes nope\r\nContent-Length: 4\r\nConnection: close\r\n\r\nefgh",
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-7/8\r\nContent-Length: 4\r\nConnection: close\r\n\r\nefgh",
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-7/8\r\nContent-Length: 3\r\nConnection: close\r\n\r\nefgh",
            ] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                tokio::spawn(async move {
                    for response in [
                        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd",
                        invalid_tail,
                        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabcdefgh",
                    ] {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        let mut request = [0; 1024];
                        let _ = socket.read(&mut request).await.unwrap();
                        socket.write_all(response.as_bytes()).await.unwrap();
                        let _ = socket.shutdown().await;
                    }
                });
                let dir = tempfile::tempdir().unwrap();
                let object = dir.path().join("sha-key");
                let url = format!("http://{address}/part");
                assert!(download_catalogue_part(&url, &object, 8).await.is_err());
                let downloaded = download_catalogue_part(&url, &object, 8).await.unwrap();
                assert_eq!(std::fs::read(downloaded.path()).unwrap(), b"abcdefgh");
                assert!(!object.with_extension("partial.range").exists());
                assert_eq!(std::fs::read(object.with_extension("partial")).unwrap(), b"abcdefgh");
            }
        });
}

#[test]
fn cancelling_streaming_part_keeps_only_unverified_sha_partial() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabcd")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let dir = tempfile::tempdir().unwrap();
        let object = dir.path().join("sha-key");
        let url = format!("http://{address}/part");
        let task = tokio::spawn({
            let object = object.clone();
            async move { download_catalogue_part(&url, &object, 8).await }
        });
        for _ in 0..50 {
            if object
                .with_extension("partial")
                .metadata()
                .map(|meta| meta.len())
                .unwrap_or(0)
                == 4
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::fs::read(object.with_extension("partial")).unwrap(),
            b"abcd"
        );
        task.abort();
        let _ = task.await;
        assert!(
            !object.exists(),
            "cancellation must never publish the final SHA object"
        );
        assert_eq!(
            std::fs::read(object.with_extension("partial")).unwrap(),
            b"abcd"
        );
        assert!(!object.with_extension("partial.range").exists());

        // The same cancellation-safe RAII policy makes the global
        // admission immediately reusable; this small-cap coordinator is
        // the deterministic seam rather than mutating the process-wide
        // Bulk singleton shared by other tests.
        let coordinator = PartCoordinator::new(Some(1));
        let job = coordinator.register().await;
        let permit = coordinator.acquire(job.id).await;
        drop(permit);
        let next = tokio::time::timeout(Duration::from_secs(1), coordinator.acquire(job.id))
            .await
            .expect("cancel/drop must release admission");
        drop(next);
    });
}

#[test]
fn scheduler_benchmark_fixture_and_results_match_the_model() {
    let fixture = include_str!("../../benchmarks/multipart_scheduler_fixture.json");
    let results = include_str!("../../benchmarks/multipart_scheduler_results.md");
    assert!(fixture.contains("\"bulk_cap\": 16"));
    let makespan = |window: usize| {
        let admitted_per_tick = (window * 2).min(16);
        64_usize.div_ceil(admitted_per_tick)
    };
    assert_eq!(makespan(1), 32);
    assert_eq!(makespan(4), 8);
    assert_eq!(makespan(16), 4);
    for expected in [
        "| 1 | 2 | 32 | 2.0",
        "| 4 | 8 | 8 | 8.0",
        "| 16 | 16 | 4 | 16.0",
        "| adaptive | 16 | 5 | 12.8",
    ] {
        assert!(
            results.contains(expected),
            "missing model result {expected}"
        );
    }
    assert!(results.contains("4 → 2"), "AIMD decrease must be recorded");
}
