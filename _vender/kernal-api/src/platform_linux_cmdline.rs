
    //! Linux `/proc/<pid>/cmdline` implementation. The kernel writes
    //! argv as NUL-separated UTF-8 (typically Ã¢â‚¬â€ argv is opaque bytes,
    //! we lossy-decode), with a trailing NUL.

    pub fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler Ã¢â‚¬â€ not queryable",
            ));
        }
        let path = format!("/proc/{pid}/cmdline");
        let bytes = std::fs::read(&path)?;
        // `/proc/<pid>/cmdline` is empty for kernel threads Ã¢â‚¬â€ return
        // empty string rather than synthesizing fake separators.
        if bytes.is_empty() {
            return Ok(String::new());
        }
        // Drop the trailing NUL terminator if present, then turn
        // remaining NUL separators into spaces so the result reads as
        // a single shell-style command line (same convention as
        // Windows NtQueryInformationProcess and macOS KERN_PROCARGS2,
        // both of which return one logical command line per PID).
        let mut trimmed = bytes.as_slice();
        if trimmed.last() == Some(&0) {
            trimmed = &trimmed[..trimmed.len() - 1];
        }
        let joined: Vec<u8> = trimmed
            .iter()
            .map(|b| if *b == 0 { b' ' } else { *b })
            .collect();
        Ok(String::from_utf8_lossy(&joined).into_owned())
    }
