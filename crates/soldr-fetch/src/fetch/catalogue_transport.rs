use std::collections::{HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sha2::Digest;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use url::Url;

use super::catalogue_lookup::MANIFEST_FETCH_TIMEOUT;
use super::catalogue_model::{AssetTransport, ManifestEntry, Part};
use crate::core::{SoldrError, SoldrPaths};
use crate::platform::sync::AsyncMutex as Mutex;
/// Materialize either catalogue transport through the one blessed asset
/// streaming boundary.  A part is deliberately one request: catalogue parts
/// are already immutable content-addressed chunks, so applying the legacy
/// range segmenter to them would multiply requests and defeat mirror/cache
/// accounting.  The scheduler below owns the only inter-part concurrency:
/// each logical part is one request and the process-wide Bulk pool limits
/// all assets together.  Direct assets still use the legacy Range path.
pub(crate) async fn materialize_catalogue_entry(
    paths: &SoldrPaths,
    entry: &ManifestEntry,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let cache = catalogue_cache_root(paths)?;
    let object = cache.join("assets").join(&entry.sha256);
    if let Some(asset) = cached_asset(&object, &entry.sha256, entry.size_bytes)? {
        return Ok(asset);
    }
    // The lock covers the recheck and promotion, not just the write.  This
    // makes concurrent processes converge on one verified object and means a
    // killed downloader leaves only an ignorable `.tmp-*` file.
    let _lock = CacheLock::acquire(&object).await?;
    if let Some(asset) = cached_asset(&object, &entry.sha256, entry.size_bytes)? {
        return Ok(asset);
    }
    match &entry.transport {
        AssetTransport::Direct { urls } => {
            let asset = download_from_mirrors(urls).await?;
            verify_catalogue_asset_sha256(entry, asset.sha256())?;
            if !expected_size_matches(entry.size_bytes, asset.bytes()) {
                return Err(SoldrError::Other(format!(
                    "catalogue asset size mismatch for {}: expected {}, got {}",
                    entry.asset,
                    entry.size_bytes,
                    asset.bytes()
                )));
            }
            promote_cached_asset(&cache.join("assets"), &entry.sha256, asset.path())?;
            cached_asset(&object, &entry.sha256, entry.size_bytes)?.ok_or_else(|| {
                SoldrError::Other("catalogue asset vanished after cache promotion".into())
            })
        }
        AssetTransport::Multipart { parts } => {
            materialize_catalogue_parts(&cache, parts).await?;
            let mut output = tempfile::NamedTempFile::new_in(cache.join("assets"))?;
            let mut hasher = sha2::Sha256::new();
            let mut bytes = 0u64;
            for part in parts {
                let object = cache.join("parts").join(&part.sha256);
                let mut body = std::fs::File::open(&object)?;
                let copied = copy_and_hash(&mut body, &mut output, &mut hasher)?;
                bytes = bytes.saturating_add(copied);
            }
            output.flush()?;
            let sha256 = hex::encode(hasher.finalize());
            if sha256 != entry.sha256 || bytes != entry.size_bytes {
                return Err(SoldrError::Other(format!(
                    "catalogue multipart asset {} failed final size/hash verification",
                    entry.asset
                )));
            }
            promote_named_temp(&cache.join("assets"), &entry.sha256, output)?;
            cached_asset(&object, &entry.sha256, entry.size_bytes)?.ok_or_else(|| {
                SoldrError::Other("catalogue multipart asset vanished after cache promotion".into())
            })
        }
    }
}

pub(crate) const MULTIPART_INITIAL_WINDOW: usize = 4;
pub(crate) const MULTIPART_MAX_WINDOW: usize = 16;
pub(crate) const MAX_ORIGIN_WINDOWS: usize = 64;
pub(crate) const MAX_MULTIPART_RETRY_AFTER: Duration = Duration::from_secs(60);

// One process-wide, asset-aware admission queue.  The Bulk socket pool is
// still acquired by the actual stream operation; this coordinator merely
// decides which asset may *start* its next logical part.  Rotating an asset
// after every grant prevents a 16-part manifest from parking all of its work
// ahead of a second asset.
static PART_COORDINATOR: OnceLock<Arc<PartCoordinator>> = OnceLock::new();
static NEXT_PART_JOB: AtomicU64 = AtomicU64::new(1);

pub(crate) struct PartCoordinator {
    cap: Option<usize>,
    state: Mutex<PartCoordinatorState>,
    changed: Notify,
}

#[derive(Default)]
pub(crate) struct PartCoordinatorState {
    inflight: usize,
    jobs: VecDeque<u64>,
    live: HashSet<u64>,
    pending: std::collections::HashMap<u64, usize>,
    origins: std::collections::HashMap<String, MultipartWindow>,
}

pub(crate) struct PartJob {
    pub(crate) id: u64,
    coordinator: Arc<PartCoordinator>,
}

pub(crate) struct PartAdmission {
    coordinator: Arc<PartCoordinator>,
}

struct PartWaiter {
    job: u64,
    coordinator: Arc<PartCoordinator>,
    queued: bool,
}

impl PartCoordinator {
    fn bounded_origin<'a>(
        state: &'a mut PartCoordinatorState,
        origin: &str,
    ) -> &'a mut MultipartWindow {
        if !state.origins.contains_key(origin) && state.origins.len() >= MAX_ORIGIN_WINDOWS {
            if let Some(oldest) = state.origins.keys().next().cloned() {
                state.origins.remove(&oldest);
            }
        }
        state
            .origins
            .entry(origin.to_string())
            .or_insert_with(MultipartWindow::new)
    }
    pub(crate) fn new(cap: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            cap,
            state: Mutex::new(PartCoordinatorState::default()),
            changed: Notify::new(),
        })
    }

    pub(crate) async fn register(self: &Arc<Self>) -> PartJob {
        let id = NEXT_PART_JOB.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().await;
        state.live.insert(id);
        drop(state);
        self.changed.notify_waiters();
        PartJob {
            id,
            coordinator: Arc::clone(self),
        }
    }

    pub(crate) async fn acquire(self: &Arc<Self>, job: u64) -> PartAdmission {
        let mut waiter = PartWaiter {
            job,
            coordinator: Arc::clone(self),
            queued: true,
        };
        {
            let mut state = self.state.lock().await;
            let first = {
                let count = state.pending.entry(job).or_default();
                let first = *count == 0;
                *count += 1;
                first
            };
            if first {
                state.jobs.push_back(job);
            }
        }
        self.changed.notify_waiters();
        loop {
            let notified = self.changed.notified();
            let mut state = self.state.lock().await;
            let allowed = self.cap.is_none_or(|cap| state.inflight < cap)
                && state.jobs.front().copied() == Some(job);
            if allowed {
                state.inflight += 1;
                // Consume exactly one queued request.  A job remains at the
                // tail only while it has another waiter; idle registered jobs
                // are absent from this queue and can never block admission.
                state.jobs.pop_front();
                let count = state.pending.get_mut(&job).expect("front job has a waiter");
                *count -= 1;
                if *count == 0 {
                    state.pending.remove(&job);
                } else {
                    state.jobs.push_back(job);
                }
                waiter.queued = false;
                return PartAdmission {
                    coordinator: Arc::clone(self),
                };
            }
            drop(state);
            notified.await;
        }
    }

    async fn release(&self) {
        let mut state = self.state.lock().await;
        state.inflight = state.inflight.saturating_sub(1);
        drop(state);
        self.changed.notify_waiters();
    }

    async fn unregister(&self, job: u64) {
        let mut state = self.state.lock().await;
        state.live.remove(&job);
        state.jobs.retain(|id| *id != job);
        state.pending.remove(&job);
        drop(state);
        self.changed.notify_waiters();
    }

    pub(crate) async fn origin_window(&self, origin: &str) -> MultipartWindow {
        let mut state = self.state.lock().await;
        Self::bounded_origin(&mut state, origin).clone()
    }

    pub(crate) async fn healthy_origin(&self, origin: &str) -> MultipartWindow {
        let mut state = self.state.lock().await;
        let window = Self::bounded_origin(&mut state, origin);
        window.healthy();
        window.clone()
    }

    pub(crate) async fn congested_origin(
        &self,
        origin: &str,
        retry_after: Option<Duration>,
    ) -> MultipartWindow {
        let mut state = self.state.lock().await;
        let window = Self::bounded_origin(&mut state, origin);
        window.congested();
        if let Some(delay) = retry_after {
            window.cooldown = window.cooldown.max(delay);
        }
        window.clone()
    }
}

impl PartCoordinator {
    async fn cancel_waiter(&self, job: u64) {
        let mut state = self.state.lock().await;
        if let Some(count) = state.pending.get_mut(&job) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.pending.remove(&job);
                state.jobs.retain(|id| *id != job);
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

impl Drop for PartAdmission {
    fn drop(&mut self) {
        if let Some(mut state) = self.coordinator.state.try_lock() {
            state.inflight = state.inflight.saturating_sub(1);
            drop(state);
            self.coordinator.changed.notify_waiters();
            return;
        }
        let coordinator = Arc::clone(&self.coordinator);
        // No await is available in Drop.  Spawn is safe here: this object is
        // only created under Tokio by the multipart downloader, and the task
        // releases the logical admission even when cancelled or panicked.
        tokio::spawn(async move { coordinator.release().await });
    }
}

impl Drop for PartWaiter {
    fn drop(&mut self) {
        if !self.queued {
            return;
        }
        if let Some(mut state) = self.coordinator.state.try_lock() {
            if let Some(count) = state.pending.get_mut(&self.job) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.pending.remove(&self.job);
                    state.jobs.retain(|id| *id != self.job);
                }
            }
            drop(state);
            self.coordinator.changed.notify_waiters();
            return;
        }
        let coordinator = Arc::clone(&self.coordinator);
        let job = self.job;
        tokio::spawn(async move { coordinator.cancel_waiter(job).await });
    }
}

impl Drop for PartJob {
    fn drop(&mut self) {
        if let Some(mut state) = self.coordinator.state.try_lock() {
            state.live.remove(&self.id);
            state.jobs.retain(|id| *id != self.id);
            state.pending.remove(&self.id);
            drop(state);
            self.coordinator.changed.notify_waiters();
            return;
        }
        let coordinator = Arc::clone(&self.coordinator);
        let job = self.id;
        tokio::spawn(async move { coordinator.unregister(job).await });
    }
}

fn part_coordinator() -> Arc<PartCoordinator> {
    Arc::clone(
        PART_COORDINATOR
            .get_or_init(|| PartCoordinator::new(super::segmented_download::bulk_pool_capacity())),
    )
}

/// AIMD state is deliberately small and deterministic.  A healthy completed
/// part adds one slot; overload/timeout/reset feedback halves it and pauses
/// new work.  The pool remains the process-wide hard cap, while this window
/// prevents one origin from stampeding before it has demonstrated health.
#[derive(Debug, Clone)]
pub(crate) struct MultipartWindow {
    pub(crate) current: usize,
    pub(crate) cooldown: Duration,
}

impl MultipartWindow {
    pub(crate) fn new() -> Self {
        Self {
            current: MULTIPART_INITIAL_WINDOW,
            cooldown: Duration::ZERO,
        }
    }
    pub(super) fn healthy(&mut self) {
        self.current = (self.current + 1).min(MULTIPART_MAX_WINDOW);
    }
    pub(super) fn congested(&mut self) {
        self.current = (self.current / 2).max(1);
        self.cooldown = Duration::from_millis(250);
    }
}

fn multipart_origin(parts: &[Part]) -> String {
    parts
        .first()
        .and_then(|part| part.urls.first())
        .and_then(|url| Url::parse(url).ok())
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| "unknown-origin".into())
}

pub(crate) fn congestion_error(error: &SoldrError) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("429")
        || text.contains("503")
        || text.contains("retry-after")
        || text.contains("timed out")
        || text.contains("stalled")
        || text.contains("interrupted")
        || text.contains("reset")
}

/// Start in manifest order and replenish only after a completion.  Retries
/// live here, rather than inside a worker, so each individual overload,
/// timeout, stall, or reset changes the AIMD window before another request is
/// admitted.  This also keeps a retry at the back of the local ready queue:
/// a sick part cannot monopolize the window.
pub(crate) async fn materialize_catalogue_parts(
    cache: &Path,
    parts: &[Part],
) -> Result<(), SoldrError> {
    let origin = multipart_origin(parts);
    let coordinator = part_coordinator();
    let mut window = coordinator.origin_window(&origin).await;
    let job = Arc::new(coordinator.register().await);
    let mut ready = (0..parts.len()).collect::<VecDeque<_>>();
    let mut attempts = vec![0u32; parts.len()];
    let mut inflight = JoinSet::new();
    while !ready.is_empty() || !inflight.is_empty() {
        while inflight.len() < window.current {
            let Some(index) = ready.pop_front() else {
                break;
            };
            let cache = cache.to_owned();
            let part = parts[index].clone();
            let job = Arc::clone(&job);
            inflight.spawn(async move {
                let admission = job.coordinator.acquire(job.id).await;
                let result = materialize_catalogue_part(&cache, &part).await;
                drop(admission);
                (index, result)
            });
        }
        let result = inflight
            .join_next()
            .await
            .expect("non-empty multipart join set");
        match result {
            Ok((_, Ok(_))) => {
                window = coordinator.healthy_origin(&origin).await;
            }
            Ok((index, Err(error))) => {
                if congestion_error(&error) {
                    window = coordinator
                        .congested_origin(&origin, retry_after(&error))
                        .await;
                }
                attempts[index] += 1;
                if super::retry::is_transient(&error)
                    && attempts[index] < super::retry::FETCH_ATTEMPTS
                {
                    let delay = window.cooldown;
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    ready.push_back(index);
                } else {
                    inflight.abort_all();
                    while inflight.join_next().await.is_some() {}
                    return Err(error);
                }
            }
            Err(error) => {
                inflight.abort_all();
                while inflight.join_next().await.is_some() {}
                return Err(SoldrError::Network(format!(
                    "catalogue part task cancelled: {error}"
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn retry_after(error: &SoldrError) -> Option<Duration> {
    let text = error.to_string();
    let (_, value) = text.split_once("Retry-After:")?;
    let seconds = value.split_whitespace().next()?.parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds).min(MAX_MULTIPART_RETRY_AFTER))
}

/// Content-addressed persistent cache below the caller's [`SoldrPaths::cache`].
/// Cached bytes are never trusted by their filename: every reuse hashes the
/// file and checks its exact advertised length before returning it to an
/// archive reader.
fn catalogue_cache_root(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    let root = paths.cache.join("catalogue-v2");
    std::fs::create_dir_all(root.join("assets"))?;
    std::fs::create_dir_all(root.join("parts"))?;
    Ok(root)
}

pub(crate) async fn materialize_catalogue_part(
    cache: &Path,
    part: &Part,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let object = cache.join("parts").join(&part.sha256);
    if let Some(asset) = cached_asset(&object, &part.sha256, part.size_bytes)? {
        return Ok(asset);
    }
    let _lock = CacheLock::acquire(&object).await?;
    if let Some(asset) = cached_asset(&object, &part.sha256, part.size_bytes)? {
        return Ok(asset);
    }
    let downloaded = download_part_from_mirrors(&part.urls, &object, part.size_bytes).await?;
    if downloaded.sha256() != part.sha256 || downloaded.bytes() != part.size_bytes {
        return Err(SoldrError::Other(format!(
            "catalogue part {} failed integrity verification",
            part.number
        )));
    }
    promote_cached_asset(&cache.join("parts"), &part.sha256, downloaded.path())?;
    // The verified content-addressed object is now durable.  Any old resume
    // state is strictly unverified and must not survive as an alternative.
    let _ = std::fs::remove_file(object.with_extension("partial"));
    let _ = std::fs::remove_file(object.with_extension("partial.range"));
    cached_asset(&object, &part.sha256, part.size_bytes)?
        .ok_or_else(|| SoldrError::Other("catalogue part vanished after cache promotion".into()))
}

/// Same availability-only mirror policy as full assets, but deliberately uses
/// the no-segmentation stream boundary.  A checksum mismatch remains fatal
/// in `materialize_catalogue_part` and therefore never falls through here.
pub(crate) async fn download_part_from_mirrors(
    urls: &[String],
    object: &Path,
    expected_bytes: u64,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let mut last = None;
    for url in urls {
        match download_catalogue_part(url, object, expected_bytes).await {
            Ok(asset) => return Ok(asset),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| SoldrError::Other("catalogue transport has no mirrors".into())))
}

pub(crate) fn cached_asset(
    path: &Path,
    expected_sha256: &str,
    expected_bytes: u64,
) -> Result<Option<super::stream_download::DownloadedAsset>, SoldrError> {
    if !path.is_file() {
        return Ok(None);
    }
    let (sha256, bytes) = sha256_file(path)?;
    if sha256 != expected_sha256 || !expected_size_matches(expected_bytes, bytes) {
        // A corrupted or interrupted object can never be observed as a hit.
        let _ = std::fs::remove_file(path);
        return Ok(None);
    }
    let mut temp = tempfile::NamedTempFile::new_in(soldr_core::core::ensure_temp_root())?;
    std::io::copy(&mut std::fs::File::open(path)?, &mut temp)?;
    temp.flush()?;
    Ok(Some(super::stream_download::DownloadedAsset {
        file: temp,
        sha256,
        bytes,
    }))
}

fn sha256_file(path: &Path) -> Result<(String, u64), SoldrError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let bytes = std::io::copy(&mut file, &mut hasher)?;
    Ok((hex::encode(hasher.finalize()), bytes))
}

pub(crate) fn copy_and_hash<W: Write>(
    reader: &mut std::fs::File,
    writer: &mut W,
    hasher: &mut sha2::Sha256,
) -> Result<u64, SoldrError> {
    use std::io::Read;
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        bytes = bytes.saturating_add(count as u64);
    }
    Ok(bytes)
}

pub(super) fn promote_cached_asset(
    dir: &Path,
    name: &str,
    source: &Path,
) -> Result<(), SoldrError> {
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::copy(&mut std::fs::File::open(source)?, &mut temp)?;
    temp.flush()?;
    promote_named_temp(dir, name, temp)
}

fn promote_named_temp(
    dir: &Path,
    name: &str,
    temp: tempfile::NamedTempFile,
) -> Result<(), SoldrError> {
    let final_path = dir.join(name);
    match temp.persist_noclobber(&final_path) {
        Ok(_) => Ok(()),
        Err(_error) if final_path.is_file() => Ok(()),
        Err(error) => Err(SoldrError::from(error.error)),
    }
}

/// Cross-process, async-friendly lock. `try_lock_exclusive` avoids blocking a
/// Tokio worker; filesystem locks are released by the OS after interruption.
struct CacheLock(std::fs::File);

impl CacheLock {
    pub(crate) async fn acquire(object: &Path) -> Result<Self, SoldrError> {
        use fs2::FileExt;
        let lock_path = object.with_extension("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self(file)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                Err(error) => return Err(SoldrError::from(error)),
            }
        }
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// Mirrors are availability alternatives only.  A successfully transferred
/// object with a bad digest is a publication/integrity failure and must never
/// be hidden by trying a different mirror.
async fn download_from_mirrors(
    urls: &[String],
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let mut last = None;
    for url in urls {
        match download_catalogue_asset(url).await {
            Ok(asset) => return Ok(asset),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| SoldrError::Other("catalogue transport has no mirrors".into())))
}

/// Download a catalogue-pinned asset, retrying transient failures.
///
/// soldr#2132: the last unretried sender in this crate. Reached from
/// `fetch_verified_catalogue_asset`, whose only caller is
/// `dylint_toolchain.rs` -- nothing above it retries, so wrapping here cannot
/// nest (unlike `archive.rs`, see the note at the top of that file).
///
/// The retry lives inside this leaf rather than at the two call sites so both
/// the first fetch and the cache-busted refresh below inherit it. sha256
/// verification happens in the caller and therefore stays outside the retry.
pub(crate) async fn download_catalogue_asset(
    url: &str,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let safe_url = super::stream_download::safe_asset_url(url);
    super::retry::with_backoff(&safe_url, || download_catalogue_asset_once(url)).await
}

pub(crate) async fn download_catalogue_asset_once(
    url: &str,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let client = super::stream_download::asset_http_client("catalogue asset download")?;
    let response = super::stream_download::send_asset_request(
        super::stream_download::get_request(&client, url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity"),
        url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    stream_catalogue_asset_body(response, url, MANIFEST_FETCH_TIMEOUT).await
}

pub(super) async fn download_catalogue_part(
    url: &str,
    object: &Path,
    expected_bytes: u64,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let safe_url = super::stream_download::safe_asset_url(url);
    let partial = object.with_extension("partial");
    let capability = object.with_extension("partial.range");
    let prefix = partial.metadata().map(|meta| meta.len()).unwrap_or(0);
    if prefix > 0 {
        // The first tail request is itself the capability probe.  Nothing is
        // appended or persisted as Range-capable until its exact 206 evidence
        // has been checked in `download_catalogue_part_tail`.
        match download_catalogue_part_tail(url, &partial, &capability, prefix, expected_bytes).await
        {
            Ok(downloaded) => return Ok(downloaded),
            Err(TailFailure::Retry(error)) => return Err(error),
            Err(TailFailure::RestartWhole) => {
                let _ = std::fs::remove_file(&partial);
                let _ = std::fs::remove_file(&capability);
            }
        }
    }

    // A partial whole GET is never a completed artefact.  Its prefix remains
    // unverified; `Accept-Ranges` is deliberately not saved as capability.
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&partial)?;
    match download_catalogue_part_response(url, None, &mut file).await {
        Ok((bytes, _range_advertised)) if bytes == expected_bytes => {
            let _ = std::fs::remove_file(&capability);
            downloaded_from_partial(&partial)
        }
        Ok((bytes, _range_advertised)) => Err(SoldrError::Network(format!(
            "catalogue part {safe_url} ended short: {bytes}/{expected_bytes} bytes"
        ))),
        // A transport/stall error does not leave us trustworthy header
        // evidence.  Retain the bytes only as an unverified scratch file;
        // the next scheduler attempt restarts a whole GET rather than issuing
        // a tail Range on the basis of an advertisement we never received.
        Err(error) => Err(error),
    }
}

pub(crate) async fn download_catalogue_part_response(
    url: &str,
    range: Option<(u64, u64)>,
    file: &mut std::fs::File,
) -> Result<(u64, bool), SoldrError> {
    let client = super::stream_download::asset_http_client_no_redirect("catalogue part download")?;
    let mut request = super::stream_download::get_request(&client, url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity");
    if let Some((start, end)) = range {
        request = request.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
    }
    let response =
        super::stream_download::send_asset_request(request, url, MANIFEST_FETCH_TIMEOUT).await?;
    let range_advertised = response
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("bytes"));
    let bytes =
        super::stream_download::stream_catalogue_part_into_file(response, url, file).await?;
    Ok((bytes, range_advertised))
}

pub(crate) enum TailFailure {
    Retry(SoldrError),
    RestartWhole,
}

pub(crate) async fn download_catalogue_part_tail(
    url: &str,
    partial: &Path,
    capability: &Path,
    start: u64,
    expected_bytes: u64,
) -> Result<super::stream_download::DownloadedAsset, TailFailure> {
    if start >= expected_bytes {
        return Err(TailFailure::RestartWhole);
    }
    let end = expected_bytes - 1;
    let client =
        super::stream_download::asset_http_client_no_redirect("catalogue part tail resume")
            .map_err(TailFailure::Retry)?;
    let response = super::stream_download::send_asset_request(
        super::stream_download::get_request(&client, url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .header(reqwest::header::RANGE, format!("bytes={start}-{end}")),
        url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await
    .map_err(TailFailure::Retry)?;
    if !valid_part_tail_response(&response, start, end, expected_bytes) {
        return Err(TailFailure::RestartWhole);
    }
    // This marker contains no URL or request metadata (which could include
    // query credentials).  It records only that this SHA-keyed object saw an
    // exact valid 206 before any tail bytes were appended.
    std::fs::write(capability, b"validated-206\n")
        .map_err(SoldrError::from)
        .map_err(TailFailure::Retry)?;
    let mut file = OpenOptions::new()
        .append(true)
        .open(partial)
        .map_err(SoldrError::from)
        .map_err(TailFailure::Retry)?;
    super::stream_download::stream_catalogue_part_into_file(response, url, &mut file)
        .await
        .map_err(TailFailure::Retry)?;
    let bytes = partial
        .metadata()
        .map_err(SoldrError::from)
        .map_err(TailFailure::Retry)?
        .len();
    if bytes != expected_bytes {
        return Err(TailFailure::Retry(SoldrError::Network(format!(
            "catalogue part tail remained short: {bytes}/{expected_bytes}"
        ))));
    }
    downloaded_from_partial(partial).map_err(TailFailure::Retry)
}

fn valid_part_tail_response(
    response: &reqwest::Response,
    start: u64,
    end: u64,
    total: u64,
) -> bool {
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        || response.content_length() != Some(end - start + 1)
    {
        return false;
    }
    let Some(value) = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    value == format!("bytes {start}-{end}/{total}")
}

fn downloaded_from_partial(
    partial: &Path,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let mut temp = tempfile::NamedTempFile::new_in(soldr_core::core::ensure_temp_root())?;
    std::io::copy(&mut std::fs::File::open(partial)?, &mut temp)?;
    temp.flush()?;
    let (sha256, bytes) = sha256_file(partial)?;
    Ok(super::stream_download::DownloadedAsset {
        file: temp,
        sha256,
        bytes,
    })
}

pub(crate) async fn stream_catalogue_asset_body(
    response: reqwest::Response,
    url: &str,
    body_timeout: std::time::Duration,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let safe_url = super::stream_download::safe_asset_url(url);
    tokio::time::timeout(
        body_timeout,
        super::stream_download::stream_response_to_temp_file(
            response,
            url,
            super::stream_download::ASSET_IDLE_TIMEOUT,
        ),
    )
    .await
    .map_err(|_| SoldrError::Network(format!("asset body read timed out: {safe_url}")))?
}

pub(crate) fn cache_busted_url(url: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{url}{separator}soldr_refresh={nonce}")
}

pub(crate) fn verify_catalogue_asset_sha256(
    entry: &ManifestEntry,
    actual: &str,
) -> Result<(), SoldrError> {
    if actual == entry.sha256 {
        return Ok(());
    }
    Err(SoldrError::Other(format!(
        "catalogue asset {} failed SHA-256 verification",
        entry.asset
    )))
}

pub(crate) fn expected_size_matches(expected_bytes: u64, actual_bytes: u64) -> bool {
    expected_bytes == 0 || expected_bytes == actual_bytes
}
