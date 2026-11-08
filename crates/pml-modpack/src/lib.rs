//! Modpack manifests and instance synchronisation.
//!
//! A manifest is a small JSON document that pins the Minecraft version, the
//! loader, the server and every managed file with a SHA-256 digest. Synchronising
//! an instance is a delta operation: files whose digest matches the last synced
//! state are not hashed or downloaded again, removed entries are deleted, and
//! optional files honour the per-source overrides stored in `config.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pml_config::{save_json_atomic, LoaderSpec, PortablePaths, ServerSpec};
use pml_downloader::{
    hash_bytes, is_safe_relative, DownloadEvent, DownloadTask, Downloader, HashAlgo,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

mod mrpack;

pub use mrpack::{
    archive_identity, extract_overrides, extract_overrides_when_changed, read_index, MrpackArchive,
    MrpackIndex, OVERRIDES_STATE_FILE,
};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const STATE_FILE: &str = ".pml_manifest_state.json";
/// Manifests are JSON documents of a few hundred KiB; anything larger is a
/// broken or hostile endpoint and must not be buffered without a cap.
const MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;

pub const BUILTIN_VANILLA_ID: &str = "builtin://vanilla";
pub const BUILTIN_VANILLA: &str = r#"{
  "schema_version": 1,
  "name": "Vanilla 1.21.1 + Fabric",
  "version": "1.0.0",
  "mc_version": "1.21.1",
  "loader": { "type": "fabric", "version": "0.16.5" },
  "server": { "ip": "45.85.60.39", "port": 25592 },
  "files": [],
  "optional_files": []
}"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub name: String,
    /// Human-readable pack version, e.g. `2.3.1`. Optional for old manifests.
    #[serde(default)]
    pub version: String,
    pub mc_version: String,
    pub loader: LoaderSpec,
    #[serde(default)]
    pub server: Option<ServerSpec>,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub optional_files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    /// Forward-slash relative path inside the instance directory.
    pub path: String,
    pub url: String,
    /// SHA-256 (preferred). Empty when the source only provides SHA-512/SHA-1.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    /// SHA-512, used by Modrinth `.mrpack` indexes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha512: String,
    /// SHA-1, the weakest fallback supported by `.mrpack` indexes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha1: String,
    #[serde(default)]
    pub size: u64,
}

impl ManifestFile {
    /// Strongest digest available for this file.
    pub fn best_hash(&self) -> Option<(HashAlgo, &str)> {
        for (algo, value) in [
            (HashAlgo::Sha256, self.sha256.as_str()),
            (HashAlgo::Sha512, self.sha512.as_str()),
            (HashAlgo::Sha1, self.sha1.as_str()),
        ] {
            let value = value.trim();
            if !value.is_empty() {
                return Some((algo, value));
            }
        }
        None
    }
}

impl Manifest {
    pub fn from_json(bytes: &[u8]) -> Result<Self, ModpackError> {
        let manifest: Self = serde_json::from_slice(bytes).map_err(ModpackError::ManifestJson)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_json_pretty(&self) -> Result<String, ModpackError> {
        serde_json::to_string_pretty(self).map_err(ModpackError::ManifestJson)
    }

    pub fn validate(&self) -> Result<(), ModpackError> {
        if self.schema_version == 0 || self.schema_version > MANIFEST_SCHEMA_VERSION {
            return Err(ModpackError::Validation(format!(
                "schema_version {} не поддерживается (максимум {MANIFEST_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        if self.name.trim().is_empty() {
            return Err(ModpackError::Validation("пустое имя сборки".to_owned()));
        }
        if self.mc_version.trim().is_empty() {
            return Err(ModpackError::Validation(
                "пустой mc_version в манифесте".to_owned(),
            ));
        }
        if self.loader.version.trim().is_empty() {
            return Err(ModpackError::Validation(
                "пустая версия загрузчика".to_owned(),
            ));
        }
        // `mc_version` and `loader.version` end up inside filesystem paths
        // (loader cache, version JSON folder). A manifest is external input, so
        // keep them on the ASCII path-safe allowlist instead of trusting them.
        validate_version_label("mc_version", &self.mc_version)?;
        validate_version_label("версии загрузчика", &self.loader.version)?;
        if let Some(server) = &self.server {
            server.validate()?;
        }

        let mut paths = BTreeSet::new();
        for file in self.files.iter().chain(self.optional_files.iter()) {
            file.validate()?;
            if !paths.insert(normalize_path(&file.path)) {
                return Err(ModpackError::Validation(format!(
                    "файл {} указан дважды",
                    file.path
                )));
            }
        }
        Ok(())
    }

    /// Required files plus the optional files the user has not disabled.
    pub fn enabled_files<'a>(
        &'a self,
        overrides: &'a BTreeMap<String, bool>,
    ) -> Vec<&'a ManifestFile> {
        let mut files: Vec<&ManifestFile> = self.files.iter().collect();
        for file in &self.optional_files {
            let enabled = overrides
                .get(&normalize_path(&file.path))
                .copied()
                .unwrap_or(true);
            if enabled {
                files.push(file);
            }
        }
        files
    }
}

impl ManifestFile {
    fn validate(&self) -> Result<(), ModpackError> {
        let normalized = normalize_path(&self.path);
        if normalized.is_empty() || !is_safe_relative(Path::new(&normalized)) {
            return Err(ModpackError::UnsafePath(self.path.clone()));
        }
        if normalized.starts_with('/') || normalized.contains(':') {
            return Err(ModpackError::UnsafePath(self.path.clone()));
        }
        if self.url.trim().is_empty() {
            return Err(ModpackError::Validation(format!(
                "у файла {} пустой URL",
                self.path
            )));
        }
        if !(self.url.starts_with("https://")
            || self.url.starts_with("http://")
            || self.url.starts_with("file://"))
        {
            return Err(ModpackError::Validation(format!(
                "неподдерживаемая схема URL у {}",
                self.path
            )));
        }
        let hash = self.best_hash().ok_or_else(|| {
            ModpackError::Validation(format!("у файла {} нет контрольной суммы", self.path))
        })?;
        let (algo, hash) = hash;
        if hash.len() != algo.hex_len()
            || !hash.chars().all(|character| character.is_ascii_hexdigit())
        {
            return Err(ModpackError::Validation(format!(
                "контрольная сумма {} у {} имеет неверный формат",
                algo_name(algo),
                self.path
            )));
        }
        Ok(())
    }
}

fn algo_name(algo: HashAlgo) -> &'static str {
    match algo {
        HashAlgo::Sha1 => "SHA-1",
        HashAlgo::Sha256 => "SHA-256",
        HashAlgo::Sha512 => "SHA-512",
    }
}

/// Allowlist for version strings that become path components: `1.21.1`,
/// `52.0.28`, `24w14a` all fit; `../../evil` and everything exotic does not.
fn validate_version_label(field: &str, value: &str) -> Result<(), ModpackError> {
    if value.is_empty() || value.len() > 96 {
        return Err(ModpackError::Validation(format!(
            "{field} имеет недопустимую длину"
        )));
    }
    if !value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
    }) {
        return Err(ModpackError::Validation(format!(
            "{field} содержит недопустимые символы: {value}"
        )));
    }
    Ok(())
}

fn default_schema_version() -> u32 {
    MANIFEST_SCHEMA_VERSION
}

/// Converts Windows separators to `/` and collapses `.` components so the same
/// manifest produces the same state keys on every platform.
pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn path_buf(relative: &str) -> PathBuf {
    relative
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(PathBuf::new(), |path, part| path.join(part))
}

#[derive(Debug, Error)]
pub enum ModpackError {
    #[error("некорректный манифест: {0}")]
    ManifestJson(serde_json::Error),
    #[error("манифест не прошёл проверку: {0}")]
    Validation(String),
    #[error("небезопасный путь в манифесте: {0}")]
    UnsafePath(String),
    #[error(transparent)]
    Download(#[from] pml_downloader::DownloadError),
    #[error(transparent)]
    Config(#[from] pml_config::ConfigError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("ошибка ввода-вывода {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("манифест {0} недоступен и в кэше нет копии")]
    ManifestUnavailable(String),
    #[error("операция отменена")]
    Cancelled,
}

/// Loads a manifest from `builtin://`, `file://` or HTTP(S), caching the last
/// successful HTTP response so a network outage does not brick the launcher.
pub async fn load_manifest(
    client: &reqwest::Client,
    url: &str,
    cache_dir: &Path,
) -> Result<Manifest, ModpackError> {
    if url == BUILTIN_VANILLA_ID {
        return Manifest::from_json(BUILTIN_VANILLA.as_bytes());
    }

    if let Some(path) = url.strip_prefix("file://") {
        let path = local_path(path);
        // Apply the same hard cap as the HTTP path so a hostile local file
        // cannot be read into memory without limit.
        let length = tokio::fs::metadata(&path)
            .await
            .map_err(|source| ModpackError::Io {
                path: path.clone(),
                source,
            })?
            .len();
        if length > MAX_MANIFEST_BYTES as u64 {
            return Err(ModpackError::Validation(format!(
                "манифест {url} больше {} МиБ",
                MAX_MANIFEST_BYTES / (1024 * 1024)
            )));
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|source| ModpackError::Io { path, source })?;
        return Manifest::from_json(&bytes);
    }

    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(ModpackError::Validation(format!(
            "неподдерживаемый URL манифеста: {url}"
        )));
    }

    let cache = cache_dir.join("manifests").join(format!(
        "{}.json",
        &hash_bytes(HashAlgo::Sha256, url.as_bytes())[..16]
    ));
    let response = match client.get(url).send().await {
        Ok(response) => match response.error_for_status() {
            Ok(response) => response,
            Err(error) => {
                // A CDN answering 404/503 is still a failed download: fall back
                // to the last good copy instead of aborting the launch.
                tracing::warn!(%error, %url, "manifest HTTP status failed, trying cache");
                return cached_manifest(&cache, url).await;
            }
        },
        Err(network_error) => {
            tracing::warn!(%network_error, %url, "manifest download failed, trying cache");
            return cached_manifest(&cache, url).await;
        }
    };

    let bytes = match read_limited_body(response, url).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, %url, "manifest body failed, trying cache");
            return cached_manifest(&cache, url).await;
        }
    };
    let manifest = match Manifest::from_json(&bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::warn!(%error, %url, "manifest response is invalid, trying cache");
            return cached_manifest(&cache, url).await;
        }
    };

    if let Some(parent) = cache.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| ModpackError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
    }
    let temporary = cache.with_extension("json.tmp");
    tokio::fs::write(&temporary, &bytes)
        .await
        .map_err(|source| ModpackError::Io {
            path: temporary.clone(),
            source,
        })?;
    tokio::fs::rename(&temporary, &cache)
        .await
        .map_err(|source| ModpackError::Io {
            path: cache.clone(),
            source,
        })?;
    Ok(manifest)
}

/// Streams an HTTP body into a byte vector while enforcing a hard size cap, so
/// a huge response cannot exhaust launcher memory.
async fn read_limited_body(
    response: reqwest::Response,
    url: &str,
) -> Result<Vec<u8>, ModpackError> {
    use futures_util::StreamExt as _;

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > MAX_MANIFEST_BYTES {
            return Err(ModpackError::Validation(format!(
                "манифест {url} больше {} МиБ",
                MAX_MANIFEST_BYTES / (1024 * 1024)
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Reads the last successfully downloaded manifest. A corrupt cache entry is
/// reported as unavailable so callers show the friendly Russian error instead
/// of a raw JSON parser message.
async fn cached_manifest(cache: &Path, url: &str) -> Result<Manifest, ModpackError> {
    match tokio::fs::read(cache).await {
        Ok(bytes) => Manifest::from_json(&bytes).map_err(|error| {
            tracing::warn!(%error, path = %cache.display(), "cached manifest is corrupt");
            ModpackError::ManifestUnavailable(url.to_owned())
        }),
        Err(_) => Err(ModpackError::ManifestUnavailable(url.to_owned())),
    }
}

/// A pack loaded from either a JSON manifest or a Modrinth `.mrpack`.
pub struct LoadedPack {
    pub manifest: Manifest,
    /// Present only for `.mrpack` sources; overrides are extracted from it
    /// after the delta sync.
    pub mrpack: Option<PathBuf>,
}

pub async fn load_pack(
    client: &reqwest::Client,
    source: &pml_config::Source,
    cache_dir: &Path,
) -> Result<LoadedPack, ModpackError> {
    match source.kind() {
        pml_config::SourceKind::Manifest => Ok(LoadedPack {
            manifest: load_manifest(client, &source.manifest_url, cache_dir).await?,
            mrpack: None,
        }),
        pml_config::SourceKind::Mrpack => {
            let pack = mrpack::fetch(client, &source.manifest_url, cache_dir).await?;
            let manifest = pack.index.to_manifest()?;
            Ok(LoadedPack {
                manifest,
                mrpack: Some(pack.archive),
            })
        }
    }
}

pub(crate) fn local_path(value: &str) -> PathBuf {
    // `file:///C:/dir` must become `C:/dir` on Windows, while Unix absolute
    // paths keep their leading slash.
    #[cfg(windows)]
    {
        if let Some(rest) = value.strip_prefix('/') {
            if rest.len() > 2 && rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    PathBuf::from(value)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SyncState {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    /// Pack version that produced the current instance. Used to decide whether
    /// an update is due a backup of worlds/configs before the delta sync.
    #[serde(default)]
    pack_version: String,
    #[serde(default)]
    files: BTreeMap<String, String>,
}

/// Returns the pack version recorded by the last successful sync, if any.
pub async fn synced_pack_version(instance_dir: &Path) -> Option<String> {
    let bytes = tokio::fs::read(instance_dir.join(STATE_FILE)).await.ok()?;
    let state: SyncState = serde_json::from_slice(&bytes).ok()?;
    (!state.pack_version.is_empty()).then_some(state.pack_version)
}

pub struct SyncRequest<'a> {
    pub manifest: &'a Manifest,
    pub instance_dir: &'a Path,
    pub overrides: &'a BTreeMap<String, bool>,
    pub progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub total_files: usize,
    pub downloaded: usize,
    pub skipped: usize,
    pub removed: usize,
    pub optional_enabled: usize,
}

#[derive(Debug, Clone)]
pub struct ModpackSync {
    paths: PortablePaths,
    downloader: Downloader,
}

impl ModpackSync {
    pub fn new(paths: PortablePaths, downloader: Downloader) -> Self {
        Self { paths, downloader }
    }

    /// Brings `instance_dir` to the state described by the manifest.
    pub async fn sync(&self, request: SyncRequest<'_>) -> Result<SyncReport, ModpackError> {
        tracing::debug!(root = %self.paths.root().display(), "syncing modpack");
        request.manifest.validate()?;
        tokio::fs::create_dir_all(request.instance_dir)
            .await
            .map_err(|source| ModpackError::Io {
                path: request.instance_dir.to_path_buf(),
                source,
            })?;

        let state_path = request.instance_dir.join(STATE_FILE);
        let mut state = load_state(&state_path).await;
        let enabled = request.manifest.enabled_files(request.overrides);
        // A set once, instead of an O(n·m) rebuild over every enabled file.
        let optional_paths = request
            .manifest
            .optional_files
            .iter()
            .map(|file| normalize_path(&file.path))
            .collect::<BTreeSet<_>>();
        let optional_enabled = enabled
            .iter()
            .filter(|file| optional_paths.contains(&normalize_path(&file.path)))
            .count();

        let mut desired = BTreeMap::new();
        let mut tasks = Vec::new();
        let mut report = SyncReport {
            total_files: enabled.len(),
            optional_enabled,
            ..SyncReport::default()
        };

        for file in enabled {
            let relative = normalize_path(&file.path);
            let Some((algo, digest)) = file.best_hash() else {
                return Err(ModpackError::Validation(format!(
                    "у файла {} нет контрольной суммы",
                    file.path
                )));
            };
            let digest = digest.to_ascii_lowercase();
            desired.insert(relative.clone(), digest.clone());
            let destination = request.instance_dir.join(path_buf(&relative));

            if state.files.get(&relative) == Some(&digest) && destination.is_file() {
                report.skipped += 1;
                continue;
            }

            tasks.push(DownloadTask::new(
                file.url.clone(),
                destination,
                Some(digest),
                algo,
                (file.size > 0).then_some(file.size),
                relative,
            ));
        }

        if !tasks.is_empty() {
            let summary = self
                .downloader
                .download_many(tasks, request.progress.clone())
                .await?;
            report.downloaded = summary.downloaded;
            report.skipped += summary.skipped;
        }

        // The downloader verifies every digest, so the state is only written
        // after the whole batch succeeded.
        for (relative, digest) in &desired {
            state.files.insert(relative.clone(), digest.clone());
        }

        let stale = state
            .files
            .keys()
            .filter(|path| !desired.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        for relative in stale {
            if is_safe_relative(Path::new(&relative)) {
                let path = request.instance_dir.join(path_buf(&relative));
                match tokio::fs::remove_file(&path).await {
                    // Only drop the state record once the file is really gone;
                    // otherwise a locked file would become a permanent orphan
                    // that nothing retries to delete.
                    Ok(()) => {
                        report.removed += 1;
                        state.files.remove(&relative);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        state.files.remove(&relative);
                    }
                    Err(error) => {
                        tracing::warn!(%error, path = %path.display(), "could not remove stale file");
                    }
                }
            } else {
                state.files.remove(&relative);
            }
        }

        state.schema_version = MANIFEST_SCHEMA_VERSION;
        state.pack_version = request.manifest.version.clone();
        save_json_atomic(&state_path, &state)?;
        tracing::info!(
            instance = %request.instance_dir.display(),
            total = report.total_files,
            downloaded = report.downloaded,
            skipped = report.skipped,
            removed = report.removed,
            "modpack sync finished"
        );
        Ok(report)
    }
}

async fn load_state(path: &Path) -> SyncState {
    let Ok(bytes) = tokio::fs::read(path).await else {
        return SyncState::default();
    };
    match serde_json::from_slice(&bytes) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "ignoring corrupt sync state");
            SyncState::default()
        }
    }
}

/// Outcome of [`reset_instance`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResetReport {
    /// Files that were removed.
    pub removed: usize,
    /// Files that could not be removed (usually locked by a running game).
    pub failed: usize,
}

/// Deletes the files installed by a pack so the next sync downloads them again.
///
/// Only manifest-managed paths and paths recorded by the last sync are
/// removed. Worlds (`saves`), screenshots, logs and `options.txt` are never
/// touched: resetting a build must not cost the player their progress. The sync
/// and override markers are removed as well, so the next run does not skip a
/// single file.
pub async fn reset_instance(
    instance_dir: &Path,
    manifest_files: &[String],
) -> Result<ResetReport, ModpackError> {
    let state_path = instance_dir.join(STATE_FILE);
    let mut targets = BTreeSet::new();
    for path in manifest_files {
        targets.insert(normalize_path(path));
    }
    if let Ok(bytes) = tokio::fs::read(&state_path).await {
        if let Ok(state) = serde_json::from_slice::<SyncState>(&bytes) {
            // The state also lists files from the previous pack version, so a
            // reset removes stale entries that the new manifest no longer has.
            targets.extend(state.files.into_keys());
        }
    }

    let mut report = ResetReport::default();
    for relative in targets {
        if !is_safe_relative(Path::new(&relative)) {
            tracing::warn!(%relative, "skipping unsafe path while resetting an instance");
            continue;
        }
        let path = instance_dir.join(path_buf(&relative));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => report.removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                report.failed += 1;
                tracing::warn!(%error, path = %path.display(), "could not remove an instance file");
            }
        }
    }

    // Removing the markers makes the next sync treat every file as new and
    // re-extract the overrides, which is what "download the pack again" means.
    for name in [
        STATE_FILE,
        OVERRIDES_STATE_FILE,
        pml_downloader::HASH_CACHE_FILE,
    ] {
        match tokio::fs::remove_file(instance_dir.join(name)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(%error, name, "could not remove an instance marker"),
        }
    }

    tracing::info!(
        removed = report.removed,
        failed = report.failed,
        "instance build files reset"
    );
    Ok(report)
}

/// Removes the cached manifest and `.mrpack` copy for one source URL, forcing
/// the next refresh to use the network instead of the last good response.
pub fn clear_source_cache(cache_dir: &Path, url: &str) -> std::io::Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        // `builtin://` and `file://` sources have no network cache.
        return Ok(());
    }
    let key = &hash_bytes(HashAlgo::Sha256, url.as_bytes())[..16];
    let files = [
        cache_dir.join("manifests").join(format!("{key}.json")),
        cache_dir.join("manifests").join(format!("{key}.json.tmp")),
        cache_dir.join("mrpack").join(format!("{key}.mrpack")),
        cache_dir.join("mrpack").join(format!("{key}.mrpack.part")),
        cache_dir
            .join("mrpack")
            .join(format!("{key}.mrpack.part.json")),
        cache_dir.join("mrpack").join(format!("{key}.json")),
    ];
    for path in files {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Files a pack update must never silently overwrite. Worlds and user configs
/// are copied before the delta sync; the caller decides when a backup is due
/// (normally only when the pack version changes).
const BACKUP_ENTRIES: [&str; 4] = ["saves", "config", "options.txt", "servers.dat"];

/// Copies worlds/configs into `backups_root/<source_id>/<unix>/` and keeps the
/// newest `keep` snapshots. Blocking; call it from `spawn_blocking`.
pub fn backup_instance(
    instance_dir: &Path,
    backups_root: &Path,
    source_id: &str,
    keep: usize,
) -> Result<Option<PathBuf>, ModpackError> {
    if !pml_config::is_valid_source_id(source_id) {
        return Err(ModpackError::Validation(format!(
            "небезопасный source_id: {source_id}"
        )));
    }
    let present = BACKUP_ENTRIES
        .iter()
        .filter(|name| instance_dir.join(name).exists())
        .collect::<Vec<_>>();
    if present.is_empty() {
        return Ok(None);
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let destination = backups_root.join(source_id).join(stamp.to_string());
    std::fs::create_dir_all(&destination).map_err(|source| ModpackError::Io {
        path: destination.clone(),
        source,
    })?;
    for name in present {
        let from = instance_dir.join(name);
        let to = destination.join(name);
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|source| ModpackError::Io {
                path: to.clone(),
                source,
            })?;
        }
    }

    rotate_backups(&destination, keep);
    tracing::info!(
        path = %destination.display(),
        "created an instance backup before the pack update"
    );
    Ok(Some(destination))
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), ModpackError> {
    std::fs::create_dir_all(destination).map_err(|error| ModpackError::Io {
        path: destination.to_path_buf(),
        source: error,
    })?;
    for entry in std::fs::read_dir(source).map_err(|error| ModpackError::Io {
        path: source.to_path_buf(),
        source: error,
    })? {
        let entry = entry.map_err(|error| ModpackError::Io {
            path: source.to_path_buf(),
            source: error,
        })?;
        let file_type = entry.file_type().map_err(|error| ModpackError::Io {
            path: entry.path(),
            source: error,
        })?;
        // Symlinks are never produced by the launcher and are skipped so a
        // malicious pack cannot make the backup read outside the instance.
        if file_type.is_symlink() {
            continue;
        }
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to).map_err(|error| ModpackError::Io {
                path: to.clone(),
                source: error,
            })?;
        }
    }
    Ok(())
}

fn rotate_backups(snapshot: &Path, keep: usize) {
    let Some(parent) = snapshot.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let mut snapshots = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    // Names are unix timestamps, so reverse lexicographic order is newest first.
    snapshots.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    for old in snapshots.into_iter().skip(keep.max(1)) {
        if let Err(error) = std::fs::remove_dir_all(&old) {
            tracing::warn!(%error, path = %old.display(), "could not prune an old backup");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> Manifest {
        Manifest {
            schema_version: 1,
            name: "Test pack".to_owned(),
            version: "1.0.0".to_owned(),
            mc_version: "1.21.1".to_owned(),
            loader: LoaderSpec {
                kind: pml_config::LoaderKind::Fabric,
                version: "0.16.5".to_owned(),
            },
            server: Some(ServerSpec {
                ip: "45.85.60.39".to_owned(),
                port: 25592,
            }),
            files: vec![ManifestFile {
                path: "mods/example.jar".to_owned(),
                url: "https://example.com/example.jar".to_owned(),
                sha256: "a".repeat(64),
                sha512: String::new(),
                sha1: String::new(),
                size: 123,
            }],
            optional_files: vec![ManifestFile {
                path: "mods/optional.jar".to_owned(),
                url: "https://example.com/optional.jar".to_owned(),
                sha256: "b".repeat(64),
                sha512: String::new(),
                sha1: String::new(),
                size: 456,
            }],
        }
    }

    #[test]
    fn manifest_roundtrip() {
        let manifest = sample_manifest();
        manifest.validate().expect("valid");
        let json = manifest.to_json_pretty().expect("serialize");
        let parsed = Manifest::from_json(json.as_bytes()).expect("parse");
        assert_eq!(parsed, manifest);
        assert!(json.contains("\"optional_files\""));
    }

    #[test]
    fn builtin_manifest_is_valid() {
        let manifest = Manifest::from_json(BUILTIN_VANILLA.as_bytes()).expect("builtin");
        assert_eq!(manifest.mc_version, "1.21.1");
        assert_eq!(
            manifest.server.as_ref().map(ServerSpec::display).as_deref(),
            Some("45.85.60.39:25592")
        );
        assert_eq!(manifest.loader.kind, pml_config::LoaderKind::Fabric);
    }

    #[test]
    fn backup_instance_copies_worlds_and_prunes_old_snapshots() {
        let directory = tempfile::tempdir().expect("tempdir");
        let instance = directory.path().join("instance");
        std::fs::create_dir_all(instance.join("saves").join("world")).expect("saves");
        std::fs::write(
            instance.join("saves").join("world").join("level.dat"),
            b"world",
        )
        .expect("level");
        std::fs::write(instance.join("options.txt"), b"fov:90").expect("options");
        let backups = directory.path().join("backups");

        let snapshot = backup_instance(&instance, &backups, "pack", 2)
            .expect("backup")
            .expect("some backup");
        assert!(snapshot
            .join("saves")
            .join("world")
            .join("level.dat")
            .is_file());
        assert_eq!(
            std::fs::read(snapshot.join("options.txt")).expect("options"),
            b"fov:90"
        );

        // A second call with a distinct timestamp keeps both snapshots.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        backup_instance(&instance, &backups, "pack", 2)
            .expect("backup")
            .expect("some backup");
        let count = std::fs::read_dir(backups.join("pack"))
            .expect("list")
            .count();
        assert_eq!(count, 2);

        // A third call prunes the oldest one, never the newest.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        backup_instance(&instance, &backups, "pack", 2)
            .expect("backup")
            .expect("some backup");
        let count = std::fs::read_dir(backups.join("pack"))
            .expect("list")
            .count();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn reset_removes_pack_files_but_keeps_worlds() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let instance = directory.path().join("instance");
        std::fs::create_dir_all(instance.join("mods"))?;
        std::fs::create_dir_all(instance.join("saves").join("world"))?;
        std::fs::write(instance.join("mods/old.jar"), b"old")?;
        std::fs::write(instance.join("mods/managed.jar"), b"managed")?;
        std::fs::write(instance.join("saves/world/level.dat"), b"world")?;
        std::fs::write(instance.join("options.txt"), b"fov:90")?;

        let state = SyncState {
            schema_version: MANIFEST_SCHEMA_VERSION,
            pack_version: "1.0.0".to_owned(),
            files: BTreeMap::from([("mods/old.jar".to_owned(), "a".repeat(64))]),
        };
        std::fs::write(instance.join(STATE_FILE), serde_json::to_vec(&state)?)?;
        std::fs::write(instance.join(OVERRIDES_STATE_FILE), b"{}")?;
        std::fs::write(instance.join(pml_downloader::HASH_CACHE_FILE), b"{}")?;

        let report = reset_instance(&instance, &["mods/managed.jar".to_owned()]).await?;
        assert_eq!(report.removed, 2);
        assert_eq!(report.failed, 0);
        assert!(!instance.join("mods/old.jar").exists());
        assert!(!instance.join("mods/managed.jar").exists());
        // A reset must never cost the player their world or settings.
        assert!(instance.join("saves/world/level.dat").is_file());
        assert!(instance.join("options.txt").is_file());
        assert!(!instance.join(STATE_FILE).exists());
        assert!(!instance.join(OVERRIDES_STATE_FILE).exists());
        assert!(!instance.join(pml_downloader::HASH_CACHE_FILE).exists());
        Ok(())
    }

    #[test]
    fn clear_source_cache_removes_only_that_source() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let meta = directory.path();
        let url = "https://example.com/pack.mrpack";
        let other = "https://example.com/other.mrpack";
        let key = &hash_bytes(HashAlgo::Sha256, url.as_bytes())[..16];
        let other_key = &hash_bytes(HashAlgo::Sha256, other.as_bytes())[..16];
        std::fs::create_dir_all(meta.join("manifests"))?;
        std::fs::create_dir_all(meta.join("mrpack"))?;
        std::fs::write(meta.join("manifests").join(format!("{key}.json")), b"{}")?;
        std::fs::write(meta.join("mrpack").join(format!("{key}.mrpack")), b"zip")?;
        std::fs::write(
            meta.join("manifests").join(format!("{other_key}.json")),
            b"{}",
        )?;

        clear_source_cache(meta, url)?;
        assert!(!meta.join("manifests").join(format!("{key}.json")).exists());
        assert!(!meta.join("mrpack").join(format!("{key}.mrpack")).exists());
        assert!(meta
            .join("manifests")
            .join(format!("{other_key}.json"))
            .is_file());
        Ok(())
    }

    #[test]
    fn rejects_zip_slip_and_bad_hash() {
        let mut manifest = sample_manifest();
        manifest.files[0].path = "../evil.jar".to_owned();
        assert!(matches!(
            manifest.validate(),
            Err(ModpackError::UnsafePath(_))
        ));

        let mut manifest = sample_manifest();
        manifest.files[0].sha256 = "xyz".to_owned();
        assert!(matches!(
            manifest.validate(),
            Err(ModpackError::Validation(_))
        ));
    }

    #[test]
    fn rejects_duplicate_paths() {
        let mut manifest = sample_manifest();
        manifest.optional_files[0].path = "mods/example.jar".to_owned();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn rejects_unsafe_version_labels() {
        for bad in [
            "../../evil",
            "1.21/evil",
            "1.21\\evil",
            "1.21 x64",
            "версия",
        ] {
            let mut manifest = sample_manifest();
            manifest.mc_version = bad.to_owned();
            assert!(manifest.validate().is_err(), "mc_version {bad:?}");

            let mut manifest = sample_manifest();
            manifest.loader.version = bad.to_owned();
            assert!(manifest.validate().is_err(), "loader version {bad:?}");
        }
    }

    #[test]
    fn optional_files_respect_overrides() {
        let manifest = sample_manifest();
        let mut overrides = BTreeMap::new();
        assert_eq!(manifest.enabled_files(&overrides).len(), 2);
        overrides.insert("mods/optional.jar".to_owned(), false);
        let enabled = manifest.enabled_files(&overrides);
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].path, "mods/example.jar");
    }

    #[test]
    fn normalizes_windows_paths() {
        assert_eq!(normalize_path(r"mods\sub\file.jar"), "mods/sub/file.jar");
        assert_eq!(normalize_path("./mods//a.jar"), "mods/a.jar");
        // `..` is preserved and rejected by validate(), never silently resolved.
        assert_eq!(normalize_path("../evil.jar"), "../evil.jar");
    }

    #[tokio::test]
    async fn manifest_network_failure_falls_back_to_cache() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        // Bind a port, then drop the listener: the connection is refused with
        // no resolver involved, which must degrade to the last good manifest.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        drop(listener);

        let url = format!("http://{address}/manifest.json");
        let cache_dir = directory.path().join("meta");
        let cache = cache_dir.join("manifests").join(format!(
            "{}.json",
            &hash_bytes(HashAlgo::Sha256, url.as_bytes())[..16]
        ));
        tokio::fs::create_dir_all(cache.parent().expect("cache path has a parent")).await?;
        tokio::fs::write(&cache, sample_manifest().to_json_pretty()?).await?;

        let client = reqwest::Client::builder().no_proxy().build()?;
        let manifest = load_manifest(&client, &url, &cache_dir).await?;
        assert_eq!(manifest.mc_version, sample_manifest().mc_version);
        assert_eq!(manifest.loader.version, sample_manifest().loader.version);
        Ok(())
    }

    fn local_file(path: &str, source: &Path) -> ManifestFile {
        let bytes = std::fs::read(source).expect("read fixture");
        ManifestFile {
            path: path.to_owned(),
            url: reqwest::Url::from_file_path(source)
                .expect("file url")
                .to_string(),
            sha256: hash_bytes(HashAlgo::Sha256, &bytes),
            sha512: String::new(),
            sha1: String::new(),
            size: bytes.len() as u64,
        }
    }

    #[tokio::test]
    async fn sync_is_a_delta_and_honours_optional_files() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let paths = PortablePaths::from_root(temp.path().join("launcher_data"))?;
        let source_dir = temp.path().join("source");
        std::fs::create_dir_all(source_dir.join("mods"))?;
        let fixtures: [(&str, &[u8]); 4] = [
            ("a.jar", b"alpha"),
            ("b.jar", b"beta"),
            ("opt.jar", b"opt"),
            ("c.jar", b"gamma"),
        ];
        for (name, body) in fixtures {
            std::fs::write(source_dir.join("mods").join(name), body)?;
        }

        let mut manifest = sample_manifest();
        manifest.files = vec![
            local_file("mods/a.jar", &source_dir.join("mods").join("a.jar")),
            local_file("mods/b.jar", &source_dir.join("mods").join("b.jar")),
        ];
        manifest.optional_files = vec![local_file(
            "mods/opt.jar",
            &source_dir.join("mods").join("opt.jar"),
        )];
        let instance = temp.path().join("instance");
        let sync = ModpackSync::new(paths, Downloader::new(2)?);
        let mut overrides = BTreeMap::from([("mods/opt.jar".to_owned(), false)]);

        let first = sync
            .sync(SyncRequest {
                manifest: &manifest,
                instance_dir: &instance,
                overrides: &overrides,
                progress: None,
            })
            .await?;
        assert_eq!(first.downloaded, 2);
        assert_eq!(first.optional_enabled, 0);
        assert!(instance.join("mods/a.jar").is_file());
        assert!(instance.join("mods/b.jar").is_file());
        assert!(!instance.join("mods/opt.jar").exists());

        let second = sync
            .sync(SyncRequest {
                manifest: &manifest,
                instance_dir: &instance,
                overrides: &overrides,
                progress: None,
            })
            .await?;
        assert_eq!(second.skipped, 2);
        assert_eq!(second.downloaded, 0);

        manifest.files = vec![
            local_file("mods/a.jar", &source_dir.join("mods").join("a.jar")),
            local_file("mods/c.jar", &source_dir.join("mods").join("c.jar")),
        ];
        overrides.insert("mods/opt.jar".to_owned(), true);
        let third = sync
            .sync(SyncRequest {
                manifest: &manifest,
                instance_dir: &instance,
                overrides: &overrides,
                progress: None,
            })
            .await?;
        assert_eq!(third.downloaded, 2);
        assert_eq!(third.skipped, 1);
        assert_eq!(third.removed, 1);
        assert_eq!(third.optional_enabled, 1);
        assert!(!instance.join("mods/b.jar").exists());
        assert!(instance.join("mods/c.jar").is_file());
        assert!(instance.join("mods/opt.jar").is_file());
        Ok(())
    }
}
