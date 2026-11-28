//! Filesystem-change-notification primitive: watcher construction and event
//! classification.
//!
//! This is deliberately the *primitive* only. Debouncing, ignore-list
//! filtering, and cache-invalidation policy are product decisions that stay
//! with the application; this facade owns constructing a watcher for the
//! selected host and classifying what it reports.
//!
//! On Linux and macOS this privately adapts the upstream `notify` crate. On
//! Windows this privately implements `ReadDirectoryChangesW` watching rather
//! than depending on `notify` at all, because upstream `notify` 7.0.0 has two
//! failure modes that leave a Windows watcher looking alive while silently
//! reporting nothing further:
//!
//! 1. The completion routine firing with a zero byte count, which the
//!    Windows documentation defines as a change-notification buffer
//!    overflow: some events were lost, but the watch is still running.
//! 2. `ReadDirectoryChangesW` failing to re-issue after a completion, which
//!    kills the watch for that directory outright.
//!
//! Both are detected here and surfaced through [`RescanRequired`] rather than
//! dropped, so a caller that trusts its watcher -- such as a build cache
//! deciding whether a cached artifact is still valid -- never trusts a stale
//! view of the filesystem without knowing it. See
//! `src/platform_win/fs_watch.rs` for the implementation and the exact
//! rationale carried alongside each fix.

use std::path::PathBuf;

/// Whether a watched directory's descendants are included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecursiveMode {
    /// Watch the directory and everything below it.
    Recursive,
    /// Watch only the directory itself and its immediate children.
    NonRecursive,
}

/// Whether a created or removed filesystem entry is known to be a file, a
/// folder, or simply not distinguished by the host that reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// The entry is a regular file.
    File,
    /// The entry is a directory.
    Folder,
    /// The host reported the change without saying which kind of entry it
    /// applies to.
    Unknown,
}

/// Which side of a rename this notification carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RenameSide {
    /// The path carried by this event is the entry's new name. A sibling
    /// [`RenameSide::From`] event, if the host emits one, carries the old
    /// name.
    To,
    /// The path carried by this event is the entry's old name. A sibling
    /// [`RenameSide::To`] event, if the host emits one, carries the new
    /// name.
    From,
    /// Both the old and new paths are known and carried together in one
    /// event. [`ChangeEvent::paths`] holds exactly two paths in this case,
    /// old name first: `[from, to]`.
    Both,
    /// The host reported a name change without saying which side, or
    /// whether it paired both halves.
    Unknown,
}

/// Classification of one filesystem change.
///
/// This mirrors the granularity real hosts actually provide. Linux and macOS
/// can report content, metadata, and paired-rename detail; Windows can only
/// distinguish creation, removal, an unpaired rename half, and an
/// unclassified modification, so it reports the closest variant below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// An entry was created.
    Created(EntryKind),
    /// An entry was removed. Hosts also report this for a rename that moves
    /// an entry out of the watched scope.
    Removed(EntryKind),
    /// The content of a file changed.
    ContentModified,
    /// The metadata (permissions, ownership, timestamps, or an extended
    /// attribute) of an entry changed.
    MetadataModified,
    /// An entry's name changed.
    NameModified(RenameSide),
    /// An entry was accessed (opened, read, or executed) without being
    /// mutated. Only some hosts are capable of reporting this.
    Accessed,
    /// The host reported a change it could not classify more precisely.
    Other,
}

/// One classified filesystem change and the path(s) it applies to.
///
/// A [`RenameSide::Both`] event carries exactly two paths, `[from, to]`.
/// Every other kind carries the single affected path, unless the host could
/// not supply one, in which case the path list is empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEvent {
    kind: ChangeKind,
    paths: Vec<PathBuf>,
}

impl ChangeEvent {
    /// Construct a change event.
    ///
    /// Classifying raw host notifications is this crate's job -- a client
    /// receives these, it does not produce them from a watch. This is public
    /// for the case that is not production: a client whose own code turns
    /// these into its product's event type has logic worth testing, and
    /// testing it means handing it events. Refusing to construct one forces
    /// that client to either skip the tests or reach for a real watcher and
    /// a real filesystem race.
    ///
    /// `paths` follows the shape this type documents: a
    /// [`RenameSide::Both`] event carries `[from, to]`, every other kind
    /// carries the single affected path, and an empty list means the host
    /// supplied none. Nothing enforces that here, because a test that wants
    /// to feed its own conversion a malformed event to see what happens is a
    /// legitimate test.
    pub fn new(kind: ChangeKind, paths: Vec<PathBuf>) -> Self {
        Self { kind, paths }
    }

    /// The classification of this change.
    pub fn kind(&self) -> ChangeKind {
        self.kind
    }

    /// The path(s) this change applies to.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// A watcher's view of the filesystem may be incomplete, and any file or
/// folder under the watched roots might have changed.
///
/// This is the facade's rescan/overflow signal: the condition every host
/// backend can reach when it cannot pause event production faster than the
/// filesystem is changing, and -- on Windows specifically -- the condition
/// the two `ReadDirectoryChangesW` fixes this crate carries make reachable
/// as a reported event instead of silence. A caller that ignores this
/// variant can end up trusting a watch that has stopped telling the truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RescanRequired {
    watch_lost: bool,
    paths: Vec<PathBuf>,
}

impl RescanRequired {
    /// Construct a rescan notification.
    ///
    /// Public for the same reason as [`ChangeEvent::new`], and with more at
    /// stake. A client has to decide what this variant means for it, and the
    /// decision has two halves that are easy to get half-right: a rescan
    /// means its view is incomplete, and [`watch_lost`](Self::watch_lost)
    /// additionally means the watch is gone and must be re-established before
    /// any further event arrives. A client that handles the first and forgets
    /// the second keeps a watch that has quietly stopped reporting -- which is
    /// the failure this variant exists to make visible. Testing that requires
    /// constructing both.
    pub fn new(watch_lost: bool, paths: Vec<PathBuf>) -> Self {
        Self { watch_lost, paths }
    }

    /// Whether the underlying watch itself died and must be re-established
    /// with a fresh call to `watch` before events resume, as opposed to the
    /// watch still running but having skipped events in the meantime.
    pub fn watch_lost(&self) -> bool {
        self.watch_lost
    }

    /// A hint of which subtree needs rescanning, when the host backend can
    /// supply one. An empty list means the caller should treat every
    /// watched root as suspect.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// One notification delivered to a watcher's handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchNotification {
    /// A classified filesystem change.
    Change(ChangeEvent),
    /// The watcher's view of the filesystem may be incomplete; see
    /// [`RescanRequired`].
    RescanRequired(RescanRequired),
}

#[cfg(feature = "fs-watch")]
pub use crate::FsWatchWatcher as Watcher;

#[cfg(all(test, feature = "fs-watch"))]
mod tests {
    use super::*;

    #[test]
    fn change_event_exposes_its_kind_and_paths() {
        let event = ChangeEvent::new(
            ChangeKind::ContentModified,
            vec![PathBuf::from("a"), PathBuf::from("b")],
        );
        assert_eq!(event.kind(), ChangeKind::ContentModified);
        assert_eq!(event.paths(), &[PathBuf::from("a"), PathBuf::from("b")]);
    }

    #[test]
    fn rescan_required_distinguishes_a_lost_watch_from_a_skipped_window() {
        let lost = RescanRequired::new(true, vec![PathBuf::from("dir")]);
        assert!(lost.watch_lost());
        assert_eq!(lost.paths(), &[PathBuf::from("dir")]);

        let skipped = RescanRequired::new(false, Vec::new());
        assert!(!skipped.watch_lost());
        assert!(skipped.paths().is_empty());
    }
}
