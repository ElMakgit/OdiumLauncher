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

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use pml_config::{LoaderKind, LoaderSpec};
use pml_downloader::{hash_bytes, is_safe_relative, HashAlgo};
use serde::Deserialize;

use crate::{Manifest, ManifestFile, ModpackError, MANIFEST_SCHEMA_VERSION};

pub const INDEX_FILE: &str = "modrinth.index.json";
const OVERRIDE_PREFIXES: [&str; 2] = ["overrides/", "client-overrides/"];

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
        for file in &self.files {
            // Server-only entries must not be installed on a client.
            if file.env.get("client").map(String::as_str) == Some("unsupported") {
                continue;
            }
            let url = file.downloads.first().ok_or_else(|| {
                ModpackError::Validation(format!("у файла {} нет ссылки на скачивание", file.path))
            })?;
            files.push(ManifestFile {
                path: file.path.clone(),
                url: url.clone(),
                sha256: file.hashes.get("sha256").cloned().unwrap_or_default(),
                sha512: file.hashes.get("sha512").cloned().unwrap_or_default(),
                sha1: file.hashes.get("sha1").cloned().unwrap_or_default(),
                size: file.file_size,
            });
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
            optional_files: Vec::new(),
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
    let response = request.send().await?;
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
    let bytes = response.bytes().await?;
    let part = archive.with_extension("mrpack.part");
    tokio::fs::write(&part, &bytes)
        .await
        .map_err(|source| ModpackError::Io {
            path: part.clone(),
            source,
        })?;
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
    let file = std::fs::File::open(archive).map_err(|source| ModpackError::Io {
        path: archive.to_path_buf(),
        source,
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| {
        ModpackError::Validation(format!("не удалось открыть .mrpack: {error}"))
    })?;
    let mut written = 0;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| {
            ModpackError::Validation(format!("не удалось прочитать файл архива: {error}"))
        })?;
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
        let destination = instance_dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ModpackError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
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
            path: destination,
            source,
        })?;
        written += 1;
    }
    Ok(written)
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
}
