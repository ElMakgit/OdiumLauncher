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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex, Semaphore};
use tokio::task::JoinSet;

pub const USER_AGENT: &str = concat!(
    "OdiumLauncher/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/odium-launcher/odium-launcher)"
);

/// Cooperative cancellation shared by the UI and every long-running stage.
///
/// `watch` is used instead of an `AtomicBool` polled every 100 ms: waiters are
/// woken immediately and there is no race between "check the flag" and
/// "subscribe to the notification".
#[derive(Debug, Clone)]
pub struct CancelToken {
    sender: Arc<watch::Sender<bool>>,
}

impl CancelToken {
    pub fn new() -> Self {
        let (sender, _receiver) = watch::channel(false);
        Self {
            sender: Arc::new(sender),
        }
    }

    /// Marks the operation as cancelled and wakes every waiter.
    pub fn cancel(&self) {
        // `send_replace` also updates the stored value when no receiver is
        // currently subscribed, unlike `send`, which fails on a closed channel.
        self.sender.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }

    /// Resolves as soon as the token is cancelled. Returns immediately when the
    /// token was already cancelled before the call.
    pub async fn cancelled(&self) {
        let mut receiver = self.sender.subscribe();
        if *receiver.borrow() {
            return;
        }
        // `changed()` returns an error when the sender is dropped, which can
        // only happen after every clone is gone; treat that as cancellation.
        let _ = receiver.changed().await;
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

const MAX_ATTEMPTS: usize = 3;
/// An integrity failure means the whole body has to be fetched again. Retrying
/// it as often as a transient network error would turn one bad CDN response
/// into three full downloads, so the second attempt is the last one.
const MAX_INTEGRITY_ATTEMPTS: usize = 2;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);
const COPY_BUFFER: usize = 64 * 1024;
/// A stalled body must be detected without imposing a wall-clock deadline on
/// the whole transfer: a 500 MiB jar on a slow line may legitimately take an
/// hour, but it should never wait forever for the next chunk.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard limits for a generic archive extraction. `overrides` has its own,
/// pack-specific budget; these protect every other caller (natives, Java,
/// runtime zips) from a decompression bomb.
const MAX_ZIP_ENTRIES: usize = 100_000;
const MAX_ZIP_ENTRY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ZIP_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Retry-After values above this are clamped: a CDN asking for an hour must not
/// freeze the launcher, and the user can always press "Отмена".
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
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
    /// Further hosts that serve the exact same object, tried in order when the
    /// primary fails. The `.part` file is kept across the switch, so a mirror
    /// that dies halfway through a jar costs one connection instead of the
    /// whole download.
    pub alternates: Vec<String>,
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
            alternates: Vec::new(),
            dest: dest.into(),
            // An empty digest is not a digest. Keeping `Some("")` made the
            // downloader compare every byte stream against "" and fail with a
            // hash mismatch, while `file_matches` treated the same value as
            // "already installed".
            sha: sha.filter(|hash| !hash.trim().is_empty()),
            algo,
            size,
            label: label.into(),
        }
    }

    /// Adds fallback URLs for the same bytes. Order is preserved and the
    /// primary URL is never duplicated.
    #[must_use]
    pub fn with_alternates<I, S>(mut self, urls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for url in urls {
            let url = url.into();
            if url != self.url && !self.alternates.contains(&url) {
                self.alternates.push(url);
            }
        }
        self
    }

    /// Primary URL followed by every distinct fallback.
    fn sources(&self) -> Vec<&str> {
        let mut sources = Vec::with_capacity(self.alternates.len() + 1);
        sources.push(self.url.as_str());
        for url in &self.alternates {
            if !sources.contains(&url.as_str()) {
                sources.push(url.as_str());
            }
        }
        sources
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
    /// Volume + file index (Windows) or device + inode (Unix). Size and mtime
    /// alone let a replaced file reuse an old cache entry; the identity closes
    /// that hole. Empty for caches written by older builds.
    #[serde(default)]
    file_id: String,
    algo: String,
    hash: String,
}

/// Stable identity of the file on disk, independent of its contents.
#[cfg(windows)]
fn file_identity(path: &Path) -> String {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    // Stable Rust does not expose the file index through `Metadata`, so reopen
    // the file and ask the filesystem for the volume serial and file index.
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` owns a live handle and `info` is a valid, aligned
    // out-parameter for the duration of the call.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    if ok == 0 {
        return String::new();
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    format!("{:x}:{index:x}", info.dwVolumeSerialNumber)
}

#[cfg(unix)]
fn file_identity(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(metadata) => format!("{:x}:{:x}", metadata.dev(), metadata.ino()),
        Err(_) => String::new(),
    }
}

#[cfg(not(any(windows, unix)))]
fn file_identity(_path: &Path) -> String {
    String::new()
}

/// Validators stored next to a `.part` file. `If-Range` makes a resumed
/// download safe: when the remote object changed, the server answers `200`
/// with the full body instead of appending unrelated bytes.
///
/// Validators are host-specific: after a failover the same path is served by a
/// different CDN node with its own ETag, so `source` records who issued them
/// and a request to another host omits `If-Range` instead of forcing a restart.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PartMeta {
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    last_modified: Option<String>,
    #[serde(default)]
    source: Option<String>,
    /// Full object size as reported by the host that opened the `.part`.
    #[serde(default)]
    total: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HashCacheData {
    #[serde(default)]
    entries: BTreeMap<String, HashCacheEntry>,
}

/// Serialises the read-merge-write cycle of the persisted hash cache. Two
/// batches (for example assets and libraries) can finish at the same time; each
/// one only owns its own entries, so a blind overwrite used to drop the other
/// batch's records.
static HASH_CACHE_WRITE_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

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
            let identity = file_identity(&task.dest);
            // Legacy entries have no identity; they are accepted once and
            // rewritten by `record` with the new field.
            (entry.file_id.is_empty() || entry.file_id == identity)
                && entry.size == metadata.len()
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
            file_id: file_identity(&task.dest),
            algo: algo_name(task.algo).to_owned(),
            hash: expected.to_ascii_lowercase(),
        };
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, entry);
    }

    async fn persist(&self) {
        let ours = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let _guard = HASH_CACHE_WRITE_LOCK.lock().await;
        // Merge with whatever another batch persisted while we were running.
        let mut entries = match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice::<HashCacheData>(&bytes)
                .map(|data| data.entries)
                .unwrap_or_default(),
            Err(_) => BTreeMap::new(),
        };
        entries.extend(ours);
        let data = HashCacheData { entries };
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

/// Reads `Retry-After` in its numeric-seconds form. HTTP-date values are rare
/// for download CDNs and are ignored, falling back to the exponential delay.
fn retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    raw.parse::<u64>().ok().map(Duration::from_secs)
}

/// Free bytes available to the current user on the volume that holds `path`.
/// The path itself may not exist yet (the first launch creates it), so the
/// closest existing ancestor is probed instead.
#[cfg(windows)]
fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut probe = path.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            return None;
        }
    }
    let wide = probe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let mut free = 0u64;
    // SAFETY: `wide` is a NUL-terminated UTF-16 path and the remaining pointers
    // are valid, aligned `u64` out-parameters (the last two may be null).
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
fn free_space(_path: &Path) -> Option<u64> {
    // The launcher is Windows-first; other platforms skip the pre-flight check
    // and rely on the filesystem error if the disk fills up.
    None
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
    cancel: Option<CancelToken>,
}

impl DownloadContext {
    /// Cooperative cancellation is checked by every worker between chunks, so
    /// a cancelled batch stops within one network chunk instead of waiting for
    /// the whole file set to finish.
    fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelToken::is_cancelled)
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
    cancel: Option<CancelToken>,
    /// Shared by every clone of this downloader, so a mirror that died during
    /// one file is tried last for the rest of the batch instead of costing a
    /// timeout per file.
    health: Arc<HostHealth>,
}

/// Small circuit breaker for download hosts.
///
/// A failing host collects strikes; after [`HOST_STRIKES_BEFORE_COOLING`] the
/// host is put on a cooldown that doubles with every further strike. Cooling
/// hosts are not dropped: they move to the end of the source list, so a batch
/// that has no other source still finishes instead of failing outright.
#[derive(Debug, Default)]
struct HostHealth {
    hosts: std::sync::Mutex<std::collections::HashMap<String, HostState>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct HostState {
    strikes: u32,
    cooling_until: Option<Instant>,
}

const HOST_STRIKES_BEFORE_COOLING: u32 = 2;
const HOST_BASE_COOLDOWN: Duration = Duration::from_secs(45);
const HOST_MAX_COOLDOWN: Duration = Duration::from_secs(15 * 60);

impl HostHealth {
    fn is_cooling(&self, host: &str) -> bool {
        let Ok(mut hosts) = self.hosts.lock() else {
            return false;
        };
        let Some(state) = hosts.get_mut(host) else {
            return false;
        };
        match state.cooling_until {
            Some(until) if until > Instant::now() => true,
            Some(_) => {
                // The cooldown expired: give the host a clean slate but keep
                // its strikes until it actually succeeds.
                state.cooling_until = None;
                false
            }
            None => false,
        }
    }

    fn record_failure(&self, host: &str) {
        let Ok(mut hosts) = self.hosts.lock() else {
            return;
        };
        let state = hosts.entry(host.to_owned()).or_default();
        state.strikes = state.strikes.saturating_add(1);
        if state.strikes >= HOST_STRIKES_BEFORE_COOLING {
            let doublings = state.strikes - HOST_STRIKES_BEFORE_COOLING;
            let cooldown = HOST_BASE_COOLDOWN
                .saturating_mul(1u32 << doublings.min(5))
                .min(HOST_MAX_COOLDOWN);
            state.cooling_until = Some(Instant::now() + cooldown);
        }
    }

    fn record_success(&self, host: &str) {
        if let Ok(mut hosts) = self.hosts.lock() {
            hosts.remove(host);
        }
    }
}

/// Lower-cased host of an absolute URL, used as the health and validator key.
fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

/// Scheme + host + port, used to decide whether a stored validator belongs to
/// the host that is about to answer. Two mirrors may share an IP and differ
/// only by port, so the origin is the right scope for `If-Range`.
fn origin_of(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// First byte offset from a `Content-Range: bytes <start>-<end>/<total>` header.
fn content_range_start(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .trim();
    let rest = value.strip_prefix("bytes")?.trim_start();
    rest.split('-').next()?.trim().parse().ok()
}

/// Full object size from a `Content-Range` header.
fn content_range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .trim();
    value.rsplit('/').next()?.trim().parse().ok()
}

/// A `.part` that cannot be trusted any more: drop both the bytes and the
/// validators so the next attempt starts from zero.
async fn discard_part(part: &Path, meta: &Path) {
    let _ = tokio::fs::remove_file(part).await;
    let _ = tokio::fs::remove_file(meta).await;
}

impl Downloader {
    pub fn new(concurrency: usize) -> Result<Self, DownloadError> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            // `timeout` would cap the whole response body. `read_timeout` only
            // aborts when no bytes arrive for a minute, so a slow but healthy
            // download of a large jar is not killed mid-transfer.
            .read_timeout(READ_IDLE_TIMEOUT)
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
                health: Arc::new(HostHealth::default()),
            }),
        }
    }

    /// Returns a clone that cooperatively cancels every batch started through
    /// it. Domain crates keep their `Downloader` parameter unchanged: the UI
    /// passes the cancellable clone down the pipeline once.
    pub fn with_cancel(&self, cancel: CancelToken) -> Self {
        Self {
            inner: Arc::new(DownloaderInner {
                client: self.inner.client.clone(),
                concurrency: self.inner.concurrency,
                cancel: Some(cancel),
                // Keep the circuit breaker warm across cancellation: a host
                // that just died is still a bad choice for the retry.
                health: Arc::clone(&self.inner.health),
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
            .is_some_and(CancelToken::is_cancelled)
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

        // Pre-flight disk check. Only files that are not already the expected
        // size count, so a fully installed instance is never rejected because
        // of its total footprint.
        let mut missing_bytes = 0u64;
        for task in &tasks {
            let Some(size) = task.size.filter(|size| *size > 0) else {
                continue;
            };
            let existing = tokio::fs::metadata(&task.dest)
                .await
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            if existing != size {
                missing_bytes = missing_bytes.saturating_add(size);
            }
        }
        if missing_bytes > 0 {
            if let Some(root) = common_root(&tasks) {
                if let Some(free) = free_space(&root) {
                    let needed = missing_bytes
                        .saturating_add(missing_bytes / 10)
                        .saturating_add(64 * 1024 * 1024);
                    if free < needed {
                        return Err(DownloadError::NotEnoughSpace {
                            path: root,
                            needed,
                            free,
                        });
                    }
                }
            }
        }

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
                    context.cancel.as_ref(),
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
                Err(error)
                    if error.is_retryable()
                        && attempt + 1
                            < if error.is_integrity() {
                                MAX_INTEGRITY_ATTEMPTS
                            } else {
                                MAX_ATTEMPTS
                            } =>
                {
                    let delay = error
                        .retry_after()
                        .map(|hint| hint.min(MAX_RETRY_AFTER))
                        .unwrap_or_else(|| retry_delay(attempt));
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
                    // Keep the `.part`: a transient network failure is exactly
                    // what Range resume is for. The next launch (or the next
                    // attempt from another batch) continues from the verified
                    // prefix instead of starting a multi-hundred-megabyte
                    // download over.
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

    /// Downloads one task, failing over between the mirrors that publish the
    /// same object. Every source shares one `.part` file, so a mirror that
    /// disappears in the middle of a jar costs a reconnect, not the transfer.
    async fn try_download(
        &self,
        task: &DownloadTask,
        index: usize,
        events: Option<&mpsc::UnboundedSender<DownloadEvent>>,
        overall_done: &AtomicU64,
        overall_total: u64,
        cancel: Option<&CancelToken>,
    ) -> Result<FileOutcome, DownloadError> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
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

        // Healthy hosts first. A host that failed twice in this batch waits at
        // the back of the queue, but is still tried before giving up, so a
        // single-source task never loses its only chance.
        let mut ordered: Vec<&str> = Vec::new();
        let mut cooling: Vec<&str> = Vec::new();
        for source in task.sources() {
            match host_of(source) {
                Some(host) if self.inner.health.is_cooling(&host) => cooling.push(source),
                _ => ordered.push(source),
            }
        }
        ordered.append(&mut cooling);
        let source_count = ordered.len();

        let mut last_error: Option<DownloadError> = None;
        for (position, source) in ordered.into_iter().enumerate() {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(DownloadError::Cancelled);
            }
            // The per-source copy carries its own URL, so errors, `.part`
            // provenance and logs name the host that really answered.
            let mut candidate = task.clone();
            candidate.url = source.to_owned();
            match self
                .try_source(
                    &candidate,
                    index,
                    events,
                    overall_done,
                    overall_total,
                    cancel,
                )
                .await
            {
                Ok(outcome) => {
                    if let Some(host) = host_of(&candidate.url) {
                        self.inner.health.record_success(&host);
                    }
                    if position > 0 {
                        // Support logs should show which mirror really served
                        // the file, not just the URL that was asked for first.
                        tracing::info!(
                            primary = %task.url,
                            served_by = %candidate.url,
                            label = %task.label,
                            "download served by a fallback mirror"
                        );
                    }
                    return Ok(outcome);
                }
                Err(DownloadError::Cancelled) => return Err(DownloadError::Cancelled),
                Err(error) => {
                    if error.is_local() {
                        // Disk, permissions or a broken manifest: another host
                        // cannot fix this.
                        return Err(error);
                    }
                    if let Some(host) = host_of(&candidate.url) {
                        if error.blames_host() {
                            self.inner.health.record_failure(&host);
                        }
                    }
                    if position + 1 < source_count {
                        tracing::warn!(
                            url = %candidate.url,
                            %error,
                            "download source failed, switching to the next mirror"
                        );
                    }
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| DownloadError::InvalidUrl(task.url.clone())))
    }

    /// One attempt against one URL. The `.part` survives every failure except
    /// a full-body checksum mismatch, so the next source resumes from the last
    /// verified byte instead of starting the file over.
    async fn try_source(
        &self,
        task: &DownloadTask,
        index: usize,
        events: Option<&mpsc::UnboundedSender<DownloadEvent>>,
        overall_done: &AtomicU64,
        overall_total: u64,
        cancel: Option<&CancelToken>,
    ) -> Result<FileOutcome, DownloadError> {
        if let Some(parent) = task.dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| io_error(parent, source))?;
        }

        let part = part_path(&task.dest);
        let meta_path = part_meta_path(&part);
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
            let _ = tokio::fs::remove_file(&meta_path).await;
            resume_from = 0;
        }
        let mut part_meta: Option<PartMeta> = if resume_from > 0 {
            match tokio::fs::read(&meta_path).await {
                Ok(bytes) => serde_json::from_slice(&bytes).ok(),
                Err(_) => None,
            }
        } else {
            None
        };
        // A `.part` opened against another object version is useless even when
        // its byte count looks plausible.
        if let (Some(meta), Some(expected)) = (part_meta.as_ref(), task.size.filter(|v| *v > 0)) {
            if meta.total.is_some_and(|total| total != expected) {
                discard_part(&part, &meta_path).await;
                resume_from = 0;
                part_meta = None;
            }
        }
        // `If-Range` is only meaningful for the host that issued the ETag. Each
        // mirror has its own CDN edge; a foreign validator would turn a resume
        // into a full `200` restart and throw good bytes away.
        let origin = origin_of(&task.url);
        let same_host = part_meta
            .as_ref()
            .and_then(|meta| meta.source.as_deref())
            .is_none_or(|owner| origin.as_deref() == Some(owner));
        let mut request = self.inner.client.get(&task.url);
        if resume_from > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
            if same_host {
                if let Some(validator) = part_meta
                    .as_ref()
                    .and_then(|meta| meta.etag.as_ref().or(meta.last_modified.as_ref()))
                {
                    request = request.header(reqwest::header::IF_RANGE, validator);
                }
            }
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
            // A `.part` at or past EOF is usually stale, but when its length
            // exactly matches the manifest and the hash checks out, the file is
            // already complete: finish it instead of downloading again.
            let complete = task
                .size
                .is_some_and(|expected| expected > 0 && expected == resume_from);
            if complete {
                let expected = task.sha.as_deref().unwrap_or_default();
                if !expected.is_empty() && verify_hash(&part, expected, task.algo).await? {
                    match tokio::fs::rename(&part, &task.dest).await {
                        Ok(()) => {
                            let _ = tokio::fs::remove_file(&meta_path).await;
                            tracing::info!(
                                path = %task.dest.display(),
                                bytes = resume_from,
                                "completed a download from a full .part file"
                            );
                            return Ok(FileOutcome {
                                skipped: false,
                                bytes: resume_from,
                            });
                        }
                        Err(_) if destination_matches(task).await => {
                            let _ = tokio::fs::remove_file(&part).await;
                            let _ = tokio::fs::remove_file(&meta_path).await;
                            return Ok(FileOutcome {
                                skipped: false,
                                bytes: resume_from,
                            });
                        }
                        Err(source) => return Err(io_error(&task.dest, source)),
                    }
                }
            }
            // A stale part at or past EOF: start the file over.
            discard_part(&part, &meta_path).await;
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
                retry_after: retry_after_header(response.headers()),
            });
        }
        if resume_from > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            // The server ignored `Range` (or `If-Range` proved the object
            // changed) and sent the whole body from byte 0.
            resume_from = 0;
        }
        if resume_from > 0 {
            if let Some(start) = content_range_start(response.headers()) {
                if start != resume_from {
                    // A mirror that answers with another offset would glue
                    // unrelated bytes together; drop the prefix and restart.
                    discard_part(&part, &meta_path).await;
                    return Err(DownloadError::ResumeMismatch { path: part.clone() });
                }
            }
        }
        let remaining = response.content_length();
        let response_total = if status == reqwest::StatusCode::PARTIAL_CONTENT {
            content_range_total(response.headers())
                .or_else(|| remaining.map(|rest| resume_from.saturating_add(rest)))
        } else {
            remaining
        };
        if resume_from > 0 {
            if let (Some(expected), Some(actual)) =
                (task.size.filter(|value| *value > 0), response_total)
            {
                if expected != actual {
                    // The prefix belongs to another version of the object, so
                    // appending this body would corrupt the file.
                    discard_part(&part, &meta_path).await;
                    return Err(DownloadError::SizeMismatch {
                        path: task.dest.clone(),
                        expected,
                        actual,
                    });
                }
            }
        }
        // Remember the validators before the body is consumed so the next run
        // can resume against the same object version on the same host.
        let validators = PartMeta {
            etag: header_value(&response, reqwest::header::ETAG),
            last_modified: header_value(&response, reqwest::header::LAST_MODIFIED),
            source: origin,
            total: response_total.or(task.size),
        };
        if let Ok(json) = serde_json::to_vec(&validators) {
            let _ = tokio::fs::write(&meta_path, json).await;
        }

        let total = task.size.or(response_total);
        if let (Some(expected), Some(actual)) =
            (task.size.filter(|value| *value > 0), response_total)
        {
            if expected != actual {
                tracing::warn!(
                    url = %task.url,
                    expected,
                    actual,
                    "server reports a different size than the manifest"
                );
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
                discard_part(&part, &meta_path).await;
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
            if cancel.is_some_and(CancelToken::is_cancelled) {
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
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(source) => {
                    // Do not lose the bytes still sitting in the buffer: the
                    // file length is the resume offset, so flushing here means
                    // the next attempt starts at the real end of the file.
                    let _ = writer.flush().await;
                    return Err(DownloadError::Network {
                        url: task.url.clone(),
                        source,
                    });
                }
            };
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

        // A clean end of stream only means the socket closed: a proxy or a
        // mirror being killed can truncate a chunked body without a transport
        // error. Compare against the length the host announced, so half a jar
        // is never renamed into place.
        if let Some(expected) = response_total {
            if downloaded < expected {
                return Err(DownloadError::Truncated {
                    url: task.url.clone(),
                    expected,
                    actual: downloaded,
                });
            }
            if downloaded > expected {
                discard_part(&part, &meta_path).await;
                return Err(DownloadError::SizeMismatch {
                    path: task.dest.clone(),
                    expected,
                    actual: downloaded,
                });
            }
        }

        if let Some(expected) = task.sha.as_deref() {
            let actual = hasher.finalize_hex();
            if !actual.eq_ignore_ascii_case(expected) {
                discard_part(&part, &meta_path).await;
                return Err(DownloadError::HashMismatch {
                    path: task.dest.clone(),
                    expected: expected.to_owned(),
                    actual,
                });
            }
        }

        if let Some(expected) = task.size {
            if expected > 0 && downloaded != expected {
                discard_part(&part, &meta_path).await;
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
                let _ = tokio::fs::remove_file(&meta_path).await;
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
                        let _ = tokio::fs::remove_file(&meta_path).await;
                        return Ok(FileOutcome {
                            skipped: false,
                            bytes: downloaded,
                        });
                    }
                    Err(error) => source = error,
                }
                if destination_matches(task).await {
                    let _ = tokio::fs::remove_file(&meta_path).await;
                    return Ok(FileOutcome {
                        skipped: false,
                        bytes: downloaded,
                    });
                }
            }
            return Err(io_error(&task.dest, source));
        }

        let _ = tokio::fs::remove_file(&meta_path).await;
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
        cancel: Option<&CancelToken>,
    ) -> Result<FileOutcome, DownloadError> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
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
        if cancel.is_some_and(CancelToken::is_cancelled) {
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

/// Sidecar that stores the HTTP validators for a resumable `.part` file.
fn part_meta_path(part: &Path) -> PathBuf {
    let mut name = part.file_name().unwrap_or_default().to_os_string();
    name.push(".json");
    part.with_file_name(name)
}

fn header_value(response: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
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

/// Blocking variant for callers that already run on a worker thread (for
/// example the `dist/` builder inside `spawn_blocking`).
pub fn hash_file_blocking(path: &Path, algo: HashAlgo) -> Result<String, DownloadError> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path).map_err(|source| io_error(path, source))?;
    let mut hasher = algo.new_hasher();
    let mut buffer = vec![0u8; COPY_BUFFER];
    loop {
        let read = file
            .read(&mut buffer)
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
    cancel: Option<&CancelToken>,
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
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(DownloadError::ZipTooLarge {
            path: zip_path.to_path_buf(),
            detail: format!("{} записей (максимум {MAX_ZIP_ENTRIES})", archive.len()),
        });
    }

    let mut extracted = Vec::new();
    let mut total_bytes: u64 = 0;
    for index in 0..archive.len() {
        if cancel.is_some_and(CancelToken::is_cancelled) {
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

        // Check the declared size before creating a file, then enforce the
        // same budget on the bytes actually produced. A zip bomb usually lies
        // in one of the two places.
        let declared = entry.size();
        if declared > MAX_ZIP_ENTRY_BYTES
            || total_bytes.saturating_add(declared) > MAX_ZIP_TOTAL_BYTES
        {
            return Err(DownloadError::ZipTooLarge {
                path: zip_path.to_path_buf(),
                detail: format!(
                    "{} распаковывается до {declared} байт (лимит на запись {MAX_ZIP_ENTRY_BYTES}, суммарно {MAX_ZIP_TOTAL_BYTES})",
                    entry.name()
                ),
            });
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
        let entry_limit = MAX_ZIP_ENTRY_BYTES.min(MAX_ZIP_TOTAL_BYTES.saturating_sub(total_bytes));
        let copied = match std::io::copy(
            &mut std::io::Read::take(&mut entry, entry_limit.saturating_add(1)),
            &mut output,
        ) {
            Ok(copied) => copied,
            Err(source) => {
                let _ = std::fs::remove_file(&out_path);
                return Err(io_error(&out_path, source));
            }
        };
        if copied > entry_limit {
            let _ = std::fs::remove_file(&out_path);
            return Err(DownloadError::ZipTooLarge {
                path: zip_path.to_path_buf(),
                detail: format!("{} превышает лимит распаковки", entry.name()),
            });
        }
        total_bytes = total_bytes.saturating_add(copied);
        extracted.push(relative);
    }
    Ok(extracted)
}

/// Rejects absolute paths, drive prefixes, `..` and empty components.
///
/// The check is host-independent on purpose: the same manifest is read by the
/// Windows and the macOS build, and on Unix `Path::new("C:\\Windows\\evil.dll")`
/// is a single normal component. Backslashes and colons are refused everywhere,
/// so an archive cannot escape the target directory on another operating system.
pub fn is_safe_relative(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    if text.is_empty() || text.contains('\\') || text.contains(':') {
        return false;
    }
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Finds the first file with `file_name` below `root` without following links.
pub fn find_file(root: &Path, file_name: &str) -> Option<PathBuf> {
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        // A corrupted or hostile tree can contain deep junctions/loops; an
        // unbounded walk used to be able to run for minutes on a frame.
        .max_depth(8)
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
    HttpStatus {
        status: u16,
        url: String,
        /// Server-provided backoff hint (`Retry-After`, seconds).
        retry_after: Option<Duration>,
    },
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
    #[error("соединение оборвалось на {actual} из {expected} байт для {url}")]
    Truncated {
        url: String,
        expected: u64,
        actual: u64,
    },
    #[error("нет свободного слота загрузки")]
    Semaphore,
    #[error("задача загрузки завершилась паникой: {0}")]
    Join(String),
    #[error("недостаточно свободного места в {path}: нужно {needed} байт, свободно {free}")]
    NotEnoughSpace {
        path: PathBuf,
        needed: u64,
        free: u64,
    },
    #[error("небезопасный путь в zip: {0}")]
    UnsafeZipPath(String),
    #[error("zip {path} превышает безопасные лимиты распаковки: {detail}")]
    ZipTooLarge { path: PathBuf, detail: String },
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
    /// Server-requested delay before the next attempt, when present.
    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::HttpStatus { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

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
            Self::Truncated { .. } => true,
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

    /// The failure is caused by the host that answered, so another mirror may
    /// still serve the file and the host deserves a strike.
    fn blames_host(&self) -> bool {
        match self {
            Self::Network { .. } => true,
            Self::HttpStatus { status, .. } => {
                // 404/403 on a mirror means "this mirror does not have the
                // artifact", which is also a reason to stop using it.
                *status == 404
                    || *status == 403
                    || *status == 408
                    || *status == 429
                    || (500..=599).contains(status)
            }
            Self::Truncated { .. } | Self::HashMismatch { .. } => true,
            // A wrong size can be either a stale mirror or a changed manifest;
            // trying the next source is cheaper than arguing with the server.
            Self::SizeMismatch { .. } => true,
            _ => false,
        }
    }

    /// Local problems (disk, permissions, bad URL, cancellation) are not fixed
    /// by another host, so the caller must not waste time failing over.
    fn is_local(&self) -> bool {
        matches!(
            self,
            Self::Cancelled
                | Self::InvalidUrl(_)
                | Self::UnsupportedScheme(_)
                | Self::Io { .. }
                | Self::NotEnoughSpace { .. }
                | Self::Semaphore
                | Self::Join(_)
                | Self::Client(_)
        )
    }

    /// The body was fully received but did not match the manifest, so a retry
    /// must start from byte zero.
    fn is_integrity(&self) -> bool {
        matches!(self, Self::HashMismatch { .. } | Self::SizeMismatch { .. })
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
        let dest = dir.path().join("objects").join("ab").join("hash");
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

    /// Windows 7 and other out-of-support systems no longer receive root
    /// certificate updates, so they lack ISRG Root X2, which signs
    /// `maven.neoforged.net`. The launcher must keep working there thanks to the
    /// bundled Mozilla store, without touching the system store at all.
    #[tokio::test]
    #[ignore = "network test: reaches maven.neoforged.net"]
    async fn bundled_roots_cover_the_neoforge_cdn() -> Result<(), Box<dyn std::error::Error>> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .tls_built_in_native_certs(false)
            .build()?;
        let response = client
            .get(
                "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.248/\
                 neoforge-21.1.248-installer.jar.sha1",
            )
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.text().await?;
        assert_eq!(
            body.trim().len(),
            40,
            "expected a sha1 digest, got {body:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_before_start_skips_the_network() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("never.bin");
        let cancel = CancelToken::new();
        cancel.cancel();
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
        let cancel = CancelToken::new();
        let downloader = Downloader::new(1)?.with_cancel(cancel.clone());
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
        cancel.cancel();

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
        assert!(is_safe_relative(Path::new("./mods/example.jar")));
        assert!(!is_safe_relative(Path::new("../example.jar")));
        assert!(!is_safe_relative(Path::new("C:\\Windows\\evil.dll")));
        assert!(!is_safe_relative(Path::new("C:/Windows/evil.dll")));
        // Backslashes are separators on Windows and plain characters on Unix:
        // both interpretations must be rejected, or the same archive would be
        // safe on one OS and unsafe on the other.
        assert!(!is_safe_relative(Path::new("mods\\example.jar")));
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
    async fn hash_cache_rejects_a_replaced_file() -> Result<(), Box<dyn std::error::Error>> {
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
        cache.record(&task, &metadata);
        assert!(cache.matches(&task, &metadata));

        let key = dest
            .strip_prefix(dir.path())?
            .to_string_lossy()
            .replace('\\', "/");
        {
            let mut entries = cache
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = entries.get_mut(&key).expect("recorded entry");
            entry.file_id = "deadbeef:1".to_owned();
        }
        assert!(
            !cache.matches(&task, &metadata),
            "another file identity must invalidate the cache"
        );
        {
            let mut entries = cache
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = entries.get_mut(&key).expect("recorded entry");
            entry.file_id.clear();
        }
        assert!(
            cache.matches(&task, &metadata),
            "legacy entries without an identity stay usable"
        );
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

    /// A mirror that dies in the middle of a jar must not force a restart: the
    /// `.part` is handed to the next host and only the missing tail is fetched.
    #[tokio::test]
    async fn dying_mirror_hands_the_part_to_another_host() -> Result<(), Box<dyn std::error::Error>>
    {
        let body = b"a mirror can die mid-file ".repeat(4096);
        let cut = 4096;

        let dying = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let dying_address = dying.local_addr()?;
        let dying_body = body.clone();
        let dying_server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = dying.accept().await {
                let mut request = vec![0u8; 4096];
                let _ = socket.read(&mut request).await;
                // Announce the full length, send a prefix, then vanish.
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"edge-a\"\r\nConnection: close\r\n\r\n",
                    dying_body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&dying_body[..cut]).await;
            }
        });

        let healthy = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let healthy_address = healthy.local_addr()?;
        let healthy_body = body.clone();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let healthy_server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = healthy.accept().await {
                let mut request = vec![0u8; 4096];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
                let start = text
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .and_then(|value| value.split('-').next())
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                let if_range = text.lines().any(|line| line.starts_with("if-range:"));
                let remaining = &healthy_body[start..];
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{}/{}\r\nETag: \"edge-b\"\r\nConnection: close\r\n\r\n",
                    remaining.len(),
                    healthy_body.len() - 1,
                    healthy_body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(remaining).await;
                let _ = seen_tx.send((start, if_range));
            }
        });

        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("payload.bin");
        let client = reqwest::Client::builder().no_proxy().build()?;
        let downloader = Downloader::new_with_client(client, 1);
        let task = DownloadTask::new(
            format!("http://{dying_address}/payload.bin"),
            dest.clone(),
            Some(hash_bytes(HashAlgo::Sha256, &body)),
            HashAlgo::Sha256,
            Some(body.len() as u64),
            "payload",
        )
        .with_alternates([format!("http://{healthy_address}/payload.bin")]);

        let summary = downloader.download_many(vec![task], None).await?;
        assert_eq!(summary.downloaded, 1);
        assert_eq!(tokio::fs::read(&dest).await?, body);
        let (start, if_range) = seen_rx.await?;
        assert_eq!(start, cut, "the second host must resume, not restart");
        assert!(
            !if_range,
            "another host has its own ETag; If-Range must not be forwarded"
        );
        let _ = dying_server.await;
        healthy_server.await?;
        Ok(())
    }

    /// Network test: the primary source is a closed port, so the task has to
    /// fail over and fetch the real installer from a mirror. Run with
    /// `cargo test -p pml-downloader -- --ignored`.
    #[tokio::test]
    #[ignore = "network test: downloads the NeoForge installer from a mirror"]
    async fn real_mirror_serves_the_installer_after_a_dead_source(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("neoforge-installer.jar");
        let artifact = "https://maven.creeperhost.net/net/neoforged/neoforge/21.1.248/neoforge-21.1.248-installer.jar";
        let client = reqwest::Client::builder().no_proxy().build()?;
        let downloader = Downloader::new_with_client(client, 2);
        let task = DownloadTask::new(
            "http://127.0.0.1:1/neoforge-installer.jar",
            dest.clone(),
            Some("e818014b2ef9cdaa76dc9fedddaf17f46ade8fce".to_owned()),
            HashAlgo::Sha1,
            None,
            "NeoForge installer",
        )
        .with_alternates([artifact]);
        downloader.download_many(vec![task], None).await?;
        let length = tokio::fs::metadata(&dest).await?.len();
        assert!(
            length > 1_000_000,
            "installer is suspiciously small: {length} bytes"
        );
        Ok(())
    }
}
