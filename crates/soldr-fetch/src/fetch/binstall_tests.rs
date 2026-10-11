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
    let meta = binstall_template::BinstallMeta::from_cargo_toml(toml, &target)
        .expect("parse")
        .expect("present");
    assert_eq!(meta.pkg_url, "https://w/{ name }.zip");
    assert_eq!(meta.pkg_fmt, PkgFmt::Zip);
    let none = binstall_template::BinstallMeta::from_cargo_toml(
        "[package]\nname = \"t\"\nversion = \"1.0.0\"\n",
        &target,
    )
    .expect("parse");
    assert!(none.is_none());
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
