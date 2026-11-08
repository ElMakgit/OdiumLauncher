//! Main window panels and dialogs. These methods only render and dispatch;
//! all I/O stays in the pipeline or the publisher task.

use egui::{Align, Align2, Layout, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use pml_config::{validate_nick, Theme};

use super::util::{open_path, sunken_well, theme_description};
use super::LauncherApp;

impl LauncherApp {
    pub(crate) fn draw_sources(&mut self, ui: &mut egui::Ui) {
        let sources = self.store.config().sources.clone();
        // A distributed pack ships exactly one source; showing a one-item list
        // only confuses the player, so the manifest panel takes its place.
        if sources.len() <= 1 {
            return;
        }
        pml_theme::group_box(ui, self.theme, "Сборка", |ui| {
            let mut clicked = None;
            for source in &sources {
                let selected = source.id == self.source_id;
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::click());
                let painter = ui.painter();
                if selected {
                    painter.rect_filled(
                        rect,
                        egui::Rounding::same(pml_theme::row_rounding(self.theme)),
                        pml_theme::selection_color(self.theme),
                    );
                } else if response.hovered() {
                    painter.rect_filled(
                        rect,
                        egui::Rounding::same(pml_theme::row_rounding(self.theme)),
                        pml_theme::row_hover_color(self.theme),
                    );
                }
                painter.text(
                    rect.left_center() + Vec2::new(5.0, 0.0),
                    Align2::LEFT_CENTER,
                    &source.name,
                    pml_theme::font(12.0),
                    if selected {
                        pml_theme::selection_text_color(self.theme)
                    } else {
                        pml_theme::text_color(self.theme)
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

    pub(crate) fn draw_manifest_info(&mut self, ui: &mut egui::Ui) {
        pml_theme::group_box(ui, self.theme, "Сборка", |ui| {
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
                ui.colored_label(pml_theme::error_color(self.theme), "Манифест недоступен");
                ui.label(RichText::new(error).small());
            } else {
                ui.label("Проверка сборки…");
            }

            if let Some(source) = self.selected_source() {
                if source.manifest_url.starts_with("http://") {
                    ui.colored_label(
                        pml_theme::error_color(self.theme),
                        "Источник использует незащищённый http://",
                    );
                    ui.label(
                        RichText::new(
                            "Файлы могут быть подменены в сети. Попросите издателя включить HTTPS.",
                        )
                        .small(),
                    );
                }
            }

            let server = self
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.server.clone())
                .unwrap_or_else(|| self.store.config().server.clone());
            ui.add_space(2.0);
            let (text, color) = match self.server_status {
                Some(status) if status.reachable => (
                    format!(
                        "Сервер {}: онлайн ({} мс)",
                        server.display(),
                        status.latency_ms
                    ),
                    None,
                ),
                Some(_) => (
                    format!("Сервер {}: недоступен", server.display()),
                    Some(pml_theme::error_color(self.theme)),
                ),
                None => (format!("Сервер {}: проверка…", server.display()), None),
            };
            match color {
                Some(color) => {
                    ui.colored_label(color, text);
                }
                None => {
                    ui.label(text);
                }
            }

            // Optional components are part of the manifest contract; exposing
            // them here makes the stored per-source overrides discoverable.
            let optional_files = self
                .manifest
                .as_ref()
                .map(|manifest| manifest.optional_files.clone())
                .unwrap_or_default();
            if !optional_files.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("Дополнительные компоненты").strong());
                let source_id = self.source_id.clone();
                let overrides = self
                    .store
                    .config()
                    .optional_mods_overrides
                    .get(&source_id)
                    .cloned()
                    .unwrap_or_default();
                let mut changed = None;
                egui::ScrollArea::vertical()
                    .max_height(96.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for file in &optional_files {
                            let path = pml_modpack::normalize_path(&file.path);
                            let mut enabled = overrides.get(&path).copied().unwrap_or(true);
                            if pml_theme::checkbox(ui, self.theme, &mut enabled, &path, true)
                                .changed()
                            {
                                changed = Some((path, enabled));
                            }
                        }
                    });
                if let Some((path, enabled)) = changed {
                    self.store
                        .config_mut()
                        .optional_mods_overrides
                        .entry(source_id)
                        .or_default()
                        .insert(path, enabled);
                    self.persist();
                }
                ui.label(RichText::new("Изменения применятся при следующем запуске.").small());
            }

            ui.add_space(4.0);
            if pml_theme::button(ui, self.theme, "Проверить обновления", !self.busy).clicked()
            {
                self.refresh_manifest();
            }
        });
    }

    pub(crate) fn draw_player(&mut self, ui: &mut egui::Ui) {
        pml_theme::group_box(ui, self.theme, "Игрок и параметры", |ui| {
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
                    ui.colored_label(pml_theme::error_color(self.theme), error.to_string());
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
            if pml_theme::checkbox(ui, self.theme, &mut auto, "Авто: ровно половина ОЗУ", true)
                .changed()
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
                pml_theme::slider_u32(ui, self.theme, &mut self.xms, 512..=self.max_xmx, 170.0);
                ui.label(format!("{} МБ", self.xms));
            });
            ui.horizontal(|ui| {
                ui.label("Xmx:");
                let response = pml_theme::slider_u32(
                    ui,
                    self.theme,
                    &mut self.xmx,
                    1024..=self.max_xmx,
                    170.0,
                );
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
            pml_theme::checkbox(
                ui,
                self.theme,
                &mut close,
                "Закрывать лаунчер после запуска",
                true,
            );
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

    pub(crate) fn draw_log(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        pml_theme::group_box(ui, self.theme, "Журнал", |ui| {
            let log_height = if self.theme == Theme::Win98 {
                150.0
            } else {
                132.0
            };
            sunken_well(ui, self.theme, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(log_height)
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.log_lines.is_empty() {
                            ui.label(
                                RichText::new("Здесь появятся строки лога игры.")
                                    .italics()
                                    .color(pml_theme::muted_color(self.theme)),
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
                if pml_theme::button(ui, self.theme, "Папка логов", true).clicked() {
                    open_path(&self.paths.logs_dir());
                }
                if pml_theme::button(ui, self.theme, "Копировать", !self.log_lines.is_empty())
                    .clicked()
                {
                    ui.ctx().copy_text(self.log_lines.join("\r\n"));
                }
            });
        });
    }

    pub(crate) fn draw_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut close_clicked = false;
        let mut diagnostics_clicked = false;
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

                pml_theme::group_box(ui, self.theme, "Оформление", |ui| {
                    for theme in Theme::ALL {
                        let selected = self.theme == theme;
                        let (rect, response) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 44.0),
                            Sense::click(),
                        );
                        let current = pml_theme::palette(self.theme);
                        let preview = pml_theme::palette(theme);
                        let painter = ui.painter();
                        let rounding = egui::Rounding::same(pml_theme::row_rounding(self.theme));

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
                    if pml_theme::button_sized(
                        ui,
                        self.theme,
                        "Закрыть",
                        Some(Vec2::new(100.0, 26.0)),
                        true,
                    )
                    .clicked()
                    {
                        close_clicked = true;
                    }
                    if pml_theme::button(ui, self.theme, "Экспорт диагностики", !self.busy)
                        .clicked()
                    {
                        diagnostics_clicked = true;
                    }
                });
            });

        if let Some(theme) = chosen {
            self.set_theme(ctx, theme);
        }
        if diagnostics_clicked {
            self.export_diagnostics();
        }
        if !open || close_clicked {
            self.settings_open = false;
        }
    }

    pub(crate) fn draw_error_window(&mut self, ctx: &egui::Context) {
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
                if pml_theme::button_sized(ui, self.theme, "OK", Some(Vec2::new(90.0, 24.0)), true)
                    .clicked()
                {
                    ok_clicked = true;
                }
            });
        if !window_open || ok_clicked {
            self.error = None;
        }
    }

    pub(crate) fn draw_bottom(&mut self, ui: &mut egui::Ui) {
        pml_theme::progress_bar(ui, self.theme, self.progress, &self.status);
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
                self.theme,
                label,
                Some(Vec2::new(150.0, 30.0)),
                !self.busy && !self.game_running,
            )
            .clicked()
            {
                self.start_launch();
            }

            if self.busy && pml_theme::button(ui, self.theme, "Отмена", true).clicked() {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                }
                self.status = "Отмена…".to_owned();
            }

            if pml_theme::button(ui, self.theme, "Папка сборки", true).clicked() {
                if let Ok(directory) = self.paths.instance_dir(&self.source_id) {
                    open_path(&directory);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(java) = &self.java {
                    ui.label(RichText::new(format!("Java {}", java.major)).small());
                }
                if pml_theme::button(ui, self.theme, "Издатель", true).clicked() {
                    self.publisher.open = true;
                }
                if pml_theme::button(ui, self.theme, "Настройки", true).clicked() {
                    self.settings_open = true;
                }
                if pml_theme::button(ui, self.theme, "?", true)
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
