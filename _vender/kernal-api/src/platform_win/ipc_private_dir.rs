#[cfg(feature = "ipc")]
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
#[cfg(any(feature = "fs", feature = "ipc"))]
use std::fs::File;
#[cfg(any(feature = "fs", feature = "ipc"))]
use std::os::windows::io::AsRawHandle as _;

#[cfg(feature = "ipc")]
use crate::platform::ipc::OwnerPrivateDirectoryOutcome;

/// Protected, inheritable current-user-and-SYSTEM DACL for private IPC directories.
///
/// OICI is required because applying a protected DACL re-propagates inherited
/// ACEs through existing descendants. The earlier non-inheritable policy could
/// leave descendants with an empty DACL, including files with hardlinks outside
/// the directory. Reapplying this policy repairs that legacy state.
#[cfg(any(feature = "ipc", feature = "fs"))]
fn private_dir_sddl() -> io::Result<String> {
    Ok(format!(
        "D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)",
        current_user_sid_sddl()?
    ))
}

#[cfg(feature = "ipc")]
pub fn ensure_owner_private_directory(path: &Path) -> io::Result<OwnerPrivateDirectoryOutcome> {
    fs::create_dir_all(path)?;
    // Avoid an expensive recursive DACL propagation on every warm manifest
    // write while still forcing legacy/non-inheritable policies through repair.
    if owner_private_directory(path).unwrap_or(false) {
        return Ok(OwnerPrivateDirectoryOutcome::AlreadyPrivate);
    }
    apply_protected_dacl_sddl(path, &private_dir_sddl()?)?;
    if owner_private_directory(path)? {
        Ok(OwnerPrivateDirectoryOutcome::Hardened)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "private-directory permissions were not applied to {}",
                path.display()
            ),
        ))
    }
}

pub fn owner_private_directory(path: &Path) -> io::Result<bool> {
    let actual = file_security_descriptor(path)?;
    if !actual.dacl_is_protected()? {
        return Ok(false);
    }
    // `OW` follows the file owner's SID.  An elevated token may otherwise
    // create a directory owned by the Administrators group, which would make
    // every member of that group an Owner Rights principal.  A private
    // directory is private to this token's user SID, not to its default-owner
    // group.
    if actual.owner_sid_bytes()? != current_user_sid_bytes()? {
        return Ok(false);
    }
    // Binary equality covers ACL revision, ACE flags/masks/SIDs/order and
    // callback or object payloads that SDDL substring checks can misclassify.
    let expected = LocalSecurityDescriptor::from_sddl(&private_dir_sddl()?)?;
    Ok(actual.dacl()?.bytes()? == expected.dacl()?.bytes()?)
}

/// Validate the confidentiality policy of an already-open regular file.
///
/// This uses `GetSecurityInfo` on the handle rather than reopening the path:
/// a rename or reparse-point swap cannot change the object whose DACL is
/// checked. We accept only the exact current-user/SYSTEM full-control
/// ACL forms Windows creates directly or by inheriting `private_dir_sddl()`.
/// Every other ACE kind, principal, mask, order, size, or callback payload
/// fails closed.
#[cfg(feature = "fs")]
pub(super) fn opened_file_is_current_user_private(file: &File) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let current_sid = current_user_sid_bytes()?;
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `file` owns a live handle; all output pointers refer to writable
    // locals. On success the descriptor is a LocalFree allocation adopted
    // immediately below, so no native allocation escapes this function.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as _,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if descriptor.is_null() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file security descriptor is incomplete"));
    }
    let actual = LocalSecurityDescriptor(descriptor);
    let is_private = dacl_is_exact_user_system_file_policy(
        &actual.dacl()?.bytes()?,
        &current_sid,
    );
    #[cfg(test)]
    if !is_private {
        eprintln!("private file DACL does not match the current-user/SYSTEM policy");
    }
    Ok(is_private)
}

#[cfg(feature = "fs")]
fn dacl_is_exact_user_system_file_policy(dacl: &[u8], current_sid: &[u8]) -> bool {
    // ACL header: revision, reserved, byte size, ACE count, reserved.
    if dacl.len() < 8
        || dacl[0] != 2
        || u16::from_le_bytes([dacl[2], dacl[3]]) as usize != dacl.len()
        || u16::from_le_bytes([dacl[4], dacl[5]]) != 2
    {
        return false;
    }
    const SYSTEM_SID: &[u8] = &[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
    let Some(next) = exact_full_control_ace(dacl, 8, current_sid) else {
        return false;
    };
    exact_full_control_ace(dacl, next, SYSTEM_SID).is_some_and(|end| end == dacl.len())
}

#[cfg(feature = "fs")]
fn exact_full_control_ace(dacl: &[u8], offset: usize, principal: &[u8]) -> Option<usize> {
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const INHERITED_ACE: u8 = 0x10;
    const OBJECT_INHERIT_ACE: u8 = 0x01;
    const CONTAINER_INHERIT_ACE: u8 = 0x02;
    const FULL_CONTROL: u32 = 0x001f_01ff;

    let header_end = offset.checked_add(8)?;
    if header_end > dacl.len() || dacl[offset] != ACCESS_ALLOWED_ACE_TYPE {
        return None;
    }
    let flags = dacl[offset + 1];
    // Direct files use no inheritance flags. Windows may retain the parent's
    // OI|CI flags when materializing an inherited file ACE, always with ID.
    if flags != 0
        && flags != INHERITED_ACE
        && flags != (INHERITED_ACE | OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE)
    {
        return None;
    }
    let ace_size = u16::from_le_bytes([dacl[offset + 2], dacl[offset + 3]]) as usize;
    if ace_size != 8 + principal.len() {
        return None;
    }
    let end = offset.checked_add(ace_size)?;
    if end > dacl.len()
        || u32::from_le_bytes([
            dacl[offset + 4],
            dacl[offset + 5],
            dacl[offset + 6],
            dacl[offset + 7],
        ]) != FULL_CONTROL
        || &dacl[header_end..end] != principal
    {
        return None;
    }
    Some(end)
}

fn current_user_sid_bytes() -> io::Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, IsValidSid, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    // SAFETY: output pointer is valid; returned token is closed before return.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    struct Token(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: this is the unique successful OpenProcessToken handle.
            unsafe { CloseHandle(self.0); }
        }
    }
    let _token = Token(token);
    let mut needed = 0;
    // SAFETY: documented size query with null buffer.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 { return Err(io::Error::last_os_error()); }
    let mut buffer = vec![0_u8; needed as usize];
    // SAFETY: buffer has exactly the requested capacity and token remains live.
    if unsafe { GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: TOKEN_USER may be only byte-aligned in the Vec; read_unaligned
    // copies its pointer field without creating an aligned reference.
    let sid = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<TOKEN_USER>()) }.User.Sid;
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 { return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid current-user SID")); }
    let length = unsafe { GetLengthSid(sid) } as usize;
    if length == 0 || length > 1024 { return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible current-user SID length")); }
    // SAFETY: IsValidSid and GetLengthSid above validated this live token SID.
    Ok(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), length).to_vec() })
}

#[cfg(any(feature = "fs", feature = "ipc"))]
fn current_user_sid_sddl() -> io::Result<String> {
    let bytes = current_user_sid_bytes()?;
    let revision = *bytes
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty current-user SID"))?;
    let sub_authority_count = *bytes
        .get(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated current-user SID"))?
        as usize;
    let expected_len = 8 + sub_authority_count * 4;
    if bytes.len() != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid current-user SID length",
        ));
    }
    let mut authority = 0_u64;
    for byte in &bytes[2..8] {
        authority = (authority << 8) | u64::from(*byte);
    }
    let mut sddl = format!("S-{revision}-{authority}");
    for index in 0..sub_authority_count {
        let offset = 8 + index * 4;
        let sub_authority = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        sddl.push('-');
        sddl.push_str(&sub_authority.to_string());
    }
    Ok(sddl)
}

#[cfg(feature = "ipc")]
fn apply_protected_dacl_sddl(path: &Path, sddl: &str) -> io::Result<()> {
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;

    apply_dacl_sddl(path, sddl, PROTECTED_DACL_SECURITY_INFORMATION)
}

/// Open a directory for DACL inspection and update without demanding an owner
/// change that ordinary owners are not entitled to make.
///
/// Windows grants an object owner `READ_CONTROL` and `WRITE_DAC` implicitly,
/// but not `WRITE_OWNER`. Request only the former two rights on the normal
/// path; ownership correction reopens with `WRITE_OWNER` below only when the
/// live object actually needs it.
#[cfg(feature = "ipc")]
fn open_directory_for_dacl_update(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
    use winapi::um::winnt::{READ_CONTROL, WRITE_DAC};

    std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Reopen only after inspection showed that assigning TokenUser is necessary.
/// Both mutating calls then target this one handle, so a pathname race cannot
/// split the owner and DACL updates across different directories.
#[cfg(feature = "ipc")]
fn open_directory_for_owner_and_dacl_update(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
    use winapi::um::winnt::{READ_CONTROL, WRITE_DAC, WRITE_OWNER};

    std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC | WRITE_OWNER)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Compare the owner of this live handle to the caller's TokenUser SID.
///
/// This deliberately requests no pathname metadata. The caller may use the
/// answer to decide whether `WRITE_OWNER` is required, and it must recheck
/// after opening the stronger handle before mutating a mismatched object.
#[cfg(feature = "ipc")]
fn opened_owner_is_current_user(file: &File) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        EqualSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let mut owner: PSID = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `file` owns a live handle with READ_CONTROL; every output
    // pointer is a writable local. On success the descriptor is adopted below
    // and freed by `LocalSecurityDescriptor` before this function returns.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as _,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if descriptor.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory security query returned a null descriptor",
        ));
    }
    let _descriptor = LocalSecurityDescriptor(descriptor);
    if owner.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory security descriptor has no owner",
        ));
    }
    let current_sid = current_user_sid_bytes()?;
    // SAFETY: `owner` is borrowed from `_descriptor`, which remains live, and
    // `current_sid` is the valid SID copied from the live process token.
    Ok(unsafe { EqualSid(owner, current_sid.as_ptr().cast_mut().cast()) } != 0)
}

/// Assign the current token user as an open file's owner without accepting
/// its default owner SID (which can be an elevated local group).
///
/// This operates on the caller's handle, rather than reopening its pathname:
/// the object whose owner changes is necessarily the one the caller created
/// and still holds. The handle must have `WRITE_OWNER` access.
#[cfg(any(feature = "fs", feature = "ipc"))]
pub(super) fn apply_current_user_owner(file: &File) -> io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;

    let current_sid = current_user_sid_bytes()?;
    // SAFETY: `file` owns a live handle with WRITE_OWNER access, and the
    // current-user SID was copied from a live process token. The API consumes
    // neither buffer and changes the handle's object, never a path lookup.
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle() as _,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            current_sid.as_ptr().cast_mut().cast(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

/// Bind the effective private-file DACL to an already-created file handle.
///
/// The DACL is protected and non-inheriting: current user and SYSTEM only. A
/// file created below the private directory would normally inherit effective
/// owner and SYSTEM ACEs; this instead pins the current token user's concrete
/// SID. An `OWNER RIGHTS` ACE is not equivalent here: its stored SID does not
/// prove which principal owns a file when the reader validates it later.
///
/// The caller must first assign TokenUser as owner on this same handle. This
/// avoids a path reopen and binds the concrete user SID rather than an
/// elevated token's default-owner group.
#[cfg(feature = "fs")]
pub(super) fn apply_current_user_private_file_dacl(file: &File) -> io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    let descriptor = LocalSecurityDescriptor::from_sddl(&format!(
        "D:P(A;;FA;;;{})(A;;FA;;;SY)",
        current_user_sid_sddl()?
    ))?;
    let dacl = descriptor.dacl()?;
    // SAFETY: `file` is the still-open newly-created object, opened with
    // WRITE_DAC by the caller. `dacl` borrows `descriptor`, which stays live
    // through this synchronous call; no pathname is resolved.
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle() as _,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl.as_ptr(),
            std::ptr::null_mut(),
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

#[cfg(feature = "ipc")]
fn apply_dacl_sddl(
    path: &Path,
    sddl: &str,
    inheritance_control: windows_sys::Win32::Security::OBJECT_SECURITY_INFORMATION,
) -> io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    let file = open_directory_for_dacl_update(path)?;
    let file = if opened_owner_is_current_user(&file)? {
        file
    } else {
        drop(file);
        let file = open_directory_for_owner_and_dacl_update(path)?;
        // The first inspection intentionally carries no authority across this
        // reopen. Recheck the stronger handle, then bind both mutating calls
        // to exactly that live object.
        if !opened_owner_is_current_user(&file)? {
            apply_current_user_owner(&file)?;
        }
        file
    };
    let descriptor = LocalSecurityDescriptor::from_sddl(sddl)?;
    let dacl = descriptor.dacl()?;
    // SAFETY: `file` pins the object updated above and has WRITE_DAC access;
    // `dacl` borrows the live descriptor for this call. All unused
    // owner/group/SACL pointers are null. When ownership differed, TokenUser
    // was assigned on this same handle first, so `OW` is a single-user
    // principal even for an elevated token whose default owner is a group.
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle() as _,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | inheritance_control,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl.as_ptr(),
            std::ptr::null_mut(),
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

fn file_security_descriptor(path: &Path) -> io::Result<LocalSecurityDescriptor> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and `descriptor` is a writable out
    // pointer. The successful result is checked for null before RAII adoption.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if descriptor.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "GetNamedSecurityInfoW returned a null security descriptor",
        ));
    }
    Ok(LocalSecurityDescriptor(descriptor))
}

struct LocalSecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

impl LocalSecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };

        let wide = sddl
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `wide` is a NUL-terminated SDDL string and `descriptor` is a
        // writable out pointer. A successful non-null allocation is adopted
        // exactly once by `LocalSecurityDescriptor` and freed in `Drop`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || descriptor.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(descriptor))
        }
    }

    fn dacl_is_protected(&self) -> io::Result<bool> {
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorControl, SECURITY_DESCRIPTOR_CONTROL, SE_DACL_PROTECTED,
        };

        let mut control: SECURITY_DESCRIPTOR_CONTROL = 0;
        let mut revision = 0;
        // SAFETY: `self.0` is a live descriptor owned by `self`; both output
        // pointers refer to initialized writable locals.
        let ok = unsafe { GetSecurityDescriptorControl(self.0, &mut control, &mut revision) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(control & SE_DACL_PROTECTED != 0)
    }

    fn dacl(&self) -> io::Result<SecurityDescriptorDacl<'_>> {
        use windows_sys::Win32::Security::{GetSecurityDescriptorDacl, ACL};

        let mut present = 0;
        let mut defaulted = 0;
        let mut pointer: *mut ACL = std::ptr::null_mut();
        // SAFETY: `self.0` is a live descriptor owned by `self`; all output
        // pointers refer to writable locals. The returned borrow is tied to
        // `self`, so the descriptor outlives every use of its DACL pointer.
        let ok = unsafe {
            GetSecurityDescriptorDacl(self.0, &mut present, &mut pointer, &mut defaulted)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let pointer = std::ptr::NonNull::new(pointer).ok_or_else(|| {
            io::Error::new(io::ErrorKind::PermissionDenied, "directory has no DACL")
        })?;
        Ok(SecurityDescriptorDacl {
            pointer,
            descriptor: std::marker::PhantomData,
        })
    }

    fn owner_sid_bytes(&self) -> io::Result<Vec<u8>> {
        use windows_sys::Win32::Security::{
            GetLengthSid, GetSecurityDescriptorOwner, IsValidSid, PSID,
        };

        let mut owner: PSID = std::ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: `self.0` is a live descriptor; both output pointers are
        // writable locals. The returned SID remains owned by the descriptor.
        if unsafe { GetSecurityDescriptorOwner(self.0, &mut owner, &mut defaulted) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if owner.is_null() || unsafe { IsValidSid(owner) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "security descriptor has no valid owner SID",
            ));
        }
        let length = unsafe { GetLengthSid(owner) } as usize;
        if length == 0 || length > 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "implausible security descriptor owner SID length",
            ));
        }
        // SAFETY: the validated SID is borrowed from the live descriptor.
        Ok(unsafe { std::slice::from_raw_parts(owner.cast::<u8>(), length).to_vec() })
    }
}

struct SecurityDescriptorDacl<'descriptor> {
    pointer: std::ptr::NonNull<windows_sys::Win32::Security::ACL>,
    descriptor: std::marker::PhantomData<&'descriptor LocalSecurityDescriptor>,
}

impl SecurityDescriptorDacl<'_> {
    fn as_ptr(&self) -> *mut windows_sys::Win32::Security::ACL {
        self.pointer.as_ptr()
    }

    fn bytes(&self) -> io::Result<Vec<u8>> {
        use windows_sys::Win32::Security::{
            AclSizeInformation, GetAclInformation, ACL_SIZE_INFORMATION,
        };

        let mut information = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: `self.pointer` is a checked non-null DACL borrowed from a
        // live descriptor; the information buffer and size match the selected
        // ACL information class.
        let ok = unsafe {
            GetAclInformation(
                self.as_ptr(),
                (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let byte_len = information.AclBytesInUse as usize;
        if byte_len < std::mem::size_of::<windows_sys::Win32::Security::ACL>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DACL byte length is smaller than its header",
            ));
        }
        // SAFETY: GetAclInformation validated this live DACL and reported the
        // bytes in use. The slice is copied immediately while its descriptor
        // owner remains alive, so no borrowed native memory escapes.
        Ok(unsafe { std::slice::from_raw_parts(self.as_ptr().cast::<u8>(), byte_len).to_vec() })
    }
}

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the unique non-null allocation returned by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW or
        // GetNamedSecurityInfoW. Both APIs transfer LocalFree ownership, and
        // the allocation is released exactly once here.
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.0.cast());
        }
    }
}

#[cfg(all(test, feature = "ipc"))]
mod tests {
    use std::fs::{self, File};

    use super::*;

    #[test]
    fn dacl_update_handle_opens_and_reads_a_new_directory_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir(&directory).unwrap();

        // Regression for GitHub-hosted Windows: an owner gets WRITE_DAC, not
        // implicit WRITE_OWNER. DACL hardening must therefore inspect the
        // ordinary handle before asking for an owner-changing handle. An
        // elevated token may create the directory with a group default owner,
        // so both owner answers are valid here; successful inspection is the
        // invariant this path needs before conditionally escalating access.
        let handle = open_directory_for_dacl_update(&directory).unwrap();
        let _is_current_user_owner = opened_owner_is_current_user(&handle).unwrap();
    }

    #[test]
    fn ensure_private_dir_is_a_noop_for_an_already_private_populated_tree() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        ensure_owner_private_directory(&directory).unwrap();
        for index in 0..1_500 {
            let shard = directory.join(format!("shard-{index:04}"));
            fs::create_dir(&shard).unwrap();
            fs::write(shard.join("artifact.bin"), b"payload").unwrap();
        }

        assert_eq!(
            ensure_owner_private_directory(&directory).unwrap(),
            OwnerPrivateDirectoryOutcome::AlreadyPrivate
        );
        File::open(directory.join("shard-1499/artifact.bin")).unwrap();
    }

    #[test]
    fn nonstandard_two_ace_policy_is_rejected_and_repaired() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir_all(&directory).unwrap();
        apply_protected_dacl_sddl(
            &directory,
            &format!(
                "D:P(A;OICI;FA;;;SY)(OA;OICI;FA;;;{})",
                current_user_sid_sddl().unwrap()
            ),
        )
        .unwrap();

        assert!(!owner_private_directory(&directory).unwrap());
        assert_eq!(
            ensure_owner_private_directory(&directory).unwrap(),
            OwnerPrivateDirectoryOutcome::Hardened
        );
        assert!(owner_private_directory(&directory).unwrap());
    }

    #[test]
    fn interactive_principal_dacl_is_rejected_and_repaired() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir_all(&directory).unwrap();
        apply_protected_dacl_sddl(&directory, "D:P(A;OICI;FA;;;IU)(A;OICI;FA;;;SY)").unwrap();

        assert!(!owner_private_directory(&directory).unwrap());
        assert_eq!(
            ensure_owner_private_directory(&directory).unwrap(),
            OwnerPrivateDirectoryOutcome::Hardened
        );
        assert!(owner_private_directory(&directory).unwrap());
    }

    #[test]
    fn unprotected_identical_acl_is_rejected_and_repaired() {
        use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;

        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir_all(&directory).unwrap();
        // Establish the production owner and DACL first. A raw test-created
        // directory may retain an elevated token's default-owner group, which
        // does not imply WRITE_DAC for TokenUser once its inherited DACL is
        // replaced. This test is about the protected-DACL bit, not that
        // unrelated elevated-owner edge case.
        ensure_owner_private_directory(&directory).unwrap();
        let private_sddl = private_dir_sddl().unwrap();
        apply_dacl_sddl(
            &directory,
            &private_sddl,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
        )
        .unwrap();
        let unprotected = file_security_descriptor(&directory).unwrap();
        assert!(!unprotected.dacl_is_protected().unwrap());
        // Clearing protection causes Windows to materialize inheritable ACEs
        // from the parent, so the byte representation need not remain equal.
        // The policy decision is the protection bit and its subsequent repair.
        assert!(!owner_private_directory(&directory).unwrap());
        assert_eq!(
            ensure_owner_private_directory(&directory).unwrap(),
            OwnerPrivateDirectoryOutcome::Hardened
        );
    }

    #[test]
    fn ensure_private_dir_keeps_existing_children_accessible() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        let child = directory.join("nested");
        fs::create_dir_all(&child).unwrap();
        let file = child.join("service.bin");
        fs::write(&file, b"payload").unwrap();

        ensure_owner_private_directory(&directory).unwrap();

        File::open(&file).unwrap();
        fs::write(&file, b"payload2").unwrap();
        fs::read_dir(&child).unwrap();
    }

    #[test]
    fn ensure_private_dir_does_not_brick_hardlinked_files_outside() {
        let temporary = tempfile::tempdir().unwrap();
        let outside = temporary.path().join("outside.bin");
        fs::write(&outside, b"binary").unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir_all(&directory).unwrap();
        fs::hard_link(&outside, directory.join("inside.bin")).unwrap();

        ensure_owner_private_directory(&directory).unwrap();

        File::open(&outside).unwrap();
        fs::write(&outside, b"binary2").unwrap();
    }

    #[test]
    fn legacy_non_inheritable_dacl_is_rejected_and_healed_by_reapply() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        let file = directory.join("service.bin");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&file, b"payload").unwrap();

        apply_protected_dacl_sddl(
            &directory,
            &format!("D:P(A;;FA;;;{})", current_user_sid_sddl().unwrap()),
        )
        .unwrap();
        assert!(!owner_private_directory(&directory).unwrap());
        assert!(File::open(&file).is_err());
        assert_eq!(
            ensure_owner_private_directory(&directory).unwrap(),
            OwnerPrivateDirectoryOutcome::Hardened
        );
        File::open(&file).unwrap();
    }

    #[cfg(feature = "fs")]
    #[test]
    fn direct_private_child_is_accepted_but_explicitly_permissive_child_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        ensure_owner_private_directory(&directory).unwrap();
        let child = directory.join("marker");
        // An elevated token can default file ownership to Administrators.
        // Exercise the production creator: it pins TokenUser ownership and
        // applies the effective owner/SYSTEM policy on that same handle.
        let mut child_file = crate::platform::fs::create_private_file(&child).unwrap();
        use std::io::Write as _;
        child_file.write_all(b"marker").unwrap();
        drop(child_file);

        let read = crate::platform::fs::read_private_regular_file_bounded(&child, 6);
        assert!(
            matches!(read.as_deref(), Ok(bytes) if bytes == b"marker"),
            "direct child private-file read failed: {read:?}; child DACL bytes: {:02x?}",
            file_security_descriptor(&child).unwrap().dacl().unwrap().bytes().unwrap(),
        );

        apply_protected_dacl_sddl(&child, "D:P(A;;GR;;;WD)").unwrap();
        assert_eq!(
            crate::platform::fs::read_private_regular_file_bounded(&child, 6)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(feature = "fs")]
    #[test]
    fn inherited_private_child_is_accepted_but_explicitly_permissive_child_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        ensure_owner_private_directory(&directory).unwrap();
        let child = directory.join("inherited-marker");
        // Unlike `create_private_file`, ordinary creation must let Windows
        // materialize the parent DACL's inheritable ACEs on the child.
        fs::write(&child, b"marker").unwrap();

        let descriptor = file_security_descriptor(&child).unwrap();
        let dacl = descriptor.dacl().unwrap().bytes().unwrap();
        let current_sid = current_user_sid_bytes().unwrap();
        let owner_sid = descriptor.owner_sid_bytes().unwrap();
        assert!(
            dacl_is_exact_user_system_file_policy(&dacl, &current_sid),
            "inherited child DACL is not current-user/SYSTEM private; \
             current SID: {current_sid:02x?}; owner SID: {owner_sid:02x?}; \
             ACE flags: {:02x?}; DACL bytes: {dacl:02x?}",
            dacl_ace_flags(&dacl),
        );
        assert!(
            dacl_ace_flags(&dacl).iter().all(|flags| flags & 0x10 != 0),
            "ordinary child did not retain inherited ACE flags: {:02x?}",
            dacl_ace_flags(&dacl),
        );

        let read = crate::platform::fs::read_private_regular_file_bounded(&child, 6);
        assert!(
            matches!(read.as_deref(), Ok(bytes) if bytes == b"marker"),
            "inherited child private-file read failed: {read:?}; \
             current SID: {current_sid:02x?}; owner SID: {owner_sid:02x?}; \
             ACE flags: {:02x?}; DACL bytes: {dacl:02x?}",
            dacl_ace_flags(&dacl),
        );

        apply_protected_dacl_sddl(&child, "D:P(A;;GR;;;WD)").unwrap();
        assert_eq!(
            crate::platform::fs::read_private_regular_file_bounded(&child, 6)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(feature = "fs")]
    #[test]
    fn private_file_dacl_parser_accepts_only_the_expected_inherited_aces() {
        let user_sid = [1, 2, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 7, 0, 0, 0];
        let system_sid = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
        let mut dacl = vec![2, 0, 0, 0, 2, 0, 0, 0];
        for sid in [&user_sid[..], &system_sid[..]] {
            dacl.extend([0, 0x10]);
            dacl.extend((8 + sid.len() as u16).to_le_bytes());
            dacl.extend(0x001f_01ff_u32.to_le_bytes());
            dacl.extend(sid);
        }
        let dacl_len = dacl.len() as u16;
        dacl[2..4].copy_from_slice(&dacl_len.to_le_bytes());

        assert!(dacl_is_exact_user_system_file_policy(&dacl, &user_sid));
        dacl[9] = 1;
        assert!(!dacl_is_exact_user_system_file_policy(&dacl, &user_sid));
    }

    fn dacl_ace_flags(dacl: &[u8]) -> Vec<u8> {
        if dacl.len() < 8 {
            return Vec::new();
        }
        let count = u16::from_le_bytes([dacl[4], dacl[5]]) as usize;
        let mut flags = Vec::with_capacity(count);
        let mut offset: usize = 8;
        for _ in 0..count {
            let Some(header_end) = offset.checked_add(4) else {
                return Vec::new();
            };
            if header_end > dacl.len() {
                return Vec::new();
            }
            let ace_size = u16::from_le_bytes([dacl[offset + 2], dacl[offset + 3]]) as usize;
            if ace_size < 4 || offset.checked_add(ace_size).is_none_or(|end| end > dacl.len()) {
                return Vec::new();
            }
            flags.push(dacl[offset + 1]);
            offset += ace_size;
        }
        if offset == dacl.len() {
            flags
        } else {
            Vec::new()
        }
    }
}
