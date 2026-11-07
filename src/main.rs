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
        nick = %store.config().nick,
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
    let appender = tracing_appender::rolling::daily(paths.logs_dir(), "launcher.log");
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
    }));
}
