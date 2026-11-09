//! Mod loader provisioning.
//!
//! Fabric and Quilt expose ready-to-use profile JSON documents, which are
//! equivalent to what their installers generate and avoid running Java at all.
//! Forge and NeoForge only ship a headless installer; it is executed with
//! `--installClient` pointed at `launcher_data`, and the generated version JSON
//! is then read back and merged with the vanilla version by `pml-minecraft`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use pml_config::{LoaderKind, LoaderSpec, PortablePaths};
use pml_downloader::{CancelToken, DownloadEvent, DownloadTask, Downloader, HashAlgo};
use pml_minecraft::VersionJson;
use thiserror::Error;
use tokio::sync::mpsc;

pub mod mirrors;

use mirrors::NEOFORGE_MIRRORS;

/// Offline NeoForge archives compiled into this binary by `build.rs`.
mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_bundles.rs"));
}

/// Bytes of a bundled archive, when this build embedded it.
fn embedded_bundle(file: &str) -> Option<&'static [u8]> {
    embedded::EMBEDDED_BUNDLES
        .iter()
        .find(|(name, _)| *name == file)
        .map(|(_, bytes)| *bytes)
}

const FABRIC_META: &str = "https://meta.fabricmc.net/v2/versions/loader";
const QUILT_META: &str = "https://meta.quiltmc.org/v3/versions/loader";
const FORGE_MAVEN: &str = "https://maven.minecraftforge.net/net/minecraftforge/forge";
const NEOFORGE_MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged/neoforge";
/// NeoForge 1.20.1 was published as `net.neoforged:forge` with the
/// `1.20.1-47.x` version; newer releases use `net.neoforged:neoforge`.
const NEOFORGE_LEGACY_MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged/forge";
/// A Forge/NeoForge installer may need to download libraries, but it must never
/// hang the launch forever when Maven or the network stalls.
const INSTALLER_TIMEOUT: Duration = Duration::from_secs(300);
/// A checksum fetch is a tiny request; a couple of retries hide flaky mirrors
/// without delaying a launch for long.
const CHECKSUM_ATTEMPTS: u32 = 3;
const CHECKSUM_RETRY_DELAY: Duration = Duration::from_millis(400);

/// Offline mirror of the NeoForge artifacts hosted on `maven.neoforged.net`.
///
/// Some providers (notably in Russia) block that host or its CDN while Mojang's
/// library host keeps working. When the installer cannot be downloaded the
/// launcher fetches this bundle instead: it contains the official installer plus
/// the exact artifacts the installer would have taken from NeoForge's Maven.
/// Mojang-hosted libraries are deliberately absent, so the installer keeps
/// downloading them from Mojang and Minecraft's redistribution terms do not
/// apply to the bundle.
const NEOFORGE_MIRROR_BASE: &str =
    "https://github.com/ElMakgit/OdiumLauncher/releases/download/neoforge-mirror";

/// Public GitHub proxies. Release assets are throttled or blocked from some
/// networks while GitHub itself stays reachable; each proxy serves the same
/// bytes, and the pinned SHA-256 catches any proxy that tampers with them.
const GITHUB_PROXIES: &[&str] = &[
    "https://ghproxy.net/",
    "https://ghfast.top/",
    "https://gh-proxy.com/",
];

struct OfflineBundle {
    minecraft: &'static str,
    /// Bare loader version, without the `1.20.1-` prefix used by legacy builds.
    loader: &'static str,
    file: &'static str,
    sha256: &'static str,
    size: u64,
}

/// Pinned so a compromised release asset cannot inject an installer. Every file
/// inside is verified again against the SHA-1 list in the bundle manifest.
const OFFLINE_BUNDLES: &[OfflineBundle] = &[
    OfflineBundle {
        minecraft: "1.21.1",
        loader: "21.1.248",
        file: "neoforge-21.1.248-offline.zip",
        sha256: "59e0e8a436f780f90da8ab54bf687cb4dfb4f85ff43057e5c4cd65e17b50b26b",
        size: 24_145_957,
    },
    OfflineBundle {
        minecraft: "1.20.1",
        loader: "47.1.79",
        file: "neoforge-1.20.1-47.1.79-offline.zip",
        sha256: "ab863b0e7b6f872e7fad80ee062e322771a4f8dc12bed2a47eb6407d9707251a",
        size: 18_678_629,
    },
];

fn offline_bundle(minecraft: &str, loader: &str) -> Option<&'static OfflineBundle> {
    OFFLINE_BUNDLES
        .iter()
        .find(|bundle| bundle.minecraft == minecraft && bundle.loader == loader)
}

/// The official installer URL followed by the same artifact on every mirror.
fn installer_candidates(url: &str) -> Vec<String> {
    let mut candidates = vec![url.to_owned()];
    for base in NEOFORGE_MIRRORS {
        if let Some(candidate) = mirrors::rewrite(url, base) {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

/// A library the NeoForge installer would fetch from its own Maven.
struct MavenArtifact {
    /// Path after `/releases/`, so it can be appended to any mirror base.
    relative: String,
    /// Destination below `libraries/`.
    path: String,
    sha1: String,
}

/// Reads the installer's own library list. Using the installer as the source
/// of truth means a new NeoForge release needs no launcher update.
fn installer_maven_artifacts(installer: &Path) -> Result<Vec<MavenArtifact>, String> {
    let file = std::fs::File::open(installer)
        .map_err(|error| format!("{}: {error}", installer.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| format!("архив установщика: {error}"))?;
    let mut json = String::new();
    for name in ["install_profile.json", "version.json"] {
        if let Ok(mut entry) = archive.by_name(name) {
            entry
                .read_to_string(&mut json)
                .map_err(|error| format!("{name}: {error}"))?;
            break;
        }
    }
    if json.is_empty() {
        return Err("установщик не содержит install_profile.json".to_owned());
    }
    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|error| format!("install_profile.json: {error}"))?;

    let mut artifacts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for library in value["libraries"].as_array().into_iter().flatten() {
        let artifact = &library["downloads"]["artifact"];
        let (Some(url), Some(path), Some(sha1)) = (
            artifact["url"].as_str(),
            artifact["path"].as_str(),
            artifact["sha1"].as_str(),
        ) else {
            continue;
        };
        if !url.contains("maven.neoforged.net") || !safe_library_path(path) {
            continue;
        }
        let Some((_, relative)) = url.split_once("/releases/") else {
            continue;
        };
        if !seen.insert(path.to_owned()) {
            continue;
        }
        artifacts.push(MavenArtifact {
            relative: relative.to_owned(),
            path: path.to_owned(),
            sha1: sha1.to_ascii_lowercase(),
        });
    }
    Ok(artifacts)
}

/// Rejects metadata paths that would land outside `libraries/`.
fn safe_library_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[derive(Debug, Error)]
pub enum LoaderError {
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
    #[error("некорректный JSON загрузчика {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("установщик {loader} завершился с кодом {code:?}: {tail}")]
    InstallerFailed {
        loader: String,
        code: Option<i32>,
        tail: String,
    },
    #[error("установка {0} отменена")]
    InstallerCancelled(String),
    #[error("установщик {loader} не завершился за {seconds} с и был остановлен")]
    InstallerTimedOut { loader: String, seconds: u64 },
    #[error(
        ".sha1/.sha256 для {url} недоступен ({reason}); загрузка установщика без проверки отменена"
    )]
    ChecksumUnavailable { url: String, reason: String },
    #[error("профиль загрузчика наследует {actual}, сборка требует {expected}")]
    ProfileMismatch { actual: String, expected: String },
    #[error("установщик {0} не создал version JSON")]
    MissingVersionJson(String),
    #[error("профиль загрузчика не найден: {0}")]
    MissingProfile(PathBuf),
    #[error("резервный набор NeoForge повреждён: {0}")]
    BundleInvalid(String),
    #[error("ни одно зеркало NeoForge не отдало библиотеки: {reason}")]
    MirrorsFailed { reason: String },
    #[error("небезопасный URL загрузчика (требуется HTTPS): {0}")]
    InsecureUrl(String),
    #[error("не удалось запустить установщик: {0}")]
    Spawn(String),
}

#[derive(Debug, Clone)]
pub struct LoaderInstaller {
    paths: PortablePaths,
    downloader: Downloader,
}

struct InstallerRequest<'a> {
    loader_name: &'a str,
    url: &'a str,
    cache: &'a Path,
    java_exe: &'a Path,
    version_id: &'a str,
    minecraft: &'a str,
    loader: &'a str,
    cancel: Option<CancelToken>,
}

struct Checksum {
    value: String,
    algo: HashAlgo,
}

impl LoaderInstaller {
    pub fn new(paths: PortablePaths, downloader: Downloader) -> Self {
        Self { paths, downloader }
    }

    /// Installs (or reuses) the requested loader and returns its profile JSON.
    /// The profile is not merged here; `Minecraft::prepare_launch` does that so
    /// the vanilla metadata stays authoritative for assets and the client jar.
    /// `cancel` must be checked so pressing "Отмена" during the up to five
    /// minutes of an installer run does not leave the UI stuck.
    pub async fn install(
        &self,
        mc_version: &str,
        spec: &LoaderSpec,
        java_exe: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
        cancel: Option<CancelToken>,
    ) -> Result<VersionJson, LoaderError> {
        let profile = match spec.kind {
            LoaderKind::Fabric => {
                let url = fabric_profile_url(mc_version, &spec.version);
                let cache = self.profile_cache("fabric", mc_version, &spec.version);
                self.fetch_profile(&url, &cache, progress).await?
            }
            LoaderKind::Quilt => {
                let url = quilt_profile_url(mc_version, &spec.version);
                let cache = self.profile_cache("quilt", mc_version, &spec.version);
                self.fetch_profile(&url, &cache, progress).await?
            }
            LoaderKind::Forge => {
                let url = forge_installer_url(mc_version, &spec.version);
                let cache = self.installer_cache("forge", mc_version, &spec.version);
                let version_id = format!("{mc_version}-forge-{}", spec.version);
                self.install_with_java(
                    InstallerRequest {
                        loader_name: "Forge",
                        url: &url,
                        cache: &cache,
                        java_exe,
                        version_id: &version_id,
                        minecraft: mc_version,
                        loader: &spec.version,
                        cancel: cancel.clone(),
                    },
                    progress,
                )
                .await?
            }
            LoaderKind::NeoForge => {
                let url = neoforge_installer_url(mc_version, &spec.version);
                let cache = self.installer_cache(
                    "neoforge",
                    mc_version,
                    neoforge_bare_version(mc_version, &spec.version),
                );
                let version_id = neoforge_version_id(mc_version, &spec.version);
                self.install_with_java(
                    InstallerRequest {
                        loader_name: "NeoForge",
                        url: &url,
                        cache: &cache,
                        java_exe,
                        version_id: &version_id,
                        minecraft: mc_version,
                        loader: neoforge_bare_version(mc_version, &spec.version),
                        cancel,
                    },
                    progress,
                )
                .await?
            }
        };
        validate_profile(&profile, mc_version)?;
        Ok(profile)
    }

    async fn fetch_profile(
        &self,
        url: &str,
        cache: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    ) -> Result<VersionJson, LoaderError> {
        require_https(url)?;
        if let Some(profile) = read_verified_profile(cache).await {
            tracing::debug!(path = %cache.display(), "using cached loader profile");
            return Ok(profile);
        }

        if let Some(parent) = cache.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| LoaderError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        let task = DownloadTask::new(
            url,
            cache.to_path_buf(),
            None,
            HashAlgo::Sha1,
            None,
            "loader profile",
        );
        self.downloader.download_many(vec![task], progress).await?;
        let profile = read_profile(cache)
            .await
            .ok_or_else(|| LoaderError::MissingProfile(cache.to_path_buf()))?;
        write_profile_hash(cache).await;
        Ok(profile)
    }

    async fn install_with_java(
        &self,
        request: InstallerRequest<'_>,
        progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    ) -> Result<VersionJson, LoaderError> {
        let InstallerRequest {
            loader_name,
            url,
            cache,
            java_exe,
            version_id,
            minecraft,
            loader,
            cancel,
        } = request;
        require_https(url)?;
        // The installer also writes its JSON into versions/. Reuse it when it is
        // already there, which makes subsequent launches network-free. The JSON
        // alone is not proof of a finished install: Forge and NeoForge patch the
        // client jar right before writing it, so an interrupted install leaves a
        // profile that looks complete but cannot be launched.
        if let Some(profile) = self.locate_version_json(version_id).await {
            if self.loader_payload_installed(loader_name, minecraft, loader) {
                tracing::debug!(path = %profile.display(), "using installed loader profile");
                return read_profile_required(&profile).await;
            }
            tracing::warn!(
                path = %profile.display(),
                loader = loader_name,
                "loader profile has no patched client jar; reinstalling"
            );
            if let Err(source) = tokio::fs::remove_file(&profile).await {
                tracing::warn!(path = %profile.display(), %source, "cannot drop stale loader profile");
            }
        }

        // Only NeoForge has pinned mirrors and an offline archive today; Forge
        // keeps the single-source behaviour.
        let bundle = offline_bundle(minecraft, loader);
        let mut supplied_locally = false;

        // A release build carries the archive inside the executable. Using it
        // first makes the loader installation independent of every mirror: the
        // installer then only talks to Mojang, which is reachable far more
        // often than the NeoForge Maven.
        if let Some(bundle) = bundle.filter(|bundle| embedded_bundle(bundle.file).is_some()) {
            self.install_bundle(bundle, cache, progress.clone()).await?;
            supplied_locally = true;
        } else if !cache.is_file() {
            if let Err(error) = self
                .download_installer(loader_name, url, cache, progress.clone())
                .await
            {
                let Some(bundle) = bundle else {
                    return Err(error);
                };
                tracing::warn!(
                    url,
                    loader = loader_name,
                    %error,
                    "no installer host answered; switching to the offline NeoForge mirror"
                );
                self.install_bundle(bundle, cache, progress.clone()).await?;
                supplied_locally = true;
            }
        }

        if let Some(bundle) = bundle.filter(|_| !supplied_locally) {
            // A cached installer does not prove that Maven is reachable, and the
            // installer itself would stall for minutes on a blocked host. One
            // tiny request decides whether the libraries must come from a mirror
            // before Java is even started.
            let available = matches!(
                probe_checksum(&self.downloader, &format!("{url}.sha1"), HashAlgo::Sha1).await,
                ChecksumProbe::Found(_)
            );
            if !available {
                tracing::warn!(
                    url,
                    loader = loader_name,
                    "NeoForge Maven is unreachable; trying the public mirrors"
                );
                match self
                    .prefetch_libraries_from_mirrors(cache, loader, progress.clone())
                    .await
                {
                    Ok(()) => supplied_locally = true,
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            loader = loader_name,
                            "no NeoForge mirror served the libraries; using the offline bundle"
                        );
                        self.install_bundle(bundle, cache, progress.clone()).await?;
                        supplied_locally = true;
                    }
                }
            }
        }

        if let Err(error) = self
            .run_installer(loader_name, cache, java_exe, cancel.clone())
            .await
        {
            // The jar is cached now, so a failure here is almost always the
            // installer stalling on a blocked Maven host. Retry once with the
            // verified libraries already in place.
            let retry = !supplied_locally
                && bundle.is_some()
                && matches!(
                    error,
                    LoaderError::InstallerFailed { .. } | LoaderError::InstallerTimedOut { .. }
                );
            if !retry {
                return Err(error);
            }
            tracing::warn!(
                loader = loader_name,
                "installer failed; retrying with mirrored libraries"
            );
            if self
                .prefetch_libraries_from_mirrors(cache, loader, progress.clone())
                .await
                .is_err()
            {
                let bundle = bundle.expect("checked above");
                self.install_bundle(bundle, cache, progress).await?;
            }
            self.run_installer(loader_name, cache, java_exe, cancel)
                .await?;
        }

        if let Some(profile) = self.locate_version_json(version_id).await {
            return read_profile_required(&profile).await;
        }

        // Some installer versions ignore the optional directory argument and
        // write into the default `%APPDATA%\.minecraft` instead. Copy that JSON
        // into our portable tree; pml-minecraft will fetch its libraries itself.
        if let Some(default_versions) = default_minecraft_versions() {
            if let Some(profile) = find_version_json(&default_versions, version_id).await {
                let file_name = format!("{version_id}.json");
                let dest = self.paths.versions_dir().join(version_id).join(&file_name);
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|source| LoaderError::Io {
                            path: parent.to_path_buf(),
                            source,
                        })?;
                }
                tokio::fs::copy(&profile, &dest)
                    .await
                    .map_err(|source| LoaderError::Io {
                        path: dest.clone(),
                        source,
                    })?;
                tracing::info!(
                    from = %profile.display(),
                    to = %dest.display(),
                    "copied loader profile from the default .minecraft"
                );
                return read_profile_required(&dest).await;
            }
        }

        Err(LoaderError::MissingVersionJson(format!(
            "{loader_name} {version_id}"
        )))
    }

    /// Fetches the installer with its published checksum. A missing checksum
    /// aborts the download: an unverified executable is never acceptable.
    ///
    /// The NeoForge Maven is also published behind Forge's CDN, and some
    /// providers only break one of the two, so every host in
    /// [`NEOFORGE_MIRRORS`] is tried before the caller falls back to the
    /// bundled archive.
    /// Downloads the installer with every known mirror attached as a fallback.
    ///
    /// The checksum is taken from the first host that publishes it, then one
    /// download task covers all hosts: the `.part` survives a mirror dying in
    /// the middle of the jar, so a dropped connection costs the missing tail
    /// instead of the whole file.
    async fn download_installer(
        &self,
        loader_name: &str,
        url: &str,
        cache: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    ) -> Result<(), LoaderError> {
        let candidates = installer_candidates(url);
        let mut checksum = None;
        let mut last_reason = None;
        for candidate in &candidates {
            match fetch_checksum(&self.downloader, candidate).await {
                Ok(found) => {
                    checksum = Some(found);
                    break;
                }
                Err(reason) => {
                    tracing::warn!(
                        url = candidate,
                        loader = loader_name,
                        %reason,
                        "installer checksum is unavailable on this host"
                    );
                    last_reason = Some(reason);
                }
            }
        }
        let Some(checksum) = checksum else {
            // An unverified installer is never executed.
            return Err(LoaderError::ChecksumUnavailable {
                url: url.to_owned(),
                reason: last_reason.unwrap_or_else(|| {
                    "ни одно зеркало не отдало контрольную сумму установщика".to_owned()
                }),
            });
        };
        if let Some(parent) = cache.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| LoaderError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        tracing::debug!(
            url,
            algo = checksum_extension(checksum.algo),
            "verified installer download"
        );
        let task = DownloadTask::new(
            url,
            cache.to_path_buf(),
            Some(checksum.value),
            checksum.algo,
            None,
            format!("{loader_name} installer"),
        )
        .with_alternates(candidates);
        self.downloader.download_many(vec![task], progress).await?;
        Ok(())
    }

    /// Runs the headless installer. Split from `install_with_java` so the
    /// offline fallback can retry it after prefetching the libraries.
    async fn run_installer(
        &self,
        loader_name: &str,
        cache: &Path,
        java_exe: &Path,
        cancel: Option<CancelToken>,
    ) -> Result<(), LoaderError> {
        // A cancelled run must never spawn the installer in the first place.
        if cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(LoaderError::InstallerCancelled(loader_name.to_owned()));
        }

        let mut command = tokio::process::Command::new(java_exe);
        // Forge and NeoForge installers refuse to run unless the target looks
        // like a Minecraft installation. A minimal launcher profile is enough;
        // our own launcher never reads it back.
        self.ensure_launcher_profiles().await?;
        command
            .arg("-jar")
            .arg(cache)
            .arg("--installClient")
            .arg(self.paths.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        hide_console(&mut command);

        let output = match tokio::time::timeout(INSTALLER_TIMEOUT, async {
            tokio::select! {
                result = command.output() => result.map_err(|source| {
                    LoaderError::Spawn(format!("{}: {source}", java_exe.display()))
                }),
                _ = wait_for_cancel(cancel.as_ref()) => {
                    Err(LoaderError::InstallerCancelled(loader_name.to_owned()))
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    loader = loader_name,
                    seconds = INSTALLER_TIMEOUT.as_secs(),
                    "loader installer timed out and was killed"
                );
                return Err(LoaderError::InstallerTimedOut {
                    loader: loader_name.to_owned(),
                    seconds: INSTALLER_TIMEOUT.as_secs(),
                });
            }
        };
        let output = match output {
            Ok(output) => output,
            // Dropping the `output()` future kills the child (`kill_on_drop`).
            Err(error) => return Err(error),
        };
        if !output.status.success() {
            return Err(LoaderError::InstallerFailed {
                loader: loader_name.to_owned(),
                code: output.status.code(),
                tail: tail(&format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )),
            });
        }
        Ok(())
    }

    /// Lays the offline mirror out where the installer expects it.
    ///
    /// A release build embeds the archive, so this becomes a purely local
    /// operation; otherwise the same archive is downloaded and hash-checked by
    /// the downloader. Every extracted file is verified against the SHA-1 list
    /// in the bundle manifest.
    async fn install_bundle(
        &self,
        bundle: &OfflineBundle,
        cache: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    ) -> Result<(), LoaderError> {
        let loaders = self.paths.loaders_dir();
        tokio::fs::create_dir_all(&loaders)
            .await
            .map_err(|source| LoaderError::Io {
                path: loaders.clone(),
                source,
            })?;
        let archive = loaders.join(bundle.file);

        if let Some(bytes) = embedded_bundle(bundle.file) {
            // The archive travels inside the executable, so a truncated or
            // tampered build must fail here instead of inside the extractor.
            let digest = pml_downloader::hash_bytes(HashAlgo::Sha256, bytes);
            if bytes.len() as u64 != bundle.size || !digest.eq_ignore_ascii_case(bundle.sha256) {
                return Err(LoaderError::BundleInvalid(format!(
                    "встроенный архив {} повреждён: {} байт, SHA-256 {digest}",
                    bundle.file,
                    bytes.len()
                )));
            }
            tracing::info!(
                file = bundle.file,
                bytes = bytes.len(),
                "using the embedded NeoForge offline bundle"
            );
            let part = archive.with_extension("zip.part");
            tokio::fs::write(&part, bytes)
                .await
                .map_err(|source| LoaderError::Io {
                    path: part.clone(),
                    source,
                })?;
            tokio::fs::rename(&part, &archive)
                .await
                .map_err(|source| LoaderError::Io {
                    path: archive.clone(),
                    source,
                })?;
        } else {
            let url = format!("{NEOFORGE_MIRROR_BASE}/{}", bundle.file);
            let alternates: Vec<String> = GITHUB_PROXIES
                .iter()
                .map(|proxy| format!("{proxy}{url}"))
                .collect();
            tracing::info!(url, "fetching the NeoForge offline mirror");
            let task = DownloadTask::new(
                url,
                archive.clone(),
                Some(bundle.sha256.to_owned()),
                HashAlgo::Sha256,
                Some(bundle.size),
                "NeoForge offline mirror",
            )
            .with_alternates(alternates);
            self.downloader.download_many(vec![task], progress).await?;
        }

        let installer = cache.to_path_buf();
        let libraries = self.paths.libraries_dir();
        tokio::task::spawn_blocking(move || extract_bundle(&archive, &installer, &libraries))
            .await
            .map_err(|error| LoaderError::BundleInvalid(format!("распаковка прервана: {error}")))?
            .map_err(LoaderError::BundleInvalid)
    }

    /// Downloads the artifacts the installer would take from the NeoForge
    /// Maven, each file from the first mirror that serves it.
    ///
    /// The list comes from the installer's own `install_profile.json`, so a new
    /// NeoForge release needs no changes here. Mojang-hosted libraries are
    /// skipped: the installer fetches those itself.
    async fn prefetch_libraries_from_mirrors(
        &self,
        cache: &Path,
        loader: &str,
        progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    ) -> Result<(), LoaderError> {
        let installer = cache.to_path_buf();
        let artifacts = tokio::task::spawn_blocking(move || installer_maven_artifacts(&installer))
            .await
            .map_err(|error| LoaderError::MirrorsFailed {
                reason: format!("чтение установщика прервано: {error}"),
            })?
            .map_err(|reason| LoaderError::MirrorsFailed { reason })?;
        if artifacts.is_empty() {
            return Err(LoaderError::MirrorsFailed {
                reason: "в установщике нет библиотек с maven.neoforged.net".to_owned(),
            });
        }

        // Probes run in parallel: several hosts that all time out would
        // otherwise delay the install by one timeout each. The canonical host
        // is skipped because the caller only gets here when it is unreachable.
        let mut probes = tokio::task::JoinSet::new();
        for base in NEOFORGE_MIRRORS.iter().skip(1) {
            let sidecar = format!(
                "{base}/net/neoforged/neoforge/{loader}/neoforge-{loader}-installer.jar.sha1"
            );
            let downloader = self.downloader.clone();
            let base = *base;
            probes.spawn(async move {
                let found = matches!(
                    probe_checksum(&downloader, &sidecar, HashAlgo::Sha1).await,
                    ChecksumProbe::Found(_)
                );
                (base, found)
            });
        }
        // The first host to answer goes first: on a bad connection that is
        // also the host most likely to finish the transfer.
        let mut usable = Vec::new();
        while let Some(result) = probes.join_next().await {
            match result {
                Ok((base, true)) => usable.push(base),
                Ok((_, false)) => {}
                Err(error) => tracing::warn!(%error, "mirror probe task failed"),
            }
        }
        if usable.is_empty() {
            return Err(LoaderError::MirrorsFailed {
                reason: "ни одно зеркало NeoForge не отвечает".to_owned(),
            });
        }
        tracing::info!(mirrors = ?usable, "preloading NeoForge libraries from a mirror");

        let libraries = self.paths.libraries_dir();
        let mut downloaded = 0;
        for artifact in artifacts {
            let destination = libraries.join(Path::new(&artifact.path));
            let label = artifact
                .path
                .rsplit('/')
                .next()
                .unwrap_or("библиотека")
                .to_owned();
            // One task per file with every mirror as a fallback: a host that
            // dies halfway through a 20 MB library is resumed on the next one.
            let task = DownloadTask::new(
                format!("{}/{}", usable[0], artifact.relative),
                destination,
                Some(artifact.sha1.clone()),
                HashAlgo::Sha1,
                None,
                label,
            )
            .with_alternates(
                usable
                    .iter()
                    .skip(1)
                    .map(|base| format!("{base}/{}", artifact.relative)),
            );
            self.downloader
                .download_many(vec![task], progress.clone())
                .await
                .map_err(|error| LoaderError::MirrorsFailed {
                    reason: error.to_string(),
                })?;
            downloaded += 1;
        }
        tracing::info!(files = downloaded, "NeoForge libraries preloaded");
        Ok(())
    }

    /// Creates the placeholder `launcher_profiles.json` expected by the
    /// Forge/NeoForge installers. Existing files are left untouched so that a
    /// real Minecraft launcher profile is never overwritten.
    async fn ensure_launcher_profiles(&self) -> Result<(), LoaderError> {
        let path = self.paths.root().join("launcher_profiles.json");
        if path.is_file() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| LoaderError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        const MINIMAL_PROFILE: &str =
            "{\"profiles\":{},\"settings\":{\"language\":\"ru_ru\"},\"version\":3}";
        tokio::fs::write(&path, MINIMAL_PROFILE)
            .await
            .map_err(|source| LoaderError::Io { path, source })
    }

    async fn locate_version_json(&self, version_id: &str) -> Option<PathBuf> {
        find_version_json(&self.paths.versions_dir(), version_id).await
    }

    /// Checks for the patched client jar that the Forge/NeoForge installer
    /// produces next to the version JSON. Loaders that only ship a profile
    /// (Fabric, Quilt) have no such jar and are always considered complete.
    fn loader_payload_installed(&self, loader_name: &str, mc_version: &str, loader: &str) -> bool {
        if !loader_name.to_ascii_lowercase().contains("forge") {
            return true;
        }
        let libraries = self.paths.libraries_dir();
        let legacy = format!("{mc_version}-{loader}");
        [
            libraries
                .join("net/neoforged/neoforge")
                .join(loader)
                .join(format!("neoforge-{loader}-client.jar")),
            libraries
                .join("net/neoforged/forge")
                .join(&legacy)
                .join(format!("forge-{legacy}-client.jar")),
            libraries
                .join("net/minecraftforge/forge")
                .join(&legacy)
                .join(format!("forge-{legacy}-client.jar")),
        ]
        .iter()
        .any(|path| path.is_file())
    }

    fn profile_cache(&self, loader: &str, mc_version: &str, version: &str) -> PathBuf {
        self.paths
            .loaders_dir()
            .join(format!("{loader}-{mc_version}-{version}.json"))
    }

    fn installer_cache(&self, loader: &str, mc_version: &str, version: &str) -> PathBuf {
        self.paths
            .loaders_dir()
            .join(format!("{loader}-{mc_version}-{version}-installer.jar"))
    }
}

pub fn fabric_profile_url(mc_version: &str, loader_version: &str) -> String {
    format!("{FABRIC_META}/{mc_version}/{loader_version}/profile/json")
}

pub fn quilt_profile_url(mc_version: &str, loader_version: &str) -> String {
    format!("{QUILT_META}/{mc_version}/{loader_version}/profile/json")
}

pub fn forge_installer_url(mc_version: &str, loader_version: &str) -> String {
    format!(
        "{FORGE_MAVEN}/{mc_version}-{loader_version}/forge-{mc_version}-{loader_version}-installer.jar"
    )
}

/// NeoForge 1.20.1 is published as `net.neoforged:forge`, its version carries
/// the Minecraft prefix and its installer writes a `forge` profile id.
fn neoforge_legacy(mc_version: &str, loader_version: &str) -> bool {
    mc_version == "1.20.1" || loader_version.starts_with("47.")
}

/// Modrinth packs are inconsistent about the version prefix (`47.1.79` versus
/// `1.20.1-47.1.79`), so normalize it before building URLs or profile ids.
fn neoforge_bare_version<'a>(mc_version: &str, loader_version: &'a str) -> &'a str {
    loader_version
        .strip_prefix(mc_version)
        .and_then(|rest| rest.strip_prefix('-'))
        .unwrap_or(loader_version)
}

pub fn neoforge_installer_url(mc_version: &str, loader_version: &str) -> String {
    let version = neoforge_bare_version(mc_version, loader_version);
    // NeoForge used the Forge coordinates for its 1.20.1 line, and only there
    // the version carries the Minecraft prefix. Everything newer lives under
    // `net.neoforged:neoforge` with the plain loader version.
    if neoforge_legacy(mc_version, version) {
        format!(
            "{NEOFORGE_LEGACY_MAVEN}/{mc_version}-{version}/forge-{mc_version}-{version}-installer.jar"
        )
    } else {
        format!("{NEOFORGE_MAVEN}/{version}/neoforge-{version}-installer.jar")
    }
}

/// The version folder the NeoForge installer creates. The 1.20.1 installer is
/// the Forge one and names the profile `1.20.1-forge-47.x`.
pub fn neoforge_version_id(mc_version: &str, loader_version: &str) -> String {
    let version = neoforge_bare_version(mc_version, loader_version);
    if neoforge_legacy(mc_version, version) {
        format!("{mc_version}-forge-{version}")
    } else {
        format!("neoforge-{version}")
    }
}

/// A profile that claims another Minecraft base would surface as strange
/// library paths deep inside pml-minecraft, far from the real cause. Check it
/// right where the profile is obtained.
fn validate_profile(profile: &VersionJson, mc_version: &str) -> Result<(), LoaderError> {
    match profile.inherits_from.as_deref() {
        Some(actual) if actual != mc_version => Err(LoaderError::ProfileMismatch {
            actual: actual.to_owned(),
            expected: mc_version.to_owned(),
        }),
        _ => Ok(()),
    }
}

/// Resolves when the token is cancelled; without a token it never resolves.
/// Used as the cancellation arm of the `select!` around the installer run.
async fn wait_for_cancel(cancel: Option<&CancelToken>) {
    match cancel {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// Loader metadata and installers are executable content; a plaintext HTTP
/// endpoint would let anyone on the path swap them. Every loader URL is
/// hard-coded to HTTPS, so a mismatch is a bug or an attack.
fn require_https(url: &str) -> Result<(), LoaderError> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(LoaderError::InsecureUrl(url.to_owned()))
    }
}

fn profile_hash_path(cache: &Path) -> PathBuf {
    let mut name = cache.file_name().unwrap_or_default().to_os_string();
    name.push(".sha256");
    cache.with_file_name(name)
}

/// Reads a cached profile only when its sidecar digest matches. A missing
/// sidecar (a cache written by an older build) is accepted and upgraded on the
/// next successful read; a mismatching one is treated as corrupt.
async fn read_verified_profile(cache: &Path) -> Option<VersionJson> {
    let bytes = tokio::fs::read(cache).await.ok()?;
    let profile: VersionJson = serde_json::from_slice(&bytes).ok()?;
    match tokio::fs::read_to_string(profile_hash_path(cache)).await {
        Ok(expected) => {
            let actual = pml_downloader::hash_bytes(HashAlgo::Sha256, &bytes);
            if expected.trim().eq_ignore_ascii_case(&actual) {
                Some(profile)
            } else {
                tracing::warn!(path = %cache.display(), "cached loader profile digest mismatch");
                None
            }
        }
        Err(_) => {
            write_profile_hash(cache).await;
            Some(profile)
        }
    }
}

async fn write_profile_hash(cache: &Path) {
    let Ok(bytes) = tokio::fs::read(cache).await else {
        return;
    };
    let hash = pml_downloader::hash_bytes(HashAlgo::Sha256, &bytes);
    if let Err(error) = tokio::fs::write(profile_hash_path(cache), hash).await {
        tracing::debug!(%error, path = %cache.display(), "failed to write profile digest");
    }
}

async fn read_profile(path: &Path) -> Option<VersionJson> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn read_profile_required(path: &Path) -> Result<VersionJson, LoaderError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|source| LoaderError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    serde_json::from_slice(&bytes).map_err(|source| LoaderError::Json {
        path: path.to_path_buf(),
        source,
    })
}

async fn find_version_json(root: &Path, version_id: &str) -> Option<PathBuf> {
    let direct = root.join(version_id).join(format!("{version_id}.json"));
    if direct.is_file() {
        return Some(direct);
    }

    // Only an exact `{version_id}.json` name is accepted: guessing by loader
    // marker used to pick an unrelated profile and could start the wrong
    // version.
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                %error,
                root = %root.display(),
                "cannot list the versions directory"
            );
            return None;
        }
    };
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let directory = entry.path();
                if !directory.is_dir() {
                    continue;
                }
                let file = directory.join(format!("{version_id}.json"));
                if file.is_file() {
                    return Some(file);
                }
            }
            Ok(None) => break,
            // One unreadable entry (permissions, `\\?\` quirks) must not stop
            // the rest of the scan: warn and move on to the next directory.
            Err(error) => {
                tracing::warn!(
                    %error,
                    root = %root.display(),
                    "skipping an unreadable version directory"
                );
                continue;
            }
        }
    }
    tracing::debug!(
        version_id,
        root = %root.display(),
        "loader profile with the exact version id was not found"
    );
    None
}

// The whole purpose of this fallback is `%APPDATA%\.minecraft`; on other
// systems there is no such default location to borrow a profile from.
#[cfg(windows)]
fn default_minecraft_versions() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join(".minecraft").join("versions"))
}

#[cfg(not(windows))]
fn default_minecraft_versions() -> Option<PathBuf> {
    None
}

/// Fetches the checksum sidecar for an installer.
///
/// Maven publishes `.sha1` for every artifact; `.sha256` is the fallback for
/// mirrors that only carry stronger digests. A transient network error is
/// retried, but a missing checksum still aborts the install: running an
/// unverified executable is never acceptable.
///
/// The failure reason is propagated because the two realistic causes need
/// completely different fixes: a TLS error usually means an outdated root store
/// on the client, while a timeout points at a blocked network.
async fn fetch_checksum(downloader: &Downloader, url: &str) -> Result<Checksum, String> {
    let mut reason = String::from("нет ответа");
    for algo in [HashAlgo::Sha1, HashAlgo::Sha256] {
        let extension = checksum_extension(algo);
        let sidecar = format!("{url}.{extension}");
        for attempt in 0..CHECKSUM_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(CHECKSUM_RETRY_DELAY * attempt).await;
            }
            match probe_checksum(downloader, &sidecar, algo).await {
                ChecksumProbe::Found(value) => return Ok(Checksum { value, algo }),
                // A 404 will not heal within a retry; try the next digest.
                ChecksumProbe::Missing => {
                    reason = format!("HTTP 404 для .{extension}");
                    break;
                }
                ChecksumProbe::Failed(detail) => {
                    tracing::warn!(
                        url = sidecar,
                        attempt = attempt + 1,
                        %detail,
                        "installer checksum request failed"
                    );
                    reason = detail;
                    continue;
                }
            }
        }
    }
    Err(reason)
}

enum ChecksumProbe {
    Found(String),
    Missing,
    Failed(String),
}

async fn probe_checksum(downloader: &Downloader, url: &str, algo: HashAlgo) -> ChecksumProbe {
    let response = match downloader.client().get(url).send().await {
        Ok(response) => response,
        Err(error) => return ChecksumProbe::Failed(describe_request_error(&error)),
    };
    // Some mirrors answer 403/429 under load; those are worth another try,
    // unlike a plain 404.
    match response.status().as_u16() {
        404 | 410 => return ChecksumProbe::Missing,
        status if !(200..300).contains(&status) => {
            return ChecksumProbe::Failed(format!("HTTP {status}"));
        }
        _ => {}
    }
    let text = match response.text().await {
        Ok(text) => text,
        Err(error) => return ChecksumProbe::Failed(describe_request_error(&error)),
    };
    // Maven sidecars are either a bare digest or `<digest>  <file name>`.
    let hash = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if hash.len() == algo.hex_len() && hash.chars().all(|character| character.is_ascii_hexdigit()) {
        ChecksumProbe::Found(hash)
    } else {
        // A truncated or HTML body is not a checksum; retrying can help when a
        // proxy injected an error page.
        ChecksumProbe::Failed("ответ не похож на контрольную сумму".to_owned())
    }
}

/// Turns a reqwest failure into a short, user-facing cause. TLS errors get
/// their own wording: they mean the certificate chain was rejected, not that
/// the host is down, and users can act on that.
pub(crate) fn describe_request_error(error: &reqwest::Error) -> String {
    let text = format!("{error}");
    let lower = text.to_ascii_lowercase();
    if lower.contains("certificate") || lower.contains("tls") || lower.contains("unknownissuer") {
        return format!("TLS: {text}");
    }
    if error.is_timeout() {
        return "таймаут соединения".to_owned();
    }
    if error.is_connect() {
        return format!("нет соединения: {text}");
    }
    text
}

fn checksum_extension(algo: HashAlgo) -> &'static str {
    match algo {
        HashAlgo::Sha1 => "sha1",
        HashAlgo::Sha256 => "sha256",
        HashAlgo::Sha512 => "sha512",
    }
}

fn tail(text: &str) -> String {
    let lines = text.lines().rev().take(12).collect::<Vec<_>>();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Unpacks a verified offline bundle into the layout the installer expects.
///
/// The archive is pinned by SHA-256 in `OFFLINE_BUNDLES`, so a malicious entry
/// cannot get in unnoticed. Path traversal is still rejected explicitly, and
/// every file is checked against the SHA-1 recorded in `manifest.json`; a bundle
/// that fails either check is treated as corrupt rather than trusted.
fn extract_bundle(archive: &Path, installer: &Path, libraries: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive)
        .map_err(|error| format!("не открыть {}: {error}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| format!("не читать zip: {error}"))?;

    let manifest: serde_json::Value = {
        let mut entry = zip
            .by_name("manifest.json")
            .map_err(|_| "в архиве нет manifest.json".to_owned())?;
        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .map_err(|error| format!("не прочитать manifest.json: {error}"))?;
        serde_json::from_str(&text)
            .map_err(|error| format!("некорректный manifest.json: {error}"))?
    };

    let installer_sha1 = manifest["installer"]["sha1"]
        .as_str()
        .filter(|value| value.len() == 40)
        .ok_or_else(|| "manifest.json не содержит SHA-1 установщика".to_owned())?
        .to_ascii_lowercase();

    let mut expected = std::collections::HashMap::new();
    for item in manifest["files"].as_array().into_iter().flatten() {
        let (Some(path), Some(sha1)) = (item["path"].as_str(), item["sha1"].as_str()) else {
            return Err("manifest.json содержит запись без пути или SHA-1".to_owned());
        };
        expected.insert(path.to_owned(), sha1.to_ascii_lowercase());
    }
    if expected.is_empty() {
        return Err("manifest.json не содержит списка библиотек".to_owned());
    }

    let mut seen_installer = false;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| format!("не открыть запись {index}: {error}"))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        if name == "manifest.json" {
            continue;
        }
        if name != "installer.jar" && !name.starts_with("libraries/") {
            return Err(format!("неожиданный файл в архиве: {name}"));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("не прочитать {name}: {error}"))?;
        let digest = pml_downloader::hash_bytes(HashAlgo::Sha1, &bytes);
        if name == "installer.jar" {
            if digest != installer_sha1 {
                return Err("SHA-1 установщика не совпал".to_owned());
            }
            write_verified(installer, &bytes)?;
            seen_installer = true;
            continue;
        }
        match expected.get(&name) {
            Some(expected_sha1) if *expected_sha1 == digest => {}
            Some(_) => return Err(format!("SHA-1 не совпал: {name}")),
            None => return Err(format!("файл отсутствует в манифесте: {name}")),
        }
        let relative = safe_relative(&name)?;
        write_verified(&libraries.join(relative), &bytes)?;
    }

    if !seen_installer {
        return Err("в архиве нет installer.jar".to_owned());
    }
    Ok(())
}

/// Turns `libraries/a/b.jar` into a relative path, rejecting anything that
/// could escape the libraries directory.
fn safe_relative(name: &str) -> Result<PathBuf, String> {
    let rest = name
        .strip_prefix("libraries/")
        .ok_or_else(|| format!("неверный путь в архиве: {name}"))?;
    let mut path = PathBuf::new();
    for part in rest.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\\')
            || part.contains(':')
        {
            return Err(format!("небезопасный путь в архиве: {name}"));
        }
        path.push(part);
    }
    Ok(path)
}

/// Writes a file atomically. An existing identical file is kept untouched so a
/// repeated fallback does not rewrite a few hundred megabytes of libraries.
fn write_verified(destination: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Ok(existing) = std::fs::read(destination) {
        if existing == bytes {
            return Ok(());
        }
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("не создать {}: {error}", parent.display()))?;
    }
    let temporary = destination.with_extension("part");
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("не записать {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, destination)
        .map_err(|error| format!("не переименовать {}: {error}", destination.display()))
}

#[cfg(windows)]
fn hide_console(command: &mut tokio::process::Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut tokio::process::Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fabric_url_is_stable() {
        assert_eq!(
            fabric_profile_url("1.21.1", "0.16.5"),
            "https://meta.fabricmc.net/v2/versions/loader/1.21.1/0.16.5/profile/json"
        );
    }

    #[test]
    fn quilt_url_is_stable() {
        assert_eq!(
            quilt_profile_url("1.21.1", "0.27.0"),
            "https://meta.quiltmc.org/v3/versions/loader/1.21.1/0.27.0/profile/json"
        );
    }

    #[test]
    fn forge_urls_are_stable() {
        assert_eq!(
            forge_installer_url("1.21.1", "52.0.28"),
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.0.28/forge-1.21.1-52.0.28-installer.jar"
        );
        assert_eq!(
            neoforge_installer_url("1.21.1", "21.1.248"),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.248/neoforge-21.1.248-installer.jar"
        );
        // NeoForge 1.20.1 lives under the Forge coordinates and its version
        // carries the Minecraft prefix.
        assert_eq!(
            neoforge_installer_url("1.20.1", "47.1.79"),
            "https://maven.neoforged.net/releases/net/neoforged/forge/1.20.1-47.1.79/forge-1.20.1-47.1.79-installer.jar"
        );
        // A 47.x loader version is enough to detect the legacy layout even when
        // the Minecraft version is passed in an unusual shape.
        assert_eq!(
            neoforge_installer_url("1.20.1", "47.1.82"),
            "https://maven.neoforged.net/releases/net/neoforged/forge/1.20.1-47.1.82/forge-1.20.1-47.1.82-installer.jar"
        );
        // Modrinth packs sometimes repeat the Minecraft prefix in the loader
        // version; it must not be duplicated in the path.
        assert_eq!(
            neoforge_installer_url("1.20.1", "1.20.1-47.1.79"),
            "https://maven.neoforged.net/releases/net/neoforged/forge/1.20.1-47.1.79/forge-1.20.1-47.1.79-installer.jar"
        );
        assert_eq!(
            neoforge_installer_url("1.21.1", "1.21.1-21.1.248"),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.248/neoforge-21.1.248-installer.jar"
        );
    }

    #[test]
    fn neoforge_profile_ids_match_the_installer() {
        // Verified against the `install_profile.json` shipped inside both jars.
        assert_eq!(
            neoforge_version_id("1.21.1", "21.1.248"),
            "neoforge-21.1.248"
        );
        assert_eq!(
            neoforge_version_id("1.20.1", "47.1.79"),
            "1.20.1-forge-47.1.79"
        );
        assert_eq!(
            neoforge_version_id("1.20.1", "1.20.1-47.1.79"),
            "1.20.1-forge-47.1.79"
        );
    }

    #[test]
    fn parses_fabric_profile() {
        let json = br#"{
            "id": "fabric-loader-0.16.5-1.21.1",
            "inheritsFrom": "1.21.1",
            "type": "release",
            "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
            "libraries": [
                {"name": "net.fabricmc:fabric-loader:0.16.5", "url": "https://maven.fabricmc.net/", "sha1": "0000000000000000000000000000000000000000", "size": 1}
            ]
        }"#;
        let profile: VersionJson = serde_json::from_slice(json).expect("profile");
        assert_eq!(profile.id, "fabric-loader-0.16.5-1.21.1");
        assert_eq!(profile.inherits_from.as_deref(), Some("1.21.1"));
        assert_eq!(
            profile.main_class.as_deref(),
            Some("net.fabricmc.loader.impl.launch.knot.KnotClient")
        );
        assert_eq!(profile.libraries.len(), 1);
    }

    #[test]
    fn tail_keeps_last_lines() {
        let text = (0..20)
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail(&text);
        assert!(tail.starts_with("8\n"));
        assert!(tail.ends_with("\n19"));
    }

    #[tokio::test]
    async fn profile_lookup_requires_the_exact_id() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let versions = directory.path().join("versions");
        std::fs::create_dir_all(versions.join("forge-other"))?;
        std::fs::write(
            versions.join("forge-other").join("other.json"),
            br#"{"id":"1.21.1-forge-99.0.0","inheritsFrom":"1.21.1"}"#,
        )?;

        assert!(find_version_json(&versions, "1.21.1-forge-52.0.28")
            .await
            .is_none());

        std::fs::create_dir_all(versions.join("exact"))?;
        let exact = versions.join("exact").join("1.21.1-forge-52.0.28.json");
        std::fs::write(&exact, br#"{"id":"1.21.1-forge-52.0.28"}"#)?;
        assert_eq!(
            find_version_json(&versions, "1.21.1-forge-52.0.28").await,
            Some(exact)
        );
        Ok(())
    }

    /// Hits the real Maven repository. Run with `cargo test -p pml-loader
    /// -- --ignored` when the network path around checksums changes.
    #[tokio::test]
    #[ignore = "network test: reaches maven.neoforged.net"]
    async fn real_neoforge_checksums_are_reachable() -> Result<(), Box<dyn std::error::Error>> {
        let downloader = Downloader::new(2)?;
        for (mc_version, loader_version) in [("1.21.1", "21.1.248"), ("1.20.1", "47.1.79")] {
            let url = neoforge_installer_url(mc_version, loader_version);
            let checksum = fetch_checksum(&downloader, &url)
                .await
                .unwrap_or_else(|reason| panic!("no checksum for {url}: {reason}"));
            assert_eq!(checksum.algo, HashAlgo::Sha1, "{url}");
            assert_eq!(checksum.value.len(), 40, "{url}");
            assert!(
                checksum.value.chars().all(|c| c.is_ascii_hexdigit()),
                "{url}"
            );
        }
        Ok(())
    }

    #[test]
    fn offline_bundles_are_looked_up_by_the_bare_version() {
        for (minecraft, loader) in [("1.21.1", "21.1.248"), ("1.20.1", "47.1.79")] {
            let bundle = offline_bundle(minecraft, loader).expect("supported version");
            assert!(bundle.file.starts_with("neoforge-"), "{}", bundle.file);
            assert_eq!(bundle.sha256.len(), 64);
            assert!(bundle.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(bundle.size > 1_000_000, "{}", bundle.file);
        }
        assert!(offline_bundle("1.21.1", "21.1.999").is_none());
        // Legacy packs spell the loader as `1.20.1-47.1.79`; the bundle key is
        // the bare version the installer reports.
        assert!(offline_bundle("1.20.1", "1.20.1-47.1.79").is_none());
    }

    /// A profile JSON appears before the patched client jar, so an interrupted
    /// install must not be mistaken for a finished one.
    #[tokio::test]
    async fn patched_client_jar_is_required_for_forge_profiles(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = PortablePaths::from_root(directory.path())?;
        let installer = LoaderInstaller::new(paths.clone(), Downloader::new(2)?);

        assert!(!installer.loader_payload_installed("NeoForge", "1.21.1", "21.1.248"));
        assert!(!installer.loader_payload_installed("Forge", "1.21.1", "52.0.28"));

        let jar = paths
            .libraries_dir()
            .join("net/neoforged/neoforge/21.1.248/neoforge-21.1.248-client.jar");
        std::fs::create_dir_all(jar.parent().expect("parent"))?;
        std::fs::write(&jar, b"jar")?;
        assert!(installer.loader_payload_installed("NeoForge", "1.21.1", "21.1.248"));

        // Forge and NeoForge 1.20.1 keep the jar under a versioned directory.
        let forge = paths
            .libraries_dir()
            .join("net/minecraftforge/forge/1.21.1-52.0.28/forge-1.21.1-52.0.28-client.jar");
        std::fs::create_dir_all(forge.parent().expect("parent"))?;
        std::fs::write(&forge, b"jar")?;
        assert!(installer.loader_payload_installed("Forge", "1.21.1", "52.0.28"));

        // Fabric and Quilt do not patch the client jar at all.
        assert!(installer.loader_payload_installed("Fabric", "1.21.1", "0.16.5"));
        Ok(())
    }

    #[test]
    fn bundle_paths_cannot_escape_the_libraries_directory() {
        assert!(safe_relative("libraries/net/neoforged/demo.jar").is_ok());
        for bad in [
            "libraries/../evil.jar",
            "libraries/net/../../evil.jar",
            "libraries//demo.jar",
            "libraries/net\\demo.jar",
            "libraries/C:/evil.jar",
            "net/neoforged/demo.jar",
        ] {
            assert!(safe_relative(bad).is_err(), "{bad} must be rejected");
        }
    }

    fn write_bundle(
        archive: &Path,
        installer: &[u8],
        library: &[u8],
        library_sha1: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        let manifest = serde_json::json!({
            "format": 1,
            "installer": {
                "path": "installer.jar",
                "sha1": pml_downloader::hash_bytes(HashAlgo::Sha1, installer),
            },
            "files": [{
                "path": "libraries/net/neoforged/demo.jar",
                "sha1": library_sha1,
            }],
        });
        let file = std::fs::File::create(archive)?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("manifest.json", options)?;
        zip.write_all(serde_json::to_string(&manifest)?.as_bytes())?;
        zip.start_file("installer.jar", options)?;
        zip.write_all(installer)?;
        zip.start_file("libraries/net/neoforged/demo.jar", options)?;
        zip.write_all(library)?;
        zip.finish()?;
        Ok(())
    }

    #[test]
    fn bundle_extraction_verifies_files_and_writes_them() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let installer = b"verified installer".as_slice();
        let library = b"verified library".as_slice();
        let archive = directory.path().join("bundle.zip");
        write_bundle(
            &archive,
            installer,
            library,
            pml_downloader::hash_bytes(HashAlgo::Sha1, library),
        )?;

        let cache = directory.path().join("loaders/installer.jar");
        let libraries = directory.path().join("libraries");
        extract_bundle(&archive, &cache, &libraries)?;

        assert_eq!(std::fs::read(&cache)?, installer);
        assert_eq!(
            std::fs::read(libraries.join("net/neoforged/demo.jar"))?,
            library
        );
        Ok(())
    }

    #[test]
    fn bundle_extraction_rejects_a_tampered_library() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let archive = directory.path().join("bundle.zip");
        write_bundle(
            &archive,
            b"verified installer".as_slice(),
            b"tampered library".as_slice(),
            "0".repeat(40),
        )?;
        let error = extract_bundle(
            &archive,
            &directory.path().join("installer.jar"),
            &directory.path().join("libraries"),
        )
        .expect_err("a wrong SHA-1 must abort the extraction");
        assert!(error.contains("SHA-1"), "{error}");
        Ok(())
    }

    /// A release build compiles the archives into the executable, so the bytes
    /// in the image must be exactly the ones pinned in `OFFLINE_BUNDLES`. A dev
    /// build without `dist/neoforge-mirror/` legitimately embeds nothing.
    #[test]
    fn embedded_bundles_match_the_pinned_offline_bundles() {
        for (file, bytes) in embedded::EMBEDDED_BUNDLES {
            let bundle = OFFLINE_BUNDLES
                .iter()
                .find(|bundle| bundle.file == *file)
                .unwrap_or_else(|| panic!("{file} is embedded but not pinned"));
            assert_eq!(
                bytes.len() as u64,
                bundle.size,
                "embedded {file} has the wrong size"
            );
            assert_eq!(
                pml_downloader::hash_bytes(HashAlgo::Sha256, bytes),
                bundle.sha256,
                "embedded {file} has the wrong SHA-256"
            );
        }
    }

    /// Confirms that the pinned bundles are actually published and unchanged.
    /// Run with `cargo test -p pml-loader -- --ignored`.
    #[tokio::test]
    #[ignore = "network test: downloads the published bundles from GitHub"]
    async fn offline_bundles_are_published_and_intact() -> Result<(), Box<dyn std::error::Error>> {
        let client = reqwest::Client::builder().no_proxy().build()?;
        for (minecraft, loader) in [("1.21.1", "21.1.248"), ("1.20.1", "47.1.79")] {
            let bundle = offline_bundle(minecraft, loader).expect("supported version");
            let url = format!("{NEOFORGE_MIRROR_BASE}/{}", bundle.file);
            let response = client.get(&url).send().await?;
            assert_eq!(
                response.status(),
                reqwest::StatusCode::OK,
                "{url} is not published"
            );
            let bytes = response.bytes().await?;
            assert_eq!(bytes.len() as u64, bundle.size, "{url}");
            assert_eq!(
                pml_downloader::hash_bytes(HashAlgo::Sha256, &bytes),
                bundle.sha256,
                "{url}"
            );
        }
        Ok(())
    }

    fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let destination = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_dir(&entry.path(), &destination)?;
            } else {
                std::fs::copy(entry.path(), destination)?;
            }
        }
        Ok(())
    }

    #[test]
    fn installer_candidates_try_the_official_host_first() {
        let official =
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.248/neoforge-21.1.248-installer.jar";
        let candidates = installer_candidates(official);
        assert_eq!(candidates[0], official);
        assert_eq!(candidates.len(), NEOFORGE_MIRRORS.len());
        assert!(candidates
            .iter()
            .any(|url| url.starts_with("https://neoforged.forgecdn.net/releases/")));
        // The mirror NeoForged recommends keeps the artifact path and drops
        // the `/releases` prefix.
        assert!(candidates.contains(&official.replace(
            "https://maven.neoforged.net/releases",
            "https://maven.creeperhost.net"
        )));
        // Forge URLs live on another Maven; they must not be rewritten onto
        // NeoForge mirrors.
        let forge = "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.0.28/forge-1.21.1-52.0.28-installer.jar";
        assert_eq!(installer_candidates(forge), vec![forge.to_owned()]);
    }

    #[test]
    fn installer_artifacts_are_read_from_the_installer_itself(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("installer.jar");
        let profile = serde_json::json!({
            "libraries": [
                {
                    "downloads": {
                        "artifact": {
                            "url": "https://maven.neoforged.net/releases/net/neoforged/loader/4.0.43/loader-4.0.43.jar",
                            "path": "net/neoforged/loader/4.0.43/loader-4.0.43.jar",
                            "sha1": "FB10B7BF2F568A9676AD8B426B19C23BADBBD98A"
                        }
                    }
                },
                // Duplicate path: the later entry must not be fetched twice.
                {
                    "downloads": {
                        "artifact": {
                            "url": "https://maven.neoforged.net/releases/net/neoforged/loader/4.0.43/loader-4.0.43.jar",
                            "path": "net/neoforged/loader/4.0.43/loader-4.0.43.jar",
                            "sha1": "0000000000000000000000000000000000000000"
                        }
                    }
                },
                // Hosted by Mojang: the installer downloads those itself.
                {
                    "downloads": {
                        "artifact": {
                            "url": "https://libraries.minecraft.net/com/google/guava/guava/20.0/guava-20.0.jar",
                            "path": "com/google/guava/guava/20.0/guava-20.0.jar",
                            "sha1": "4b6c5f5c0e5c9b4b1b0b0c5f4b0f0b0c5f4b0f0b"
                        }
                    }
                },
                // Path traversal: never unpack outside `libraries/`.
                {
                    "downloads": {
                        "artifact": {
                            "url": "https://maven.neoforged.net/releases/../../evil.jar",
                            "path": "../../evil.jar",
                            "sha1": "1111111111111111111111111111111111111111"
                        }
                    }
                }
            ]
        });
        {
            let file = std::fs::File::create(&path)?;
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("install_profile.json", options)?;
            zip.write_all(profile.to_string().as_bytes())?;
            zip.finish()?;
        }

        let artifacts = installer_maven_artifacts(&path)?;
        assert_eq!(artifacts.len(), 1);
        let artifact = &artifacts[0];
        assert_eq!(
            artifact.relative,
            "net/neoforged/loader/4.0.43/loader-4.0.43.jar"
        );
        assert_eq!(
            artifact.path,
            "net/neoforged/loader/4.0.43/loader-4.0.43.jar"
        );
        assert_eq!(artifact.sha1, "fb10b7bf2f568a9676ad8b426b19c23badbbd98a");
        Ok(())
    }

    #[test]
    fn library_paths_cannot_escape_the_libraries_directory() {
        assert!(safe_library_path(
            "net/neoforged/loader/4.0.43/loader-4.0.43.jar"
        ));
        assert!(!safe_library_path(""));
        assert!(!safe_library_path("/etc/passwd"));
        assert!(!safe_library_path("../evil.jar"));
        assert!(!safe_library_path("net/../../evil.jar"));
        assert!(!safe_library_path("C:/windows/system32/evil.dll"));
        assert!(!safe_library_path("net\\neoforged\\loader.jar"));
        assert!(!safe_library_path("net//loader.jar"));
    }

    /// Runs the real installation path while `blocked` hosts are forced to an
    /// unreachable address, which is what a censored provider looks like.
    ///
    /// When `expect_bundle` is set, every host that serves NeoForge artifacts is
    /// blocked and the published offline bundle has to supply them; otherwise
    /// only the canonical Maven is blocked and the mirror chain must succeed
    /// without touching the bundle.
    ///
    /// Requires a data directory with vanilla 1.21.1 in `ODIUM_TEST_DATA_DIR`
    /// and a JDK in `ODIUM_TEST_JAVA`; skips silently otherwise.
    async fn install_neoforge_with_blocked_hosts(
        blocked: &[&str],
        expect_bundle: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (Ok(source), Ok(java)) = (
            std::env::var("ODIUM_TEST_DATA_DIR"),
            std::env::var("ODIUM_TEST_JAVA"),
        ) else {
            return Ok(());
        };

        let directory = tempfile::tempdir()?;
        let root = directory.path().join("root");
        std::fs::create_dir_all(root.join("libraries"))?;
        copy_dir(
            &Path::new(&source).join("versions").join("1.21.1"),
            &root.join("versions").join("1.21.1"),
        )?;

        // A half-finished install leaves a profile JSON without the patched
        // client jar. It must be replaced instead of being reused.
        let stale = root.join("versions").join("neoforge-21.1.248");
        std::fs::create_dir_all(&stale)?;
        std::fs::write(
            stale.join("neoforge-21.1.248.json"),
            br#"{"id":"stale","inheritsFrom":"1.21.1"}"#,
        )?;

        let mut builder = reqwest::Client::builder().no_proxy();
        for host in blocked {
            builder = builder.resolve(host, "127.0.0.1:9".parse()?);
        }
        let downloader = Downloader::new_with_client(builder.build()?, 2);
        let paths = PortablePaths::from_root(&root)?;
        let installer = LoaderInstaller::new(paths, downloader);
        let spec = LoaderSpec {
            kind: LoaderKind::NeoForge,
            version: "21.1.248".to_owned(),
        };
        let profile = installer
            .install("1.21.1", &spec, Path::new(&java), None, None)
            .await?;

        assert_eq!(profile.id, "neoforge-21.1.248");
        let libraries = root.join("libraries");
        assert!(
            libraries
                .join("net/neoforged/neoforge/21.1.248/neoforge-21.1.248-client.jar")
                .is_file(),
            "the installer must produce the patched client jar"
        );
        let bundle = offline_bundle("1.21.1", "21.1.248").expect("supported version");
        let archive = root.join("loaders").join(bundle.file);
        // A build with the archive compiled into the executable always lays it
        // out from the image, even when every mirror is reachable.
        let embedded = embedded_bundle(bundle.file).is_some();
        assert_eq!(
            archive.is_file(),
            expect_bundle || embedded,
            "bundle usage should match the blocked host set"
        );

        // Every artifact from the bundle must be in place, because the installer
        // would otherwise try the blocked host again.
        if expect_bundle || embedded {
            let file = std::fs::File::open(&archive)?;
            let mut zip = zip::ZipArchive::new(file)?;
            let manifest: serde_json::Value = {
                let mut entry = zip.by_name("manifest.json")?;
                let mut text = String::new();
                entry.read_to_string(&mut text)?;
                serde_json::from_str(&text)?
            };
            let mut checked = 0;
            for item in manifest["files"].as_array().expect("manifest files") {
                let path = item["path"].as_str().expect("path");
                let sha1 = item["sha1"].as_str().expect("sha1");
                let relative = safe_relative(path)?;
                let installed = libraries.join(relative);
                assert!(installed.is_file(), "{} is missing", installed.display());
                assert_eq!(
                    pml_downloader::hash_file_blocking(&installed, HashAlgo::Sha1)?,
                    sha1,
                    "{} is corrupt",
                    installed.display()
                );
                checked += 1;
            }
            assert_eq!(checked, 51);
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "network test: downloads the offline bundle and runs the NeoForge installer"]
    async fn offline_bundle_installs_neoforge_when_maven_is_blocked(
    ) -> Result<(), Box<dyn std::error::Error>> {
        install_neoforge_with_blocked_hosts(
            &[
                "maven.neoforged.net",
                "neoforged.forgecdn.net",
                "mirrors.neoforged.net",
            ],
            true,
        )
        .await
    }

    /// The canonical Maven is often blocked while Forge's CDN still works, so
    /// the installer and its libraries have to come from the mirror alone.
    ///
    /// A build that embeds the archive takes the local path first, so the
    /// mirror chain is only exercised by a dev build without `dist/`.
    #[tokio::test]
    #[ignore = "network test: downloads the installer from Forge CDN and runs it"]
    async fn mirror_installs_neoforge_when_only_the_maven_is_blocked(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bundle = offline_bundle("1.21.1", "21.1.248").expect("supported version");
        if embedded_bundle(bundle.file).is_some() {
            eprintln!(
                "skipped: this build embeds {} and never reaches a mirror",
                bundle.file
            );
            return Ok(());
        }
        install_neoforge_with_blocked_hosts(&["maven.neoforged.net"], false).await
    }

    /// Proves the release path: with every host that can serve NeoForge
    /// artifacts — Maven, its mirrors, and the GitHub release plus all proxies —
    /// unreachable, the installer still runs because the archive comes out of
    /// the executable.
    #[tokio::test]
    #[ignore = "network test: runs the NeoForge installer with all loader hosts blocked"]
    async fn embedded_bundle_installs_neoforge_without_any_loader_host(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bundle = offline_bundle("1.21.1", "21.1.248").expect("supported version");
        if embedded_bundle(bundle.file).is_none() {
            eprintln!(
                "skipped: this build has no embedded {} (run tools/fetch-offline-bundles.ps1)",
                bundle.file
            );
            return Ok(());
        }
        let mut blocked = vec![
            "maven.neoforged.net",
            "neoforged.forgecdn.net",
            "mirrors.neoforged.net",
            "maven.creeperhost.net",
            "github.com",
        ];
        // GitHub proxies live on their own hosts; the archive must not need them.
        for proxy in GITHUB_PROXIES {
            let host = proxy
                .split("://")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .expect("proxy host");
            blocked.push(host);
        }
        install_neoforge_with_blocked_hosts(&blocked, true).await
    }
}
