use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Condvar, Mutex, MutexGuard};

use super::{FileIdentity, Metadata, OwnedEntry as Entry, ScanError, ScanOptions, Summary};
use crate::native_fs_scan::{self as platform, Dir};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Task {
    dir: Dir,
    path: std::path::PathBuf,
    system_root: bool,
    identity: FileIdentity,
}

struct Work {
    queues: Vec<Mutex<VecDeque<Task>>>,
    outstanding: AtomicUsize,
    gate: Mutex<()>,
    ready: Condvar,
    cancelled: AtomicBool,
    errors: AtomicU64,
    seen: Vec<Mutex<HashSet<(u64, u64)>>>,
}

impl Work {
    fn pop(&self, id: usize) -> Option<Task> {
        if let Some(task) = lock(&self.queues[id]).pop_back() {
            return Some(task);
        }
        (1..self.queues.len())
            .find_map(|offset| lock(&self.queues[(id + offset) % self.queues.len()]).pop_front())
    }

    fn next(&self, id: usize) -> Option<Task> {
        let mut gate = lock(&self.gate);
        loop {
            if self.cancelled.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(task) = self.pop(id) {
                return Some(task);
            }
            if self.outstanding.load(Ordering::Relaxed) == 0 {
                return None;
            }
            gate = self
                .ready
                .wait(gate)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn enqueue(&self, id: usize, task: Task) -> Result<(), Task> {
        let _gate = lock(&self.gate);
        let mut queue = lock(&self.queues[id]);
        // Bound descriptor pressure. A saturated worker handles a child inline.
        if queue.len() >= 64 {
            return Err(task);
        }
        self.outstanding.fetch_add(1, Ordering::Relaxed);
        queue.push_back(task);
        self.ready.notify_one();
        Ok(())
    }

    fn complete(&self) {
        let _gate = lock(&self.gate);
        if self.outstanding.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.ready.notify_all();
        }
    }

    fn unique(&self, meta: Metadata, follow: bool) -> bool {
        if !meta.is_dir && meta.links <= 1 && !follow {
            return true;
        }
        let shard = (meta.inode ^ meta.device) as usize % self.seen.len();
        lock(&self.seen[shard]).insert(meta.identity())
    }
}

struct Worker<'a> {
    id: usize,
    work: &'a Work,
    options: &'a ScanOptions,
    root: Metadata,
    tx: mpsc::SyncSender<Vec<Entry>>,
    batch: Vec<Entry>,
}

impl Worker<'_> {
    fn emit(&mut self, entry: Entry) -> bool {
        self.batch.push(entry);
        self.batch.len() < 256 || self.flush()
    }

    fn flush(&mut self) -> bool {
        if self.batch.is_empty() {
            return true;
        }
        self.tx
            .send(std::mem::replace(&mut self.batch, Vec::with_capacity(256)))
            .is_ok()
    }

    fn child(&mut self, task: &Task, name: &std::ffi::OsStr, result: io::Result<Metadata>) -> bool {
        if self.work.cancelled.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(mut meta) = result else {
            self.work.errors.fetch_add(1, Ordering::Relaxed);
            return true;
        };
        if (self.options.one_filesystem && !self.root.same_filesystem(meta))
            || super::skipped(task.system_root, name, self.root, meta)
        {
            return true;
        }
        let unique = self.work.unique(meta, self.options.follow_symlinks);
        if !unique && meta.is_dir {
            return true;
        }
        if !unique {
            meta.allocated = 0;
            meta.apparent = 0;
        }
        let mut path = task.path.join(name);
        // Only queued directories need to retain a second path. Moving file
        // paths keeps the consumer adapter from adding an allocation per file.
        let event_path = if meta.is_dir {
            path.clone()
        } else {
            std::mem::take(&mut path)
        };
        if !self.emit(Entry::new(event_path, meta, task.identity)) {
            return false;
        }
        if meta.is_dir {
            match task
                .dir
                .open_at(name, self.options.follow_symlinks)
                .and_then(|dir| {
                    if dir.metadata()?.identity() != meta.identity() {
                        return Err(io::Error::other("directory replaced during scan"));
                    }
                    Ok(dir)
                }) {
                Ok(dir) => {
                    if let Err(child) = self.work.enqueue(
                        self.id,
                        Task {
                            dir,
                            path,
                            system_root: false,
                            identity: meta.file_identity(),
                        },
                    ) {
                        // Saturation is unusual; reuse the ordinary traversal with a
                        // separate buffer while the parent's buffer is borrowed.
                        self.process(child, &mut vec![0; 128 * 1024]);
                    }
                }
                Err(_) => {
                    self.work.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        true
    }

    fn process(&mut self, task: Task, buffer: &mut [u8]) {
        if task
            .dir
            .read(buffer, self.options.follow_symlinks, |name, meta| {
                self.child(&task, name, meta)
            })
            .is_err()
        {
            self.work.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn run(mut self) {
        let mut buffer = vec![0; 128 * 1024];
        while let Some(task) = self.work.next(self.id) {
            self.process(task, &mut buffer);
            self.work.complete();
        }
        self.flush();
    }
}

fn account(summary: &mut Summary, entry: &Entry) {
    summary.allocated = summary.allocated.saturating_add(entry.meta.allocated);
    summary.apparent = summary.apparent.saturating_add(entry.meta.apparent);
    summary.entries += 1;
}

pub fn scan_stream(
    root: &Path,
    opts: &ScanOptions,
    mut sink: impl FnMut(Entry) -> bool,
) -> Result<Summary, ScanError> {
    if opts.threads == Some(0) || opts.threads.is_some_and(|n| n > 256) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "threads must be between 1 and 256",
        )
        .into());
    }
    if let Some(summary) = platform::fast_scan(root, opts, &mut sink)? {
        return Ok(summary);
    }
    let meta = platform::root(root, opts.follow_symlinks)?;
    let dir = if meta.is_dir {
        let dir = Dir::open(root, opts.follow_symlinks)?;
        if dir.metadata()?.identity() != meta.identity() {
            return Err(io::Error::other("root replaced during scan").into());
        }
        Some(dir)
    } else {
        None
    };
    let mut summary = Summary::default();
    let entry = Entry::new(std::path::PathBuf::new(), meta, meta.file_identity());
    account(&mut summary, &entry);
    if !sink(entry) {
        summary.cancelled = true;
        return Ok(summary);
    }
    let Some(dir) = dir else {
        return Ok(summary);
    };
    let work = Work::new(meta, dir, opts);
    collect(&work, opts, meta, summary, sink)
}

fn collect(
    work: &Work,
    opts: &ScanOptions,
    meta: Metadata,
    mut summary: Summary,
    mut sink: impl FnMut(Entry) -> bool,
) -> Result<Summary, ScanError> {
    let threads = work.queues.len();
    let (tx, rx) = mpsc::sync_channel(threads * 2);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for id in 0..threads {
            let worker = Worker {
                id,
                work,
                options: opts,
                root: meta,
                tx: tx.clone(),
                batch: Vec::with_capacity(256),
            };
            handles.push(scope.spawn(move || worker.run()));
        }
        drop(tx);
        for batch in rx {
            for entry in batch {
                if summary.cancelled {
                    continue;
                }
                account(&mut summary, &entry);
                if !sink(entry) {
                    summary.cancelled = true;
                    let _gate = lock(&work.gate);
                    work.cancelled.store(true, Ordering::Relaxed);
                    work.ready.notify_all();
                }
            }
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| io::Error::other("scanner worker panicked"))?;
        }
        Ok::<_, io::Error>(())
    })?;
    summary.errors = work.errors.load(Ordering::Relaxed);
    Ok(summary)
}

impl Work {
    fn new(meta: Metadata, dir: Dir, opts: &ScanOptions) -> Self {
        let threads = opts
            .threads
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(8)));
        let work = Self {
            queues: (0..threads).map(|_| Mutex::new(VecDeque::new())).collect(),
            outstanding: AtomicUsize::new(1),
            gate: Mutex::new(()),
            ready: Condvar::new(),
            cancelled: AtomicBool::new(false),
            errors: AtomicU64::new(0),
            seen: (0..64).map(|_| Mutex::new(HashSet::new())).collect(),
        };
        work.unique(meta, opts.follow_symlinks);
        lock(&work.queues[0]).push_back(Task {
            dir,
            path: std::path::PathBuf::new(),
            system_root: platform::is_system_root(meta),
            identity: meta.file_identity(),
        });
        work
    }
}
