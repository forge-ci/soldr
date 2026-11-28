//! Windows owner-only directory mechanics: explicit protected DACLs.
//!
//! Path-based, follows links; callers must control parent paths. This is not
//! a secure-open primitive or a guarantee against concurrent path replacement.
//!
//! The Linux and macOS trees answer the same two questions with mode bits.
//! Here the question is which trustees a DACL names, so "private" means a
//! DACL protected from inheritance that names only this user, SYSTEM, and
//! Administrators.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    GetNamedSecurityInfoW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL,
    DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Inheritance flags on every ACE we write: `OBJECT_INHERIT` +
/// `CONTAINER_INHERIT`, so the deployed `zccache-daemon.exe` carries the same
/// owner-only pair rather than relying on the directory alone.
const ACE_INHERIT_FLAGS: &str = "OICI";

/// `FILE_ALL_ACCESS`. Spelled as the file-object right rather than the generic
/// `GA` the pipe uses, because `icacls` and the round-tripped SDDL report a
/// file-object DACL in mapped form and an unmapped `GA` would make the
/// already-private comparison below never match its own output.
const ACE_RIGHTS: &str = "FA";

/// Trustees that may appear in a deploy-directory DACL without it counting as
/// exposed. Both the two-letter SDDL alias and the raw SID are listed because
/// which one comes back from
/// `ConvertSecurityDescriptorToStringSecurityDescriptorW` is not contractual.
const ALLOWED_TRUSTEES: &[&str] = &[
    "SY",           // NT AUTHORITY\SYSTEM
    "S-1-5-18",     //   "
    "BA",           // BUILTIN\Administrators
    "S-1-5-32-544", //   "
];

/// Number of `;`-separated fields in an SDDL ACE; the trustee is the last one.
const ACE_FIELDS: usize = 6;

/// Ensure `path` is writable only by the current user (plus SYSTEM and
/// Administrators), applying an explicit protected DACL when it is not.
///
/// Returns `Ok(false)` when the directory was already private, `Ok(true)` when
/// it was tightened, and `Err` when it is still exposed afterwards — the same
/// three outcomes the unix arm reports, so the caller's "tightened" /
/// "refused" lifecycle contract is unchanged.
pub fn ensure_dir_private(path: &Path) -> io::Result<bool> {
    // Matches the unix arm's `metadata()` probe: a missing directory is an
    // error, not a silent pass — the caller is about to deploy a binary here.
    let _ = std::fs::metadata(path)?;

    let user_sid = current_user_sid()?;
    let current = dacl_sddl(path)?;
    if is_owner_only(&current, &user_sid) {
        return Ok(false);
    }

    apply_dacl(path, &owner_only_sddl(&user_sid))?;

    // Read back rather than trusting the write. `SetNamedSecurityInfoW` can
    // report success on filesystems that do not carry ACLs at all (FAT32, some
    // network redirectors), and this is exactly the case where a false negative
    // is expensive.
    let after = dacl_sddl(path)?;
    if !is_owner_only(&after, &user_sid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is writable by other local users (DACL {after}) and could not be tightened",
                path.display()
            ),
        ));
    }
    Ok(true)
}

/// Create `path` with the owner-only DACL already on it, creating any missing
/// parents the same way.
///
/// #1172 residual: `ensure_dir_private` can only tighten a directory that
/// already exists, so every caller had the shape "create with whatever the
/// parent hands down, then fix it". Between those two steps the directory is
/// live with the inherited ACL. Under `%USERPROFILE%` that inheritance is
/// already narrow and the window is harmless, but the relocated-root case this
/// module exists for (`ZCCACHE_CACHE_DIR` on `C:\ProgramData\…` or a volume
/// root) inherits `BUILTIN\Users:(OI)(CI)(M)` — and there another local user
/// can win the race and populate the directory the daemon binary is about to
/// be deployed into.
///
/// The unix arm never had this gap: `DirBuilder::mode(0o700)` passes the mode
/// to `mkdir(2)` itself, so no directory is ever briefly group-writable. This
/// is the Windows equivalent — the descriptor goes to `CreateDirectoryW` in
/// `SECURITY_ATTRIBUTES`, so the directory is never visible with any other
/// DACL on ACL-supporting filesystems. Call ensure_dir_private afterwards
/// to verify the resulting restriction, including on pre-existing directories.
///
/// Already-existing directories are left to `ensure_dir_private`: this only
/// closes the window for directories *it* creates. `Ok(())` when the path
/// exists as a directory already, so it composes like `create_dir_all`.
pub fn create_dir_all_private(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_all_private(parent)?;
        }
    }
    let user_sid = current_user_sid()?;
    match create_dir_with_dacl(path, &owner_only_sddl(&user_sid)) {
        Ok(()) => Ok(()),
        // Match create_dir_all's existing-directory behavior. The winner's
        // descriptor is not known; callers must ensure_dir_private afterwards.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// `CreateDirectoryW` with `sddl` supplied at creation time.
fn create_dir_with_dacl(path: &Path, sddl: &str) -> io::Result<()> {
    let wide_path = wide(path)?;
    let wide_sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide_sddl` is NUL-terminated and outlives the call; on success
    // Windows allocates `descriptor`, released below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || descriptor.is_null() {
        return Err(io::Error::last_os_error());
    }

    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.cast(),
        bInheritHandle: 0,
    };
    // SAFETY: `wide_path` is NUL-terminated and `attributes` borrows the live
    // descriptor freed below. The kernel copies the descriptor into the new
    // object, so releasing ours afterwards is sound.
    let created = unsafe { CreateDirectoryW(wide_path.as_ptr(), &attributes) };
    let result = if created == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };

    // SAFETY: `descriptor` came from the SDDL conversion above and is freed
    // exactly once, after the last use of `attributes`.
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
}

/// The protected owner-only DACL written to an exposed directory.
fn owner_only_sddl(user_sid: &str) -> String {
    format!("D:P(A;{ACE_INHERIT_FLAGS};{ACE_RIGHTS};;;{user_sid})(A;{ACE_INHERIT_FLAGS};{ACE_RIGHTS};;;SY)")
}

/// Is this DACL protected from inheritance *and* free of any trustee other
/// than the deploying user, SYSTEM, and Administrators?
///
/// Both halves matter. An unprotected DACL whose current aces happen to be
/// narrow is one `icacls` reset — or one move to a differently-permissioned
/// parent — away from re-acquiring `BUILTIN\Users`, so it is reported as
/// needing repair rather than accepted.
fn is_owner_only(sddl: &str, user_sid: &str) -> bool {
    let Some(rest) = sddl.strip_prefix("D:") else {
        return false;
    };
    let flags = rest.split('(').next().unwrap_or_default();
    if !flags.contains('P') {
        return false;
    }
    // A NULL DACL grants everyone everything and renders as a flag word, with
    // no aces to iterate — the `P` check above already rejects it, but the
    // ace loop must not be read as "no aces means private".
    if rest.contains("NO_ACCESS_CONTROL") {
        return false;
    }
    aces(rest).all(|trustee| trustee_is_self_or_admin(trustee, user_sid))
}

/// Is this ACE trustee the running user, SYSTEM, or Administrators?
///
/// Compares **SIDs, not strings**. `ConvertSecurityDescriptorToStringSecurityDescriptorW`
/// substitutes a two-letter alias for well-known SIDs, and which SIDs count as
/// "well-known" is not something the caller controls: a process running as the
/// built-in Administrator gets its own account rendered as `LA`, so a raw-SID
/// string comparison reports the directory as exposed *after we just tightened
/// it*, and the read-back check then fails a DACL that is in fact correct.
///
/// CI found this — the Windows runner runs as that account and my dev host
/// does not.
fn trustee_is_self_or_admin(trustee: &str, user_sid: &str) -> bool {
    if trustee == user_sid || ALLOWED_TRUSTEES.contains(&trustee) {
        return true;
    }
    // Resolve both sides; `ConvertStringSidToSidW` accepts an alias as happily
    // as a raw SID, which is exactly the normalization the string compare
    // above lacks.
    match (native_sid(trustee), native_sid(user_sid)) {
        (Some(lhs), Some(rhs)) => {
            // SAFETY: both allocations contain valid, natively aligned SIDs
            // from the conversion API and remain alive throughout the call.
            unsafe { EqualSid(lhs.0, rhs.0) != 0 }
        }
        _ => false,
    }
}

/// Keep the conversion API's aligned allocation until its last native use.
struct NativeSid(windows_sys::Win32::Security::PSID);

impl Drop for NativeSid {
    fn drop(&mut self) {
        // SAFETY: constructed only from a successful LocalAlloc-backed SID
        // conversion and never copied; released exactly once.
        unsafe { LocalFree(self.0.cast::<std::ffi::c_void>()) };
    }
}

/// Resolve the binary SID an SDDL trustee denotes, alias or raw.
fn native_sid(trustee: &str) -> Option<NativeSid> {
    if trustee.contains('\0') {
        return None;
    }
    let wide: Vec<u16> = trustee.encode_utf16().chain(std::iter::once(0)).collect();
    let mut psid: windows_sys::Win32::Security::PSID = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; on success the SID is LocalAlloc'd and
    // freed below on every path.
    if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut psid) } == 0 || psid.is_null() {
        return None;
    }
    Some(NativeSid(psid))
}

/// Yield the trustee field of every ACE in an SDDL DACL body.
fn aces(body: &str) -> impl Iterator<Item = &str> {
    body.split('(').skip(1).filter_map(|ace| {
        let ace = ace.split(')').next()?;
        let fields: Vec<&str> = ace.split(';').collect();
        (fields.len() >= ACE_FIELDS).then(|| fields[ACE_FIELDS - 1])
    })
}

/// UTF-16, NUL-terminated — what every `*W` entry point below expects.
fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    value.push(0);
    Ok(value)
}

/// String SID of the user this process runs as.
fn current_user_sid() -> io::Result<String> {
    let mut token: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
    // release; `token` is a valid out-pointer we close below on every path.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = token_user_sid(token);
    // SAFETY: `token` was opened above and is closed exactly once here.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };
    result
}

fn token_user_sid(token: windows_sys::Win32::Foundation::HANDLE) -> io::Result<String> {
    let mut needed: u32 = 0;
    // SAFETY: the first call is the documented size probe — a null buffer with
    // zero length is expected to fail with ERROR_INSUFFICIENT_BUFFER and fill
    // `needed`.
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    // TOKEN_USER contains a pointer, so retain native pointer alignment for
    // the API's output buffer as well as sufficient byte capacity.
    let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; words];
    // SAFETY: `buffer` is aligned, at least `needed` bytes, and outlives the call.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success Windows wrote a TOKEN_USER at the head of `buffer`,
    // whose `Sid` points into that same allocation and stays valid while
    // `buffer` lives.
    let sid = unsafe {
        std::ptr::read_unaligned(buffer.as_ptr().cast::<TOKEN_USER>())
            .User
            .Sid
    };

    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` is the live SID above; `text` is a valid out-pointer that
    // Windows fills with a LocalAlloc'd string we free below.
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut text) };
    if ok == 0 || text.is_null() {
        return Err(io::Error::last_os_error());
    }
    let sid_string = wide_to_string(text);
    // SAFETY: `text` came from ConvertSidToStringSidW, whose documented
    // release is LocalFree, and is freed exactly once.
    unsafe { LocalFree(text as HLOCAL) };
    Ok(sid_string)
}

/// Read `path`'s DACL back as SDDL. This is the honest observation the
/// tightening decision and the tests are both made from — never the constant
/// we passed in.
fn dacl_sddl(path: &Path) -> io::Result<String> {
    let wide_path = wide(path)?;
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();

    // SAFETY: `wide_path` is NUL-terminated and outlives the call; all
    // out-pointers are valid. On success Windows allocates `descriptor` and we
    // release it below with the documented LocalFree.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `descriptor` is the descriptor just returned and still live.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || text.is_null() {
        let err = io::Error::last_os_error();
        // SAFETY: `descriptor` is live and freed exactly once on this path.
        unsafe { LocalFree(descriptor as HLOCAL) };
        return Err(err);
    }
    let sddl = wide_to_string(text);
    // SAFETY: both allocations came from Win32 LocalAlloc-family calls and are
    // each freed exactly once here.
    unsafe {
        LocalFree(text as HLOCAL);
        LocalFree(descriptor as HLOCAL);
    }
    Ok(sddl)
}

/// Install `sddl`'s DACL on `path`, protected from inheritance.
fn apply_dacl(path: &Path, sddl: &str) -> io::Result<()> {
    let mut wide_path = wide(path)?;
    let wide_sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide_sddl` is NUL-terminated and outlives the call; on success
    // Windows allocates `descriptor`, released below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || descriptor.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut present: i32 = 0;
    let mut defaulted: i32 = 0;
    // SAFETY: `descriptor` is the live descriptor above; `dacl` borrows from
    // it and stays valid until the LocalFree below.
    let ok =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    let result = if ok == 0 || present == 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `wide_path` is a NUL-terminated mutable buffer and `dacl`
        // points into the descriptor that is still live for this call. The
        // DACL is copied into the object's security descriptor by the kernel,
        // so releasing ours afterwards is sound.
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    };

    // SAFETY: `descriptor` came from the SDDL conversion above and is freed
    // exactly once, after the last use of the `dacl` pointer into it.
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
}

/// Copy a NUL-terminated Windows string out of a Win32 allocation.
fn wide_to_string(text: *const u16) -> String {
    let mut len = 0usize;
    // SAFETY: `text` is a NUL-terminated buffer produced by Win32; the walk
    // stops at that terminator and never reads past it.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units precede the terminator found above.
    let slice = unsafe { std::slice::from_raw_parts(text, len) };
    String::from_utf16_lossy(slice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_nul_is_rejected_without_touching_the_prefix_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("prefix\0suffix");
        let user = current_user_sid().unwrap();
        let error = create_dir_with_dacl(&path, &owner_only_sddl(&user)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!temp.path().join("prefix").exists());
    }

    #[test]
    fn protected_trustee_policy_rejects_inheritance_and_everyone() {
        let user = "S-1-5-21-1-2-3-1001";
        assert!(is_owner_only(&owner_only_sddl(user), user));
        assert!(is_owner_only("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)", user));
        assert!(!is_owner_only("D:(A;OICI;FA;;;SY)", user));
        assert!(!is_owner_only("D:P(A;OICI;FA;;;WD)", user));
        assert!(!is_owner_only("D:PNO_ACCESS_CONTROL", user));
    }

    #[test]
    fn system_alias_and_raw_sid_compare_as_the_same_trustee() {
        assert!(trustee_is_self_or_admin("SY", "S-1-5-18"));
        assert!(trustee_is_self_or_admin("S-1-5-18", "SY"));
        assert!(!trustee_is_self_or_admin("WD", "S-1-5-18"));
    }

    #[test]
    fn native_sid_normalization_retains_valid_aligned_allocations() {
        let alias = native_sid("SY").expect("SYSTEM alias");
        let raw = native_sid("S-1-5-18").expect("SYSTEM SID");
        // SAFETY: both NativeSid guards retain valid conversion allocations.
        assert_ne!(unsafe { EqualSid(alias.0, raw.0) }, 0);
        assert!(native_sid("not-a-sid").is_none());
        assert!(native_sid("SY\0WD").is_none());
    }

    #[test]
    fn creation_persists_protected_dacl_and_tightening_is_verified() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private");
        create_dir_all_private(&path).unwrap();
        let user = current_user_sid().unwrap();
        assert!(is_owner_only(&dacl_sddl(&path).unwrap(), &user));
        assert!(!ensure_dir_private(&path).unwrap());
        // Change only this test-owned directory. Keep the current owner and
        // SYSTEM ACEs so the test can repair and remove the directory.
        let exposed = format!("{}(A;OICI;FA;;;WD)", owner_only_sddl(&user));
        apply_dacl(&path, &exposed).unwrap();
        let observed = dacl_sddl(&path).unwrap();
        let tightened = ensure_dir_private(&path).unwrap();
        assert!(!is_owner_only(&observed, &user));
        assert!(tightened);
        assert!(is_owner_only(&dacl_sddl(&path).unwrap(), &user));
    }
}
