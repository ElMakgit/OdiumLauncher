//! The launcher pipeline: manifest -> Java -> loader -> sync -> Minecraft.
//!
//! The pipeline runs on the Tokio runtime and reports through an unbounded
//! channel. It never touches egui state. After `GameStarted` it hands process
//! supervision to a small watcher task and returns immediately, so `Finished`
//! and `GameExited` are independent events and the UI can close on start
//! without keeping the whole pipeline alive.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pml_config::{Config, PortablePaths, Source};
use pml_downloader::{DownloadEvent, Downloader};
use pml_java::{ensure_java, JavaInstallation};
use pml_launcher::{spawn_game, LaunchSpec};
use pml_loader::LoaderInstaller;
use pml_minecraft::{LaunchRequest, Minecraft, UserProfile};
use pml_modpack::{
    extract_overrides_when_changed, load_pack, normalize_path, Manifest, ModpackSync, SyncRequest,
};
use tokio::sync::mpsc;

pub(super) const CANCELLED: &str = "Отменено";

pub(super) enum UiEvent {
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

/// Maps a domain error to the UI contract. If the user cancelled while the
/// future was running, the cancellation wins over whatever error surfaced
/// inside the downloader, installer or filesystem.
fn stage_result<T, E: std::fmt::Display>(
    result: Result<T, E>,
    cancel: &AtomicBool,
    stage: &str,
) -> Result<T, String> {
    match result {
        Ok(value) => Ok(value),
        Err(_) if cancel.load(Ordering::SeqCst) => Err(CANCELLED.to_owned()),
        Err(error) => Err(format!("{stage}: {error}")),
    }
}

/// Resolves when the user presses "Отмена". `select!` drops the losing future,
/// which closes an in-flight metadata/mrpack HTTP request instead of waiting
/// for a multi-megabyte body. Batch downloads use the downloader's own token.
async fn wait_for_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn run_pipeline(
    paths: PortablePaths,
    downloader: Downloader,
    config: Config,
    source: Source,
    tx: mpsc::UnboundedSender<UiEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let check_cancel = || {
        if cancel.load(Ordering::SeqCst) {
            Err(CANCELLED.to_owned())
        } else {
            Ok(())
        }
    };

    tx.send(UiEvent::Stage("Загрузка сборки…".to_owned())).ok();
    check_cancel()?;
    let meta_dir = paths.meta_dir();
    let pack = tokio::select! {
        result = load_pack(downloader.client(), &source, &meta_dir) => {
            stage_result(result, &cancel, "Сборка")?
        }
        _ = wait_for_cancel(&cancel) => return Err(CANCELLED.to_owned()),
    };
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
    let java = tokio::select! {
        result = ensure_java(
            &paths,
            &downloader,
            Some(download_tx.clone()),
            Some(Arc::clone(&cancel)),
        ) => {
            stage_result(result, &cancel, "Java")?
        }
        _ = wait_for_cancel(&cancel) => return Err(CANCELLED.to_owned()),
    };
    tx.send(UiEvent::Java(java.clone())).ok();
    check_cancel()?;

    tx.send(UiEvent::Stage(format!(
        "Установка {} {}…",
        manifest.loader.kind.label(),
        manifest.loader.version
    )))
    .ok();
    let loader = LoaderInstaller::new(paths.clone(), downloader.clone());
    let profile = stage_result(
        loader
            .install(
                &manifest.mc_version,
                &manifest.loader,
                &java.java_exe,
                Some(download_tx.clone()),
                Some(Arc::clone(&cancel)),
            )
            .await,
        &cancel,
        "Загрузчик",
    )?;
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
    let report = stage_result(
        sync.sync(SyncRequest {
            manifest: &manifest,
            instance_dir: &instance_dir,
            overrides: &overrides,
            progress: Some(download_tx.clone()),
        })
        .await,
        &cancel,
        "Синхронизация",
    )?;
    tx.send(UiEvent::Sync(format!(
        "Файлы: {} всего, {} скачано, {} пропущено, {} удалено",
        report.total_files, report.downloaded, report.skipped, report.removed
    )))
    .ok();
    check_cancel()?;

    if let Some(archive) = pack.mrpack {
        tx.send(UiEvent::Stage("Распаковка overrides…".to_owned()))
            .ok();
        // Overrides never own a path that the delta sync manages: otherwise a
        // disabled optional mod or a stale jar would be resurrected from the zip.
        let managed = manifest
            .files
            .iter()
            .chain(manifest.optional_files.iter())
            .map(|file| normalize_path(&file.path))
            .collect::<std::collections::BTreeSet<_>>();
        // Reapplies only when the archive fingerprint changed, so user-edited
        // configs survive ordinary launches and the cancellation token reaches
        // the blocking extraction loop.
        let extracted = extract_overrides_when_changed(
            &archive,
            &instance_dir,
            &managed,
            Some(Arc::clone(&cancel)),
        )
        .await;
        let written = stage_result(extracted, &cancel, "Распаковка overrides")?;
        let message = match written {
            Some(written) => format!("Overrides: {written} файлов распаковано"),
            None => "Overrides: уже применены".to_owned(),
        };
        tx.send(UiEvent::Sync(message)).ok();
        check_cancel()?;
    }

    tx.send(UiEvent::Stage("Подготовка Minecraft…".to_owned()))
        .ok();
    let minecraft = Minecraft::new(paths.clone(), downloader.clone());
    let user = UserProfile::offline(config.nick.clone());
    let plan = stage_result(
        minecraft
            .prepare_launch(LaunchRequest {
                mc_version: &manifest.mc_version,
                instance_dir: &instance_dir,
                loader_profile: Some(&profile),
                user: &user,
                server: &server,
                jvm: &config.jvm,
                progress: Some(download_tx.clone()),
            })
            .await,
        &cancel,
        "Minecraft",
    )?;
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

    // The launcher may close immediately after `GameStarted`. Keeping the
    // process handle in this task would tie its lifetime to the runtime, so a
    // dedicated watcher owns it and reports the exit code when it arrives.
    let watcher_tx = tx;
    tokio::spawn(async move {
        match game.wait().await {
            Ok(status) => {
                let _ = watcher_tx.send(UiEvent::GameExited(status.code()));
            }
            Err(error) => {
                tracing::warn!(%error, "could not wait for the Minecraft process");
                let _ = watcher_tx.send(UiEvent::GameExited(None));
            }
        }
    });

    Ok(())
}
