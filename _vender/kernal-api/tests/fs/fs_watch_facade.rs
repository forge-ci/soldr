//! Consumer-style contract for the filesystem-change facade.
//!
//! This fixture is a separate crate, so it can only reach what a real client
//! can. That is the point: the in-crate unit tests construct a `ChangeEvent`
//! whether or not its constructor is public, and so cannot notice it being
//! crate-private.

#![cfg(feature = "fs-watch")]

use std::path::PathBuf;

use kernal_api::platform::fs_watch::{
    ChangeEvent, ChangeKind, EntryKind, RenameSide, RescanRequired,
};

/// A client can build the events its own conversion consumes.
///
/// zccache turns these into its product's watch events, with ignore-filter
/// and rename handling worth testing; testing it means handing that code
/// events rather than racing a real watcher against a real filesystem.
#[test]
fn a_client_can_construct_the_events_it_converts() {
    let created = ChangeEvent::new(
        ChangeKind::Created(EntryKind::File),
        vec![PathBuf::from("new.txt")],
    );
    assert_eq!(created.kind(), ChangeKind::Created(EntryKind::File));
    assert_eq!(created.paths(), &[PathBuf::from("new.txt")]);

    // The documented two-path shape: `[from, to]`, in that order.
    let renamed = ChangeEvent::new(
        ChangeKind::NameModified(RenameSide::Both),
        vec![PathBuf::from("before"), PathBuf::from("after")],
    );
    assert_eq!(renamed.paths().len(), 2);
    assert_eq!(renamed.paths()[0], PathBuf::from("before"));
    assert_eq!(renamed.paths()[1], PathBuf::from("after"));
}

/// A host that supplied no path is representable, because a conversion has
/// to decide what to do about it and that decision deserves a test.
#[test]
fn an_event_without_paths_is_representable() {
    let pathless = ChangeEvent::new(ChangeKind::Other, Vec::new());
    assert!(pathless.paths().is_empty());
    assert_eq!(pathless.kind(), ChangeKind::Other);
}

/// A client can construct both halves of the rescan signal.
///
/// The two are not interchangeable: a skipped window means "rescan", while a
/// lost watch additionally means "nothing further will arrive until you watch
/// again". A client that treats them the same keeps a dead watch, so it needs
/// to be able to build each one and prove it routes them differently.
#[test]
fn a_client_can_construct_both_halves_of_the_rescan_signal() {
    let skipped = RescanRequired::new(false, vec![PathBuf::from("watched")]);
    assert!(!skipped.watch_lost());
    assert_eq!(skipped.paths(), &[PathBuf::from("watched")]);

    let lost = RescanRequired::new(true, vec![PathBuf::from("watched")]);
    assert!(
        lost.watch_lost(),
        "a lost watch must be distinguishable from a merely skipped window"
    );
}
