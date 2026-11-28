//! Linux filesystem-change-notification backend, adapted from the `notify`
//! crate's `inotify` backend.
//!
//! `notify`'s Linux backend carries no divergence from the published
//! `notify` 7.0.0 release -- the fork zccache and soldr vendored exists only
//! for the two Windows fixes documented in `src/platform_win/fs_watch.rs` --
//! so this backend depends on the exact upstream release directly rather
//! than re-vendoring code with nothing to fix.

use std::io;
use std::path::Path;

use notify::Watcher as _;

use crate::platform::fs_watch::{
    ChangeEvent, ChangeKind, EntryKind, RecursiveMode, RenameSide, RescanRequired,
    WatchNotification,
};

/// Watches filesystem paths for changes using the host's recommended
/// `notify` backend (inotify on Linux).
pub struct Watcher(notify::RecommendedWatcher);

impl Watcher {
    /// Create a watcher that delivers notifications to `handler`.
    ///
    /// `handler` runs on a dedicated background thread owned by the
    /// backend, matching `notify::recommended_watcher`.
    ///
    /// # Errors
    ///
    /// Returns an error if the host watcher cannot be initialized.
    pub fn new<F>(mut handler: F) -> io::Result<Self>
    where
        F: FnMut(io::Result<WatchNotification>) + Send + 'static,
    {
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            handler(translate(result));
        })
        .map_err(to_io_error)?;
        Ok(Self(watcher))
    }

    /// Begin watching `path`. See [`RecursiveMode`] for directory scope.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be watched.
    pub fn watch(&mut self, path: &Path, mode: RecursiveMode) -> io::Result<()> {
        self.0
            .watch(path, translate_recursive_mode(mode))
            .map_err(to_io_error)
    }

    /// Stop watching `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if `path` was not being watched, or if removing the
    /// watch fails.
    pub fn unwatch(&mut self, path: &Path) -> io::Result<()> {
        self.0.unwatch(path).map_err(to_io_error)
    }
}

fn translate_recursive_mode(mode: RecursiveMode) -> notify::RecursiveMode {
    match mode {
        RecursiveMode::Recursive => notify::RecursiveMode::Recursive,
        RecursiveMode::NonRecursive => notify::RecursiveMode::NonRecursive,
    }
}

fn to_io_error(error: notify::Error) -> io::Error {
    match error.kind {
        notify::ErrorKind::Io(io_error) => io_error,
        notify::ErrorKind::PathNotFound => {
            io::Error::new(io::ErrorKind::NotFound, error.to_string())
        }
        _ => io::Error::other(error.to_string()),
    }
}

/// Translate one raw `notify` delivery into a facade notification.
///
/// `notify::Event::need_rescan` is how inotify's `IN_Q_OVERFLOW` (the queue
/// filled and events were dropped) surfaces once it reaches the public API;
/// the watch itself keeps running, so this is never a lost watch on Linux.
fn translate(result: notify::Result<notify::Event>) -> io::Result<WatchNotification> {
    let event = result.map_err(to_io_error)?;
    if event.need_rescan() {
        return Ok(WatchNotification::RescanRequired(RescanRequired::new(
            false,
            event.paths.clone(),
        )));
    }
    Ok(WatchNotification::Change(translate_change(event)))
}

fn translate_change(event: notify::Event) -> ChangeEvent {
    use notify::event::ModifyKind;
    use notify::EventKind;

    let kind = match event.kind {
        EventKind::Create(create_kind) => ChangeKind::Created(translate_create(create_kind)),
        EventKind::Remove(remove_kind) => ChangeKind::Removed(translate_remove(remove_kind)),
        EventKind::Modify(ModifyKind::Name(rename_mode)) => {
            ChangeKind::NameModified(translate_rename(rename_mode))
        }
        EventKind::Modify(ModifyKind::Metadata(_)) => ChangeKind::MetadataModified,
        EventKind::Modify(_) => ChangeKind::ContentModified,
        EventKind::Access(_) => ChangeKind::Accessed,
        EventKind::Any | EventKind::Other => ChangeKind::Other,
    };
    ChangeEvent::new(kind, event.paths)
}

fn translate_create(kind: notify::event::CreateKind) -> EntryKind {
    use notify::event::CreateKind;
    match kind {
        CreateKind::File => EntryKind::File,
        CreateKind::Folder => EntryKind::Folder,
        CreateKind::Any | CreateKind::Other => EntryKind::Unknown,
    }
}

fn translate_remove(kind: notify::event::RemoveKind) -> EntryKind {
    use notify::event::RemoveKind;
    match kind {
        RemoveKind::File => EntryKind::File,
        RemoveKind::Folder => EntryKind::Folder,
        RemoveKind::Any | RemoveKind::Other => EntryKind::Unknown,
    }
}

fn translate_rename(mode: notify::event::RenameMode) -> RenameSide {
    use notify::event::RenameMode;
    match mode {
        RenameMode::To => RenameSide::To,
        RenameMode::From => RenameSide::From,
        RenameMode::Both => RenameSide::Both,
        RenameMode::Any | RenameMode::Other => RenameSide::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn create_event_classifies_as_created_file() {
        let event = notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
            .add_path(PathBuf::from("a.txt"));
        let changed = translate_change(event);
        assert_eq!(changed.kind(), ChangeKind::Created(EntryKind::File));
        assert_eq!(changed.paths(), &[PathBuf::from("a.txt")]);
    }

    #[test]
    fn remove_event_classifies_as_removed_folder() {
        let event =
            notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::Folder))
                .add_path(PathBuf::from("dir"));
        let changed = translate_change(event);
        assert_eq!(changed.kind(), ChangeKind::Removed(EntryKind::Folder));
    }

    #[test]
    fn data_modify_event_classifies_as_content_modified() {
        let event = notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Data(notify::event::DataChange::Content),
        ))
        .add_path(PathBuf::from("f"));
        let changed = translate_change(event);
        assert_eq!(changed.kind(), ChangeKind::ContentModified);
    }

    #[test]
    fn rename_both_carries_the_from_to_pair_in_order() {
        let event = notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Name(notify::event::RenameMode::Both),
        ))
        .add_path(PathBuf::from("old"))
        .add_path(PathBuf::from("new"));
        let changed = translate_change(event);
        assert_eq!(
            changed.kind(),
            ChangeKind::NameModified(RenameSide::Both)
        );
        assert_eq!(
            changed.paths(),
            &[PathBuf::from("old"), PathBuf::from("new")]
        );
    }

    #[test]
    fn rescan_flag_becomes_rescan_required_with_the_watch_still_alive() {
        let event = notify::Event::new(notify::EventKind::Other)
            .set_flag(notify::event::Flag::Rescan);
        let notification = translate(Ok(event)).expect("translate never errors on Ok");
        match notification {
            WatchNotification::RescanRequired(rescan) => assert!(!rescan.watch_lost()),
            WatchNotification::Change(_) => panic!("rescan flag must not become a Change"),
        }
    }

    /// End-to-end: watch a temp directory, write a file, and observe a
    /// `Change` notification. Polls with a bounded deadline instead of a
    /// fixed sleep because inotify delivery timing is not guaranteed.
    #[test]
    fn watching_a_directory_reports_a_file_creation() {
        let directory = tempfile::tempdir().expect("temp directory");
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher =
            Watcher::new(move |result| { let _ = tx.send(result); }).expect("create watcher");
        watcher
            .watch(directory.path(), RecursiveMode::NonRecursive)
            .expect("watch directory");

        let target = directory.path().join("created.txt");
        std::fs::write(&target, b"hello").expect("write file");

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_creation = false;
        while Instant::now() < deadline {
            let Ok(notification) = rx.recv_timeout(Duration::from_millis(200)) else {
                continue;
            };
            if let Ok(WatchNotification::Change(change)) = notification {
                if change
                    .paths()
                    .iter()
                    .any(|path| path == Path::new(&target))
                {
                    saw_creation = true;
                    break;
                }
            }
        }
        assert!(saw_creation, "expected a change notification for the created file");
    }
}
