//! The launcher pipeline: manifest -> Java -> loader -> sync -> Minecraft.
//!
//! The pipeline runs on the Tokio runtime and reports through an unbounded
//! channel. It never touches egui state. After `GameStarted` it hands process
//! supervision to a small watcher task and returns immediately, so `Finished`
//! and `GameExited` are independent events and the UI can close on start
//! without keeping the whole pipeline alive.

use std::path::PathBuf;

use pml_config::{Config, PortablePaths, Source};
use pml_downloader::{CancelToken, DownloadEvent, Downloader};
use pml_java::{ensure_java, JavaInstallation};
use pml_launcher::{spawn_game, LaunchSpec};
use pml_loader::LoaderInstaller;
use pml_minecraft::{LaunchRequest, Minecraft, UserProfile};
use pml_modpack::{
    backup_instance, extract_overrides_when_changed, load_pack, normalize_path,
    synced_pack_version, Manifest, ModpackSync, SyncRequest,
};
use tokio::sync::mpsc;

pub(super) const CANCELLED: &str = "Отменено";

/// Number of pack-update snapshots kept per source before the oldest is pruned.
const BACKUPS_TO_KEEP: usize = 3;

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
    cancel: &CancelToken,
    stage: &str,
) -> Result<T, String> {
    match result {
        Ok(value) => Ok(value),
        Err(_) if cancel.is_cancelled() => Err(CANCELLED.to_owned()),
        Err(error) => Err(format!("{stage}: {error}")),
    }
}

/// Resolves when the user presses "Отмена". `select!` drops the losing future,
/// which closes an in-flight metadata/mrpack HTTP request instead of waiting
/// for a multi-megabyte body. Batch downloads use the downloader's own token.
async fn wait_for_cancel(cancel: &CancelToken) {
    cancel.cancelled().await;
}

pub(super) async fn run_pipeline(
    paths: PortablePaths,
    downloader: Downloader,
    config: Config,
    source: Source,
    tx: mpsc::UnboundedSender<UiEvent>,
    cancel: CancelToken,
) -> Result<(), String> {
    let check_cancel = || {
        if cancel.is_cancelled() {
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

    let required_java = pml_java::required_java_major(&manifest.mc_version);
    tx.send(UiEvent::Stage(format!("Поиск Java {required_java}…")))
        .ok();
    let java = tokio::select! {
        result = ensure_java(
            &paths,
            &downloader,
            required_java,
            Some(download_tx.clone()),
            Some(cancel.clone()),
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
                Some(cancel.clone()),
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

    // A pack update must not silently overwrite worlds or hand-edited configs.
    // Snapshot them once per version bump; ordinary launches skip the copy.
    let previous_version = synced_pack_version(&instance_dir).await;
    let version_changed = previous_version
        .as_deref()
        .is_some_and(|version| !version.is_empty() && version != manifest.version);
    if version_changed {
        tx.send(UiEvent::Stage(
            "Резервная копия мира и конфигов…".to_owned(),
        ))
        .ok();
        let instance = instance_dir.clone();
        let backups = paths.backups_dir();
        let source_id = source.id.clone();
        match tokio::task::spawn_blocking(move || {
            backup_instance(&instance, &backups, &source_id, BACKUPS_TO_KEEP)
        })
        .await
        {
            Ok(Ok(Some(path))) => {
                tx.send(UiEvent::Sync(format!(
                    "Резервная копия: {}",
                    path.display()
                )))
                .ok();
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "instance backup failed"),
            Err(error) => tracing::warn!(%error, "instance backup task failed"),
        }
    }

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
        let extracted =
            extract_overrides_when_changed(&archive, &instance_dir, &managed, Some(cancel.clone()))
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
    // Resolve `auto_xmx` here, in the domain pipeline, rather than only in the
    // UI. Any caller that starts a launch gets the same half-of-RAM policy.
    let (xms, xmx) = config
        .jvm_for(&source.id)
        .resolved(pml_config::total_system_ram_mb());
    let launch_jvm = pml_config::JvmConfig {
        xms,
        xmx,
        auto_xmx: false,
    };
    let plan = stage_result(
        minecraft
            .prepare_launch(LaunchRequest {
                mc_version: &manifest.mc_version,
                instance_dir: &instance_dir,
                loader_profile: Some(&profile),
                user: &user,
                server: &server,
                jvm: &launch_jvm,
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
