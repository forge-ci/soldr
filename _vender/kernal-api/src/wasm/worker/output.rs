//! Parent-owned output staging. The supervisor must retain this owner until
//! the worker has been reaped; guest-visible authority never includes the final
//! destination. Only successful, reaped execution may publish staged bytes.

use std::io;
use std::path::{Path, PathBuf};

pub(super) struct StagedOutput {
    directory: crate::platform::fs::OwnedScratchDirectory,
    destination: PathBuf,
    #[cfg(test)]
    fail_cleanup: bool,
    #[cfg(test)]
    before_publish: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

pub(super) struct FinalizedOutput {
    // None means replacement succeeded; Some means it was stopped before rename.
    pub(super) stop: Option<super::SketchWorkerStopReason>,
    pub(super) cleanup: io::Result<()>,
}

impl StagedOutput {
    pub(super) fn new(destination: &Path) -> io::Result<Self> {
        // A grant authorizes precisely one canonical parent plus one normal
        // leaf. Reject a parent component before canonicalizing, because
        // canonicalization could otherwise turn `approved/../result.png`
        // into a grant rooted outside `approved`.
        if destination
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output destination contains a parent path component",
            ));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output has no parent"))?;
        let name = destination.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "output has no file name")
        })?;
        // Keep this boundary path-free for guests and non-ambiguous for the
        // host: the final component must be exactly one ordinary file name.
        let mut name_components = Path::new(name).components();
        if !matches!(
            name_components.next(),
            Some(std::path::Component::Normal(_))
        ) || name_components.next().is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output file name is not a normal path component",
            ));
        }
        let parent = std::fs::canonicalize(parent)?;
        let directory = crate::platform::fs::OwnedScratchDirectory::create_in(&parent)?;
        Ok(Self {
            directory,
            destination: parent.join(name),
            #[cfg(test)]
            fail_cleanup: false,
            #[cfg(test)]
            before_publish: std::sync::Mutex::new(None),
        })
    }

    pub(super) fn worker_destination(&self) -> PathBuf {
        self.directory.path().join("completed-output")
    }

    /// Call only after successful worker completion and reap. Never publish
    /// the directory itself, an unfinished sibling, or a symbolic link.
    pub(super) fn commit(
        self,
        stop: impl FnOnce() -> Option<super::SketchWorkerStopReason>,
    ) -> io::Result<FinalizedOutput> {
        let staged = self.worker_destination();
        if !std::fs::symlink_metadata(&staged)?.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker output is not a regular file",
            ));
        }
        // Windows FlushFileBuffers requires a write-capable handle.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&staged)?
            .sync_all()?;
        #[cfg(test)]
        if let Some(before_publish) = self.before_publish.lock().unwrap().take() {
            before_publish();
        }
        // Flushing may be slow. A stop observed at the publication boundary
        // must preserve the original destination, not report success.
        if let Some(reason) = stop() {
            return Ok(FinalizedOutput {
                stop: Some(reason),
                cleanup: self.cleanup(),
            });
        }
        crate::fs_replace_file(&staged, &self.destination)?;
        Ok(FinalizedOutput {
            stop: None,
            cleanup: self.cleanup(),
        })
    }

    /// Explicit cleanup lets the supervisor report failure instead of relying
    /// solely on best-effort Drop. This must also follow worker reap.
    pub(super) fn discard(self) -> io::Result<()> {
        self.cleanup()
    }

    #[cfg(test)]
    pub(super) fn with_before_publish(mut self, hook: impl FnOnce() + Send + 'static) -> Self {
        self.before_publish = std::sync::Mutex::new(Some(Box::new(hook)));
        self
    }

    #[cfg(test)]
    pub(super) fn with_cleanup_failure(mut self) -> Self {
        self.fail_cleanup = true;
        self
    }

    fn cleanup(self) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_cleanup {
            return Err(io::Error::other("injected parent staging cleanup failure"));
        }
        self.directory.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_grant_rejects_parent_path_component() {
        let root = tempfile::tempdir().unwrap();
        let approved = root.path().join("approved");
        std::fs::create_dir(&approved).unwrap();
        let error = StagedOutput::new(&approved.join("..").join("result.png"))
            .err()
            .expect("a parent path component must not broaden an exact output grant");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn staged_output_is_invisible_until_parent_commit() {
        let root = tempfile::tempdir().unwrap();
        let final_path = root.path().join("result.png");
        std::fs::write(&final_path, b"original").unwrap();
        let output = StagedOutput::new(&final_path).unwrap();
        let staged = output.worker_destination();
        assert_ne!(staged, final_path);
        std::fs::write(&staged, b"completed bytes").unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"original");
        output.commit(|| None).unwrap().cleanup.unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"completed bytes");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn parent_discard_removes_worker_partial_and_completed_files() {
        for completed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let final_path = root.path().join("result.png");
            std::fs::write(&final_path, b"original").unwrap();
            let output = StagedOutput::new(&final_path).unwrap();
            std::fs::write(output.directory.path().join("partial.tmp"), b"partial").unwrap();
            if completed {
                std::fs::write(output.worker_destination(), b"uncommitted").unwrap();
            }
            output.discard().unwrap();
            assert_eq!(std::fs::read(&final_path).unwrap(), b"original");
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn parent_discard_cleans_staging_after_destination_parent_is_renamed() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("destination-parent");
        let moved = root.path().join("moved-parent");
        std::fs::create_dir(&parent).unwrap();
        let final_path = parent.join("result.png");
        std::fs::write(&final_path, b"original").unwrap();
        let output = StagedOutput::new(&final_path).unwrap();
        std::fs::write(output.worker_destination(), b"uncommitted").unwrap();
        let staging_name = output.directory.path().file_name().unwrap().to_owned();
        std::fs::rename(&parent, &moved).unwrap();
        // Reusing the old pathname must not redirect cleanup to new data.
        let replacement = parent.join(staging_name);
        std::fs::create_dir_all(&replacement).unwrap();
        std::fs::write(replacement.join("keep"), b"unrelated").unwrap();

        output.discard().unwrap();

        assert_eq!(
            std::fs::read(moved.join("result.png")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read_dir(&moved).unwrap().count(),
            1,
            "discard must remove staging from the original directory after rename"
        );
        assert_eq!(
            std::fs::read(replacement.join("keep")).unwrap(),
            b"unrelated"
        );
    }

    #[test]
    fn commit_error_cleans_staging_after_destination_parent_is_renamed() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let moved = root.path().join("moved");
        std::fs::create_dir(&parent).unwrap();
        let destination = parent.join("result.png");
        std::fs::write(&destination, b"original").unwrap();
        let output = StagedOutput::new(&destination).unwrap();
        std::fs::write(output.worker_destination(), b"uncommitted").unwrap();
        std::fs::rename(&parent, &moved).unwrap();

        assert!(output.commit(|| None).is_err());
        assert_eq!(
            std::fs::read(moved.join("result.png")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 1);
    }

    #[test]
    fn missing_completed_output_preserves_destination_and_cleans_staging() {
        let root = tempfile::tempdir().unwrap();
        let final_path = root.path().join("result.png");
        std::fs::write(&final_path, b"original").unwrap();
        let output = StagedOutput::new(&final_path).unwrap();
        std::fs::write(output.directory.path().join("partial.tmp"), b"partial").unwrap();
        assert!(output.commit(|| None).is_err());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn replacement_failure_preserves_existing_directory_and_cleans_staging() {
        let root = tempfile::tempdir().unwrap();
        let final_path = root.path().join("existing-directory");
        std::fs::create_dir(&final_path).unwrap();
        std::fs::write(final_path.join("keep"), b"original").unwrap();
        let output = StagedOutput::new(&final_path).unwrap();
        std::fs::write(output.worker_destination(), b"completed").unwrap();
        assert!(output.commit(|| None).is_err());
        assert_eq!(std::fs::read(final_path.join("keep")).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn completed_symlink_is_not_published_or_followed() {
        let root = tempfile::tempdir().unwrap();
        let final_path = root.path().join("result.png");
        let unrelated = root.path().join("unrelated");
        std::fs::write(&final_path, b"original").unwrap();
        std::fs::write(&unrelated, b"private").unwrap();
        let output = StagedOutput::new(&final_path).unwrap();
        std::os::unix::fs::symlink(&unrelated, output.worker_destination()).unwrap();
        assert!(output.commit(|| None).is_err());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"original");
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"private");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }
}
