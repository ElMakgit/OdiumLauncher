//! Main window panels and dialogs. These methods only render and dispatch;
//! all I/O stays in the pipeline or the publisher task.

use egui::{Align, Align2, Color32, Layout, Pos2, Rect, RichText, Sense, Stroke, Vec2};
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
        pml_theme::group_box(ui, self.theme, "Профили", |ui| {
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
        // Snapshot the facts before drawing: the panel closure later mutates
        // `self` (overrides, refresh, reset), so it cannot hold a borrow of the
        // manifest at the same time.
        let facts = self.manifest.as_ref().map(|manifest| {
            let optional = manifest.optional_files.len();
            (
                manifest.name.clone(),
                manifest.version.clone(),
                manifest.mc_version.clone(),
                format!(
                    "{} {}",
                    manifest.loader.kind.label(),
                    manifest.loader.version
                ),
                if optional == 0 {
                    manifest.files.len().to_string()
                } else {
                    format!("{} + {} доп.", manifest.files.len(), optional)
                },
            )
        });
        let manifest_error = self.manifest_error.clone();
        let insecure_source = self
            .selected_source()
            .is_some_and(|source| source.manifest_url.starts_with("http://"));

        pml_theme::group_box(ui, self.theme, "Сборка", |ui| {
            if let Some((name, version, minecraft, loader, files)) = &facts {
                egui::Grid::new("pack_facts")
                    .num_columns(2)
                    .spacing([10.0, 3.0])
                    .show(ui, |ui| {
                        fact(ui, self.theme, "Название", name);
                        if !version.is_empty() {
                            fact(ui, self.theme, "Версия", version);
                        }
                        fact(ui, self.theme, "Minecraft", minecraft);
                        fact(ui, self.theme, "Загрузчик", loader);
                        fact(ui, self.theme, "Файлы", files);
                    });
            } else if let Some(error) = &manifest_error {
                ui.colored_label(pml_theme::error_color(self.theme), "Манифест недоступен");
                ui.label(RichText::new(error).small());
            } else {
                ui.label(
                    RichText::new("Проверка сборки…")
                        .italics()
                        .color(pml_theme::muted_color(self.theme)),
                );
            }

            if insecure_source {
                ui.add_space(4.0);
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
            // Wrapped so a bigger font or a narrow window pushes the second
            // button to its own line instead of widening the column.
            ui.horizontal_wrapped(|ui| {
                if pml_theme::button(ui, self.theme, "Проверить обновления", !self.busy).clicked()
                {
                    self.refresh_manifest();
                }
                let can_reset = !self.busy && !self.game_running && self.reset_rx.is_none();
                if pml_theme::button(ui, self.theme, "Удалить файлы сборки", can_reset).clicked()
                {
                    self.confirm_reset = true;
                }
            });
            ui.label(
                RichText::new(
                    "Удаление заставит лаунчер скачать модпак заново. Миры и скриншоты сохранятся.",
                )
                .small(),
            );
        });
    }

    pub(crate) fn draw_player(&mut self, ui: &mut egui::Ui) {
        pml_theme::group_box(ui, self.theme, "Игрок и параметры", |ui| {
            // The cards shrink to their content by default, which leaves a bare
            // strip on the right; the panel should line up with the window edge.
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                label_slot(ui, "Ник");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.nick)
                        .desired_width(200.0)
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
                label_slot(ui, "Xms");
                pml_theme::slider_u32(ui, self.theme, &mut self.xms, 512..=self.max_xmx, 170.0);
                value_slot(ui, &format!("{} МБ", self.xms));
            });
            ui.horizontal(|ui| {
                label_slot(ui, "Xmx");
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
                value_slot(ui, &format!("{} МБ", self.xmx));
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
            // The log is always the last panel, so it takes whatever height is
            // left. Cards and the classic frame need a row for the buttons.
            let log_height = (ui.available_height() - 30.0).max(120.0);
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
        let mut close_clicked = false;
        let mut diagnostics_clicked = false;
        let mut chosen: Option<Theme> = None;
        let mut text_scale: Option<u8> = None;
        let mut save_settings = false;
        let mut close_window = false;
        let mut request_uninstall = false;
        // Settings live in their own OS window so they can be dragged anywhere,
        // including outside the launcher. An immediate viewport is used because
        // it borrows `self`; deferred viewports would require 'static state.
        let viewport_id = egui::ViewportId::from_hash_of("odium_settings");
        ctx.show_viewport_immediate(
            viewport_id,
            egui::ViewportBuilder::default()
                .with_title("Настройки — Odium Launcher")
                .with_inner_size([470.0, 690.0])
                .with_min_inner_size([430.0, 420.0]),
            |ctx, _class| {
                if ctx.input(|input| input.viewport().close_requested()) && !self.uninstalling {
                    close_window = true;
                    ctx.request_repaint();
                }
                // Pinned below the scrolling content, so the buttons stay
                // reachable even when the window is squeezed to its minimum.
                egui::TopBottomPanel::bottom("odium_settings_actions")
                    .frame(
                        egui::Frame::none()
                            .fill(pml_theme::panel_color(self.theme))
                            .inner_margin(egui::Margin::same(10.0)),
                    )
                    .show(ctx, |ui| {
                        if self.uninstalling {
                            ui.disable();
                        }
                        // The diagnostics button drops to a second line when the
                        // window is at its minimum width, never off the edge.
                        ui.horizontal_wrapped(|ui| {
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
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::none()
                            .fill(pml_theme::panel_color(self.theme))
                            .inner_margin(egui::Margin::same(10.0)),
                    )
                    .show(ctx, |ui| {
                        if self.uninstalling {
                            ui.disable();
                        }
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
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
                                        let rounding = egui::Rounding::same(
                                            pml_theme::row_rounding(self.theme),
                                        );

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
                                        painter.rect_filled(
                                            swatch,
                                            egui::Rounding::same(2.0),
                                            preview.face,
                                        );
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
                                                painter.rect_filled(
                                                    body,
                                                    egui::Rounding::ZERO,
                                                    preview.face,
                                                );
                                                painter.rect_stroke(
                                                    body,
                                                    egui::Rounding::ZERO,
                                                    Stroke::new(1.0_f32, preview.bevel_dark),
                                                );
                                            }
                                            pml_theme::LayoutStyle::Aero => {
                                                painter.rect_filled(
                                                    body,
                                                    egui::Rounding::same(1.0),
                                                    preview.card,
                                                );
                                                painter.rect_stroke(
                                                    body,
                                                    egui::Rounding::same(1.0),
                                                    Stroke::new(1.0_f32, preview.card_border),
                                                );
                                            }
                                            pml_theme::LayoutStyle::Metro => {
                                                painter.rect_filled(
                                                    body,
                                                    egui::Rounding::ZERO,
                                                    preview.card,
                                                );
                                                painter.rect_stroke(
                                                    body,
                                                    egui::Rounding::ZERO,
                                                    Stroke::new(1.0_f32, preview.card_border),
                                                );
                                            }
                                            pml_theme::LayoutStyle::Cards => {
                                                painter.rect_filled(
                                                    body,
                                                    egui::Rounding::ZERO,
                                                    preview.card,
                                                );
                                                painter.rect_filled(
                                                    Rect::from_min_size(
                                                        body.min,
                                                        Vec2::new(2.0, body.height()),
                                                    ),
                                                    egui::Rounding::ZERO,
                                                    preview.accent,
                                                );
                                            }
                                            pml_theme::LayoutStyle::Fluent => {
                                                painter.rect_filled(
                                                    body,
                                                    egui::Rounding::same(2.0),
                                                    preview.card,
                                                );
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
                                            truncate_label(
                                                painter,
                                                theme_description(theme),
                                                pml_theme::font(10.0),
                                                (rect.width() - 68.0 - 26.0).max(60.0),
                                                secondary,
                                            ),
                                            pml_theme::font(10.0),
                                            secondary,
                                        );

                                        if selected {
                                            let check = Rect::from_center_size(
                                                rect.right_center() - Vec2::new(16.0, 0.0),
                                                Vec2::splat(13.0),
                                            );
                                            pml_theme::check_mark(
                                                painter,
                                                check,
                                                current.selection_text,
                                            );
                                        }

                                        if response.clicked() {
                                            chosen = Some(theme);
                                        }
                                        ui.add_space(2.0);
                                    }
                                });

                                ui.add_space(8.0);
                                pml_theme::group_box(ui, self.theme, "Текст", |ui| {
                                    // Discrete steps instead of a slider: every size change
                                    // reflows the whole window, and a continuous drag makes the
                                    // labels jump on every pixel step.
                                    ui.label(format!("Размер шрифта: {}%", self.font_scale));
                                    ui.horizontal_wrapped(|ui| {
                                        for (percent, label) in [
                                            (90u8, "Мелкий"),
                                            (pml_config::DEFAULT_FONT_SCALE, "Обычный"),
                                            (130, "Крупный"),
                                            (150, "Очень крупный"),
                                        ] {
                                            let selected = self.font_scale == percent;
                                            let clicked = if selected {
                                                pml_theme::primary_button_sized(
                                                    ui, self.theme, label, None, true,
                                                )
                                                .clicked()
                                            } else {
                                                pml_theme::button(ui, self.theme, label, true)
                                                    .clicked()
                                            };
                                            if clicked && !selected {
                                                text_scale = Some(percent);
                                                save_settings = true;
                                            }
                                        }
                                    });
                                    ui.add_space(4.0);
                                    ui.label(
                                        RichText::new(
                                            "Пример: так выглядит текст лаунчера и подписи.",
                                        )
                                        .font(pml_theme::font(12.0)),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "Крупнее — удобнее на ноутбуке или 4K-экране.",
                                        )
                                        .font(pml_theme::font(10.0))
                                        .color(
                                            pml_theme::text_color(self.theme).gamma_multiply(0.6),
                                        ),
                                    );
                                });

                                ui.add_space(8.0);
                                pml_theme::group_box(ui, self.theme, "Удаление", |ui| {
                                    ui.label(
                                        RichText::new(
                                            "Полностью удаляет лаунчер и всё, что он создал: версии, библиотеки, Java, миры, скриншоты, настройки и логи.",
                                        )
                                        .font(pml_theme::font(10.0)),
                                    );
                                    ui.add_space(3.0);
                                    ui.label(
                                        RichText::new(format!(
                                            "Папка данных: {}",
                                            self.paths.root().display()
                                        ))
                                        .font(pml_theme::mono_font(9.0))
                                        .color(
                                            pml_theme::text_color(self.theme).gamma_multiply(0.6),
                                        ),
                                    );
                                    ui.add_space(6.0);
                                    let can_uninstall =
                                        !self.busy && !self.game_running && !self.uninstalling;
                                    if pml_theme::button(
                                        ui,
                                        self.theme,
                                        "Удалить лаунчер…",
                                        can_uninstall,
                                    )
                                    .clicked()
                                    {
                                        self.uninstall_confirm = true;
                                        self.uninstall_agree = false;
                                    }
                                });
                            });
                    });

                if self.uninstall_confirm {
                    // Second, explicit confirmation: this cannot be undone, so
                    // the destructive button stays disabled until the checkbox
                    // is ticked.
                    let mut dialog_open = true;
                    let mut confirmed = false;
                    let mut cancelled = false;
                    egui::Window::new("Удалить лаунчер?")
                        .collapsible(false)
                        .resizable(false)
                        .open(&mut dialog_open)
                        .default_width(400.0)
                        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                        .show(ctx, |ui| {
                            ui.set_max_width(390.0);
                            ui.label(RichText::new("Будут безвозвратно удалены:").strong());
                            for item in [
                                "версии, библиотеки и Java",
                                "сборки, миры, скриншоты и бэкапы",
                                "настройки, логи и кэш загрузок",
                            ] {
                                ui.label(
                                    RichText::new(format!("• {item}"))
                                        .font(pml_theme::font(10.0)),
                                );
                            }
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(format!(
                                    "Папка данных: {}",
                                    self.paths.root().display()
                                ))
                                .font(pml_theme::mono_font(9.0))
                                .color(pml_theme::text_color(self.theme).gamma_multiply(0.6)),
                            );
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(
                                    "Отменить это нельзя. Файл самого лаунчера тоже будет удалён.",
                                )
                                .color(pml_theme::error_color(self.theme)),
                            );
                            ui.add_space(6.0);
                            ui.checkbox(
                                &mut self.uninstall_agree,
                                "Я понимаю, что данные не восстановить",
                            );
                            ui.add_space(8.0);
                            ui.horizontal_wrapped(|ui| {
                                if pml_theme::primary_button_sized(
                                    ui,
                                    self.theme,
                                    "Удалить всё и выйти",
                                    Some(Vec2::new(190.0, 26.0)),
                                    self.uninstall_agree,
                                )
                                .clicked()
                                {
                                    confirmed = true;
                                }
                                if pml_theme::button(ui, self.theme, "Отмена", true).clicked() {
                                    cancelled = true;
                                }
                            });
                        });
                    if confirmed {
                        self.uninstall_confirm = false;
                        self.uninstall_agree = false;
                        request_uninstall = true;
                    } else if cancelled || !dialog_open {
                        self.uninstall_confirm = false;
                        self.uninstall_agree = false;
                    }
                }

                if self.uninstalling {
                    egui::Window::new("Удаление…")
                        .collapsible(false)
                        .resizable(false)
                        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                        .show(ctx, |ui| {
                            ui.set_max_width(360.0);
                            ui.label("Удаляю файлы лаунчера. Это может занять несколько секунд.");
                            ui.label(RichText::new("Окно закроется само.").small());
                        });
                }
            },
        );

        if request_uninstall {
            self.start_uninstall();
        }

        if let Some(theme) = chosen {
            self.set_theme(ctx, theme);
        }
        if let Some(percent) = text_scale {
            self.set_font_scale(ctx, percent);
        }
        if save_settings {
            self.persist();
        }
        if diagnostics_clicked {
            self.export_diagnostics();
        }
        if (close_window || close_clicked) && !self.uninstalling {
            self.settings_open = false;
            ctx.request_repaint();
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

    pub(crate) fn draw_reset_confirm(&mut self, ctx: &egui::Context) {
        if !self.confirm_reset {
            return;
        }
        let mut open = true;
        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new("Удалить файлы сборки?")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(400.0);
                ui.label("Лаунчер удалит моды, конфиги и другие файлы, установленные сборкой.");
                ui.label("При следующем запуске они скачаются заново.");
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Миры (saves), скриншоты и логи останутся на месте.").small(),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if pml_theme::primary_button_sized(
                        ui,
                        self.theme,
                        "Удалить",
                        Some(Vec2::new(100.0, 24.0)),
                        true,
                    )
                    .clicked()
                    {
                        confirmed = true;
                    }
                    if pml_theme::button_sized(
                        ui,
                        self.theme,
                        "Отмена",
                        Some(Vec2::new(100.0, 24.0)),
                        true,
                    )
                    .clicked()
                    {
                        cancelled = true;
                    }
                });
            });
        if confirmed {
            self.confirm_reset = false;
            self.delete_build_files();
        } else if cancelled || !open {
            self.confirm_reset = false;
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

/// One "key / value" row of a fact grid: the key is dim and the value carries
/// the weight, which makes a block of pack facts readable at a glance.
fn fact(ui: &mut egui::Ui, theme: Theme, key: &str, value: &str) {
    ui.label(
        RichText::new(key)
            .small()
            .color(pml_theme::muted_color(theme)),
    );
    ui.label(value);
    ui.end_row();
}

/// A fixed-width field label. Two sliders started with labels of different
/// lengths otherwise sit on different pixels, which reads as broken layout.
fn label_slot(ui: &mut egui::Ui, text: &str) {
    ui.allocate_ui_with_layout(
        Vec2::new(34.0, 18.0),
        Layout::left_to_right(Align::Center),
        |ui| ui.label(text),
    );
}

/// A right-aligned numeric readout next to a slider, wide enough for "65536 МБ".
fn value_slot(ui: &mut egui::Ui, text: &str) {
    ui.allocate_ui_with_layout(
        Vec2::new(70.0, 18.0),
        Layout::right_to_left(Align::Center),
        |ui| ui.label(RichText::new(text).small()),
    );
}

/// Shortens a painted label with an ellipsis so it never runs into a column
/// that is drawn to its right.
fn truncate_label(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    max_width: f32,
    color: Color32,
) -> String {
    if painter
        .layout_no_wrap(text.to_owned(), font.clone(), color)
        .size()
        .x
        <= max_width
    {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate = format!("{}…", chars.iter().collect::<String>());
        if painter
            .layout_no_wrap(candidate.clone(), font.clone(), color)
            .size()
            .x
            <= max_width
        {
            return candidate;
        }
    }
    "…".to_owned()
}
