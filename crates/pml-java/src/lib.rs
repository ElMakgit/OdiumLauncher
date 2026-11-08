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
use pml_downloader::{
    safe_extract_zip_with_cancel, CancelToken, DownloadEvent, DownloadTask, Downloader, HashAlgo,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

pub const MIN_JAVA_MAJOR: u32 = 21;

/// Java major required by a Minecraft version.
///
/// Mojang's own launcher uses the same three breakpoints: 1.17 moved to Java
/// 16, 1.18 to 17 and 1.20.5 to 21. Java 17 also runs 1.17.x, so the launcher
/// requests the smallest Adoptium LTS that satisfies the version.
pub fn required_java_major(mc_version: &str) -> u32 {
    let mut parts = mc_version.split(|character: char| !character.is_ascii_digit());
    let major = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(1);
    if major != 1 {
        // Snapshots (`24w14a`) and future version schemes target modern Java.
        return MIN_JAVA_MAJOR;
    }
    let minor = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    if minor <= 16 {
        8
    } else if minor == 17 || minor <= 19 {
        17
    } else if minor == 20 {
        if patch >= 5 {
            MIN_JAVA_MAJOR
        } else {
            17
        }
    } else {
        MIN_JAVA_MAJOR
    }
}

/// Adoptium download endpoint for the current OS, 64-bit architecture and the
/// requested Java major. Minecraft only needs a runtime, so the smaller `jre`
/// image is requested first; [`download_temurin`] falls back to `jdk` for
/// releases where Adoptium publishes no JRE build.
pub fn adoptium_latest_url(major: u32) -> String {
    adoptium_url(major, "jre")
}

fn adoptium_url(major: u32, image_type: &str) -> String {
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        "linux"
    };
    let architecture = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x64"
    };
    format!(
        "https://api.adoptium.net/v3/assets/latest/{major}/hotspot?architecture={architecture}&image_type={image_type}&os={os}&vendor=eclipse"
    )
}

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
            Self::Downloaded => "Temurin",
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
    /// `os.arch` width reported by the JVM: 32, 64, or 0 when unknown.
    #[serde(default)]
    pub bits: u32,
    pub origin: JavaOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaProbe {
    pub version: String,
    pub major: u32,
    /// 32, 64, or 0 when the JVM did not report `os.arch`.
    pub bits: u32,
}

#[derive(Debug, Error)]
pub enum JavaError {
    #[error("подходящая Java {0}+ не найдена")]
    NotFound(u32),
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
    #[error("загрузка Java отменена")]
    Cancelled,
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
    // `-XshowSettings:properties` also prints `java.version = 21.0.5` before
    // the human-readable line, so require a quoted token to skip it.
    let line = output
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("version") && line.contains('"'))?;
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
    Some(JavaProbe {
        version,
        major,
        bits: 0,
    })
}

/// Reads the JVM's `os.arch` property emitted by `-XshowSettings:properties`.
/// 32-bit runtimes cannot use the 4 GiB heap configured for Minecraft 1.21.
fn parse_arch_bits(output: &str) -> Option<u32> {
    let arch = output.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("os.arch")
            .then(|| value.trim().to_ascii_lowercase())
    })?;
    match arch.as_str() {
        "x86" | "i386" | "i486" | "i586" | "i686" | "x86_32" | "arm" => Some(32),
        "amd64" | "x86_64" | "aarch64" | "arm64" | "ppc64" | "ppc64le" | "s390x" | "riscv64" => {
            Some(64)
        }
        _ => None,
    }
}

async fn probe_java(java_exe: &Path) -> Result<JavaProbe, JavaError> {
    let mut command = tokio::process::Command::new(java_exe);
    command
        .arg("-XshowSettings:properties")
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
    parse_java_version(&text)
        .ok_or_else(|| JavaError::BadVersion(java_exe.to_path_buf()))
        .map(|mut probe| {
            probe.bits = parse_arch_bits(&text).unwrap_or(0);
            probe
        })
}

/// Probes every plausible candidate concurrently. Broken shortcuts and stale
/// cache entries simply produce no result instead of failing the whole scan.
pub async fn discover_all(paths: &PortablePaths, required: u32) -> Vec<JavaInstallation> {
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
                bits: probe.bits,
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
    found.sort_by_key(|installation| (installation.major != required, installation.major));
    found
}

/// Picks the requested major when available, otherwise the lowest major above
/// it (closest to the version Minecraft was built for). Older runtimes are
/// rejected.
pub fn select_best(installations: &[JavaInstallation], required: u32) -> Option<JavaInstallation> {
    installations
        .iter()
        // A 32-bit JVM cannot honour the default multi-gigabyte heap; probing
        // rejects it early with a clear reason instead of a cryptic game crash
        // later. `bits == 0` means the JVM did not report os.arch and is kept
        // for compatibility.
        .filter(|installation| installation.major >= required && installation.bits != 32)
        .min_by_key(|installation| (installation.major != required, installation.major))
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
                let is_downloaded = entry.path().starts_with(paths.java_dir())
                    && entry.path().components().any(|component| {
                        component
                            .as_os_str()
                            .to_string_lossy()
                            .starts_with("temurin-")
                    });
                push(
                    entry.into_path(),
                    if is_downloaded {
                        JavaOrigin::Downloaded
                    } else {
                        JavaOrigin::Common
                    },
                );
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

#[cfg(windows)]
fn common_java_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut bases = Vec::new();
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(value) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
            bases.push(PathBuf::from(value));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
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

#[cfg(not(windows))]
fn common_java_roots() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/lib/jvm"),
        PathBuf::from("/usr/java"),
        PathBuf::from("/Library/Java/JavaVirtualMachines"),
    ]
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

/// Returns a usable Java runtime for `required`, downloading Temurin if
/// necessary.
pub async fn ensure_java(
    paths: &PortablePaths,
    downloader: &Downloader,
    required: u32,
    progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    cancel: Option<CancelToken>,
) -> Result<JavaInstallation, JavaError> {
    if cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
        return Err(JavaError::Cancelled);
    }

    // Fast path: the runtime used last time is usually still installed.
    // Probing a single cached executable is far cheaper than scanning every
    // vendor folder on the machine, and it keeps the common case instant.
    if let Some(cached) = read_cached(paths) {
        if cached.is_file() {
            if let Ok(probe) = probe_java(&cached).await {
                if probe.major >= required && probe.bits != 32 {
                    let installation = JavaInstallation {
                        home: cached
                            .parent()
                            .and_then(Path::parent)
                            .unwrap_or(&cached)
                            .to_path_buf(),
                        java_exe: cached,
                        major: probe.major,
                        version: probe.version,
                        bits: probe.bits,
                        origin: JavaOrigin::Cached,
                    };
                    tracing::info!(
                        java = %installation.java_exe.display(),
                        major = installation.major,
                        "using cached Java"
                    );
                    return Ok(installation);
                }
            }
        }
    }

    let installations = discover_all(paths, required).await;
    if let Some(best) = select_best(&installations, required) {
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

    download_temurin(paths, downloader, required, progress, cancel).await
}

async fn download_temurin(
    paths: &PortablePaths,
    downloader: &Downloader,
    required: u32,
    progress: Option<mpsc::UnboundedSender<DownloadEvent>>,
    cancel: Option<CancelToken>,
) -> Result<JavaInstallation, JavaError> {
    tracing::info!(
        required,
        "no suitable Java found, downloading Eclipse Temurin"
    );
    let mut assets = fetch_adoptium_assets(downloader, required, "jre").await?;
    if assets.is_empty() {
        tracing::warn!(
            required,
            "Adoptium has no JRE build for this major, falling back to a JDK"
        );
        assets = fetch_adoptium_assets(downloader, required, "jdk").await?;
    }

    // The endpoint may return several builds (for example a point release per
    // vendor); take the newest numeric version instead of the first array item.
    let asset = assets
        .into_iter()
        .filter(|asset| !asset.binary.package.link.trim().is_empty())
        .max_by_key(asset_version_key)
        .ok_or_else(|| JavaError::Adoptium("пустой список ассетов".to_owned()))?;
    let package = asset.binary.package;
    let archive_name = package
        .name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("temurin{required}.zip"));
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
        format!("Temurin {required}"),
    );
    downloader.download_many(vec![task], progress).await?;

    let extract_dir = paths.java_dir().join(format!("temurin-{required}"));
    let existing = find_java_executable(&extract_dir);
    let java_exe = match existing {
        Some(path) => path,
        None => {
            if cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
                return Err(JavaError::Cancelled);
            }
            tokio::fs::create_dir_all(&extract_dir)
                .await
                .map_err(|source| JavaError::Io {
                    path: extract_dir.clone(),
                    source,
                })?;
            let zip = archive.clone();
            let dest = extract_dir.clone();
            let cancel_for_extract = cancel.clone();
            tokio::task::spawn_blocking(move || {
                safe_extract_zip_with_cancel(&zip, &dest, cancel_for_extract.as_ref())
            })
            .await
            .map_err(|error| JavaError::Adoptium(format!("extract task failed: {error}")))??;
            find_java_executable(&extract_dir).ok_or(JavaError::MissingExecutable)?
        }
    };

    let probe = probe_java(&java_exe).await?;
    if probe.major < required {
        return Err(JavaError::Adoptium(format!(
            "скачалась Java {}, а нужна {required}+",
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
        bits: probe.bits,
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
        major = required,
        "Temurin installed"
    );
    Ok(installation)
}

async fn fetch_adoptium_assets(
    downloader: &Downloader,
    major: u32,
    image_type: &str,
) -> Result<Vec<AdoptiumAsset>, JavaError> {
    Ok(downloader
        .client()
        .get(adoptium_url(major, image_type))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<AdoptiumAsset>>()
        .await?)
}

/// Numeric release key used to pick the newest Adoptium build from the API's
/// (unordered) asset list: `21.0.10` must beat `21.0.9`.
fn asset_version_key(asset: &AdoptiumAsset) -> Vec<u32> {
    let text = asset
        .version
        .as_ref()
        .and_then(|version| version.semver.as_deref())
        .or(asset.release_name.as_deref())
        .unwrap_or_default();
    text.split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok())
        .collect()
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
            bits: 64,
            origin: JavaOrigin::Common,
        };
        let installations = vec![make(25, "a"), make(17, "b"), make(21, "c"), make(22, "d")];
        let best = select_best(&installations, 21).expect("best");
        assert_eq!(best.major, 21);
        assert_eq!(best.java_exe, PathBuf::from("c"));

        // A 32-bit JVM is unusable with the default 4 GiB heap, even when its
        // version would otherwise win.
        let mut x86 = make(21, "x86");
        x86.bits = 32;
        assert!(select_best(&[x86, make(22, "d")], 21).is_some_and(|best| best.major == 22));

        let installations = vec![make(25, "a"), make(22, "d")];
        assert_eq!(select_best(&installations, 21).map(|i| i.major), Some(22));

        let installations = vec![make(17, "b"), make(8, "e")];
        assert!(select_best(&installations, 21).is_none());

        // An older pack may explicitly request Java 8; a modern runtime is
        // still acceptable but the exact major wins.
        let installations = vec![make(21, "a"), make(8, "b"), make(17, "c")];
        assert_eq!(select_best(&installations, 8).map(|i| i.major), Some(8));
    }

    #[test]
    fn required_java_major_follows_minecraft_breakpoints() {
        assert_eq!(required_java_major("1.7.10"), 8);
        assert_eq!(required_java_major("1.12.2"), 8);
        assert_eq!(required_java_major("1.16.5"), 8);
        assert_eq!(required_java_major("1.17.1"), 17);
        assert_eq!(required_java_major("1.18.2"), 17);
        assert_eq!(required_java_major("1.20.4"), 17);
        assert_eq!(required_java_major("1.20.5"), 21);
        assert_eq!(required_java_major("1.20.6"), 21);
        assert_eq!(required_java_major("1.21.1"), 21);
        assert_eq!(required_java_major("24w14a"), 21);
    }

    #[test]
    fn parses_arch_from_show_settings() {
        assert_eq!(
            parse_arch_bits("    os.arch = amd64\n    java.version = 21.0.5\n"),
            Some(64)
        );
        assert_eq!(parse_arch_bits("os.arch = x86\n"), Some(32));
        assert_eq!(parse_arch_bits("os.arch = aarch64\n"), Some(64));
        assert_eq!(parse_arch_bits("no properties"), None);
    }

    #[test]
    fn adoptium_url_matches_the_current_platform() {
        let url = adoptium_latest_url(21);
        assert!(url.contains("/21/hotspot"));
        assert!(url.contains("image_type=jre"));
        assert!(url.contains("vendor=eclipse"));
        if cfg!(windows) {
            assert!(url.contains("os=windows"));
        }
        if cfg!(target_arch = "aarch64") {
            assert!(url.contains("architecture=aarch64"));
        } else {
            assert!(url.contains("architecture=x64"));
        }
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
