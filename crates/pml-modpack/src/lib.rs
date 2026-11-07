//! Modpack manifests and instance synchronisation.
//!
//! A manifest is a small JSON document that pins the Minecraft version, the
//! loader, the server and every managed file with a SHA-256 digest. Synchronising
//! an instance is a delta operation: files whose digest matches the last synced
//! state are not hashed or downloaded again, removed entries are deleted, and
//! optional files honour the per-source overrides stored in `config.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use pml_config::{save_json_atomic, LoaderSpec, PortablePaths, ServerSpec};
use pml_downloader::{
    hash_bytes, is_safe_relative, safe_extract_zip, DownloadEvent, DownloadTask, Downloader,
    HashAlgo,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

mod mrpack;

pub use mrpack::{extract_overrides, read_index, MrpackArchive, MrpackIndex};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const STATE_FILE: &str = ".pml_manifest_state.json";

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

    let bytes = match response.bytes().await {
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
    #[serde(default)]
    files: BTreeMap<String, String>,
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
        let optional_enabled =
            enabled
                .iter()
                .filter(|file| {
                    request.manifest.optional_files.iter().any(|optional| {
                        normalize_path(&optional.path) == normalize_path(&file.path)
                    })
                })
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
                    Ok(()) => report.removed += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::warn!(%error, path = %path.display(), "could not remove stale file");
                    }
                }
            }
            state.files.remove(&relative);
        }

        state.schema_version = MANIFEST_SCHEMA_VERSION;
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

    /// Installs a zip-based modpack into an instance. Kept separate from the
    /// manifest flow because zips are not delta-tracked by SHA-256 entries.
    pub async fn install_zip(
        &self,
        zip_path: &Path,
        instance_dir: &Path,
    ) -> Result<Vec<PathBuf>, ModpackError> {
        let zip_path = zip_path.to_path_buf();
        let instance_dir = instance_dir.to_path_buf();
        tokio::fs::create_dir_all(&instance_dir)
            .await
            .map_err(|source| ModpackError::Io {
                path: instance_dir.clone(),
                source,
            })?;
        let extracted =
            tokio::task::spawn_blocking(move || safe_extract_zip(&zip_path, &instance_dir))
                .await
                .map_err(|error| ModpackError::Validation(format!("extract task: {error}")))??;
        Ok(extracted)
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
}
