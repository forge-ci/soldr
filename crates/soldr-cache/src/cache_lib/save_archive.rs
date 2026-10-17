#[derive(Debug, Clone)]
pub struct SaveOptions<'a> {
    /// Workspace root to snapshot source mtimes from. `None` skips the
    /// source-file portion entirely (cache-only archive). Required when
    /// `mtimes_only` is `true`.
    pub workspace: Option<&'a Path>,
    /// Cache directory whose contents will be bundled. `None` is only
    /// permitted when `mtimes_only` is `true` (manifest-only archive).
    pub cache_dir: Option<&'a Path>,
    /// Destination archive path.
    pub out: &'a Path,
    /// zstd compression level (1..=22). 3 is a good default; anything
    /// over 9 hurts CI wall-clock more than it saves on transfer.
    pub zstd_level: i32,
    /// Number of rayon threads for the hash + tar walk. `None` uses the
    /// global rayon pool (`num_cpus`).
    pub threads: Option<usize>,
    /// Produce a manifest-only archive: skip the cache-dir walk and
    /// write a tar.zst whose sole entry is `SOLDR_MANIFEST.pb`. Requires
    /// `workspace` to be `Some`. The intent is a standalone source-file
    /// mtime snapshot that setup-soldr (or any other CI wrapper) can
    /// produce + restore without bundling an artifact cache. The on-disk
    /// shape is otherwise byte-identical to a normal save, so the same
    /// `load()` path consumes it.
    pub mtimes_only: bool,
    /// Cache payload profile. `Full` preserves the historical behavior;
    /// `Ci` excludes runtime-only files and reports those omissions.
    pub profile: SaveProfile,
}

#[derive(Debug, Clone, Default)]
pub struct SaveReport {
    pub profile: SaveProfile,
    pub source_files: u64,
    pub cache_files: u64,
    pub deleted_cache_files: u64,
    pub excluded_files: u64,
    pub excluded_bytes: u64,
    pub archive_bytes: u64,
    pub elapsed_ms: u64,
    /// In-root cache symlinks recorded in the manifest (#1548).
    pub cache_symlinks: u64,
    /// Cache symlinks skipped at save time because their target was
    /// absolute, escaped the cache root, or was broken (#1548). Each
    /// skip also emits a stderr warning — never silent.
    pub cache_symlinks_skipped: u64,
}

#[derive(Clone)]
pub struct SaveDeltaOptions<'a> {
    /// Workspace root to snapshot source mtimes from. `None` skips the
    /// source-file portion entirely.
    pub workspace: Option<&'a Path>,
    /// Current cache directory whose changed/new files will be bundled.
    pub cache_dir: &'a Path,
    /// Base-layer manifest to compare current cache contents against.
    pub base_manifest: &'a Manifest,
    /// Destination delta archive path.
    pub out: &'a Path,
    /// zstd compression level (1..=22).
    pub zstd_level: i32,
    /// Number of rayon threads for hash + tar work.
    pub threads: Option<usize>,
    /// Cache payload profile. Applies before delta comparison so excluded
    /// paths are omitted from the delta manifest and can become tombstones
    /// against a fuller base layer.
    pub profile: SaveProfile,
}

/// Validate save inputs:
/// * When `mtimes_only`, `workspace` MUST be `Some` and `cache_dir`
///   MUST be `None` (passing both would silently ignore one of them).
/// * Otherwise `cache_dir` MUST be `Some` (cache-only archives are the
///   historical baseline behavior; an archive with neither a cache nor a
///   workspace is empty and almost certainly a CLI mistake).
fn validate_save_inputs(opts: &SaveOptions<'_>) -> Result<()> {
    if opts.mtimes_only {
        if opts.workspace.is_none() {
            return Err(SaveLoadError::BareIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "soldr save --mtimes-only requires a --workspace to snapshot",
            )));
        }
        if opts.cache_dir.is_some() {
            return Err(SaveLoadError::BareIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "soldr save --mtimes-only must NOT be combined with --cache-dir",
            )));
        }
    } else if opts.cache_dir.is_none() {
        return Err(SaveLoadError::BareIo(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "soldr save requires either --cache-dir or --mtimes-only",
        )));
    }
    Ok(())
}

/// Bundle `cache_dir` + a workspace-source-mtime snapshot into a single
/// `.tar.zst` at `out`.
///
/// When [`SaveOptions::mtimes_only`] is `true`, the cache walk is skipped
/// entirely and the archive contains only `SOLDR_MANIFEST.pb`. That mode
/// requires `workspace` to be `Some` — there is nothing else to snapshot.
pub fn save(opts: &SaveOptions<'_>) -> Result<SaveReport> {
    save_with_projection(opts, None)
}

/// Save through the existing archive writer with an optional immutable subtree.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub fn save_with_projection(
    opts: &SaveOptions<'_>,
    projection: Option<&CacheProjection<'_>>,
) -> Result<SaveReport> {
    validate_save_inputs(opts)?;
    if let Some(projection) = projection {
        let cache_dir = opts.cache_dir.ok_or_else(|| {
            SaveLoadError::BadArchivePath("cache projection requires a cache directory".into())
        })?;
        projection.validate(cache_dir)?;
    }
    if projection.is_some() && opts.mtimes_only {
        return Err(SaveLoadError::BadArchivePath(
            "manifest-only save cannot project cache contents".into(),
        ));
    }
    let start = std::time::Instant::now();

    // Build the manifest (parallel hash if a workspace is provided)
    // AND enumerate cache files concurrently. The two walks touch
    // disjoint directory trees, so running them in parallel halves
    // page-cache-cold first-walk latency on big workspaces.
    //
    // In `mtimes_only` mode the cache half is a no-op closure — we
    // keep the join shape so the source walk still benefits from the
    // shared rayon pool.
    let pool = build_pool(opts.threads)?;
    let (source_result, cache_walk_result): (Result<Vec<SourceFile>>, Result<CacheWalk>) = pool
        .install(|| {
            rayon::join(
                || -> Result<Vec<SourceFile>> {
                    let Some(ws) = opts.workspace else {
                        return Ok(Vec::new());
                    };
                    let files = workspace_files_for_save(ws, opts.threads)?;
                    files
                        .par_iter()
                        .map(|rel| -> Result<SourceFile> {
                            let abs = ws.join(rel);
                            let meta = std::fs::metadata(&abs).map_err(|e| io(&abs, e))?;
                            let hash = hash_file(&abs)?;
                            Ok(SourceFile {
                                path: rel_to_posix(rel),
                                mtime_ms: mtime_ms(&meta),
                                mtime_ns: Some(mtime_ns(&meta)),
                                size: meta.len(),
                                blake3: hash.to_vec(),
                            })
                        })
                        .collect()
                },
                || -> Result<CacheWalk> {
                    if opts.mtimes_only {
                        return Ok(CacheWalk::default());
                    }
                    match opts.cache_dir {
                        Some(dir) if dir.exists() => {
                            walk_cache_with_projection(dir, opts.threads, opts.profile, projection)
                        }
                        _ => Ok(CacheWalk::default()),
                    }
                },
            )
        });
    let manifest_files = source_result?;
    let cache_walk = cache_walk_result?;
    let cache_files_paths = cache_walk.included_paths;
    let cache_symlink_entries = cache_walk.symlinks;
    // Entries that survived the stat+hash pass, each carrying its own path:
    // files that vanished mid-pass are dropped *with* their path, so the
    // tar append below can never zip a surviving path against another
    // file's metadata (#3533).
    let cache_manifest_entries =
        build_cache_manifest_entries(&pool, opts.cache_dir, &cache_files_paths, projection)?;
    let mut cache_manifest_files = Vec::with_capacity(cache_manifest_entries.len());
    let mut cache_tar_entries: Vec<(PathBuf, String, std::fs::Metadata)> =
        Vec::with_capacity(cache_manifest_entries.len());
    for (abs, entry, meta) in cache_manifest_entries {
        cache_tar_entries.push((abs, entry.path.clone(), meta));
        cache_manifest_files.push(entry);
    }

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        saved_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        workspace: opts
            .workspace
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        // Empty cache_dir_name signals an mtimes-only archive to any
        // future reader that wants to short-circuit the cache-extract
        // path without re-parsing the SaveOptions. The existing reader
        // tolerates either value because it strips the CACHE_DIR_NAME
        // prefix only when a `cache/...` entry shows up in the tar.
        cache_dir_name: if opts.mtimes_only {
            String::new()
        } else {
            CACHE_DIR_NAME.into()
        },
        source_file_count: manifest_files.len() as u64,
        cache_file_count: cache_manifest_files.len() as u64,
        files: manifest_files,
        cache_layer_kind: CacheLayerKind::Complete as i32,
        cache_files: cache_manifest_files,
        base_manifest_blake3: Vec::new(),
        deleted_cache_paths: Vec::new(),
        cache_symlinks: cache_symlink_entries,
    };

    let manifest_bytes = {
        let mut buf = Vec::with_capacity(manifest.encoded_len());
        manifest.encode(&mut buf)?;
        buf
    };

    // Stream tar -> zstd encoder -> file. We append the manifest first
    // (cheap, ~hundreds of KB) and the cache files second so a streaming
    // load can read the manifest without buffering the whole archive.
    let mut cache_files: u64 = 0;
    write_archive_file(opts.out, |out_file| {
    let out_buf = BufWriter::with_capacity(8 * 1024 * 1024, out_file);
    let mut zstd_encoder =
        zstd::stream::write::Encoder::new(out_buf, opts.zstd_level).map_err(SaveLoadError::Zstd)?;
    zstd_encoder
        .multithread(num_cpus_for(opts.threads))
        .map_err(SaveLoadError::Zstd)?;

    {
        let mut tar_builder = tar::Builder::new(&mut zstd_encoder);
        tar_builder.mode(tar::HeaderMode::Deterministic);

        // 1) manifest as a regular file at archive root
        {
            append_manifest_entry(&mut tar_builder, &manifest, &manifest_bytes)?;
        }

        // 2) cache dir contents under `cache/`.
        //
        // Cache file list was already enumerated in parallel above,
        // concurrent with the source-file walk. We just stream those
        // files into tar here. The tar writer feeds the multithreaded
        // zstd encoder, which does the heavy CPU work in its own
        // thread pool.
        if !cache_tar_entries.is_empty() {
            // cache_tar_entries is non-empty only when we enumerated a
            // real cache_dir above (i.e. not the mtimes_only branch),
            // so this expect() is unreachable in practice.
            let cache_dir = opts
                .cache_dir
                .expect("cache_tar_entries non-empty implies cache_dir was set");
            for (abs, relative, meta) in &cache_tar_entries {
                // Counts what actually landed in the archive: a file the
                // daemon renamed away between the manifest pass and this
                // append is skipped, not fatal, and not counted (#3533).
                let appended = if projection.is_some() {
                    append_cache_file_entry_at(&mut tar_builder, abs, Path::new(relative), meta)?
                } else {
                    append_cache_file_entry(&mut tar_builder, cache_dir, abs, meta)?
                };
                if appended {
                    cache_files += 1;
                }
            }
        }
        tar_builder.finish().map_err(SaveLoadError::BareIo)?;
    }

    let writer = zstd_encoder.finish().map_err(SaveLoadError::Zstd)?;
    writer
        .into_inner()
        .map_err(|e| SaveLoadError::BareIo(e.into_error()))
    })?;

    let archive_bytes = std::fs::metadata(opts.out).map(|m| m.len()).unwrap_or(0);

    Ok(SaveReport {
        profile: opts.profile,
        source_files: manifest.source_file_count,
        cache_files,
        deleted_cache_files: 0,
        excluded_files: cache_walk.excluded_files,
        excluded_bytes: cache_walk.excluded_bytes,
        archive_bytes,
        elapsed_ms: start.elapsed().as_millis() as u64,
        cache_symlinks: manifest.cache_symlinks.len() as u64,
        cache_symlinks_skipped: cache_walk.skipped_symlinks,
    })
}

pub fn save_delta(opts: &SaveDeltaOptions<'_>) -> Result<SaveReport> {
    save_delta_with_projection(opts, None)
}

/// Compare projected archive paths with the base using the same delta writer.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub fn save_delta_with_projection(
    opts: &SaveDeltaOptions<'_>,
    projection: Option<&CacheProjection<'_>>,
) -> Result<SaveReport> {
    if let Some(projection) = projection {
        projection.validate(opts.cache_dir)?;
    }
    let start = std::time::Instant::now();
    let pool = build_pool(opts.threads)?;

    let (source_result, cache_walk_result): (Result<Vec<SourceFile>>, Result<CacheWalk>) = pool
        .install(|| {
            rayon::join(
                || -> Result<Vec<SourceFile>> {
                    let Some(ws) = opts.workspace else {
                        return Ok(Vec::new());
                    };
                    let files = workspace_files_for_save(ws, opts.threads)?;
                    files
                        .par_iter()
                        .map(|rel| -> Result<SourceFile> {
                            let abs = ws.join(rel);
                            let meta = std::fs::metadata(&abs).map_err(|e| io(&abs, e))?;
                            let hash = hash_file(&abs)?;
                            Ok(SourceFile {
                                path: rel_to_posix(rel),
                                mtime_ms: mtime_ms(&meta),
                                mtime_ns: Some(mtime_ns(&meta)),
                                size: meta.len(),
                                blake3: hash.to_vec(),
                            })
                        })
                        .collect()
                },
                || -> Result<CacheWalk> {
                    if opts.cache_dir.exists() {
                        walk_cache_with_projection(opts.cache_dir, opts.threads, opts.profile, projection)
                    } else {
                        Ok(CacheWalk::default())
                    }
                },
            )
        });
    let manifest_files = source_result?;
    let cache_walk = cache_walk_result?;
    let cache_files_paths = cache_walk.included_paths;
    // Each entry carries the path it was hashed from, so a file that
    // vanished mid-pass is dropped together with its path and the map
    // below can never pair a surviving path with another file's metadata
    // (#3533).
    let cache_manifest_entries =
        build_cache_manifest_entries(&pool, Some(opts.cache_dir), &cache_files_paths, projection)?;

    let base_by_path: BTreeMap<&str, &CacheFile> = opts
        .base_manifest
        .cache_files
        .iter()
        .filter(|entry| !manifest_path_is_daemon_runtime(&entry.path))
        .filter(|entry| !projection.is_some_and(|projection| projection.protects_manifest_path(&entry.path)))
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let current_by_path: BTreeMap<&str, (&CacheFile, &PathBuf, &std::fs::Metadata)> =
        cache_manifest_entries
            .iter()
            .map(|(path, entry, meta)| (entry.path.as_str(), (entry, path, meta)))
            .collect();

    let mut delta_entries = Vec::new();
    let mut delta_paths = Vec::new();
    for (path, (entry, abs, meta)) in &current_by_path {
        match base_by_path.get(path) {
            Some(base) if cache_file_metadata_matches(base, entry) => {}
            Some(base) if cache_file_content_matches(base, entry) => {
                delta_entries.push((*entry).clone());
            }
            _ => {
                delta_entries.push((*entry).clone());
                delta_paths.push(((*abs).clone(), entry.path.clone(), (*meta).clone()));
            }
        }
    }

    let current_paths: BTreeSet<&str> = current_by_path.keys().copied().collect();
    let mut deleted_cache_paths: Vec<String> = base_by_path
        .keys()
        .copied()
        .filter(|path| !current_paths.contains(path))
        .map(ToOwned::to_owned)
        .collect();

    // Symlink tombstones (#1548): a link present in the base layer but
    // absent from the current cache tree (and not replaced by a regular
    // file, which extraction would overwrite anyway) must be removed on
    // load, exactly like a deleted regular file.
    let current_symlink_paths: BTreeSet<&str> = cache_walk
        .symlinks
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    for base_link in &opts.base_manifest.cache_symlinks {
        let path = base_link.path.as_str();
        if manifest_path_is_daemon_runtime(path)
            || projection.is_some_and(|projection| projection.protects_manifest_path(path))
        {
            continue;
        }
        if !current_symlink_paths.contains(path) && !current_paths.contains(path) {
            deleted_cache_paths.push(path.to_owned());
        }
    }
    deleted_cache_paths.sort();
    deleted_cache_paths.dedup();

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        saved_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        workspace: opts
            .workspace
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        cache_dir_name: CACHE_DIR_NAME.into(),
        source_file_count: manifest_files.len() as u64,
        cache_file_count: delta_entries.len() as u64,
        files: manifest_files,
        cache_layer_kind: CacheLayerKind::Delta as i32,
        cache_files: delta_entries,
        base_manifest_blake3: manifest_digest(opts.base_manifest)?,
        deleted_cache_paths,
        // Deltas carry the FULL current symlink set (entries are a few
        // bytes each): load recreates them idempotently, so base-vs-delta
        // diffing buys nothing but complexity.
        cache_symlinks: cache_walk.symlinks.clone(),
    };

    write_delta_archive(
        opts.out,
        opts.zstd_level,
        opts.threads,
        &manifest,
        &delta_paths,
    )?;
    let archive_bytes = std::fs::metadata(opts.out).map(|m| m.len()).unwrap_or(0);

    Ok(SaveReport {
        profile: opts.profile,
        source_files: manifest.source_file_count,
        cache_files: manifest.cache_file_count,
        deleted_cache_files: manifest.deleted_cache_paths.len() as u64,
        excluded_files: cache_walk.excluded_files,
        excluded_bytes: cache_walk.excluded_bytes,
        archive_bytes,
        elapsed_ms: start.elapsed().as_millis() as u64,
        cache_symlinks: manifest.cache_symlinks.len() as u64,
        cache_symlinks_skipped: cache_walk.skipped_symlinks,
    })
}

/// Hash + stat every cache file in parallel. Output order matches
/// `cache_files_paths` (rayon's indexed collect preserves order), and each
/// result carries the path it came from: files that vanish between the
/// walk, the stat, and the hash are dropped as a whole `(path, entry,
/// meta)` triple (#3533), so callers can never zip a surviving path
/// against another file's metadata.
fn build_cache_manifest_entries(
    pool: &rayon::ThreadPool,
    cache_dir: Option<&Path>,
    cache_files_paths: &[PathBuf],
    projection: Option<&CacheProjection<'_>>,
) -> Result<Vec<(PathBuf, CacheFile, std::fs::Metadata)>> {
    let Some(cache_dir) = cache_dir else {
        return Ok(Vec::new());
    };
    pool.install(|| {
        cache_files_paths
            .par_iter()
            .filter_map(|abs| {
                let result = match projection {
                    Some(projection) => projection.relative_path(cache_dir, abs)
                        .and_then(|relative| cache_file_entry_at(abs, &relative)),
                    None => cache_file_entry(cache_dir, abs),
                };
                result
                    .transpose()
                    .map(|result| result.map(|(entry, meta)| (abs.clone(), entry, meta)))
            })
            .collect()
    })
}

fn cache_file_metadata_matches(left: &CacheFile, right: &CacheFile) -> bool {
    left.size == right.size && left.mtime_ns == right.mtime_ns && left.blake3 == right.blake3
}

fn cache_file_content_matches(left: &CacheFile, right: &CacheFile) -> bool {
    left.size == right.size && left.blake3 == right.blake3
}

fn encode_manifest(manifest: &Manifest) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(manifest.encoded_len());
    manifest.encode(&mut buf)?;
    Ok(buf)
}

fn append_manifest_entry<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    manifest: &Manifest,
    manifest_bytes: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(manifest.saved_at_ms.max(0) as u64 / 1000);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar_builder
        .append_data(&mut header, MANIFEST_NAME, manifest_bytes)
        .map_err(SaveLoadError::BareIo)
}

/// Append one cache file into the tar. `meta` comes from the manifest
/// pre-pass ([`cache_file_entry`]) so the file is stat'd exactly once
/// per save and the tar header always agrees with the manifest (#1541).
///
/// Returns `Ok(false)` — appended nothing — when the file was renamed or
/// removed between the manifest pre-pass and this open (#3533). The
/// window cannot be closed: the daemon is rewriting its tree while we
/// archive it, and a file that no longer exists is not worth failing a
/// whole save over. `load` already tolerates a manifest entry with no
/// payload (it is the delta-metadata-only shape), so the archive stays
/// loadable; any other open error still fails loudly.
fn append_cache_file_entry<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    cache_dir: &Path,
    abs: &Path,
    meta: &std::fs::Metadata,
) -> Result<bool> {
    let rel = abs
        .strip_prefix(cache_dir)
        .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
    append_cache_file_entry_at(tar_builder, abs, rel, meta)
}

fn append_cache_file_entry_at<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    abs: &Path,
    rel: &Path,
    meta: &std::fs::Metadata,
) -> Result<bool> {
    let mut archive_path = PathBuf::from(CACHE_DIR_NAME);
    archive_path.push(rel);
    let archive_path_str = rel_to_posix(&archive_path);
    let mut file = match File::open(abs) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                event = "save_append_vanished",
                path = %abs.display(),
                "file vanished between manifest pass and append; skipped"
            );
            return Ok(false);
        }
        Err(e) => return Err(io(abs, e)),
    };
    let mut header = tar::Header::new_gnu();
    header.set_metadata(meta);
    header.set_cksum();
    tar_builder
        .append_data(&mut header, archive_path_str, &mut file)
        .map_err(SaveLoadError::BareIo)?;
    Ok(true)
}

fn manifest_digest(manifest: &Manifest) -> Result<Vec<u8>> {
    Ok(zccache::hash::hash_bytes(&encode_manifest(manifest)?)
        .as_bytes()
        .to_vec())
}

fn write_delta_archive(
    out: &Path,
    zstd_level: i32,
    threads: Option<usize>,
    manifest: &Manifest,
    cache_files_paths: &[(PathBuf, String, std::fs::Metadata)],
) -> Result<()> {
    let manifest_bytes = encode_manifest(manifest)?;
    write_archive_file(out, |out_file| {
    let out_buf = BufWriter::with_capacity(8 * 1024 * 1024, out_file);
    let mut zstd_encoder =
        zstd::stream::write::Encoder::new(out_buf, zstd_level).map_err(SaveLoadError::Zstd)?;
    zstd_encoder
        .multithread(num_cpus_for(threads))
        .map_err(SaveLoadError::Zstd)?;

    {
        let mut tar_builder = tar::Builder::new(&mut zstd_encoder);
        tar_builder.mode(tar::HeaderMode::Deterministic);

        append_manifest_entry(&mut tar_builder, manifest, &manifest_bytes)?;

        for (abs, relative, meta) in cache_files_paths {
            append_cache_file_entry_at(&mut tar_builder, abs, Path::new(relative), meta)?;
        }
        tar_builder.finish().map_err(SaveLoadError::BareIo)?;
    }

    let writer = zstd_encoder.finish().map_err(SaveLoadError::Zstd)?;
    writer
        .into_inner()
        .map_err(|e| SaveLoadError::BareIo(e.into_error()))
    })
}

/// Produce the archive at `out` atomically (soldr#3687): `write` streams into
/// a temp file in `out`'s directory, which is synced and renamed over `out`
/// only on success. A failed save leaves any previous archive intact, and
/// the temp file is removed when dropped, so no partial archive remains for
/// a CI cache step to upload.
fn write_archive_file<T>(out: &Path, write: impl FnOnce(File) -> Result<T>) -> Result<()> {
    let parent = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    // tempfile creates 0o600; request what a plain `File::create` would
    // (0o666 under the umask) so the renamed archive stays readable by
    // other users, e.g. a cache step running as another UID (soldr#3712).
    let mut builder = tempfile::Builder::new();
    builder.prefix(".soldr-save-").suffix(".tmp");
    if let Some(perms) = soldr_platform::fs::permissions::default_create_permissions() {
        builder.permissions(perms);
    }
    let tmp = builder.tempfile_in(parent).map_err(|e| io(parent, e))?;
    let handle = tmp.reopen().map_err(|e| io(tmp.path(), e))?;
    write(handle)?;
    tmp.as_file().sync_all().map_err(|e| io(tmp.path(), e))?;
    // Replacing an existing archive keeps that archive's mode.
    if let Some(existing) = soldr_platform::fs::permissions::mode(out) {
        soldr_platform::fs::permissions::restore_mode(tmp.path(), Some(existing & 0o7777))
            .map_err(|e| io(tmp.path(), e))?;
    }
    tmp.persist(out).map_err(|e| io(out, e.error))?;
    Ok(())
}

pub fn read_manifest_from_archive(archive: &Path) -> Result<Manifest> {
    let in_file = File::open(archive).map_err(|e| io(archive, e))?;
    let buf = BufReader::with_capacity(16 * 1024 * 1024, in_file);
    let zstd_reader = zstd::stream::read::Decoder::new(buf).map_err(SaveLoadError::Zstd)?;
    let mut tar_reader = tar::Archive::new(zstd_reader);
    for entry in tar_reader.entries().map_err(SaveLoadError::BareIo)? {
        let mut entry = entry.map_err(SaveLoadError::BareIo)?;
        let path = entry.path().map_err(SaveLoadError::BareIo)?.into_owned();
        if path.as_os_str() != MANIFEST_NAME {
            continue;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(SaveLoadError::BareIo)?;
        return Ok(Manifest::decode(&buf[..])?);
    }
    Err(SaveLoadError::MissingManifest)
}

pub fn read_manifest_file(path: &Path) -> Result<Manifest> {
    let bytes = std::fs::read(path).map_err(|e| io(path, e))?;
    Ok(Manifest::decode(&bytes[..])?)
}

pub fn write_manifest_file(path: &Path, manifest: &Manifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
    }
    let bytes = encode_manifest(manifest)?;
    std::fs::write(path, bytes).map_err(|e| io(path, e))
}

// ---------- load ----------
