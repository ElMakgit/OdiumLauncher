//! Publisher window: uploads a manifest pack or an `.mrpack`, then optionally
//! builds a ready-to-send `dist/` folder through `pml-publish`.

use std::path::PathBuf;
use std::time::Duration;

use egui::{RichText, Vec2};
use pml_config::{ServerSpec, SourceKind};
use pml_publish::{
    build_dist, DistRequest, ManifestPublish, MrpackPublish, Progress as PublishProgress, Publisher,
};
use tokio::sync::mpsc;

use super::util::{
    default_dist_dir, open_path, read_pack_info, sunken_well, total_system_ram_mb, PackInfo,
};
use super::LauncherApp;

enum PublisherEvent {
    Log(String),
    Progress(f32),
    Done(Result<String, String>),
}

pub(crate) struct PublisherState {
    pub(crate) open: bool,
    pub(crate) path: String,
    pub(crate) path_is_dir: bool,
    pub(crate) info: Option<PackInfo>,
    pub(crate) repo: String,
    pub(crate) token: String,
    pub(crate) tag: String,
    pub(crate) branch: String,
    pub(crate) release_name: String,
    pub(crate) server_ip: String,
    pub(crate) server_port: String,
    pub(crate) output_dir: String,
    pub(crate) create_repo: bool,
    pub(crate) build_dist: bool,
    pub(crate) busy: bool,
    pub(crate) progress: f32,
    pub(crate) log: Vec<String>,
    /// `log` joined once per added line instead of once per frame.
    pub(crate) log_text: String,
    rx: Option<mpsc::UnboundedReceiver<PublisherEvent>>,
    /// Result of reading the selected pack, delivered off the UI thread.
    info_rx: Option<mpsc::UnboundedReceiver<(PathBuf, Option<PackInfo>)>>,
    pub(crate) result: Option<Result<String, String>>,
}

impl PublisherState {
    pub(crate) fn push_log(&mut self, line: String) {
        if !self.log_text.is_empty() {
            self.log_text.push('\n');
        }
        self.log_text.push_str(&line);
        self.log.push(line);
    }

    pub(crate) fn clear_log(&mut self) {
        self.log.clear();
        self.log_text.clear();
    }

    pub(crate) fn new() -> Self {
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
            log_text: String::new(),
            rx: None,
            info_rx: None,
            result: None,
        }
    }
}

impl LauncherApp {
    pub(crate) fn poll_publisher(&mut self, ctx: &egui::Context) {
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
            // The publish task ends with `Done`. A channel that closes without
            // it means the task died (panic, runtime shutdown): without this
            // the window stayed in "Публикация…" forever.
            if self.publisher.busy {
                self.publisher.busy = false;
                self.publisher.progress = 0.0;
                self.publisher.result = Some(Err("Публикация прервалась неожиданно".to_owned()));
            }
        }
        for event in events {
            match event {
                PublisherEvent::Log(line) => self.publisher.push_log(line),
                PublisherEvent::Progress(value) => self.publisher.progress = value,
                PublisherEvent::Done(result) => {
                    self.publisher.busy = false;
                    self.publisher.progress = 1.0;
                    if let Ok(message) = &result {
                        let message = message.clone();
                        self.publisher.push_log(message);
                    }
                    self.publisher.result = Some(result);
                }
            }
        }
        self.poll_pack_info();
        if self.publisher.busy {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    /// Applies the pack info once the blocking read finished. The path is
    /// compared so a slow read of a previously selected file cannot overwrite
    /// the facts of the file the user picked afterwards.
    fn poll_pack_info(&mut self) {
        let Some(mut receiver) = self.publisher.info_rx.take() else {
            return;
        };
        match receiver.try_recv() {
            Ok((path, info)) => {
                if self.publisher.path == path.display().to_string() {
                    if let Some(info) = &info {
                        if self.publisher.release_name.trim().is_empty() {
                            self.publisher.release_name =
                                format!("{} {}", info.name, info.version).trim().to_owned();
                        }
                        if self.publisher.tag.trim().is_empty() && !info.version.is_empty() {
                            self.publisher.tag = format!("v{}", info.version);
                        }
                    }
                    self.publisher.info = info;
                }
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                self.publisher.info_rx = Some(receiver);
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {}
        }
    }

    pub(crate) fn set_publisher_path(&mut self, path: PathBuf) {
        self.publisher.path = path.display().to_string();
        self.publisher.path_is_dir = path.is_dir();
        if self.publisher.output_dir.trim().is_empty() {
            self.publisher.output_dir = default_dist_dir(&path).display().to_string();
        }
        self.publisher.info = None;
        self.publisher.clear_log();
        self.publisher.result = None;
        // Reading a manifest or opening a .mrpack zip touches the disk, so it
        // runs on a worker and the panel shows no facts until it arrives.
        let (tx, rx) = mpsc::unbounded_channel();
        self.publisher.info_rx = Some(rx);
        self.runtime.spawn_blocking(move || {
            let info = read_pack_info(&path);
            let _ = tx.send((path, info));
        });
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

    pub(crate) fn start_publish(&mut self) {
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
        let exe = std::env::current_exe().ok();
        let runtime = self.runtime.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        self.publisher.rx = Some(rx);
        self.publisher.busy = true;
        self.publisher.progress = 0.0;
        self.publisher.clear_log();
        self.publisher.result = None;

        runtime.spawn(async move {
            // Same read as the preview, but the upload must not depend on the
            // preview having finished: derive the tag and release name here.
            let info_path = path.clone();
            let info = tokio::task::spawn_blocking(move || read_pack_info(&info_path))
                .await
                .ok()
                .flatten();
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

    pub(crate) fn draw_publisher(&mut self, ctx: &egui::Context) {
        if !self.publisher.open {
            return;
        }
        let mut pick_file = false;
        let mut pick_dir = false;
        let mut pick_output = false;
        let mut publish = false;
        let mut open_output = false;
        let mut copy_log = false;
        let mut close_window = false;

        // A dedicated OS window: the publisher stays reachable while the main
        // launcher is busy, and can be dragged to a second monitor.
        let viewport_id = egui::ViewportId::from_hash_of("odium_publisher");
        ctx.show_viewport_immediate(
            viewport_id,
            egui::ViewportBuilder::default()
                .with_title("Публикация сборки — для издателя")
                .with_inner_size([760.0, 700.0])
                .with_min_inner_size([600.0, 460.0]),
            |ctx, _class| {
                if ctx.input(|input| input.viewport().close_requested()) {
                    close_window = true;
                    ctx.request_repaint();
                }
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::none()
                            .fill(pml_theme::panel_color(self.theme))
                            .inner_margin(egui::Margin::same(10.0)),
                    )
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
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
                            if pml_theme::button(ui, self.theme, "Файл…", !self.publisher.busy)
                                .clicked()
                            {
                                pick_file = true;
                            }
                            if pml_theme::button(ui, self.theme, "Папка…", !self.publisher.busy)
                                .clicked()
                            {
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
                        if pml_theme::button(ui, self.theme, "Выбрать…", !self.publisher.busy)
                            .clicked()
                        {
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
                        self.theme,
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
                        && pml_theme::button(ui, self.theme, "Открыть папку", true).clicked()
                    {
                        open_output = true;
                    }
                    if pml_theme::button(ui, self.theme, "Копировать лог", true).clicked() {
                        copy_log = true;
                    }
                });

                pml_theme::progress_bar(
                    ui,
                    self.theme,
                    self.publisher.progress,
                    if self.publisher.busy {
                        "Публикация…"
                    } else {
                        "Готово"
                    },
                );

                ui.add_space(4.0);
                sunken_well(ui, self.theme, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(170.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if self.publisher.log.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new("Здесь появится журнал публикации.")
                                            .monospace(),
                                    )
                                    .selectable(true)
                                    .wrap(),
                                );
                            } else {
                                // Joined once per added line, not per frame.
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&self.publisher.log_text).monospace(),
                                    )
                                    .selectable(true)
                                    .wrap(),
                                );
                            }
                        });
                });

                if let Some(Err(error)) = &self.publisher.result {
                    ui.colored_label(pml_theme::error_color(self.theme), error);
                }
                            });
                    });
            },
        );

        if close_window {
            self.publisher.open = false;
        }
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
            ctx.copy_text(self.publisher.log_text.clone());
        }
    }
}
