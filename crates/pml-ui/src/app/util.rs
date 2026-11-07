//! Small, non-orchestration helpers shared by the views and the publisher.

use std::path::{Path, PathBuf};

use pml_config::Theme;

#[derive(Debug, Clone)]
pub(crate) struct PackInfo {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) mc_version: String,
    pub(crate) loader: String,
}

pub(crate) fn theme_description(theme: Theme) -> &'static str {
    match theme {
        Theme::Win98 => "Классический серый интерфейс с объёмными кнопками",
        Theme::Win7 => "Стеклянные заголовки и голубые кнопки Aero",
        Theme::Win81 => "Плоские элементы Metro и синий акцент",
        Theme::Win10 => "Строгий интерфейс с синей полосой заголовка",
        Theme::Win11 => "Скруглённые углы и мягкие поверхности Mica",
    }
}

pub(crate) fn sunken_well<R>(
    ui: &mut egui::Ui,
    theme: Theme,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let inner = egui::Frame::none()
        .fill(pml_theme::well_color(theme))
        .rounding(egui::Rounding::same(pml_theme::well_rounding(theme)))
        .inner_margin(egui::Margin::same(4.0))
        .show(ui, add_contents);
    pml_theme::paint_bevel(ui.painter(), inner.response.rect, false, theme);
    inner.inner
}

pub(crate) fn total_system_ram_mb() -> u32 {
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

pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.2} ГБ", value / GB)
    } else if value >= MB {
        format!("{:.1} МБ", value / MB)
    } else {
        format!("{:.0} КБ", value / KB)
    }
}

pub(crate) fn read_pack_info(path: &Path) -> Option<PackInfo> {
    if path.is_dir() {
        let manifest = path.join("manifest.json");
        let bytes = std::fs::read(manifest).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let text = |key: &str| {
            value
                .get(key)
                .and_then(|item| item.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let loader = value
            .get("loader")
            .map(|loader| {
                format!(
                    "{} {}",
                    loader
                        .get("kind")
                        .and_then(|item| item.as_str())
                        .unwrap_or(""),
                    loader
                        .get("version")
                        .and_then(|item| item.as_str())
                        .unwrap_or("")
                )
            })
            .unwrap_or_default();
        return Some(PackInfo {
            name: {
                let name = text("name");
                if name.is_empty() {
                    "Odium pack".to_owned()
                } else {
                    name
                }
            },
            version: text("version"),
            mc_version: text("mc_version"),
            loader: loader.trim().to_owned(),
        });
    }

    let index = pml_modpack::read_index(path).ok()?;
    let loader = index
        .loader()
        .ok()
        .map(|loader| format!("{} {}", loader.kind.label(), loader.version))
        .unwrap_or_default();
    Some(PackInfo {
        name: if index.name.trim().is_empty() {
            "Modrinth pack".to_owned()
        } else {
            index.name.clone()
        },
        version: index.version_id.clone(),
        mc_version: index.mc_version().unwrap_or_default().to_owned(),
        loader,
    })
}

pub(crate) fn default_dist_dir(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("dist")
    } else {
        path.parent().unwrap_or_else(|| Path::new(".")).join("dist")
    }
}

pub(crate) fn open_path(path: &Path) {
    #[cfg(windows)]
    let command = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(windows))]
    let command = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(error) = command {
        tracing::warn!(%error, path = %path.display(), "could not open folder");
    }
}
