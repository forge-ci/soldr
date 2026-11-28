//! Host facts, directories, user identity, resources, and autostart primitives.
//!
//! Callers ask what is true of this host and this process -- who am I, am I
//! elevated -- and decide for themselves what that means. Whether the answer
//! came from a uid comparison or a token query is not something a caller
//! should have to know, or be able to tell.

use std::io;

pub use crate::{
    host_boot_id as boot_id, host_current_process_privilege as current_process_privilege,
    host_current_user as current_user,
    host_environment_keys_are_case_insensitive as environment_keys_are_case_insensitive,
    host_filesystem_device_id as filesystem_device_id, host_home_dir as home_dir,
    host_hostname as hostname, host_is_elevated as is_elevated,
    host_login_environment as login_environment, host_machine_id as machine_id,
    host_namespace_id as namespace_id, host_user_machine_identity as user_machine_identity,
    HostPrivilegedIdentity as PrivilegedIdentity,
};

pub use crate::host_login_environment_block as login_environment_block;

/// Ordered compatibility-key feature subset this process can see.
///
/// On x86 and x86-64 this reports whichever of `sse2`, `sse4.2`, `avx`,
/// `avx2`, `avx512f`, `fma`, `bmi1` and `bmi2` are detected, in that order.
/// Every other architecture reports an empty list. This is neither a complete
/// CPU inventory nor a host-architecture probe: it answers "which of the
/// features a cache key cares about does this process have".
///
/// The order and the spellings are contractual -- [`cpu_identity_material`]
/// hashes them, and so may callers -- so an entry may be added to the end but
/// an existing one may not be renamed or reordered.
pub use crate::host_cpu_compatibility_features as cpu_compatibility_features;

/// Compile target of this executing binary, not the physical host hardware.
///
/// Names follow Rust's target vocabulary (for example `windows`, `macos`,
/// `linux`, `x86_64`, and `aarch64`). Package registry aliases and supported
/// artifact selection are caller policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProcessTarget {
    /// Operating system the binary was compiled for.
    pub os: &'static str,
    /// Architecture the binary was compiled for.
    pub architecture: &'static str,
}

/// Report the executing binary's compile target without probing the host.
///
/// An x86-64 binary running under emulation on ARM64 still reports `x86_64`.
/// This does not detect Rosetta, CPU features, the host's native architecture,
/// or ABI details such as libc. No subprocess or environment lookup is used.
pub const fn process_target() -> ProcessTarget {
    ProcessTarget {
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
    }
}

/// Whether this binary was compiled for Windows, in a constant expression.
///
/// A predicate over [`process_target`], not a probe: an x86-64 Windows binary
/// running under emulation still reports `true`. Written against
/// [`std::env::consts::OS`] rather than a `cfg!` so host selection stays in
/// the one selector this crate has, and so the answer is the same fact
/// [`process_target`] already reports.
pub const fn target_is_windows() -> bool {
    matches!(std::env::consts::OS.as_bytes(), b"windows")
}

/// Whether this binary was compiled for macOS, in a constant expression.
///
/// See [`target_is_windows`] for what this does and does not answer.
pub const fn target_is_macos() -> bool {
    matches!(std::env::consts::OS.as_bytes(), b"macos")
}

/// Whether this binary was compiled for Linux, in a constant expression.
///
/// See [`target_is_windows`] for what this does and does not answer.
pub const fn target_is_linux() -> bool {
    matches!(std::env::consts::OS.as_bytes(), b"linux")
}

/// Logical concurrency this host exposes to this process.
///
/// A thin, host-neutral restatement of [`std::thread::available_parallelism`]
/// -- kept on the facade so a caller sizing a thread pool from "how many CPUs
/// does this host have" reads that intent from `platform::host` alongside the
/// rest of what it already asks this module, rather than reaching past it for
/// one `std::thread` call.
pub fn available_parallelism() -> Option<usize> {
    std::thread::available_parallelism()
        .ok()
        .map(std::num::NonZeroUsize::get)
}

/// Opaque raw host inputs a caller can hash to domain-separate a native CPU
/// key.
///
/// Combines architecture, OS, and whichever of this machine's identity or
/// this host's name is available, plus the CPU feature flags this process
/// observes, into one string with no delimiter a caller needs to parse --
/// they hash it, they do not read it apart. Composed entirely from facts this
/// facade already reports elsewhere (see [`machine_id`] and [`hostname`]), so
/// it needs no platform-specific code of its own; a raw process id is used
/// only when neither of those answers, which keeps the material scoped to
/// this machine rather than this one process wherever possible.
pub fn cpu_identity_material() -> String {
    let mut material = format!(
        "arch={}\0os={}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    if let Some(id) = machine_id() {
        material.push_str("\0machine-id=");
        material.push_str(&id);
    } else if let Some(name) = hostname() {
        material.push_str("\0hostname=");
        material.push_str(&name);
    } else {
        material.push_str("\0pid=");
        material.push_str(&std::process::id().to_string());
    }
    append_cpu_feature_material(&mut material);
    material
}

fn append_cpu_feature_material(material: &mut String) {
    for name in cpu_compatibility_features() {
        material.push_str("\0feature=");
        material.push_str(name);
    }
}

/// Resolve a machine identity from the first readable of `machine_id_paths`,
/// falling back to a boot-scoped id.
///
/// Lives in the neutral leaf, not the Linux tree, so it compiles and is tested
/// on every host. The rules it encodes are subtle enough to be worth testing
/// where the tests actually run, and only the Linux implementation supplies
/// real paths to it.
// Only the Linux implementation supplies real paths to this, so other
// hosts see it as dead code. It stays compiled on all of them anyway:
// that is what keeps the tests below running everywhere rather than on
// one host.
#[allow(dead_code)]
pub(crate) fn machine_id_from(machine_id_paths: &[&str], boot_id_path: &str) -> io::Result<String> {
    for path in machine_id_paths {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            // An unreadable machine-id stays a hard error rather than falling
            // through: sibling processes of the same user may read the file
            // fine, and deriving a different identity here would split the
            // user across two identities -- two brokers, each believing it is
            // the singleton.
            Err(err) => return Err(io::Error::other(format!("read {path}: {err}"))),
        }
    }
    // Read-only fallback for hosts that ship no machine-id file at all
    // (minimal containers, machine-id-less musl distros): a boot-scoped
    // identity from the kernel's boot_id. Every process in the same boot
    // derives the same value -- exactly the lifetime this must cover -- and
    // file *absence*, unlike readability, cannot differ between one user's
    // processes, so the fallback stays consistent.
    if let Ok(s) = std::fs::read_to_string(boot_id_path) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(format!("boot:{trimmed}"));
        }
    }
    Err(io::Error::other(
        "no /etc/machine-id or /var/lib/dbus/machine-id found, and no usable boot_id fallback",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly one host predicate is true, and it agrees with `process_target`.
    ///
    /// Stated as a property rather than "on Linux this is true": the point is
    /// that the three answers are consistent with the one fact the facade
    /// already reports, on whichever host runs them.
    #[test]
    fn exactly_one_target_predicate_holds_and_matches_process_target() {
        let predicates = [
            ("windows", target_is_windows()),
            ("macos", target_is_macos()),
            ("linux", target_is_linux()),
        ];

        let named: Vec<_> = predicates
            .iter()
            .filter_map(|(name, holds)| holds.then_some(*name))
            .collect();

        assert_eq!(named.len(), 1, "one supported host answers: {named:?}");
        assert_eq!(named[0], process_target().os);
    }

    /// The predicates are usable where a `cfg!` would be: in a const.
    #[test]
    fn target_predicates_are_constant_expressions() {
        const WINDOWS: bool = target_is_windows();
        const MACOS: bool = target_is_macos();
        const LINUX: bool = target_is_linux();

        assert_eq!(
            [WINDOWS, MACOS, LINUX].iter().filter(|held| **held).count(),
            1
        );
    }

    /// Reported features keep the documented order, without repeats.
    ///
    /// Callers may put these in a cache key, so the contract under test is the
    /// ordering and spelling rather than which features this particular CPU
    /// has -- an assertion on the latter would only restate the host.
    #[test]
    fn cpu_compatibility_features_are_ordered_and_unique() {
        const DOCUMENTED: [&str; 8] = [
            "sse2", "sse4.2", "avx", "avx2", "avx512f", "fma", "bmi1", "bmi2",
        ];

        let reported = cpu_compatibility_features();

        let mut positions = reported
            .iter()
            .map(|name| DOCUMENTED.iter().position(|known| known == name));
        assert!(
            positions.all(|position| position.is_some()),
            "every reported feature is one this facade documents: {reported:?}"
        );
        let indices: Vec<_> = reported
            .iter()
            .filter_map(|name| DOCUMENTED.iter().position(|known| known == name))
            .collect();
        assert!(
            indices.windows(2).all(|pair| pair[0] < pair[1]),
            "documented order is preserved, with no repeats: {reported:?}"
        );
    }

    /// The identity material still carries every reported feature.
    ///
    /// `cpu_identity_material` is hashed by callers, so extracting the feature
    /// list into a public function must not change what it contains.
    #[test]
    fn identity_material_still_names_each_reported_feature() {
        let material = cpu_identity_material();

        for name in cpu_compatibility_features() {
            assert!(
                material.contains(&format!("\0feature={name}")),
                "{name} is part of the identity material"
            );
        }
    }

    /// Privilege detection is stable within one process.
    ///
    /// CI containers legitimately run as root, while desktop runners normally
    /// do not. The HAL must report either host accurately rather than treating
    /// a privileged test environment as a product failure.
    #[test]
    fn current_process_privilege_is_stable() {
        let first = current_process_privilege().expect("privilege lookup must succeed");
        let second = current_process_privilege().expect("repeat lookup must succeed");
        assert_eq!(first, second);
    }

    /// Elevation detection must always answer, whatever the answer turns out
    /// to be -- the query itself failing is the only outcome a caller cannot
    /// act on.
    #[test]
    fn is_elevated_answers_without_erroring() {
        assert!(is_elevated().is_ok());
    }

    /// A CI test process has some login name and home directory, whatever the
    /// host turns out to be.
    #[test]
    fn current_user_and_home_dir_answer_on_a_normal_session() {
        assert!(current_user().is_some_and(|name| !name.is_empty()));
        assert!(home_dir().is_some_and(|dir| !dir.as_os_str().is_empty()));
    }

    /// The material is never empty and is stable within one process, and it
    /// carries the architecture and OS every caller composes it from.
    #[test]
    fn cpu_identity_material_is_present_and_stable() {
        let first = cpu_identity_material();
        assert!(!first.is_empty());
        assert_eq!(first, cpu_identity_material());
        assert!(first.contains(&format!("arch={}", std::env::consts::ARCH)));
        assert!(first.contains(&format!("os={}", std::env::consts::OS)));
    }

    /// Every host this crate supports reports at least one logical CPU.
    #[test]
    fn available_parallelism_reports_at_least_one_cpu() {
        assert!(available_parallelism().is_some_and(|count| count >= 1));
    }

    mod machine_id_sources {
        use super::super::machine_id_from;

        fn temp_dir(label: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "rp-host-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id(),
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            dir
        }

        fn write(dir: &std::path::Path, name: &str, content: &str) -> String {
            let path = dir.join(name);
            std::fs::write(&path, content).expect("write fixture file");
            path.to_string_lossy().into_owned()
        }

        #[test]
        fn machine_id_file_wins_over_boot_fallback() {
            let dir = temp_dir("wins");
            let machine = write(
                &dir,
                "machine-id",
                "  abc123
",
            );
            let boot = write(
                &dir, "boot-id", "zzz
",
            );
            assert_eq!(
                machine_id_from(&[&machine], &boot).expect("resolve"),
                "abc123"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn second_path_is_consulted_when_first_is_missing() {
            let dir = temp_dir("second");
            let missing = dir.join("absent").to_string_lossy().into_owned();
            let machine = write(
                &dir,
                "machine-id",
                "def456
",
            );
            let boot = write(
                &dir, "boot-id", "zzz
",
            );
            assert_eq!(
                machine_id_from(&[&missing, &machine], &boot).expect("resolve"),
                "def456"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn missing_machine_id_files_fall_back_to_boot_id() {
            let dir = temp_dir("fallback");
            let missing = dir.join("absent").to_string_lossy().into_owned();
            let boot = write(
                &dir,
                "boot-id",
                "boot-value
",
            );
            assert_eq!(
                machine_id_from(&[&missing], &boot).expect("resolve"),
                "boot:boot-value"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn empty_machine_id_file_falls_through_to_boot_id() {
            let dir = temp_dir("empty");
            let machine = write(
                &dir,
                "machine-id",
                "
",
            );
            let boot = write(
                &dir,
                "boot-id",
                "boot-value
",
            );
            assert_eq!(
                machine_id_from(&[&machine], &boot).expect("resolve"),
                "boot:boot-value"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn unreadable_machine_id_stays_a_hard_error_despite_boot_fallback() {
            let dir = temp_dir("unreadable");
            // A directory in the machine-id slot yields a non-NotFound read
            // error -- the split-identity hazard the hard error protects.
            let as_dir = dir.join("machine-id-dir");
            std::fs::create_dir_all(&as_dir).expect("create dir fixture");
            let as_dir = as_dir.to_string_lossy().into_owned();
            let boot = write(&dir, "boot-id", "boot-uuid\n");
            machine_id_from(&[&as_dir], &boot)
                .expect_err("unreadable machine-id must not fall through");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn everything_missing_is_an_error() {
            let dir = temp_dir("nothing");
            let missing = dir.join("absent").to_string_lossy().into_owned();
            let no_boot = dir.join("absent-boot").to_string_lossy().into_owned();
            assert!(machine_id_from(&[&missing], &no_boot).is_err());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Each identity prints the detail an operator needs to recognise it.
    #[test]
    fn privileged_identities_describe_themselves_concretely() {
        assert_eq!(
            PrivilegedIdentity::UnixRoot.to_string(),
            "root (effective uid 0)"
        );
        assert_eq!(
            PrivilegedIdentity::WindowsLocalSystem.to_string(),
            "Windows LocalSystem (S-1-5-18)"
        );
    }
}
