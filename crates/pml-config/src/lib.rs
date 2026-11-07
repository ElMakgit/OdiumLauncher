//! Portable paths, the user-facing `config.json` schema and crash-safe persistence.
//!
//! The launcher never writes to the registry and never uses the current working
//! directory: every mutable byte lives in `launcher_data` next to the executable.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Product name used by the window title, Minecraft's launcher_name and the
/// multiplayer entry. A branded build can override it without touching the
/// domain crates:
///
/// ```text
/// set ODIUM_BRAND_NAME=My Launcher
/// cargo build --release
/// ```
pub const BRAND_NAME: &str = match option_env!("ODIUM_BRAND_NAME") {
    Some(name) => name,
    None => "Odium Launcher",
};

/// Short name of the managed multiplayer entry (`servers.dat`).
pub const SERVER_LIST_NAME: &str = match option_env!("ODIUM_SERVER_LIST_NAME") {
    Some(name) => name,
    None => "Odium",
};

/// Built-in source defaults. A distributor can point a branded build at its own
/// manifest and server at compile time; the Odium values remain the fallback.
pub const DEFAULT_SOURCE_ID: &str = match option_env!("ODIUM_DEFAULT_SOURCE_ID") {
    Some(id) => id,
    None => "odium",
};
pub const DEFAULT_SOURCE_NAME: &str = match option_env!("ODIUM_DEFAULT_SOURCE_NAME") {
    Some(name) => name,
    None => "Odium",
};
pub const DEFAULT_SOURCE_URL: &str = match option_env!("ODIUM_DEFAULT_SOURCE_URL") {
    Some(url) => url,
    None => "https://raw.githubusercontent.com/ElMakgit/Odium/main/EblyaCraft%20Don%27s%20Edit%202.3.1.mrpack",
};
const DEFAULT_SERVER_IP: &str = match option_env!("ODIUM_DEFAULT_SERVER_IP") {
    Some(ip) => ip,
    None => "45.85.60.39",
};

fn default_server_port() -> u16 {
    option_env!("ODIUM_DEFAULT_SERVER_PORT")
        .and_then(|value| value.parse().ok())
        .unwrap_or(25592)
}

/// Fallback heap size for configs that omit `jvm.xmx`. New configs also set
/// `auto_xmx`, so the launcher replaces this with half of the installed RAM.
pub const DEFAULT_XMX: u32 = 4096;

/// How the launcher should interpret `manifest_url`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// JSON manifest produced by Import-Mrpack.ps1 or written by hand.
    #[default]
    Manifest,
    /// Modrinth `.mrpack` archive (a zip with `modrinth.index.json`).
    Mrpack,
}

impl SourceKind {
    /// Falls back to the file extension so existing configs keep working
    /// without an explicit `kind` field.
    pub fn detect(url: &str) -> Self {
        let path = url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .to_ascii_lowercase();
        if path.ends_with(".mrpack") {
            Self::Mrpack
        } else {
            Self::Manifest
        }
    }
}

/// A modpack source. `manifest_url` may point at GitHub raw, any HTTPS CDN,
/// a local `file://` manifest, or one of the embedded `builtin://` manifests.
/// For `SourceKind::Mrpack` it points at the `.mrpack` archive itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub manifest_url: String,
    /// Explicit format override. Omitted means "detect from the URL".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SourceKind>,
}

impl Source {
    pub fn kind(&self) -> SourceKind {
        self.kind
            .unwrap_or_else(|| SourceKind::detect(&self.manifest_url))
    }
}

/// Selectable visual style. Stored as `win98`, `win7`, `win81`, `win10` or
/// `win11`; unknown or missing values fall back to the classic Windows 98 look.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Win98,
    Win7,
    Win81,
    Win10,
    Win11,
}

impl Theme {
    pub const ALL: [Theme; 5] = [
        Theme::Win98,
        Theme::Win7,
        Theme::Win81,
        Theme::Win10,
        Theme::Win11,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Win98 => "Windows 98",
            Self::Win7 => "Windows 7",
            Self::Win81 => "Windows 8.1",
            Self::Win10 => "Windows 10",
            Self::Win11 => "Windows 11",
        }
    }
}

/// Loader types supported by the manifest. Serialized as lowercase strings.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LoaderKind {
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

impl fmt::Display for LoaderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fabric => f.write_str("fabric"),
            Self::Quilt => f.write_str("quilt"),
            Self::Forge => f.write_str("forge"),
            Self::NeoForge => f.write_str("neoforge"),
        }
    }
}

impl LoaderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fabric => "Fabric",
            Self::Quilt => "Quilt",
            Self::Forge => "Forge",
            Self::NeoForge => "NeoForge",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoaderSpec {
    #[serde(rename = "type")]
    pub kind: LoaderKind,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerSpec {
    pub ip: String,
    pub port: u16,
}

impl Default for ServerSpec {
    fn default() -> Self {
        Self {
            ip: DEFAULT_SERVER_IP.to_owned(),
            port: default_server_port(),
        }
    }
}

impl ServerSpec {
    pub fn display(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let ip = self.ip.trim();
        if ip.is_empty() || ip.len() > 253 || ip.chars().any(char::is_whitespace) {
            return Err(ConfigError::Validation(
                "IP/хост сервера пустой или содержит пробелы".to_owned(),
            ));
        }
        if self.port == 0 {
            return Err(ConfigError::Validation(
                "Порт сервера должен быть в диапазоне 1..65535".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JvmConfig {
    /// Maximum heap in MiB (`-Xmx`). Ignored while `auto_xmx` is set.
    #[serde(default = "default_xmx")]
    pub xmx: u32,
    /// Initial heap in MiB (`-Xms`).
    #[serde(default = "default_xms")]
    pub xms: u32,
    /// Use exactly half of the installed RAM instead of the stored `xmx`.
    /// New installs and distributed packs default to `true`; older configs
    /// keep their explicit value because the field is absent there.
    #[serde(default)]
    pub auto_xmx: bool,
}

impl Default for JvmConfig {
    fn default() -> Self {
        Self {
            xmx: default_xmx(),
            xms: default_xms(),
            auto_xmx: true,
        }
    }
}

impl JvmConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.xms < 512 {
            return Err(ConfigError::Validation(
                "Xms должен быть не меньше 512 МБ".to_owned(),
            ));
        }
        if self.xmx < self.xms {
            return Err(ConfigError::Validation(
                "Xmx не может быть меньше Xms".to_owned(),
            ));
        }
        if self.xmx > 65_536 {
            return Err(ConfigError::Validation(
                "Xmx выглядит нереально большим (максимум 65536 МБ)".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_nick")]
    pub nick: String,
    #[serde(default = "default_sources")]
    pub sources: Vec<Source>,
    #[serde(default)]
    pub active_source_id: String,
    #[serde(default)]
    pub jvm: JvmConfig,
    /// Global fallback. A manifest's own `server` object wins over this.
    #[serde(default)]
    pub server: ServerSpec,
    /// `source_id -> path -> enabled`. Only explicitly changed optional mods are stored.
    #[serde(default)]
    pub optional_mods_overrides: BTreeMap<String, BTreeMap<String, bool>>,
    #[serde(default = "default_true")]
    pub close_launcher_on_start: bool,
    #[serde(default = "default_concurrency")]
    pub download_concurrency: usize,
    /// `false` until the player finishes or skips the first-run guide.
    #[serde(default)]
    pub welcome_seen: bool,
    /// Visual style of the launcher window.
    #[serde(default, deserialize_with = "deserialize_theme")]
    pub theme: Theme,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            nick: default_nick(),
            sources: default_sources(),
            // The distributor's source is primary. The embedded vanilla entry
            // remains available as a network-free fallback.
            active_source_id: DEFAULT_SOURCE_ID.to_owned(),
            jvm: JvmConfig::default(),
            server: ServerSpec::default(),
            optional_mods_overrides: BTreeMap::new(),
            close_launcher_on_start: true,
            download_concurrency: default_concurrency(),
            welcome_seen: false,
            theme: Theme::Win98,
        }
    }
}

impl Config {
    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|source| source.id == id)
    }

    pub fn active_source(&self) -> Option<&Source> {
        self.source(&self.active_source_id)
    }

    /// Fixes benign inconsistencies from hand-edited configs without discarding data.
    pub fn repair(&mut self) {
        if self.sources.is_empty() {
            self.sources = default_sources();
        }
        // Source ids belong to the pack publisher: never rewrite them here.
        // A distributed config may legitimately use any id, including the
        // historical `eblyacraft` placeholder.
        let mut seen = std::collections::BTreeSet::new();
        self.sources.retain(|source| {
            if !is_valid_source_id(&source.id) || !seen.insert(source.id.clone()) {
                tracing::warn!(source_id = %source.id, "removing invalid/duplicate source");
                return false;
            }
            true
        });
        if self.sources.is_empty() {
            self.sources = default_sources();
        }
        if !self.sources.iter().any(|s| s.id == self.active_source_id) {
            if let Some(first) = self.sources.first() {
                self.active_source_id = first.id.clone();
            }
        }
        self.download_concurrency = self.download_concurrency.clamp(1, 16);
        if self.schema_version == 0 {
            self.schema_version = CONFIG_SCHEMA_VERSION;
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version > CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::Validation(format!(
                "config schema {} новее поддерживаемой {}",
                self.schema_version, CONFIG_SCHEMA_VERSION
            )));
        }
        validate_nick(&self.nick)?;
        self.jvm.validate()?;
        self.server.validate()?;
        if self.sources.is_empty() {
            return Err(ConfigError::Validation(
                "В config.json нет ни одной сборки".to_owned(),
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        for source in &self.sources {
            if !is_valid_source_id(&source.id) {
                return Err(ConfigError::Validation(format!(
                    "Некорректный id сборки: {}",
                    source.id
                )));
            }
            if !ids.insert(source.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "Повторяющийся id сборки: {}",
                    source.id
                )));
            }
            if source.name.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "У сборки {} пустое имя",
                    source.id
                )));
            }
            validate_manifest_url(&source.manifest_url)?;
        }
        if !self.sources.iter().any(|s| s.id == self.active_source_id) {
            return Err(ConfigError::Validation(
                "active_source_id не совпадает ни с одной сборкой".to_owned(),
            ));
        }
        Ok(())
    }
}

fn default_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

fn default_nick() -> String {
    "Player".to_owned()
}

fn default_xmx() -> u32 {
    DEFAULT_XMX
}

fn default_xms() -> u32 {
    1024
}

fn default_true() -> bool {
    true
}

fn default_concurrency() -> usize {
    8
}

/// Hand-edited configs may use spaces or a dotted 8.1 spelling. Unknown
/// values fall back to Windows 98 instead of rejecting the whole config.
fn deserialize_theme<'de, D>(deserializer: D) -> Result<Theme, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    let normalized = raw.trim().to_ascii_lowercase().replace([' ', '_'], "");
    Ok(match normalized.as_str() {
        "win7" | "windows7" => Theme::Win7,
        "win81" | "win8.1" | "windows8.1" => Theme::Win81,
        "win10" | "windows10" => Theme::Win10,
        "win11" | "windows11" => Theme::Win11,
        _ => Theme::Win98,
    })
}

/// The first source points at the published Odium `.mrpack`; the second is an
/// embedded, network-free manifest so a freshly built executable is always
/// usable.
pub fn default_sources() -> Vec<Source> {
    vec![
        Source {
            id: DEFAULT_SOURCE_ID.to_owned(),
            name: DEFAULT_SOURCE_NAME.to_owned(),
            manifest_url: DEFAULT_SOURCE_URL.to_owned(),
            kind: Some(SourceKind::detect(DEFAULT_SOURCE_URL)),
        },
        Source {
            id: "vanilla".to_owned(),
            name: "Vanilla 1.21.1 (Fabric)".to_owned(),
            manifest_url: "builtin://vanilla".to_owned(),
            kind: None,
        },
    ]
}

pub fn validate_nick(nick: &str) -> Result<(), ConfigError> {
    let len = nick.chars().count();
    if !(3..=16).contains(&len) {
        return Err(ConfigError::Validation(
            "Ник должен содержать от 3 до 16 символов".to_owned(),
        ));
    }
    if !nick.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ConfigError::Validation(
            "Ник может содержать только a-z, A-Z, 0-9 и _".to_owned(),
        ));
    }
    Ok(())
}

pub fn is_valid_source_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn validate_manifest_url(url: &str) -> Result<(), ConfigError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(ConfigError::Validation(
            "manifest_url не может быть пустым".to_owned(),
        ));
    }
    if url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("file://")
        || url.starts_with("builtin://")
    {
        Ok(())
    } else {
        Err(ConfigError::Validation(format!(
            "manifest_url должен начинаться с https://, http://, file:// или builtin://: {url}"
        )))
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("не удалось определить папку рядом с .exe")]
    NoExeDirectory,
    #[error("ошибка файловой системы {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("ошибка JSON в {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("некорректная конфигурация: {0}")]
    Validation(String),
}

/// Every path is derived from the executable location, never from `current_dir()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortablePaths {
    root: PathBuf,
}

impl PortablePaths {
    pub fn discover() -> Result<Self, ConfigError> {
        let exe = std::env::current_exe().map_err(|source| ConfigError::Io {
            path: PathBuf::from("<current_exe>"),
            source,
        })?;
        let dir = exe.parent().ok_or(ConfigError::NoExeDirectory)?;
        Self::from_root(dir.join("launcher_data"))
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let root = root.into();
        let paths = Self { root };
        for dir in [
            paths.root.clone(),
            paths.logs_dir(),
            paths.instances_dir(),
            paths.java_dir(),
            paths.assets_dir(),
            paths.libraries_dir(),
            paths.versions_dir(),
            paths.meta_dir(),
            paths.loaders_dir(),
            paths.temp_dir(),
        ] {
            fs::create_dir_all(&dir).map_err(|source| ConfigError::Io { path: dir, source })?;
        }
        Ok(paths)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn instances_dir(&self) -> PathBuf {
        self.root.join("instances")
    }

    pub fn instance_dir(&self, source_id: &str) -> Result<PathBuf, ConfigError> {
        if !is_valid_source_id(source_id) {
            return Err(ConfigError::Validation(format!(
                "небезопасный source_id: {source_id}"
            )));
        }
        Ok(self.instances_dir().join(source_id))
    }

    pub fn java_dir(&self) -> PathBuf {
        self.root.join("java")
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.root.join("assets")
    }

    pub fn libraries_dir(&self) -> PathBuf {
        self.root.join("libraries")
    }

    pub fn versions_dir(&self) -> PathBuf {
        self.root.join("versions")
    }

    pub fn meta_dir(&self) -> PathBuf {
        self.root.join("meta")
    }

    pub fn loaders_dir(&self) -> PathBuf {
        self.root.join("loaders")
    }

    pub fn temp_dir(&self) -> PathBuf {
        self.root.join("temp")
    }
}

pub struct ConfigStore {
    path: PathBuf,
    config: Config,
}

impl ConfigStore {
    /// Loads the config, repairs harmless omissions and creates a default file.
    /// A syntactically broken config is renamed to `config.json.broken.<unix>` so
    /// the user never loses the original bytes.
    pub fn load_or_create(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let config = if path.exists() {
            let raw = fs::read_to_string(&path).map_err(|source| ConfigError::Io {
                path: path.clone(),
                source,
            })?;
            match serde_json::from_str::<Config>(&raw) {
                Ok(config) => config,
                Err(source) => {
                    let backup = broken_path(&path);
                    tracing::error!(
                        path = %path.display(),
                        backup = %backup.display(),
                        error = %source,
                        "config.json is broken; backing it up and creating a default"
                    );
                    fs::rename(&path, &backup).map_err(|source| ConfigError::Io {
                        path: path.clone(),
                        source,
                    })?;
                    Config::default()
                }
            }
        } else {
            Config::default()
        };

        let mut store = Self { path, config };
        store.config.repair();
        store.config.validate()?;
        store.save()?;
        Ok(store)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.config.validate()?;
        save_json_atomic(&self.path, &self.config)
    }
}

fn broken_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".broken.{stamp}"));
    path.with_file_name(name)
}

/// Writes `value` to a sibling `.tmp` file, fsyncs it and then atomically renames.
/// `std::fs::rename` on Windows maps to `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING)`,
/// so readers never observe a half-written config.json.
pub fn save_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigError::Validation(format!("у пути нет родительской папки: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let bytes = serde_json::to_vec_pretty(value).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);

    {
        let mut file = File::create(&tmp).map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(&bytes).map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(b"\n").map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
    }

    if let Err(source) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> Config {
        Config {
            schema_version: 1,
            nick: "Steve_01".to_owned(),
            sources: vec![Source {
                id: "survival".to_owned(),
                name: "Survival".to_owned(),
                manifest_url: "https://raw.githubusercontent.com/o/r/main/manifest.json".to_owned(),
                kind: None,
            }],
            active_source_id: "survival".to_owned(),
            jvm: JvmConfig {
                xmx: 6144,
                xms: 1024,
                auto_xmx: false,
            },
            server: ServerSpec {
                ip: "45.85.60.39".to_owned(),
                port: 25592,
            },
            optional_mods_overrides: BTreeMap::from([(
                "survival".to_owned(),
                BTreeMap::from([("mods/shaders.jar".to_owned(), true)]),
            )]),
            close_launcher_on_start: false,
            download_concurrency: 6,
            welcome_seen: true,
            theme: Theme::Win10,
        }
    }

    #[test]
    fn config_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let original = sample_config();
        let json = serde_json::to_string_pretty(&original)?;
        let restored: Config = serde_json::from_str(&json)?;
        assert_eq!(original, restored);
        Ok(())
    }

    #[test]
    fn config_save_is_atomic_and_readable() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.json");
        let config = sample_config();
        save_json_atomic(&path, &config)?;
        let raw = fs::read_to_string(&path)?;
        let restored: Config = serde_json::from_str(&raw)?;
        assert_eq!(config, restored);
        assert!(!path.with_file_name("config.json.tmp").exists());
        Ok(())
    }

    #[test]
    fn nick_validation() {
        for valid in ["Steve", "Alex_123", "a_b", "ABCDEFGHIJKLMNOP"] {
            assert!(validate_nick(valid).is_ok(), "{valid} should be valid");
        }
        for invalid in ["ab", "abcdefghijklmnopq", "bad name", "bad-name", "ник", ""] {
            assert!(
                validate_nick(invalid).is_err(),
                "{invalid} should be invalid"
            );
        }
    }

    #[test]
    fn source_id_validation() {
        for valid in ["vanilla", "my_pack-01", "A1"] {
            assert!(is_valid_source_id(valid));
        }
        for invalid in ["", "../escape", "a/b", "a.b", "пакет"] {
            assert!(!is_valid_source_id(invalid));
        }
    }

    #[test]
    fn default_config_is_valid() {
        let config = Config::default();
        assert!(config.validate().is_ok());
        assert!(config.active_source().is_some());
        assert_eq!(config.active_source_id, DEFAULT_SOURCE_ID);
        assert!(config.jvm.auto_xmx, "new installs use half of the RAM");
    }

    #[test]
    fn legacy_config_without_auto_xmx_keeps_explicit_heap() -> Result<(), Box<dyn std::error::Error>>
    {
        let config: Config = serde_json::from_value(serde_json::json!({
            "sources": [{
                "id": "pack",
                "name": "Pack",
                "manifest_url": "https://example.com/manifest.json"
            }],
            "active_source_id": "pack",
            "jvm": { "xmx": 6144, "xms": 1024 },
            "server": { "ip": "1.2.3.4", "port": 25565 }
        }))?;
        assert!(!config.jvm.auto_xmx);
        assert_eq!(config.jvm.xmx, 6144);
        Ok(())
    }

    #[test]
    fn repair_fixes_missing_active_source() {
        let mut config = Config {
            active_source_id: "missing".to_owned(),
            ..Config::default()
        };
        config.repair();
        assert!(config.active_source().is_some());
    }

    #[test]
    fn repair_keeps_publisher_source_ids() {
        let mut config = sample_config();
        config.sources[0].id = "eblyacraft".to_owned();
        config.active_source_id = "eblyacraft".to_owned();
        config.repair();
        assert_eq!(config.sources.len(), 1);
        assert_eq!(config.sources[0].id, "eblyacraft");
        assert_eq!(config.active_source_id, "eblyacraft");
        assert_eq!(
            config.sources[0].manifest_url,
            "https://raw.githubusercontent.com/o/r/main/manifest.json"
        );
    }

    #[test]
    fn legacy_config_defaults_to_windows_98_theme() -> Result<(), Box<dyn std::error::Error>> {
        let config: Config = serde_json::from_value(serde_json::json!({
            "sources": [{
                "id": "pack",
                "name": "Pack",
                "manifest_url": "https://example.com/manifest.json"
            }],
            "active_source_id": "pack",
            "server": { "ip": "1.2.3.4", "port": 25565 }
        }))?;
        assert_eq!(config.theme, Theme::Win98);
        Ok(())
    }

    #[test]
    fn theme_ids_are_stable() -> Result<(), Box<dyn std::error::Error>> {
        let expected = ["win98", "win7", "win81", "win10", "win11"];
        for (theme, id) in Theme::ALL.into_iter().zip(expected) {
            assert_eq!(serde_json::to_value(theme)?.as_str(), Some(id));
            let restored: Theme = serde_json::from_value(serde_json::Value::String(id.to_owned()))?;
            assert_eq!(restored, theme);
        }
        Ok(())
    }

    #[test]
    fn unknown_theme_falls_back_to_windows_98() -> Result<(), Box<dyn std::error::Error>> {
        let config: Config = serde_json::from_value(serde_json::json!({
            "theme": "windows 12",
            "sources": [{
                "id": "pack",
                "name": "Pack",
                "manifest_url": "https://example.com/manifest.json"
            }],
            "active_source_id": "pack",
            "server": { "ip": "1.2.3.4", "port": 25565 }
        }))?;
        assert_eq!(config.theme, Theme::Win98);

        let spaced: Config = serde_json::from_value(serde_json::json!({
            "theme": "Windows 8.1",
            "sources": [{
                "id": "pack",
                "name": "Pack",
                "manifest_url": "https://example.com/manifest.json"
            }],
            "active_source_id": "pack",
            "server": { "ip": "1.2.3.4", "port": 25565 }
        }))?;
        assert_eq!(spaced.theme, Theme::Win81);
        Ok(())
    }
}
