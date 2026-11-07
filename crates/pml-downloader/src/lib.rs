//! Async, streaming, hash-verified downloader.
//!
//! Design notes:
//! * `reqwest::Response::bytes_stream()` yields chunks; each chunk is written to a
//!   `.part` file immediately, so a 500 MiB jar never lives in RAM.
//! * `tokio::sync::Semaphore` bounds concurrent sockets without spawning one task
//!   per file in an unbounded way.
//! * Progress is reported over `tokio::sync::mpsc`, never through a mutex-protected
//!   float read on the UI thread.
//! * `Arc` is used only for the immutable client/semaphore shared by tasks.
//! * `spawn_blocking` is used for CPU/blocking zip extraction, never for network I/O.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

pub const USER_AGENT: &str = concat!(
    "OdiumLauncher/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/odium-launcher/odium-launcher)"
);

const MAX_ATTEMPTS: usize = 3;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);
const COPY_BUFFER: usize = 64 * 1024;
/// Sidecar cache with size+mtime fingerprints, so an unchanged file is not
/// re-read and re-hashed on every launch.
pub const HASH_CACHE_FILE: &str = ".pml_hash_cache.json";
/// A `.part` file this old without a matching download task is considered
/// abandoned and removed on the next batch.
const ORPHAN_PART_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Sha1,
    Sha256,
    Sha512,
}

impl HashAlgo {
    fn new_hasher(self) -> HashState {
        match self {
            Self::Sha1 => HashState::Sha1(sha1::Sha1::new()),
            Self::Sha256 => HashState::Sha256(sha2::Sha256::new()),
            Self::Sha512 => HashState::Sha512(sha2::Sha512::new()),
        }
    }

    pub fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }
}

enum HashState {
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
}

impl HashState {
    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha1(hasher) => hasher.update(bytes),
            Self::Sha256(hasher) => hasher.update(bytes),
            Self::Sha512(hasher) => hasher.update(bytes),
        }
    }

    fn finalize_hex(self) -> String {
        match self {
            Self::Sha1(hasher) => hex::encode(hasher.finalize()),
            Self::Sha256(hasher) => hex::encode(hasher.finalize()),
            Self::Sha512(hasher) => hex::encode(hasher.finalize()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadTask {
    pub url: String,
    pub dest: PathBuf,
    /// Expected lowercase/uppercase hex digest. `None` disables verification.
    pub sha: Option<String>,
    pub algo: HashAlgo,
    pub size: Option<u64>,
    pub label: String,
}

impl DownloadTask {
    pub fn new(
        url: impl Into<String>,
        dest: impl Into<PathBuf>,
        sha: Option<String>,
        algo: HashAlgo,
        size: Option<u64>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            dest: dest.into(),
            sha,
            algo,
            size,
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum DownloadEvent {
    Started {
        index: usize,
        total: usize,
        label: String,
        size: Option<u64>,
    },
    Progress {
        index: usize,
        label: String,
        downloaded: u64,
        total: Option<u64>,
        overall_done: u64,
        overall_total: u64,
    },
    FileDone {
        index: usize,
        label: String,
        skipped: bool,
        bytes: u64,
    },
    FileFailed {
        index: usize,
        label: String,
        error: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DownloadSummary {
    pub total_files: usize,
    pub downloaded: usize,
    pub skipped: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOutcome {
    pub skipped: bool,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HashCacheEntry {
    size: u64,
    mtime_ns: u64,
    algo: String,
    hash: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HashCacheData {
    #[serde(default)]
    entries: BTreeMap<String, HashCacheEntry>,
}

/// Shared, persisted size+mtime -> hash map for one download batch.
struct HashCache {
    root: PathBuf,
    path: PathBuf,
    entries: Mutex<BTreeMap<String, HashCacheEntry>>,
}

impl HashCache {
    async fn load(root: PathBuf) -> Self {
        let path = root.join(HASH_CACHE_FILE);
        let entries = match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<HashCacheData>(&bytes) {
                Ok(data) => data.entries,
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "hash cache is corrupt");
                    BTreeMap::new()
                }
            },
            Err(_) => BTreeMap::new(),
        };
        Self {
            root,
            path,
            entries: Mutex::new(entries),
        }
    }

    fn key(&self, dest: &Path) -> Option<String> {
        let relative = dest.strip_prefix(&self.root).ok()?;
        Some(relative.to_string_lossy().replace('\\', "/"))
    }

    fn matches(&self, task: &DownloadTask, metadata: &std::fs::Metadata) -> bool {
        let Some(expected) = task.sha.as_deref() else {
            return false;
        };
        if expected.is_empty() {
            return false;
        }
        let Some(key) = self.key(&task.dest) else {
            return false;
        };
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.get(&key).is_some_and(|entry| {
            entry.size == metadata.len()
                && entry.mtime_ns == mtime_ns(metadata)
                && entry.algo == algo_name(task.algo)
                && entry.hash.eq_ignore_ascii_case(expected)
        })
    }

    fn record(&self, task: &DownloadTask, metadata: &std::fs::Metadata) {
        let Some(expected) = task.sha.as_deref() else {
            return;
        };
        if expected.is_empty() {
            return;
        }
        let Some(key) = self.key(&task.dest) else {
            return;
        };
        let entry = HashCacheEntry {
            size: metadata.len(),
            mtime_ns: mtime_ns(metadata),
            algo: algo_name(task.algo).to_owned(),
            hash: expected.to_ascii_lowercase(),
        };
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, entry);
    }

    async fn persist(&self) {
        let data = HashCacheData {
            entries: self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        };
        let Ok(json) = serde_json::to_vec(&data) else {
            return;
        };
        let temporary = self.path.with_extension("json.tmp");
        if let Err(error) = tokio::fs::write(&temporary, &json).await {
            tracing::debug!(%error, path = %temporary.display(), "failed to write hash cache");
            return;
        }
        if let Err(error) = tokio::fs::rename(&temporary, &self.path).await {
            tracing::debug!(%error, path = %self.path.display(), "failed to persist hash cache");
        }
    }
}

fn algo_name(algo: HashAlgo) -> &'static str {
    match algo {
        HashAlgo::Sha1 => "sha1",
        HashAlgo::Sha256 => "sha256",
        HashAlgo::Sha512 => "sha512",
    }
}

fn mtime_ns(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// Deepest directory shared by every destination in the batch. Paths in the
/// hash cache are stored relative to it, so a portable launcher folder can be
/// moved without invalidating the cache.
fn common_root(tasks: &[DownloadTask]) -> Option<PathBuf> {
    let mut root: Vec<Component> = tasks.first()?.dest.parent()?.components().collect();
    for task in &tasks[1..] {
        let components: Vec<Component> = task.dest.parent()?.components().collect();
        let shared = root
            .iter()
            .zip(components.iter())
            .take_while(|(left, right)| left == right)
            .count();
        root.truncate(shared);
        if root.is_empty() {
            return None;
        }
    }
    Some(root.into_iter().collect())
}

/// Removes `.part` files left by downloads that are no longer in any task
/// list. Fresh parts are kept because another batch may be resuming them.
async fn cleanup_orphaned_parts(root: PathBuf, active: HashSet<PathBuf>) {
    let removed = tokio::task::spawn_blocking(move || {
        let mut removed = 0usize;
        for entry in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .max_depth(12)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "part") {
                continue;
            }
            let destination = path.with_extension("");
            if active.contains(path) || active.contains(&destination) {
                continue;
            }
            let old_enough = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age >= ORPHAN_PART_AGE);
            if old_enough && std::fs::remove_file(path).is_ok() {
                removed += 1;
            }
        }
        removed
    })
    .await;
    if let Ok(removed) = removed {
        if removed > 0 {
            tracing::info!(removed, "removed orphaned partial downloads");
        }
    }
}

fn retry_delay(attempt: usize) -> Duration {
    let base = 500u64 << attempt.min(6);
    let spread = base / 5;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0) as u64;
    let jitter = if spread == 0 {
        0
    } else {
        nanos % (spread * 2 + 1)
    };
    Duration::from_millis(base - spread + jitter)
}

/// Per-batch values every download task needs for progress reporting. Kept in
/// one `Clone`-able struct so the worker signature stays readable.
#[derive(Clone)]
struct DownloadContext {
    total: usize,
    events: Option<mpsc::UnboundedSender<DownloadEvent>>,
    overall_done: Arc<AtomicU64>,
    overall_total: u64,
    cache: Option<Arc<HashCache>>,
    cancel: Option<Arc<AtomicBool>>,
}

impl DownloadContext {
    /// Cooperative cancellation is checked by every worker between chunks, so
    /// a cancelled batch stops within one network chunk instead of waiting for
    /// the whole file set to finish.
    fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
    }
}

#[derive(Debug, Clone)]
pub struct Downloader {
    inner: Arc<DownloaderInner>,
}

#[derive(Debug)]
struct DownloaderInner {
    client: reqwest::Client,
    concurrency: usize,
    cancel: Option<Arc<AtomicBool>>,
}

impl Downloader {
    pub fn new(concurrency: usize) -> Result<Self, DownloadError> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(30 * 60))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(DownloadError::Client)?;
        Ok(Self::new_with_client(client, concurrency))
    }

    /// Useful for tests and for callers that already configured a client
    /// (proxy, extra roots, tracing middleware, etc.).
    pub fn new_with_client(client: reqwest::Client, concurrency: usize) -> Self {
        Self {
            inner: Arc::new(DownloaderInner {
                client,
                concurrency: concurrency.clamp(1, 32),
                cancel: None,
            }),
        }
    }

    /// Returns a clone that cooperatively cancels every batch started through
    /// it. Domain crates keep their `Downloader` parameter unchanged: the UI
    /// passes the cancellable clone down the pipeline once.
    pub fn with_cancel(&self, cancel: Arc<AtomicBool>) -> Self {
        Self {
            inner: Arc::new(DownloaderInner {
                client: self.inner.client.clone(),
                concurrency: self.inner.concurrency,
                cancel: Some(cancel),
            }),
        }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.inner.client
    }

    /// Downloads a batch with bounded parallelism. All tasks are attempted even
    /// if one fails; the first error is returned after the batch finishes so the
    /// caller sees a useful failure instead of a partially silent install.
    pub async fn download_many(
        &self,
        tasks: Vec<DownloadTask>,
        events: Option<mpsc::UnboundedSender<DownloadEvent>>,
    ) -> Result<DownloadSummary, DownloadError> {
        if self
            .inner
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
        {
            return Err(DownloadError::Cancelled);
        }

        // Minecraft's asset index routinely maps several logical names (for
        // example two sounds) to the same object hash. Without deduplication
        // two workers write the same `.part` file and race on the final
        // rename: one succeeds, the other removes the freshly installed file
        // and fails with `os error 2`. Collapse identical destinations before
        // spawning workers. A conflicting digest for one destination is a
        // manifest bug, not something to guess about.
        let mut unique: Vec<DownloadTask> = Vec::with_capacity(tasks.len());
        let mut seen: std::collections::HashMap<PathBuf, (String, Option<u64>)> =
            std::collections::HashMap::with_capacity(tasks.len());
        let mut duplicates = 0usize;
        for task in tasks {
            let key = task.dest.clone();
            let sha = task.sha.clone().unwrap_or_default();
            if let Some((known_sha, known_size)) = seen.get(&key) {
                if *known_sha != sha || *known_size != task.size {
                    tracing::warn!(
                        path = %task.dest.display(),
                        label = %task.label,
                        "ignoring a duplicate download task with a conflicting digest"
                    );
                }
                duplicates += 1;
                continue;
            }
            seen.insert(key, (sha, task.size));
            unique.push(task);
        }
        if duplicates > 0 {
            tracing::debug!(duplicates, "collapsed duplicate download destinations");
        }
        let tasks = unique;
        let total_files = tasks.len();
        let overall_total = tasks.iter().filter_map(|task| task.size).sum::<u64>();
        let overall_done = Arc::new(AtomicU64::new(0));
        let semaphore = Arc::new(Semaphore::new(self.inner.concurrency));
        let mut set: JoinSet<Result<(usize, FileOutcome), DownloadError>> = JoinSet::new();

        let cache = match common_root(&tasks) {
            Some(root) => Some(Arc::new(HashCache::load(root).await)),
            None => None,
        };
        if let Some(cache) = cache.as_ref() {
            let active = tasks.iter().map(|task| task.dest.clone()).collect();
            cleanup_orphaned_parts(cache.root.clone(), active).await;
        }

        let context = DownloadContext {
            total: total_files,
            events,
            overall_done: Arc::clone(&overall_done),
            overall_total,
            cache,
            cancel: self.inner.cancel.clone(),
        };
        let cache = context.cache.clone();
        for (index, task) in tasks.into_iter().enumerate() {
            let downloader = self.clone();
            let semaphore = Arc::clone(&semaphore);
            let context = context.clone();
            set.spawn(async move {
                // The permit is acquired inside the task, so spawning thousands of
                // lightweight futures does not open thousands of sockets.
                let permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| DownloadError::Semaphore)?;
                let outcome = downloader.download_one(&task, index, &context).await;
                drop(permit);
                outcome.map(|outcome| (index, outcome))
            });
        }

        let mut summary = DownloadSummary {
            total_files,
            ..DownloadSummary::default()
        };
        let mut first_error: Option<DownloadError> = None;
        let mut cancelled = false;

        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok((_index, outcome))) => {
                    summary.bytes = summary.bytes.saturating_add(outcome.bytes);
                    if outcome.skipped {
                        summary.skipped += 1;
                    } else {
                        summary.downloaded += 1;
                    }
                }
                Ok(Err(error)) => {
                    if matches!(error, DownloadError::Cancelled) {
                        cancelled = true;
                    } else {
                        tracing::warn!(%error, "download task failed");
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
                Err(error) => {
                    let error = DownloadError::Join(error.to_string());
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if let Some(cache) = cache.as_ref() {
            cache.persist().await;
        }

        if cancelled {
            return Err(DownloadError::Cancelled);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(summary),
        }
    }

    async fn download_one(
        &self,
        task: &DownloadTask,
        index: usize,
        context: &DownloadContext,
    ) -> Result<FileOutcome, DownloadError> {
        let events = context.events.as_ref();
        if context.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        if let Some(expected) = task.sha.as_deref() {
            if !expected.is_empty() {
                let metadata = tokio::fs::metadata(&task.dest).await.ok();
                let cached = match (metadata.as_ref(), context.cache.as_ref()) {
                    (Some(metadata), Some(cache)) => cache.matches(task, metadata),
                    _ => false,
                };
                if cached || verify_hash(&task.dest, expected, task.algo).await? {
                    if !cached {
                        if let (Some(cache), Some(metadata)) =
                            (context.cache.as_ref(), metadata.as_ref())
                        {
                            cache.record(task, metadata);
                        }
                    }
                    let bytes = match task.size {
                        Some(size) => size,
                        None => metadata
                            .as_ref()
                            .map(|metadata| metadata.len())
                            .unwrap_or(0),
                    };
                    context.overall_done.fetch_add(bytes, Ordering::Relaxed);
                    send(
                        events,
                        DownloadEvent::FileDone {
                            index,
                            label: task.label.clone(),
                            skipped: true,
                            bytes,
                        },
                    );
                    return Ok(FileOutcome {
                        skipped: true,
                        bytes,
                    });
                }
            }
        }

        send(
            events,
            DownloadEvent::Started {
                index,
                total: context.total,
                label: task.label.clone(),
                size: task.size,
            },
        );

        let mut last_error: Option<DownloadError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self
                .try_download(
                    task,
                    index,
                    events,
                    &context.overall_done,
                    context.overall_total,
                    context.cancel.as_deref(),
                )
                .await
            {
                Ok(outcome) => {
                    if let (Some(cache), Ok(metadata)) = (
                        context.cache.as_ref(),
                        tokio::fs::metadata(&task.dest).await,
                    ) {
                        cache.record(task, &metadata);
                    }
                    context
                        .overall_done
                        .fetch_add(outcome.bytes, Ordering::Relaxed);
                    send(
                        events,
                        DownloadEvent::FileDone {
                            index,
                            label: task.label.clone(),
                            skipped: false,
                            bytes: outcome.bytes,
                        },
                    );
                    return Ok(outcome);
                }
                Err(DownloadError::Cancelled) => return Err(DownloadError::Cancelled),
                Err(error) if error.is_retryable() && attempt + 1 < MAX_ATTEMPTS => {
                    let delay = retry_delay(attempt);
                    tracing::warn!(
                        url = %task.url,
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis(),
                        %error,
                        "retrying download"
                    );
                    tokio::time::sleep(delay).await;
                    last_error = Some(error);
                }
                Err(error) => {
                    let _ = tokio::fs::remove_file(part_path(&task.dest)).await;
                    send(
                        events,
                        DownloadEvent::FileFailed {
                            index,
                            label: task.label.clone(),
                            error: error.to_string(),
                        },
                    );
                    return Err(error);
                }
            }
        }

        let error = last_error.unwrap_or_else(|| DownloadError::Io {
            path: task.dest.clone(),
            source: std::io::Error::other("download failed without a specific error"),
        });
        let _ = tokio::fs::remove_file(part_path(&task.dest)).await;
        send(
            events,
            DownloadEvent::FileFailed {
                index,
                label: task.label.clone(),
                error: error.to_string(),
            },
        );
        Err(error)
    }

    async fn try_download(
        &self,
        task: &DownloadTask,
        index: usize,
        events: Option<&mpsc::UnboundedSender<DownloadEvent>>,
        overall_done: &AtomicU64,
        overall_total: u64,
        cancel: Option<&AtomicBool>,
    ) -> Result<FileOutcome, DownloadError> {
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
            return Err(DownloadError::Cancelled);
        }
        let parsed = reqwest::Url::parse(&task.url)
            .map_err(|_| DownloadError::InvalidUrl(task.url.clone()))?;
        match parsed.scheme() {
            "http" | "https" => {}
            "file" => {
                let source = parsed
                    .to_file_path()
                    .map_err(|_| DownloadError::InvalidUrl(task.url.clone()))?;
                return self
                    .copy_local(
                        task,
                        index,
                        events,
                        overall_done,
                        overall_total,
                        &source,
                        cancel,
                    )
                    .await;
            }
            other => return Err(DownloadError::UnsupportedScheme(other.to_owned())),
        }

        if let Some(parent) = task.dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| io_error(parent, source))?;
        }

        let part = part_path(&task.dest);
        let mut resume_from = tokio::fs::metadata(&part)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if task
            .size
            .is_some_and(|expected| expected > 0 && resume_from > expected)
        {
            // The leftover `.part` is larger than the file described by the
            // manifest; it cannot be resumed safely.
            let _ = tokio::fs::remove_file(&part).await;
            resume_from = 0;
        }
        let mut request = self.inner.client.get(&task.url);
        if resume_from > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
        }
        let mut response = request
            .send()
            .await
            .map_err(|source| DownloadError::Network {
                url: task.url.clone(),
                source,
            })?;
        let mut status = response.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // A stale part at or past EOF: start the file over.
            let _ = tokio::fs::remove_file(&part).await;
            resume_from = 0;
            response = self
                .inner
                .client
                .get(&task.url)
                .send()
                .await
                .map_err(|source| DownloadError::Network {
                    url: task.url.clone(),
                    source,
                })?;
            status = response.status();
        }
        if !status.is_success() {
            return Err(DownloadError::HttpStatus {
                status: status.as_u16(),
                url: task.url.clone(),
            });
        }
        if resume_from > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            // The server ignored `Range` and sent the whole body from byte 0.
            resume_from = 0;
        }

        let remaining = response.content_length();
        let total = match (task.size, remaining) {
            (Some(expected), _) => Some(expected),
            (None, Some(remaining)) => Some(resume_from.saturating_add(remaining)),
            (None, None) => None,
        };
        if let Some(expected) = task.size.filter(|value| *value > 0) {
            if let Some(remaining) = remaining {
                let actual_total = resume_from.saturating_add(remaining);
                if expected != actual_total {
                    tracing::warn!(
                        url = %task.url,
                        expected,
                        actual = actual_total,
                        "server Content-Length differs from manifest size"
                    );
                }
            }
        }

        let mut hasher = task.algo.new_hasher();
        let mut downloaded = resume_from;
        if resume_from > 0 {
            let mut existing = tokio::fs::File::open(&part)
                .await
                .map_err(|source| io_error(&part, source))?;
            let mut buffer = vec![0u8; COPY_BUFFER];
            let mut fed: u64 = 0;
            loop {
                let read = existing
                    .read(&mut buffer)
                    .await
                    .map_err(|source| io_error(&part, source))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                fed = fed.saturating_add(read as u64);
            }
            if fed != resume_from {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(DownloadError::ResumeMismatch { path: part.clone() });
            }
        }

        let file = if resume_from > 0 {
            tokio::fs::OpenOptions::new().append(true).open(&part).await
        } else {
            tokio::fs::File::create(&part).await
        }
        .map_err(|source| io_error(&part, source))?;
        let mut writer = tokio::io::BufWriter::with_capacity(COPY_BUFFER, file);
        let mut stream = response.bytes_stream();
        let mut last_emit = Instant::now() - PROGRESS_INTERVAL;

        while let Some(chunk) = stream.next().await {
            if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
                // Flush what has arrived and keep the `.part`: every byte in
                // it was hashed and is safe to resume with a Range request.
                writer
                    .flush()
                    .await
                    .map_err(|source| io_error(&part, source))?;
                let file = writer.into_inner();
                let _ = file.sync_all().await;
                return Err(DownloadError::Cancelled);
            }
            let chunk = chunk.map_err(|source| DownloadError::Network {
                url: task.url.clone(),
                source,
            })?;
            hasher.update(&chunk);
            writer
                .write_all(&chunk)
                .await
                .map_err(|source| io_error(&part, source))?;
            downloaded = downloaded.saturating_add(chunk.len() as u64);

            if last_emit.elapsed() >= PROGRESS_INTERVAL
                || total.is_some_and(|total| downloaded >= total)
            {
                last_emit = Instant::now();
                send(
                    events,
                    DownloadEvent::Progress {
                        index,
                        label: task.label.clone(),
                        downloaded,
                        total,
                        overall_done: overall_done
                            .load(Ordering::Relaxed)
                            .saturating_add(downloaded),
                        overall_total,
                    },
                );
            }
        }

        writer
            .flush()
            .await
            .map_err(|source| io_error(&part, source))?;
        let file = writer.into_inner();
        file.sync_all()
            .await
            .map_err(|source| io_error(&part, source))?;
        drop(file);

        if let Some(expected) = task.sha.as_deref() {
            let actual = hasher.finalize_hex();
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(DownloadError::HashMismatch {
                    path: task.dest.clone(),
                    expected: expected.to_owned(),
                    actual,
                });
            }
        }

        if let Some(expected) = task.size {
            if expected > 0 && downloaded != expected {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(DownloadError::SizeMismatch {
                    path: task.dest.clone(),
                    expected,
                    actual: downloaded,
                });
            }
        }

        if let Err(mut source) = tokio::fs::rename(&part, &task.dest).await {
            // A concurrent worker (an identical asset object referenced under
            // two names, or two batches sharing one destination) may have
            // completed the same file between our flush and this rename. In
            // that case the verified destination is just as good as ours;
            // never delete it, because the other worker already reported
            // success and Minecraft will later read exactly this path.
            if destination_matches(task).await {
                tracing::debug!(
                    path = %task.dest.display(),
                    "destination was completed by a concurrent download"
                );
                return Ok(FileOutcome {
                    skipped: false,
                    bytes: downloaded,
                });
            }
            // Windows normally replaces on rename; antivirus scanners can hold
            // the file for a few hundred milliseconds. Retry without ever
            // removing the destination, which may be a valid copy.
            for attempt in 0..4 {
                tokio::time::sleep(Duration::from_millis(50 * (attempt + 1))).await;
                match tokio::fs::rename(&part, &task.dest).await {
                    Ok(()) => {
                        return Ok(FileOutcome {
                            skipped: false,
                            bytes: downloaded,
                        })
                    }
                    Err(error) => source = error,
                }
                if destination_matches(task).await {
                    return Ok(FileOutcome {
                        skipped: false,
                        bytes: downloaded,
                    });
                }
            }
            return Err(io_error(&task.dest, source));
        }

        Ok(FileOutcome {
            skipped: false,
            bytes: downloaded,
        })
    }

    /// `file://` sources are used by local packs and integration tests. The
    /// copy still goes through `.part` + hash verification, so a local file is
    /// held to exactly the same integrity contract as an HTTP download.
    #[allow(clippy::too_many_arguments)]
    async fn copy_local(
        &self,
        task: &DownloadTask,
        index: usize,
        events: Option<&mpsc::UnboundedSender<DownloadEvent>>,
        overall_done: &AtomicU64,
        overall_total: u64,
        source: &Path,
        cancel: Option<&AtomicBool>,
    ) -> Result<FileOutcome, DownloadError> {
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
            return Err(DownloadError::Cancelled);
        }
        let metadata = tokio::fs::metadata(source)
            .await
            .map_err(|source_error| io_error(source, source_error))?;
        if !metadata.is_file() {
            return Err(DownloadError::Io {
                path: source.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "file:// source is not a regular file",
                ),
            });
        }
        if let Some(parent) = task.dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| io_error(parent, source))?;
        }
        let part = part_path(&task.dest);
        tokio::fs::copy(source, &part)
            .await
            .map_err(|source| io_error(&part, source))?;

        let bytes = tokio::fs::metadata(&part)
            .await
            .map_err(|source| io_error(&part, source))?
            .len();
        if let Some(expected) = task.size {
            if expected > 0 && bytes != expected {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(DownloadError::SizeMismatch {
                    path: task.dest.clone(),
                    expected,
                    actual: bytes,
                });
            }
        }
        if let Some(expected) = task.sha.as_deref() {
            let actual = hash_file(&part, task.algo).await?;
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(DownloadError::HashMismatch {
                    path: task.dest.clone(),
                    expected: expected.to_owned(),
                    actual,
                });
            }
        }
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
            return Err(DownloadError::Cancelled);
        }
        if let Err(source) = tokio::fs::rename(&part, &task.dest).await {
            tracing::debug!(path = %task.dest.display(), %source, "rename failed, retrying after remove");
            let _ = tokio::fs::remove_file(&task.dest).await;
            tokio::fs::rename(&part, &task.dest)
                .await
                .map_err(|source| io_error(&task.dest, source))?;
        }
        send(
            events,
            DownloadEvent::Progress {
                index,
                label: task.label.clone(),
                downloaded: bytes,
                total: Some(bytes),
                overall_done: overall_done.load(Ordering::Relaxed).saturating_add(bytes),
                overall_total,
            },
        );
        Ok(FileOutcome {
            skipped: false,
            bytes,
        })
    }
}

fn send(events: Option<&mpsc::UnboundedSender<DownloadEvent>>, event: DownloadEvent) {
    if let Some(sender) = events {
        // A closed receiver (launcher UI closed) is not an error for the download.
        let _ = sender.send(event);
    }
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// True when `task.dest` is already the complete, verified file this task
/// would have produced. Used only to resolve cross-worker rename races.
async fn destination_matches(task: &DownloadTask) -> bool {
    let Ok(metadata) = tokio::fs::metadata(&task.dest).await else {
        return false;
    };
    if let Some(expected) = task.size.filter(|size| *size > 0) {
        if metadata.len() != expected {
            return false;
        }
    }
    match task.sha.as_deref().filter(|sha| !sha.is_empty()) {
        Some(expected) => verify_hash(&task.dest, expected, task.algo)
            .await
            .unwrap_or(false),
        None => metadata.is_file(),
    }
}

pub async fn verify_hash(
    path: &Path,
    expected: &str,
    algo: HashAlgo,
) -> Result<bool, DownloadError> {
    if expected.is_empty() {
        return Ok(false);
    }
    let actual = match hash_file(path, algo).await {
        Ok(actual) => actual,
        Err(DownloadError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    Ok(actual.eq_ignore_ascii_case(expected))
}

pub async fn hash_file(path: &Path, algo: HashAlgo) -> Result<String, DownloadError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|source| io_error(path, source))?;
    let mut hasher = algo.new_hasher();
    let mut buffer = vec![0u8; COPY_BUFFER];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|source| io_error(path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize_hex())
}

pub fn hash_bytes(algo: HashAlgo, bytes: &[u8]) -> String {
    let mut hasher = algo.new_hasher();
    hasher.update(bytes);
    hasher.finalize_hex()
}

/// Extracts a zip without ever allowing a path outside `dest`.
///
/// `ZipFile::enclosed_name()` already rejects `..` and absolute paths; we add a
/// canonical prefix check as defence in depth (Windows junctions, odd separators)
/// and reject Unix symlink entries. This function is blocking and must be called
/// from `tokio::task::spawn_blocking` when used in async code.
pub fn safe_extract_zip(zip_path: &Path, dest: &Path) -> Result<Vec<PathBuf>, DownloadError> {
    safe_extract_zip_with_cancel(zip_path, dest, None)
}

/// Same as [`safe_extract_zip`], but checks `cancel` before every archive
/// entry so a large runtime extraction stops promptly instead of keeping the
/// UI on "Отмена…" until the whole zip is on disk.
pub fn safe_extract_zip_with_cancel(
    zip_path: &Path,
    dest: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<PathBuf>, DownloadError> {
    std::fs::create_dir_all(dest).map_err(|source| io_error(dest, source))?;
    let dest_canonical = dest
        .canonicalize()
        .map_err(|source| io_error(dest, source))?;
    let file = std::fs::File::open(zip_path).map_err(|source| io_error(zip_path, source))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|source| DownloadError::Zip {
        path: zip_path.to_path_buf(),
        source,
    })?;

    let mut extracted = Vec::new();
    for index in 0..archive.len() {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(DownloadError::Cancelled);
        }
        let mut entry = archive
            .by_index(index)
            .map_err(|source| DownloadError::Zip {
                path: zip_path.to_path_buf(),
                source,
            })?;

        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                return Err(DownloadError::ZipSymlink(entry.name().to_owned()));
            }
        }

        let relative = entry
            .enclosed_name()
            .ok_or_else(|| DownloadError::UnsafeZipPath(entry.name().to_owned()))?;
        if !is_safe_relative(&relative) {
            return Err(DownloadError::UnsafeZipPath(entry.name().to_owned()));
        }

        let out_path = dest.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|source| io_error(&out_path, source))?;
            continue;
        }

        let parent = out_path
            .parent()
            .ok_or_else(|| DownloadError::UnsafeZipPath(entry.name().to_owned()))?;
        std::fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        let parent_canonical = parent
            .canonicalize()
            .map_err(|source| io_error(parent, source))?;
        if !parent_canonical.starts_with(&dest_canonical) {
            return Err(DownloadError::UnsafeZipPath(entry.name().to_owned()));
        }

        let mut output =
            std::fs::File::create(&out_path).map_err(|source| io_error(&out_path, source))?;
        std::io::copy(&mut entry, &mut output).map_err(|source| io_error(&out_path, source))?;
        extracted.push(relative);
    }
    Ok(extracted)
}

/// Rejects absolute paths, drive prefixes, `..` and empty components.
pub fn is_safe_relative(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Finds the first file with `file_name` below `root` without following links.
pub fn find_file(root: &Path, file_name: &str) -> Option<PathBuf> {
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(file_name))
        {
            return Some(entry.into_path());
        }
    }
    None
}

fn io_error(path: &Path, source: std::io::Error) -> DownloadError {
    DownloadError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("загрузка отменена")]
    Cancelled,

    #[error("некорректный URL: {0}")]
    InvalidUrl(String),
    #[error("неподдерживаемая схема URL: {0}")]
    UnsupportedScheme(String),
    #[error("HTTP {status} для {url}")]
    HttpStatus { status: u16, url: String },
    #[error("сетевая ошибка для {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("ошибка файла {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("несовпадение SHA для {path}: ожидался {expected}, получен {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("несовпадение размера для {path}: ожидалось {expected}, получено {actual}")]
    SizeMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    #[error("частично скачанный файл {path} изменился во время возобновления")]
    ResumeMismatch { path: PathBuf },
    #[error("нет свободного слота загрузки")]
    Semaphore,
    #[error("задача загрузки завершилась паникой: {0}")]
    Join(String),
    #[error("небезопасный путь в zip: {0}")]
    UnsafeZipPath(String),
    #[error("в zip обнаружена символическая ссылка: {0}")]
    ZipSymlink(String),
    #[error("ошибка zip {path}: {source}")]
    Zip {
        path: PathBuf,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("не удалось создать HTTP-клиент: {0}")]
    Client(#[source] reqwest::Error),
}

impl DownloadError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Network { source, .. } => {
                source.is_timeout() || source.is_connect() || source.is_request()
            }
            Self::HttpStatus { status, .. } => {
                *status == 408 || *status == 429 || (500..=599).contains(status)
            }
            Self::HashMismatch { .. } | Self::SizeMismatch { .. } => true,
            Self::ResumeMismatch { .. } => true,
            Self::Cancelled => false,
            Self::Io { source, .. } => matches!(
                source.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            ),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn verify_sha256_and_sha1() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("data.bin");
        let bytes = b"portable launcher data";
        tokio::fs::write(&path, bytes).await?;

        let sha256 = hash_bytes(HashAlgo::Sha256, bytes);
        let sha1 = hash_bytes(HashAlgo::Sha1, bytes);
        assert_eq!(sha256.len(), 64);
        assert_eq!(sha1.len(), 40);
        assert!(verify_hash(&path, &sha256, HashAlgo::Sha256).await?);
        assert!(verify_hash(&path, &sha1.to_uppercase(), HashAlgo::Sha1).await?);
        assert!(!verify_hash(&path, &"0".repeat(64), HashAlgo::Sha256).await?);
        Ok(())
    }

    #[tokio::test]
    async fn missing_file_is_not_a_hash_match() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        assert!(
            !verify_hash(
                &dir.path().join("missing"),
                &"a".repeat(64),
                HashAlgo::Sha256
            )
            .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn streams_over_http_and_skips_second_run() -> Result<(), Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let body = b"hello portable minecraft launcher ".repeat(8192);
        let body_for_server = body.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_for_server.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body_for_server).await;
            }
        });

        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("payload.bin");
        let expected = hash_bytes(HashAlgo::Sha256, &body);
        let client = reqwest::Client::builder().no_proxy().build()?;
        let downloader = Downloader::new_with_client(client, 2);
        let task = DownloadTask::new(
            format!("http://{address}/payload.bin"),
            dest.clone(),
            Some(expected),
            HashAlgo::Sha256,
            Some(body.len() as u64),
            "payload",
        );

        let first = downloader.download_many(vec![task.clone()], None).await?;
        assert_eq!(first.downloaded, 1);
        assert_eq!(tokio::fs::read(&dest).await?, body);
        assert!(!part_path(&dest).exists());

        let second = downloader.download_many(vec![task], None).await?;
        assert_eq!(second.skipped, 1);
        assert_eq!(second.downloaded, 0);
        server.await?;
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_destinations_do_not_race_on_the_part_file(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("orb.ogg");
        let body = b"two asset names, one object".repeat(4096);
        tokio::fs::write(&source, &body).await?;
        let dest = dir.path().join("assets").join("objects").join("ab").join("hash");
        let url = reqwest::Url::from_file_path(&source)
            .expect("file url")
            .to_string();
        let task = DownloadTask::new(
            url,
            dest.clone(),
            Some(hash_bytes(HashAlgo::Sha256, &body)),
            HashAlgo::Sha256,
            Some(body.len() as u64),
            "shared asset",
        );

        // Minecraft's asset index lists several names for one hash. Before
        // deduplication the second worker renamed the same `.part` and the
        // loser deleted the winner's file, producing `os error 2`.
        let summary = Downloader::new(4)?
            .download_many(vec![task.clone(), task], None)
            .await?;
        assert_eq!(summary.total_files, 1);
        assert_eq!(summary.downloaded, 1);
        assert_eq!(tokio::fs::read(&dest).await?, body);
        assert!(!part_path(&dest).exists());
        Ok(())
    }

    #[tokio::test]
    async fn file_url_is_copied_and_verified() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source.bin");
        let body = b"local pack payload";
        tokio::fs::write(&source, body).await?;
        let dest = dir.path().join("instance").join("payload.bin");
        let url = reqwest::Url::from_file_path(&source)
            .expect("file url")
            .to_string();
        let downloader = Downloader::new(1)?;
        let task = DownloadTask::new(
            url,
            dest.clone(),
            Some(hash_bytes(HashAlgo::Sha256, body)),
            HashAlgo::Sha256,
            Some(body.len() as u64),
            "local",
        );
        let summary = downloader.download_many(vec![task], None).await?;
        assert_eq!(summary.downloaded, 1);
        assert_eq!(tokio::fs::read(&dest).await?, body);
        assert!(!part_path(&dest).exists());
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_before_start_skips_the_network() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("never.bin");
        let cancel = Arc::new(AtomicBool::new(true));
        let downloader = Downloader::new(1)?.with_cancel(cancel);
        let task = DownloadTask::new(
            "http://127.0.0.1:9/never.bin",
            dest.clone(),
            None,
            HashAlgo::Sha256,
            None,
            "never",
        );

        let error = downloader
            .download_many(vec![task], None)
            .await
            .expect_err("batch must be cancelled");
        assert!(matches!(error, DownloadError::Cancelled));
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_mid_download_keeps_a_resumable_part(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;
                let chunk = vec![7u8; COPY_BUFFER];
                let total = chunk.len() * 64;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                );
                if socket.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                for _ in 0..64 {
                    if socket.write_all(&chunk).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        });

        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("slow.bin");
        let cancel = Arc::new(AtomicBool::new(false));
        let downloader = Downloader::new(1)?.with_cancel(Arc::clone(&cancel));
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let task = DownloadTask::new(
            format!("http://{address}/slow.bin"),
            dest.clone(),
            None,
            HashAlgo::Sha256,
            None,
            "slow",
        );
        let run =
            tokio::spawn(
                async move { downloader.download_many(vec![task], Some(events_tx)).await },
            );

        let first = tokio::time::timeout(Duration::from_secs(5), events_rx.recv())
            .await?
            .expect("progress event");
        assert!(matches!(first, DownloadEvent::Started { .. }));
        cancel.store(true, Ordering::SeqCst);

        let result = tokio::time::timeout(Duration::from_secs(5), run).await??;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert!(
            part_path(&dest).exists(),
            "partial data must be kept for resume"
        );
        assert!(!dest.exists());
        let _ = server.await;
        Ok(())
    }

    #[test]
    fn zip_slip_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let zip_path = dir.path().join("evil.zip");
        {
            let file = std::fs::File::create(&zip_path)?;
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("../evil.txt", options)?;
            std::io::Write::write_all(&mut writer, b"pwned")?;
            writer.finish()?;
        }

        let dest = dir.path().join("dest");
        let result = safe_extract_zip(&zip_path, &dest);
        assert!(matches!(result, Err(DownloadError::UnsafeZipPath(_))));
        assert!(!dir.path().join("evil.txt").exists());
        Ok(())
    }

    #[test]
    fn safe_relative_paths() {
        assert!(is_safe_relative(Path::new("mods/example.jar")));
        assert!(!is_safe_relative(Path::new("../example.jar")));
        assert!(!is_safe_relative(Path::new("C:\\Windows\\evil.dll")));
        assert!(!is_safe_relative(Path::new("")));
    }

    #[test]
    fn retry_delay_stays_within_jitter_bounds() {
        for attempt in 0..6 {
            let base = 500u64 << attempt;
            let spread = base / 5;
            for _ in 0..32 {
                let millis = retry_delay(attempt).as_millis() as u64;
                assert!(
                    (base - spread..=base + spread).contains(&millis),
                    "attempt {attempt}: {millis} ms outside ±20% of {base} ms"
                );
            }
        }
    }

    #[tokio::test]
    async fn hash_cache_survives_reload() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("mods").join("example.jar");
        tokio::fs::create_dir_all(dest.parent().expect("parent")).await?;
        let body = b"cached mod bytes";
        tokio::fs::write(&dest, body).await?;

        let task = DownloadTask::new(
            "http://127.0.0.1/example.jar".to_owned(),
            dest.clone(),
            Some(hash_bytes(HashAlgo::Sha256, body)),
            HashAlgo::Sha256,
            Some(body.len() as u64),
            "example",
        );
        let cache = HashCache::load(dir.path().to_path_buf()).await;
        let metadata = tokio::fs::metadata(&dest).await?;
        assert!(!cache.matches(&task, &metadata));
        cache.record(&task, &metadata);
        assert!(cache.matches(&task, &metadata));
        cache.persist().await;

        let reloaded = HashCache::load(dir.path().to_path_buf()).await;
        let metadata = tokio::fs::metadata(&dest).await?;
        assert!(reloaded.matches(&task, &metadata));

        // A different expected hash must invalidate the entry.
        let mut changed = task.clone();
        changed.sha = Some("0".repeat(64));
        assert!(!reloaded.matches(&changed, &metadata));
        Ok(())
    }

    #[tokio::test]
    async fn resumes_a_partial_file_with_range() -> Result<(), Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let body = b"resume this download ".repeat(4096);
        let body_for_server = body.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0u8; 4096];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
                let start = text
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .and_then(|value| value.split('-').next())
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                let remaining = &body_for_server[start..];
                let header = if start > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{}/{}\r\nConnection: close\r\n\r\n",
                        remaining.len(),
                        body_for_server.len() - 1,
                        body_for_server.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        remaining.len()
                    )
                };
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(remaining).await;
            }
        });

        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("payload.bin");
        let split = body.len() / 2;
        tokio::fs::write(part_path(&dest), &body[..split]).await?;

        let client = reqwest::Client::builder().no_proxy().build()?;
        let downloader = Downloader::new_with_client(client, 1);
        let task = DownloadTask::new(
            format!("http://{address}/payload.bin"),
            dest.clone(),
            Some(hash_bytes(HashAlgo::Sha256, &body)),
            HashAlgo::Sha256,
            Some(body.len() as u64),
            "payload",
        );
        let summary = downloader.download_many(vec![task], None).await?;
        assert_eq!(summary.downloaded, 1);
        assert_eq!(tokio::fs::read(&dest).await?, body);
        assert!(!part_path(&dest).exists());
        server.await?;
        Ok(())
    }
}
