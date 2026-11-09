//! Windows-style launcher window and end-to-end orchestration task.
//!
//! The crate is split by responsibility:
//!
//! * [`app`] — window state, lifecycle and the eframe entry point.
//! * [`app::pipeline`] — manifest -> Java -> loader -> modpack -> Minecraft.
//! * [`app::publisher`] — GitHub publishing dialog and `dist/` builder.
//! * [`app::views`] — main panels, settings and error dialog.
//! * [`app::welcome`] — first-run guide.
//! * [`app::diagnostics`] — one-file support report.
//! * [`assistant`] / [`glass`] — the mascot and the Windows 7 backdrop.

mod app;
mod assistant;
mod glass;

pub use app::{run, UiError};
