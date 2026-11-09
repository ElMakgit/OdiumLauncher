//! Launcher window state and lifecycle.
//!
//! The UI thread never performs I/O. `pipeline` owns the Tokio task
//! (manifest -> Java -> loader -> modpack -> Minecraft -> JVM) and reports
//! through an unbounded channel; `update` drains that channel once per frame.
//! Downloads additionally stream `DownloadEvent`s into the same channel.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use pml_config::{ConfigStore, PortablePaths, Source, Theme};
use pml_downloader::{CancelToken, DownloadEvent, Downloader};
use pml_java::JavaInstallation;
use pml_launcher::tail_log_async;
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

/// Result of the lightweight reachability probe shown next to the server
/// address. It is a TCP connect, not a full Minecraft status handshake, so it
/// works without a protocol implementation and never loads a player list.
#[derive(Debug, Clone, Copy)]
struct ServerStatus {
    reachable: bool,
    latency_ms: u128,
}

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
    let text_scale = store.config().text_scale();
    let dog_icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/dog_face.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1010.0, 680.0])
        .with_min_inner_size([940.0, 620.0])
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
            pml_theme::set_text_scale(&creation.egui_ctx, theme, text_scale);
            pml_theme::apply(&creation.egui_ctx, theme);
            Ok(Box::new(LauncherApp::new(
                paths, store, runtime, downloader, backdrop,
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
    cancel: Option<CancelToken>,
    manifest_rx: Option<mpsc::UnboundedReceiver<Result<Manifest, String>>>,
    reset_rx: Option<mpsc::UnboundedReceiver<Result<pml_modpack::ResetReport, String>>>,
    confirm_reset: bool,
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
    log_rx: Option<mpsc::UnboundedReceiver<Vec<String>>>,
    last_log_poll: Instant,
    server_status: Option<ServerStatus>,
    server_rx: Option<mpsc::UnboundedReceiver<ServerStatus>>,
    last_server_ping: Instant,
    error: Option<String>,
    auto_refreshed: bool,
    publisher: PublisherState,
    welcome: WelcomeState,
    theme: Theme,
    /// Text scale in percent, applied globally through [`pml_theme::font`].
    font_scale: u8,
    glass_enabled: Option<bool>,
    hwnd: Option<isize>,
    backdrop: glass::Backdrop,
    settings_open: bool,
    /// Shown while the user reviews what a full uninstall will delete.
    uninstall_confirm: bool,
    /// The irreversible-action checkbox in that dialog.
    uninstall_agree: bool,
    uninstall_rx:
        Option<mpsc::UnboundedReceiver<Result<pml_config::uninstall::PurgeReport, String>>>,
    uninstalling: bool,
    /// True once the data root is gone: nothing may write to it again, so the
    /// usual "save on close" is skipped and the window exits quietly.
    uninstalled: bool,
}

impl LauncherApp {
    fn new(
        paths: PortablePaths,
        store: ConfigStore,
        runtime: tokio::runtime::Handle,
        downloader: Downloader,
        backdrop: glass::Backdrop,
    ) -> Self {
        let config = store.config();
        let welcome_open = !config.welcome_seen;
        let theme = config.theme;
        let font_scale = config.font_scale;
        // Leave at least 1 GiB for Windows and background apps, and never offer
        // more than the physical RAM installed in this machine.
        let total_ram_mb = total_system_ram_mb();
        let max_xmx = total_ram_mb.saturating_sub(1024).clamp(1024, 65_536);
        // New installs use exactly half of the physical RAM. Moving the Xmx
        // slider disables auto mode so an explicit choice survives restarts.
        let jvm = config.jvm_for(&config.active_source_id);
        let auto_xmx = jvm.auto_xmx;
        let (xms, xmx) = jvm.resolved(total_ram_mb);
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
            reset_rx: None,
            confirm_reset: false,
            manifest: None,
            manifest_error: None,
            java: None,
            status: "Готов к запуску".to_owned(),
            progress: 0.0,
            busy: false,
            game_running: false,
            log_path: None,
            log_lines: Vec::new(),
            log_rx: None,
            last_log_poll: Instant::now(),
            server_status: None,
            server_rx: None,
            last_server_ping: Instant::now() - Duration::from_secs(30),
            error: None,
            auto_refreshed: false,
            publisher: PublisherState::new(),
            welcome: WelcomeState {
                open: welcome_open,
                step: 0,
            },
            theme,
            font_scale,
            glass_enabled: None,
            hwnd: None,
            backdrop,
            settings_open: false,
            uninstall_confirm: false,
            uninstall_agree: false,
            uninstall_rx: None,
            uninstalling: false,
            uninstalled: false,
        }
    }

    fn selected_source(&self) -> Option<Source> {
        self.store.config().source(&self.source_id).cloned()
    }

    fn persist(&mut self) {
        {
            let config = self.store.config_mut();
            config.nick = self.nick.clone();
            let jvm = pml_config::JvmConfig {
                xms: self.xms,
                xmx: self.xmx,
                auto_xmx: self.auto_xmx,
            };
            // The global block is the fallback for sources that never had an
            // explicit choice; the per-source entry keeps this pack's value.
            config.jvm = jvm.clone();
            config.set_jvm_for(&self.source_id, jvm);
            config.close_launcher_on_start = self.close_on_start;
            config.active_source_id = self.source_id.clone();
            config.theme = self.theme;
            config.font_scale = self.font_scale;
        }
        if let Err(error) = self.store.save() {
            self.error = Some(format!("Не удалось сохранить config.json: {error}"));
        }
    }

    /// Loads the heap settings remembered for the active source, falling back
    /// to the global defaults and clamping them to this machine.
    fn apply_jvm_for_active_source(&mut self) {
        let jvm = self.store.config().jvm_for(&self.source_id);
        self.auto_xmx = jvm.auto_xmx;
        let (xms, xmx) = jvm.resolved(self.total_ram_mb);
        self.xmx = xmx;
        self.xms = xms;
    }

    fn select_source(&mut self, id: &str) {
        if self.source_id == id {
            return;
        }
        // Keep the current pack's heap choice before switching to another one.
        self.persist();
        self.source_id = id.to_owned();
        self.apply_jvm_for_active_source();
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

    /// Applies a new text scale immediately. The in-memory config is updated
    /// here; the caller saves it once the slider is released.
    fn set_font_scale(&mut self, ctx: &egui::Context, percent: u8) {
        let percent = percent.clamp(80, 160);
        if self.font_scale == percent {
            return;
        }
        self.font_scale = percent;
        self.store.config_mut().font_scale = percent;
        pml_theme::set_text_scale(ctx, self.theme, f32::from(percent) / 100.0);
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
        // A click while a check is already in flight used to spawn a second
        // task and drop the first receiver; both requests still ran. Ignore
        // repeated clicks instead.
        if self.manifest_rx.is_some() {
            return;
        }
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

    /// Removes the pack-managed files of the active source and its cached
    /// manifest/.mrpack, so the next launch downloads the build again. Worlds
    /// and screenshots are preserved by `pml_modpack::reset_instance`.
    fn delete_build_files(&mut self) {
        if self.busy || self.game_running {
            self.error = Some("Нельзя удалять файлы сборки во время запуска или игры".to_owned());
            return;
        }
        if self.reset_rx.is_some() {
            return;
        }
        let Some(source) = self.selected_source() else {
            self.error = Some("Сборка не выбрана".to_owned());
            return;
        };
        let instance_dir = match self.paths.instance_dir(&source.id) {
            Ok(path) => path,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        let meta = self.paths.meta_dir();
        let url = source.manifest_url.clone();
        let manifest_files = self
            .manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .files
                    .iter()
                    .chain(manifest.optional_files.iter())
                    .map(|file| file.path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let (tx, rx) = mpsc::unbounded_channel();
        self.reset_rx = Some(rx);
        self.status = "Удаление файлов сборки…".to_owned();
        self.runtime.spawn(async move {
            let result = match pml_modpack::reset_instance(&instance_dir, &manifest_files).await {
                Ok(report) => {
                    // A locked cache file must not turn a successful reset into
                    // an error: the next refresh revalidates it anyway.
                    if let Err(error) = pml_modpack::clear_source_cache(&meta, &url) {
                        tracing::warn!(%error, "could not clear the source cache");
                    }
                    Ok(report)
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = tx.send(result);
        });
    }

    /// Deletes the data root and, once that succeeded, the executable itself.
    /// The window closes on success: the launcher has nothing left to show.
    fn start_uninstall(&mut self) {
        if self.uninstalling || self.uninstall_rx.is_some() {
            return;
        }
        if self.busy {
            self.error = Some("Дождитесь окончания текущей операции".to_owned());
            return;
        }
        if self.game_running {
            self.error = Some(
                "Сначала закройте игру: Windows не даст удалить файлы запущенной сборки".to_owned(),
            );
            return;
        }
        let root = self.paths.root().to_path_buf();
        let exe = std::env::current_exe().ok();
        let (tx, rx) = mpsc::unbounded_channel();
        self.uninstall_rx = Some(rx);
        self.uninstalling = true;
        self.status = "Удаление данных лаунчера…".to_owned();
        self.runtime.spawn_blocking(move || {
            let result = match pml_config::uninstall::purge(&root) {
                Ok(report) if report.leftovers.is_empty() => {
                    // The image is locked while the process is alive, so a
                    // detached shell removes it a moment after we exit.
                    if let Some(exe) = exe {
                        if let Err(error) = pml_config::uninstall::schedule_exe_removal(&exe) {
                            tracing::warn!(%error, "could not schedule executable removal");
                        }
                    }
                    Ok(report)
                }
                Ok(report) => Ok(report),
                Err(error) => Err(error.to_string()),
            };
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
        let cancel = CancelToken::new();
        let cancel_for_task = cancel.clone();
        // One cancellable clone flows through the whole pipeline; individual
        // domain crates keep their plain `Downloader` parameters.
        let downloader = self.downloader.with_cancel(cancel.clone());
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

    /// Queues a background tail of the game log. Only one request is in flight
    /// at a time; a slow disk therefore cannot pile up tasks or block a frame.
    fn request_log_tail(&mut self, path: PathBuf) {
        if self.log_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        self.log_rx = Some(rx);
        self.runtime.spawn(async move {
            let lines = tail_log_async(path, 200).await;
            let _ = tx.send(lines);
        });
    }

    /// Starts a background reachability probe of the active server. A TCP
    /// connect is enough to tell the player "online" from "unreachable"; the
    /// full status protocol is deliberately not implemented here.
    fn request_server_ping(&mut self) {
        if self.server_rx.is_some() {
            return;
        }
        let server = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.server.clone())
            .unwrap_or_else(|| self.store.config().server.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        self.server_rx = Some(rx);
        self.last_server_ping = Instant::now();
        self.runtime.spawn(async move {
            let started = Instant::now();
            let address = format!("{}:{}", server.ip, server.port);
            let status = match tokio::time::timeout(
                Duration::from_secs(3),
                tokio::net::TcpStream::connect(&address),
            )
            .await
            {
                Ok(Ok(_stream)) => ServerStatus {
                    reachable: true,
                    latency_ms: started.elapsed().as_millis(),
                },
                _ => ServerStatus {
                    reachable: false,
                    latency_ms: 0,
                },
            };
            let _ = tx.send(status);
        });
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
                        self.log_lines.clear();
                        self.request_log_tail(log_path.clone());
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
                        if let Some(path) = self.log_path.clone() {
                            self.request_log_tail(path);
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

        if let Some(mut receiver) = self.reset_rx.take() {
            match receiver.try_recv() {
                Ok(Ok(report)) => {
                    self.manifest = None;
                    self.manifest_error = None;
                    self.status = if report.failed == 0 {
                        format!(
                            "Удалено файлов сборки: {}. Скачиваю список…",
                            report.removed
                        )
                    } else {
                        format!(
                            "Удалено файлов: {}, не удалось удалить: {}",
                            report.removed, report.failed
                        )
                    };
                    self.refresh_manifest();
                }
                Ok(Err(error)) => {
                    self.status = "Ошибка удаления файлов сборки".to_owned();
                    self.error = Some(error);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    self.reset_rx = Some(receiver);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {}
            }
        }

        if let Some(mut receiver) = self.uninstall_rx.take() {
            match receiver.try_recv() {
                Ok(Ok(report)) if report.leftovers.is_empty() => {
                    self.uninstalling = false;
                    self.uninstalled = true;
                    self.status = if report.root_existed {
                        format!(
                            "Удалено файлов: {}. Лаунчер закрывается…",
                            report.removed_files
                        )
                    } else {
                        "Данные уже были удалены. Лаунчер закрывается…".to_owned()
                    };
                    // Everything is gone, including the config the window is
                    // reading, so there is nothing useful left to draw.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Ok(Ok(report)) => {
                    self.uninstalling = false;
                    // The error dialog belongs to the main window; close the
                    // settings so the user actually sees it.
                    self.settings_open = false;
                    self.status = "Удаление завершено не полностью".to_owned();
                    self.error = Some(format!(
                        "Не удалось удалить файлов: {}. Скорее всего, игра ещё запущена — закройте её и повторите.\n\nОсталось: {}",
                        report.leftovers.len(),
                        report
                            .leftovers
                            .iter()
                            .take(5)
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                Ok(Err(error)) => {
                    self.uninstalling = false;
                    self.settings_open = false;
                    self.status = "Ошибка удаления".to_owned();
                    self.error = Some(error);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    self.uninstall_rx = Some(receiver);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.uninstalling = false;
                    self.settings_open = false;
                    self.status = "Ошибка удаления".to_owned();
                    self.error = Some("Удаление прервалось неожиданно".to_owned());
                }
            }
        }

        if let Some(mut receiver) = self.log_rx.take() {
            match receiver.try_recv() {
                Ok(lines) => self.log_lines = lines,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    self.log_rx = Some(receiver);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {}
            }
        }

        if let Some(mut receiver) = self.server_rx.take() {
            match receiver.try_recv() {
                Ok(status) => self.server_status = Some(status),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    self.server_rx = Some(receiver);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {}
            }
        }
        if self.server_rx.is_none() && self.last_server_ping.elapsed() >= Duration::from_secs(30) {
            self.request_server_ping();
        }

        if close_after_start {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if self.game_running && self.last_log_poll.elapsed() >= Duration::from_millis(500) {
            self.last_log_poll = Instant::now();
            if let Some(path) = self.log_path.clone() {
                self.request_log_tail(path);
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
        // The nick and sliders are edited in-place; save them even when the
        // player closes the window without pressing "Играть". After an
        // uninstall there is nothing left to save, and writing config.json
        // would resurrect the folder the user just deleted.
        if !self.uninstalled && ctx.input(|input| input.viewport().close_requested()) {
            self.persist();
        }
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
                // Top row: the pack on the left, the player settings on the
                // right; the log takes the full width underneath. The pack
                // column scales a little with the window so the pack facts never
                // wrap, while the settings keep the sliders readable.
                let gutter = 10.0;
                let left_width = (ui.available_width() * 0.36).clamp(300.0, 420.0);
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::Vec2::new(left_width, ui.available_height()),
                        egui::Layout::top_down(egui::Align::LEFT),
                        |ui| {
                            self.draw_sources(ui);
                            self.draw_manifest_info(ui);
                        },
                    );
                    ui.add_space(gutter);
                    ui.allocate_ui_with_layout(
                        egui::Vec2::new(ui.available_width(), ui.available_height()),
                        egui::Layout::top_down(egui::Align::LEFT),
                        |ui| self.draw_player(ui),
                    );
                });
                self.draw_log(ui);
            });

        self.draw_error_window(ctx);
        self.draw_reset_confirm(ctx);
        self.draw_publisher(ctx);
        self.draw_settings(ctx);
        self.draw_welcome(ctx);

        if self.busy || self.uninstalling {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.game_running {
            ctx.request_repaint_after(Duration::from_millis(500));
        } else {
            // Idle ticks keep the server probe and its "checked N s ago" label
            // live without any user input.
            ctx.request_repaint_after(Duration::from_millis(1000));
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
