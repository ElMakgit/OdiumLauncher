//! First-run guide and its small state machine.

use std::time::Duration;

use egui::{Align, Align2, Layout, RichText, Vec2};

use super::LauncherApp;

pub(crate) struct WelcomeState {
    pub(crate) open: bool,
    pub(crate) step: usize,
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

impl LauncherApp {
    pub(crate) fn finish_welcome(&mut self) {
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

    pub(crate) fn draw_welcome(&mut self, ctx: &egui::Context) {
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
                    .fill(pml_theme::face(self.theme))
                    .inner_margin(egui::Margin::same(4.0)),
            )
            .show(ctx, |ui| {
                pml_theme::title_bar(ui, self.theme, "Помощник Байт — добро пожаловать!");
                ui.add_space(8.0);
                ui.horizontal_top(|ui| {
                    crate::assistant::draw_assistant(ui, ctx.input(|input| input.time));
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
                    if pml_theme::button(ui, self.theme, "Пропустить", true).clicked() {
                        action = Some(WelcomeAction::Skip);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let last = self.welcome.step + 1 == WELCOME_TITLES.len();
                        if pml_theme::primary_button_sized(
                            ui,
                            self.theme,
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
                                self.theme,
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
}
