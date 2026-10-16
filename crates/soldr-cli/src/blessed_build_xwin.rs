fn clang_shim_names() -> Vec<String> {
    // Only `clang` + `clang++`. DO NOT add `clang-cl`: the shim invokes
    // it as its downstream, and PATH could then find the shim's own
    // clang-cl first and recurse (ci/docker-aarch64-windows-msvc-cross/).
    vec![
        crate::platform::executable::name::native("clang"),
        crate::platform::executable::name::native("clang++"),
    ]
}

/// Build the MSVC-style include-flag string that cargo-xwin would
/// have injected for a child cargo invocation. Format:
/// `"/imsvc <crt/include> /imsvc <sdk/include/ucrt> ..."` — each
/// `/imsvc` is followed by the path as a separate token. clang-cl
/// natively recognizes `/imsvc <path>` as an MSVC-style include
/// directive (equivalent to clang's `-isystem <path>`).
///
/// Paths that don't exist on disk are silently skipped — defends
/// against catalogue-shape drift (e.g. a future xwin-cache that
/// stops shipping the winrt include tree shouldn't make the whole
/// CFLAGS injection error out).
fn xwin_msvc_cflags(cache_dir: &std::path::Path) -> String {
    let existing = xwin_include_dirs(cache_dir);
    let mut flags = existing
        .iter()
        // No space between `/imsvc` and the path. clang-cl accepts
        // the `/imsvc<path>` joined form; the two-token `/imsvc <path>`
        // form gets mangled because cc-rs receives CFLAGS, splits on
        // whitespace, then passes each token as a separate argv entry
        // — and clang-cl ends up seeing `/imsvc <path>` as two
        // unrelated args (the path arg is treated as a positional
        // source file). soldr#1070 root cause.
        .map(|p| format!("/imsvc{}", p.display()))
        .collect::<Vec<_>>()
        .join(" ");
    // soldr#3415: code outside the SDK includes headers in the casing it was
    // written in (`<BaseTsd.h>`), and the bundle stores one casing. A generated
    // case-insensitive clang overlay resolves any spelling; anything it does not
    // list falls through to the real file system. Best-effort: without it the
    // include trees still work for their stored and aliased casings.
    if let Some(overlay) = xwin_case_overlay(cache_dir, &existing) {
        flags.push_str(&format!(" -vfsoverlay {}", overlay.display()));
    }
    flags
}

/// The xwin bundle's C/C++ include directories that exist on disk, in search
/// order. Shared by the cargo path above and `soldr cc` (soldr#3806) so the
/// two cannot disagree about which SDK headers a target sees.
pub(crate) fn xwin_include_dirs(cache_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    [
        cache_dir.join("crt").join("include"),
        cache_dir.join("sdk").join("include").join("ucrt"),
        cache_dir.join("sdk").join("include").join("um"),
        cache_dir.join("sdk").join("include").join("shared"),
        cache_dir.join("sdk").join("include").join("winrt"),
        cache_dir.join("sdk").join("include").join("cppwinrt"),
    ]
    .into_iter()
    .filter(|p| p.is_dir())
    .collect()
}

/// soldr#3415: the case-insensitive header overlay for `include_dirs`, or
/// `None` when it is not needed or could not be written (best-effort).
pub(crate) fn xwin_case_overlay(
    cache_dir: &std::path::Path,
    include_dirs: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    match crate::fetch::xwin_case_overlay::ensure_case_overlay(cache_dir, include_dirs) {
        Ok(overlay) => overlay,
        Err(error) => {
            eprintln!(
                "soldr: could not write the SDK case-insensitive overlay ({error}); \
                 mixed-case #include spellings may not resolve"
            );
            None
        }
    }
}

/// The xwin bundle's per-arch library directories for an MSVC target that
/// exist on disk (`crt/lib/<arch>`, `sdk/lib/{um,ucrt}/<arch>`). Empty for a
/// non-MSVC or unsupported-arch triple. Shared with `soldr cc` / `soldr link`.
pub(crate) fn xwin_lib_dirs(
    cache_dir: &std::path::Path,
    target_triple: &str,
) -> Vec<std::path::PathBuf> {
    if !target_triple.ends_with("-pc-windows-msvc") {
        return Vec::new();
    }
    let arch_dirs: &[&str] = if target_triple.starts_with("aarch64-") {
        &["arm64", "aarch64"]
    } else if target_triple.starts_with("x86_64-") {
        &["x64", "x86_64"]
    } else {
        return Vec::new();
    };
    arch_dirs
        .iter()
        .flat_map(|arch| {
            [
                cache_dir.join("crt").join("lib").join(arch),
                cache_dir.join("sdk").join("lib").join("um").join(arch),
                cache_dir.join("sdk").join("lib").join("ucrt").join(arch),
            ]
        })
        .filter(|p| p.is_dir())
        .collect()
}

/// Build the rustc lld-link + CRT flags and `-C link-arg=/LIBPATH:<path>`
/// chain for the xwin-cache.
///
/// By default this selects the UCRT/VCRuntime **import** libraries: the same
/// bundle also ships static `libucrt.lib`, which cannot satisfy
/// `__declspec(dllimport)` references emitted by clang-cl's default `/MD`
/// mode.
///
/// When `linkage` is [`CrtLinkage::Static`] the choice is mirrored — the
/// static archives are selected and the import libraries excluded. soldr#2794:
/// the dynamic set used to be unconditional, so a caller passing
/// `-C target-feature=+crt-static` got rustc's `/defaultlib:libcmt` *and*
/// soldr's import libraries on one link line, and lld-link failed on duplicate
/// symbols. The linkage is a policy that belongs to the caller; only the
/// linker flavor and the library search paths are soldr's plumbing.
///
/// The xwin tarball lays libs out per-arch as `crt/lib/<arch>/` and
/// `sdk/lib/{um,ucrt}/<arch>/` where `<arch>` is the MS arch name
/// (`x64`, `arm64`) — matching xwin's `--preserve-ms-arch-notation`
/// flag in the upstream recipe.
fn xwin_msvc_link_args(
    cache_dir: &std::path::Path,
    target_triple: &str,
    linkage: CrtLinkage,
) -> String {
    if !target_triple.ends_with("-pc-windows-msvc")
        || !(target_triple.starts_with("aarch64-") || target_triple.starts_with("x86_64-"))
    {
        return String::new();
    }
    let lib_dirs = xwin_lib_dirs(cache_dir, target_triple);
    // Exclude the CRT we are not using and name the one we are, for both the
    // UCRT and the VCRuntime. Naming without excluding is not enough: the
    // unwanted archive still arrives via another object's default-lib
    // directive, which is how the duplicate-symbol failure in soldr#2794
    // reached the linker in the first place.
    //
    // The dynamic arm is byte-for-byte what soldr shipped before soldr#2794.
    // Widening it (say, adding a symmetric `/NODEFAULTLIB:libvcruntime.lib`)
    // would be harmless in principle and is still a change to every existing
    // consumer's link line, which the issue explicitly rules out.
    let crt_args: &[&str] = match linkage {
        CrtLinkage::Dynamic => &[
            "link-arg=/NODEFAULTLIB:libucrt.lib",
            "link-arg=/DEFAULTLIB:ucrt.lib",
            "link-arg=/DEFAULTLIB:vcruntime.lib",
        ],
        CrtLinkage::Static => &[
            "link-arg=/NODEFAULTLIB:ucrt.lib",
            "link-arg=/NODEFAULTLIB:vcruntime.lib",
            "link-arg=/DEFAULTLIB:libucrt.lib",
            "link-arg=/DEFAULTLIB:libvcruntime.lib",
        ],
    };
    let mut args = vec!["-C".to_string(), "linker-flavor=lld-link".to_string()];
    args.extend(
        crt_args
            .iter()
            .flat_map(|arg| ["-C".to_string(), (*arg).to_string()]),
    );
    args.extend(
        lib_dirs
            .iter()
            .flat_map(|p| {
                vec![
                    "-C".to_string(),
                    format!("link-arg=/LIBPATH:{}", p.display()),
                ]
            }),
    );
    args.join(" ")
}
