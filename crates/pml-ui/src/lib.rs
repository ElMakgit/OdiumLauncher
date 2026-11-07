//! Windows 98 style launcher window and the end-to-end orchestration task.
//!
//! The UI thread never performs I/O. A `tokio` task runs the pipeline
//! (manifest -> Java -> loader -> modpack -> Minecraft metadata -> JVM) and
//! reports through an unbounded channel; `update` drains that channel once per
//! frame. Downloads additionally stream `DownloadEvent`s into the same channel.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align, Align2, Layout, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use pml_config::{
    validate_nick, Config, ConfigStore, PortablePaths, ServerSpec, Source, SourceKind, Theme,
};
use pml_downloader::{DownloadEvent, Downloader};
use pml_java::{ensure_java, JavaInstallation};
use pml_launcher::{spawn_game, tail_log, LaunchSpec};
use pml_loader::LoaderInstaller;
use pml_minecraft::{LaunchRequest, Minecraft, UserProfile};
use pml_modpack::{extract_overrides, load_pack, Manifest, ModpackSync, SyncRequest};
use pml_publish::{
    build_dist, DistRequest, ManifestPublish, MrpackPublish, Progress as PublishProgress, Publisher,
};
use tokio::sync::mpsc;

mod assistant;
mod glass;

use assistant::draw_assistant;

#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error(transparent)]
    Config(#[from] pml_config::ConfigError),
    #[error(transparent)]
    Download(#[from] pml_downloader::DownloadError),
    #[error("eframe: {0}")]
    Eframe(String),
}

enum UiEvent {
    Stage(String),
    Download(DownloadEvent),
    Manifest(Box<Manifest>),
    Java(JavaInstallation),
    Sync(String),
    GameStarted { pid: Option<u32>, log_path: PathBuf },
    GameExited(Option<i32>),
    Finished,
    Failed(String),
    Cancelled,
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
        .with_title("Odium Launcher");
    if let Some(icon) = &dog_icon {
        viewport = viewport.with_icon(icon.clone());
    }

    // The snapshot has to be taken before eframe creates the window, otherwise
    // the launcher would photograph itself.
    let backdrop = glass::Backdrop::capture_now();
    let options = eframe::NativeOptions {
        viewport,
        persist_window: false,
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "Odium Launcher",
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

#[derive(Debug, Clone)]
struct PackInfo {
    name: String,
    version: String,
    mc_version: String,
    loader: String,
}

enum PublisherEvent {
    Log(String),
    Progress(f32),
    Done(Result<String, String>),
}

struct WelcomeState {
    open: bool,
    step: usize,
}

#[derive(Clone, Copy)]
enum WelcomeAction {
    Back,
    Next,
    Finish,
    Skip,
}

const WELCOME_TITLES: [&str; 3] = [
    "Привет! Я Байт — помощник Odium Launcher.",
    "Ваш ник и память игры.",
    "Один клик — и вы в игре.",
];

const WELCOME_TEXTS: [&str; 3] = [
    "Я проведу вас по первым шагам. Лаунчер сам скачает Java, загрузчик, моды и конфиги — вручную ничего устанавливать не нужно.",
    "Ник должен быть от 3 до 16 символов. Ползунок Xmx задаёт, сколько оперативной памяти получит Minecraft: по умолчанию — ровно половина ОЗУ компьютера.",
    "Нажмите «Играть». Первый запуск может занять время — скачиваются файлы. Сервер появится в «Сетевой игре» автоматически, а обновления будут при следующих запусках.",
];

fn theme_description(theme: Theme) -> &'static str {
    match theme {
        Theme::Win98 => "Классический серый интерфейс с объёмными кнопками",
        Theme::Win7 => "Стеклянные заголовки и голубые кнопки Aero",
        Theme::Win81 => "Плоские элементы Metro и синий акцент",
        Theme::Win10 => "Строгий интерфейс с синей полосой заголовка",
        Theme::Win11 => "Скруглённые углы и мягкие поверхности Mica",
    }
}

struct PublisherState {
    open: bool,
    path: String,
    path_is_dir: bool,
    info: Option<PackInfo>,
    repo: String,
    token: String,
    tag: String,
    branch: String,
    release_name: String,
    server_ip: String,
    server_port: String,
    output_dir: String,
    create_repo: bool,
    build_dist: bool,
    busy: bool,
    progress: f32,
    log: Vec<String>,
    rx: Option<mpsc::UnboundedReceiver<PublisherEvent>>,
    result: Option<Result<String, String>>,
}

impl PublisherState {
    fn new() -> Self {
        Self {
            open: false,
            path: String::new(),
            path_is_dir: false,
            info: None,
            repo: String::new(),
            token: String::new(),
            tag: String::new(),
            branch: "main".to_owned(),
            release_name: String::new(),
            server_ip: "127.0.0.1".to_owned(),
            server_port: "25565".to_owned(),
            output_dir: String::new(),
            create_repo: true,
            build_dist: true,
            busy: false,
            progress: 0.0,
            log: Vec::new(),
            rx: None,
            result: None,
        }
    }
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
        self.theme = theme;
        pml_theme::apply(ctx, theme);
        self.store.config_mut().theme = theme;
        if let Err(error) = self.store.save() {
            self.error = Some(format!("Не удалось сохранить config.json: {error}"));
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

    fn poll_publisher(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(receiver) = self.publisher.rx.as_mut() {
            loop {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.publisher.rx = None;
        }
        for event in events {
            match event {
                PublisherEvent::Log(line) => self.publisher.log.push(line),
                PublisherEvent::Progress(value) => self.publisher.progress = value,
                PublisherEvent::Done(result) => {
                    self.publisher.busy = false;
                    self.publisher.progress = 1.0;
                    if let Ok(message) = &result {
                        self.publisher.log.push(message.clone());
                    }
                    self.publisher.result = Some(result);
                }
            }
        }
        if self.publisher.busy {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn set_publisher_path(&mut self, path: PathBuf) {
        let info = read_pack_info(&path);
        if let Some(info) = &info {
            if self.publisher.release_name.trim().is_empty() {
                self.publisher.release_name =
                    format!("{} {}", info.name, info.version).trim().to_owned();
            }
            if self.publisher.tag.trim().is_empty() && !info.version.is_empty() {
                self.publisher.tag = format!("v{}", info.version);
            }
        }
        if self.publisher.output_dir.trim().is_empty() {
            self.publisher.output_dir = default_dist_dir(&path).display().to_string();
        }
        self.publisher.path = path.display().to_string();
        self.publisher.path_is_dir = path.is_dir();
        self.publisher.info = info;
        self.publisher.log.clear();
        self.publisher.result = None;
    }

    fn dist_output_dir(&self) -> Option<PathBuf> {
        let raw = self.publisher.output_dir.trim();
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
        let path = PathBuf::from(self.publisher.path.trim());
        if path.as_os_str().is_empty() {
            None
        } else {
            Some(default_dist_dir(&path))
        }
    }

    fn start_publish(&mut self) {
        if self.publisher.busy {
            return;
        }
        let path = PathBuf::from(self.publisher.path.trim());
        let repo_input = self.publisher.repo.trim().to_owned();
        let token_input = self.publisher.token.trim().to_owned();
        let tag = self.publisher.tag.trim().to_owned();
        let branch = self.publisher.branch.trim().to_owned();
        let release_name = self.publisher.release_name.trim().to_owned();
        let server_ip = self.publisher.server_ip.trim().to_owned();
        let server_port = self
            .publisher
            .server_port
            .trim()
            .parse::<u16>()
            .unwrap_or(25565);
        let create_repo = self.publisher.create_repo;
        let make_dist = self.publisher.build_dist;
        let output_dir = self.publisher.output_dir.trim().to_owned();

        if path.as_os_str().is_empty() || !path.exists() {
            self.publisher
                .log
                .push("Укажите существующий .mrpack или папку сборки.".to_owned());
            return;
        }
        if repo_input.is_empty() {
            self.publisher
                .log
                .push("Укажите GitHub-репозиторий (owner/name).".to_owned());
            return;
        }
        let token = if token_input.is_empty() {
            std::env::var("GITHUB_TOKEN")
                .or_else(|_| std::env::var("GH_TOKEN"))
                .unwrap_or_default()
        } else {
            token_input
        };
        if token.is_empty() {
            self.publisher
                .log
                .push("Нужен GitHub-токен с правами repo / Contents: write.".to_owned());
            return;
        }

        let is_dir = path.is_dir();
        let info = read_pack_info(&path);
        let exe = std::env::current_exe().ok();
        let runtime = self.runtime.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        self.publisher.rx = Some(rx);
        self.publisher.busy = true;
        self.publisher.progress = 0.0;
        self.publisher.log.clear();
        self.publisher.result = None;

        runtime.spawn(async move {
            let report_tx = tx.clone();
            let report = move |progress: PublishProgress| match progress {
                PublishProgress::Stage(text) => {
                    let _ = report_tx.send(PublisherEvent::Log(text));
                }
                PublishProgress::Upload { sent, total } => {
                    let value = if total > 0 {
                        sent as f32 / total as f32
                    } else {
                        0.0
                    };
                    let _ = report_tx.send(PublisherEvent::Progress(value));
                }
            };

            let publisher = match Publisher::new(token) {
                Ok(publisher) => publisher,
                Err(error) => {
                    let _ = tx.send(PublisherEvent::Done(Err(error.to_string())));
                    return;
                }
            };

            let result = if is_dir {
                publisher
                    .publish_manifest(
                        &ManifestPublish {
                            repo: repo_input.clone(),
                            branch,
                            pack_dir: path.clone(),
                            commit_message: "Publish Odium Launcher pack".to_owned(),
                            create_repo,
                            private: false,
                        },
                        report,
                    )
                    .await
            } else {
                let asset_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "pack.mrpack".to_owned());
                let tag_value = if tag.is_empty() {
                    info.as_ref()
                        .map(|info| format!("v{}", info.version))
                        .unwrap_or_else(|| "v1.0.0".to_owned())
                } else {
                    tag.clone()
                };
                let release = if release_name.is_empty() {
                    info.as_ref()
                        .map(|info| format!("{} {}", info.name, info.version))
                        .unwrap_or_else(|| "Odium pack".to_owned())
                } else {
                    release_name.clone()
                };
                publisher
                    .publish_mrpack(
                        &MrpackPublish {
                            repo: repo_input.clone(),
                            tag: tag_value,
                            release_name: release,
                            release_body: "Сборка для Odium Launcher".to_owned(),
                            archive: path.clone(),
                            asset_name,
                            create_repo,
                            private: false,
                        },
                        report,
                    )
                    .await
            };

            match result {
                Ok(published) => {
                    let mut message = format!("Опубликовано: {}", published.page_url);
                    if make_dist {
                        let info = info.unwrap_or_else(|| PackInfo {
                            name: "Odium pack".to_owned(),
                            version: String::new(),
                            mc_version: String::new(),
                            loader: String::new(),
                        });
                        let repo_name = repo_input.rsplit('/').next().unwrap_or("pack").to_owned();
                        let source_id = pml_publish::sanitize_source_id(&repo_name);
                        let mut source_name = info.name.clone();
                        if !info.version.is_empty() && !source_name.ends_with(&info.version) {
                            source_name.push(' ');
                            source_name.push_str(&info.version);
                        }
                        let pack_label = if info.mc_version.is_empty() {
                            "Minecraft".to_owned()
                        } else {
                            format!("Minecraft {} · {}", info.mc_version, info.loader)
                        };
                        let dist_request = DistRequest {
                            output_dir: if output_dir.is_empty() {
                                default_dist_dir(&path)
                            } else {
                                PathBuf::from(&output_dir)
                            },
                            source_id,
                            source_name,
                            pack_url: published.pack_url,
                            kind: if is_dir {
                                SourceKind::Manifest
                            } else {
                                SourceKind::Mrpack
                            },
                            server: ServerSpec {
                                ip: if server_ip.is_empty() {
                                    "127.0.0.1".to_owned()
                                } else {
                                    server_ip
                                },
                                port: server_port,
                            },
                            xms: 1024,
                            xmx: (total_system_ram_mb() / 2).max(1024),
                            pack_label,
                            exe,
                        };
                        match tokio::task::spawn_blocking(move || build_dist(&dist_request)).await {
                            Ok(Ok(dist)) => {
                                message
                                    .push_str(&format!("\nИгровой пакет: {}", dist.zip.display()));
                            }
                            Ok(Err(error)) => {
                                let _ =
                                    tx.send(PublisherEvent::Done(Err(format!("dist: {error}"))));
                                return;
                            }
                            Err(error) => {
                                let _ =
                                    tx.send(PublisherEvent::Done(Err(format!("dist: {error}"))));
                                return;
                            }
                        }
                    }
                    let _ = tx.send(PublisherEvent::Done(Ok(message)));
                }
                Err(error) => {
                    let _ = tx.send(PublisherEvent::Done(Err(error.to_string())));
                }
            }
        });
    }

    fn draw_publisher(&mut self, ctx: &egui::Context) {
        if !self.publisher.open {
            return;
        }
        let mut open = true;
        let mut pick_file = false;
        let mut pick_dir = false;
        let mut pick_output = false;
        let mut publish = false;
        let mut open_output = false;
        let mut copy_log = false;

        egui::Window::new("Публикация сборки — для издателя")
            .open(&mut open)
            .default_width(640.0)
            .default_height(560.0)
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Игрокам этот раздел не нужен. Токен хранится только в памяти и не пишется на диск.",
                    )
                    .small(),
                );
                ui.add_space(4.0);
                egui::Grid::new("publisher_grid")
                    .num_columns(3)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Сборка:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.publisher.path)
                                .desired_width(360.0)
                                .hint_text("путь к .mrpack или папке с manifest.json"),
                        );
                        ui.horizontal(|ui| {
                            if pml_theme::button(ui, "Файл…", !self.publisher.busy).clicked() {
                                pick_file = true;
                            }
                            if pml_theme::button(ui, "Папка…", !self.publisher.busy).clicked() {
                                pick_dir = true;
                            }
                        });
                        ui.end_row();

                        ui.label("Репозиторий:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.publisher.repo)
                                .desired_width(360.0)
                                .hint_text("owner/name"),
                        );
                        ui.label("");
                        ui.end_row();

                        ui.label("GitHub-токен:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.publisher.token)
                                .password(true)
                                .desired_width(360.0),
                        );
                        ui.label(RichText::new("repo / Contents: write").small());
                        ui.end_row();

                        if self.publisher.path_is_dir {
                            ui.label("Ветка:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.publisher.branch)
                                    .desired_width(160.0),
                            );
                        } else {
                            ui.label("Тег релиза:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.publisher.tag)
                                    .desired_width(160.0)
                                    .hint_text("v1.0.0"),
                            );
                        }
                        ui.label("");
                        ui.end_row();

                        ui.label("Название релиза:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.publisher.release_name)
                                .desired_width(360.0),
                        );
                        ui.label("");
                        ui.end_row();

                        ui.label("Сервер:");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.publisher.server_ip)
                                    .desired_width(140.0),
                            );
                            ui.label(":");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.publisher.server_port)
                                    .desired_width(60.0),
                            );
                        });
                        ui.label("");
                        ui.end_row();

                        ui.label("Папка dist:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.publisher.output_dir)
                                .desired_width(360.0)
                                .hint_text("пусто = рядом со сборкой"),
                        );
                        if pml_theme::button(ui, "Выбрать…", !self.publisher.busy).clicked() {
                            pick_output = true;
                        }
                        ui.end_row();
                    });

                ui.horizontal(|ui| {
                    ui.checkbox(
                        &mut self.publisher.create_repo,
                        "Создать репозиторий, если его нет",
                    );
                    ui.checkbox(&mut self.publisher.build_dist, "Собрать dist и ZIP");
                });
                if let Some(info) = &self.publisher.info {
                    ui.label(
                        RichText::new(format!(
                            "{} {} · Minecraft {} · {}",
                            info.name, info.version, info.mc_version, info.loader
                        ))
                        .small(),
                    );
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if pml_theme::primary_button_sized(
                        ui,
                        "Опубликовать",
                        Some(Vec2::new(140.0, 28.0)),
                        !self.publisher.busy,
                    )
                    .clicked()
                    {
                        publish = true;
                    }
                    if self.publisher.busy {
                        ui.label("Публикация…");
                    }
                    if matches!(self.publisher.result, Some(Ok(_)))
                        && pml_theme::button(ui, "Открыть папку", true).clicked()
                    {
                        open_output = true;
                    }
                    if pml_theme::button(ui, "Копировать лог", true).clicked() {
                        copy_log = true;
                    }
                });

                pml_theme::progress_bar(
                    ui,
                    self.publisher.progress,
                    if self.publisher.busy {
                        "Публикация…"
                    } else {
                        "Готово"
                    },
                );

                ui.add_space(4.0);
                sunken_well(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(170.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            let text = if self.publisher.log.is_empty() {
                                "Здесь появится журнал публикации.".to_owned()
                            } else {
                                self.publisher.log.join("\n")
                            };
                            ui.add(
                                egui::Label::new(RichText::new(text).monospace())
                                    .selectable(true)
                                    .wrap(),
                            );
                        });
                });

                if let Some(Err(error)) = &self.publisher.result {
                    ui.colored_label(pml_theme::error_color(), error);
                }
            });

        self.publisher.open = open;
        if pick_file {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Modrinth pack", &["mrpack"])
                .set_title("Выберите .mrpack")
                .pick_file()
            {
                self.set_publisher_path(path);
            }
        }
        if pick_dir {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Выберите папку с manifest.json и overrides")
                .pick_folder()
            {
                self.set_publisher_path(path);
            }
        }
        if pick_output {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Куда собрать dist")
                .pick_folder()
            {
                self.publisher.output_dir = path.display().to_string();
            }
        }
        if publish {
            self.start_publish();
        }
        if open_output {
            if let Some(dir) = self.dist_output_dir() {
                open_path(&dir);
            }
        }
        if copy_log {
            ctx.copy_text(self.publisher.log.join("\n"));
        }
    }

    fn finish_welcome(&mut self) {
        self.welcome.open = false;
        self.welcome.step = 0;
        {
            let config = self.store.config_mut();
            config.welcome_seen = true;
            config.nick = self.nick.clone();
        }
        if let Err(error) = self.store.save() {
            self.error = Some(format!("Не удалось сохранить config.json: {error}"));
        }
    }

    fn draw_welcome(&mut self, ctx: &egui::Context) {
        if !self.welcome.open {
            return;
        }
        let mut action: Option<WelcomeAction> = None;
        egui::Window::new("welcome")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            // A fixed width keeps the custom frame, title bar and content the
            // same size; otherwise the title bar can stretch past the frame
            // and the main window's log panel shows through on the right.
            .min_width(640.0)
            .max_width(640.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                egui::Frame::none()
                    .fill(pml_theme::face())
                    .inner_margin(egui::Margin::same(4.0)),
            )
            .show(ctx, |ui| {
                pml_theme::title_bar(ui, "Помощник Байт — добро пожаловать!");
                ui.add_space(8.0);
                ui.horizontal_top(|ui| {
                    draw_assistant(ui, ctx.input(|input| input.time));
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.set_width(490.0);
                        ui.label(
                            RichText::new(WELCOME_TITLES[self.welcome.step])
                                .strong()
                                .size(14.0),
                        );
                        ui.add_space(6.0);
                        ui.label(RichText::new(WELCOME_TEXTS[self.welcome.step]).size(12.0));
                        if self.welcome.step == 1 {
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.label("Ник:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.nick).desired_width(180.0),
                                );
                            });
                        }
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(format!(
                                "Шаг {} из {}",
                                self.welcome.step + 1,
                                WELCOME_TITLES.len()
                            ))
                            .small(),
                        );
                    });
                });
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if pml_theme::button(ui, "Пропустить", true).clicked() {
                        action = Some(WelcomeAction::Skip);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let last = self.welcome.step + 1 == WELCOME_TITLES.len();
                        if pml_theme::primary_button_sized(
                            ui,
                            if last { "Готово" } else { "Далее" },
                            Some(Vec2::new(100.0, 26.0)),
                            true,
                        )
                        .clicked()
                        {
                            action = Some(if last {
                                WelcomeAction::Finish
                            } else {
                                WelcomeAction::Next
                            });
                        }
                        if self.welcome.step > 0
                            && pml_theme::button_sized(
                                ui,
                                "Назад",
                                Some(Vec2::new(100.0, 26.0)),
                                true,
                            )
                            .clicked()
                        {
                            action = Some(WelcomeAction::Back);
                        }
                    });
                });
            });

        match action {
            Some(WelcomeAction::Back) => {
                self.welcome.step = self.welcome.step.saturating_sub(1);
            }
            Some(WelcomeAction::Next) => {
                self.welcome.step = (self.welcome.step + 1).min(WELCOME_TITLES.len() - 1);
            }
            Some(WelcomeAction::Finish | WelcomeAction::Skip) => self.finish_welcome(),
            None => {}
        }
        // Keep the assistant's tail and blink animating even when the pointer
        // is idle. Only the welcome window pays this cost.
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn start_launch(&mut self) {
        if let Err(error) = validate_nick(&self.nick) {
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
        let downloader = self.downloader.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_task = Arc::clone(&cancel);
        let (tx, rx) = mpsc::unbounded_channel();
        let task_tx = tx.clone();

        self.runtime.spawn(async move {
            match run_pipeline(
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
                Err(error) if error == "Отменено" => {
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

    fn draw_sources(&mut self, ui: &mut egui::Ui) {
        let sources = self.store.config().sources.clone();
        // A distributed pack ships exactly one source; showing a one-item list
        // only confuses the player, so the manifest panel takes its place.
        if sources.len() <= 1 {
            return;
        }
        pml_theme::group_box(ui, "Сборка", |ui| {
            let mut clicked = None;
            for source in &sources {
                let selected = source.id == self.source_id;
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::click());
                let painter = ui.painter();
                if selected {
                    painter.rect_filled(
                        rect,
                        egui::Rounding::same(pml_theme::row_rounding()),
                        pml_theme::selection_color(),
                    );
                } else if response.hovered() {
                    painter.rect_filled(
                        rect,
                        egui::Rounding::same(pml_theme::row_rounding()),
                        pml_theme::row_hover_color(),
                    );
                }
                painter.text(
                    rect.left_center() + Vec2::new(5.0, 0.0),
                    Align2::LEFT_CENTER,
                    &source.name,
                    pml_theme::font(12.0),
                    if selected {
                        pml_theme::selection_text_color()
                    } else {
                        pml_theme::text_color()
                    },
                );
                if response.clicked() {
                    clicked = Some(source.id.clone());
                }
            }
            if let Some(id) = clicked {
                self.select_source(&id);
            }
        });
    }

    fn draw_manifest_info(&mut self, ui: &mut egui::Ui) {
        pml_theme::group_box(ui, "Сборка", |ui| {
            if let Some(manifest) = &self.manifest {
                ui.label(format!("Название: {}", manifest.name));
                if !manifest.version.is_empty() {
                    ui.label(format!("Версия: {}", manifest.version));
                }
                ui.label(format!("Minecraft: {}", manifest.mc_version));
                ui.label(format!(
                    "Загрузчик: {} {}",
                    manifest.loader.kind.label(),
                    manifest.loader.version
                ));
                let optional = manifest.optional_files.len();
                if optional == 0 {
                    ui.label(format!("Файлы: {}", manifest.files.len()));
                } else {
                    ui.label(format!(
                        "Файлы: {} + {} доп.",
                        manifest.files.len(),
                        optional
                    ));
                }
            } else if let Some(error) = &self.manifest_error {
                ui.colored_label(pml_theme::error_color(), "Манифест недоступен");
                ui.label(RichText::new(error).small());
            } else {
                ui.label("Проверка сборки…");
            }
            ui.add_space(4.0);
            if pml_theme::button(ui, "Проверить обновления", !self.busy).clicked()
            {
                self.refresh_manifest();
            }
        });
    }

    fn draw_player(&mut self, ui: &mut egui::Ui) {
        pml_theme::group_box(ui, "Игрок и параметры", |ui| {
            ui.horizontal(|ui| {
                ui.label("Ник:");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.nick)
                        .desired_width(180.0)
                        .char_limit(16),
                );
                if response.changed() {
                    self.manifest_error = None;
                }
            });
            match validate_nick(&self.nick) {
                Ok(()) => {
                    ui.label(RichText::new("3–16 символов: A–Z, a–z, 0–9, _").small());
                }
                Err(error) => {
                    ui.colored_label(pml_theme::error_color(), error.to_string());
                }
            }

            ui.add_space(3.0);
            ui.label(
                RichText::new(format!(
                    "ОЗУ: {:.1} ГБ · предел Xmx: {} МБ",
                    self.total_ram_mb as f32 / 1024.0,
                    self.max_xmx
                ))
                .small(),
            );

            let half_ram = (self.total_ram_mb / 2).max(1024).min(self.max_xmx);
            let mut auto = self.auto_xmx;
            if pml_theme::checkbox(ui, &mut auto, "Авто: ровно половина ОЗУ", true).changed()
            {
                self.auto_xmx = auto;
                if auto {
                    self.xmx = half_ram;
                    self.xms = self.xms.min(self.xmx);
                }
            }
            if self.auto_xmx {
                self.xmx = half_ram;
                self.xms = self.xms.min(self.xmx);
            }

            ui.horizontal(|ui| {
                ui.label("Xms:");
                pml_theme::slider_u32(ui, &mut self.xms, 512..=self.max_xmx, 170.0);
                ui.label(format!("{} МБ", self.xms));
            });
            ui.horizontal(|ui| {
                ui.label("Xmx:");
                let response = pml_theme::slider_u32(ui, &mut self.xmx, 1024..=self.max_xmx, 170.0);
                if response.changed() {
                    // An explicit slider movement opts out of the automatic
                    // half-of-RAM default, otherwise the next frame would snap
                    // the trackbar back.
                    self.auto_xmx = false;
                }
                ui.label(format!("{} МБ", self.xmx));
            });
            if self.xms > self.xmx {
                self.xms = self.xmx;
            }

            ui.add_space(3.0);
            let mut close = self.close_on_start;
            pml_theme::checkbox(ui, &mut close, "Закрывать лаунчер после запуска", true);
            if close != self.close_on_start {
                self.close_on_start = close;
            }

            ui.add_space(2.0);
            if let Some(java) = &self.java {
                ui.label(format!(
                    "Java: {} ({}, {})",
                    java.version,
                    java.major,
                    java.origin.label()
                ));
            } else {
                ui.label("Java: будет найдена или скачана автоматически");
            }
        });
    }

    fn draw_log(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        pml_theme::group_box(ui, "Журнал", |ui| {
            let log_height = if pml_theme::current_theme() == Theme::Win98 {
                150.0
            } else {
                132.0
            };
            sunken_well(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(log_height)
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.log_lines.is_empty() {
                            ui.label(
                                RichText::new("Здесь появятся строки лога игры.")
                                    .italics()
                                    .color(pml_theme::muted_color()),
                            );
                        } else {
                            // A single selectable label keeps selection working
                            // across lines and enables Ctrl+C / the context menu.
                            let text = self.log_lines.join("\n");
                            ui.add(
                                egui::Label::new(RichText::new(text).monospace()).selectable(true),
                            );
                        }
                    });
            });
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                if pml_theme::button(ui, "Папка логов", true).clicked() {
                    open_path(&self.paths.logs_dir());
                }
                if pml_theme::button(ui, "Копировать", !self.log_lines.is_empty()).clicked()
                {
                    ui.ctx().copy_text(self.log_lines.join("\r\n"));
                }
            });
        });
    }

    fn draw_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut close_clicked = false;
        let mut chosen: Option<Theme> = None;

        egui::Window::new("Настройки")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(RichText::new("Тема оформления").strong());
                ui.add_space(6.0);

                pml_theme::group_box(ui, "Оформление", |ui| {
                    for theme in Theme::ALL {
                        let selected = self.theme == theme;
                        let (rect, response) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 44.0),
                            Sense::click(),
                        );
                        let current = pml_theme::palette_now();
                        let preview = pml_theme::palette(theme);
                        let painter = ui.painter();
                        let rounding = egui::Rounding::same(pml_theme::row_rounding());

                        if selected {
                            painter.rect_filled(rect, rounding, current.selection);
                        } else if response.hovered() {
                            painter.rect_filled(rect, rounding, current.row_hover);
                        }

                        // A miniature window preview: face, title strip, border.
                        let swatch = Rect::from_min_size(
                            rect.left_top() + Vec2::new(7.0, 8.0),
                            Vec2::new(52.0, 28.0),
                        );
                        let title_rounding = egui::Rounding {
                            nw: preview.title_rounding.min(5.0),
                            ne: preview.title_rounding.min(5.0),
                            sw: 0.0,
                            se: 0.0,
                        };
                        painter.rect_filled(swatch, egui::Rounding::same(2.0), preview.face);
                        painter.rect_filled(
                            Rect::from_min_size(swatch.min, Vec2::new(52.0, 10.0)),
                            title_rounding,
                            preview.title_bottom,
                        );

                        // A tiny body hint makes the structural difference
                        // obvious: etched 98 box, Aero card, Metro header,
                        // Windows 10 accent stripe, rounded 11 card.
                        let body = Rect::from_min_max(
                            swatch.min + Vec2::new(3.0, 12.0),
                            swatch.max - Vec2::new(3.0, 3.0),
                        );
                        match preview.layout {
                            pml_theme::LayoutStyle::Classic => {
                                painter.rect_filled(body, egui::Rounding::ZERO, preview.face);
                                painter.rect_stroke(
                                    body,
                                    egui::Rounding::ZERO,
                                    Stroke::new(1.0_f32, preview.bevel_dark),
                                );
                            }
                            pml_theme::LayoutStyle::Aero => {
                                painter.rect_filled(body, egui::Rounding::same(1.0), preview.card);
                                painter.rect_stroke(
                                    body,
                                    egui::Rounding::same(1.0),
                                    Stroke::new(1.0_f32, preview.card_border),
                                );
                            }
                            pml_theme::LayoutStyle::Metro => {
                                painter.rect_filled(body, egui::Rounding::ZERO, preview.card);
                                painter.rect_stroke(
                                    body,
                                    egui::Rounding::ZERO,
                                    Stroke::new(1.0_f32, preview.card_border),
                                );
                            }
                            pml_theme::LayoutStyle::Cards => {
                                painter.rect_filled(body, egui::Rounding::ZERO, preview.card);
                                painter.rect_filled(
                                    Rect::from_min_size(body.min, Vec2::new(2.0, body.height())),
                                    egui::Rounding::ZERO,
                                    preview.accent,
                                );
                            }
                            pml_theme::LayoutStyle::Fluent => {
                                painter.rect_filled(body, egui::Rounding::same(2.0), preview.card);
                                painter.rect_stroke(
                                    body,
                                    egui::Rounding::same(2.0),
                                    Stroke::new(1.0_f32, preview.card_border),
                                );
                            }
                        }

                        painter.rect_stroke(
                            swatch,
                            egui::Rounding::same(2.0),
                            Stroke::new(1.0_f32, preview.border),
                        );

                        let fg = if selected {
                            current.selection_text
                        } else {
                            current.text
                        };
                        let secondary = if selected {
                            current.selection_text.gamma_multiply(0.85)
                        } else {
                            current.muted
                        };
                        painter.text(
                            Pos2::new(rect.left() + 68.0, rect.center().y - 7.0),
                            Align2::LEFT_CENTER,
                            theme.label(),
                            pml_theme::font(12.5),
                            fg,
                        );
                        painter.text(
                            Pos2::new(rect.left() + 68.0, rect.center().y + 8.0),
                            Align2::LEFT_CENTER,
                            theme_description(theme),
                            pml_theme::font(10.0),
                            secondary,
                        );

                        if selected {
                            let check = Rect::from_center_size(
                                rect.right_center() - Vec2::new(16.0, 0.0),
                                Vec2::splat(13.0),
                            );
                            pml_theme::check_mark(painter, check, current.selection_text);
                        }

                        if response.clicked() {
                            chosen = Some(theme);
                        }
                        ui.add_space(2.0);
                    }
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if pml_theme::button_sized(ui, "Закрыть", Some(Vec2::new(100.0, 26.0)), true)
                        .clicked()
                    {
                        close_clicked = true;
                    }
                });
            });

        if let Some(theme) = chosen {
            self.set_theme(ctx, theme);
        }
        if !open || close_clicked {
            self.settings_open = false;
        }
    }

    fn draw_error_window(&mut self, ctx: &egui::Context) {
        let Some(message) = self.error.clone() else {
            return;
        };
        let mut window_open = true;
        let mut ok_clicked = false;
        egui::Window::new("Ошибка")
            .collapsible(false)
            .resizable(false)
            .open(&mut window_open)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(message);
                ui.add_space(6.0);
                if pml_theme::button_sized(ui, "OK", Some(Vec2::new(90.0, 24.0)), true).clicked() {
                    ok_clicked = true;
                }
            });
        if !window_open || ok_clicked {
            self.error = None;
        }
    }

    fn draw_bottom(&mut self, ui: &mut egui::Ui) {
        pml_theme::progress_bar(ui, self.progress, &self.status);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let label = if self.busy {
                "Подготовка…"
            } else if self.game_running {
                "Игра запущена"
            } else {
                "Играть"
            };
            if pml_theme::primary_button_sized(
                ui,
                label,
                Some(Vec2::new(150.0, 30.0)),
                !self.busy && !self.game_running,
            )
            .clicked()
            {
                self.start_launch();
            }

            if self.busy && pml_theme::button(ui, "Отмена", true).clicked() {
                if let Some(cancel) = &self.cancel {
                    cancel.store(true, Ordering::SeqCst);
                }
                self.status = "Отмена…".to_owned();
            }

            if pml_theme::button(ui, "Папка сборки", true).clicked() {
                if let Ok(directory) = self.paths.instance_dir(&self.source_id) {
                    open_path(&directory);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(java) = &self.java {
                    ui.label(RichText::new(format!("Java {}", java.major)).small());
                }
                if pml_theme::button(ui, "Издатель", true).clicked() {
                    self.publisher.open = true;
                }
                if pml_theme::button(ui, "Настройки", true).clicked() {
                    self.settings_open = true;
                }
                if pml_theme::button(ui, "?", true)
                    .on_hover_text("Показать приветствие")
                    .clicked()
                {
                    self.welcome.open = true;
                    self.welcome.step = 0;
                }
            });
        });
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
        pml_theme::sync_scale(ctx);
        if !self.auto_refreshed {
            self.auto_refreshed = true;
            self.refresh_manifest();
        }
        self.poll_events(ctx);
        self.poll_publisher(ctx);

        egui::TopBottomPanel::top("pml_title")
            .exact_height(pml_theme::title_bar_height())
            .frame(egui::Frame::none().fill(pml_theme::face()))
            .show(ctx, |ui| {
                pml_theme::title_bar_with_icon(
                    ui,
                    "Odium Launcher — портативный лаунчер Minecraft",
                    self.dog_texture.as_ref(),
                );
            });

        egui::TopBottomPanel::bottom("pml_bottom")
            .exact_height(84.0)
            .frame(
                egui::Frame::none()
                    .fill(pml_theme::face())
                    .inner_margin(egui::Margin::same(8.0)),
            )
            .show(ctx, |ui| self.draw_bottom(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(pml_theme::panel_color())
                    .inner_margin(egui::Margin::same(8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(270.0, ui.available_height()),
                        Layout::top_down(Align::LEFT),
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

fn sunken_well<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let inner = egui::Frame::none()
        .fill(pml_theme::well_color())
        .rounding(egui::Rounding::same(pml_theme::well_rounding()))
        .inner_margin(egui::Margin::same(4.0))
        .show(ui, add_contents);
    pml_theme::paint_bevel(ui.painter(), inner.response.rect, false);
    inner.inner
}

fn total_system_ram_mb() -> u32 {
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

fn format_bytes(bytes: u64) -> String {
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

fn read_pack_info(path: &Path) -> Option<PackInfo> {
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

fn default_dist_dir(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("dist")
    } else {
        path.parent().unwrap_or_else(|| Path::new(".")).join("dist")
    }
}

fn open_path(path: &Path) {
    #[cfg(windows)]
    let command = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(windows))]
    let command = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(error) = command {
        tracing::warn!(%error, path = %path.display(), "could not open folder");
    }
}

async fn run_pipeline(
    paths: PortablePaths,
    downloader: Downloader,
    config: Config,
    source: Source,
    tx: mpsc::UnboundedSender<UiEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let check_cancel = || {
        if cancel.load(Ordering::SeqCst) {
            Err("Отменено".to_owned())
        } else {
            Ok(())
        }
    };

    tx.send(UiEvent::Stage("Загрузка сборки…".to_owned())).ok();
    check_cancel()?;
    let pack = load_pack(downloader.client(), &source, &paths.meta_dir())
        .await
        .map_err(|error| format!("Сборка: {error}"))?;
    let manifest = pack.manifest;
    tx.send(UiEvent::Manifest(Box::new(manifest.clone()))).ok();
    check_cancel()?;

    let server = manifest
        .server
        .clone()
        .unwrap_or_else(|| config.server.clone());
    server
        .validate()
        .map_err(|error| format!("Сервер: {error}"))?;

    let (download_tx, mut download_rx) = mpsc::unbounded_channel();
    let forward_tx = tx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = download_rx.recv().await {
            if forward_tx.send(UiEvent::Download(event)).is_err() {
                break;
            }
        }
    });

    tx.send(UiEvent::Stage("Поиск Java 21…".to_owned())).ok();
    let java = ensure_java(&paths, &downloader, Some(download_tx.clone()))
        .await
        .map_err(|error| format!("Java: {error}"))?;
    tx.send(UiEvent::Java(java.clone())).ok();
    check_cancel()?;

    tx.send(UiEvent::Stage(format!(
        "Установка {} {}…",
        manifest.loader.kind.label(),
        manifest.loader.version
    )))
    .ok();
    let loader = LoaderInstaller::new(paths.clone(), downloader.clone());
    let profile = loader
        .install(
            &manifest.mc_version,
            &manifest.loader,
            &java.java_exe,
            Some(download_tx.clone()),
        )
        .await
        .map_err(|error| format!("Загрузчик: {error}"))?;
    check_cancel()?;

    let instance_dir = paths
        .instance_dir(&source.id)
        .map_err(|error| format!("Папка сборки: {error}"))?;
    let overrides = config
        .optional_mods_overrides
        .get(&source.id)
        .cloned()
        .unwrap_or_default();

    tx.send(UiEvent::Stage("Синхронизация файлов сборки…".to_owned()))
        .ok();
    let sync = ModpackSync::new(paths.clone(), downloader.clone());
    let report = sync
        .sync(SyncRequest {
            manifest: &manifest,
            instance_dir: &instance_dir,
            overrides: &overrides,
            progress: Some(download_tx.clone()),
        })
        .await
        .map_err(|error| format!("Синхронизация: {error}"))?;
    tx.send(UiEvent::Sync(format!(
        "Файлы: {} всего, {} скачано, {} пропущено, {} удалено",
        report.total_files, report.downloaded, report.skipped, report.removed
    )))
    .ok();
    check_cancel()?;

    if let Some(archive) = pack.mrpack {
        tx.send(UiEvent::Stage("Распаковка overrides…".to_owned()))
            .ok();
        let instance = instance_dir.clone();
        let written = tokio::task::spawn_blocking(move || extract_overrides(&archive, &instance))
            .await
            .map_err(|error| format!("Распаковка overrides: {error}"))?
            .map_err(|error| format!("Распаковка overrides: {error}"))?;
        tx.send(UiEvent::Sync(format!(
            "Overrides: {written} файлов распаковано"
        )))
        .ok();
        check_cancel()?;
    }

    tx.send(UiEvent::Stage("Подготовка Minecraft…".to_owned()))
        .ok();
    let minecraft = Minecraft::new(paths.clone(), downloader.clone());
    let user = UserProfile::offline(config.nick.clone());
    let plan = minecraft
        .prepare_launch(LaunchRequest {
            mc_version: &manifest.mc_version,
            instance_dir: &instance_dir,
            loader_profile: Some(&profile),
            user: &user,
            server: &server,
            jvm: &config.jvm,
            progress: Some(download_tx.clone()),
        })
        .await
        .map_err(|error| format!("Minecraft: {error}"))?;
    check_cancel()?;

    drop(download_tx);
    let _ = forwarder.await;

    tx.send(UiEvent::Stage("Запуск игры…".to_owned())).ok();
    let spec = LaunchSpec {
        java_exe: java.java_exe,
        plan,
        logs_dir: paths.logs_dir(),
    };
    let game = spawn_game(&spec).map_err(|error| format!("Запуск: {error}"))?;
    tx.send(UiEvent::GameStarted {
        pid: game.pid,
        log_path: game.log_path.clone(),
    })
    .ok();

    let status = game
        .wait()
        .await
        .map_err(|error| format!("Игра: {error}"))?;
    tx.send(UiEvent::GameExited(status.code())).ok();
    Ok(())
}
