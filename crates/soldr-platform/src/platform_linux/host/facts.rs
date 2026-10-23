//! Linux host facts, including libc detection.

/// The host operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    /// Microsoft Windows.
    Windows,
    /// Apple macOS.
    MacOs,
    /// Linux.
    Linux,
}

/// The host CPU architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostArch {
    /// x86-64.
    X86_64,
    /// 64-bit ARM.
    Aarch64,
    /// Anything else; carries the `std::env::consts::ARCH` string.
    Unknown(&'static str),
}

/// The host's C library environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostLibc {
    /// Not applicable (Windows, macOS).
    None,
    /// GNU libc.
    Gnu,
    /// musl.
    Musl,
}

/// The raw compile-host facts `TargetTriple::host()` consumes. The
/// triple construction itself is the caller's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostInfo {
    /// Host operating system.
    pub os: HostOs,
    /// Host architecture.
    pub arch: HostArch,
    /// Host libc environment.
    pub libc: HostLibc,
}

/// The host operating system (this tree compiles only on Linux).
pub fn os() -> HostOs {
    HostOs::Linux
}

/// The host architecture.
pub fn arch() -> HostArch {
    if cfg!(target_arch = "x86_64") {
        HostArch::X86_64
    } else if cfg!(target_arch = "aarch64") {
        HostArch::Aarch64
    } else {
        HostArch::Unknown(std::env::consts::ARCH)
    }
}

/// The compile-time libc environment (the `target_env` of this build).
/// The runtime probes in [`detect_linux_libc`] exist for hosts where
/// the binary's own env differs from the OS's dominant libc.
pub fn libc() -> HostLibc {
    if cfg!(target_env = "musl") {
        HostLibc::Musl
    } else {
        HostLibc::Gnu
    }
}

/// The OS's dominant libc, probed at runtime **without** the compile-time
/// shortcut [`detect_linux_libc`] takes.
///
/// soldr's default Linux download is a static musl binary (soldr#1060), so on
/// a glibc distribution [`libc`] -- and therefore [`info`] -- report `Musl`
/// even though glibc is present. Use this when the question is "can a
/// glibc-dynamic executable run on this OS", not "what was soldr built for"
/// (soldr#3435).
pub fn os_libc() -> HostLibc {
    let musl_loader = probe_musl_dynamic_linker_present();
    let glibc_loader = probe_glibc_dynamic_linker_present();
    classify_os_libc(
        ldd_reports_musl_unless_loaders_decide(musl_loader, glibc_loader),
        musl_loader,
        glibc_loader,
    )
}

/// [`classify_linux_libc`] with the compile-time flag pinned off, so
/// [`os_libc`] and [`detect_linux_libc`] share one classification table.
fn classify_os_libc(
    ldd_reports_musl: bool,
    musl_dynamic_linker_present: bool,
    glibc_dynamic_linker_present: bool,
) -> HostLibc {
    classify_linux_libc(
        false,
        ldd_reports_musl,
        musl_dynamic_linker_present,
        glibc_dynamic_linker_present,
    )
}

/// The compile-time host triple (the target this binary was built for).
pub fn triple() -> &'static str {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let env = if cfg!(target_env = "musl") {
        "musl"
    } else {
        "gnu"
    };
    // Only the four canonical triples are in scope here.
    match (arch, env) {
        ("aarch64", "musl") => "aarch64-unknown-linux-musl",
        ("x86_64", "musl") => "x86_64-unknown-linux-musl",
        ("aarch64", _) => "aarch64-unknown-linux-gnu",
        _ => "x86_64-unknown-linux-gnu",
    }
}

/// Linux has no MAX_PATH-style ceiling worth projecting against.
pub fn max_path() -> Option<usize> {
    None
}

/// The PATH list separator for this host.
pub fn path_list_separator() -> &'static str {
    ":"
}

/// The version of the glibc this process is running against (for example
/// `2.39`), read from `gnu_get_libc_version(3)`. `None` for a musl build,
/// which has no glibc to ask.
pub fn glibc_version() -> Option<String> {
    running_glibc_version()
}

#[cfg(target_env = "gnu")]
fn running_glibc_version() -> Option<String> {
    // SAFETY: `gnu_get_libc_version` takes no arguments and returns a pointer
    // to a static, NUL-terminated string owned by glibc for the life of the
    // process.
    let raw = unsafe { libc::gnu_get_libc_version() };
    if raw.is_null() {
        return None;
    }
    // SAFETY: non-null, NUL-terminated, and static (see above).
    let version = unsafe { std::ffi::CStr::from_ptr(raw) };
    version.to_str().ok().map(str::to_string)
}

#[cfg(not(target_env = "gnu"))]
fn running_glibc_version() -> Option<String> {
    None
}

/// The OS version string. Soldr's Linux host facts have no version
/// probe — the Windows-specific registry/PowerShell queries live in the
/// Windows tree — so this is always `None` on Linux.
pub fn os_version() -> Option<String> {
    None
}

/// All host facts in one probe (with the runtime musl/glibc detection).
pub fn info() -> HostInfo {
    HostInfo {
        os: os(),
        arch: arch(),
        libc: detect_linux_libc(),
    }
}

/// Detect musl vs glibc. Order matters:
///
/// 1. **Compile-time musl**: the build itself is musl (also covers
///    Android-style unknown envs).
/// 2. **ldd reports musl**: glibc writes its version banner to stdout;
///    musl writes to stderr and exits non-zero — check both.
/// 3. **musl dynamic linker present without a glibc linker**: e.g. CI
///    runners install `musl-tools` for cross-compile lanes, and that
///    drops a musl linker onto an otherwise glibc host.
/// 4. **Default**: glibc. Most Linux distributions ship glibc — only
///    musl distros need the override.
pub(crate) fn detect_linux_libc() -> HostLibc {
    if cfg!(target_env = "musl") {
        return HostLibc::Musl;
    }
    let musl_loader = probe_musl_dynamic_linker_present();
    let glibc_loader = probe_glibc_dynamic_linker_present();
    classify_linux_libc(
        false,
        ldd_reports_musl_unless_loaders_decide(musl_loader, glibc_loader),
        musl_loader,
        glibc_loader,
    )
}

/// Whether the loader stats alone settle the host as glibc: a glibc loader
/// and no musl loader. Every glibc distribution looks like this, so the
/// `ldd --version` spawn (a serial front-door probe, soldr#3794) is only paid
/// where a musl loader exists and ldd must break the tie.
fn loaders_prove_glibc(musl_loader: bool, glibc_loader: bool) -> bool {
    glibc_loader && !musl_loader
}

fn ldd_reports_musl_unless_loaders_decide(musl_loader: bool, glibc_loader: bool) -> bool {
    !loaders_prove_glibc(musl_loader, glibc_loader) && probe_ldd_reports_musl()
}

fn classify_linux_libc(
    compile_time_musl: bool,
    ldd_reports_musl: bool,
    musl_dynamic_linker_present: bool,
    glibc_dynamic_linker_present: bool,
) -> HostLibc {
    if compile_time_musl {
        return HostLibc::Musl;
    }
    if ldd_reports_musl {
        return HostLibc::Musl;
    }
    if musl_dynamic_linker_present && !glibc_dynamic_linker_present {
        return HostLibc::Musl;
    }
    HostLibc::Gnu
}

fn probe_ldd_reports_musl() -> bool {
    let Ok(output) = crate::platform::wait::output_for(
        std::process::Command::new("ldd").arg("--version"),
        crate::platform::wait::PROBE,
    ) else {
        return false;
    };
    ldd_output_mentions_musl(&output.stdout) || ldd_output_mentions_musl(&output.stderr)
}

fn ldd_output_mentions_musl(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .is_some_and(|s| s.to_ascii_lowercase().contains("musl"))
}

fn probe_musl_dynamic_linker_present() -> bool {
    const CANDIDATES: &[&str] = &[
        "/lib/ld-musl-x86_64.so.1",
        "/lib/ld-musl-aarch64.so.1",
        "/lib/ld-musl-armhf.so.1",
        "/lib/ld-musl-arm.so.1",
        "/lib/ld-musl-i386.so.1",
    ];
    CANDIDATES.iter().any(|p| std::path::Path::new(p).exists())
}

fn probe_glibc_dynamic_linker_present() -> bool {
    const CANDIDATES: &[&str] = &[
        "/lib64/ld-linux-x86-64.so.2",
        "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
        "/lib/ld-linux-aarch64.so.1",
        "/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
    ];
    CANDIDATES.iter().any(|p| std::path::Path::new(p).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libc_classification_prefers_compile_time_then_probes() {
        assert_eq!(
            classify_linux_libc(true, false, false, false),
            HostLibc::Musl
        );
        assert_eq!(
            classify_linux_libc(false, true, false, false),
            HostLibc::Musl
        );
        assert_eq!(
            classify_linux_libc(false, false, true, false),
            HostLibc::Musl
        );
        assert_eq!(classify_linux_libc(false, false, true, true), HostLibc::Gnu);
        assert_eq!(
            classify_linux_libc(false, false, false, false),
            HostLibc::Gnu
        );
    }

    #[test]
    fn os_libc_classification_ignores_how_soldr_was_built() {
        // A musl-static soldr on a glibc distro: no musl ldd, both loaders
        // or only glibc's present -> the OS is glibc.
        assert_eq!(classify_os_libc(false, false, true), HostLibc::Gnu);
        assert_eq!(classify_os_libc(false, true, true), HostLibc::Gnu);
        assert_eq!(classify_os_libc(false, false, false), HostLibc::Gnu);
        // Alpine: ldd is musl's, or only the musl loader exists.
        assert_eq!(classify_os_libc(true, true, false), HostLibc::Musl);
        assert_eq!(classify_os_libc(false, true, false), HostLibc::Musl);
    }

    #[test]
    fn ldd_runs_only_when_a_musl_loader_leaves_the_question_open() {
        // soldr#3794: a plain glibc host is settled by stat calls alone.
        assert!(loaders_prove_glibc(false, true));
        // A musl loader (Alpine, or musl-tools on a glibc runner), or no
        // known loader at all, still asks ldd.
        assert!(!loaders_prove_glibc(true, true));
        assert!(!loaders_prove_glibc(true, false));
        assert!(!loaders_prove_glibc(false, false));
        // Where ldd is skipped, the verdict is the one it would get from a
        // glibc ldd.
        assert_eq!(classify_os_libc(false, false, true), HostLibc::Gnu);
    }

    #[test]
    fn linux_facts_report_linux() {
        assert_eq!(os(), HostOs::Linux);
    }

    #[test]
    fn glibc_version_is_read_from_the_running_libc_on_a_gnu_build() {
        match libc() {
            HostLibc::Gnu => {
                let version = glibc_version().expect("a glibc build can read its glibc version");
                assert!(version.starts_with("2."), "{version}");
            }
            _ => assert_eq!(glibc_version(), None),
        }
    }
}
