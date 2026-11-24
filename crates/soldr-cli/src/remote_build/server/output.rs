use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use super::{key, retention};

pub(super) struct FinishedOutput {
    pub(super) path: PathBuf,
    _retention: Option<std::sync::Arc<retention::OutputDirectory>>,
}

pub(super) fn publish_session_failure(root: &Path, id: &str, error: &io::Error) -> io::Result<()> {
    let session = root.join("sessions").join(id);
    let mut staging = tempfile::NamedTempFile::new_in(&session)?;
    let message: String = error.to_string().chars().take(1024).collect();
    staging.write_all(message.as_bytes())?;
    staging.persist(session.join("failure.txt"))?;
    Ok(())
}

pub(super) fn finished_output(
    root: &Path,
    id: &str,
    connection: &retention::Connection,
) -> io::Result<FinishedOutput> {
    let id = key(id, 32)?;
    let leased = connection.output(id);
    let path = leased.as_ref().map_or_else(
        || root.join("sessions").join(id).join("output"),
        |lease| lease.path().join("output"),
    );
    if !path.join("attestation.json").is_file() {
        let session = root.join("sessions").join(id);
        match fs::read_to_string(session.join("failure.txt")) {
            Ok(message) => return Err(io::Error::other(message)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if session.join("started").is_file()
            && leased.as_ref().is_none_or(|lease| !lease.path().is_dir())
        {
            return Err(io::Error::other(
                "remote build started, but its worker no longer holds the result",
            ));
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "remote artifact session unavailable",
        ));
    }
    Ok(FinishedOutput {
        path,
        _retention: leased,
    })
}
