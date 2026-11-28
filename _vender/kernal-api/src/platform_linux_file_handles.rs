
    pub fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler Ã¢â‚¬â€ not queryable",
            ));
        }
        let dir = format!("/proc/{pid}/fd");
        let entries = std::fs::read_dir(&dir)?;
        let mut handles = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            // Each entry is a symlink to either a filesystem path
            // (e.g. /etc/hosts) or an anonymous kernel object
            // (`socket:[12345]`, `pipe:[67890]`, `anon_inode:...`).
            // `read_link` returns the target as a PathBuf Ã¢â‚¬â€ keep the
            // raw lossy-decoded string so anonymous targets survive
            // intact for downstream pattern-matching.
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            handles.push(target.to_string_lossy().into_owned());
        }
        Ok(handles)
    }
