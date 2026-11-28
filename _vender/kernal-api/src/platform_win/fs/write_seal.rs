//! Windows in-place write sealing without the `READONLY` attribute.
//!
//! A sealed file refuses in-place writes by anybody, including its owner, but
//! `Permissions::readonly()` stays **false** and rename-over-replace still
//! works. That combination is what rustc needs from a cache blob
//! (zccache#1791): `check_file_is_writeable` fails fatally when the attribute
//! is set, even though rustc itself only ever writes a temp file and renames
//! it over the output — it never writes the existing file in place.
//!
//! The seal is a deny ACE for `FILE_WRITE_DATA | FILE_APPEND_DATA` addressed
//! to `World`, inserted at the front of the file's DACL so it precedes every
//! allow ACE. `DELETE` and the write-DACL rights are untouched, so
//! `MoveFileEx(MOVEFILE_REPLACE_EXISTING)` and unlink keep working, and reads
//! are unaffected. The DACL lives on the file record, so every hardlink to
//! the blob carries the seal — the same sharing behaviour the attribute had.
//!
//! Rebuilding the DACL copies the caller's existing ACEs verbatim; only the
//! seal ACE is ours. Detection matches the ACE structurally (type, exact
//! mask, World trustee), not by SDDL spelling, so a normalization change in
//! the string APIs can neither orphan the seal nor make removal miss it.
//!
//! Like the other native operations in this crate this is path-based and
//! follows links; it is not a secure-open primitive. On filesystems without
//! ACL support the write reports success but the read-back finds no seal, and
//! that mismatch is returned as an error rather than silently accepted.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    AddAccessAllowedAce, AddAccessDeniedAce, AddAce, GetSecurityDescriptorDacl, InitializeAcl,
    ACE_HEADER, ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
};

/// `FILE_WRITE_DATA | FILE_APPEND_DATA` — the exact rights the seal denies.
/// `FILE_WRITE_DATA` (0x2) covers overwriting existing bytes;
/// `FILE_APPEND_DATA` (0x4) covers extending the file. Together they block
/// every in-place write the Win32 layer offers while leaving metadata,
/// delete, and rename alone.
const SEAL_RIGHTS: u32 = 0x2 | 0x4;

/// `ACCESS_DENIED_ACE_TYPE`.
const DENY_ACE_TYPE: u8 = 1;

/// `ACCESS_ALLOWED_ACE_TYPE`. Test-only: it is what a non-seal ACE looks
/// like in the structural matcher's negative cases.
#[cfg(test)]
const ALLOW_ACE_TYPE: u8 = 0;

/// The `S-1-1-0` (World) SID in packed binary form: revision, subauthority
/// count, identifier authority `{0,0,0,0,0,1}`, one subauthority `0`.
fn world_sid() -> &'static [u8] {
    const WORLD: &[u8] = &[1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
    WORLD
}

/// Byte length this module's seal ACE adds to an ACL: ACE header + access
/// mask + the packed World SID.
fn seal_ace_size() -> usize {
    std::mem::size_of::<ACE_HEADER>() + 4 + world_sid().len()
}

/// Deny in-place writes to `path` for every trustee, including the owner,
/// while keeping `Permissions::readonly()` false and rename-over-replace
/// working. Idempotent: an already-sealed file is a no-op.
///
/// # Errors
///
/// Returns the metadata or security-descriptor error for a missing path, or
/// when the filesystem does not carry ACLs and the seal cannot be read back.
pub fn deny_in_place_writes(path: &Path) -> io::Result<()> {
    let (descriptor, dacl, null_dacl) = read_dacl(path)?;
    let result = (|| {
        if has_seal(dacl) {
            return Ok(());
        }
        let rebuilt = rebuild_with_seal(dacl, null_dacl, true)?;
        set_dacl(path, &rebuilt)
    })();
    // SAFETY: `descriptor` came from `GetNamedSecurityInfoW`, which allocates
    // it; released exactly once here on every path.
    unsafe { LocalFree(descriptor) };
    result?;

    // Read back rather than trusting the write: `SetNamedSecurityInfoW`
    // reports success on filesystems that do not carry ACLs at all (FAT32,
    // some network mounts), which would leave the seal absent while every
    // caller believes the file is protected.
    if !in_place_writes_denied(path)? {
        // Carry the observed DACL: a silent SetNamedSecurityInfoW success
        // with a non-matching read-back is otherwise un-diagnosable from a
        // CI log alone.
        let observed = dacl_sddl(path).unwrap_or_else(|| "<sddl unavailable>".into());
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the filesystem did not retain the in-place write seal (no ACL support?); \
                 observed DACL after write: {observed}"
            ),
        ));
    }
    Ok(())
}

/// Best-effort SDDL rendering of `path`'s DACL, for error diagnostics only.
fn dacl_sddl(path: &Path) -> Option<String> {
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated for the call; on success Windows
    // allocates `descriptor`, freed below exactly once.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if rc != ERROR_SUCCESS || descriptor.is_null() {
        return None;
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `descriptor` is live; the string output is freed below.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            std::ptr::null_mut(),
        )
    };
    let sddl = if ok != 0 && !text.is_null() {
        // SAFETY: NUL-terminated wide string owned by LocalAlloc.
        let mut chars = Vec::new();
        let mut p = text;
        // SAFETY: `p` walks a NUL-terminated wide string owned by LocalAlloc;
        // the loop stops at the terminator before any out-of-bounds step.
        unsafe {
            while *p != 0 {
                chars.push(*p);
                p = p.add(1);
            }
        }
        Some(String::from_utf16_lossy(&chars))
    } else {
        None
    };
    // SAFETY: each allocation is freed exactly once here.
    unsafe {
        if !text.is_null() {
            LocalFree(text as HLOCAL);
        }
        LocalFree(descriptor as HLOCAL);
    }
    sddl
}

/// Remove a seal this module applied, restoring in-place writes. A file
/// without the seal is a no-op.
///
/// # Errors
///
/// Returns the metadata or security-descriptor error for a missing path.
pub fn allow_in_place_writes(path: &Path) -> io::Result<()> {
    let (descriptor, dacl, null_dacl) = read_dacl(path)?;
    let result = (|| {
        if !has_seal(dacl) {
            return Ok(());
        }
        let rebuilt = rebuild_with_seal(dacl, null_dacl, false)?;
        set_dacl(path, &rebuilt)
    })();
    // SAFETY: one `LocalFree` per `GetNamedSecurityInfoW`, on every path.
    unsafe { LocalFree(descriptor) };
    result
}

/// Whether [`deny_in_place_writes`] currently applies to `path`.
///
/// # Errors
///
/// Returns the metadata or security-descriptor error for a missing path.
pub fn in_place_writes_denied(path: &Path) -> io::Result<bool> {
    let (descriptor, dacl, _null_dacl) = read_dacl(path)?;
    let denied = has_seal(dacl);
    // SAFETY: one `LocalFree` per `GetNamedSecurityInfoW`.
    unsafe { LocalFree(descriptor) };
    Ok(denied)
}

/// Install `acl` as the file's DACL, leaving owner and group untouched.
fn set_dacl(path: &Path, acl: &[u8]) -> io::Result<()> {
    // SAFETY: `acl` is a complete ACL built by `rebuild_with_seal`; `wide` is
    // NUL-terminated for the call; owner and group are null, which leaves
    // those security components unchanged because they are not requested.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl.as_ptr() as *mut ACL,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

/// Read the file's DACL. The caller owns the returned descriptor and must
/// `LocalFree` it; the DACL pointer borrows from it and dies with it.
///
/// An absent or null DACL (which grants everyone everything) is reported as
/// `null_dacl = true` with an empty standing ACL, so a seal rebuild produces
/// "deny the sealed rights to World, then allow everything to World" instead
/// of silently replacing the grant-everything behaviour with a deny-only
/// ACL that would forbid reads too.
fn read_dacl(path: &Path) -> io::Result<(
    *mut std::ffi::c_void,
    *const ACL,
    bool,
)> {
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `wide(path)` is NUL-terminated for the duration of the call;
    // the output pointer receives an allocation owned by this module. The
    // owner, group, and SACL outputs are null because only the DACL security
    // information is requested.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    if descriptor.is_null() {
        return Err(io::Error::other(
            "GetNamedSecurityInfoW returned a null security descriptor",
        ));
    }
    let mut present = 0;
    let mut defaulted = 0;
    // SAFETY: `descriptor` is a live self-relative descriptor from the call
    // above; the out-params receive plain flags and the DACL pointer, in the
    // windows-sys order (present, dacl, defaulted).
    if unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        == 0
    {
        // SAFETY: freeing the descriptor whose read just failed.
        unsafe { LocalFree(descriptor) };
        return Err(io::Error::last_os_error());
    }
    if present == 0 || dacl.is_null() {
        return Ok((descriptor, empty_acl(), true));
    }
    Ok((descriptor, dacl, false))
}

/// A shared header-only standing ACL for an absent DACL. Static storage is
/// sound because an empty ACL carries no variable-length trailing bytes.
fn empty_acl() -> *const ACL {
    static EMPTY: ACL = ACL {
        AclRevision: ACL_REVISION as u8,
        Sbz1: 0,
        AceCount: 0,
        AclSize: std::mem::size_of::<ACL>() as u16,
        Sbz2: 0,
    };
    &EMPTY
}

/// Whether the DACL carries this module's deny ACE: deny type, exactly
/// [`SEAL_RIGHTS`], World trustee.
fn has_seal(dacl: *const ACL) -> bool {
    // SAFETY: `dacl` is either the live DACL borrowed from a descriptor that
    // outlives this call, or the static empty ACL.
    let acl = unsafe { &*dacl };
    // SAFETY: `AclSize` describes the allocation `dacl` points into.
    let bytes = unsafe { std::slice::from_raw_parts(dacl.cast::<u8>(), acl.AclSize as usize) };
    aces(bytes).any(|offset| is_seal_ace(&bytes[offset.0..offset.1]))
}

/// Iterate the raw ACEs of an ACL byte view as `(start, end)` ranges.
fn aces(bytes: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut offset = std::mem::size_of::<ACL>();
    std::iter::from_fn(move || {
        if offset + std::mem::size_of::<ACE_HEADER>() > bytes.len() {
            return None;
        }
        let header: ACE_HEADER = unsafe {
            std::ptr::read_unaligned(bytes[offset..].as_ptr().cast::<ACE_HEADER>())
        };
        let len = header.AceSize as usize;
        if len < std::mem::size_of::<ACE_HEADER>() || offset + len > bytes.len() {
            return None;
        }
        let range = (offset, offset + len);
        offset += len;
        Some(range)
    })
}

/// Whether one raw ACE is the seal: deny type, mask exactly
/// [`SEAL_RIGHTS`], World trustee.
fn is_seal_ace(ace: &[u8]) -> bool {
    let header_len = std::mem::size_of::<ACE_HEADER>();
    let need = header_len + 4 + world_sid().len();
    if ace.len() != need {
        return false;
    }
    let header: ACE_HEADER = unsafe { std::ptr::read_unaligned(ace.as_ptr().cast::<ACE_HEADER>()) };
    if header.AceType != DENY_ACE_TYPE {
        return false;
    }
    let mask = u32::from_ne_bytes(
        ace[header_len..header_len + 4]
            .try_into()
            .expect("four bytes follow every ACE header"),
    );
    mask == SEAL_RIGHTS && &ace[header_len + 4..] == world_sid()
}

/// Build the replacement ACL. With `seal = true`: one deny ACE for World at
/// the front (deny must precede allow for the access check), followed by
/// every ACE of the current DACL verbatim — and, for a null-DACL source, a
/// trailing World allow-all so the previous grant-everything behaviour is
/// preserved minus the sealed rights. With `seal = false`: the current ACEs
/// minus the seal. The returned buffer is owned by the caller.
fn rebuild_with_seal(dacl: *const ACL, null_dacl: bool, seal: bool) -> io::Result<Vec<u8>> {
    // SAFETY: `dacl` is either the live DACL borrowed from a descriptor that
    // outlives this call, or the static empty ACL.
    let acl = unsafe { &*dacl };
    // SAFETY: `AclSize` describes the allocation `dacl` points into.
    let current = unsafe { std::slice::from_raw_parts(dacl.cast::<u8>(), acl.AclSize as usize) };

    // The seal is never part of the kept set: when adding, `has_seal` was
    // false and the filter is a no-op; when removing, dropping it *is* the
    // point — keeping it would make `allow_in_place_writes` a no-op that
    // still reported success.
    let kept: Vec<(usize, usize)> = aces(current)
        .filter(|(start, end)| !is_seal_ace(&current[*start..*end]))
        .collect();

    let extra = if seal {
        seal_ace_size()
    } else {
        0
    } + if seal && null_dacl {
        // Deny-only DACLs forbid everything not explicitly allowed; a null
        // DACL source must keep granting, so follow the deny with World
        // allow-all (`FILE_GENERIC_ALL` unmapped as `GA`).
        std::mem::size_of::<ACE_HEADER>() + 4 + world_sid().len()
    } else {
        0
    };
    let total = std::mem::size_of::<ACL>() + extra + kept.iter().map(|(_, len)| *len).sum::<usize>();
    let mut buffer = vec![0_u8; total];
    // SAFETY: `buffer` is zeroed and at least `size_of::<ACL>()` bytes; the
    // call writes only the fixed ACL header.
    if unsafe { InitializeAcl(buffer.as_mut_ptr().cast::<ACL>(), buffer.len() as u32, ACL_REVISION) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }

    let mut aces_in_rebuilt = 0_u32;
    if seal {
        // SAFETY: the buffer was sized for this ACE, the revision matches
        // the initialized ACL, and `world_sid` is a valid packed SID that
        // outlives the call.
        if unsafe {
            AddAccessDeniedAce(
                buffer.as_mut_ptr().cast::<ACL>(),
                ACL_REVISION,
                SEAL_RIGHTS,
                world_sid().as_ptr().cast_mut().cast(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        aces_in_rebuilt += 1;
        if null_dacl {
            // SAFETY: as above; `GA` is `FILE_GENERIC_ALL`, restoring the
            // null DACL's grant minus the deny that now precedes it.
            if unsafe {
                AddAccessAllowedAce(
                    buffer.as_mut_ptr().cast::<ACL>(),
                    ACL_REVISION,
                    0x1F01FF, // FILE_GENERIC_ALL
                    world_sid().as_ptr().cast_mut().cast(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            aces_in_rebuilt += 1;
        }
    }

    for (start, end) in &kept {
        // Inserting at `aces_in_rebuilt` appends: the index is one past the
        // last ACE already present, and `AddAce` shifts the remainder (of
        // which there is none) right.
        // SAFETY: `start..end` is a validated ACE inside `current`, the
        // buffer was sized for every kept ACE plus the extras, and the
        // index equals the current ACE count.
        if unsafe {
            AddAce(
                buffer.as_mut_ptr().cast::<ACL>(),
                ACL_REVISION,
                aces_in_rebuilt,
                current[*start..].as_ptr().cast_mut().cast(),
                (end - start) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        aces_in_rebuilt += 1;
    }

    Ok(buffer)
}

/// The path's wide spelling for the Win32 `*W` entry points.
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_deny_ace_written_by_windows() {
        // Build through the native API rather than the matcher's constants:
        // Windows emits type 1 for deny and type 0 for allow (#1791).
        let rebuilt = rebuild_with_seal(empty_acl(), true, true).expect("build sealed ACL");
        let entries: Vec<_> = aces(&rebuilt).collect();
        assert_eq!(entries.len(), 2, "deny followed by the null DACL's grant");
        let (start, end) = entries[0];
        assert!(is_seal_ace(&rebuilt[start..end]), "the native deny ACE must match");
        let (start, end) = entries[1];
        assert!(!is_seal_ace(&rebuilt[start..end]), "the native allow ACE must not match");
    }

    #[test]
    fn seal_ace_is_recognized_and_rejected_structurally() {
        // `ACE_HEADER` is four bytes (type, flags, u16 size) — lay the ACE out
        // from `size_of`, exactly as the matcher does, never from a hard-coded
        // offset (a 20-byte seal ACE has its mask at [4..8], SID at [8..20]).
        let header_len = std::mem::size_of::<ACE_HEADER>();
        let mut ace = vec![0_u8; seal_ace_size()];
        // A deny ACE header: type, flags, size; then mask; then World SID.
        ace[0] = DENY_ACE_TYPE;
        ace[1] = 0;
        ace[2..4].copy_from_slice(&(seal_ace_size() as u16).to_ne_bytes());
        ace[header_len..header_len + 4].copy_from_slice(&SEAL_RIGHTS.to_ne_bytes());
        ace[header_len + 4..].copy_from_slice(world_sid());
        assert!(is_seal_ace(&ace), "the constructed seal must match");

        let mut wrong_mask = ace.clone();
        wrong_mask[header_len..header_len + 4].copy_from_slice(&0x1u32.to_ne_bytes());
        assert!(!is_seal_ace(&wrong_mask), "mask must match exactly");

        let mut wrong_type = ace.clone();
        wrong_type[0] = ALLOW_ACE_TYPE;
        assert!(!is_seal_ace(&wrong_type), "an allow ACE is not a seal");

        let mut truncated = ace;
        truncated.pop();
        assert!(!is_seal_ace(&truncated), "length must match");
    }

    #[test]
    fn world_sid_is_the_packed_s1_1_0() {
        // revision 1, one subauthority, identifier authority 1, subauth 0.
        assert_eq!(world_sid(), &[1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(world_sid().len(), 12);
    }
}
