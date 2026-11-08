//! Mod loader provisioning.
//!
//! Fabric and Quilt expose ready-to-use profile JSON documents, which are
//! equivalent to what their installers generate and avoid running Java at all.
//! Forge and NeoForge only ship a headless installer; it is executed with
//! `--installClient` pointed at `launcher_data`, and the generated version JSON
//! is then read back and merged with the vanilla version by `pml-minecraft`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use pml_config::{LoaderKind, LoaderSpec, PortablePaths};
use pml_downloader::{CancelToken, DownloadEvent, DownloadTask, Downloader, HashAlgo};
use pml_minecraft::VersionJson;
use thiserror::Error;
use tokio::sync::mpsc;

const FABRIC_META: &str = "https://meta.fabricmc.net/v2/versions/loader";
const QUILT_META: &str = "https://meta.quiltmc.org/v3/versions/loader";
const FORGE_MAVEN: &str = "https://maven.minecraftforge.net/net/minecraftforge/forge";
const NEOFORGE_MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged/neoforge";
/// A Forge/NeoForge installer may need to download libraries, but it must never
/// hang the launch forever when Maven or the network stalls.
const INSTALLER_TIMEOUT: Duration = Duration::from_secs(300);

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
    #[error(".sha1 для {0} недоступен; загрузка установщика без проверки отменена")]
    Sha1Unavailable(String),
    #[error("профиль загрузчика наследует {actual}, сборка требует {expected}")]
    ProfileMismatch { actual: String, expected: String },
    #[error("установщик {0} не создал version JSON")]
    MissingVersionJson(String),
    #[error("профиль загрузчика не найден: {0}")]
    MissingProfile(PathBuf),
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
    cancel: Option<CancelToken>,
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
                        cancel: cancel.clone(),
                    },
                    progress,
                )
                .await?
            }
            LoaderKind::NeoForge => {
                let url = neoforge_installer_url(&spec.version);
                let cache = self.installer_cache("neoforge", mc_version, &spec.version);
                let version_id = format!("neoforge-{}", spec.version);
                self.install_with_java(
                    InstallerRequest {
                        loader_name: "NeoForge",
                        url: &url,
                        cache: &cache,
                        java_exe,
                        version_id: &version_id,
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
            cancel,
        } = request;
        require_https(url)?;
        // The installer also writes its JSON into versions/. Reuse it when it is
        // already there, which makes subsequent launches network-free.
        if let Some(profile) = self.locate_version_json(version_id).await {
            tracing::debug!(path = %profile.display(), "using installed loader profile");
            return read_profile_required(&profile).await;
        }

        if !cache.is_file() {
            if let Some(parent) = cache.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|source| LoaderError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
            }
            // Maven always publishes the .sha1 next to the jar; a missing one
            // means the endpoint is broken or hostile, and silently installing
            // an unverified executable is not an option.
            let Some(sha1) = fetch_sha1(&self.downloader, url).await else {
                tracing::warn!(
                    url,
                    loader = loader_name,
                    "installer .sha1 is unavailable; refusing an unverified download"
                );
                return Err(LoaderError::Sha1Unavailable(url.to_owned()));
            };
            let task = DownloadTask::new(
                url,
                cache.to_path_buf(),
                Some(sha1),
                HashAlgo::Sha1,
                None,
                format!("{loader_name} installer"),
            );
            self.downloader
                .download_many(vec![task], progress.clone())
                .await?;
        }

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

pub fn neoforge_installer_url(loader_version: &str) -> String {
    format!("{NEOFORGE_MAVEN}/{loader_version}/neoforge-{loader_version}-installer.jar")
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

async fn fetch_sha1(downloader: &Downloader, url: &str) -> Option<String> {
    let response = downloader
        .client()
        .get(format!("{url}.sha1"))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let text = response.text().await.ok()?;
    let hash = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if hash.len() == 40 && hash.chars().all(|character| character.is_ascii_hexdigit()) {
        Some(hash)
    } else {
        None
    }
}

fn tail(text: &str) -> String {
    let lines = text.lines().rev().take(12).collect::<Vec<_>>();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
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
            neoforge_installer_url("21.1.248"),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.248/neoforge-21.1.248-installer.jar"
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
}
