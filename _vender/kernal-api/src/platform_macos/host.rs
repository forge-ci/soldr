//! macOS host facts, directories, user identity, resources, and autostart.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// A privileged system identity this process may be running as.
///
/// The variants name the *answer*, not the mechanism: what a caller does with
/// "this process is the machine's system account" does not change with how the
/// host was asked. `None` from [`current_process_privilege`] means an ordinary
/// user, which is the only case most callers care to distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedIdentity {
    /// Unix effective UID 0.
    UnixRoot,
    /// Windows LocalSystem account (`S-1-5-18`).
    WindowsLocalSystem,
}

impl std::fmt::Display for PrivilegedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnixRoot => f.write_str("root (effective uid 0)"),
            Self::WindowsLocalSystem => f.write_str("Windows LocalSystem (S-1-5-18)"),
        }
    }
}

/// The privileged identity this process is running as, if any.
pub fn current_process_privilege() -> io::Result<Option<PrivilegedIdentity>> {
    Ok(privilege_from_effective_uid(unsafe { libc::geteuid() }))
}

/// Root is effective uid 0, and an ordinary uid is not root.
///
/// Kept as a pure function so the rule stays testable without being able to
/// change this process's identity.
fn privilege_from_effective_uid(euid: libc::uid_t) -> Option<PrivilegedIdentity> {
    (euid == 0).then_some(PrivilegedIdentity::UnixRoot)
}

/// A stable identity for this user on this machine.
///
/// The uid alone is not enough -- two machines both have a uid 501 -- so it is
/// paired with the platform UUID. Callers hash this; they do not parse it.
pub fn user_machine_identity() -> io::Result<String> {
    let uid = unsafe { libc::getuid() };
    let uuid = platform_uuid()?;
    Ok(format!("{uid}:{uuid}"))
}

/// This machine's `IOPlatformUUID`.
///
/// `ioreg -d2 -c IOPlatformExpertDevice` prints a block containing a line like
/// `"IOPlatformUUID" = "ABCDEF..."`. Parsing that one line is cheaper and more
/// predictable than a full plist parser for a value of this shape.
///
/// This is a fixed-argument, read-only system query with no caller input. It
/// deliberately does not route through the sanitized spawn layer: it runs
/// before any broker endpoint is bound, because the identity it produces is an
/// *input* to the endpoint name.
fn platform_uuid() -> io::Result<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-d2", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|e| io::Error::other(format!("spawn ioreg: {e}")))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "ioreg failed (status={:?})",
            output.status.code()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("\"IOPlatformUUID\"") {
            // rest looks like ` = "ABCDEF-..."`
            if let Some(eq_idx) = rest.find('=') {
                let value = rest[eq_idx + 1..].trim();
                let unquoted = value.trim_matches('"');
                if !unquoted.is_empty() {
                    return Ok(unquoted.to_string());
                }
            }
        }
    }
    Err(io::Error::other(
        "ioreg output did not contain IOPlatformUUID",
    ))
}

// ---------------------------------------------------------------------------
// Host identity facts
// ---------------------------------------------------------------------------

/// This machine's name as the host reports it.
pub fn hostname() -> Option<String> {
    let mut buf = [0_u8; 256];
    // SAFETY: `buf` is writable for its full length, which is what is passed
    // as the bound. The kernel NUL-terminates within it on success.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if ok != 0 {
        return None;
    }
    let nul = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..nul]).into_owned();
    (!name.is_empty()).then_some(name)
}

/// The filesystem device this path lives on.
pub fn filesystem_device_id(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path).ok().map(|meta| meta.dev())
}

/// A durable per-machine identifier that survives reboots.
///
/// The hardware answer is `IOPlatformUUID`, which [`user_machine_identity`]
/// already reads. This fact keeps the hostname-derived form instead: it is
/// recorded verbatim into on-disk manifests, and changing the spelling would
/// make every manifest written by an older build compare as a different
/// machine. Switching the two onto one source is a data-format change, not a
/// refactor, so it is left to whoever is willing to migrate the manifests.
pub fn machine_id() -> Option<String> {
    hostname().map(|name| format!("macos-{name}"))
}

/// An identifier that changes on every boot of this machine.
///
/// macOS has no boot uuid, so the kernel's recorded boot instant stands in for
/// one: it is fixed for the life of a boot and differs across boots.
pub fn boot_id() -> Option<String> {
    boot_time().map(|(seconds, micros)| format!("macos-boot-{seconds}-{micros}"))
}

fn boot_time() -> Option<(i64, i64)> {
    use std::ffi::CString;

    let name = CString::new("kern.boottime").expect("static sysctl name");
    // SAFETY: `boot` is a plain-old-data timeval, and an all-zero bit pattern
    // is a valid one.
    let mut boot: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::timeval>();
    // SAFETY: the name is a NUL-terminated static string alive for the call,
    // and `boot`/`len` are valid writable storage of exactly the size the
    // kernel is told to write.
    let ok = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut boot as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    let recorded = (boot.tv_sec as i64, boot.tv_usec as i64);
    (ok == 0).then_some(recorded)
}

/// macOS has no process namespaces of the kind this identity distinguishes.
pub fn namespace_id() -> Option<String> {
    None
}

/// Best-effort login name for the user running this process.
///
/// Reads `$USER`, falling back to `$LOGNAME` when unset. Neither variable is
/// guaranteed to be set -- a launchd agent can start a process with neither
/// -- so a caller that needs an identity it can rely on should use
/// [`user_machine_identity`] instead; this exists for the weaker case of
/// wanting a display name.
pub fn current_user() -> Option<String> {
    std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok())
}

/// Best-effort home directory for the user running this process.
///
/// Reads `$HOME` only. A process whose session never set it has no answer
/// here; a caller that needs a directory even then wants
/// [`crate::fs_user_data_dir`] and its siblings, which synthesize a fallback
/// path instead of reporting `None`.
pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Whether this process holds host administrator privileges.
///
/// On this host that is exactly effective UID 0 -- there is no separate
/// "elevated but not root" state the way Windows has one, so this is a thin
/// restatement of [`current_process_privilege`].
pub fn is_elevated() -> io::Result<bool> {
    Ok(current_process_privilege()?.is_some())
}

// ---------------------------------------------------------------------------
// Login environment
// ---------------------------------------------------------------------------

/// The logged-in user's environment, as a fresh login would see it.
///
/// Unix has no API that reconstructs a login environment, so this is rebuilt
/// from the user's identity rather than copied from this process: `getpwuid_r`
/// supplies `USER`/`LOGNAME`/`HOME`/`SHELL`, `PATH` gets this host's login
/// default, and the session-describing variables are carried over.
///
/// A user with no resolvable passwd entry -- a uid absent from NSS -- has no
/// identity to rebuild from, so the current process environment is returned
/// instead. That is a worse answer than a real login environment and a much
/// better one than nothing.
pub fn login_environment() -> io::Result<Vec<(OsString, OsString)>> {
    Ok(passwd_login_environment().unwrap_or_else(|| std::env::vars_os().collect()))
}

/// Unix environment variable names compare byte for byte.
pub fn environment_keys_are_case_insensitive() -> bool {
    false
}

/// Build the login environment from the passwd entry, or `None` when there is
/// no entry to build it from.
fn passwd_login_environment() -> Option<Vec<(OsString, OsString)>> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStringExt;

    // SAFETY: an all-zero `passwd` is a valid one; `getpwuid_r` fills it.
    let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // sysconf(_SC_GETPW_R_SIZE_MAX) is allowed to return -1 ("no limit");
    // 1 KiB covers real-world passwd entries and getpwuid_r reports ERANGE
    // if it does not, in which case we grow and retry.
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: `passwd`, `buf`, and `result` are all live and writable for
        // the sizes handed over, and `buf.len()` is the buffer's real length.
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut passwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        break;
    }

    let field = |ptr: *const libc::c_char| -> Option<OsString> {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: a non-null passwd field points at a NUL-terminated string
        // inside `buf`, which outlives this read.
        let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
        (!bytes.is_empty()).then(|| OsString::from_vec(bytes.to_vec()))
    };
    let name = field(passwd.pw_name)?;
    let home = field(passwd.pw_dir)?;

    let mut env: Vec<(OsString, OsString)> = vec![
        (OsString::from("USER"), name.clone()),
        (OsString::from("LOGNAME"), name),
        (OsString::from("HOME"), home),
        (OsString::from("PATH"), OsString::from(LOGIN_DEFAULT_PATH)),
    ];
    if let Some(shell) = field(passwd.pw_shell) {
        env.push((OsString::from("SHELL"), shell));
    }
    env.extend(carried_session_variables());
    Some(env)
}

/// Variables that describe the login *session* rather than this process.
///
/// Locale, timezone, and the per-user runtime/tmp dirs are set by the login
/// session (PAM/logind), not by `getpwuid_r` or by profile scripts, so a
/// reconstructed baseline can only obtain them by carrying them over. Children
/// then keep rendering text and resolving paths the way the user does.
///
/// `XDG_RUNTIME_DIR` and `TMPDIR` are the runtime-dir variables the broker's
/// own endpoint placement keys on. Dropping `XDG_RUNTIME_DIR` made a daemon
/// fall back to `/tmp` while its session-resident clients dialled
/// `$XDG_RUNTIME_DIR/…` -- every request then missed the socket
/// (zackees/soldr#2442).
fn carried_session_variables() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(key, _)| describes_the_login_session(key))
        .collect()
}

fn describes_the_login_session(key: &OsString) -> bool {
    key == "LANG"
        || key == "TZ"
        || key == "TMPDIR"
        || key == "XDG_RUNTIME_DIR"
        || key.to_str().is_some_and(|k| k.starts_with("LC_"))
}

/// The `PATH` a fresh login starts from, matching `/etc/paths` order.
const LOGIN_DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// The login environment in the double-NUL-terminated UTF-16 block form.
///
/// This shape exists because `CreateProcessW` consumes it on Windows. It is
/// provided on every host so the facade has one signature rather than one per
/// host, and because the encoding is the same data either way -- a caller
/// driving a Windows API through a cross-platform code path should not have to
/// choose between a `cfg` and a hand-rolled encoder.
pub fn login_environment_block() -> io::Result<Vec<u16>> {
    Ok(encode_environment_block(&login_environment()?))
}

/// Encode `key=value` pairs as one double-NUL-terminated UTF-16 block.
///
/// Unix environment strings are bytes, not UTF-16, so a name or value that is
/// not valid UTF-8 is encoded lossily. That is a real narrowing and it is the
/// block format's, not this function's: the format has no way to carry a byte
/// that is not a character.
fn encode_environment_block(entries: &[(OsString, OsString)]) -> Vec<u16> {
    let mut block = Vec::new();
    for (key, value) in entries {
        let entry = format!("{}={}", key.to_string_lossy(), value.to_string_lossy());
        block.extend(entry.encode_utf16());
        block.push(0);
    }
    // An empty environment is still a block: a lone terminator, never zero
    // bytes, so a consumer reading the shape finds the end where it expects to.
    block.push(0);
    block
}
/// Ordered compatibility-key feature subset this process can see.
///
/// Arch dispatch lives in the concrete host trees, not the neutral facade:
/// `target_arch` is one of the selectors the platform-boundary rule bans
/// outside them (#152). The three trees carry the same list on purpose --
/// the probe is host-independent, and duplicating it is the price of keeping
/// every `cfg` selector inside a selected tree.
///
/// Order and spellings are contractual: callers hash them, so an entry may be
/// appended but never renamed or reordered.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn cpu_compatibility_features() -> Vec<&'static str> {
    [
        ("sse2", std::arch::is_x86_feature_detected!("sse2")),
        ("sse4.2", std::arch::is_x86_feature_detected!("sse4.2")),
        ("avx", std::arch::is_x86_feature_detected!("avx")),
        ("avx2", std::arch::is_x86_feature_detected!("avx2")),
        ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
        ("fma", std::arch::is_x86_feature_detected!("fma")),
        ("bmi1", std::arch::is_x86_feature_detected!("bmi1")),
        ("bmi2", std::arch::is_x86_feature_detected!("bmi2")),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect()
}

/// No x86 compatibility-key features are reported on this architecture.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn cpu_compatibility_features() -> Vec<&'static str> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_detection_uses_effective_uid_zero() {
        assert_eq!(
            privilege_from_effective_uid(0),
            Some(PrivilegedIdentity::UnixRoot)
        );
        assert_eq!(privilege_from_effective_uid(1000), None);
    }

    /// Elevation is exactly root on this host, so it agrees with the
    /// privilege probe in both directions.
    #[test]
    fn is_elevated_agrees_with_current_process_privilege() {
        let elevated = is_elevated().expect("elevation check must succeed");
        let privileged = current_process_privilege()
            .expect("privilege lookup must succeed")
            .is_some();
        assert_eq!(elevated, privileged);
    }

    /// A test process resolves to some login name and home directory on a
    /// normal macOS host -- `$USER`/`$HOME` are set by every login session
    /// and by cargo's own test harness.
    #[test]
    fn current_user_and_home_dir_answer_on_a_normal_session() {
        assert!(current_user().is_some_and(|name| !name.is_empty()));
        assert!(home_dir().is_some_and(|dir| !dir.as_os_str().is_empty()));
    }

    /// The three string facts either answer or say they cannot; an empty
    /// string is never a valid answer, because a caller comparing two hosts
    /// would read two empties as a match.
    #[test]
    fn host_identity_facts_are_never_empty_strings() {
        let facts = [hostname(), machine_id(), boot_id(), namespace_id()];
        for value in facts.into_iter().flatten() {
            assert!(!value.is_empty(), "a reported fact must carry a value");
        }
    }

    /// This host has a name and a machine id, whatever they turn out to be.
    #[test]
    fn this_host_reports_a_name_and_a_machine_id() {
        assert!(hostname().is_some(), "a running host has a name");
        assert!(machine_id().is_some(), "a running host has a machine id");
    }

    /// The device id is a property of the volume, so a path and its parent
    /// answer alike, and a directory that exists always answers.
    #[test]
    fn filesystem_device_id_answers_for_an_existing_directory() {
        let cwd = std::env::current_dir().expect("cwd");
        let dev = filesystem_device_id(&cwd).expect("an existing directory has a device");
        assert_eq!(filesystem_device_id(&cwd), Some(dev), "stable across reads");
    }

    /// A path that does not exist has no device to report. Unlike the volume
    /// probe on Windows, there is nothing to walk up to here: the caller asked
    /// about a path, and the honest answer is that the host does not know.
    #[test]
    fn filesystem_device_id_declines_a_missing_path() {
        let missing = std::env::temp_dir().join(format!(
            "rp-host-absent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        assert_eq!(filesystem_device_id(&missing), None);
    }

    /// The reconstructed login environment carries the identity the passwd
    /// entry supplies, and a `PATH` to start from.
    #[test]
    fn login_environment_contains_identity_and_default_path() {
        let env = login_environment().unwrap();
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let user = get("USER").expect("baseline must contain USER");
        assert!(!user.is_empty());
        assert_eq!(get("LOGNAME").as_ref(), Some(&user));
        assert!(!get("HOME").expect("baseline must contain HOME").is_empty());
        assert!(!get("PATH").expect("baseline must contain PATH").is_empty());
    }

    /// A variable that exists only in this process must not survive into the
    /// login baseline -- carrying everything is what `Inherit` is for.
    #[test]
    fn login_environment_does_not_leak_arbitrary_process_vars() {
        std::env::set_var("RUNNING_PROCESS_BASELINE_CANARY", "1");
        let env = passwd_login_environment().expect("test user must have a passwd entry");
        std::env::remove_var("RUNNING_PROCESS_BASELINE_CANARY");
        assert!(
            !env.iter()
                .any(|(key, _)| key == "RUNNING_PROCESS_BASELINE_CANARY"),
            "process-local variables must not leak into the login baseline"
        );
    }

    /// The broker keys its socket path on `XDG_RUNTIME_DIR`. A baseline that
    /// drops it makes a daemon bind under `/tmp` while its session-resident
    /// clients dial `$XDG_RUNTIME_DIR/…`, stranding every request
    /// (zackees/soldr#2442).
    #[test]
    fn login_environment_carries_xdg_runtime_dir() {
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/4242");
        let env = passwd_login_environment().expect("test user must have a passwd entry");
        let carried = env
            .iter()
            .find(|(key, _)| key == "XDG_RUNTIME_DIR")
            .map(|(_, value)| value.clone());
        std::env::remove_var("XDG_RUNTIME_DIR");
        assert_eq!(
            carried.as_deref(),
            Some(std::ffi::OsStr::new("/run/user/4242")),
            "login baseline must carry XDG_RUNTIME_DIR when the session sets it"
        );
    }

    /// The carry rule is what separates a session variable from a process one,
    /// so it is asserted directly rather than only through a live environment.
    #[test]
    fn only_session_describing_variables_are_carried() {
        for carried in [
            "LANG",
            "TZ",
            "TMPDIR",
            "XDG_RUNTIME_DIR",
            "LC_ALL",
            "LC_TIME",
        ] {
            assert!(
                describes_the_login_session(&OsString::from(carried)),
                "{carried} describes the login session"
            );
        }
        for dropped in ["PWD", "OLDPWD", "SSH_AUTH_SOCK", "LCD_BRIGHTNESS", "L"] {
            assert!(
                !describes_the_login_session(&OsString::from(dropped)),
                "{dropped} belongs to this process, not the session"
            );
        }
    }

    /// The block always ends where a consumer looks for the end, including
    /// when there is nothing in it.
    #[test]
    fn an_encoded_block_is_double_nul_terminated() {
        let live = login_environment_block().expect("this host has a login environment");
        assert!(live.len() >= 2);
        assert_eq!(&live[live.len() - 2..], &[0, 0]);

        let empty = encode_environment_block(&[]);
        assert_eq!(empty, vec![0]);
    }

    /// Every variable survives the encoding, in order.
    #[test]
    fn an_encoded_block_carries_every_entry_in_order() {
        let block = encode_environment_block(&[
            (OsString::from("FIRST"), OsString::from("one")),
            (OsString::from("SECOND"), OsString::from("two")),
        ]);
        let text = String::from_utf16_lossy(&block);
        let entries: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
        assert_eq!(entries, vec!["FIRST=one", "SECOND=two"]);
    }
}
