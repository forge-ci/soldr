
    //! macOS `sysctl(KERN_PROCARGS2)` implementation. Returns the
    //! actual argv the kernel handed to `execve`, fully no-admin for
    //! processes the calling user owns.
    //!
    //! Layout of the returned buffer (per `sys/sysctl.h` +
    //! `bsd/kern/kern_sysctl.c` in xnu):
    //!
    //! ```text
    //! [ argc (i32, host endianness) ]
    //! [ exec_path (NUL-terminated UTF-8 string) ]
    //! [ NUL padding to align to ptr-boundary ]
    //! [ argv[0] (NUL-terminated) ]
    //! [ argv[1] ... argv[argc-1] (each NUL-terminated) ]
    //! [ envp[0] ... envp[N] (NUL-terminated; ignored here) ]
    //! ```

    const CTL_KERN: libc::c_int = 1;
    const KERN_PROCARGS2: libc::c_int = 49;

    pub fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler Ã¢â‚¬â€ not queryable",
            ));
        }
        let mut name: [libc::c_int; 3] = [CTL_KERN, KERN_PROCARGS2, pid as libc::c_int];
        // Size probe: pass null buf to learn the required length.
        let mut len: libc::size_t = 0;
        let r = unsafe {
            libc::sysctl(
                name.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if len < std::mem::size_of::<i32>() {
            return Err(std::io::Error::other(format!(
                "KERN_PROCARGS2 returned size={len}, smaller than argc header",
            )));
        }

        let mut buf = vec![0u8; len];
        let r = unsafe {
            libc::sysctl(
                name.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        buf.truncate(len);
        parse_procargs2(&buf)
    }

    fn parse_procargs2(buf: &[u8]) -> std::io::Result<String> {
        if buf.len() < std::mem::size_of::<i32>() {
            return Ok(String::new());
        }
        let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if argc <= 0 {
            return Ok(String::new());
        }
        let mut cursor = std::mem::size_of::<i32>();
        // Skip exec_path: bytes until first NUL.
        while cursor < buf.len() && buf[cursor] != 0 {
            cursor += 1;
        }
        // Skip the run of NUL padding the kernel inserts to align argv
        // start to a pointer boundary.
        while cursor < buf.len() && buf[cursor] == 0 {
            cursor += 1;
        }
        // Read exactly argc argv strings, joining with spaces Ã¢â‚¬â€ mirrors
        // the Windows NtQueryInformationProcess and Linux
        // /proc/<pid>/cmdline conventions.
        let mut argv: Vec<String> = Vec::with_capacity(argc as usize);
        for _ in 0..argc {
            if cursor >= buf.len() {
                break;
            }
            let start = cursor;
            while cursor < buf.len() && buf[cursor] != 0 {
                cursor += 1;
            }
            argv.push(String::from_utf8_lossy(&buf[start..cursor]).into_owned());
            // Skip the NUL terminator.
            cursor = cursor.saturating_add(1);
        }
        Ok(argv.join(" "))
    }

    #[cfg(test)]
    mod tests {
        use super::parse_procargs2;

        /// Build a KERN_PROCARGS2 buffer for argv = [exec, args...].
        fn build_procargs2(exec_path: &str, argv: &[&str]) -> Vec<u8> {
            let mut buf = Vec::new();
            let argc = argv.len() as i32;
            buf.extend_from_slice(&argc.to_ne_bytes());
            buf.extend_from_slice(exec_path.as_bytes());
            buf.push(0);
            // Pad to a pointer boundary with extra NULs (kernel does
            // this Ã¢â‚¬â€ exercise the skip-padding path in the parser).
            while buf.len() % 8 != 0 {
                buf.push(0);
            }
            for arg in argv {
                buf.extend_from_slice(arg.as_bytes());
                buf.push(0);
            }
            // Trailing envp would go here; we don't add any.
            buf
        }

        #[test]
        fn parses_argv_skipping_exec_path_and_padding() {
            let buf = build_procargs2("/usr/bin/myprog", &["myprog", "--flag", "value with space"]);
            let out = parse_procargs2(&buf).expect("parse");
            assert_eq!(out, "myprog --flag value with space");
        }

        #[test]
        fn empty_argv_yields_empty_string() {
            let buf = build_procargs2("/usr/bin/noop", &[]);
            let out = parse_procargs2(&buf).expect("parse");
            assert_eq!(out, "");
        }

        #[test]
        fn argc_zero_short_circuits() {
            let mut buf = 0i32.to_ne_bytes().to_vec();
            buf.extend_from_slice(b"/usr/bin/noop\0");
            let out = parse_procargs2(&buf).expect("parse");
            assert_eq!(out, "");
        }
    }
