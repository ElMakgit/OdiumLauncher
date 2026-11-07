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

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
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

#[derive(Debug, Clone)]
pub struct Downloader {
    inner: Arc<DownloaderInner>,
}

#[derive(Debug)]
struct DownloaderInner {
    client: reqwest::Client,
    concurrency: usize,
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
        let total_files = tasks.len();
        let overall_total = tasks.iter().filter_map(|task| task.size).sum::<u64>();
        let overall_done = Arc::new(AtomicU64::new(0));
        let semaphore = Arc::new(Semaphore::new(self.inner.concurrency));
        let mut set: JoinSet<Result<(usize, FileOutcome), DownloadError>> = JoinSet::new();

        for (index, task) in tasks.into_iter().enumerate() {
            let downloader = self.clone();
            let semaphore = Arc::clone(&semaphore);
            let overall_done = Arc::clone(&overall_done);
            let events = events.clone();
            set.spawn(async move {
                // The permit is acquired inside the task, so spawning thousands of
                // lightweight futures does not open thousands of sockets.
                let permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| DownloadError::Semaphore)?;
                let outcome = downloader
                    .download_one(
                        &task,
                        index,
                        total_files,
                        events.as_ref(),
                        &overall_done,
                        overall_total,
                    )
                    .await;
                drop(permit);
                outcome.map(|outcome| (index, outcome))
            });
        }

        let mut summary = DownloadSummary {
            total_files,
            ..DownloadSummary::default()
        };
        let mut first_error: Option<DownloadError> = None;

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
                    tracing::warn!(%error, "download task failed");
                    if first_error.is_none() {
                        first_error = Some(error);
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

        match first_error {
            Some(error) => Err(error),
            None => Ok(summary),
        }
    }

    async fn download_one(
        &self,
        task: &DownloadTask,
        index: usize,
        total: usize,
        events: Option<&mpsc::UnboundedSender<DownloadEvent>>,
        overall_done: &AtomicU64,
        overall_total: u64,
    ) -> Result<FileOutcome, DownloadError> {
        if let Some(expected) = task.sha.as_deref() {
            if !expected.is_empty() && verify_hash(&task.dest, expected, task.algo).await? {
                let bytes = match task.size {
                    Some(size) => size,
                    None => tokio::fs::metadata(&task.dest)
                        .await
                        .map(|metadata| metadata.len())
                        .unwrap_or(0),
                };
                overall_done.fetch_add(bytes, Ordering::Relaxed);
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

        send(
            events,
            DownloadEvent::Started {
                index,
                total,
                label: task.label.clone(),
                size: task.size,
            },
        );

        let mut last_error: Option<DownloadError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self
                .try_download(task, index, events, overall_done, overall_total)
                .await
            {
                Ok(outcome) => {
                    overall_done.fetch_add(outcome.bytes, Ordering::Relaxed);
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
                Err(error) if error.is_retryable() && attempt + 1 < MAX_ATTEMPTS => {
                    let delay = Duration::from_millis(500 * (1u64 << attempt));
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
    ) -> Result<FileOutcome, DownloadError> {
        let parsed = reqwest::Url::parse(&task.url)
            .map_err(|_| DownloadError::InvalidUrl(task.url.clone()))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => return Err(DownloadError::UnsupportedScheme(other.to_owned())),
        }

        if let Some(parent) = task.dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| io_error(parent, source))?;
        }

        let part = part_path(&task.dest);
        let response = self
            .inner
            .client
            .get(&task.url)
            .send()
            .await
            .map_err(|source| DownloadError::Network {
                url: task.url.clone(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(DownloadError::HttpStatus {
                status: status.as_u16(),
                url: task.url.clone(),
            });
        }

        let total = response.content_length().or(task.size);
        if let (Some(expected), Some(actual)) = (task.size, response.content_length()) {
            if expected > 0 && expected != actual {
                tracing::warn!(
                    url = %task.url,
                    expected,
                    actual,
                    "server Content-Length differs from manifest size"
                );
            }
        }

        let file = tokio::fs::File::create(&part)
            .await
            .map_err(|source| io_error(&part, source))?;
        let mut writer = tokio::io::BufWriter::with_capacity(COPY_BUFFER, file);
        let mut hasher = task.algo.new_hasher();
        let mut stream = response.bytes_stream();
        let mut downloaded: u64 = 0;
        let mut last_emit = Instant::now() - PROGRESS_INTERVAL;

        while let Some(chunk) = stream.next().await {
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

        if let Err(source) = tokio::fs::rename(&part, &task.dest).await {
            // Windows normally replaces on rename. If an antivirus or a stale
            // handle blocks it, remove the destination and retry once.
            tracing::debug!(path = %task.dest.display(), %source, "rename failed, retrying after remove");
            let _ = tokio::fs::remove_file(&task.dest).await;
            tokio::fs::rename(&part, &task.dest)
                .await
                .map_err(|source| io_error(&task.dest, source))?;
        }

        Ok(FileOutcome {
            skipped: false,
            bytes: downloaded,
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
}
