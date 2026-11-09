use eframe::egui;

struct Minimal;

impl eframe::App for Minimal {
    fn update(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {}
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([920.0, 660.0]),
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "Minimal",
        options,
        Box::new(|_creation| Ok(Box::new(Minimal))),
    )
}
