//! Windows host facts.

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

/// The host operating system (this tree compiles only on Windows).
pub fn os() -> HostOs {
    HostOs::Windows
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
pub fn libc() -> HostLibc {
    HostLibc::None
}

/// The OS's dominant libc, probed at runtime. Not applicable here.
pub fn os_libc() -> HostLibc {
    HostLibc::None
}

/// The compile-time host triple (the target this binary was built for).
pub fn triple() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
    }
}

/// The legacy Windows MAX_PATH ceiling in characters, when the caller
/// needs to project path budgets against it.
pub fn max_path() -> Option<usize> {
    Some(260)
}

/// The PATH list separator for this host.
pub fn path_list_separator() -> &'static str {
    ";"
}

/// The running glibc's version string (for example `2.39`). Windows has no
/// glibc, so this is always `None`.
pub fn glibc_version() -> Option<String> {
    None
}

/// The Windows version as a dotted `major.minor.build` string (e.g.
/// `10.0.22621`). Probes the registry first — it always reflects the
/// host OS, never the compatibility-mode lie — then falls back to
/// PowerShell. `None` when neither probe succeeds.
pub fn os_version() -> Option<String> {
    registry_os_version().or_else(powershell_os_version)
}

fn registry_os_version() -> Option<String> {
    let output = crate::platform::wait::output_for(
        std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "/v",
            "CurrentBuildNumber",
        ]),
        crate::platform::wait::PROBE,
    )
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let build: u32 = stdout
        .lines()
        .find_map(|line| line.split_whitespace().last())
        .and_then(|tok| tok.parse().ok())?;
    Some(format!("10.0.{build}"))
}

fn powershell_os_version() -> Option<String> {
    let output = crate::platform::wait::output_for(
        std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[System.Environment]::OSVersion.Version | Select-Object -Property Major,Minor,Build | ConvertTo-Json -Compress",
        ]),
        crate::platform::wait::PROBE,
    )
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_powershell_version_json(&stdout)
}

fn parse_powershell_version_json(stdout: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(rename = "Major")]
        major: u32,
        #[serde(rename = "Minor")]
        minor: u32,
        #[serde(rename = "Build")]
        build: u32,
    }
    let trimmed = stdout.trim();
    let parsed: Version = serde_json::from_str(trimmed).ok()?;
    Some(format!("{}.{}.{}", parsed.major, parsed.minor, parsed.build))
}

/// All host facts in one probe (the Linux probe also performs the
/// runtime musl/glibc detection).
pub fn info() -> HostInfo {
    HostInfo {
        os: os(),
        arch: arch(),
        libc: libc(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_facts_report_windows() {
        assert_eq!(os(), HostOs::Windows);
        assert_eq!(info().libc, HostLibc::None);
    }

    #[test]
    fn powershell_version_json_parses_the_dotted_triple() {
        let json = r#"{"Major":10,"Minor":0,"Build":19045}"#;
        assert_eq!(parse_powershell_version_json(json), Some("10.0.19045".into()));
        assert_eq!(parse_powershell_version_json("{}"), None);
    }
}
