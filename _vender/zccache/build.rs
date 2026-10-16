// Build scripts are allowed to panic on setup failure: cargo surfaces
// the panic message and fails the build cleanly.
#![allow(clippy::expect_used)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=ZCCACHE_BUILD_TARGET={target}");

    println!("cargo:rerun-if-changed=proto/zccache_v1.proto");
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("vendored protoc is available for zccache wire protobufs");
    std::env::set_var("PROTOC", protoc);

    prost_build::Config::new()
        .compile_protos(&["proto/zccache_v1.proto"], &["proto"])
        .expect("zccache wire protobufs compile");
}
