
    //! Windows `NtQueryInformationProcess(ProcessCommandLineInformation)`
    //! implementation. The Info class is undocumented but stable on
    //! Win8.1+ Ã¢â‚¬â€ empirically validated in clud#468 t03.

    use std::ffi::c_void;

    /// `ProcessCommandLineInformation` from `ntddk.h` Ã¢â‚¬â€ info class 60.
    /// Stable since Windows 8.1. Returns a `UNICODE_STRING` header
    /// followed by the inline wide-character cmdline bytes.
    const PROCESS_COMMAND_LINE_INFORMATION: i32 = 60;

    /// `STATUS_INFO_LENGTH_MISMATCH` (0xC0000004) Ã¢â‚¬â€ expected on the
    /// initial size-probe call.
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;

    /// `STATUS_SUCCESS` (0).
    const STATUS_SUCCESS: i32 = 0;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationProcess(
            process_handle: *mut c_void,
            process_information_class: i32,
            process_information: *mut c_void,
            process_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    pub fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the system idle process Ã¢â‚¬â€ not queryable",
            ));
        }

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        // winapi's `c_void` is `std::ffi::c_void` only when its `std` feature is
        // unified in; `cast` is correct (and lint-clean) either way.
        let result = query_cmdline(handle.cast());
        unsafe { CloseHandle(handle) };
        result
    }

    fn query_cmdline(handle: *mut c_void) -> std::io::Result<String> {
        // Size probe: pass a zero-length buffer; expect
        // STATUS_INFO_LENGTH_MISMATCH and the required size in
        // `needed`.
        let mut needed: u32 = 0;
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if status != STATUS_INFO_LENGTH_MISMATCH && status != STATUS_SUCCESS {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess size probe returned status=0x{:08x}",
                status as u32,
            )));
        }
        if needed < std::mem::size_of::<UnicodeString>() as u32 {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess returned needed={needed}, smaller than UNICODE_STRING header",
            )));
        }

        let mut buf = vec![0u8; needed as usize];
        let mut returned: u32 = 0;
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                buf.as_mut_ptr() as *mut c_void,
                needed,
                &mut returned,
            )
        };
        if status != STATUS_SUCCESS {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess returned status=0x{:08x}",
                status as u32,
            )));
        }

        // The buffer begins with a UNICODE_STRING whose `buffer` field
        // points into the same allocation, immediately past the header.
        // We cannot dereference `us.buffer` directly across the FFI
        // boundary on systems that may relocate it; instead, compute the
        // header size and read inline.
        let us = unsafe { std::ptr::read(buf.as_ptr() as *const UnicodeString) };
        let len_bytes = us.length as usize;
        if len_bytes == 0 {
            return Ok(String::new());
        }
        // The string is wide-char (UTF-16 LE) and located just after the
        // UNICODE_STRING header. The kernel writes `buffer` as a pointer
        // into our supplied allocation, but the safest portable parse is
        // to read the chars from header_size..header_size+len_bytes in
        // our own buffer.
        let header_size = std::mem::size_of::<UnicodeString>();
        if header_size + len_bytes > buf.len() {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess wrote less than {} bytes for cmdline (returned={returned}, len={len_bytes})",
                header_size + len_bytes,
            )));
        }
        let wide_slice: &[u16] = unsafe {
            std::slice::from_raw_parts(buf[header_size..].as_ptr() as *const u16, len_bytes / 2)
        };
        Ok(String::from_utf16_lossy(wide_slice))
    }
