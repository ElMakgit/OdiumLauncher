//! Data paths, the user-facing `config.json` schema and crash-safe persistence.
//!
//! The launcher never writes to the registry and never uses the current working
//! directory. By default every mutable byte lives in the per-user application
//! data directory (`%LOCALAPPDATA%\OdiumLauncher` on Windows,
//! `$XDG_DATA_HOME/odium-launcher` elsewhere), so the executable can sit on the
//! Desktop or in a Downloads folder without scattering gigabytes next to it.
//!
//! `ODIUM_DATA_DIR` overrides the root. Setting `ODIUM_PORTABLE=1` (or creating
//! a `portable.flag` file next to the executable) restores the old behaviour:
//! `launcher_data` beside the executable. A legacy folder found on first start
//! is moved into the new root automatically, preserving configs, mods and saves.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod uninstall;

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

/// Text scale shipped to new players, in percent. The UI defaults are tuned on
/// a 1080p screen and read small on laptops, so the initial value is one step
/// above 100%.
pub const DEFAULT_FONT_SCALE: u8 = 110;

fn default_font_scale() -> u8 {
    DEFAULT_FONT_SCALE
}

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

    /// Applies `auto_xmx` and clamps both heaps to the machine's RAM. This is
    /// the single source of truth for the UI and for headless/domain callers.
    pub fn resolved(&self, total_ram_mb: u32) -> (u32, u32) {
        let max_xmx = total_ram_mb.saturating_sub(1024).clamp(1024, 65_536);
        let xmx = if self.auto_xmx {
            (total_ram_mb / 2).max(1024)
        } else {
            self.xmx
        };
        let xmx = xmx.clamp(1024, max_xmx);
        let xms = self.xms.clamp(512, xmx);
        (xms, xmx)
    }

    /// Clamps a hand-edited config into the accepted range without discarding
    /// the rest of the file.
    fn repair(&mut self) {
        self.xmx = self.xmx.clamp(512, 65_536);
        self.xms = self.xms.clamp(512, self.xmx);
    }
}

/// Physical RAM in MiB. Used to resolve `auto_xmx` outside the UI as well.
pub fn total_system_ram_mb() -> u32 {
    #[cfg(windows)]
    {
        use std::mem::MaybeUninit;
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: MEMORYSTATUSEX is a plain C struct. We zero it, set dwLength
        // as required by the API and only read fields after a non-zero result.
        unsafe {
            let mut status = MaybeUninit::<MEMORYSTATUSEX>::zeroed();
            let pointer = status.as_mut_ptr();
            (*pointer).dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(pointer) != 0 {
                return ((*pointer).ullTotalPhys / (1024 * 1024)) as u32;
            }
        }
    }
    // Conservative fallback for non-Windows builds and failed API calls.
    8192
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
    /// Per-source JVM settings. A pack with heavy mods can keep a larger heap
    /// without forcing the same value on every other instance. Missing entries
    /// fall back to the global `jvm` block.
    #[serde(default)]
    pub instance_jvm: BTreeMap<String, JvmConfig>,
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
    /// Text scale in percent (80-160). Older configs get [`DEFAULT_FONT_SCALE`].
    #[serde(default = "default_font_scale")]
    pub font_scale: u8,
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
            instance_jvm: BTreeMap::new(),
            close_launcher_on_start: true,
            download_concurrency: default_concurrency(),
            welcome_seen: false,
            theme: Theme::Win98,
            font_scale: default_font_scale(),
        }
    }
}

impl Config {
    /// Text scale as a multiplier for painters, clamped to the supported range.
    pub fn text_scale(&self) -> f32 {
        f32::from(self.font_scale.clamp(80, 160)) / 100.0
    }
    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|source| source.id == id)
    }

    pub fn active_source(&self) -> Option<&Source> {
        self.source(&self.active_source_id)
    }

    /// JVM settings for a source, falling back to the global block.
    pub fn jvm_for(&self, source_id: &str) -> JvmConfig {
        self.instance_jvm
            .get(source_id)
            .cloned()
            .unwrap_or_else(|| self.jvm.clone())
    }

    /// Remembers a source's heap settings without touching the global block.
    pub fn set_jvm_for(&mut self, source_id: &str, jvm: JvmConfig) {
        self.instance_jvm.insert(source_id.to_owned(), jvm);
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
        // A syntactically valid but out-of-range heap used to fail validation
        // and abort the whole start without the `.broken` backup. Clamp it
        // instead; the user keeps every other setting.
        self.jvm.repair();
        for jvm in self.instance_jvm.values_mut() {
            jvm.repair();
        }
        if validate_nick(&self.nick).is_err() {
            tracing::warn!(nick = %self.nick, "invalid nick in config, resetting to default");
            self.nick = default_nick();
        }
        if self.server.validate().is_err() {
            tracing::warn!("invalid server in config, resetting to default");
            self.server = ServerSpec::default();
        }
        for source in &mut self.sources {
            if source.name.trim().is_empty() {
                source.name = source.id.clone();
            }
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

/// Overrides the data root completely. Useful for tests and managed installs.
pub const DATA_DIR_ENV: &str = "ODIUM_DATA_DIR";
/// Opts back into a `launcher_data` folder next to the executable.
pub const PORTABLE_ENV: &str = "ODIUM_PORTABLE";
pub const PORTABLE_MARKER: &str = "portable.flag";
pub const LEGACY_DIR_NAME: &str = "launcher_data";
const SYSTEM_APP_DIR: &str = "OdiumLauncher";

/// Resolves the mutable-data root. Never uses `current_dir()`: the executable
/// location is only the fallback and the source of the legacy folder.
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
        let exe_dir = exe.parent().ok_or(ConfigError::NoExeDirectory)?;

        if let Some(root) = std::env::var_os(DATA_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            return Self::from_root(root);
        }

        let legacy = exe_dir.join(LEGACY_DIR_NAME);
        if env_flag(PORTABLE_ENV)
            || exe_dir.join(PORTABLE_MARKER).is_file()
            || is_removable_path(exe_dir)
        {
            return Self::from_root(legacy);
        }

        let Some(root) = system_data_root() else {
            // No usable system directory (for example a stripped-down session
            // without USERPROFILE): fall back to the old portable layout rather
            // than refusing to start.
            return Self::from_root(legacy);
        };
        migrate_legacy_data(&legacy, &root);
        Self::from_root(root)
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
            paths.backups_dir(),
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

    /// Rotated snapshots of worlds and configs taken before a pack update.
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| {
        let value = value.to_string_lossy();
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

/// True when the executable lives on a removable drive (USB stick, SD card).
/// A USB copy of the launcher must keep its data on the stick instead of
/// silently migrating it into the host machine's `%LOCALAPPDATA%`.
#[cfg(windows)]
fn is_removable_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

    const DRIVE_REMOVABLE: u32 = 2;
    let Some(std::path::Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let mut root = prefix.as_os_str().encode_wide().collect::<Vec<_>>();
    if !root.ends_with(&[u16::from(b'\\')]) {
        root.push(u16::from(b'\\'));
    }
    root.push(0);
    // SAFETY: `root` is a NUL-terminated UTF-16 drive root.
    unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOVABLE }
}

#[cfg(not(windows))]
fn is_removable_path(_path: &Path) -> bool {
    false
}

/// Per-user application data directory. On Windows this is
/// `%LOCALAPPDATA%\OdiumLauncher`; on other systems the XDG data directory.
fn system_data_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // `LOCALAPPDATA` is the canonical place for application data that does
        // not roam with the user profile. `APPDATA` is a fallback for unusual
        // service accounts where only the roaming variable is defined.
        if let Some(base) = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("APPDATA").filter(|value| !value.is_empty()))
        {
            return Some(PathBuf::from(base).join(SYSTEM_APP_DIR));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Some(
                PathBuf::from(profile)
                    .join("AppData")
                    .join("Local")
                    .join(SYSTEM_APP_DIR),
            );
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        // Native macOS location: ~/Library/Application Support/OdiumLauncher.
        if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
            return Some(
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join(SYSTEM_APP_DIR),
            );
        }
        None
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(base) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(base).join("odium-launcher"));
        }
        if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
            return Some(
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("odium-launcher"),
            );
        }
        None
    }
}

/// Moves a pre-relocation `launcher_data` folder into the system data root.
///
/// A same-volume `rename` is instant even for multi-gigabyte asset folders; a
/// recursive copy is the fallback for a launcher that lives on another drive.
/// The legacy folder is kept when copying fails, and a partially copied target
/// is removed so the next start can retry cleanly.
fn migrate_legacy_data(legacy: &Path, target: &Path) {
    if legacy == target || !legacy.is_dir() {
        return;
    }
    if target.is_dir() {
        // An already initialised root belongs to the user: only seed a missing
        // config from a packaged `launcher_data/config.json` (the dist layout)
        // and never touch the user's real data.
        if target.join("config.json").exists() {
            let seed = legacy.join("config.json");
            let destination = target.join("config.json");
            if seed.is_file() && !destination.exists() {
                match fs::copy(&seed, &destination) {
                    Ok(_) => tracing::info!(
                        from = %seed.display(),
                        to = %destination.display(),
                        "seeded config.json from the packaged launcher_data"
                    ),
                    Err(error) => tracing::warn!(
                        %error,
                        from = %seed.display(),
                        "could not seed config.json"
                    ),
                }
            }
            return;
        }

        // The root exists but was never initialised (for example an earlier
        // build created only a cache subfolder). Merge the legacy data in
        // without overwriting anything already present, then drop the legacy
        // folder so the next start does not repeat the work.
        match merge_dir_recursive(legacy, target) {
            Ok(entries) => match fs::remove_dir_all(legacy) {
                Ok(()) => tracing::info!(
                    entries,
                    from = %legacy.display(),
                    to = %target.display(),
                    "merged the legacy launcher_data into the system data directory"
                ),
                Err(error) => tracing::warn!(
                    %error,
                    path = %legacy.display(),
                    "merged launcher data but could not remove the legacy folder"
                ),
            },
            Err(error) => tracing::warn!(
                %error,
                from = %legacy.display(),
                to = %target.display(),
                "could not merge launcher_data; keeping the legacy folder"
            ),
        }
        return;
    }

    if let Some(parent) = target.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            tracing::warn!(%error, path = %parent.display(), "cannot create the data root");
            return;
        }
    }

    match fs::rename(legacy, target) {
        Ok(()) => {
            tracing::info!(
                from = %legacy.display(),
                to = %target.display(),
                "moved launcher_data into the system data directory"
            );
            return;
        }
        Err(error) => tracing::info!(
            %error,
            from = %legacy.display(),
            "rename failed; copying launcher data instead"
        ),
    }

    match copy_dir_recursive(legacy, target) {
        Ok(bytes) => match fs::remove_dir_all(legacy) {
            Ok(()) => tracing::info!(
                bytes,
                from = %legacy.display(),
                to = %target.display(),
                "copied and removed the legacy launcher_data"
            ),
            Err(error) => tracing::warn!(
                %error,
                path = %legacy.display(),
                "copied launcher data but could not remove the legacy folder"
            ),
        },
        Err(error) => {
            tracing::warn!(
                %error,
                from = %legacy.display(),
                to = %target.display(),
                "could not migrate launcher_data; keeping the legacy folder"
            );
            let _ = fs::remove_dir_all(target);
        }
    }
}

/// Non-destructive merge used when the system root exists but is incomplete.
/// Existing destination files win; top-level entries are moved with `rename`
/// when possible so a same-volume migration stays instant.
fn merge_dir_recursive(source: &Path, destination: &Path) -> std::io::Result<u64> {
    let mut entries = 0u64;
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if file_type.is_dir() {
            if !to.exists() && fs::rename(&from, &to).is_ok() {
                entries = entries.saturating_add(count_entries(&to));
            } else {
                entries = entries.saturating_add(merge_dir_recursive(&from, &to)?);
            }
        } else if file_type.is_file() {
            if to.exists() {
                // Never overwrite data that the system root already holds.
                continue;
            }
            if fs::rename(&from, &to).is_err() {
                fs::copy(&from, &to)?;
            }
            entries = entries.saturating_add(1);
        }
        // Symlinks and other special entries are intentionally skipped.
    }
    Ok(entries)
}

fn count_entries(root: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(root) else {
        return total;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => {
                total = total.saturating_add(count_entries(&entry.path()))
            }
            Ok(file_type) if file_type.is_file() => total = total.saturating_add(1),
            _ => {}
        }
    }
    total
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> std::io::Result<u64> {
    let mut bytes = 0u64;
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if file_type.is_dir() {
            bytes = bytes.saturating_add(copy_dir_recursive(&from, &to)?);
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
            bytes = bytes.saturating_add(entry.metadata()?.len());
        }
        // Symlinks and other special entries are intentionally skipped: they
        // are not produced by the launcher and copying them could escape the
        // destination on Windows.
    }
    Ok(bytes)
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
        if let Err(error) = store.config.validate() {
            // `repair()` handles the common hand-edit mistakes. Anything that
            // still fails (for example an unsupported schema version) must not
            // leave the player with a launcher that refuses to start: keep the
            // original bytes in `.broken.<timestamp>` and continue with defaults.
            tracing::error!(
                path = %store.path.display(),
                %error,
                "config.json is invalid; backing it up and creating a default"
            );
            let backup = broken_path(&store.path);
            fs::rename(&store.path, &backup).map_err(|source| ConfigError::Io {
                path: store.path.clone(),
                source,
            })?;
            store.config = Config::default();
        }
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
            instance_jvm: BTreeMap::new(),
            close_launcher_on_start: false,
            download_concurrency: 6,
            welcome_seen: true,
            theme: Theme::Win10,
            font_scale: 120,
        }
    }

    #[test]
    fn old_configs_get_the_default_text_scale() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = serde_json::to_value(sample_config())?;
        let object = value.as_object_mut().expect("config object");
        object.remove("font_scale");
        let restored: Config = serde_json::from_value(value)?;
        assert_eq!(restored.font_scale, DEFAULT_FONT_SCALE);
        assert!((restored.text_scale() - 1.1).abs() < 0.001);
        Ok(())
    }

    #[test]
    fn text_scale_is_bounded() {
        let low = Config {
            font_scale: 0,
            ..Config::default()
        };
        assert!((low.text_scale() - 0.8).abs() < 0.001);
        let high = Config {
            font_scale: 255,
            ..Config::default()
        };
        assert!((high.text_scale() - 1.6).abs() < 0.001);
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
    fn legacy_launcher_data_is_migrated() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let legacy = temp.path().join("launcher_data");
        let target = temp.path().join("system").join("OdiumLauncher");
        fs::create_dir_all(legacy.join("instances").join("odium"))?;
        fs::write(legacy.join("config.json"), b"{\"nick\":\"Steve\"}")?;
        fs::write(
            legacy.join("instances").join("odium").join("level.dat"),
            b"world",
        )?;

        migrate_legacy_data(&legacy, &target);

        assert!(!legacy.exists(), "legacy folder must be moved");
        assert_eq!(
            fs::read(target.join("config.json"))?,
            b"{\"nick\":\"Steve\"}"
        );
        assert_eq!(
            fs::read(target.join("instances").join("odium").join("level.dat"))?,
            b"world"
        );
        Ok(())
    }

    #[test]
    fn existing_system_root_only_receives_a_missing_config(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let legacy = temp.path().join("launcher_data");
        let target = temp.path().join("system");
        fs::create_dir_all(&legacy)?;
        fs::create_dir_all(&target)?;
        fs::write(legacy.join("config.json"), b"packaged")?;
        fs::write(target.join("config.json"), b"user")?;
        fs::write(target.join("instances.marker"), b"keep")?;

        migrate_legacy_data(&legacy, &target);

        assert_eq!(fs::read(target.join("config.json"))?, b"user");
        assert!(target.join("instances.marker").is_file());
        assert!(
            legacy.exists(),
            "an existing root must not consume legacy data"
        );
        Ok(())
    }

    #[test]
    fn incomplete_system_root_is_merged_with_legacy_data() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let legacy = temp.path().join("launcher_data");
        let target = temp.path().join("system").join("OdiumLauncher");
        fs::create_dir_all(legacy.join("assets").join("objects"))?;
        fs::write(legacy.join("config.json"), b"{\"nick\":\"Steve\"}")?;
        fs::write(
            legacy.join("assets").join("objects").join("a.bin"),
            b"asset",
        )?;
        fs::create_dir_all(target.join("mrpack-cache"))?;
        fs::write(target.join("mrpack-cache").join("keep.tmp"), b"cache")?;

        migrate_legacy_data(&legacy, &target);

        assert!(!legacy.exists(), "legacy folder must be consumed");
        assert_eq!(
            fs::read(target.join("config.json"))?,
            b"{\"nick\":\"Steve\"}"
        );
        assert_eq!(
            fs::read(target.join("assets").join("objects").join("a.bin"))?,
            b"asset"
        );
        assert_eq!(
            fs::read(target.join("mrpack-cache").join("keep.tmp"))?,
            b"cache"
        );
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
