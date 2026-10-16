use super::*;

#[test]
fn fuse_accepts_lld_and_every_soldr_linker_value_case_insensitively() {
    for (input, fuse) in [
        ("lld", Fuse::Lld),
        ("LLD", Fuse::Lld),
        ("default", Fuse::Lld),
        ("ld", Fuse::GnuLd),
        ("RELD", Fuse::Reld),
        ("reld", Fuse::Reld),
        ("fast", Fuse::Reld),
        ("Mold", Fuse::Mold),
        ("rust-lld", Fuse::RustLld),
    ] {
        assert_eq!(parse_fuse(Some(input)).unwrap(), fuse, "{input}");
    }
    assert!(parse_fuse(Some("gold")).is_err());
}

#[test]
fn families_cover_the_supported_targets() {
    assert_eq!(family("x86_64-unknown-linux-gnu"), Some(Family::LinuxGnu));
    assert_eq!(
        family("aarch64-unknown-linux-musl"),
        Some(Family::LinuxMusl)
    );
    assert_eq!(family("aarch64-apple-darwin"), Some(Family::Darwin));
    assert_eq!(family("x86_64-pc-windows-gnu"), Some(Family::WindowsGnu));
    assert_eq!(family("aarch64-pc-windows-msvc"), Some(Family::WindowsMsvc));
    assert_eq!(family("wasm32-unknown-unknown"), None);
}

#[test]
fn the_wrong_verb_names_the_right_one() {
    let err = check_dialect(
        Dialect::Gnu,
        Family::WindowsMsvc,
        "win-x64",
        "x86_64-pc-windows-msvc",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("use `soldr link --target win-x64`"), "{err}");

    let err = check_dialect(
        Dialect::Msvc,
        Family::Darwin,
        "mac-arm64",
        "aarch64-apple-darwin",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("use `soldr ld --target mac-arm64`"), "{err}");

    for family in [
        Family::LinuxGnu,
        Family::LinuxMusl,
        Family::Darwin,
        Family::WindowsGnu,
    ] {
        assert!(check_dialect(Dialect::Gnu, family, "t", "t").is_ok());
    }
    assert!(check_dialect(Dialect::Msvc, Family::WindowsMsvc, "t", "t").is_ok());
}

#[test]
fn reld_and_mold_are_elf_only_and_say_what_is_supported() {
    for family in [Family::LinuxGnu, Family::LinuxMusl] {
        assert!(check_fuse("ld", family, Fuse::Reld, "t").is_ok());
        assert!(check_fuse("ld", family, Fuse::Mold, "t").is_ok());
        assert!(check_fuse("ld", family, Fuse::GnuLd, "t").is_ok());
    }
    let err = check_fuse("ld", Family::Darwin, Fuse::Reld, "x86_64-apple-darwin")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("--fuse=reld cannot link `x86_64-apple-darwin`"),
        "{err}"
    );
    assert!(err.contains("lld, rust-lld"), "{err}");
    assert!(check_fuse("link", Family::WindowsMsvc, Fuse::GnuLd, "t").is_err());
    assert!(check_fuse("ld", Family::WindowsGnu, Fuse::GnuLd, "t").is_ok());
    for family in [
        Family::LinuxGnu,
        Family::LinuxMusl,
        Family::Darwin,
        Family::WindowsGnu,
        Family::WindowsMsvc,
    ] {
        assert!(check_fuse("ld", family, Fuse::Lld, "t").is_ok());
    }
}

#[test]
fn darwin_link_args_name_arch_platform_and_sdk() {
    let args = darwin_link_args("aarch64-apple-darwin", Path::new("/sdk"));
    assert_eq!(&args[..2], ["-arch", "arm64"]);
    assert_eq!(&args[2..4], ["-platform_version", "macos"]);
    assert_eq!(&args[6..], ["-syslibroot", "/sdk"]);
    let x64 = darwin_link_args("x86_64-apple-darwin", Path::new("/sdk"));
    assert_eq!(x64[1], "x86_64");
}

#[test]
fn msvc_link_args_set_machine_and_libpaths() {
    let args = msvc_link_args(
        "aarch64-pc-windows-msvc",
        &[PathBuf::from("/x/crt/lib/arm64")],
    );
    assert_eq!(args, ["/MACHINE:ARM64", "/LIBPATH:/x/crt/lib/arm64"]);
    let x64 = msvc_link_args("x86_64-pc-windows-msvc", &[]);
    assert_eq!(x64, ["/MACHINE:X64"]);
}

#[test]
fn elf_search_args_only_name_existing_sysroot_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("usr/lib")).unwrap();
    let args = elf_search_args(tmp.path());
    assert_eq!(args[0], format!("--sysroot={}", tmp.path().display()));
    assert_eq!(
        &args[1..],
        [format!("-L{}", tmp.path().join("usr/lib").display())]
    );
}
