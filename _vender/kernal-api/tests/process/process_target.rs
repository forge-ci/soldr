use kernal_api::platform::host::{process_target, ProcessTarget};

#[test]
fn process_target_reports_binary_target_without_host_probing() {
    const TARGET: ProcessTarget = process_target();
    assert_eq!(TARGET.os, std::env::consts::OS);
    assert_eq!(TARGET.architecture, std::env::consts::ARCH);
    #[cfg(target_os = "windows")]
    assert_eq!(TARGET.os, "windows");
    #[cfg(target_os = "macos")]
    assert_eq!(TARGET.os, "macos");
    #[cfg(target_os = "linux")]
    assert_eq!(TARGET.os, "linux");
    #[cfg(target_arch = "x86_64")]
    assert_eq!(TARGET.architecture, "x86_64");
    #[cfg(target_arch = "aarch64")]
    assert_eq!(TARGET.architecture, "aarch64");
}
