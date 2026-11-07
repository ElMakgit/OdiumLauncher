//! Launcher window state and lifecycle.
//!
//! The UI thread never performs I/O. `pipeline` owns the Tokio task
//! (manifest -> Java -> loader -> modpack -> Minecraft -> JVM) and reports
//! through an unbounded channel; `update` drains that channel once per frame.
//! Downloads additionally stream `DownloadEvent`s into the same channel.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use pml_config::{ConfigStore, PortablePaths, Source, Theme};
use pml_downloader::{DownloadEvent, Downloader};
use pml_java::JavaInstallation;
use pml_launcher::tail_log;
use pml_modpack::{load_pack, Manifest};
use tokio::sync::mpsc;

use crate::glass;

mod diagnostics;
mod pipeline;
mod publisher;
mod util;
mod views;
mod welcome;

use pipeline::UiEvent;
use publisher::PublisherState;
use util::{format_bytes, total_system_ram_mb};
use welcome::WelcomeState;

#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error(transparent)]
    Config(#[from] pml_config::ConfigError),
    #[error(transparent)]
    Download(#[from] pml_downloader::DownloadError),
    #[error("eframe: {0}")]
    Eframe(String),
}

/// Creates the native window. Must be called from the main thread on Windows.
pub fn run(
    paths: PortablePaths,
    store: ConfigStore,
    runtime: tokio::runtime::Handle,
) -> Result<(), UiError> {
    let downloader = Downloader::new(store.config().download_concurrency)?;
    let theme = store.config().theme;
    let dog_icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/dog_face.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([920.0, 660.0])
        .with_min_inner_size([840.0, 600.0])
        .with_transparent(true)
        .with_title(pml_config::BRAND_NAME);
    if let Some(icon) = &dog_icon {
        viewport = viewport.with_icon(icon.clone());
    }

    // The snapshot has to be taken before eframe creates the window, otherwise
    // the launcher would photograph itself. Opaque themes never need it, so
    // they skip the capture (and the blur worker) entirely.
    let backdrop = if theme == Theme::Win7 {
        glass::Backdrop::capture_now()
    } else {
        glass::Backdrop::empty()
    };
    let options = eframe::NativeOptions {
        viewport,
        persist_window: false,
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        pml_config::BRAND_NAME,
        options,
        Box::new(move |creation| {
            pml_theme::apply(&creation.egui_ctx, theme);
            Ok(Box::new(LauncherApp::new(
                paths,
                store,
                runtime,
                downloader,
                backdrop,
                dog_icon,
                &creation.egui_ctx,
            )))
        }),
    )
    .map_err(|error| UiError::Eframe(error.to_string()))
}

struct LauncherApp {
    paths: PortablePaths,
    store: ConfigStore,
    runtime: tokio::runtime::Handle,
    downloader: Downloader,
    events_rx: Option<mpsc::UnboundedReceiver<UiEvent>>,
    cancel: Option<Arc<AtomicBool>>,
    manifest_rx: Option<mpsc::UnboundedReceiver<Result<Manifest, String>>>,
    manifest: Option<Manifest>,
    manifest_error: Option<String>,
    java: Option<JavaInstallation>,
    source_id: String,
    nick: String,
    xms: u32,
    xmx: u32,
    auto_xmx: bool,
    max_xmx: u32,
    total_ram_mb: u32,
    close_on_start: bool,
    status: String,
    progress: f32,
    busy: bool,
    game_running: bool,
    log_path: Option<PathBuf>,
    log_lines: Vec<String>,
    last_log_poll: Instant,
    error: Option<String>,
    auto_refreshed: bool,
    publisher: PublisherState,
    welcome: WelcomeState,
    theme: Theme,
    glass_enabled: Option<bool>,
    hwnd: Option<isize>,
    backdrop: glass::Backdrop,
    dog_texture: Option<egui::TextureHandle>,
    settings_open: bool,
}

impl LauncherApp {
    fn new(
        paths: PortablePaths,
        store: ConfigStore,
        runtime: tokio::runtime::Handle,
        downloader: Downloader,
        backdrop: glass::Backdrop,
        dog_icon: Option<egui::IconData>,
        ctx: &egui::Context,
    ) -> Self {
        let config = store.config();
        let welcome_open = !config.welcome_seen;
        let theme = config.theme;
        let dog_texture = dog_icon.map(|icon| {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            );
            ctx.load_texture("dog_face", image, egui::TextureOptions::LINEAR)
        });
        // Leave at least 1 GiB for Windows and background apps, and never offer
        // more than the physical RAM installed in this machine.
        let total_ram_mb = total_system_ram_mb();
        let max_xmx = total_ram_mb.saturating_sub(1024).clamp(1024, 65_536);
        // New installs use exactly half of the physical RAM. Moving the Xmx
        // slider disables auto mode so an explicit choice survives restarts.
        let auto_xmx = config.jvm.auto_xmx;
        let xmx = if auto_xmx {
            (total_ram_mb / 2).max(1024)
        } else {
            config.jvm.xmx
        };
        let xmx = xmx.clamp(1024, max_xmx);
        let xms = config.jvm.xms.clamp(512, xmx);
        Self {
            paths,
            source_id: config.active_source_id.clone(),
            nick: config.nick.clone(),
            xms,
            xmx,
            auto_xmx,
            max_xmx,
            total_ram_mb,
            close_on_start: config.close_launcher_on_start,
            store,
            runtime,
            downloader,
            events_rx: None,
            cancel: None,
            manifest_rx: None,
            manifest: None,
            manifest_error: None,
            java: None,
            status: "Готов к запуску".to_owned(),
            progress: 0.0,
            busy: false,
            game_running: false,
            log_path: None,
            log_lines: Vec::new(),
            last_log_poll: Instant::now(),
            error: None,
            auto_refreshed: false,
            publisher: PublisherState::new(),
            welcome: WelcomeState {
                open: welcome_open,
                step: 0,
            },
            theme,
            glass_enabled: None,
            hwnd: None,
            backdrop,
            dog_texture,
            settings_open: false,
        }
    }

    fn selected_source(&self) -> Option<Source> {
        self.store.config().source(&self.source_id).cloned()
    }

    fn persist(&mut self) {
        {
            let config = self.store.config_mut();
            config.nick = self.nick.clone();
            config.jvm.xms = self.xms;
            config.jvm.xmx = self.xmx;
            config.jvm.auto_xmx = self.auto_xmx;
            config.close_launcher_on_start = self.close_on_start;
            config.active_source_id = self.source_id.clone();
            config.theme = self.theme;
        }
        if let Err(error) = self.store.save() {
            self.error = Some(format!("Не удалось сохранить config.json: {error}"));
        }
    }

    fn select_source(&mut self, id: &str) {
        if self.source_id == id {
            return;
        }
        self.source_id = id.to_owned();
        self.manifest = None;
        self.manifest_error = None;
        self.persist();
        self.refresh_manifest();
    }

    fn set_theme(&mut self, ctx: &egui::Context, theme: Theme) {
        if self.theme == theme {
            return;
        }
        let entering_glass = theme == Theme::Win7;
        self.theme = theme;
        pml_theme::apply(ctx, theme);
        self.store.config_mut().theme = theme;
        if let Err(error) = self.store.save() {
            self.error = Some(format!("Не удалось сохранить config.json: {error}"));
        }
        if entering_glass {
            // The window already exists, so hide it briefly while the desktop
            // is photographed; otherwise the snapshot would contain the UI.
            self.backdrop.recapture(self.hwnd);
        }
        ctx.request_repaint();
    }

    /// Turns the DWM acrylic backdrop on for the Windows 7 theme and off for
    /// every opaque theme. Called once per state change; DWM calls are not free.
    fn sync_glass(&mut self, frame: &eframe::Frame) {
        if self.hwnd.is_none() {
            self.hwnd = glass::window_handle(frame);
        }
        let wanted = self.theme == Theme::Win7;
        if self.glass_enabled == Some(wanted) {
            return;
        }
        match self.hwnd {
            Some(hwnd) => {
                glass::set_enabled(hwnd, wanted);
                self.glass_enabled = Some(wanted);
            }
            None => {
                tracing::warn!("window handle is unavailable; liquid glass is disabled");
                self.glass_enabled = Some(false);
            }
        }
    }

    fn refresh_manifest(&mut self) {
        let Some(source) = self.selected_source() else {
            self.manifest_error = Some("Сборка не выбрана".to_owned());
            return;
        };
        let client = self.downloader.client().clone();
        let meta = self.paths.meta_dir();
        let (tx, rx) = mpsc::unbounded_channel();
        self.manifest_rx = Some(rx);
        self.manifest_error = None;
        self.status = format!("Проверка «{}»…", source.name);
        self.runtime.spawn(async move {
            let result = load_pack(&client, &source, &meta)
                .await
                .map(|pack| pack.manifest)
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
    }

    fn start_launch(&mut self) {
        if let Err(error) = pml_config::validate_nick(&self.nick) {
            self.error = Some(error.to_string());
            return;
        }
        if self.xms < 512 {
            self.error = Some("Xms должен быть не меньше 512 МБ".to_owned());
            return;
        }
        if self.xmx < self.xms {
            self.error = Some("Xmx не может быть меньше Xms".to_owned());
            return;
        }
        let Some(source) = self.selected_source() else {
            self.error = Some("Сборка не выбрана".to_owned());
            return;
        };
        self.persist();

        let config = self.store.config().clone();
        let paths = self.paths.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_task = Arc::clone(&cancel);
        // One cancellable clone flows through the whole pipeline; individual
        // domain crates keep their plain `Downloader` parameters.
        let downloader = self.downloader.with_cancel(Arc::clone(&cancel));
        let (tx, rx) = mpsc::unbounded_channel();
        let task_tx = tx.clone();

        self.runtime.spawn(async move {
            match pipeline::run_pipeline(
                paths,
                downloader,
                config,
                source,
                task_tx.clone(),
                cancel_for_task,
            )
            .await
            {
                Ok(()) => {
                    let _ = task_tx.send(UiEvent::Finished);
                }
                Err(error) if error == pipeline::CANCELLED => {
                    let _ = task_tx.send(UiEvent::Cancelled);
                }
                Err(error) => {
                    let _ = task_tx.send(UiEvent::Failed(error));
                }
            }
        });

        self.events_rx = Some(rx);
        self.cancel = Some(cancel);
        self.busy = true;
        self.game_running = false;
        self.progress = 0.0;
        self.status = "Подготовка…".to_owned();
        self.log_lines.clear();
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        let mut close_after_start = false;
        if let Some(mut receiver) = self.events_rx.take() {
            let mut disconnected = false;
            loop {
                match receiver.try_recv() {
                    Ok(UiEvent::Stage(stage)) => {
                        self.status = stage;
                        self.progress = 0.0;
                    }
                    Ok(UiEvent::Download(event)) => self.apply_download_event(event),
                    Ok(UiEvent::Manifest(manifest)) => {
                        self.manifest = Some(*manifest);
                        self.manifest_error = None;
                    }
                    Ok(UiEvent::Java(java)) => self.java = Some(java),
                    Ok(UiEvent::Sync(text)) => self.status = text,
                    Ok(UiEvent::GameStarted { pid, log_path }) => {
                        self.busy = false;
                        self.game_running = true;
                        self.progress = 1.0;
                        self.status = match pid {
                            Some(pid) => format!("Игра запущена (PID {pid})"),
                            None => "Игра запущена".to_owned(),
                        };
                        self.log_lines = tail_log(&log_path, 200);
                        self.last_log_poll = Instant::now();
                        self.log_path = Some(log_path);
                        close_after_start = self.close_on_start;
                    }
                    Ok(UiEvent::GameExited(code)) => {
                        self.game_running = false;
                        self.status = match code {
                            Some(0) => "Игра завершилась".to_owned(),
                            Some(code) => format!("Игра завершилась с кодом {code}"),
                            None => "Игра завершилась".to_owned(),
                        };
                        if let Some(path) = &self.log_path {
                            self.log_lines = tail_log(path, 200);
                        }
                    }
                    Ok(UiEvent::Finished) => {
                        self.busy = false;
                        self.progress = 1.0;
                        if !self.game_running {
                            self.status = "Готово".to_owned();
                        }
                    }
                    Ok(UiEvent::Failed(error)) => {
                        self.busy = false;
                        self.game_running = false;
                        self.status = "Ошибка".to_owned();
                        self.error = Some(error);
                    }
                    Ok(UiEvent::Cancelled) => {
                        self.busy = false;
                        self.progress = 0.0;
                        self.status = "Отменено".to_owned();
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if !disconnected {
                self.events_rx = Some(receiver);
            }
        }

        if let Some(mut receiver) = self.manifest_rx.take() {
            match receiver.try_recv() {
                Ok(Ok(manifest)) => {
                    self.manifest = Some(manifest);
                    self.manifest_error = None;
                    if !self.busy {
                        self.status = "Готов к запуску".to_owned();
                    }
                }
                Ok(Err(error)) => {
                    self.manifest_error = Some(error);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    self.manifest_rx = Some(receiver);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {}
            }
        }

        if close_after_start {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if self.game_running && self.last_log_poll.elapsed() >= Duration::from_millis(500) {
            self.last_log_poll = Instant::now();
            if let Some(path) = &self.log_path {
                self.log_lines = tail_log(path, 200);
            }
        }
    }

    fn apply_download_event(&mut self, event: DownloadEvent) {
        match event {
            DownloadEvent::Started { label, .. } => {
                self.status = format!("Скачивание: {label}");
            }
            DownloadEvent::Progress {
                downloaded,
                total,
                overall_done,
                overall_total,
                label,
                ..
            } => {
                if overall_total > 0 {
                    self.progress = (overall_done as f32 / overall_total as f32).clamp(0.0, 1.0);
                    self.status = format!(
                        "{} / {} · {}",
                        format_bytes(overall_done),
                        format_bytes(overall_total),
                        label
                    );
                } else if let Some(total) = total {
                    if total > 0 {
                        self.progress = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
                    }
                    self.status = format!(
                        "{} / {} · {label}",
                        format_bytes(downloaded),
                        format_bytes(total)
                    );
                }
            }
            DownloadEvent::FileDone { label, .. } => {
                self.status = format!("Готово: {label}");
            }
            DownloadEvent::FileFailed { label, error, .. } => {
                self.status = format!("Ошибка {label}: {error}");
            }
        }
    }
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.sync_glass(frame);
        self.backdrop.poll(ctx);
        if self.theme == Theme::Win7 {
            if let Some(hwnd) = self.hwnd {
                self.backdrop.paint(ctx, hwnd);
            }
        }
        pml_theme::sync_scale(ctx, self.theme);
        if !self.auto_refreshed {
            self.auto_refreshed = true;
            self.refresh_manifest();
        }
        self.poll_events(ctx);
        self.poll_publisher(ctx);

        egui::TopBottomPanel::top("pml_title")
            .exact_height(pml_theme::title_bar_height(self.theme))
            .frame(egui::Frame::none().fill(pml_theme::face(self.theme)))
            .show(ctx, |ui| {
                pml_theme::title_bar_with_icon(
                    ui,
                    self.theme,
                    pml_config::BRAND_NAME,
                    self.dog_texture.as_ref(),
                );
            });

        egui::TopBottomPanel::bottom("pml_bottom")
            .exact_height(84.0)
            .frame(
                egui::Frame::none()
                    .fill(pml_theme::face(self.theme))
                    .inner_margin(egui::Margin::same(8.0)),
            )
            .show(ctx, |ui| self.draw_bottom(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(pml_theme::panel_color(self.theme))
                    .inner_margin(egui::Margin::same(8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::Vec2::new(270.0, ui.available_height()),
                        egui::Layout::top_down(egui::Align::LEFT),
                        |ui| {
                            self.draw_sources(ui);
                            self.draw_manifest_info(ui);
                        },
                    );
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        self.draw_player(ui);
                        self.draw_log(ui);
                    });
                });
            });

        self.draw_error_window(ctx);
        self.draw_publisher(ctx);
        self.draw_settings(ctx);
        self.draw_welcome(ctx);

        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.game_running {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// A transparent framebuffer lets the DWM acrylic backdrop show through the
    /// translucent Windows 7 surfaces. Every other theme keeps an opaque clear
    /// colour so no desktop pixels leak around its solid panels.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.theme == Theme::Win7 {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            visuals.panel_fill.to_normalized_gamma_f32()
        }
    }
}
