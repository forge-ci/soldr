//! Versioned content fingerprints for selected directory trees.

use super::{blake3_file, Blake3Digest, Blake3Hasher, Blake3ReadOptions};
use crate::platform::fs::{DirectoryWalk, PatternSet, PatternSetBuilder};
use std::io;
use std::path::{Path, PathBuf};

/// Resource bounds for a tree fingerprint. Limits fail rather than truncating
/// the input set. File bodies are streamed with one 64 KiB buffer per worker.
#[derive(Clone, Copy, Debug)]
pub struct TreeHashOptions {
    /// Maximum number of selected files retained in the sorted inventory.
    pub maximum_files: usize,
    /// Maximum bytes read from any one selected file.
    pub maximum_file_bytes: u64,
    /// Maximum concurrent file readers (clamped to available host parallelism).
    pub workers: usize,
}

impl Default for TreeHashOptions {
    fn default() -> Self {
        Self {
            maximum_files: 1_000_000,
            maximum_file_bytes: 16 * 1024 * 1024 * 1024,
            workers: 8,
        }
    }
}

/// Hash selected regular files, their relative paths, and their count.
///
/// Empty `include` selects all files, including hidden files. Exclusions take
/// precedence, including exclusions of ancestor directories; `directory/**`
/// prunes that subtree. Symlinks are not followed. Patterns and UTF-8 relative
/// paths use forward slash separators on every supported host. Non-UTF-8
/// regular-file paths fail rather than colliding through lossy conversion.
///
/// The v1 encoding uses a domain prefix, sorted length-prefixed relative paths,
/// fixed-size BLAKE3 file digests, and a u64 little-endian file count. It is
/// independent of the absolute root, timestamps, worker count, and scan order.
/// Every call reads the content; no metadata cache can hide same-size edits.
/// This is not a filesystem snapshot: callers must invalidate/retry if their
/// inputs change during a scan.
///
/// # Errors
/// Returns an error for invalid patterns, a non-directory root, scan/read
/// failure, invalid UTF-8 paths, exceeded limits, or failure starting a worker.
pub fn blake3_tree(
    root: impl AsRef<Path>,
    include: &[&str],
    exclude: &[&str],
    options: TreeHashOptions,
) -> io::Result<Blake3Digest> {
    if options.workers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tree hash needs at least one worker",
        ));
    }
    let root = root.as_ref().canonicalize()?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tree hash root must be a directory",
        ));
    }
    let included = patterns(include.iter().copied())?;
    let excluded = patterns(exclude.iter().copied())?;
    let excluded_dirs = patterns(exclude.iter().filter_map(|p| p.strip_suffix("/**")))?;
    let prune_root = root.clone();
    let prune_excluded = excluded.clone();
    let prefixes = include_prefixes(include);
    let walker = DirectoryWalk::new(root.clone()).prune_directories(move |path| {
        let Ok(relative) = path.strip_prefix(&prune_root) else {
            return true;
        };
        let Ok(relative) = relative_name(relative) else {
            return true;
        };
        !prune_excluded.is_match(&relative)
            && !excluded_dirs.is_match(&relative)
            && prefixes.as_ref().is_none_or(|prefixes| {
                prefixes.iter().any(|prefix| {
                    relative == *prefix
                        || relative.starts_with(&format!("{prefix}/"))
                        || prefix.starts_with(&format!("{relative}/"))
                })
            })
    });
    let mut files = Vec::new();
    for entry in walker.walk() {
        let entry = entry?;
        if !entry.is_file() || entry.is_symbolic_link() {
            continue;
        }
        let relative = relative_name(entry.path().strip_prefix(&root).map_err(io::Error::other)?)?;
        if excluded.is_match(&relative) || (!included.is_empty() && !included.is_match(&relative)) {
            continue;
        }
        if files.len() >= options.maximum_files {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tree hash file count limit exceeded",
            ));
        }
        files.push((relative, entry.path().to_path_buf()));
    }
    files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let worker_count = options
        .workers
        .min(std::thread::available_parallelism()?.get())
        .min(files.len())
        .max(1);
    let chunk_size = files.len().div_ceil(worker_count).max(1);
    let digests = std::thread::scope(|scope| -> io::Result<Vec<Blake3Digest>> {
        let mut handles = Vec::new();
        for chunk in files.chunks(chunk_size) {
            handles.push(
                std::thread::Builder::new()
                    .name("kernal-tree-hash".into())
                    .spawn_scoped(scope, move || hash_files(chunk, options.maximum_file_bytes))?,
            );
        }
        let mut digests = Vec::with_capacity(files.len());
        for handle in handles {
            digests.extend(
                handle
                    .join()
                    .map_err(|_| io::Error::other("tree hash worker panicked"))??,
            );
        }
        Ok(digests)
    })?;
    let mut aggregate = Blake3Hasher::new();
    aggregate.update(b"kernal-api-tree-v1\0");
    for ((name, _), digest) in files.iter().zip(digests) {
        aggregate.update(&(name.len() as u64).to_le_bytes());
        aggregate.update(name.as_bytes());
        aggregate.update(digest.as_bytes());
    }
    aggregate.update(&(files.len() as u64).to_le_bytes());
    Ok(aggregate.finalize())
}

fn hash_files(files: &[(String, PathBuf)], maximum_bytes: u64) -> io::Result<Vec<Blake3Digest>> {
    files
        .iter()
        .map(|(_, path)| {
            blake3_file(path, Blake3ReadOptions::new().maximum_bytes(maximum_bytes)).map_err(
                |error| {
                    io::Error::new(
                        error.io_error_kind().unwrap_or(io::ErrorKind::InvalidData),
                        format!("hash {}: {error}", path.display()),
                    )
                },
            )
        })
        .collect()
}

fn patterns<'a>(patterns: impl Iterator<Item = &'a str>) -> io::Result<PatternSet> {
    patterns
        .fold(PatternSetBuilder::new(), |builder, pattern| {
            builder.add_pattern(pattern)
        })
        .build()
}

fn relative_name(path: &Path) -> io::Result<String> {
    path.components()
        .map(|component| {
            component.as_os_str().to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "tree hash path is not UTF-8")
            })
        })
        .collect::<io::Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

// Only literal directory prefixes can safely prune include searches. A root
// wildcard or an escaped pattern disables this optimization for the whole set.
fn include_prefixes(include: &[&str]) -> Option<Vec<String>> {
    if include.is_empty() {
        return None;
    }
    include
        .iter()
        .map(|pattern| {
            if pattern.contains('\\') {
                return None;
            }
            let end = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
            let prefix = &pattern[..end];
            let slash = prefix.rfind('/')?;
            (slash > 0).then(|| prefix[..slash].to_owned())
        })
        .collect()
}
