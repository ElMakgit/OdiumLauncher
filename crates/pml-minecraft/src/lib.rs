//! Mojang metadata pipeline: version manifest, libraries, natives, assets,
//! classpath construction and launch argument substitution.
//!
//! All file work is async (`tokio::fs`) or `spawn_blocking` (zip extraction).
//! The only large structures are the version JSON and the asset index; both are
//! dropped before the JVM is spawned by the orchestration layer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use md5::{Digest as _, Md5};
use pml_config::{JvmConfig, PortablePaths, ServerSpec};
use pml_downloader::{
    safe_extract_zip, verify_hash, DownloadEvent, DownloadTask, Downloader, HashAlgo,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

mod nbt;
mod options;

/// Name shown for the managed entry in Minecraft's multiplayer list.
const SERVER_LIST_NAME: &str = pml_config::SERVER_LIST_NAME;

pub const VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const ASSET_BASE_URL: &str = "https://resources.download.minecraft.net";
const DEFAULT_MAVEN_BASE: &str = "https://libraries.minecraft.net/";

/// Hosts a version JSON may reference for executable content (the client jar,
/// libraries and natives). A loader profile is third-party data; without this
/// list a malicious profile could point a classpath entry at `file://` or an
/// attacker-controlled host and get code executed inside the game JVM.
const ALLOWED_ARTIFACT_HOSTS: &[&str] = &[
    "piston-data.mojang.com",
    "piston-meta.mojang.com",
    "launcher.mojang.com",
    "libraries.minecraft.net",
    "resources.download.minecraft.net",
    "maven.minecraftforge.net",
    "files.minecraftforge.net",
    "maven.neoforged.net",
    "maven.fabricmc.net",
    "maven.quiltmc.org",
    "meta.fabricmc.net",
    "meta.quiltmc.org",
];

fn validate_artifact_url(url: &str) -> Result<(), MinecraftError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| MinecraftError::InvalidArtifactUrl {
        url: url.to_owned(),
        reason: "не URL".to_owned(),
    })?;
    if parsed.scheme() != "https" {
        return Err(MinecraftError::InvalidArtifactUrl {
            url: url.to_owned(),
            reason: "разрешён только HTTPS".to_owned(),
        });
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if ALLOWED_ARTIFACT_HOSTS.contains(&host.as_str()) {
        Ok(())
    } else {
        Err(MinecraftError::InvalidArtifactUrl {
            url: url.to_owned(),
            reason: format!("недоверенный хост {host}"),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VersionJson {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub version_type: Option<String>,
    #[serde(default)]
    pub main_class: Option<String>,
    #[serde(default)]
    pub inherits_from: Option<String>,
    #[serde(default)]
    pub asset_index: Option<AssetIndex>,
    #[serde(default)]
    pub downloads: Option<VersionDownloads>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default)]
    pub arguments: Option<Arguments>,
    #[serde(default)]
    pub minecraft_arguments: Option<String>,
    #[serde(default)]
    pub logging: Option<Logging>,
    #[serde(default)]
    pub jar: Option<String>,
    #[serde(default)]
    pub java_version: Option<JavaVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Arguments {
    #[serde(default)]
    pub game: Vec<Argument>,
    #[serde(default)]
    pub jvm: Vec<Argument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Argument {
    Plain(String),
    Conditional {
        rules: Vec<Rule>,
        value: ArgumentValue,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgumentValue {
    Plain(String),
    Many(Vec<String>),
}

impl ArgumentValue {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::Plain(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Rule {
    #[serde(default)]
    pub action: RuleAction,
    #[serde(default)]
    pub os: Option<OsRule>,
    #[serde(default)]
    pub features: Option<BTreeMap<String, bool>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    #[default]
    Allow,
    Disallow,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OsRule {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub downloads: Option<LibraryDownloads>,
    #[serde(default)]
    pub natives: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub extract: Option<ExtractRule>,
    #[serde(default)]
    pub url: Option<String>,
    // Fabric/Quilt profiles place these hashes directly on the library object.
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub md5: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibraryDownloads {
    #[serde(default)]
    pub artifact: Option<Artifact>,
    #[serde(default)]
    pub classifiers: Option<BTreeMap<String, Artifact>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Artifact {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExtractRule {
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AssetIndex {
    pub id: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub total_size: Option<u64>,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VersionDownloads {
    #[serde(default)]
    pub client: Option<Artifact>,
    #[serde(default)]
    pub server: Option<Artifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Logging {
    #[serde(default)]
    pub client: Option<LoggingClient>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoggingClient {
    #[serde(default)]
    pub argument: String,
    #[serde(default)]
    pub file: Artifact,
    #[serde(rename = "type", default)]
    pub log_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct JavaVersion {
    #[serde(default)]
    pub component: String,
    #[serde(default)]
    pub major_version: u32,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct VersionManifest {
    #[serde(default)]
    versions: Vec<VersionManifestEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct VersionManifestEntry {
    id: String,
    url: String,
    #[serde(default)]
    sha1: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AssetIndexFile {
    #[serde(default)]
    objects: BTreeMap<String, AssetObject>,
    #[serde(rename = "virtual", default)]
    virtual_: Option<bool>,
    #[serde(default)]
    map_to_resources: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AssetObject {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Clone)]
pub struct UserProfile {
    pub nick: String,
    /// Offline UUID without hyphens, as expected by `--uuid`.
    pub uuid: String,
}

impl UserProfile {
    pub fn offline(nick: impl Into<String>) -> Self {
        let nick = nick.into();
        let uuid = offline_uuid(&nick);
        Self { nick, uuid }
    }
}

/// Everything needed by `pml-launcher` to start the JVM.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub version_id: String,
    pub main_class: String,
    pub classpath: Vec<PathBuf>,
    pub jvm_args: Vec<String>,
    pub game_args: Vec<String>,
    pub working_dir: PathBuf,
    pub natives_dir: PathBuf,
    pub assets_dir: PathBuf,
    pub instance_dir: PathBuf,
}

pub struct LaunchRequest<'a> {
    pub mc_version: &'a str,
    pub instance_dir: &'a Path,
    pub loader_profile: Option<&'a VersionJson>,
    pub user: &'a UserProfile,
    pub server: &'a ServerSpec,
    pub jvm: &'a JvmConfig,
    pub progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
}

#[derive(Clone)]
pub struct Minecraft {
    paths: PortablePaths,
    downloader: Downloader,
}

impl Minecraft {
    pub fn new(paths: PortablePaths, downloader: Downloader) -> Self {
        Self { paths, downloader }
    }

    pub async fn prepare_launch(
        &self,
        request: LaunchRequest<'_>,
    ) -> Result<LaunchPlan, MinecraftError> {
        tokio::fs::create_dir_all(request.instance_dir)
            .await
            .map_err(|source| io_error(request.instance_dir, source))?;
        for dir in [
            "mods",
            "config",
            "resourcepacks",
            "shaderpacks",
            "saves",
            "logs",
        ] {
            let path = request.instance_dir.join(dir);
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|source| io_error(&path, source))?;
        }

        // Keep the instance ready for the player: the pack server is present in
        // the multiplayer list and the interface starts in Russian. These are
        // cosmetic side files, so a failure must not abort the launch. Both
        // helpers use blocking `std::fs`, so they run off the async worker.
        let instance_for_side_files = request.instance_dir.to_path_buf();
        let server_display = request.server.display();
        let side_files = tokio::task::spawn_blocking(move || {
            let server = nbt::ensure_server_entry(
                &instance_for_side_files,
                SERVER_LIST_NAME,
                &server_display,
            )
            .map_err(|error| format!("servers.dat: {error}"));
            let language = options::ensure_russian_language(&instance_for_side_files)
                .map_err(|error| format!("options.txt: {error}"));
            (server, language)
        })
        .await;
        match side_files {
            Ok((Err(error), _)) => tracing::warn!(%error, "servers.dat was not updated"),
            Ok((_, Err(error))) => tracing::warn!(%error, "options.txt was not updated"),
            Ok((Ok(()), Ok(()))) => {}
            Err(error) => tracing::warn!(%error, "instance side-file task failed"),
        }

        let manifest = self.fetch_version_manifest().await?;
        let entry = manifest
            .versions
            .iter()
            .find(|entry| entry.id == request.mc_version)
            .ok_or_else(|| MinecraftError::VersionNotFound(request.mc_version.to_owned()))?;
        let vanilla = self.fetch_version_json(entry).await?;
        let merged = match request.loader_profile {
            Some(loader) => merge_versions(&vanilla, loader),
            None => vanilla.clone(),
        };

        let client_jar = self
            .ensure_client(&vanilla, request.mc_version, request.progress.clone())
            .await?;
        // Forge/NeoForge ignore the game jar by name (`-DignoreList=...,
        // ${version_name}.jar`). A jar named after the Minecraft version does
        // not match that pattern, so ModLauncher would load it as a second
        // `minecraft` module and module resolution would fail on a duplicate
        // package export. Name the jar after the merged profile instead.
        let client_jar = self
            .ensure_game_jar(&client_jar, &merged.id, request.mc_version)
            .await?;
        let (mut classpath, native_jars) = self
            .ensure_libraries(&merged, request.instance_dir, request.progress.clone())
            .await?;
        let natives_dir = request.instance_dir.join("natives");
        self.extract_natives(&native_jars, &natives_dir).await?;

        let (assets_dir, assets_index_name, logging_arg) = self
            .ensure_assets(&merged, request.instance_dir, request.progress.clone())
            .await?;

        classpath.push(client_jar);
        let classpath_string = classpath
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(classpath_separator());

        let context = ArgContext {
            nick: request.user.nick.clone(),
            uuid: request.user.uuid.clone(),
            access_token: "0".to_owned(),
            user_type: "legacy".to_owned(),
            version_name: merged.id.clone(),
            version_type: merged
                .version_type
                .clone()
                .unwrap_or_else(|| "release".to_owned()),
            game_dir: request.instance_dir.to_path_buf(),
            assets_dir: assets_dir.clone(),
            assets_index_name,
            natives_dir: natives_dir.clone(),
            libraries_dir: self.paths.libraries_dir(),
            classpath: classpath_string,
            classpath_separator: classpath_separator().to_owned(),
            launcher_name: pml_config::BRAND_NAME.to_owned(),
            launcher_version: env!("CARGO_PKG_VERSION").to_owned(),
            user_properties: "{}".to_owned(),
        };

        let jvm_args = build_jvm_args(&merged, &context, request.jvm, logging_arg.as_deref());
        let game_args = build_game_args(&merged, &context);
        let main_class = merged
            .main_class
            .clone()
            .ok_or(MinecraftError::MissingField("mainClass"))?;

        Ok(LaunchPlan {
            version_id: merged.id,
            main_class,
            classpath,
            jvm_args,
            game_args,
            working_dir: request.instance_dir.to_path_buf(),
            natives_dir,
            assets_dir,
            instance_dir: request.instance_dir.to_path_buf(),
        })
    }

    async fn fetch_version_manifest(&self) -> Result<VersionManifest, MinecraftError> {
        let cache = self.paths.meta_dir().join("version_manifest_v2.json");
        match self.fetch_bytes(VERSION_MANIFEST_URL).await {
            Ok(bytes) => {
                let manifest =
                    serde_json::from_slice::<VersionManifest>(&bytes).map_err(|source| {
                        MinecraftError::Json {
                            path: cache.clone(),
                            source,
                        }
                    })?;
                let _ = tokio::fs::write(&cache, &bytes).await;
                Ok(manifest)
            }
            Err(network_error) => {
                tracing::warn!(%network_error, "version manifest fetch failed, trying cache");
                let bytes = tokio::fs::read(&cache).await.map_err(|_| network_error)?;
                serde_json::from_slice(&bytes).map_err(|source| MinecraftError::Json {
                    path: cache,
                    source,
                })
            }
        }
    }

    async fn fetch_version_json(
        &self,
        entry: &VersionManifestEntry,
    ) -> Result<VersionJson, MinecraftError> {
        let cache = self
            .paths
            .meta_dir()
            .join("versions")
            .join(&entry.id)
            .join(format!("{}.json", entry.id));
        let hash = if entry.sha1.is_empty() {
            None
        } else {
            Some(entry.sha1.clone())
        };
        validate_artifact_url(&entry.url)?;
        let task = DownloadTask::new(
            entry.url.clone(),
            cache.clone(),
            hash,
            HashAlgo::Sha1,
            None,
            format!("version {}", entry.id),
        );
        if let Err(error) = self.downloader.download_many(vec![task], None).await {
            if tokio::fs::metadata(&cache).await.is_err() {
                return Err(error.into());
            }
            tracing::warn!(%error, "version json download failed, using cache");
        }
        let bytes = tokio::fs::read(&cache)
            .await
            .map_err(|source| io_error(&cache, source))?;
        serde_json::from_slice(&bytes).map_err(|source| MinecraftError::Json {
            path: cache,
            source,
        })
    }

    async fn ensure_client(
        &self,
        version: &VersionJson,
        mc_version: &str,
        progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    ) -> Result<PathBuf, MinecraftError> {
        let client = version
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.client.as_ref())
            .ok_or(MinecraftError::MissingField("downloads.client"))?;
        validate_artifact_url(&client.url)?;
        let dest = self
            .paths
            .versions_dir()
            .join(mc_version)
            .join(format!("{mc_version}.jar"));
        let task = DownloadTask::new(
            client.url.clone(),
            dest.clone(),
            client.sha1.clone(),
            HashAlgo::Sha1,
            client.size,
            format!("client {mc_version}"),
        );
        self.downloader.download_many(vec![task], progress).await?;
        Ok(dest)
    }

    /// Returns the game jar path that matches the loader profile's version id.
    ///
    /// Forge and NeoForge put `${version_name}.jar` in `-DignoreList`, so the
    /// jar we hand to ModLauncher must carry the merged profile id. A hard link
    /// avoids duplicating the ~26 MB client jar; a copy is the fallback for
    /// filesystems that do not support links.
    async fn ensure_game_jar(
        &self,
        client_jar: &Path,
        version_id: &str,
        mc_version: &str,
    ) -> Result<PathBuf, MinecraftError> {
        let uses_ignore_list = version_id.starts_with("neoforge-")
            || version_id.starts_with("forge-")
            || version_id.starts_with("neoform-");
        if !uses_ignore_list || version_id == mc_version {
            return Ok(client_jar.to_path_buf());
        }
        let dest = self
            .paths
            .versions_dir()
            .join(version_id)
            .join(format!("{version_id}.jar"));
        let source_len = tokio::fs::metadata(client_jar)
            .await
            .map_err(|source| io_error(client_jar, source))?
            .len();
        if let Ok(metadata) = tokio::fs::metadata(&dest).await {
            if metadata.len() == source_len {
                return Ok(dest);
            }
            // A previous interrupted copy left a truncated jar. It cannot be
            // used as a classpath entry and would fail the launch forever.
            let _ = tokio::fs::remove_file(&dest).await;
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| io_error(parent, source))?;
        }
        copy_atomic(client_jar, &dest).await?;
        Ok(dest)
    }

    async fn ensure_assets(
        &self,
        version: &VersionJson,
        instance_dir: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    ) -> Result<(PathBuf, String, Option<String>), MinecraftError> {
        let index_meta = version
            .asset_index
            .as_ref()
            .ok_or(MinecraftError::MissingField("assetIndex"))?;
        validate_artifact_url(&index_meta.url)?;
        let index_path = self
            .paths
            .assets_dir()
            .join("indexes")
            .join(format!("{}.json", index_meta.id));
        let index_task = DownloadTask::new(
            index_meta.url.clone(),
            index_path.clone(),
            index_meta.sha1.clone(),
            HashAlgo::Sha1,
            index_meta.size,
            format!("asset index {}", index_meta.id),
        );
        self.downloader
            .download_many(vec![index_task], progress.clone())
            .await?;

        let bytes = tokio::fs::read(&index_path)
            .await
            .map_err(|source| io_error(&index_path, source))?;
        let index = serde_json::from_slice::<AssetIndexFile>(&bytes).map_err(|source| {
            MinecraftError::Json {
                path: index_path,
                source,
            }
        })?;

        let mut tasks = Vec::with_capacity(index.objects.len());
        for (name, object) in &index.objects {
            if object.hash.len() < 2 {
                return Err(MinecraftError::InvalidAssetHash(object.hash.clone()));
            }
            let prefix = &object.hash[..2];
            let dest = self
                .paths
                .assets_dir()
                .join("objects")
                .join(prefix)
                .join(&object.hash);
            let url = format!("{ASSET_BASE_URL}/{prefix}/{}", object.hash);
            tasks.push(DownloadTask::new(
                url,
                dest,
                Some(object.hash.clone()),
                HashAlgo::Sha1,
                Some(object.size),
                name.clone(),
            ));
        }
        if !tasks.is_empty() {
            self.downloader
                .download_many(tasks, progress.clone())
                .await?;
        }
        self.materialize_virtual_assets(&index, &index_meta.id, instance_dir)
            .await?;

        let logging_arg =
            if let Some(client) = version.logging.as_ref().and_then(|l| l.client.as_ref()) {
                if client.file.url.is_empty() {
                    None
                } else {
                    validate_artifact_url(&client.file.url)?;
                    let file_id = client
                        .file
                        .id
                        .clone()
                        .unwrap_or_else(|| "client-log4j.xml".to_owned());
                    let log_path = self.paths.assets_dir().join("log_configs").join(&file_id);
                    let task = DownloadTask::new(
                        client.file.url.clone(),
                        log_path.clone(),
                        client.file.sha1.clone(),
                        HashAlgo::Sha1,
                        client.file.size,
                        "logging config",
                    );
                    self.downloader.download_many(vec![task], progress).await?;
                    Some(
                        client
                            .argument
                            .replace("${path}", &log_path.to_string_lossy()),
                    )
                }
            } else {
                None
            };

        Ok((self.paths.assets_dir(), index_meta.id.clone(), logging_arg))
    }

    async fn materialize_virtual_assets(
        &self,
        index: &AssetIndexFile,
        index_id: &str,
        instance_dir: &Path,
    ) -> Result<(), MinecraftError> {
        let target_root = if index.virtual_ == Some(true) {
            Some(self.paths.assets_dir().join("virtual").join(index_id))
        } else if index.map_to_resources == Some(true) {
            Some(instance_dir.join("resources"))
        } else {
            None
        };
        let Some(target_root) = target_root else {
            return Ok(());
        };
        tokio::fs::create_dir_all(&target_root)
            .await
            .map_err(|source| io_error(&target_root, source))?;

        for (name, object) in &index.objects {
            let relative = Path::new(name);
            if !pml_downloader::is_safe_relative(relative) {
                tracing::warn!(asset = name, "skipping unsafe virtual asset name");
                continue;
            }
            if object.hash.len() < 2 {
                continue;
            }
            let source = self
                .paths
                .assets_dir()
                .join("objects")
                .join(&object.hash[..2])
                .join(&object.hash);
            let dest = target_root.join(relative);
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|source| io_error(parent, source))?;
            }
            let source_len = match tokio::fs::metadata(&source).await {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // The asset download failed earlier in the pipeline; the
                    // sync step reports it. Do not create a dangling entry.
                    continue;
                }
                Err(source_error) => return Err(io_error(&source, source_error)),
            };
            if let Ok(metadata) = tokio::fs::metadata(&dest).await {
                if metadata.len() == source_len {
                    continue;
                }
                let _ = tokio::fs::remove_file(&dest).await;
            }
            copy_atomic(&source, &dest).await?;
        }
        Ok(())
    }

    async fn ensure_libraries(
        &self,
        version: &VersionJson,
        instance_dir: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>), MinecraftError> {
        let mut classpath = Vec::new();
        let mut native_jars = Vec::new();
        let mut seen_classpath = BTreeSet::new();
        let mut seen_natives = BTreeSet::new();
        let mut tasks = Vec::new();
        let instance_libraries = instance_dir.join("libraries");

        for library in version
            .libraries
            .iter()
            .filter(|lib| rules_allow(&lib.rules))
        {
            if wrong_arch_native(&library.name) {
                continue;
            }
            for artifact in resolve_library(library)? {
                if artifact.path.is_empty() {
                    continue;
                }
                let relative = PathBuf::from(&artifact.path);
                if !pml_downloader::is_safe_relative(&relative) {
                    return Err(MinecraftError::UnsafeLibraryPath(artifact.path));
                }
                let instance_dest = instance_libraries.join(&relative);
                let global_dest = self.paths.libraries_dir().join(&relative);
                let use_instance =
                    file_matches(&instance_dest, artifact.sha1.as_deref(), artifact.size).await?;
                let destination = if use_instance {
                    instance_dest
                } else {
                    global_dest.clone()
                };

                if !use_instance
                    && !file_matches(&global_dest, artifact.sha1.as_deref(), artifact.size).await?
                {
                    tasks.push(DownloadTask::new(
                        artifact.url,
                        global_dest.clone(),
                        artifact.sha1,
                        HashAlgo::Sha1,
                        artifact.size,
                        artifact
                            .path
                            .rsplit('/')
                            .next()
                            .unwrap_or("library")
                            .to_owned(),
                    ));
                }

                if artifact.classpath
                    && !wrong_arch_native(&library.name)
                    && seen_classpath.insert(destination.clone())
                {
                    classpath.push(destination.clone());
                }
                if artifact.native && seen_natives.insert(destination.clone()) {
                    native_jars.push(destination);
                }
            }
        }

        if !tasks.is_empty() {
            self.downloader.download_many(tasks, progress).await?;
        }
        Ok((classpath, native_jars))
    }

    async fn extract_natives(
        &self,
        native_jars: &[PathBuf],
        natives_dir: &Path,
    ) -> Result<(), MinecraftError> {
        match tokio::fs::remove_dir_all(natives_dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(natives_dir, source)),
        }
        tokio::fs::create_dir_all(natives_dir)
            .await
            .map_err(|source| io_error(natives_dir, source))?;

        for jar in native_jars {
            let jar = jar.clone();
            let destination = natives_dir.to_path_buf();
            let extracted =
                tokio::task::spawn_blocking(move || safe_extract_zip(&jar, &destination))
                    .await
                    .map_err(|error| MinecraftError::Native(error.to_string()))?;
            extracted?;
        }
        Ok(())
    }

    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, MinecraftError> {
        let response = self
            .downloader
            .client()
            .get(url)
            .send()
            .await
            .map_err(|source| MinecraftError::Network {
                url: url.to_owned(),
                source,
            })?;
        if !response.status().is_success() {
            return Err(MinecraftError::Http {
                status: response.status().as_u16(),
                url: url.to_owned(),
            });
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|source| MinecraftError::Network {
                url: url.to_owned(),
                source,
            })
    }
}

async fn file_matches(
    path: &Path,
    sha1: Option<&str>,
    size: Option<u64>,
) -> Result<bool, MinecraftError> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(io_error(path, source)),
    };
    if let Some(expected) = sha1.filter(|hash| !hash.trim().is_empty()) {
        return verify_hash(path, expected, HashAlgo::Sha1)
            .await
            .map_err(MinecraftError::Download);
    }
    if let Some(expected) = size {
        return Ok(metadata.len() == expected);
    }
    Ok(true)
}

#[derive(Debug, Clone)]
struct ResolvedArtifact {
    path: String,
    url: String,
    sha1: Option<String>,
    size: Option<u64>,
    classpath: bool,
    native: bool,
}

fn resolve_library(library: &Library) -> Result<Vec<ResolvedArtifact>, MinecraftError> {
    let base = library.url.as_deref().unwrap_or(DEFAULT_MAVEN_BASE);
    let base = if base.ends_with('/') {
        base.to_owned()
    } else {
        format!("{base}/")
    };
    let mut resolved = Vec::new();

    if let Some(downloads) = &library.downloads {
        if let Some(artifact) = &downloads.artifact {
            if let Some(path) = artifact.path.clone().or_else(|| maven_path(&library.name)) {
                let url = if artifact.url.is_empty() {
                    format!("{base}{path}")
                } else {
                    artifact.url.clone()
                };
                validate_artifact_url(&url)?;
                resolved.push(ResolvedArtifact {
                    path,
                    url,
                    sha1: artifact.sha1.clone().or_else(|| library.sha1.clone()),
                    size: artifact.size.or(library.size),
                    classpath: true,
                    native: is_native_name(&library.name),
                });
            }
        }

        if let (Some(classifiers), Some(classifier)) =
            (&downloads.classifiers, native_classifier(library))
        {
            if let Some(artifact) = classifiers.get(&classifier) {
                let coordinate = format!("{}:{classifier}", library.name);
                if let Some(path) = artifact.path.clone().or_else(|| maven_path(&coordinate)) {
                    let url = if artifact.url.is_empty() {
                        format!("{base}{path}")
                    } else {
                        artifact.url.clone()
                    };
                    resolved.push(ResolvedArtifact {
                        path,
                        url,
                        sha1: artifact.sha1.clone().or_else(|| library.sha1.clone()),
                        size: artifact.size.or(library.size),
                        classpath: false,
                        native: true,
                    });
                }
            }
        }
    }

    if resolved.is_empty() {
        if let Some(path) = maven_path(&library.name) {
            let url = format!("{base}{path}");
            validate_artifact_url(&url)?;
            resolved.push(ResolvedArtifact {
                path: path.clone(),
                url,
                sha1: library.sha1.clone(),
                size: library.size,
                classpath: true,
                native: is_native_name(&library.name),
            });
        }
    }
    Ok(resolved)
}

pub fn maven_path(name: &str) -> Option<String> {
    let (coordinate, extension) = match name.split_once('@') {
        Some((coordinate, extension)) => (coordinate, extension),
        None => (name, "jar"),
    };
    let parts: Vec<&str> = coordinate.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let group = parts[0].replace('.', "/");
    let artifact = parts[1];
    let version = parts[2];
    let classifier = parts.get(3).copied();
    let file = match classifier {
        Some(classifier) if !classifier.is_empty() => {
            format!("{artifact}-{version}-{classifier}.{extension}")
        }
        _ => format!("{artifact}-{version}.{extension}"),
    };
    Some(format!("{group}/{artifact}/{version}/{file}"))
}

fn native_classifier(library: &Library) -> Option<String> {
    let raw = library.natives.as_ref()?.get("windows")?;
    let arch = if cfg!(target_arch = "x86_64") {
        "64"
    } else {
        "32"
    };
    Some(raw.replace("${arch}", arch))
}

fn is_native_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains(":natives-")
        || lower.contains("natives-windows")
        || lower.contains("natives-linux")
        || lower.contains("natives-macos")
        || lower.contains("natives-osx")
}

fn wrong_arch_native(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if cfg!(target_arch = "x86_64")
        && (lower.contains("natives-windows-arm64") || lower.contains("natives-windows-x86"))
    {
        return true;
    }
    if cfg!(target_arch = "aarch64") && lower.contains("natives-windows-x86") {
        return true;
    }
    lower.contains("natives-linux")
        || lower.contains("natives-macos")
        || lower.contains("natives-osx")
}

pub fn classpath_separator() -> &'static str {
    if cfg!(windows) {
        ";"
    } else {
        ":"
    }
}

pub fn merge_versions(parent: &VersionJson, child: &VersionJson) -> VersionJson {
    let mut merged = parent.clone();
    if !child.id.is_empty() {
        merged.id = child.id.clone();
    }
    merged.main_class = child
        .main_class
        .clone()
        .or_else(|| parent.main_class.clone());
    merged.version_type = child
        .version_type
        .clone()
        .or_else(|| parent.version_type.clone());
    merged.asset_index = child
        .asset_index
        .clone()
        .or_else(|| parent.asset_index.clone());
    merged.downloads = child.downloads.clone().or_else(|| parent.downloads.clone());
    merged.logging = child.logging.clone().or_else(|| parent.logging.clone());
    merged.java_version = child
        .java_version
        .clone()
        .or_else(|| parent.java_version.clone());
    merged.jar = child.jar.clone().or_else(|| parent.jar.clone());
    merged.minecraft_arguments = child
        .minecraft_arguments
        .clone()
        .or_else(|| parent.minecraft_arguments.clone());

    let child_keys: BTreeSet<String> = child.libraries.iter().map(library_key).collect();
    merged.libraries = parent
        .libraries
        .iter()
        .filter(|library| !child_keys.contains(&library_key(library)))
        .cloned()
        .collect();
    merged.libraries.extend(child.libraries.iter().cloned());

    let parent_arguments = parent.arguments.clone().unwrap_or_default();
    let child_arguments = child.arguments.clone().unwrap_or_default();
    merged.arguments = Some(Arguments {
        game: parent_arguments
            .game
            .into_iter()
            .chain(child_arguments.game)
            .collect(),
        jvm: parent_arguments
            .jvm
            .into_iter()
            .chain(child_arguments.jvm)
            .collect(),
    });
    merged.inherits_from = None;
    merged
}

fn library_key(library: &Library) -> String {
    let parts: Vec<&str> = library.name.split(':').collect();
    match parts.as_slice() {
        [group, artifact, ..] => {
            if let Some(classifier) = parts.get(3) {
                format!("{group}:{artifact}:{classifier}")
            } else {
                format!("{group}:{artifact}")
            }
        }
        _ => library.name.clone(),
    }
}

pub fn rules_allow(rules: &[Rule]) -> bool {
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule) {
            allowed = rule.action == RuleAction::Allow;
        }
    }
    allowed
}

fn current_os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

fn rule_matches(rule: &Rule) -> bool {
    if let Some(os) = &rule.os {
        if let Some(name) = &os.name {
            let current = current_os_name();
            // Mojang uses both `osx` and `macos`; old manifests sometimes say
            // `win` instead of `windows`.
            let matches = name.eq_ignore_ascii_case(current)
                || (current == "osx" && name.eq_ignore_ascii_case("macos"))
                || (current == "windows" && name.eq_ignore_ascii_case("win"));
            if !matches {
                return false;
            }
        }
        if let Some(arch) = &os.arch {
            let current = std::env::consts::ARCH;
            let arch_lower = arch.to_ascii_lowercase();
            // Manifest aliases: `amd64` == `x86_64`, `arm64` == `aarch64`,
            // and every 32-bit x86 spelling collapses to `x86`.
            let normalized = match arch_lower.as_str() {
                "x86_64" | "amd64" => "x86_64",
                "arm64" | "aarch64" => "aarch64",
                "x86" | "i386" | "i486" | "i586" | "i686" => "x86",
                other => other,
            };
            if normalized != current {
                return false;
            }
        }
        if os.version.is_some() {
            // Version regexes appear only in pre-1.6 manifests; the launcher
            // targets 1.21.1, where no rule carries one. Treating them as a
            // match is safer than evaluating a regex against a fake version.
        }
    }
    if let Some(features) = &rule.features {
        for (feature, expected) in features {
            // The launcher never enables demo/custom-resolution/quick-play
            // features, so every requested feature is false.
            if *expected {
                return false;
            }
            let _ = feature;
        }
    }
    true
}

pub fn flatten_arguments(arguments: &[Argument]) -> Vec<String> {
    let mut result = Vec::new();
    for argument in arguments {
        match argument {
            Argument::Plain(value) => result.push(value.clone()),
            Argument::Conditional { rules, value } => {
                if rules_allow(rules) {
                    result.extend(value.clone().into_vec());
                }
            }
        }
    }
    result
}

#[derive(Debug, Clone)]
pub struct ArgContext {
    pub nick: String,
    pub uuid: String,
    pub access_token: String,
    pub user_type: String,
    pub version_name: String,
    pub version_type: String,
    pub game_dir: PathBuf,
    pub assets_dir: PathBuf,
    pub assets_index_name: String,
    pub natives_dir: PathBuf,
    pub libraries_dir: PathBuf,
    pub classpath: String,
    pub classpath_separator: String,
    pub launcher_name: String,
    pub launcher_version: String,
    pub user_properties: String,
}

pub fn build_game_args(version: &VersionJson, context: &ArgContext) -> Vec<String> {
    let mut raw = version
        .arguments
        .as_ref()
        .map(|arguments| flatten_arguments(&arguments.game))
        .unwrap_or_default();
    if raw.is_empty() {
        if let Some(legacy) = &version.minecraft_arguments {
            raw = legacy.split_whitespace().map(str::to_owned).collect();
        }
    }

    let args: Vec<String> = raw
        .iter()
        .map(|argument| substitute_placeholders(argument, context))
        .collect();

    // NeoForge 1.21.1 ignores the legacy quick-connect pair and prints
    // "Completely ignored arguments" in the log. The server is written to
    // servers.dat instead, which puts it in the multiplayer list.
    let mut cleaned = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--server" || args[index] == "--port" {
            index += 2;
            continue;
        }
        cleaned.push(args[index].clone());
        index += 1;
    }
    cleaned
}

pub fn build_jvm_args(
    version: &VersionJson,
    context: &ArgContext,
    jvm: &JvmConfig,
    logging_arg: Option<&str>,
) -> Vec<String> {
    // Exact -Xms/-Xmx at the very front. Any loader-provided heap flags are
    // removed below, so the user's allocation is authoritative.
    let mut args = vec![format!("-Xms{}M", jvm.xms), format!("-Xmx{}M", jvm.xmx)];

    let raw = version
        .arguments
        .as_ref()
        .map(|arguments| flatten_arguments(&arguments.jvm))
        .unwrap_or_default();
    for argument in raw {
        let argument = substitute_placeholders(&argument, context);
        if argument.starts_with("-Xms") || argument.starts_with("-Xmx") {
            continue;
        }
        args.push(argument);
    }

    let has_classpath = args.iter().any(|arg| arg == "-cp" || arg == "-classpath");
    if !has_classpath {
        args.push("-cp".to_owned());
        args.push(context.classpath.clone());
    }
    if !args
        .iter()
        .any(|arg| arg.starts_with("-Djava.library.path="))
    {
        args.push(format!(
            "-Djava.library.path={}",
            context.natives_dir.to_string_lossy()
        ));
    }
    if !args
        .iter()
        .any(|arg| arg.starts_with("-Dminecraft.launcher.brand="))
    {
        args.push(format!(
            "-Dminecraft.launcher.brand={}",
            context.launcher_name
        ));
    }
    if let Some(logging_arg) = logging_arg {
        if !args.iter().any(|arg| arg == logging_arg) {
            args.push(logging_arg.to_owned());
        }
    }
    args
}

fn substitute_placeholders(template: &str, context: &ArgContext) -> String {
    let mut values: BTreeMap<&str, String> = BTreeMap::new();
    values.insert("auth_player_name", context.nick.clone());
    values.insert("auth_uuid", context.uuid.clone());
    values.insert("auth_access_token", context.access_token.clone());
    values.insert("auth_session", context.access_token.clone());
    values.insert("user_type", context.user_type.clone());
    values.insert("version_name", context.version_name.clone());
    values.insert("version_type", context.version_type.clone());
    values.insert(
        "game_directory",
        context.game_dir.to_string_lossy().into_owned(),
    );
    values.insert(
        "game_assets",
        context.assets_dir.to_string_lossy().into_owned(),
    );
    values.insert(
        "assets_root",
        context.assets_dir.to_string_lossy().into_owned(),
    );
    values.insert("assets_index_name", context.assets_index_name.clone());
    values.insert(
        "natives_directory",
        context.natives_dir.to_string_lossy().into_owned(),
    );
    values.insert(
        "library_directory",
        context.libraries_dir.to_string_lossy().into_owned(),
    );
    values.insert("classpath", context.classpath.clone());
    values.insert("classpath_separator", context.classpath_separator.clone());
    values.insert("launcher_name", context.launcher_name.clone());
    values.insert("launcher_version", context.launcher_version.clone());
    values.insert("user_properties", context.user_properties.clone());
    values.insert("clientid", String::new());
    values.insert("auth_xuid", String::new());
    values.insert("resolution_width", "854".to_owned());
    values.insert("resolution_height", "480".to_owned());
    values.insert("quickPlayPath", String::new());
    values.insert("quickPlaySingleplayer", String::new());
    values.insert("quickPlayMultiplayer", String::new());
    values.insert("quickPlayRealms", String::new());

    let mut output = String::with_capacity(template.len());
    let mut index = 0;
    while index < template.len() {
        if template[index..].starts_with("${") {
            if let Some(relative_end) = template[index + 2..].find('}') {
                let end = index + 2 + relative_end;
                let key = &template[index + 2..end];
                if let Some(value) = values.get(key) {
                    output.push_str(value);
                }
                index = end + 1;
                continue;
            }
        }
        let Some(character) = template[index..].chars().next() else {
            break;
        };
        output.push(character);
        index += character.len_utf8();
    }
    output
}

/// Java's `UUID.nameUUIDFromBytes("OfflinePlayer:<nick>")`: MD5, version 3,
/// RFC 4122 variant, returned without hyphens for `--uuid`.
pub fn offline_uuid(nick: &str) -> String {
    let digest = Md5::digest(format!("OfflinePlayer:{nick}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest);
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    hex::encode(bytes)
}

pub fn offline_uuid_hyphenated(nick: &str) -> String {
    let raw = offline_uuid(nick);
    format!(
        "{}-{}-{}-{}-{}",
        &raw[0..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..32]
    )
}

fn io_error(path: &Path, source: std::io::Error) -> MinecraftError {
    MinecraftError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Copies or hard-links `source` into `dest` through a sibling temporary file,
/// then renames it into place. A crash mid-copy therefore leaves a `.tmp`
/// file, never a truncated jar that looks installed forever.
async fn copy_atomic(source: &Path, dest: &Path) -> Result<(), MinecraftError> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let temp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = tokio::fs::remove_file(&temp).await;
    if tokio::fs::hard_link(source, &temp).await.is_err() {
        tokio::fs::copy(source, &temp)
            .await
            .map_err(|source| io_error(&temp, source))?;
    }
    if let Err(source) = tokio::fs::rename(&temp, dest).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(io_error(dest, source));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum MinecraftError {
    #[error("HTTP {status} для {url}")]
    Http { status: u16, url: String },
    #[error("сетевая ошибка {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("ошибка JSON {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("ошибка файла {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("ошибка загрузки: {0}")]
    Download(#[from] pml_downloader::DownloadError),
    #[error("в манифесте нет версии {0}")]
    VersionNotFound(String),
    #[error("в version JSON отсутствует поле {0}")]
    MissingField(&'static str),
    #[error("некорректный хеш ассета: {0}")]
    InvalidAssetHash(String),
    #[error("небезопасный путь библиотеки: {0}")]
    UnsafeLibraryPath(String),
    #[error("недопустимый URL артефакта {url}: {reason}")]
    InvalidArtifactUrl { url: String, reason: String },
    #[error("ошибка нативной библиотеки: {0}")]
    Native(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> ArgContext {
        ArgContext {
            nick: "Steve".to_owned(),
            uuid: "a01e3843e5213998958af459800e4d11".to_owned(),
            access_token: "0".to_owned(),
            user_type: "legacy".to_owned(),
            version_name: "fabric-loader-0.16.5-1.21.1".to_owned(),
            version_type: "release".to_owned(),
            game_dir: PathBuf::from("C:\\launcher_data\\instances\\vanilla"),
            assets_dir: PathBuf::from("C:\\launcher_data\\assets"),
            assets_index_name: "17".to_owned(),
            natives_dir: PathBuf::from("C:\\launcher_data\\instances\\vanilla\\natives"),
            libraries_dir: PathBuf::from("C:\\launcher_data\\libraries"),
            classpath: "a.jar;b.jar".to_owned(),
            classpath_separator: ";".to_owned(),
            launcher_name: pml_config::BRAND_NAME.to_owned(),
            launcher_version: "0.1.0".to_owned(),
            user_properties: "{}".to_owned(),
        }
    }

    #[test]
    fn build_args_do_not_add_legacy_server_flags() {
        let version = VersionJson {
            id: "test".to_owned(),
            arguments: Some(Arguments {
                game: vec![
                    Argument::Plain("--username".to_owned()),
                    Argument::Plain("${auth_player_name}".to_owned()),
                ],
                jvm: Vec::new(),
            }),
            ..VersionJson::default()
        };
        let context = test_context();
        let args = build_game_args(&version, &context);
        assert_eq!(args[0], "--username");
        assert_eq!(args[1], "Steve");
        assert!(!args.iter().any(|arg| arg == "--server"));
        assert!(!args.iter().any(|arg| arg == "--port"));
    }

    #[test]
    fn stale_server_args_are_removed() {
        let version = VersionJson {
            arguments: Some(Arguments {
                game: vec![
                    Argument::Plain("--server".to_owned()),
                    Argument::Plain("old.example".to_owned()),
                    Argument::Plain("--port".to_owned()),
                    Argument::Plain("1".to_owned()),
                ],
                jvm: Vec::new(),
            }),
            ..VersionJson::default()
        };
        let args = build_game_args(&version, &test_context());
        assert!(!args.iter().any(|arg| arg == "old.example"));
        assert!(!args.iter().any(|arg| arg == "--server"));
        assert!(!args.iter().any(|arg| arg == "--port"));
    }

    #[test]
    fn jvm_args_are_exact_and_classpath_present() {
        let version = VersionJson {
            arguments: Some(Arguments {
                game: Vec::new(),
                jvm: vec![
                    Argument::Plain("-Xmx99G".to_owned()),
                    Argument::Plain("-Djava.library.path=${natives_directory}".to_owned()),
                    Argument::Plain("-cp".to_owned()),
                    Argument::Plain("${classpath}".to_owned()),
                ],
            }),
            ..VersionJson::default()
        };
        let args = build_jvm_args(
            &version,
            &test_context(),
            &JvmConfig {
                xmx: 4096,
                xms: 1024,
                auto_xmx: false,
            },
            Some("-Dlog4j.configurationFile=C:\\log.xml"),
        );
        assert_eq!(args[0], "-Xms1024M");
        assert_eq!(args[1], "-Xmx4096M");
        assert!(!args.iter().any(|arg| arg == "-Xmx99G"));
        assert!(args.iter().any(|arg| arg == "-cp"));
        assert!(args.iter().any(|arg| arg == "a.jar;b.jar"));
        assert!(args
            .iter()
            .any(|arg| arg.starts_with("-Djava.library.path=")));
        assert!(args
            .iter()
            .any(|arg| arg == "-Dlog4j.configurationFile=C:\\log.xml"));
    }

    #[test]
    fn offline_uuid_matches_java() {
        assert_eq!(offline_uuid("Player"), "a01e3843e5213998958af459800e4d11");
        assert_eq!(
            offline_uuid_hyphenated("Player"),
            "a01e3843-e521-3998-958a-f459800e4d11"
        );
    }

    #[test]
    fn maven_coordinates() {
        assert_eq!(
            maven_path("net.fabricmc:fabric-loader:0.16.5").as_deref(),
            Some("net/fabricmc/fabric-loader/0.16.5/fabric-loader-0.16.5.jar")
        );
        assert_eq!(
            maven_path("org.lwjgl:lwjgl:3.3.3:natives-windows").as_deref(),
            Some("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows.jar")
        );
    }

    #[test]
    fn merge_keeps_parent_arguments_and_child_main_class() {
        let parent = VersionJson {
            id: "1.21.1".to_owned(),
            main_class: Some("net.minecraft.client.main.Main".to_owned()),
            arguments: Some(Arguments {
                game: vec![Argument::Plain("--username".to_owned())],
                jvm: vec![Argument::Plain("-cp".to_owned())],
            }),
            ..VersionJson::default()
        };
        let child = VersionJson {
            id: "fabric-loader-0.16.5-1.21.1".to_owned(),
            inherits_from: Some("1.21.1".to_owned()),
            main_class: Some("net.fabricmc.loader.impl.launch.knot.KnotClient".to_owned()),
            arguments: Some(Arguments {
                game: Vec::new(),
                jvm: vec![Argument::Plain(
                    "-DFabricMcEmu= net.minecraft.client.main.Main".to_owned(),
                )],
            }),
            ..VersionJson::default()
        };
        let merged = merge_versions(&parent, &child);
        assert_eq!(
            merged.main_class.as_deref(),
            Some("net.fabricmc.loader.impl.launch.knot.KnotClient")
        );
        assert_eq!(merged.id, "fabric-loader-0.16.5-1.21.1");
        assert_eq!(
            flatten_arguments(&merged.arguments.as_ref().expect("args").game),
            vec!["--username"]
        );
        assert_eq!(
            flatten_arguments(&merged.arguments.as_ref().expect("args").jvm).len(),
            2
        );
    }

    #[test]
    fn rules_filter_linux_and_wrong_arch() {
        let linux = vec![Rule {
            action: RuleAction::Allow,
            os: Some(OsRule {
                name: Some("linux".to_owned()),
                ..OsRule::default()
            }),
            features: None,
        }];
        assert!(!rules_allow(&linux));
        let x86 = vec![Rule {
            action: RuleAction::Allow,
            os: Some(OsRule {
                name: Some("windows".to_owned()),
                arch: Some("x86".to_owned()),
                version: None,
            }),
            features: None,
        }];
        assert!(!rules_allow(&x86));
    }
}
