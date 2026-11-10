//! One-click diagnostics bundle for support requests.
//!
//! The report contains the portable paths, a redacted-enough config snapshot
//! (there is no token field in `config.json`), the resolved Java, the selected
//! manifest and the tails of the launcher/game logs. It is written next to the
//! other logs so a player can attach one file instead of hunting through
//! `launcher_data`.
//!
//! Building the text happens on the UI thread (it only reads memory), while
//! the log tails, `read_dir` and the file write go through `spawn_blocking`:
//! a fault-tolerant "export diagnostics" button must not freeze the window
//! when the log sits on a slow or network drive.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use super::util::open_path;
use super::LauncherApp;

impl LauncherApp {
    pub(crate) fn export_diagnostics(&mut self) {
        if self.diagnostics_rx.is_some() {
            return;
        }
        let report = self.report_header();
        let logs_dir = self.paths.logs_dir();
        let game_log = self.log_path.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        self.diagnostics_rx = Some(rx);
        self.status = "Сохранение диагностики…".to_owned();
        self.runtime.spawn_blocking(move || {
            let result = write_diagnostics(report, &logs_dir, game_log.as_deref());
            let _ = tx.send(result);
        });
    }

    pub(crate) fn open_diagnostics_folder(&self) {
        open_path(&self.paths.logs_dir());
    }

    /// Everything that can be produced without touching the disk.
    fn report_header(&self) -> String {
        let mut report = String::new();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);

        let _ = writeln!(report, "Odium Launcher diagnostics");
        let _ = writeln!(report, "version: {}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(report, "generated_unix: {stamp}");
        let _ = writeln!(
            report,
            "os: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let _ = writeln!(
            report,
            "exe: {}",
            std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|error| format!("<unavailable: {error}>"))
        );
        let _ = writeln!(report, "root: {}", self.paths.root().display());
        let _ = writeln!(report, "config: {}", self.store.path().display());
        let _ = writeln!(report, "theme: {:?}", self.theme);
        let _ = writeln!(
            report,
            "busy: {}, game_running: {}",
            self.busy, self.game_running
        );
        let _ = writeln!(report, "status: {}", self.status);

        let _ = writeln!(report, "\n[config.json]");
        match serde_json::to_string_pretty(self.store.config()) {
            Ok(json) => {
                let _ = writeln!(report, "{json}");
            }
            Err(error) => {
                let _ = writeln!(report, "<serialization failed: {error}>");
            }
        }

        let _ = writeln!(report, "\n[selected source]");
        match self.selected_source() {
            Some(source) => {
                let _ = writeln!(
                    report,
                    "id: {}\nname: {}\nkind: {:?}\nurl: {}",
                    source.id,
                    source.name,
                    source.kind(),
                    source.manifest_url
                );
            }
            None => {
                let _ = writeln!(report, "<none>");
            }
        }

        let _ = writeln!(report, "\n[manifest]");
        match &self.manifest {
            Some(manifest) => {
                let _ = writeln!(
                    report,
                    "name: {}\nversion: {}\nminecraft: {}\nloader: {} {}\nfiles: {} (+{} optional)\nserver: {:?}",
                    manifest.name,
                    manifest.version,
                    manifest.mc_version,
                    manifest.loader.kind.label(),
                    manifest.loader.version,
                    manifest.files.len(),
                    manifest.optional_files.len(),
                    manifest.server
                );
            }
            None => {
                let _ = writeln!(
                    report,
                    "<not loaded>{}",
                    self.manifest_error
                        .as_ref()
                        .map(|error| format!("\nerror: {error}"))
                        .unwrap_or_default()
                );
            }
        }

        let _ = writeln!(report, "\n[java]");
        match &self.java {
            Some(java) => {
                let _ = writeln!(
                    report,
                    "path: {}\nversion: {}\nmajor: {}\norigin: {}",
                    java.java_exe.display(),
                    java.version,
                    java.major,
                    java.origin.label()
                );
            }
            None => {
                let _ = writeln!(report, "<not resolved yet>");
            }
        }
        report
    }
}

/// Appends the log tails and writes the report. Runs on a blocking worker.
fn write_diagnostics(
    mut report: String,
    logs_dir: &Path,
    game_log: Option<&Path>,
) -> Result<PathBuf, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);

    let _ = writeln!(report, "\n[launcher log]");
    match newest_launcher_log(logs_dir) {
        Some(path) => {
            let _ = writeln!(report, "file: {}", path.display());
            append_tail(&mut report, &path, 200);
        }
        None => {
            let _ = writeln!(report, "<no launcher.log yet>");
        }
    }

    let _ = writeln!(report, "\n[game log]");
    match game_log {
        Some(path) => {
            let _ = writeln!(report, "file: {}", path.display());
            append_tail(&mut report, path, 200);
        }
        None => {
            let _ = writeln!(report, "<game has not started yet>");
        }
    }

    std::fs::create_dir_all(logs_dir).map_err(|error| error.to_string())?;
    let path = logs_dir.join(format!("diagnostics-{stamp}.txt"));
    std::fs::write(&path, report).map_err(|error| error.to_string())?;
    Ok(path)
}

fn newest_launcher_log(directory: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(directory).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("launcher.log") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, entry.path()));
        }
    }
    newest.map(|(_, path)| path)
}

fn append_tail(report: &mut String, path: &Path, lines: usize) {
    // `tail_log` already caps the read window to 96 KiB; a multi-megabyte log
    // is never loaded into the report.
    let tail = pml_launcher::tail_log(path, lines);
    if tail.is_empty() {
        let _ = writeln!(report, "<empty>");
        return;
    }
    for line in tail {
        let _ = writeln!(report, "{line}");
    }
}
