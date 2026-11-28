
    //! Windows handle snapshot via
    //! `NtQuerySystemInformation(SystemExtendedHandleInformation=64)`
    //! filtered by PID, then `DuplicateHandle` + `NtQueryObject` to
    //! resolve each File-typed handle's NT name.
    //!
    //! The size-doubling loop on `NtQuerySystemInformation` follows the
    //! standard pattern: call with a buffer, grow on
    //! `STATUS_INFO_LENGTH_MISMATCH`, retry. Filtering by
    //! `UniqueProcessId == target_pid` happens after the call but
    //! before the per-handle `DuplicateHandle` dance, so we never
    //! actually touch external processes' handles Ã¢â‚¬â€ we just see their
    //! presence in the system-wide table dump.
    //!
    //! `NtQueryObject(ObjectNameInformation)` can block indefinitely on
    //! certain non-File handle types (named pipes to remote endpoints,
    //! sockets to peer-disconnected sessions). We mitigate by first
    //! calling `NtQueryObject(ObjectTypeInformation)` and skipping any
    //! handle whose type name isn't `"File"`. ObjectTypeInformation is
    //! safe to query on any handle type Ã¢â‚¬â€ it doesn't traverse the
    //! object's name graph.

    use std::ffi::c_void;

    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcess};
    use winapi::um::winnt::{DUPLICATE_SAME_ACCESS, HANDLE, PROCESS_DUP_HANDLE};

    // Ã¢â€â‚¬Ã¢â€â‚¬ Ntdll info classes Ã¢â€â‚¬Ã¢â€â‚¬

    /// `SystemExtendedHandleInformation` (info class 64). Returns
    /// `SYSTEM_HANDLE_INFORMATION_EX` (1 ULONG_PTR count + array of
    /// `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX`).
    const SYSTEM_EXTENDED_HANDLE_INFORMATION: i32 = 64;

    /// `ObjectTypeInformation` (info class 2). Returns a
    /// `PUBLIC_OBJECT_TYPE_INFORMATION` (UNICODE_STRING TypeName +
    /// opaque tail). Safe to call on any handle type.
    const OBJECT_TYPE_INFORMATION: i32 = 2;

    /// `ObjectNameInformation` (info class 1). Returns a
    /// `PUBLIC_OBJECT_NAME_INFORMATION` (UNICODE_STRING Name).
    /// **Hazard:** can block forever on certain non-File handles; we
    /// guard by calling `ObjectTypeInformation` first.
    const OBJECT_NAME_INFORMATION: i32 = 1;

    const STATUS_SUCCESS: i32 = 0;
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;

    /// Layout matches `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX` from
    /// `<winternl.h>`. ULONG_PTR is pointer-sized (8 bytes on x86_64).
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct SystemHandleTableEntryInfoEx {
        object: usize,            // PVOID
        unique_process_id: usize, // ULONG_PTR
        handle_value: usize,      // ULONG_PTR  (raw HANDLE value as integer)
        granted_access: u32,
        creator_back_trace_index: u16,
        object_type_index: u16,
        handle_attributes: u32,
        reserved: u32,
    }

    /// Header for the buffer returned by
    /// `NtQuerySystemInformation(SystemExtendedHandleInformation)`: a
    /// single `ULONG_PTR` count followed by `count` entries.
    #[repr(C)]
    struct SystemHandleInformationExHeader {
        number_of_handles: usize, // ULONG_PTR
        reserved: usize,          // ULONG_PTR
    }

    /// Layout matches `UNICODE_STRING` from `<winternl.h>`.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct UnicodeString {
        length: u16,         // bytes (excluding NUL)
        maximum_length: u16, // bytes
        buffer: *mut u16,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQuerySystemInformation(
            system_information_class: i32,
            system_information: *mut c_void,
            system_information_length: u32,
            return_length: *mut u32,
        ) -> i32;

        fn NtQueryObject(
            handle: HANDLE,
            object_information_class: i32,
            object_information: *mut c_void,
            object_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    pub fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the system idle process Ã¢â‚¬â€ not queryable",
            ));
        }

        // 1. System-wide handle table dump (size-doubling loop).
        let raw = query_system_handles()?;
        let header_size = std::mem::size_of::<SystemHandleInformationExHeader>();
        if raw.len() < header_size {
            return Err(std::io::Error::other(
                "NtQuerySystemInformation returned < header bytes",
            ));
        }
        let header =
            unsafe { std::ptr::read(raw.as_ptr() as *const SystemHandleInformationExHeader) };
        let entry_size = std::mem::size_of::<SystemHandleTableEntryInfoEx>();
        let max_entries = (raw.len() - header_size) / entry_size;
        let entries_count = std::cmp::min(header.number_of_handles, max_entries);

        // 2. Open the target process for handle duplication. If this
        // fails (most often because the process exited between the
        // table dump and now, or we don't own the process), return
        // the OS error Ã¢â‚¬â€ that's the correct behavior for the
        // LaunchedProcessTree scope where we expect to own everything.
        let target_proc = unsafe { OpenProcess(PROCESS_DUP_HANDLE, FALSE, pid) };
        if target_proc.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let target_guard = ProcHandle(target_proc);

        // 3. Walk the entries, filter by pid, duplicate + type-check +
        // name-query each surviving handle.
        let mut handles = Vec::new();
        let entries_ptr =
            unsafe { raw.as_ptr().add(header_size) } as *const SystemHandleTableEntryInfoEx;
        for i in 0..entries_count {
            let entry = unsafe { std::ptr::read(entries_ptr.add(i)) };
            if entry.unique_process_id as u32 != pid {
                continue;
            }
            if let Some(path) = resolve_entry(target_guard.0, entry.handle_value as HANDLE) {
                handles.push(path);
            }
        }
        Ok(handles)
    }

    /// `NtQuerySystemInformation` size-doubling loop. Returns the raw
    /// bytes (header + entries) so the caller can parse them.
    fn query_system_handles() -> std::io::Result<Vec<u8>> {
        // Start with 256 KB Ã¢â‚¬â€ typical Windows hosts have 50kÃ¢â‚¬â€œ100k
        // handles open across all processes; one ULONG_PTR + 28 bytes
        // each is ~2.8 MB at the 100k mark, so we double aggressively.
        let mut size: u32 = 256 * 1024;
        loop {
            let mut buf = vec![0u8; size as usize];
            let mut returned: u32 = 0;
            let status = unsafe {
                NtQuerySystemInformation(
                    SYSTEM_EXTENDED_HANDLE_INFORMATION,
                    buf.as_mut_ptr() as *mut c_void,
                    size,
                    &mut returned,
                )
            };
            if status == STATUS_SUCCESS {
                let used = returned.max(1) as usize;
                buf.truncate(used.min(buf.len()));
                return Ok(buf);
            }
            if status == STATUS_INFO_LENGTH_MISMATCH {
                // Double and retry. Cap at 256 MB to avoid runaway
                // growth on a malicious / pathological host.
                if size >= 256 * 1024 * 1024 {
                    return Err(std::io::Error::other(format!(
                        "NtQuerySystemInformation handle table exceeds 256 MiB (returned hint={returned})",
                    )));
                }
                size = size
                    .saturating_mul(2)
                    .max(returned.saturating_add(64 * 1024));
                continue;
            }
            return Err(std::io::Error::other(format!(
                "NtQuerySystemInformation returned status=0x{:08x}",
                status as u32,
            )));
        }
    }

    /// Duplicate one foreign-process handle into the calling process,
    /// check the object type, resolve the name if it's `"File"`, then
    /// close the duplicated handle. Errors / non-File handles return
    /// `None` rather than aborting the whole snapshot.
    fn resolve_entry(target_proc: HANDLE, foreign_handle: HANDLE) -> Option<String> {
        use winapi::um::handleapi::DuplicateHandle;
        let mut local_handle: HANDLE = std::ptr::null_mut();
        let ok = unsafe {
            DuplicateHandle(
                target_proc,
                foreign_handle,
                GetCurrentProcess(),
                &mut local_handle,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == FALSE || local_handle.is_null() {
            return None;
        }
        let local_guard = ProcHandle(local_handle);

        // Type-check first: ObjectTypeInformation is safe on any
        // handle. ObjectNameInformation is NOT safe on
        // pipes/sockets, so we filter before calling it.
        let type_name = query_object_string(local_guard.0, OBJECT_TYPE_INFORMATION)?;
        if type_name != "File" {
            return None;
        }
        query_object_string(local_guard.0, OBJECT_NAME_INFORMATION).filter(|s| !s.is_empty())
    }

    /// Call `NtQueryObject(class, ...)` with a size-doubling loop,
    /// extract the leading `UNICODE_STRING`, and decode it as UTF-8
    /// (lossy on the rare invalid-surrogate edge). The buffer is
    /// read from the local allocation, not the kernel-returned
    /// `buffer` pointer, so we don't deref a kernel-side address.
    fn query_object_string(handle: HANDLE, info_class: i32) -> Option<String> {
        let mut size: u32 = 4 * 1024;
        loop {
            let mut buf = vec![0u8; size as usize];
            let mut returned: u32 = 0;
            let status = unsafe {
                NtQueryObject(
                    handle,
                    info_class,
                    buf.as_mut_ptr() as *mut c_void,
                    size,
                    &mut returned,
                )
            };
            if status == STATUS_SUCCESS {
                buf.truncate((returned as usize).min(buf.len()));
                return parse_leading_unicode_string(&buf);
            }
            if status == STATUS_INFO_LENGTH_MISMATCH {
                if size >= 1024 * 1024 {
                    return None;
                }
                size = size.saturating_mul(2).max(returned);
                continue;
            }
            return None;
        }
    }

    /// Read the leading `UNICODE_STRING` from `buf` and return the
    /// wide-char data as a `String`.
    ///
    /// We must trust `us.buffer` (the kernel-supplied pointer) rather
    /// than assuming the string lives immediately after the header.
    /// For `ProcessCommandLineInformation` the string is appended
    /// directly, but for `PUBLIC_OBJECT_TYPE_INFORMATION` it lives
    /// past 88 bytes of trailing `Reserved[22]` fields. The kernel
    /// writes `us.buffer` as a pointer into our supplied allocation
    /// regardless of where it chose to place the bytes.
    fn parse_leading_unicode_string(buf: &[u8]) -> Option<String> {
        let header_size = std::mem::size_of::<UnicodeString>();
        if buf.len() < header_size {
            return None;
        }
        let us = unsafe { std::ptr::read(buf.as_ptr() as *const UnicodeString) };
        let len_bytes = us.length as usize;
        if len_bytes == 0 || us.buffer.is_null() {
            return Some(String::new());
        }
        // Sanity check: the kernel-supplied pointer should point
        // inside our buf allocation. Reject otherwise to avoid an
        // accidental wild deref.
        let buf_start = buf.as_ptr() as usize;
        let buf_end = buf_start + buf.len();
        let buffer_addr = us.buffer as usize;
        if buffer_addr < buf_start || buffer_addr.saturating_add(len_bytes) > buf_end {
            return None;
        }
        let wide: &[u16] =
            unsafe { std::slice::from_raw_parts(us.buffer as *const u16, len_bytes / 2) };
        Some(String::from_utf16_lossy(wide))
    }

    /// RAII wrapper that closes a HANDLE on drop. Used for both the
    /// OpenProcess result and the per-entry DuplicateHandle result.
    struct ProcHandle(HANDLE);
    impl Drop for ProcHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }
