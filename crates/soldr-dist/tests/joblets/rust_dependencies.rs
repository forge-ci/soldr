//! Cold Rust units with transitive metadata, a host proc macro, and an input backstop.
use super::*;

async fn local(compiler: &Compiler, cwd: &Path, args: &[&str]) {
    let output = compiler
        .command()
        .args(args)
        .current_dir(cwd)
        .arg(format!("--remap-path-prefix={}=.", cwd.display()))
        .capture()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn dependencies(compiler: &Compiler, cwd: &Path) -> std::path::PathBuf {
    fs::create_dir_all(cwd.join("deps")).unwrap();
    fs::write(
        cwd.join("Cargo.toml"),
        "[package]\nname=\"dist_deps_fixture\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[workspace]\n",
    )
    .unwrap();
    fs::write(cwd.join("a.rs"), "pub fn one() -> u32 { 1 }\n").unwrap();
    fs::write(
        cwd.join("b.rs"),
        "extern crate a; pub fn two() -> u32 { a::one()+1 }\n",
    )
    .unwrap();
    fs::write(cwd.join("mac.rs"), "extern crate proc_macro; use proc_macro::TokenStream; #[proc_macro] pub fn value(_:TokenStream)->TokenStream { \"2u32\".parse().unwrap() }\n").unwrap();
    fs::write(
        cwd.join("lib.rs"),
        "extern crate b; extern crate mac; pub fn answer()->u32 { b::two()+mac::value!() }\n",
    )
    .unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "Cargo.toml", "a.rs", "b.rs", "mac.rs", "lib.rs"],
    ] {
        assert!(AsyncProcessBuilder::new("git")
            .args(args)
            .current_dir(cwd)
            .capture()
            .await
            .unwrap()
            .status
            .success());
    }
    local(
        compiler,
        cwd,
        &[
            "--crate-name=a",
            "--crate-type=rlib",
            "a.rs",
            "-o",
            "deps/liba.rlib",
        ],
    )
    .await;
    local(
        compiler,
        cwd,
        &[
            "--crate-name=b",
            "--crate-type=rlib",
            "b.rs",
            "--extern=a=deps/liba.rlib",
            "-Ldependency=deps",
            "-o",
            "deps/libb.rlib",
        ],
    )
    .await;
    let macro_file = format!("deps/libmac{}", std::env::consts::DLL_SUFFIX);
    local(
        compiler,
        cwd,
        &[
            "--crate-name=mac",
            "--crate-type=proc-macro",
            "mac.rs",
            "-o",
            &macro_file,
        ],
    )
    .await;
    cwd.join(macro_file)
}

#[test]
fn cold_rust_unit_with_two_dependencies_and_host_proc_macro_is_byte_identical() {
    if !Path::new("/").is_absolute() {
        return;
    }
    Runtime::new().unwrap().block_on(async {
        let compiler = Compiler::rustc().await.unwrap();
        let fixture = Fixture::new(vec![compiler.clone()], true).await;
        let cwd = fixture._temp.path().join("rust-dependencies");
        fs::create_dir_all(&cwd).unwrap();
        let macro_file = dependencies(&compiler, &cwd).await;
        let external = format!("--extern=mac={}", macro_file.display());
        let args = [
            "--crate-name=main",
            "--crate-type=rlib",
            // As Cargo does: the worker then reuses this dep-info as its input proof
            // instead of adding an output type, which would change the crate hash.
            "--emit=dep-info,link",
            "lib.rs",
            "--extern=b=deps/libb.rlib",
            &external,
            "-Ldependency=deps",
            "-o",
            "out.rlib",
        ];
        local(&compiler, &cwd, &args).await;
        let expected = fs::read(cwd.join("out.rlib")).unwrap();
        let mut manifest = job(
            &compiler,
            &cwd,
            args.iter().map(|s| s.to_string()).collect(),
            &cwd.join("lib.rs"),
            &cwd.join("out.rlib"),
        );
        manifest
            .env
            .insert("CARGO_MANIFEST_DIR".into(), cwd.to_str().unwrap().into());
        let memo = tokio::sync::Mutex::new(DigestMemo::default());
        let manifest = soldr_dist::rust_inputs::construct(manifest, &memo)
            .await
            .unwrap();
        assert!(manifest
            .inputs
            .iter()
            .any(|i| i.abs_path.ends_with("liba.rlib")));
        let result = fixture.client.execute(manifest.clone()).await;
        assert!(
            matches!(result, DistOutcome::Executed { exit_code: 0, .. }),
            "{result:?}"
        );
        assert_same_archive(&fs::read(cwd.join("out.rlib")).unwrap(), &expected);
        assert!(result.metrics().need_digests > 0);
        let again = fixture.client.execute(manifest.clone()).await;
        assert!(
            matches!(again, DistOutcome::Executed { exit_code: 0, .. }),
            "{again:?}"
        );
        assert_eq!(again.metrics().uploaded_blobs, 0);
        let digest = &manifest
            .inputs
            .iter()
            .find(|i| i.abs_path.ends_with("liba.rlib"))
            .unwrap()
            .blake3;
        let cas =
            soldr_dist::cas::Cas::new(&fixture._temp.path().join("worker"), u64::MAX).unwrap();
        fs::remove_file(cas.path(digest).unwrap()).unwrap();
        let recovered = fixture.client.execute(manifest.clone()).await;
        assert!(
            matches!(recovered, DistOutcome::Executed { exit_code: 0, .. }),
            "{recovered:?}"
        );
        assert_eq!(recovered.metrics().need_digests, 1);
        assert_eq!(recovered.metrics().uploaded_blobs, 1);
        assert_eq!(fs::read(cwd.join("out.rlib")).unwrap(), expected);
        let mut cross = manifest;
        let foreign = if compiler.identity.contains("host: aarch64") { "x86_64-unknown-linux-gnu" } else { "aarch64-unknown-linux-gnu" };
        cross.argv.extend(["--target".into(), foreign.into()]);
        let declined = fixture.client.execute(cross).await;
        assert!(matches!(declined, DistOutcome::Declined { ref reason, .. } if reason.contains("cross-triple proc macro")), "{declined:?}");
        assert_eq!(fs::read(cwd.join("out.rlib")).unwrap(), expected);
        fixture.stop().await;
    });
}

#[test]
fn escaped_dep_info_input_declines_without_replacing_a_local_output() {
    if !Path::new("/").is_absolute() {
        return;
    }
    Runtime::new().unwrap().block_on(async {
        let compiler = Compiler::rustc().await.unwrap();
        let fixture = Fixture::new(vec![compiler.clone()], true).await;
        let cwd = fixture._temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let outside = fixture._temp.path().join("outside file.txt");
        fs::write(&outside, "not shipped").unwrap();
        let source = cwd.join("lib.rs");
        fs::write(&source, format!("pub const TEXT: &str = include_str!({:?});\n", outside)).unwrap();
        let output = cwd.join("out.rlib");
        fs::write(&output, "existing local output").unwrap();
        let manifest = job(&compiler, &cwd, vec!["--crate-type=rlib".into(), "lib.rs".into(), "-o".into(), "out.rlib".into()], &source, &output);
        let result = fixture.client.execute(manifest).await;
        assert!(matches!(result, DistOutcome::Declined { ref reason, .. } if reason.contains("Rust input backstop") && reason.contains("outside")), "{result:?}");
        assert_eq!(fs::read(output).unwrap(), b"existing local output");
        fixture.stop().await;
    });
}

#[test]
fn shipped_absolute_include_cannot_read_different_original_host_bytes() {
    if !Path::new("/").is_absolute() {
        return;
    }
    Runtime::new().unwrap().block_on(async {
        let compiler = Compiler::rustc().await.unwrap();
        let fixture = Fixture::new(vec![compiler.clone()], true).await;
        let cwd = fixture._temp.path().join("absolute-include");
        fs::create_dir_all(&cwd).unwrap();
        let original = fixture._temp.path().join("generated.inc");
        let padding = "// padding\n".repeat(8000);
        let expected_bytes = format!("pub const VALUE:u32=42;\n{padding}");
        fs::write(&original, &expected_bytes).unwrap();
        let declared = DigestMemo::default().input(&original).unwrap();
        let cas = soldr_dist::cas::Cas::new(&fixture._temp.path().join("worker"), u64::MAX).unwrap();
        cas.put(expected_bytes.as_bytes()).unwrap();
        fs::write(&original, format!("pub const VALUE:u32=7;\n{padding}")).unwrap();
        let source = cwd.join("lib.rs");
        fs::write(&source, format!("include!({original:?});\n")).unwrap();
        let output = cwd.join("out.rlib");
        fs::write(&output, b"keep me").unwrap();
        let mut manifest = job(&compiler, &cwd, vec!["--crate-type=rlib".into(), "lib.rs".into(), "-o".into(), "out.rlib".into()], &source, &output);
        manifest.inputs.push(declared);
        let result = fixture.client.execute(manifest).await;
        assert!(matches!(result, DistOutcome::Declined { ref reason, .. } if reason.contains("outside sandbox")), "{result:?}");
        assert_eq!(fs::read(output).unwrap(), b"keep me");
        fixture.stop().await;
    });
}

/// Asserts two `ar` archives (rlibs) are identical; on a mismatch names each
/// differing member, its sizes and first differing offset, and the absolute
/// paths embedded in it (a leaked sandbox or checkout path is the usual cause).
fn assert_same_archive(actual: &[u8], expected: &[u8]) {
    if actual == expected {
        return;
    }
    fn members(bytes: &[u8]) -> Vec<(String, &[u8])> {
        let mut out = vec![];
        let mut at = 8; // "!<arch>\n"
        while at + 60 <= bytes.len() {
            let header = &bytes[at..at + 60];
            let name = String::from_utf8_lossy(&header[..16]).trim().to_string();
            let size: usize = String::from_utf8_lossy(&header[48..58]).trim().parse().unwrap_or(0);
            let body = &bytes[at + 60..(at + 60 + size).min(bytes.len())];
            out.push((name, body));
            at += 60 + size + (size % 2);
        }
        out
    }
    fn paths(body: &[u8]) -> Vec<String> {
        let mut found: Vec<String> = body
            .split(|b| !(b.is_ascii_graphic()))
            // rmeta length-prefixes strings, so the prefix byte may be printable.
            .filter_map(|run| run.iter().position(|b| *b == b'/').map(|at| &run[at..]))
            .filter(|run| run.len() > 4)
            .map(|run| String::from_utf8_lossy(run).into_owned())
            .collect();
        found.sort();
        found.dedup();
        found.truncate(12);
        found
    }
    let (a, e) = (members(actual), members(expected));
    let mut report = format!(
        "remote archive differs from local: {} vs {} bytes, {} vs {} members\n",
        actual.len(),
        expected.len(),
        a.len(),
        e.len()
    );
    for (index, (name, body)) in a.iter().enumerate() {
        match e.get(index) {
            Some((local_name, local)) if local_name == name && local == body => {}
            Some((local_name, local)) => {
                let offset = body.iter().zip(local.iter()).position(|(x, y)| x != y);
                let differing = body.iter().zip(local.iter()).filter(|(x, y)| x != y).count();
                let window = |bytes: &[u8]| {
                    let at = offset.unwrap_or(0);
                    let range = at.saturating_sub(48)..(at + 48).min(bytes.len());
                    String::from_utf8_lossy(&bytes[range]).escape_debug().to_string()
                };
                report += &format!(
                    "  {differing} differing byte(s)\n    remote around: {}\n    local around:  {}\n",
                    window(body),
                    window(local)
                );
                report += &format!(
                    "  member {index} remote `{name}` ({} B) vs local `{local_name}` ({} B); first difference at {offset:?}\n    remote paths: {:?}\n    local paths:  {:?}\n",
                    body.len(),
                    local.len(),
                    paths(body),
                    paths(local)
                );
            }
            None => report += &format!("  member {index} `{name}` only in the remote archive\n"),
        }
    }
    panic!("{report}");
}
