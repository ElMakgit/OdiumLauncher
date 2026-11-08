//! Modrinth `.mrpack` support.
//!
//! An `.mrpack` is a zip archive with `modrinth.index.json` at the root. The
//! index lists the mod files (with SHA-512/SHA-1 digests and Modrinth CDN URLs)
//! and the loader dependencies; `overrides/` and `client-overrides/` contain
//! configs and jars that travel with the pack.
//!
//! The launcher converts the index into its own [`Manifest`] model, so the
//! delta sync, optional-mod handling and loader provisioning are shared with
//! hand-written manifests.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use pml_config::{save_json_atomic, LoaderKind, LoaderSpec};
use pml_downloader::{hash_bytes, is_safe_relative, CancelToken, HashAlgo};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;

use crate::{Manifest, ManifestFile, ModpackError, MANIFEST_SCHEMA_VERSION};

pub const INDEX_FILE: &str = "modrinth.index.json";
const OVERRIDE_PREFIXES: [&str; 2] = ["overrides/", "client-overrides/"];
/// Hard caps for `overrides/` extraction. A pack that ships more than this is
/// either corrupt or hostile; refusing early beats filling the disk.
pub const MAX_OVERRIDE_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_OVERRIDE_FILES: usize = 20_000;
/// Instance file that remembers which overrides snapshot was already applied.
pub const OVERRIDES_STATE_FILE: &str = ".pml_overrides_state.json";

#[derive(Debug, Clone, Deserialize)]
pub struct MrpackIndex {
    #[serde(default, rename = "formatVersion")]
    pub format_version: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "versionId")]
    pub version_id: String,
    #[serde(default)]
    pub files: Vec<MrpackFile>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MrpackFile {
    pub path: String,
    #[serde(default, alias = "size", rename = "fileSize")]
    pub file_size: u64,
    #[serde(default)]
    pub hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub downloads: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct MrpackArchive {
    pub archive: PathBuf,
    pub index: MrpackIndex,
}

impl MrpackIndex {
    pub fn parse(bytes: &[u8]) -> Result<Self, ModpackError> {
        let index: Self = serde_json::from_slice(bytes).map_err(ModpackError::ManifestJson)?;
        index.check_format()?;
        Ok(index)
    }

    fn check_format(&self) -> Result<(), ModpackError> {
        if self.format_version != 1 {
            return Err(ModpackError::Validation(format!(
                "неподдерживаемая версия .mrpack: {}",
                self.format_version
            )));
        }
        Ok(())
    }

    pub fn mc_version(&self) -> Result<&str, ModpackError> {
        self.dependencies
            .get("minecraft")
            .map(String::as_str)
            .ok_or_else(|| {
                ModpackError::Validation("в .mrpack нет зависимости minecraft".to_owned())
            })
    }

    pub fn loader(&self) -> Result<LoaderSpec, ModpackError> {
        const CANDIDATES: [(&str, LoaderKind); 4] = [
            ("neoforge", LoaderKind::NeoForge),
            ("forge", LoaderKind::Forge),
            ("fabric-loader", LoaderKind::Fabric),
            ("quilt-loader", LoaderKind::Quilt),
        ];
        for (key, kind) in CANDIDATES {
            if let Some(version) = self.dependencies.get(key) {
                return Ok(LoaderSpec {
                    kind,
                    version: version.clone(),
                });
            }
        }
        Err(ModpackError::Validation(
            "в .mrpack не указан загрузчик (neoforge, forge, fabric или quilt)".to_owned(),
        ))
    }

    /// Converts the Modrinth index into the launcher's manifest model.
    pub fn to_manifest(&self) -> Result<Manifest, ModpackError> {
        self.check_format()?;
        let mc_version = self.mc_version()?.to_owned();
        let loader = self.loader()?;
        let mut files = Vec::new();
        let mut optional_files = Vec::new();
        for file in &self.files {
            // Server-side env also gates entry: server-only files must never
            // end up in a client instance.
            let client_env = file.env.get("client").map(String::as_str);
            if client_env == Some("unsupported") {
                continue;
            }
            let url = file.downloads.first().ok_or_else(|| {
                ModpackError::Validation(format!("у файла {} нет ссылки на скачивание", file.path))
            })?;
            let entry = ManifestFile {
                path: file.path.clone(),
                url: url.clone(),
                sha256: file.hashes.get("sha256").cloned().unwrap_or_default(),
                sha512: file.hashes.get("sha512").cloned().unwrap_or_default(),
                sha1: file.hashes.get("sha1").cloned().unwrap_or_default(),
                size: file.file_size,
            };
            // `env: {"client": "optional"}` is Modrinth's contract for "install
            // on the client, but let the user turn it off". Placing those into
            // `optional_files` routes them through the delta sync and honours
            // the UI switch instead of force-installing them every launch.
            match client_env {
                Some("optional") => optional_files.push(entry),
                _ => files.push(entry),
            }
        }
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            name: if self.name.trim().is_empty() {
                "Modrinth pack".to_owned()
            } else {
                self.name.clone()
            },
            version: self.version_id.clone(),
            mc_version,
            loader,
            server: None,
            files,
            optional_files,
        };
        manifest.validate()?;
        Ok(manifest)
    }
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct CacheMeta {
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    last_modified: Option<String>,
}

/// Downloads (or reuses a cached copy of) an `.mrpack` and parses its index.
///
/// HTTP requests are conditional: when the server reports `304 Not Modified`
/// the cached archive is reused, so an unchanged pack costs one small request
/// instead of re-downloading the whole zip on every launch.
pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
    cache_dir: &Path,
) -> Result<MrpackArchive, ModpackError> {
    if let Some(local) = url.strip_prefix("file://") {
        let archive = crate::local_path(local);
        let index = read_index(&archive)?;
        return Ok(MrpackArchive { archive, index });
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(ModpackError::Validation(format!(
            "неподдерживаемый URL .mrpack: {url}"
        )));
    }

    let cache_root = cache_dir.join("mrpack");
    tokio::fs::create_dir_all(&cache_root)
        .await
        .map_err(|source| ModpackError::Io {
            path: cache_root.clone(),
            source,
        })?;
    let key = hash_bytes(HashAlgo::Sha256, url.as_bytes())[..16].to_owned();
    let archive = cache_root.join(format!("{key}.mrpack"));
    let meta_path = cache_root.join(format!("{key}.json"));
    let meta: Option<CacheMeta> = match tokio::fs::read(&meta_path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).ok(),
        Err(_) => None,
    };

    let mut request = client.get(url);
    if let Some(meta) = &meta {
        if let Some(etag) = &meta.etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &meta.last_modified {
            request = request.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
        }
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(network_error) => {
            // An offline launch must keep working when the archive is already
            // cached: this is the same contract the JSON manifest path has.
            if archive.is_file() {
                match read_index(&archive) {
                    Ok(index) => {
                        tracing::warn!(
                            %network_error,
                            %url,
                            "mrpack request failed, using cached archive"
                        );
                        return Ok(MrpackArchive { archive, index });
                    }
                    Err(cache_error) => tracing::warn!(
                        %cache_error,
                        path = %archive.display(),
                        "cached .mrpack is unusable"
                    ),
                }
            }
            return Err(network_error.into());
        }
    };
    let response = if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        // Revalidation only works when the cached archive is present and
        // readable. Otherwise the conditional request would keep returning 304
        // forever and the broken cache would poison every launch.
        if archive.is_file() {
            match read_index(&archive) {
                Ok(index) => return Ok(MrpackArchive { archive, index }),
                Err(error) => tracing::warn!(
                    %error,
                    path = %archive.display(),
                    "cached .mrpack is broken; re-downloading"
                ),
            }
        } else {
            tracing::warn!(%url, "server answered 304 but the cached .mrpack is missing; re-downloading");
        }
        let _ = tokio::fs::remove_file(&meta_path).await;
        let retry = client.get(url).send().await?;
        if retry.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Err(ModpackError::Validation(
                "сервер вернул 304, но кэш .mrpack отсутствует".to_owned(),
            ));
        }
        retry
    } else {
        response
    };

    let response = match response.error_for_status() {
        Ok(response) => response,
        Err(error) => {
            // A 404/503 from the CDN must not brick a launch that still has a
            // usable cached archive.
            if archive.is_file() {
                match read_index(&archive) {
                    Ok(index) => {
                        tracing::warn!(
                            %error,
                            %url,
                            "mrpack download failed, using cached archive"
                        );
                        return Ok(MrpackArchive { archive, index });
                    }
                    Err(cache_error) => tracing::warn!(
                        %cache_error,
                        path = %archive.display(),
                        "cached .mrpack is unusable"
                    ),
                }
            }
            return Err(error.into());
        }
    };
    let etag = header_value(&response, reqwest::header::ETAG);
    let last_modified = header_value(&response, reqwest::header::LAST_MODIFIED);
    let part = archive.with_extension("mrpack.part");
    if let Err(error) = stream_response_to_file(response, &part).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(error);
    }
    tokio::fs::rename(&part, &archive)
        .await
        .map_err(|source| ModpackError::Io {
            path: archive.clone(),
            source,
        })?;
    let meta = CacheMeta {
        etag,
        last_modified,
    };
    if let Ok(json) = serde_json::to_vec(&meta) {
        let _ = tokio::fs::write(&meta_path, json).await;
    }

    let index = read_index(&archive)?;
    Ok(MrpackArchive { archive, index })
}

fn header_value(response: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Streams the response body to `dest` chunk by chunk. A 500 MiB pack must not
/// be buffered in RAM just to be written to disk; the caller renames the
/// finished `.part` file into place.
async fn stream_response_to_file(
    response: reqwest::Response,
    dest: &Path,
) -> Result<(), ModpackError> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| ModpackError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|source| ModpackError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk)
            .await
            .map_err(|source| ModpackError::Io {
                path: dest.to_path_buf(),
                source,
            })?;
    }
    file.flush().await.map_err(|source| ModpackError::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    file.sync_all().await.map_err(|source| ModpackError::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    Ok(())
}

pub fn read_index(path: &Path) -> Result<MrpackIndex, ModpackError> {
    let file = std::fs::File::open(path).map_err(|source| ModpackError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| {
        ModpackError::Validation(format!("не удалось открыть .mrpack: {error}"))
    })?;
    let mut entry = zip.by_name(INDEX_FILE).map_err(|_| {
        ModpackError::Validation(format!("в архиве нет {INDEX_FILE}: {}", path.display()))
    })?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|source| ModpackError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    MrpackIndex::parse(&bytes)
}

/// Extracts `overrides/` and `client-overrides/` into the instance directory.
/// Paths are validated so a malicious archive cannot escape the instance.
pub fn extract_overrides(archive: &Path, instance_dir: &Path) -> Result<usize, ModpackError> {
    extract_overrides_with_limits(
        archive,
        instance_dir,
        MAX_OVERRIDE_BYTES,
        MAX_OVERRIDE_FILES,
        &BTreeSet::new(),
        None,
    )
}

/// Same as [`extract_overrides`] with explicit caps, which also makes the
/// limits testable without building a gigabyte archive. `managed` lists the
/// normalized manifest paths owned by the delta sync: such entries are skipped
/// because the sync already hash-verifies them, and blindly re-extracting
/// would resurrect disabled optional mods on every launch.
pub fn extract_overrides_with_limits(
    archive: &Path,
    instance_dir: &Path,
    max_bytes: u64,
    max_files: usize,
    managed: &BTreeSet<String>,
    cancel: Option<&CancelToken>,
) -> Result<usize, ModpackError> {
    std::fs::create_dir_all(instance_dir).map_err(|source| ModpackError::Io {
        path: instance_dir.to_path_buf(),
        source,
    })?;
    let instance_canonical = instance_dir
        .canonicalize()
        .map_err(|source| ModpackError::Io {
            path: instance_dir.to_path_buf(),
            source,
        })?;
    let file = std::fs::File::open(archive).map_err(|source| ModpackError::Io {
        path: archive.to_path_buf(),
        source,
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| {
        ModpackError::Validation(format!("не удалось открыть .mrpack: {error}"))
    })?;
    let mut written = 0;
    let mut total_bytes: u64 = 0;
    for index in 0..zip.len() {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(ModpackError::Cancelled);
        }
        let mut entry = zip.by_index(index).map_err(|error| {
            ModpackError::Validation(format!("не удалось прочитать файл архива: {error}"))
        })?;
        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                return Err(ModpackError::UnsafePath(entry.name().to_owned()));
            }
        }
        let Some(enclosed) = entry.enclosed_name() else {
            continue;
        };
        let name = enclosed.to_string_lossy().replace('\\', "/");
        let Some(relative) = OVERRIDE_PREFIXES
            .iter()
            .find_map(|prefix| name.strip_prefix(prefix))
        else {
            continue;
        };
        if relative.is_empty() || relative.ends_with('/') || entry.is_dir() {
            continue;
        }
        if !is_safe_relative(Path::new(relative)) {
            return Err(ModpackError::UnsafePath(relative.to_owned()));
        }
        if managed.contains(&crate::normalize_path(relative)) {
            continue;
        }
        if written >= max_files {
            return Err(ModpackError::Validation(format!(
                "в overrides больше {max_files} файлов"
            )));
        }
        total_bytes = total_bytes.saturating_add(entry.size());
        if total_bytes > max_bytes {
            return Err(ModpackError::Validation(format!(
                "overrides распаковываются более чем в {} МиБ",
                max_bytes / (1024 * 1024)
            )));
        }
        let destination = instance_dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ModpackError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            let parent_canonical = parent.canonicalize().map_err(|source| ModpackError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            if !parent_canonical.starts_with(&instance_canonical) {
                return Err(ModpackError::UnsafePath(relative.to_owned()));
            }
        }
        let mut output =
            std::fs::File::create(&destination).map_err(|source| ModpackError::Io {
                path: destination.clone(),
                source,
            })?;
        std::io::copy(&mut entry, &mut output).map_err(|source| ModpackError::Io {
            path: destination.clone(),
            source,
        })?;
        output.flush().map_err(|source| ModpackError::Io {
            source,
            path: destination,
        })?;
        written += 1;
    }
    Ok(written)
}

/// Which overrides snapshot the instance currently holds.
#[derive(Debug, Serialize, Deserialize)]
struct OverridesState {
    #[serde(default = "crate::default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    archive_identity: String,
}

/// Cheap but change-sensitive fingerprint of an archive: size plus the
/// last-modified timestamp in milliseconds. The launcher writes archives
/// atomically, so any pack update moves this fingerprint.
pub fn archive_identity(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let stamp = metadata
        .modified()
        .ok()
        .and_then(|since| since.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|delta| delta.as_millis() as u64)
        .unwrap_or(0);
    Some(format!("{}-{stamp}", metadata.len()))
}

/// Applies overrides only when the archive differs from the last applied
/// snapshot; `Ok(None)` means overrides are already up to date.
///
/// Re-extracting on every launch (the old behaviour) returned user-edited
/// `config/*`, keybinds and shader settings back to the shipped defaults. With
/// the marker, overrides behave like in the Modrinth app: extracted at install
/// and at pack updates, never simply "on each run".
pub async fn extract_overrides_when_changed(
    archive: &Path,
    instance_dir: &Path,
    managed: &BTreeSet<String>,
    cancel: Option<CancelToken>,
) -> Result<Option<usize>, ModpackError> {
    let identity = archive_identity(archive);
    let state_path = instance_dir.join(OVERRIDES_STATE_FILE);
    if let Some(id) = &identity {
        let state = tokio::fs::read(&state_path)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<OverridesState>(&bytes).ok());
        if state
            .as_ref()
            .is_some_and(|state| &state.archive_identity == id)
        {
            return Ok(None);
        }
    }

    let archive = archive.to_path_buf();
    let instance_dir = instance_dir.to_path_buf();
    let managed = managed.to_owned();
    let written = tokio::task::spawn_blocking(move || {
        extract_overrides_with_limits(
            &archive,
            &instance_dir,
            MAX_OVERRIDE_BYTES,
            MAX_OVERRIDE_FILES,
            &managed,
            cancel.as_ref(),
        )
    })
    .await
    .map_err(|error| ModpackError::Validation(format!("extract task failed: {error}")))??;

    if let Some(id) = identity {
        let state = OverridesState {
            schema_version: MANIFEST_SCHEMA_VERSION,
            archive_identity: id,
        };
        // A failed marker write is not fatal: the next run simply re-applies.
        let _ = save_json_atomic(&state_path, &state);
    }
    Ok(Some(written))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> MrpackIndex {
        MrpackIndex {
            format_version: 1,
            name: "Test pack".to_owned(),
            version_id: "1.2.3".to_owned(),
            files: vec![
                MrpackFile {
                    path: "mods/example.jar".to_owned(),
                    file_size: 123,
                    hashes: BTreeMap::from([("sha512".to_owned(), "a".repeat(128))]),
                    downloads: vec!["https://cdn.modrinth.com/example.jar".to_owned()],
                    env: BTreeMap::new(),
                },
                MrpackFile {
                    path: "mods/server-only.jar".to_owned(),
                    file_size: 10,
                    hashes: BTreeMap::from([("sha1".to_owned(), "b".repeat(40))]),
                    downloads: vec!["https://cdn.modrinth.com/server-only.jar".to_owned()],
                    env: BTreeMap::from([("client".to_owned(), "unsupported".to_owned())]),
                },
            ],
            dependencies: BTreeMap::from([
                ("minecraft".to_owned(), "1.21.1".to_owned()),
                ("neoforge".to_owned(), "21.1.248".to_owned()),
            ]),
        }
    }

    #[test]
    fn index_becomes_manifest() {
        let manifest = sample_index().to_manifest().expect("valid");
        assert_eq!(manifest.mc_version, "1.21.1");
        assert_eq!(manifest.loader.kind, LoaderKind::NeoForge);
        assert_eq!(manifest.loader.version, "21.1.248");
        assert_eq!(manifest.files.len(), 1, "server-only file must be skipped");
        assert_eq!(manifest.files[0].path, "mods/example.jar");
        assert!(manifest.files[0].sha512.starts_with('a'));
    }

    #[tokio::test]
    async fn conditional_fetch_reuses_the_cached_archive_on_304(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt as _};

        let directory = tempfile::tempdir()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let url = format!("http://{address}/pack.mrpack");

        let mut zip_body = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut zip_body);
            let mut writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file(INDEX_FILE, options)?;
            std::io::Write::write_all(
                &mut writer,
                br#"{"formatVersion":1,"name":"Test pack","versionId":"1.2.3","files":[],"dependencies":{"minecraft":"1.21.1","neoforge":"21.1.248"}}"#,
            )?;
            writer.finish()?;
        }

        // First request gets the archive plus an ETag, the conditional re-check
        // answers 304 with no body at all.
        let body_for_server = zip_body;
        let server = tokio::spawn(async move {
            for first_round in [true, false] {
                let (mut socket, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let mut request = [0u8; 8192];
                let _ = socket.read(&mut request).await;
                if first_round {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nETag: \"pack-v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body_for_server.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body_for_server).await;
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
                        .await;
                }
            }
        });

        let client = reqwest::Client::builder().no_proxy().build()?;
        let key = hash_bytes(HashAlgo::Sha256, url.as_bytes())[..16].to_owned();
        let cache_dir = directory.path().join("cache");
        let archive = cache_dir.join("mrpack").join(format!("{key}.mrpack"));

        let first = fetch(&client, &url, &cache_dir).await?;
        assert_eq!(first.index.version_id, "1.2.3");
        assert!(archive.is_file(), "first fetch downloads the archive");
        let stamp = std::fs::metadata(&archive)?.modified()?;

        let second = fetch(&client, &url, &cache_dir).await?;
        assert_eq!(second.index.version_id, "1.2.3");
        let stamp_after = std::fs::metadata(&archive)?.modified()?;
        assert_eq!(
            stamp, stamp_after,
            "a 304 answer must reuse the cached archive without rewriting it"
        );

        server.await?;
        Ok(())
    }

    #[test]
    fn unsupported_format_is_rejected() {
        let mut index = sample_index();
        index.format_version = 2;
        assert!(index.to_manifest().is_err());
    }

    #[test]
    fn missing_loader_is_rejected() {
        let mut index = sample_index();
        index.dependencies.remove("neoforge");
        assert!(index.loader().is_err());
    }

    #[test]
    fn extract_overrides_rejects_zip_slip() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let archive = directory.path().join("evil.mrpack");
        {
            let file = std::fs::File::create(&archive)?;
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("overrides/../../evil.txt", options)?;
            std::io::Write::write_all(&mut writer, b"pwned")?;
            writer.start_file("overrides/config/ok.toml", options)?;
            std::io::Write::write_all(&mut writer, b"ok")?;
            writer.finish()?;
        }

        let instance = directory.path().join("instance");
        let written = extract_overrides(&archive, &instance)?;
        assert_eq!(written, 1, "only the safe entry is extracted");
        assert!(!directory.path().join("evil.txt").exists());
        assert!(instance.join("config").join("ok.toml").is_file());
        Ok(())
    }

    #[test]
    fn extract_overrides_rejects_symlinks() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let archive = directory.path().join("symlink.mrpack");
        {
            let file = std::fs::File::create(&archive)?;
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            // `add_symlink` writes the full `S_IFLNK` mode; `unix_permissions`
            // would mask the file-type bits away.
            writer.add_symlink("overrides/link", "/etc/passwd", options)?;
            writer.finish()?;
        }

        let instance = directory.path().join("instance");
        let error = extract_overrides(&archive, &instance).expect_err("symlink must be rejected");
        assert!(matches!(error, ModpackError::UnsafePath(_)));
        assert!(!instance.join("link").exists());
        Ok(())
    }

    #[test]
    fn extract_overrides_enforces_limits() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let archive = directory.path().join("big.mrpack");
        {
            let file = std::fs::File::create(&archive)?;
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            for name in ["overrides/a.bin", "overrides/b.bin"] {
                writer.start_file(name, options)?;
                std::io::Write::write_all(&mut writer, &[0u8; 10])?;
            }
            writer.finish()?;
        }

        let instance = directory.path().join("instance");
        let error =
            extract_overrides_with_limits(&archive, &instance, 15, 100, &BTreeSet::new(), None)
                .expect_err("size limit must be enforced");
        assert!(matches!(error, ModpackError::Validation(_)));
        let error =
            extract_overrides_with_limits(&archive, &instance, 1024, 1, &BTreeSet::new(), None)
                .expect_err("file limit must be enforced");
        assert!(matches!(error, ModpackError::Validation(_)));
        Ok(())
    }

    fn write_pack(archive: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(archive).expect("zip file");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (name, body) in entries {
            writer.start_file(*name, options).expect("zip entry");
            std::io::Write::write_all(&mut writer, body).expect("zip body");
        }
        writer.finish().expect("zip finish");
    }

    #[test]
    fn env_optional_becomes_optional_files() {
        let mut index = sample_index();
        index.files.push(MrpackFile {
            path: "mods/optional.jar".to_owned(),
            file_size: 20,
            hashes: BTreeMap::from([("sha1".to_owned(), "c".repeat(40))]),
            downloads: vec!["https://cdn.modrinth.com/optional.jar".to_owned()],
            env: BTreeMap::from([("client".to_owned(), "optional".to_owned())]),
        });
        let manifest = index.to_manifest().expect("valid");
        assert_eq!(manifest.files.len(), 1, "required file stays required");
        assert_eq!(manifest.optional_files.len(), 1, "optional entry moves out");
        assert_eq!(manifest.optional_files[0].path, "mods/optional.jar");

        // The UI switch can now turn the optional entry off like any other.
        let overrides = BTreeMap::from([("mods/optional.jar".to_owned(), false)]);
        let enabled = manifest.enabled_files(&overrides);
        assert_eq!(enabled.len(), 1);
    }

    #[test]
    fn version_labels_reject_path_traversal() {
        let mut index = sample_index();
        index
            .dependencies
            .insert("minecraft".to_owned(), "../../evil".to_owned());
        assert!(index.to_manifest().is_err());

        let mut index = sample_index();
        index
            .dependencies
            .insert("neoforge".to_owned(), "21.1.248/..".to_owned());
        assert!(index.to_manifest().is_err());
    }

    #[tokio::test]
    async fn overrides_are_reapplied_only_when_the_archive_changes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let archive = directory.path().join("pack.mrpack");
        write_pack(&archive, &[("overrides/config/runtime.toml", b"shipped")]);
        let instance = directory.path().join("instance");

        let first =
            extract_overrides_when_changed(&archive, &instance, &BTreeSet::new(), None).await?;
        assert_eq!(first, Some(1));

        // The user edits the config after installation.
        let config = instance.join("config").join("runtime.toml");
        std::fs::write(&config, b"user-edited")?;

        let again =
            extract_overrides_when_changed(&archive, &instance, &BTreeSet::new(), None).await?;
        assert_eq!(again, None, "unchanged archive must not be re-extracted");
        let content = std::fs::read(&config)?;
        assert_eq!(content, b"user-edited", "user edits must survive");

        // A rebuilt pack moves the fingerprint, and overrides apply once more.
        write_pack(
            &archive,
            &[("overrides/config/runtime.toml", b"shipped v2")],
        );
        let refreshed =
            extract_overrides_when_changed(&archive, &instance, &BTreeSet::new(), None).await?;
        assert_eq!(refreshed, Some(1));
        let content = std::fs::read(&config)?;
        assert_eq!(content, b"shipped v2");
        Ok(())
    }

    #[tokio::test]
    async fn overrides_skip_sync_managed_paths() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let archive = directory.path().join("pack.mrpack");
        write_pack(
            &archive,
            &[
                ("overrides/config/managed.toml", b"zip copy"),
                ("overrides/config/free.toml", b"zip only"),
            ],
        );
        let instance = directory.path().join("instance");
        let managed = BTreeSet::from(["config/managed.toml".to_owned()]);

        let written = extract_overrides_when_changed(&archive, &instance, &managed, None).await?;
        assert_eq!(written, Some(1), "only the unmanaged file is extracted");
        assert!(
            !instance.join("config").join("managed.toml").exists(),
            "sync-owned path must stay untouched by overrides"
        );
        assert!(instance.join("config").join("free.toml").is_file());
        Ok(())
    }
}
