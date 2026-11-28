
    // libc 0.2 exposes `proc_pidinfo` / `proc_pidfdinfo` on macOS but
    // does NOT export `vnode_fdinfowithpath` / `proc_fdinfo` /
    // PROC_PIDLISTFDS / PROC_PIDFDVNODEPATHINFO. Declare them inline
    // from `<sys/proc_info.h>` Ã¢â‚¬â€ layouts and values have been
    // ABI-stable since OS X 10.5.
    const PROC_PIDLISTFDS: libc::c_int = 1;
    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
    const PROX_FDTYPE_VNODE: u32 = 1;
    /// `MAXPATHLEN` from `<sys/syslimits.h>`.
    const MAXPATHLEN: usize = 1024;

    /// `struct proc_fdinfo { int32_t proc_fd; uint32_t proc_fdtype; }`.
    /// 8 bytes; size of array entry returned by `proc_pidinfo(PROC_PIDLISTFDS)`.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct ProcFdInfo {
        proc_fd: i32,
        proc_fdtype: u32,
    }

    /// Opaque buffer matching `vnode_fdinfowithpath` (24 byte
    /// `proc_fileinfo` header + 152 byte `vnode_info` + 1024 byte
    /// `vip_path` = 1200 bytes total). We only read `vip_path`,
    /// which lives at offset 24 + 152 = 176.
    const VNODE_FDINFOWITHPATH_SIZE: usize = 1200;
    const VIP_PATH_OFFSET: usize = 176;

    #[repr(C)]
    struct VnodeFdInfoWithPath {
        _opaque: [u8; VNODE_FDINFOWITHPATH_SIZE],
    }

    pub fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler Ã¢â‚¬â€ not queryable",
            ));
        }
        // Size probe: PROC_PIDLISTFDS with null buffer returns required
        // bytes for the proc_fdinfo array.
        let size = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                PROC_PIDLISTFDS,
                0,
                std::ptr::null_mut(),
                0,
            )
        };
        if size <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let entry_size = std::mem::size_of::<ProcFdInfo>();
        let count = (size as usize) / entry_size;
        let mut fds: Vec<ProcFdInfo> = vec![
            ProcFdInfo {
                proc_fd: 0,
                proc_fdtype: 0
            };
            count
        ];
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr() as *mut libc::c_void,
                (count * entry_size) as libc::c_int,
            )
        };
        if written <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written_count = (written as usize) / entry_size;
        fds.truncate(written_count);

        let mut handles = Vec::new();
        for fd in &fds {
            // We only resolve vnode-backed fds (regular files,
            // directories, devices). Sockets/pipes/kqueues have no
            // POSIX path; skip them.
            if fd.proc_fdtype != PROX_FDTYPE_VNODE {
                continue;
            }
            let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
            let n = unsafe {
                libc::proc_pidfdinfo(
                    pid as libc::c_int,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    &mut info as *mut VnodeFdInfoWithPath as *mut libc::c_void,
                    std::mem::size_of::<VnodeFdInfoWithPath>() as libc::c_int,
                )
            };
            if n <= 0 {
                // fd closed between listfds and fdinfo Ã¢â‚¬â€ skip the race.
                continue;
            }
            let path_bytes = &info._opaque[VIP_PATH_OFFSET..VIP_PATH_OFFSET + MAXPATHLEN];
            let nul = path_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(path_bytes.len());
            if nul == 0 {
                continue;
            }
            let path = String::from_utf8_lossy(&path_bytes[..nul]).into_owned();
            handles.push(path);
        }
        Ok(handles)
    }
