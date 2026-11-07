//! Java runtime discovery and provisioning.
//!
//! Minecraft 1.21.1 requires Java 21. The launcher first looks for an existing
//! installation (cached path, `JAVA_HOME`, `PATH`, common vendor folders and
//! `launcher_data/java`). If none is new enough, it downloads Eclipse Temurin
//! 21 from the Adoptium API, verifies the published SHA-256 and extracts the
//! archive with the zip-slip-safe extractor from `pml-downloader`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use pml_config::{save_json_atomic, PortablePaths};
use pml_downloader::{safe_extract_zip, DownloadEvent, DownloadTask, Downloader, HashAlgo};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

pub const MIN_JAVA_MAJOR: u32 = 21;
pub const ADOPTIUM_LATEST_21: &str = "https://api.adoptium.net/v3/assets/latest/21/hotspot?architecture=x64&image_type=jdk&os=windows&vendor=eclipse";

/// How an installation was found. Cached entries are re-probed on every run,
/// so a deleted runtime is never trusted just because it is in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JavaOrigin {
    Cached,
    JavaHome,
    Path,
    Common,
    Downloaded,
}

impl JavaOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cached => "кэш",
            Self::JavaHome => "JAVA_HOME",
            Self::Path => "PATH",
            Self::Common => "системная",
            Self::Downloaded => "Temurin 21",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaInstallation {
    /// Full path to `java.exe`.
    pub java_exe: PathBuf,
    /// Runtime root (the directory containing `bin`).
    pub home: PathBuf,
    pub major: u32,
    /// Version string exactly as reported by `java -version`.
    pub version: String,
    pub origin: JavaOrigin,
}

impl JavaInstallation {
    pub fn bin_dir(&self) -> &Path {
        self.home.as_path()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaProbe {
    pub version: String,
    pub major: u32,
}

#[derive(Debug, Error)]
pub enum JavaError {
    #[error("подходящая Java {MIN_JAVA_MAJOR}+ не найдена")]
    NotFound,
    #[error("не удалось запустить Java {path}: {source}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Java {0} не сообщила версию за 15 секунд")]
    ProbeTimeout(PathBuf),
    #[error("не удалось разобрать версию Java: {0}")]
    BadVersion(PathBuf),
    #[error(transparent)]
    Download(#[from] pml_downloader::DownloadError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Config(#[from] pml_config::ConfigError),
    #[error("ошибка ввода-вывода {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("некорректный ответ Adoptium: {0}")]
    Adoptium(String),
    #[error("в архиве Java не найден bin/java.exe")]
    MissingExecutable,
}

#[derive(Debug, Deserialize)]
struct AdoptiumAsset {
    binary: AdoptiumBinary,
    #[serde(default)]
    release_name: Option<String>,
    #[serde(default)]
    version: Option<AdoptiumVersion>,
}

#[derive(Debug, Deserialize)]
struct AdoptiumBinary {
    package: AdoptiumPackage,
}

#[derive(Debug, Deserialize)]
struct AdoptiumPackage {
    #[serde(default)]
    checksum: Option<String>,
    link: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AdoptiumVersion {
    #[serde(default)]
    semver: Option<String>,
}

/// Reads the first quoted version token from `java -version` output.
///
/// Examples:
/// `openjdk version "21.0.5" 2024-10-15 LTS` -> `21.0.5` / 21
/// `java version "1.8.0_401"` -> `1.8.0_401` / 8
/// `openjdk version "25" 2025-09-16` -> `25` / 25
pub fn parse_java_version(output: &str) -> Option<JavaProbe> {
    let line = output
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("version"))?;
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    let version = line[start..end].trim().to_owned();
    if version.is_empty() {
        return None;
    }

    let mut parts = version.split(['.', '_', '+', '-']);
    let first = parts.next()?.trim();
    let major = if first == "1" {
        parts.next()?.trim().parse::<u32>().ok()?
    } else {
        first.parse::<u32>().ok()?
    };
    Some(JavaProbe { version, major })
}

async fn probe_java(java_exe: &Path) -> Result<JavaProbe, JavaError> {
    let mut command = tokio::process::Command::new(java_exe);
    command
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_console(&mut command);

    let output = match tokio::time::timeout(Duration::from_secs(15), command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(source)) => {
            return Err(JavaError::Spawn {
                path: java_exe.to_path_buf(),
                source,
            })
        }
        Err(_) => return Err(JavaError::ProbeTimeout(java_exe.to_path_buf())),
    };

    // `java -version` writes to stderr on every known implementation, but some
    // wrappers mirror it to stdout; check both.
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_java_version(&text).ok_or_else(|| JavaError::BadVersion(java_exe.to_path_buf()))
}

/// Probes every plausible candidate concurrently. Broken shortcuts and stale
/// cache entries simply produce no result instead of failing the whole scan.
pub async fn discover_all(paths: &PortablePaths) -> Vec<JavaInstallation> {
    let paths = paths.clone();
    let candidates = match tokio::task::spawn_blocking(move || collect_candidates(&paths)).await {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(%error, "java candidate scan failed");
            Vec::new()
        }
    };

    let mut set: JoinSet<Option<JavaInstallation>> = JoinSet::new();
    for (java_exe, origin) in candidates {
        set.spawn(async move {
            let probe = probe_java(&java_exe).await.ok()?;
            let home = java_exe.parent()?.parent()?.to_path_buf();
            Some(JavaInstallation {
                java_exe,
                home,
                major: probe.major,
                version: probe.version,
                origin,
            })
        });
    }

    let mut found = Vec::new();
    while let Some(result) = set.join_next().await {
        match result {
            Ok(Some(installation)) => found.push(installation),
            Ok(None) => {}
            Err(error) => tracing::debug!(%error, "java probe task failed"),
        }
    }
    found.sort_by_key(|installation| (installation.major != MIN_JAVA_MAJOR, installation.major));
    found
}

/// Picks Java 21 when available, otherwise the lowest major above 21 (closest
/// to the version Minecraft 1.21.1 was built for). Older runtimes are rejected.
pub fn select_best(installations: &[JavaInstallation]) -> Option<JavaInstallation> {
    installations
        .iter()
        .filter(|installation| installation.major >= MIN_JAVA_MAJOR)
        .min_by_key(|installation| (installation.major != MIN_JAVA_MAJOR, installation.major))
        .cloned()
}

fn collect_candidates(paths: &PortablePaths) -> Vec<(PathBuf, JavaOrigin)> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();

    let mut push = |path: PathBuf, origin: JavaOrigin| {
        let key = path.to_string_lossy().to_ascii_lowercase();
        if seen.insert(key) && path.is_file() {
            candidates.push((path, origin));
        }
    };

    if let Some(cached) = read_cached(paths) {
        push(cached, JavaOrigin::Cached);
    }

    if let Some(java_home) = std::env::var_os("JAVA_HOME") {
        push(
            PathBuf::from(java_home).join("bin").join(java_exe_name()),
            JavaOrigin::JavaHome,
        );
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path_var) {
            push(entry.join(java_exe_name()), JavaOrigin::Path);
        }
    }

    // A previous Adoptium download (or a user-supplied portable JRE) lives here.
    if let Ok(walker) = walkdir::WalkDir::new(paths.java_dir())
        .max_depth(6)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
    {
        for entry in walker {
            if entry.file_type().is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(java_exe_name())
                && entry
                    .path()
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
            {
                push(entry.into_path(), JavaOrigin::Common);
            }
        }
    }

    for root in common_java_roots() {
        push(root.join("bin").join(java_exe_name()), JavaOrigin::Common);
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let home = entry.path();
                if home.is_dir() {
                    push(home.join("bin").join(java_exe_name()), JavaOrigin::Common);
                }
            }
        }
    }

    candidates
}

fn common_java_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut bases = vec![
        PathBuf::from(r"C:\Program Files"),
        PathBuf::from(r"C:\Program Files (x86)"),
    ];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        bases.push(PathBuf::from(local).join("Programs"));
    }
    let vendors = [
        "Eclipse Adoptium",
        "Java",
        "Microsoft",
        "Amazon Corretto",
        "Zulu",
        "BellSoft",
        "Semeru",
        "IBM",
        "Eclipse Foundation",
        "OpenJDK",
    ];
    for base in bases {
        for vendor in vendors {
            roots.push(base.join(vendor));
        }
    }
    roots
}

fn java_exe_name() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

fn cache_file(paths: &PortablePaths) -> PathBuf {
    paths.java_dir().join("java.json")
}

fn read_cached(paths: &PortablePaths) -> Option<PathBuf> {
    let bytes = std::fs::read(cache_file(paths)).ok()?;
    let installation: JavaInstallation = serde_json::from_slice(&bytes).ok()?;
    Some(installation.java_exe)
}

fn save_cache(paths: &PortablePaths, installation: &JavaInstallation) -> Result<(), JavaError> {
    save_json_atomic(&cache_file(paths), installation)?;
    Ok(())
}

/// Returns a usable Java 21+ runtime, downloading Temurin 21 if necessary.
pub async fn ensure_java(
    paths: &PortablePaths,
    downloader: &Downloader,
    progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
) -> Result<JavaInstallation, JavaError> {
    let installations = discover_all(paths).await;
    if let Some(best) = select_best(&installations) {
        tracing::info!(
            java = %best.java_exe.display(),
            major = best.major,
            origin = best.origin.label(),
            "using existing Java"
        );
        if let Err(error) = save_cache(paths, &best) {
            tracing::warn!(%error, "failed to cache Java path");
        }
        return Ok(best);
    }

    download_temurin_21(paths, downloader, progress).await
}

async fn download_temurin_21(
    paths: &PortablePaths,
    downloader: &Downloader,
    progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
) -> Result<JavaInstallation, JavaError> {
    tracing::info!("no Java 21+ found, downloading Eclipse Temurin 21");
    let response = downloader
        .client()
        .get(ADOPTIUM_LATEST_21)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<AdoptiumAsset>>()
        .await?;

    let asset = response
        .into_iter()
        .find(|asset| !asset.binary.package.link.trim().is_empty())
        .ok_or_else(|| JavaError::Adoptium("пустой список ассетов".to_owned()))?;
    let package = asset.binary.package;
    let archive_name = package
        .name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "temurin21.zip".to_owned());
    let archive = paths.temp_dir().join(sanitize_file_name(&archive_name));

    let task = DownloadTask::new(
        package.link.clone(),
        archive.clone(),
        package
            .checksum
            .clone()
            .filter(|hash| !hash.trim().is_empty()),
        HashAlgo::Sha256,
        package.size,
        "Temurin 21".to_owned(),
    );
    downloader.download_many(vec![task], progress).await?;

    let extract_dir = paths.java_dir().join("temurin-21");
    let existing = find_java_executable(&extract_dir);
    let java_exe = match existing {
        Some(path) => path,
        None => {
            tokio::fs::create_dir_all(&extract_dir)
                .await
                .map_err(|source| JavaError::Io {
                    path: extract_dir.clone(),
                    source,
                })?;
            let zip = archive.clone();
            let dest = extract_dir.clone();
            tokio::task::spawn_blocking(move || safe_extract_zip(&zip, &dest))
                .await
                .map_err(|error| JavaError::Adoptium(format!("extract task failed: {error}")))??;
            find_java_executable(&extract_dir).ok_or(JavaError::MissingExecutable)?
        }
    };

    let probe = probe_java(&java_exe).await?;
    if probe.major < MIN_JAVA_MAJOR {
        return Err(JavaError::Adoptium(format!(
            "скачалась Java {}, а нужна {MIN_JAVA_MAJOR}+",
            probe.major
        )));
    }

    let home = java_exe
        .parent()
        .and_then(Path::parent)
        .ok_or(JavaError::MissingExecutable)?
        .to_path_buf();
    let installation = JavaInstallation {
        java_exe,
        home,
        major: probe.major,
        version: asset
            .release_name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| asset.version.and_then(|version| version.semver))
            .unwrap_or_else(|| probe.version.clone()),
        origin: JavaOrigin::Downloaded,
    };
    save_cache(paths, &installation)?;

    // The archive is large (~200 MB); do not keep it after a successful install.
    if let Err(error) = tokio::fs::remove_file(&archive).await {
        tracing::debug!(%error, "could not remove Java archive");
    }
    tracing::info!(
        java = %installation.java_exe.display(),
        version = %installation.version,
        "Temurin 21 installed"
    );
    Ok(installation)
}

fn find_java_executable(root: &Path) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    walkdir::WalkDir::new(root)
        .max_depth(6)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_type().is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(java_exe_name())
                && entry
                    .path()
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
        })
        .map(walkdir::DirEntry::into_path)
}

fn sanitize_file_name(name: &str) -> String {
    let name = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if name.is_empty() {
        "temurin21.zip".to_owned()
    } else {
        name
    }
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
    fn parses_modern_java_version() {
        let output = "openjdk version \"21.0.5\" 2024-10-15 LTS\nOpenJDK Runtime Environment Temurin-21.0.5+11 (build 21.0.5+11-LTS)\n";
        let probe = parse_java_version(output).expect("version");
        assert_eq!(probe.version, "21.0.5");
        assert_eq!(probe.major, 21);
    }

    #[test]
    fn parses_legacy_java_8_version() {
        let output =
            "java version \"1.8.0_401\"\nJava(TM) SE Runtime Environment (build 1.8.0_401-b10)\n";
        let probe = parse_java_version(output).expect("version");
        assert_eq!(probe.version, "1.8.0_401");
        assert_eq!(probe.major, 8);
    }

    #[test]
    fn parses_single_component_version() {
        let output = "openjdk version \"25\" 2025-09-16\nOpenJDK Runtime Environment Temurin-25+36 (build 25+36-LTS)\n";
        let probe = parse_java_version(output).expect("version");
        assert_eq!(probe.version, "25");
        assert_eq!(probe.major, 25);
    }

    #[test]
    fn rejects_garbage_version_output() {
        assert!(parse_java_version("no version here").is_none());
        assert!(parse_java_version("version \"\"\n").is_none());
    }

    #[test]
    fn select_best_prefers_21_then_closest_newer() {
        let make = |major: u32, name: &str| JavaInstallation {
            java_exe: PathBuf::from(name),
            home: PathBuf::from("."),
            major,
            version: major.to_string(),
            origin: JavaOrigin::Common,
        };
        let installations = vec![make(25, "a"), make(17, "b"), make(21, "c"), make(22, "d")];
        let best = select_best(&installations).expect("best");
        assert_eq!(best.major, 21);
        assert_eq!(best.java_exe, PathBuf::from("c"));

        let installations = vec![make(25, "a"), make(22, "d")];
        assert_eq!(select_best(&installations).map(|i| i.major), Some(22));

        let installations = vec![make(17, "b"), make(8, "e")];
        assert!(select_best(&installations).is_none());
    }

    #[test]
    fn sanitizes_adoptium_archive_name() {
        assert_eq!(
            sanitize_file_name("OpenJDK21U-jdk_x64_windows_hotspot_21.0.5_11.zip"),
            "OpenJDK21U-jdk_x64_windows_hotspot_21.0.5_11.zip"
        );
        assert_eq!(sanitize_file_name("../../evil"), ".._.._evil");
        assert_eq!(sanitize_file_name(""), "temurin21.zip");
    }
}
