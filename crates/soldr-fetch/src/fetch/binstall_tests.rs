//! Hermetic tests for the `[package.metadata.binstall]` hop (soldr#3726): a
//! loopback HTTP fixture serves a `.crate` whose only prebuilt is described
//! by its binstall `pkg-url` template.

use super::*;
use crate::fetch::binstall_template::{self, PkgFmt, TemplateVars};
use std::collections::HashMap;
use std::io::Write as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CRATE: &str = "fixture-bin";
const VERSION: &str = "1.2.3";

fn tar_of(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        for (name, payload, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *payload)
                .expect("append");
        }
        builder.finish().expect("finish");
    }
    tar_bytes
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(bytes).expect("gzip");
    gz.finish().expect("gzip finish")
}

fn crate_file(cargo_toml: &str) -> Vec<u8> {
    let path = format!("{CRATE}-{VERSION}/Cargo.toml");
    gzip(&tar_of(&[(&path, cargo_toml.as_bytes(), 0o644)]))
}

/// Serve `routes` (HEAD + GET); every other path is 404.
fn serve(listener: TcpListener, routes: HashMap<String, Vec<u8>>) {
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let response = match routes.get(&path) {
                    Some(body) => {
                        let mut head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        if method == "GET" {
                            head.extend_from_slice(body);
                        }
                        head
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
}

struct Fixture {
    _root: tempfile::TempDir,
    paths: SoldrPaths,
    target: TargetTriple,
    crates_base: String,
    asset_name: String,
    sha256: String,
}

/// `pkg_url` may use `@HOST@` for the loopback origin.
async fn fixture(pkg_url: &str, pkg_fmt: Option<&str>, asset_suffix: &str, zstd: bool) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());
    let target = TargetTriple::host().expect("host triple");
    let binary = archive::desired_binary_names(&[CRATE], &target)
        .into_iter()
        .next()
        .expect("binary name");
    let payload = b"#!/bin/sh\necho fixture-bin 1.2.3\n";
    let bin_entry = format!("{CRATE}-{VERSION}/bin/{binary}");
    let tar = tar_of(&[(&bin_entry, payload, 0o755)]);
    let body = if zstd {
        zstd::stream::encode_all(&tar[..], 3).expect("zstd")
    } else {
        gzip(&tar)
    };
    let sha256 = trust::sha256_of(&body);
    let asset_name = format!("{CRATE}-v{VERSION}-{}{asset_suffix}", target.triple());
    let fmt_line = pkg_fmt
        .map(|fmt| format!("pkg-fmt = \"{fmt}\"\n"))
        .unwrap_or_default();
    let cargo_toml = format!(
        "[package]\nname = \"{CRATE}\"\nversion = \"{VERSION}\"\nrepository = \"https://github.com/acme/{CRATE}\"\n\n[package.metadata.binstall]\npkg-url = \"{}\"\n{fmt_line}bin-dir = \"{{ name }}-{{ version }}/bin/{{ bin }}{{ binary-ext }}\"\n",
        pkg_url.replace("@HOST@", &origin)
    );
    let mut routes = HashMap::new();
    routes.insert(
        format!("/crates/{CRATE}/{CRATE}-{VERSION}.crate"),
        crate_file(&cargo_toml),
    );
    routes.insert(format!("/dl/v{VERSION}/{asset_name}"), body);
    serve(listener, routes);
    Fixture {
        _root: root,
        paths,
        target,
        crates_base: format!("{origin}/crates"),
        asset_name,
        sha256,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

const TGZ_URL: &str = "@HOST@/dl/v{ version }/{ name }-v{ version }-{ target }.{ archive-format }";

async fn resolve(
    fx: &Fixture,
    store: &trust::PinnedChecksumStore,
    mode: trust::TrustMode,
    allow_loopback_http: bool,
) -> Result<Option<FetchResult>, SoldrError> {
    try_resolve(
        &fx.paths,
        CRATE,
        &[CRATE],
        VERSION,
        &fx.target,
        &fx.crates_base,
        (store, mode),
        allow_loopback_http,
    )
    .await
}

#[test]
fn crate_described_only_by_binstall_metadata_resolves() {
    runtime().block_on(async {
        let fx = fixture(TGZ_URL, None, ".tgz", false).await;
        let store = trust::PinnedChecksumStore::empty();
        let result = resolve(&fx, &store, trust::TrustMode::Permissive, true)
            .await
            .expect("binstall resolve")
            .expect("binstall-only crate must resolve");
        assert!(result.binary_path.is_file(), "{:?}", result.binary_path);
        assert_eq!(result.version, VERSION);
        assert!(!result.cached);
    });
}

#[test]
fn tzstd_pkg_fmt_resolves() {
    runtime().block_on(async {
        let url = "@HOST@/dl/v{ version }/{ name }-v{ version }-{ target }{ archive-suffix }";
        let fx = fixture(url, Some("tzstd"), ".tar.zst", true).await;
        let store = trust::PinnedChecksumStore::empty();
        let result = resolve(&fx, &store, trust::TrustMode::Permissive, true)
            .await
            .expect("tzstd resolve")
            .expect("tzstd asset must resolve");
        assert!(result.binary_path.is_file());
    });
}

#[test]
fn strict_mode_refuses_unpinned_binstall_asset() {
    runtime().block_on(async {
        let fx = fixture(TGZ_URL, None, ".tgz", false).await;
        let store = trust::PinnedChecksumStore::empty();
        let err = resolve(&fx, &store, trust::TrustMode::Strict, true)
            .await
            .expect_err("strict mode must refuse an unpinned binstall asset");
        assert!(err.to_string().contains("strict"), "{err}");
        assert!(!fx.paths.bin.join(format!("{CRATE}-{VERSION}")).exists());
    });
}

#[test]
fn pin_verifies_and_mismatch_is_fatal() {
    runtime().block_on(async {
        let fx = fixture(TGZ_URL, None, ".tgz", false).await;
        let pin = |sha: &str| {
            trust::PinnedChecksumStore::from_toml(&format!(
                "[[tool]]\ntool = \"{CRATE}\"\nversion = \"{VERSION}\"\nasset = \"{}\"\nsha256 = \"{sha}\"\n",
                fx.asset_name
            ))
            .expect("pin store")
        };
        let err = resolve(&fx, &pin(&"0".repeat(64)), trust::TrustMode::Permissive, true)
            .await
            .expect_err("pin mismatch must be fatal");
        assert!(err.to_string().contains("pinned sha256 mismatch"), "{err}");
        let ok = resolve(&fx, &pin(&fx.sha256), trust::TrustMode::Strict, true)
            .await
            .expect("pinned asset passes strict mode");
        assert!(ok.is_some());
    });
}

#[test]
fn non_https_pkg_url_is_rejected() {
    runtime().block_on(async {
        let fx = fixture(TGZ_URL, None, ".tgz", false).await;
        let store = trust::PinnedChecksumStore::empty();
        // The production policy: no loopback-http allowance. The .crate base
        // is itself http here, so the refusal must name https.
        let err = resolve(&fx, &store, trust::TrustMode::Permissive, false)
            .await
            .expect_err("http must be refused");
        assert!(err.to_string().contains("https"), "{err}");
    });
}

#[test]
fn unknown_template_variable_is_rejected() {
    runtime().block_on(async {
        let fx = fixture("@HOST@/dl/{ nope }.tgz", None, ".tgz", false).await;
        let store = trust::PinnedChecksumStore::empty();
        let err = resolve(&fx, &store, trust::TrustMode::Permissive, true)
            .await
            .expect_err("unknown variable must be refused");
        assert!(err.to_string().contains("nope"), "{err}");
    });
}

#[test]
fn missing_crate_is_a_miss() {
    runtime().block_on(async {
        let fx = fixture(TGZ_URL, None, ".tgz", false).await;
        let store = trust::PinnedChecksumStore::empty();
        let result = try_resolve(
            &fx.paths,
            CRATE,
            &[CRATE],
            "9.9.9",
            &fx.target,
            &fx.crates_base,
            (&store, trust::TrustMode::Permissive),
            true,
        )
        .await
        .expect("miss is not an error");
        assert!(result.is_none());
    });
}

#[test]
fn resolver_order_gates_binstall() {
    assert!(ResolverOrder::all().try_binstall);
    assert!(ResolverOrder::parse("binstall").try_binstall);
    assert!(!ResolverOrder::parse("binstall").try_quickinstall);
    assert!(!ResolverOrder::parse("embed,live,api,quickinstall").try_binstall);
}

#[test]
fn template_renders_documented_variables() {
    let target = TargetTriple::from_triple("x86_64-unknown-linux-musl").expect("triple");
    let vars = TemplateVars {
        name: "tool",
        version: "1.0.0",
        target: &target,
        pkg_fmt: PkgFmt::Txz,
        repo: Some("https://github.com/a/tool"),
        bin: None,
        url: None,
    };
    let out = binstall_template::render(
        "{ repo }/releases/download/v{version}/{ name }-{ target }-{ target-arch }-{ target-libc }.{ archive-format }{ binary-ext }",
        &vars,
    )
    .expect("render");
    assert_eq!(
        out,
        "https://github.com/a/tool/releases/download/v1.0.0/tool-x86_64-unknown-linux-musl-x86_64-musl.txz"
    );
    for bad in ["{ bin }", "{ unknown }", "{ name", "name }", "{}"] {
        assert!(binstall_template::render(bad, &vars).is_err(), "{bad}");
    }
}

#[test]
fn metadata_overrides_and_absence() {
    let target = TargetTriple::from_triple("x86_64-pc-windows-msvc").expect("triple");
    let toml = "[package]\nname = \"t\"\nversion = \"1.0.0\"\n[package.metadata.binstall]\npkg-url = \"https://a/{ name }.tgz\"\n[package.metadata.binstall.overrides.x86_64-pc-windows-msvc]\npkg-url = \"https://w/{ name }.zip\"\npkg-fmt = \"zip\"\n";
    let meta = binstall_template::BinstallMeta::from_cargo_toml(toml, &target).expect("parse");
    assert_eq!(meta.pkg_url.as_deref(), Some("https://w/{ name }.zip"));
    assert_eq!(meta.pkg_fmt, Some(PkgFmt::Zip));
    let none = binstall_template::BinstallMeta::from_cargo_toml(
        "[package]\nname = \"t\"\nversion = \"1.0.0\"\n",
        &target,
    )
    .expect("parse");
    assert!(none.pkg_url.is_none() && none.signing.is_none());
    assert!(PkgFmt::parse("tbz9").is_err());
}

#[test]
fn https_policy_refuses_non_https_pkg_urls() {
    assert!(require_https("https://example.com/a.tgz", "pkg-url", false).is_ok());
    for bad in [
        "http://example.com/a.tgz",
        "ftp://example.com/a.tgz",
        "file:///etc/passwd",
    ] {
        let err = require_https(bad, "pkg-url", true).expect_err(bad);
        assert!(err.to_string().contains("https"), "{err}");
    }
    assert!(require_https("http://127.0.0.1:1/a", "pkg-url", false).is_err());
    assert!(require_https("http://127.0.0.1:1/a", "pkg-url", true).is_ok());
}

// ---- soldr#3735: tbz2, default pkg-url guesses, minisign signing ----

/// Committed minisign fixtures: a TEST keypair generated for soldr#3735
/// (the secret half was never committed; it is not a real publisher key).
const SIGNED_ASSET: &[u8] = include_bytes!("../../tests/fixtures/binstall_minisign/asset.tgz");
const GOOD_SIG: &[u8] = include_bytes!("../../tests/fixtures/binstall_minisign/asset.tgz.sig");
const WRONG_KEY_SIG: &[u8] =
    include_bytes!("../../tests/fixtures/binstall_minisign/asset.tgz.wrongkey.sig");
const TEST_PUBKEY: &str = include_str!("../../tests/fixtures/binstall_minisign/test.pub");

struct Served {
    _root: tempfile::TempDir,
    paths: SoldrPaths,
    target: TargetTriple,
    crates_base: String,
}

/// Serve a `.crate` whose `[package]` table is followed by `extra_toml`
/// (`@HOST@` = loopback origin) plus `routes` (`@TARGET@` = host triple).
async fn serve_crate(extra_toml: &str, routes: Vec<(String, Vec<u8>)>) -> Served {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());
    let target = TargetTriple::host().expect("host triple");
    let cargo_toml = format!(
        "[package]\nname = \"{CRATE}\"\nversion = \"{VERSION}\"\nrepository = \"@HOST@/acme/{CRATE}\"\n\n{extra_toml}"
    )
    .replace("@HOST@", &origin);
    let mut map = HashMap::new();
    map.insert(
        format!("/crates/{CRATE}/{CRATE}-{VERSION}.crate"),
        crate_file(&cargo_toml),
    );
    for (path, body) in routes {
        map.insert(path.replace("@TARGET@", &target.triple()), body);
    }
    serve(listener, map);
    Served {
        _root: root,
        paths,
        target,
        crates_base: format!("{origin}/crates"),
    }
}

async fn resolve_served(
    s: &Served,
    store: &trust::PinnedChecksumStore,
    mode: trust::TrustMode,
) -> Result<Option<FetchResult>, SoldrError> {
    try_resolve(
        &s.paths,
        CRATE,
        &[CRATE],
        VERSION,
        &s.target,
        &s.crates_base,
        (store, mode),
        true,
    )
    .await
}

fn bin_tar() -> Vec<u8> {
    tar_of(&[(
        &format!("{CRATE}-{VERSION}/bin/{CRATE}"),
        b"#!/bin/sh\necho ok\n",
        0o755,
    )])
}

#[test]
fn tbz2_pkg_fmt_resolves() {
    runtime().block_on(async {
        let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        bz.write_all(&bin_tar()).expect("bzip2");
        let body = bz.finish().expect("bzip2 finish");
        let s = serve_crate(
            "[package.metadata.binstall]\npkg-url = \"@HOST@/dl/{ name }-{ target }{ archive-suffix }\"\npkg-fmt = \"tbz2\"\n",
            vec![(format!("/dl/{CRATE}-@TARGET@.tar.bz2"), body)],
        )
        .await;
        let store = trust::PinnedChecksumStore::empty();
        let result = resolve_served(&s, &store, trust::TrustMode::Permissive)
            .await
            .expect("tbz2 resolve")
            .expect("tbz2 asset must resolve");
        assert!(result.binary_path.is_file());
    });
}

#[test]
fn default_pkg_url_guess_resolves_without_pkg_url() {
    runtime().block_on(async {
        // No [package.metadata.binstall] at all: binstall's default guesses
        // against `{ repo }/releases/download/...` must find the asset.
        let mut xz = xz2::write::XzEncoder::new(Vec::new(), 1);
        xz.write_all(&bin_tar()).expect("xz");
        let body = xz.finish().expect("xz finish");
        let s = serve_crate(
            "",
            vec![(
                format!(
                    "/acme/{CRATE}/releases/download/v{VERSION}/{CRATE}-@TARGET@-v{VERSION}.tar.xz"
                ),
                body,
            )],
        )
        .await;
        let store = trust::PinnedChecksumStore::empty();
        let result = resolve_served(&s, &store, trust::TrustMode::Permissive)
            .await
            .expect("guess resolve")
            .expect("a default guess must resolve");
        assert!(result.binary_path.is_file());
    });
}

#[test]
fn default_guess_without_repository_is_a_miss() {
    let target = TargetTriple::from_triple("x86_64-unknown-linux-gnu").expect("triple");
    let meta = binstall_template::BinstallMeta::from_cargo_toml(
        "[package]\nname = \"t\"\nversion = \"1.0.0\"\n",
        &target,
    )
    .expect("parse");
    assert!(meta
        .candidates("t", "1.0.0", &target)
        .expect("render")
        .is_empty());
}

fn signed_routes(sig: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut routes = vec![("/dl/asset.tgz".to_string(), SIGNED_ASSET.to_vec())];
    if !sig.is_empty() {
        routes.push(("/dl/asset.tgz.sig".to_string(), sig.to_vec()));
    }
    routes
}

fn signing_toml(pubkey: &str) -> String {
    format!(
        "[package.metadata.binstall]\npkg-url = \"@HOST@/dl/asset.tgz\"\n[package.metadata.binstall.signing]\nalgorithm = \"minisign\"\npubkey = \"{}\"\n",
        pubkey.trim()
    )
}

#[test]
fn minisign_valid_signature_installs() {
    runtime().block_on(async {
        let s = serve_crate(&signing_toml(TEST_PUBKEY), signed_routes(GOOD_SIG)).await;
        let store = trust::PinnedChecksumStore::empty();
        let result = resolve_served(&s, &store, trust::TrustMode::Permissive)
            .await
            .expect("signed resolve")
            .expect("signed asset must resolve");
        assert!(result.binary_path.is_file());
    });
}

#[test]
fn minisign_signature_is_not_a_pin_under_strict_mode() {
    runtime().block_on(async {
        let s = serve_crate(&signing_toml(TEST_PUBKEY), signed_routes(GOOD_SIG)).await;
        let store = trust::PinnedChecksumStore::empty();
        let err = resolve_served(&s, &store, trust::TrustMode::Strict)
            .await
            .expect_err("a publisher key from crate metadata is not a pin");
        assert!(err.to_string().contains("strict"), "{err}");
    });
}

#[test]
fn minisign_bad_signature_is_fatal_even_when_pinned() {
    runtime().block_on(async {
        let s = serve_crate(&signing_toml(TEST_PUBKEY), signed_routes(WRONG_KEY_SIG)).await;
        let pin = trust::PinnedChecksumStore::from_toml(&format!(
            "[[tool]]\ntool = \"{CRATE}\"\nversion = \"{VERSION}\"\nasset = \"asset.tgz\"\nsha256 = \"{}\"\n",
            trust::sha256_of(SIGNED_ASSET)
        ))
        .expect("pin");
        let err = resolve_served(&s, &pin, trust::TrustMode::Permissive)
            .await
            .expect_err("bad signature must be fatal");
        assert!(err.to_string().contains("minisign"), "{err}");
        assert!(!s.paths.bin.join(format!("{CRATE}-{VERSION}")).exists());
    });
}

#[test]
fn minisign_missing_signature_bad_key_or_algorithm_is_fatal() {
    runtime().block_on(async {
        let store = trust::PinnedChecksumStore::empty();
        let missing = serve_crate(&signing_toml(TEST_PUBKEY), signed_routes(b"")).await;
        let err = resolve_served(&missing, &store, trust::TrustMode::Permissive)
            .await
            .expect_err("declared signing without a signature must fail closed");
        assert!(err.to_string().contains("minisign"), "{err}");
        let bad_key = serve_crate(&signing_toml("RWnotakey"), signed_routes(GOOD_SIG)).await;
        let err = resolve_served(&bad_key, &store, trust::TrustMode::Permissive)
            .await
            .expect_err("malformed pubkey must fail closed");
        assert!(err.to_string().contains("minisign"), "{err}");
        let other_alg = serve_crate(
            &signing_toml(TEST_PUBKEY).replace("\"minisign\"", "\"pgp\""),
            signed_routes(GOOD_SIG),
        )
        .await;
        assert!(
            resolve_served(&other_alg, &store, trust::TrustMode::Permissive)
                .await
                .is_err()
        );
    });
}
