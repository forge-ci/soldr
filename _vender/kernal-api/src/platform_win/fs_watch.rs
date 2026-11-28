//! Windows filesystem-change-notification backend.
//!
//! This backend does not depend on the `notify` crate. It privately
//! implements `ReadDirectoryChangesW` watching, adapted from the `notify`
//! project's Windows backend (`notify` 7.0.0, CC0-1.0 licensed:
//! <https://github.com/notify-rs/notify>, source in
//! `notify::windows` / `src/windows.rs`), carrying two fixes that upstream
//! `notify` 7.0.0 does not have:
//!
//! 1. **Buffer overflow detection.** Per the `ReadDirectoryChangesW`
//!    documentation, when the completion routine fires with
//!    `dwNumberOfBytesTransferred == 0`, the change-notification buffer
//!    overflowed and events were lost. Upstream silently drops this signal;
//!    this backend reports it as [`RescanRequired`] with `watch_lost:
//!    false`, because the watch itself keeps running -- `start_read` below
//!    already re-issues the read before this check runs.
//! 2. **Re-issue failure detection.** If `ReadDirectoryChangesW` fails to
//!    re-issue after a completion (the `ret == 0` branch in `start_read`),
//!    the watch for that directory is dead: no further events will ever
//!    arrive, and upstream reports nothing. This backend reports it as
//!    [`RescanRequired`] with `watch_lost: true`, so a caller knows it must
//!    also call [`Watcher::watch`] again to resume coverage, not just
//!    rescan its cached view.
//!
//! Both failure modes previously left a watcher looking alive while
//! reporting nothing further -- the worst failure mode for a build cache
//! that trusts its watcher, since it means indefinitely stale cache hits.
//! kernal-api owns this backend privately so the fix travels with every
//! client instead of being re-vendored per repository.
//!
//! This module also fixes two mechanical incompatibilities the upstream
//! 7.0.0 release has with `windows-sys` 0.61 (where `HANDLE` became a raw
//! pointer instead of an integer): semaphore-handle failure is checked with
//! `.is_null()` rather than `== 0`.

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::io;
use std::os::raw::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_OPERATION_ABORTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY,
    FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{
    CreateSemaphoreW, ReleaseSemaphore, WaitForSingleObjectEx, INFINITE,
};
use windows_sys::Win32::System::IO::{CancelIo, OVERLAPPED};

use crate::platform::fs_watch::{
    ChangeEvent, ChangeKind, EntryKind, RecursiveMode, RenameSide, RescanRequired,
    WatchNotification,
};

const BUFFER_SIZE: u32 = 16384;

/// The caller's notification handler, held behind the completion routine.
type NotificationHandler = dyn FnMut(io::Result<WatchNotification>) + Send + 'static;

#[derive(Clone)]
struct ReadState {
    /// Directory that is actually being watched (a single-file watch opens
    /// its parent).
    dir: PathBuf,
    /// If a single file is being watched, its full path.
    file: Option<PathBuf>,
    complete_semaphore: HANDLE,
    is_recursive: bool,
}

struct ReadRequest {
    handler: Arc<Mutex<NotificationHandler>>,
    buffer: [u8; BUFFER_SIZE as usize],
    handle: HANDLE,
    state: ReadState,
}

enum Action {
    Watch(PathBuf, RecursiveMode),
    Unwatch(PathBuf),
    Stop,
}

struct WatchState {
    dir_handle: HANDLE,
    complete_semaphore: HANDLE,
}

struct Server {
    actions: Receiver<Action>,
    handler: Arc<Mutex<NotificationHandler>>,
    acks: Sender<io::Result<PathBuf>>,
    watches: HashMap<PathBuf, WatchState>,
    wakeup_semaphore: HANDLE,
}

impl Server {
    /// Spawn the background thread that owns every `ReadDirectoryChangesW`
    /// handle for this watcher.
    ///
    /// The spawn itself can fail (thread/resource exhaustion), and that
    /// failure is propagated to the caller rather than swallowed: a watcher
    /// that silently never starts is exactly the failure class this backend
    /// exists to stop.
    fn start(
        handler: Arc<Mutex<NotificationHandler>>,
        wakeup_semaphore: HANDLE,
    ) -> io::Result<(Sender<Action>, Receiver<io::Result<PathBuf>>)> {
        let (action_tx, action_rx) = channel();
        let (ack_tx, ack_rx) = channel();
        // HANDLE is a raw pointer; it is fine to send it across the thread
        // boundary, but raw pointers are not `Send` so it travels as a
        // pointer-sized integer and is reconstructed on the other side.
        let wakeup_semaphore_value = wakeup_semaphore as usize;
        thread::Builder::new()
            .name("kernal-api fs-watch (windows)".to_string())
            .spawn(move || {
                let wakeup_semaphore = wakeup_semaphore_value as HANDLE;
                let server = Server {
                    actions: action_rx,
                    handler,
                    acks: ack_tx,
                    watches: HashMap::new(),
                    wakeup_semaphore,
                };
                server.run();
            })?;
        Ok((action_tx, ack_rx))
    }

    fn run(mut self) {
        loop {
            let mut stopped = false;

            while let Ok(action) = self.actions.try_recv() {
                match action {
                    Action::Watch(path, mode) => {
                        let outcome = self.add_watch(path, is_recursive(mode));
                        let _ = self.acks.send(outcome);
                    }
                    Action::Unwatch(path) => self.remove_watch(&path),
                    Action::Stop => {
                        stopped = true;
                        for watch in self.watches.values() {
                            stop_watch(watch);
                        }
                        break;
                    }
                }
            }

            if stopped {
                break;
            }

            unsafe {
                // Wait with the alertable flag so the completion routine
                // (an APC) can fire while we wait.
                WaitForSingleObjectEx(self.wakeup_semaphore, 100, 1);
            }
        }

        // The watcher itself may already be gone; the background thread
        // owns this cleanup.
        unsafe {
            CloseHandle(self.wakeup_semaphore);
        }
    }

    fn add_watch(&mut self, path: PathBuf, is_recursive: bool) -> io::Result<PathBuf> {
        if !path.is_dir() && !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "watch path is neither a file nor a directory: {}",
                    path.display()
                ),
            ));
        }

        let (watching_file, dir_target) = if path.is_dir() {
            (false, path.clone())
        } else {
            // Emulate file watching by watching the parent directory.
            match path.parent() {
                Some(parent) => (true, parent.to_path_buf()),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("watch path has no parent directory: {}", path.display()),
                    ))
                }
            }
        };

        let encoded_path: Vec<u16> = dir_target
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                encoded_path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_DELETE | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(if watching_file {
                io::Error::other(format!(
                    "watching a single file requires opening its parent directory, \
                     which failed: {}",
                    path.display()
                ))
            } else {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("could not open directory to watch: {}", path.display()),
                )
            });
        }

        let watched_file = watching_file.then(|| path.clone());
        // Every watch gets its own completion semaphore.
        let semaphore = unsafe { CreateSemaphoreW(ptr::null_mut(), 0, 1, ptr::null_mut()) };
        if semaphore.is_null() || semaphore == INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(handle);
            }
            return Err(io::Error::other(format!(
                "failed to create completion semaphore for watch: {}",
                path.display()
            )));
        }

        let state = ReadState {
            dir: dir_target,
            file: watched_file,
            complete_semaphore: semaphore,
            is_recursive,
        };
        self.watches.insert(
            path.clone(),
            WatchState {
                dir_handle: handle,
                complete_semaphore: semaphore,
            },
        );
        start_read(&state, self.handler.clone(), handle);
        Ok(path)
    }

    fn remove_watch(&mut self, path: &Path) {
        if let Some(watch) = self.watches.remove(path) {
            stop_watch(&watch);
        }
    }
}

fn stop_watch(watch: &WatchState) {
    unsafe {
        let cancelled = CancelIo(watch.dir_handle);
        let closed = CloseHandle(watch.dir_handle);
        // Have to wait for it, otherwise the memory allocated for the
        // outstanding read request leaks.
        if cancelled != 0 && closed != 0 {
            while WaitForSingleObjectEx(watch.complete_semaphore, INFINITE, 1) != WAIT_OBJECT_0 {
                // Drain the APC queue; see notify-rs/notify#287.
            }
        }
        CloseHandle(watch.complete_semaphore);
    }
}

fn is_recursive(mode: RecursiveMode) -> bool {
    matches!(mode, RecursiveMode::Recursive)
}

fn start_read(state: &ReadState, handler: Arc<Mutex<NotificationHandler>>, handle: HANDLE) {
    let request = Box::new(ReadRequest {
        handler,
        handle,
        buffer: [0u8; BUFFER_SIZE as usize],
        state: state.clone(),
    });

    let flags = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION
        | FILE_NOTIFY_CHANGE_SECURITY;

    let monitor_subdirectories =
        i32::from(request.state.file.is_none() && request.state.is_recursive);

    unsafe {
        let overlapped =
            std::alloc::alloc_zeroed(std::alloc::Layout::new::<OVERLAPPED>()) as *mut OVERLAPPED;
        // When using callback-based async requests, the `hEvent` member is
        // free for our own use; stash the request pointer there so the
        // completion routine can recover it.
        let request = Box::leak(request);
        (*overlapped).hEvent = request as *mut _ as _;

        let ret = ReadDirectoryChangesW(
            handle,
            request.buffer.as_mut_ptr() as *mut c_void,
            BUFFER_SIZE,
            monitor_subdirectories,
            flags,
            &mut 0u32 as *mut u32, // not used for async requests
            overlapped,
            Some(handle_event),
        );

        if ret == 0 {
            // Fix 1/2: `ReadDirectoryChangesW` failed to re-issue. The watch
            // for this directory is now dead -- no further events will be
            // delivered until the caller re-watches it.
            emit(
                &request.handler,
                Ok(watch_lost_notification(request.state.dir.clone())),
            );
            // Ownership of the `overlapped` allocation was never passed to
            // `ReadDirectoryChangesW` because it failed, so it is safe to
            // reclaim both allocations here for drop.
            let _overlapped = Box::from_raw(overlapped);
            let request = Box::from_raw(request);
            ReleaseSemaphore(request.state.complete_semaphore, 1, ptr::null_mut());
        }
    }
}

unsafe extern "system" fn handle_event(
    error_code: u32,
    bytes_written: u32,
    overlapped: *mut OVERLAPPED,
) {
    let overlapped: Box<OVERLAPPED> = Box::from_raw(overlapped);
    let request: Box<ReadRequest> = Box::from_raw(overlapped.hEvent as *mut _);

    if error_code == ERROR_OPERATION_ABORTED {
        // Received when the directory is unwatched or the watcher is shut
        // down; return and let `overlapped`/`request` get drop-cleaned.
        ReleaseSemaphore(request.state.complete_semaphore, 1, ptr::null_mut());
        return;
    }

    // Get the next request queued up as soon as possible.
    start_read(&request.state, request.handler.clone(), request.handle);

    // Fix 2/2: per the Windows documentation, when the completion routine is
    // called with `dwNumberOfBytesTransferred == 0`, the change-notification
    // buffer overflowed and events were lost. The watch itself keeps
    // running -- `start_read` above already re-issued the read -- so this is
    // reported with `watch_lost: false`.
    if bytes_written == 0 {
        emit(
            &request.handler,
            Ok(overflow_notification(request.state.dir.clone())),
        );
        return;
    }

    // The FILE_NOTIFY_INFORMATION struct has a variable length due to the
    // variable-length string as its last member. Each struct carries an
    // offset to the next entry in the buffer.
    let mut cursor: *const u8 = request.buffer.as_ptr();
    let mut entry = cursor as *const FILE_NOTIFY_INFORMATION;
    loop {
        // File-name length is a byte count, so divide by two for UTF-16.
        let name_len = (*entry).FileNameLength as usize / 2;
        let encoded_name: &[u16] = slice::from_raw_parts((*entry).FileName.as_ptr(), name_len);
        let path = request
            .state
            .dir
            .join(PathBuf::from(OsString::from_wide(encoded_name)));

        // If watching a single file, ignore events for any other entry in
        // the parent directory.
        let skip = match request.state.file {
            None => false,
            Some(ref watched_path) => *watched_path != path,
        };

        if !skip {
            let kind = classify_action((*entry).Action);
            if let Some(kind) = kind {
                emit(
                    &request.handler,
                    Ok(WatchNotification::Change(ChangeEvent::new(
                        kind,
                        vec![path],
                    ))),
                );
            }
        }

        if (*entry).NextEntryOffset == 0 {
            break;
        }
        cursor = cursor.offset((*entry).NextEntryOffset as isize);
        entry = cursor as *const FILE_NOTIFY_INFORMATION;
    }
}

fn emit(handler: &Mutex<NotificationHandler>, notification: io::Result<WatchNotification>) {
    if let Ok(mut guard) = handler.lock() {
        let handler: &mut NotificationHandler = &mut *guard;
        handler(notification);
    }
}

/// Classify one `FILE_NOTIFY_INFORMATION` entry's `Action` field.
///
/// `None` means the action is not one Windows currently defines; the caller
/// skips it rather than guessing.
fn classify_action(action: u32) -> Option<ChangeKind> {
    match action {
        FILE_ACTION_RENAMED_OLD_NAME => Some(ChangeKind::NameModified(RenameSide::From)),
        FILE_ACTION_RENAMED_NEW_NAME => Some(ChangeKind::NameModified(RenameSide::To)),
        FILE_ACTION_ADDED => Some(ChangeKind::Created(EntryKind::Unknown)),
        FILE_ACTION_REMOVED => Some(ChangeKind::Removed(EntryKind::Unknown)),
        FILE_ACTION_MODIFIED => Some(ChangeKind::ContentModified),
        _ => None,
    }
}

/// Fix 2/2: the notification delivered when the completion routine fires
/// with `bytes_written == 0` (a buffer overflow). The watch itself keeps
/// running, since `start_read` has already re-issued the read by the time
/// this is called.
fn overflow_notification(dir: PathBuf) -> WatchNotification {
    WatchNotification::RescanRequired(RescanRequired::new(false, vec![dir]))
}

/// Fix 1/2: the notification delivered when `ReadDirectoryChangesW` fails to
/// re-issue. The watch for `dir` is dead until the caller calls
/// [`Watcher::watch`] again.
fn watch_lost_notification(dir: PathBuf) -> WatchNotification {
    WatchNotification::RescanRequired(RescanRequired::new(true, vec![dir]))
}

/// Watches filesystem paths for changes via `ReadDirectoryChangesW`.
pub struct Watcher {
    actions: Sender<Action>,
    acks: Receiver<io::Result<PathBuf>>,
    wakeup_semaphore: HANDLE,
}

impl Watcher {
    /// Create a watcher that delivers notifications to `handler` from a
    /// dedicated background thread owned by the backend.
    ///
    /// # Errors
    ///
    /// Returns an error if the host watcher cannot be initialized, including
    /// if the background thread itself fails to start.
    pub fn new<F>(handler: F) -> io::Result<Self>
    where
        F: FnMut(io::Result<WatchNotification>) + Send + 'static,
    {
        let handler: Arc<Mutex<NotificationHandler>> = Arc::new(Mutex::new(handler));

        let wakeup_semaphore = unsafe { CreateSemaphoreW(ptr::null_mut(), 0, 1, ptr::null_mut()) };
        if wakeup_semaphore.is_null() || wakeup_semaphore == INVALID_HANDLE_VALUE {
            return Err(io::Error::other("failed to create wakeup semaphore"));
        }

        let (actions, acks) = match Server::start(handler, wakeup_semaphore) {
            Ok(channels) => channels,
            Err(error) => {
                unsafe {
                    CloseHandle(wakeup_semaphore);
                }
                return Err(error);
            }
        };

        Ok(Self {
            actions,
            acks,
            wakeup_semaphore,
        })
    }

    /// Begin watching `path`. See [`RecursiveMode`] for directory scope.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be watched.
    pub fn watch(&mut self, path: &Path, mode: RecursiveMode) -> io::Result<()> {
        let absolute = to_absolute(path)?;
        if !absolute.is_dir() && !absolute.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "watch path is neither a file nor a directory: {}",
                    absolute.display()
                ),
            ));
        }
        self.send_action_require_ack(Action::Watch(absolute.clone(), mode), &absolute)
    }

    /// Stop watching `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the internal channel to the backend thread has
    /// disconnected.
    pub fn unwatch(&mut self, path: &Path) -> io::Result<()> {
        let absolute = to_absolute(path)?;
        let result = self
            .actions
            .send(Action::Unwatch(absolute))
            .map_err(|_| io::Error::other("fs-watch backend disconnected"));
        self.wakeup_server();
        result
    }

    fn wakeup_server(&mut self) {
        // Breaks the server out of its wait state; purely an optimization
        // so `watch`/`unwatch` do not block for up to 100ms while the
        // server sleeps.
        unsafe {
            ReleaseSemaphore(self.wakeup_semaphore, 1, ptr::null_mut());
        }
    }

    fn send_action_require_ack(&mut self, action: Action, path: &Path) -> io::Result<()> {
        self.actions
            .send(action)
            .map_err(|_| io::Error::other("fs-watch backend disconnected"))?;

        // Wake the server; do not wait around for the ack before returning
        // control, only before this call itself returns.
        self.wakeup_server();

        let acknowledged_path = self
            .acks
            .recv()
            .map_err(|_| io::Error::other("fs-watch backend disconnected"))??;

        if path != acknowledged_path.as_path() {
            return Err(io::Error::other(format!(
                "expected acknowledgement for {} but got {}",
                path.display(),
                acknowledged_path.display()
            )));
        }
        Ok(())
    }
}

fn to_absolute(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.actions.send(Action::Stop);
        self.wakeup_server();
    }
}

// `Watcher` is not `Send`/`Sync` by auto-trait inference because of the raw
// semaphore `HANDLE`. It is safe to send across threads (the handle is not
// closed here; the background thread closes it once `Stop` is processed),
// and every public method takes `&mut self`, so shared references are safe
// too.
unsafe impl Send for Watcher {}
unsafe impl Sync for Watcher {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_action_added_becomes_created_unknown() {
        assert_eq!(
            classify_action(FILE_ACTION_ADDED),
            Some(ChangeKind::Created(EntryKind::Unknown))
        );
    }

    #[test]
    fn file_action_removed_becomes_removed_unknown() {
        assert_eq!(
            classify_action(FILE_ACTION_REMOVED),
            Some(ChangeKind::Removed(EntryKind::Unknown))
        );
    }

    #[test]
    fn file_action_modified_becomes_content_modified() {
        assert_eq!(
            classify_action(FILE_ACTION_MODIFIED),
            Some(ChangeKind::ContentModified)
        );
    }

    #[test]
    fn rename_old_and_new_name_become_paired_name_modified_sides() {
        assert_eq!(
            classify_action(FILE_ACTION_RENAMED_OLD_NAME),
            Some(ChangeKind::NameModified(RenameSide::From))
        );
        assert_eq!(
            classify_action(FILE_ACTION_RENAMED_NEW_NAME),
            Some(ChangeKind::NameModified(RenameSide::To))
        );
    }

    #[test]
    fn unknown_action_is_skipped_rather_than_guessed_at() {
        assert_eq!(classify_action(0xffff_ffff), None);
    }

    #[test]
    fn completion_with_zero_bytes_written_reports_overflow_with_the_watch_alive() {
        let notification = overflow_notification(PathBuf::from(r"C:\watched"));
        match notification {
            WatchNotification::RescanRequired(rescan) => {
                assert!(!rescan.watch_lost());
                assert_eq!(rescan.paths(), &[PathBuf::from(r"C:\watched")]);
            }
            WatchNotification::Change(_) => panic!("zero bytes written must not become a Change"),
        }
    }

    #[test]
    fn reissue_failure_reports_overflow_with_the_watch_lost() {
        let notification = watch_lost_notification(PathBuf::from(r"C:\watched"));
        match notification {
            WatchNotification::RescanRequired(rescan) => {
                assert!(rescan.watch_lost());
                assert_eq!(rescan.paths(), &[PathBuf::from(r"C:\watched")]);
            }
            WatchNotification::Change(_) => panic!("a dead watch must not become a Change"),
        }
    }

    /// End-to-end: watch a temp directory, write a file, and observe a
    /// `Change` notification. Polls with a bounded deadline instead of a
    /// fixed sleep because completion-routine delivery timing is not
    /// guaranteed.
    #[test]
    fn watching_a_directory_reports_a_file_creation() {
        use std::time::{Duration, Instant};

        let directory = tempfile::tempdir().expect("temp directory");
        let (tx, rx) = channel();
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
                if change.paths().iter().any(|path| path == &target) {
                    saw_creation = true;
                    break;
                }
            }
        }
        assert!(saw_creation, "expected a change notification for the created file");
    }
}
