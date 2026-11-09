//! Odium Launcher entry point: portable paths, daily file logging, a small Tokio
//! runtime and the Win98-style eframe window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::Duration;

use anyhow::{Context, Result};
use pml_config::{ConfigStore, PortablePaths};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    // The mutable data root lives in the system app-data directory (with a
    // portable override), never next to the executable or the working
    // directory.
    let paths = PortablePaths::discover().context("не удалось определить папку данных лаунчера")?;
    let _log_guard = init_logging(&paths)?;
    install_panic_hook();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        root = %paths.root().display(),
        "Odium Launcher starting"
    );

    let store = ConfigStore::load_or_create(paths.config_file())
        .context("не удалось загрузить config.json лаунчера")?;
    tracing::info!(
        source = %store.config().active_source_id,
        "configuration loaded"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("pml-worker")
        .enable_all()
        .build()
        .context("не удалось создать Tokio runtime")?;
    let handle = runtime.handle().clone();

    let ui_result = pml_ui::run(paths, store, handle);
    // Give detached tasks a moment to flush; the game process itself is not a
    // child of the runtime and keeps running after the launcher closes.
    runtime.shutdown_timeout(Duration::from_secs(2));

    if let Err(error) = &ui_result {
        tracing::error!(%error, "launcher UI failed");
    }
    ui_result.context("окно лаунчера завершилось с ошибкой")?;
    tracing::info!("Odium Launcher stopped");
    Ok(())
}

fn init_logging(paths: &PortablePaths) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    // Daily files with a 14-day retention. The old `rolling::daily` helper
    // never deleted anything, so a long-lived portable install grew without
    // bound.
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("launcher.log")
        .max_log_files(14)
        .build(paths.logs_dir())
        .context("не удалось открыть журнал лаунчера")?;
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,pml_ui=debug,pml_downloader=debug"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        )
        .init();
    Ok(guard)
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(panic = %info, "unhandled panic");
        // A double-clicked GUI has no visible console: without a dialog the
        // window simply vanishes and the user has no idea what happened.
        show_panic_dialog(&info.to_string());
    }));
}

#[cfg(windows)]
fn show_panic_dialog(details: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let text = format!(
        "Odium Launcher столкнулся с непредвиденной ошибкой.\n\n{details}\n\n\
         Подробности записаны в журнал лаунчера."
    );
    let text: Vec<u16> = std::ffi::OsStr::new(&text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let title: Vec<u16> = std::ffi::OsStr::new(pml_config::BRAND_NAME)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: both buffers are NUL-terminated UTF-16 strings that outlive the
    // call; a null owner window is valid for a process-wide message box.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_panic_dialog(details: &str) {
    eprintln!("Odium Launcher panicked: {details}");
}
