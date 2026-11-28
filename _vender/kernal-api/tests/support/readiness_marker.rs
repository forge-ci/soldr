use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

pub fn publish(path: &Path, text: &str) -> io::Result<()> {
    publish_with(path, |file| file.write_all(text.as_bytes()))
}

pub fn publish_with(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("marker needs a parent"))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    write(staged.as_file_mut())?;
    staged.as_file_mut().flush()?;
    // Readers must never observe the create-before-write interval. Keeping
    // the temporary file beside the marker permits atomic publication, while
    // no-clobber catches stale paths rather than overwriting their evidence.
    staged
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    Ok(())
}
