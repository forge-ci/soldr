use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The symbol and snapshot APIs identify Linux ELF artifacts by their GNU
    // build ID. Managed clang/lld does not add the distro GCC default, so make
    // this artifact contract explicit for the requested target (not the host).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-Wl,--build-id=sha1");
    }
    let cargo_manifest = Path::new(
        &std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("cargo always sets CARGO_MANIFEST_DIR for build.rs"),
    )
    .join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", cargo_manifest.display());
    verify_wasm_feature_isolation(&cargo_manifest)?;
    println!("cargo:rustc-env=KERNAL_API_WASM_FEATURE_ISOLATION=verified");
    let manifest_path = Path::new(
        &std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("cargo always sets CARGO_MANIFEST_DIR for build.rs"),
    )
    .join("conpty-sidecar.sha256.toml");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let rendered = render_hash_table(&read_manifest(&manifest_path));
    let out_dir = std::env::var_os("OUT_DIR").expect("cargo always sets OUT_DIR for build.rs");
    std::fs::write(
        Path::new(&out_dir).join("conpty_sidecar_hashes.rs"),
        rendered,
    )?;
    Ok(())
}

fn verify_wasm_feature_isolation(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path)?;
    let manifest = toml::from_str::<toml::Value>(&raw)?;
    let features = manifest
        .get("features")
        .and_then(toml::Value::as_table)
        .ok_or("Cargo.toml is missing [features]")?;
    let sketch_host = features
        .get("wasm-sketch-host")
        .and_then(toml::Value::as_array)
        .ok_or("wasm-sketch-host must be an array feature")?;
    let has_required_dependency =
        |name| sketch_host.iter().any(|value| value.as_str() == Some(name));
    if !has_required_dependency("dep:wasmtime") || !has_required_dependency("dep:wasmparser") {
        return Err("wasm-sketch-host must opt into wasmtime and wasmparser".into());
    }
    if features
        .get("full")
        .and_then(toml::Value::as_array)
        .is_some_and(|full| {
            full.iter()
                .any(|value| value.as_str() == Some("wasm-sketch-host"))
        })
    {
        return Err("full must not opt into wasm-sketch-host".into());
    }
    let dependencies = manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .ok_or("Cargo.toml is missing [dependencies]")?;
    if dependencies.contains_key("wasmtime-wasi") {
        return Err("ambient wasmtime-wasi must not be a direct dependency".into());
    }
    let wasmtime = dependencies
        .get("wasmtime")
        .and_then(toml::Value::as_table)
        .ok_or("wasmtime must be an optional table dependency")?;
    if wasmtime.get("version").and_then(toml::Value::as_str) != Some("=45.0.0")
        || wasmtime
            .get("default-features")
            .and_then(toml::Value::as_bool)
            != Some(false)
        || wasmtime.get("optional").and_then(toml::Value::as_bool) != Some(true)
    {
        return Err("wasmtime must remain exact-pinned, optional, and defaults-disabled".into());
    }
    Ok(())
}

#[derive(Default)]
struct ParsedManifest {
    x64: Option<(String, u64)>,
    arm64: Option<(String, u64)>,
    x86: Option<(String, u64)>,
    arm: Option<(String, u64)>,
}

fn read_manifest(path: &Path) -> ParsedManifest {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return ParsedManifest::default();
    };
    let Ok(table) = toml::from_str::<toml::Value>(&raw) else {
        return ParsedManifest::default();
    };
    let mut parsed = ParsedManifest::default();
    let Some(assets) = table.get("asset").and_then(toml::Value::as_table) else {
        return parsed;
    };
    for arch in ["x64", "arm64", "x86", "arm"] {
        let Some(entry) = assets.get(arch).and_then(toml::Value::as_table) else {
            continue;
        };
        let Some(sha) = entry.get("sha256").and_then(toml::Value::as_str) else {
            continue;
        };
        let size = entry
            .get("size_bytes")
            .and_then(toml::Value::as_integer)
            .unwrap_or_default()
            .max(0) as u64;
        let slot = match arch {
            "x64" => &mut parsed.x64,
            "arm64" => &mut parsed.arm64,
            "x86" => &mut parsed.x86,
            "arm" => &mut parsed.arm,
            _ => unreachable!(),
        };
        *slot = Some((sha.to_owned(), size));
    }
    parsed
}

fn render_hash_table(parsed: &ParsedManifest) -> String {
    let mut output = String::from(
        "pub(super) struct ExpectedAsset {\n    pub sha256_hex: &'static str,\n    pub size_bytes: u64,\n}\n\n",
    );
    for (name, value) in [
        ("EXPECTED_X64", &parsed.x64),
        ("EXPECTED_ARM64", &parsed.arm64),
        ("EXPECTED_X86", &parsed.x86),
        ("EXPECTED_ARM", &parsed.arm),
    ] {
        let body = value.as_ref().map_or_else(
            || "None".to_owned(),
            |(sha, size)| {
                format!("Some(ExpectedAsset {{ sha256_hex: \"{sha}\", size_bytes: {size} }})")
            },
        );
        output.push_str(&format!(
            "#[allow(dead_code)]\npub(super) const {name}: Option<ExpectedAsset> = {body};\n"
        ));
    }
    output
}
